//! Full-session recording: the per-session segment directory, its ledger, the lossless
//! concatenation at stop, and recovery of a session a crash left behind.
//!
//! [`SessionRing`] is deliberately not a replay ring: a four-hour session has no oldest segment to
//! spare, so it scans the same directory names with the same [`replayed_replay::scanner`]
//! functions and never evicts. The bypass is structural (there is no cap to pass), and the scan,
//! sequence reservation and clip window are [`replayed_replay`]'s public pieces
//! ([`SegmentLedger`], [`scanner`], [`window`], [`replayed_replay::splice::ClipSplicer`]);
//! nothing here re-implements a
//! concatenation.
//!
//! ```text
//! <app data dir>/sessions/
//! ├── session-1758612345/            <- the per-session segment directory
//! │   ├── seg-000000.mp4             <- written by the encoder's segment muxer
//! │   ├── seg-000001.mp4
//! │   └── ...
//! └── session-1758612345.mp4         <- written at stop, `-c copy`, segments then removed
//! ```
//!
//! There is no ledger file: segments are enumerated from the directory (`seg-%06d.mp4`, in
//! sequence order), which is what makes a crashed session recoverable from the file names alone.

use crate::index::now_ms;
use anyhow::{bail, Context, Result};
use replayed_media::{edit, FfmpegBinaries, MediaInfo};
use replayed_replay::ledger::{Segment, SegmentLedger};
use replayed_replay::scanner::{self, newly_complete, next_segment_number};
use replayed_replay::splice::ClipMetadata;
use replayed_replay::window;
use replayed_store::{Session, Store, SESSION_MODE_SESSION};
use std::path::{Path, PathBuf};

/// Attempts at claiming a unique session directory name before giving up.
const MAX_SESSION_DIR_ATTEMPTS: u32 = 100;

/// A session's segments, indexed and never evicted.
///
/// The replay ring's cap-free twin: same directory convention, same "a segment is only trusted once
/// a strictly later one exists" scan rule, same timeline arithmetic, no eviction.
pub struct SessionRing {
    bin: FfmpegBinaries,
    dir: PathBuf,
    clips_dir: PathBuf,
    ledger: SegmentLedger,
    highest_known: Option<u64>,
    segment_ms: u64,
    /// Offset of this run's first segment on the ledger timeline (`reserve_number() * segment_ms`),
    /// so the clip path's arithmetic matches the ring's.
    origin_ms: u64,
    /// Name of the encoder in use, recorded on every clip.
    encoder: String,
    /// Media time kept before a trigger (the engine's `buffer.pre_seconds`).
    pre_ms: u64,
    /// Media time kept after a trigger (the engine's `buffer.post_seconds`).
    post_ms: u64,
    /// Whether the encoder has a microphone input. With a second audio track the splicer's plain
    /// `-c copy` concat would drop it, so clips use the map-preserving concat (`-map 0`; see `finalise`).
    microphone: bool,
}

impl SessionRing {
    /// Open the session's segment directory, creating it if the encoder is about to write into it.
    /// Nothing is scanned yet: the caller runs [`SessionRing::adopt_existing`] before the encoder is
    /// spawned, because the segment number ffmpeg is told depends on it.
    ///
    /// Takes the same `BufferConfig` as the replay ring so both modes resolve a clip the same way.
    pub fn open(
        bin: &FfmpegBinaries,
        dir: &Path,
        cfg: &replayed_replay::buffer::BufferConfig,
        microphone: bool,
        encoder: String,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the session directory {}", dir.display()))?;
        std::fs::create_dir_all(&cfg.clips_dir)
            .with_context(|| format!("creating {}", cfg.clips_dir.display()))?;
        Ok(Self {
            bin: bin.clone(),
            dir: dir.to_path_buf(),
            clips_dir: cfg.clips_dir.clone(),
            ledger: SegmentLedger::default(),
            highest_known: None,
            segment_ms: cfg.segment_ms,
            pre_ms: cfg.pre_ms,
            post_ms: cfg.post_ms,
            microphone,
            origin_ms: 0,
            encoder,
        })
    }

    /// Index what is already in the directory (normally nothing; a restart into an adopted directory
    /// gets the ring's answer rather than an overwrite).
    pub fn adopt_existing(&mut self) -> Result<usize> {
        let before = self.ledger.len();
        self.scan()?;
        Ok(self.ledger.len() - before)
    }

    /// Index every segment in the directory, including the newest file.
    ///
    /// [`SessionRing::scan`] does not trust the newest segment while ffmpeg appends to it. At shutdown
    /// the encoder has exited and that file is complete; dropping it would lose the last
    /// `segment_time` of every session.
    pub fn scan_all(&mut self) -> Result<()> {
        for segment in segments_on_disk(&self.dir, self.segment_ms)? {
            let seq = segment.seq;
            self.ledger.push(segment);
            self.highest_known = Some(self.highest_known.map_or(seq, |known| known.max(seq)));
        }
        Ok(())
    }

    /// Index newly completed segments. No eviction happens here.
    pub fn scan(&mut self) -> Result<()> {
        let observed = observed_seqs(&self.dir)?;
        for seq in newly_complete(&observed, self.highest_known) {
            let file = self.dir.join(format!("seg-{seq:06}.mp4"));
            let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            self.ledger.push(Segment {
                seq,
                file,
                start_ms: seq.saturating_mul(self.segment_ms),
                // The segment's length is NOMINAL: the segment muxer is told a length, and the
                // file it writes holds a little less (with `-fps_mode passthrough` the last
                // frame of a segment arrives one frame interval before the boundary, measured
                // at ~940ms of media per 1000ms segment at 10fps). So this ledger's clock runs
                // ahead of the real media by up to one frame interval per segment — about 3% of
                // the timeline at 30fps and 10% at 10fps.
                //
                // The RAM ring is the reference behaviour: its fragments are measured from their
                // own sample tables, so its clock is the real one. A session's clock is nominal
                // because a segment's file is a whole number of seconds by construction and
                // measuring it would mean probing every segment on every scan.
                //
                // What this costs: "30s of pre-roll" is 30s of *nominal* media in a session and
                // 30s of real media in a buffer, and the post-roll wait is expressed in the
                // ledger's clock, so it can be satisfied while the last frames of the post-roll
                // are still unwritten. The budget is sized for that (see `POST_ROLL_MARGIN` and
                // the segment term in the engine's post-roll budget).
                duration_ms: self.segment_ms,
                bytes,
            });
            self.highest_known = Some(seq);
        }
        Ok(())
    }

    /// Reserve the sequence number the encoder must start at (the ring's rule, [`next_segment_number`]),
    /// so a directory that already holds segments cannot be overwritten.
    pub fn reserve_number(&mut self) -> Result<u64> {
        let number = self.next_number()?;
        self.origin_ms = number.saturating_mul(self.segment_ms);
        Ok(number)
    }

    /// The number the next encoder must start at *without* moving this run's zero.
    ///
    /// [`SessionRing::reserve_number`] is for the start of a run: it also sets the run's origin on the
    /// ledger timeline. A capture resize starts a second encoder in the middle of the run, and
    /// moving the origin then would put every position published so far (events, clips waiting for
    /// their post-roll) on a different timeline from the footage.
    pub fn next_number(&self) -> Result<u64> {
        let names = observed_names(&self.dir)?;
        Ok(next_segment_number(&self.ledger.seqs(), &names))
    }

    /// The ledger position, in ms, of this run's zero.
    pub fn origin_ms(&self) -> u64 {
        self.origin_ms
    }

    /// This run's recorded media time, in ms (the same quantity as a ring's `stats().span_ms`).
    pub fn span_ms(&self) -> u64 {
        self.ledger.span_ms().saturating_sub(self.origin_ms)
    }

    /// Complete segments, or the ledger as it stands.
    pub fn segments(&self) -> &[Segment] {
        self.ledger.segments()
    }

    /// Bytes the session currently occupies on disk.
    pub fn bytes_on_disk(&self) -> u64 {
        self.ledger.total_bytes()
    }

    pub fn segment_count(&self) -> usize {
        self.ledger.len()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The ffmpeg binaries this session's splices run with.
    pub fn bin(&self) -> &FfmpegBinaries {
        &self.bin
    }

    /// The directory the clips are written into.
    pub fn clips_dir(&self) -> &Path {
        &self.clips_dir
    }

    /// Extract a clip around `trigger_ms` from the session's own footage: the ring's trigger, using the
    /// same [`replayed_replay::splice::ClipSplicer::splice`] with a window from this ledger and
    /// the same truncated-pre-roll warning.
    pub fn trigger(&self, trigger_ms: u64, stem: &str) -> Result<ClipMetadata> {
        self.trigger_window(trigger_ms, stem, self.pre_ms, self.post_ms)
    }

    /// [`SessionRing::trigger`] with this clip's own pre- and post-roll.
    pub fn trigger_window(
        &self,
        trigger_ms: u64,
        stem: &str,
        pre_ms: u64,
        post_ms: u64,
    ) -> Result<ClipMetadata> {
        let out = self.clips_dir.join(format!("{stem}.mp4"));
        let source = self.clip_source(trigger_ms, pre_ms, post_ms)?;
        source.splice(&self.bin, &out, &self.encoder)
    }

    /// The footage a clip around `trigger_ms` is cut from.
    ///
    /// The window is resolved here and only the *segments* travel on, so the concatenation can
    /// run on a thread of its own: a session's footage is files, and reading them costs the
    /// recording nothing.
    ///
    /// The window's start is the resolved one — the first segment that overlaps the request —
    /// because that is where the clip's first frame really is. The requested start is only a
    /// bound on it, and dating a clip by the request is off by up to one segment.
    pub(crate) fn clip_source(
        &self,
        trigger_ms: u64,
        pre_ms: u64,
        post_ms: u64,
    ) -> Result<crate::ClipSource> {
        let win = window::resolve(
            &self.ledger,
            // `trigger_ms` is run-relative; the ledger's timeline is absolute.
            self.origin_ms.saturating_add(trigger_ms),
            pre_ms,
            post_ms,
        )
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        if win.truncated_front {
            tracing::warn!(
                "only {}ms of pre-roll was buffered for a {}ms request",
                win.duration_ms(),
                pre_ms
            );
        }
        Ok(crate::ClipSource::Segments {
            window: win,
            multitrack: self.microphone,
        })
    }
}

/// The pump's view of a session's segments, with no eviction behind the scan.
impl crate::pump::MediaRing for SessionRing {
    fn scan(&mut self) -> Result<()> {
        SessionRing::scan(self)
    }

    fn span_ms(&self) -> u64 {
        SessionRing::span_ms(self)
    }
}

/// Segment file names in `dir`; used by the microphone safety net (see `finalise`).
fn observed_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// The sequence numbers of the segments in `dir` (concat lists and stray files are not segments).
fn observed_seqs(dir: &Path) -> Result<Vec<u64>> {
    Ok(observed_names(dir)?
        .iter()
        .filter_map(|n| scanner::segment_seq(n))
        .collect())
}

/// Every segment file in `dir`, in sequence order, with their actual sizes.
///
/// Built from the directory, not a saved index, so footage is findable after a crash. Zero-byte
/// files are skipped: ffmpeg creates the next segment's file before writing into it.
pub fn segments_on_disk(dir: &Path, segment_ms: u64) -> Result<Vec<Segment>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut segments: Vec<Segment> = observed_seqs(dir)?
        .into_iter()
        .map(|seq| {
            let file = dir.join(format!("seg-{seq:06}.mp4"));
            Segment {
                seq,
                file,
                start_ms: seq.saturating_mul(segment_ms),
                duration_ms: segment_ms,
                bytes: std::fs::metadata(dir.join(format!("seg-{seq:06}.mp4")))
                    .map(|m| m.len())
                    .unwrap_or(0),
            }
        })
        .filter(|segment| segment.bytes > 0)
        .collect();
    segments.sort_by_key(|s| s.seq);
    Ok(segments)
}

/// A directory name for a new session: `session-<unix seconds>`, with `-2`, `-3`, ... when a run
/// already started in that second.
///
/// The name is claimed by creation (`create_dir` fails rather than overwriting), so two processes
/// cannot mix their segments into one session file. A name whose finished file
/// ([`session_file_for_dir`]) is already there is skipped too: the directory of a session that
/// finalised within the same second is gone, so creation alone would hand its name out again and the
/// second run's concatenation would overwrite the first run's file while its row still named it.
pub fn create_session_dir(sessions_dir: &Path, started_at_ms: i64) -> Result<PathBuf> {
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    // The same instant as the row's `started_at`, so directory and file read as one session.
    let stamp = started_at_ms.div_euclid(1_000) as u64;
    for attempt in 1..=MAX_SESSION_DIR_ATTEMPTS {
        let name = if attempt == 1 {
            format!("session-{stamp}")
        } else {
            format!("session-{stamp}-{attempt}")
        };
        let dir = sessions_dir.join(name);
        if session_file_for_dir(&dir).exists() {
            continue;
        }
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", dir.display()));
            }
        }
    }
    bail!(
        "could not find a free session directory under {} after {MAX_SESSION_DIR_ATTEMPTS} \
         attempts in the same second; is something else creating them?",
        sessions_dir.display()
    )
}

/// The concatenated file for the session whose segments live in `session_dir`:
/// `sessions/<directory name>.mp4`, so `session-<seconds>[-N].mp4`.
///
/// Derived from the session's directory, which is already unique ([`create_session_dir`] claims
/// it), not from its start second: two runs that started in the same second used to compute the
/// same file name and the second finalise overwrote the first. It is still stable across a recovery
/// retry (the directory name does not change), so a retry rewrites the same path rather than leaving
/// a second file beside a partial one.
pub fn session_file_for_dir(session_dir: &Path) -> PathBuf {
    let name = session_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    session_dir
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{name}.mp4"))
}

/// Make `path` durable on disk before anything that depends on it is deleted.
///
/// `finalise` writes the session file and the caller then removes the segments, which are the only
/// other copy. Without a flush the file can still be in the page cache when power goes, leaving a
/// short or empty session file and no segments. Opened for writing because Windows refuses to flush a
/// read-only handle; the parent directory is flushed as well on Unix so the new name is durable too
/// (Windows has no directory flush to call).
pub fn sync_output(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to flush it", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing {} to disk", path.display()))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("flushing the directory {}", parent.display()))?;
    }
    Ok(())
}

/// Concatenate a finished session's segments into one file.
///
/// `segments` must be in sequence order and complete; the caller flushes the encoder first and
/// takes the list from [`segments_on_disk`] (or [`SessionRing::scan_all`]).
///
/// Shares the splicer's lossless path (`check_concat_layout`, `write_concat_list`,
/// `concat_lossless_sized` in `replayed_media::edit`) so a fix to `-map 0` (microphone tracks)
/// or to the size-scaled timeout lands in both callers. What this adds for a session: a partial
/// output is deleted rather than left occupying the space its retry needs, and a session with a
/// microphone has its audio-stream count verified afterwards.
pub fn finalise(
    bin: &FfmpegBinaries,
    out: &Path,
    segments: &[Segment],
    encoder: &str,
    multitrack: bool,
) -> Result<ClipMetadata> {
    if segments.is_empty() {
        bail!("no segments to concatenate: this session recorded nothing");
    }
    let files: Vec<PathBuf> = segments.iter().map(|s| s.file.clone()).collect();

    // Refused before anything is written: a segment whose layout differs from the first would make
    // `-map 0` silently drop or truncate a track. The layout also says how many audio tracks to
    // re-name, since a `-c copy` concat loses the encoder's per-stream names.
    let layout = edit::check_concat_layout(bin, &files)?;

    let list = out.with_extension("concat.txt");
    let total_bytes = edit::write_concat_list(&files, &list)?;
    // The list is scratch whether the copy works out or not.
    let concat = edit::concat_lossless_sized(bin, &list, out, total_bytes, layout.audio_count());
    let _ = std::fs::remove_file(&list);
    if let Err(e) = concat {
        // A partial file is not a session and occupies the space the retry needs.
        let _ = std::fs::remove_file(out);
        return Err(e).with_context(|| {
            format!(
                "concatenating {} segment(s) into {}",
                segments.len(),
                out.display()
            )
        });
    }

    let info = MediaInfo::probe(bin, out)
        .with_context(|| format!("probing the concatenated session {}", out.display()))?;
    // Durable before the caller removes the segments: they are the only other copy.
    sync_output(out)?;
    if info.video.is_none() {
        tracing::warn!(
            "{} has no video stream: the concatenation produced a file ffprobe cannot see \
             a picture in",
            out.display()
        );
    }
    // The microphone safety net: `-map 0` keeps the second track, and a file that lost it looks like
    // one that never had it, so count the audio streams.
    if multitrack {
        match audio_stream_count(bin, out) {
            Some(count) if count < 2 => tracing::warn!(
                "{} has {count} audio stream(s) but this session recorded a microphone: the \
                 microphone track is missing from the session file (the segments under the \
                 session directory still have both tracks)",
                out.display()
            ),
            Some(count) => tracing::info!(
                "session {} carries {count} audio tracks (game audio + microphone)",
                out.display()
            ),
            None => tracing::warn!(
                "could not count the audio streams of {}: whether the microphone track \
                 survived the concatenation is unverified for this session",
                out.display()
            ),
        }
    }

    Ok(ClipMetadata {
        path: out.to_path_buf(),
        duration_ms: info.duration_ms,
        size_bytes: info.size_bytes,
        encoder: encoder.to_string(),
    })
}

/// Make the file a replay buffer wrote alongside itself ([`crate::memory_ring::SESSION_PART`], fragmented MP4 as
/// the encoder produced it) into an ordinary, seekable session file at `out`.
///
/// `None` when the part is missing or empty: a recording that never produced a fragment has no session
/// to name. A part cut short by a crash or a full disk is still read up to its last whole fragment,
/// which is why recovery uses this too. The audio tracks are named from the part's own stream count, so
/// a recovered session needs no word from the row about its microphone.
pub fn finalise_part(
    bin: &FfmpegBinaries,
    part: &Path,
    out: &Path,
    encoder: &str,
    multitrack: bool,
) -> Result<Option<ClipMetadata>> {
    if part.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        return Ok(None);
    }
    let audio = audio_stream_count(bin, part);
    if let Err(e) = edit::remux_file_lossless_titled(bin, part, out, audio.unwrap_or(1)) {
        // A partial file is not a session and occupies the space the retry needs.
        let _ = std::fs::remove_file(out);
        return Err(e)
            .with_context(|| format!("remuxing {} into {}", part.display(), out.display()));
    }
    let info = MediaInfo::probe(bin, out)
        .with_context(|| format!("probing the saved session {}", out.display()))?;
    // Durable before the caller removes the part: it is the only other copy.
    sync_output(out)?;
    if info.video.is_none() {
        tracing::warn!(
            "{} has no video stream: the remux produced a file ffprobe cannot see a picture in",
            out.display()
        );
    }
    if multitrack && audio.is_some_and(|count| count < 2) {
        tracing::warn!(
            "{} has fewer than two audio streams but this session recorded a microphone",
            out.display()
        );
    }
    Ok(Some(ClipMetadata {
        path: out.to_path_buf(),
        duration_ms: info.duration_ms,
        size_bytes: info.size_bytes,
        encoder: encoder.to_string(),
    }))
}

/// How many audio streams `path` carries, or `None` when ffprobe cannot say.
///
/// A second probe because [`MediaInfo`] reports only the first video and first audio stream and
/// cannot tell whether the microphone survived.
fn audio_stream_count(bin: &FfmpegBinaries, path: &Path) -> Option<usize> {
    let output = replayed_media::sidecar_command(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

/// What a recovery pass found and did, for the caller to log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Sessions that were concatenated and closed by this pass.
    pub recovered: Vec<i64>,
    /// Sessions whose segments could not be concatenated (row left running; the next start retries).
    pub unfinalised: Vec<i64>,
    /// Rows for a recording that left nothing on disk, closed with no file.
    pub closed_empty: Vec<i64>,
    /// Directories under the sessions area that no session row names.
    pub orphan_dirs: Vec<PathBuf>,
    /// Session directories still holding segments although their session is finished (a crash between
    /// concatenation and removal). Left in place; retention removes them with the row.
    pub leftover_dirs: Vec<PathBuf>,
    /// Trailing segments that could not be read (a kill or power loss leaves a plain segment with no
    /// `moov`) and were left out of the recovered session. What the recovered file lacks, by name.
    pub dropped_segments: Vec<DroppedSegment>,
}

/// A segment recovery left out of a session because ffprobe could not open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedSegment {
    pub session_id: i64,
    pub seq: u64,
    pub file: PathBuf,
    pub bytes: u64,
}

impl Recovery {
    /// Whether this pass changed anything worth a log line.
    pub fn is_empty(&self) -> bool {
        self.recovered.is_empty()
            && self.unfinalised.is_empty()
            && self.closed_empty.is_empty()
            && self.orphan_dirs.is_empty()
            && self.leftover_dirs.is_empty()
            && self.dropped_segments.is_empty()
    }
}

/// Find what a previous run left behind, finish it, and report it.
///
/// A crash leaves a `sessions` row with `ended_at IS NULL`, a directory of segments and no session
/// file. This pass runs at every start, before this run creates anything:
///
/// * a row with segments on disk is concatenated into the file it would have written and closed
///   with the real size; segments are then removed. If concatenation fails (a full disk), the row is
///   left running and the segments kept so the next start retries, and retention (which never evicts
///   a running session) cannot delete the only copy;
/// * a row that wrote no segments is closed with no file;
/// * a buffer-mode row left running is closed with its size; its scratch directory belongs to the
///   ring, so nothing on disk is touched;
/// * a directory no row names is reported, never deleted: without a row there is no proof it is
///   this application's to remove.
///
/// `encoder` is a label only: the encoder that wrote the segments lived in a previous process.
///
/// # Only ever beside no live recorder
///
/// "A row with `ended_at IS NULL` is a crash" holds only if no other recorder is running against this
/// data directory: a second process (the CLI started while the desktop records) would otherwise
/// finalise or delete a live session. `_owner` is the proof that none is: the data directory's
/// exclusive lock ([`crate::lock::DataDirLock`]), which the operating system releases when a process
/// dies. A row left running while this process holds it belongs to a dead one, by construction,
/// and the signature makes the check impossible to forget.
///
/// # A session cut off mid-segment
///
/// A kill or a power loss leaves the last plain segment without its `moov` index, which `ffmpeg`
/// refuses, so concatenating *every* segment failed at every start and the row never closed. Trailing
/// segments ffprobe cannot open are therefore left out of the concatenation and named in
/// [`Recovery::dropped_segments`]; the readable ones are recovered. This probes each file instead of
/// assuming a layout, so a fragmented segment (readable up to its last whole fragment) is kept whole
/// and a plain one is judged by whether it opens. Only the tail is dropped: an unreadable segment in
/// the middle would leave a hole in the timeline, so that session is left running and its footage kept.
pub fn recover_sessions(
    store: &Store,
    sessions_dir: &Path,
    bin: &FfmpegBinaries,
    segment_ms: u64,
    encoder: &str,
    _owner: &crate::lock::DataDirLock,
) -> Recovery {
    let mut report = Recovery::default();
    let rows = match store.list_sessions() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                "could not read the session store to recover unfinished sessions: {err:#}"
            );
            return report;
        }
    };
    let named: Vec<PathBuf> = rows
        .iter()
        .map(|row| PathBuf::from(&row.scratch_dir))
        .collect();

    for row in &rows {
        if row.mode == SESSION_MODE_SESSION {
            recover_session_row(store, row, bin, segment_ms, encoder, &mut report);
        } else if row.ended_at_ms.is_none() {
            // A buffer-mode row (or a mode a newer build wrote): the ring owns its footage; just stamp the end so retention sees it as history.
            match store.end_session(row.id, now_ms(), None, row.size_bytes, 0) {
                Ok(()) => {
                    tracing::info!(
                        "closed the {} session #{} a previous run left running ({} bytes)",
                        row.mode,
                        row.id,
                        row.size_bytes
                    );
                    report.closed_empty.push(row.id);
                }
                Err(err) => tracing::warn!(
                    "could not close the {} session #{} a previous run left running: {err:#}",
                    row.mode,
                    row.id
                ),
            }
        }
    }

    // Directories no row names: reported, never removed.
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || named.contains(&path) {
                continue;
            }
            match segments_on_disk(&path, segment_ms) {
                Ok(segments) if !segments.is_empty() => {
                    tracing::warn!(
                        "{} holds {} session segment(s) ({} bytes) that no session row names: \
                         nothing will delete them, and they are not in the library. Inspect it \
                         and remove it by hand, or leave it — this report repeats at every start.",
                        path.display(),
                        segments.len(),
                        segments.iter().map(|s| s.bytes).sum::<u64>()
                    );
                    report.orphan_dirs.push(path);
                }
                _ => {}
            }
        }
    }
    report
}

/// One session row's recovery (see [`recover_sessions`]).
fn recover_session_row(
    store: &Store,
    row: &Session,
    bin: &FfmpegBinaries,
    segment_ms: u64,
    encoder: &str,
    report: &mut Recovery,
) {
    let dir = PathBuf::from(&row.scratch_dir);
    let segments = match segments_on_disk(&dir, segment_ms) {
        Ok(segments) => segments,
        Err(err) => {
            tracing::warn!(
                "could not read the session directory {} of session #{}: {err:#}",
                dir.display(),
                row.id
            );
            return;
        }
    };
    let running = row.ended_at_ms.is_none();

    // A replay buffer that was also saving its session leaves one fragmented file instead of segments.
    let part = dir.join(crate::memory_ring::SESSION_PART);
    if running && segments.is_empty() && part.is_file() {
        let out = session_file_for_dir(&dir);
        match finalise_part(bin, &part, &out, encoder, true) {
            Ok(meta) => {
                let (path, size, duration) = match &meta {
                    Some(m) => (
                        Some(m.path.display().to_string()),
                        m.size_bytes as i64,
                        m.duration_ms as i64,
                    ),
                    None => (None, 0, 0),
                };
                match store.end_session(row.id, now_ms(), path.as_deref(), size, duration) {
                    Ok(()) => {
                        tracing::info!(
                            "recovered session #{} left by a previous run: {}",
                            row.id,
                            path.as_deref()
                                .unwrap_or("it recorded nothing; closed with no file")
                        );
                        remove_session_dir(&dir);
                        if meta.is_some() {
                            report.recovered.push(row.id);
                        } else {
                            report.closed_empty.push(row.id);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "recovered session #{} but could not close its row: {err:#}",
                            row.id
                        );
                        report.unfinalised.push(row.id);
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    "could not recover session #{} left by a previous run: {err:#}. Its file is \
                     kept in {}, the row is left running, and the next start tries again.",
                    row.id,
                    dir.display()
                );
                report.unfinalised.push(row.id);
            }
        }
        return;
    }

    if segments.is_empty() {
        if running {
            match store.end_session(row.id, now_ms(), None, row.size_bytes, 0) {
                Ok(()) => {
                    tracing::info!(
                        "session #{} was left running by a previous run and recorded nothing; \
                         closed with no file",
                        row.id
                    );
                    report.closed_empty.push(row.id);
                }
                Err(err) => tracing::warn!("could not close session #{}: {err:#}", row.id),
            }
            // An empty directory is ours and costs nothing to remove.
            if dir.is_dir() {
                if let Err(err) = std::fs::remove_dir(&dir) {
                    tracing::debug!("could not remove the empty {}: {err}", dir.display());
                }
            }
        }
        return;
    }

    if !running && row.final_path.is_some() {
        // A finished session whose segments remain (crash between concatenation and removal): the final
        // file exists and the row names it, so these are a duplicate. Reported; retention removes them with the row.
        tracing::warn!(
            "session #{} is finished ({}) but its segment directory {} still holds {} \
             segment(s); the retention pass removes them with the session",
            row.id,
            row.final_path.as_deref().unwrap_or("no file"),
            dir.display(),
            segments.len()
        );
        report.leftover_dirs.push(dir);
        return;
    }

    let out = session_file_for_dir(&dir);
    let (segments, dropped) = split_unreadable_tail(bin, segments);
    for gone in &dropped {
        tracing::warn!(
            "session #{}: {} ({} bytes) cannot be read (the recording was cut off while it was \
             being written) and is left out of the recovered session",
            row.id,
            gone.display(),
            std::fs::metadata(gone).map(|m| m.len()).unwrap_or(0)
        );
        report.dropped_segments.push(DroppedSegment {
            session_id: row.id,
            seq: segment_seq(gone),
            file: gone.clone(),
            bytes: std::fs::metadata(gone).map(|m| m.len()).unwrap_or(0),
        });
    }
    if segments.is_empty() {
        // Nothing readable: close the row so it stops being retried, with no file. The damaged
        // segments stay where they are for retention to remove with the row; recovering nothing is no
        // reason to delete anything.
        match store.end_session(row.id, now_ms(), None, row.size_bytes, 0) {
            Ok(()) => {
                tracing::warn!(
                    "session #{} left no readable segment; closed with no file (its damaged \
                     files remain in {})",
                    row.id,
                    dir.display()
                );
                report.closed_empty.push(row.id);
            }
            Err(err) => tracing::warn!("could not close session #{}: {err:#}", row.id),
        }
        return;
    }
    let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    // `true` asks for the audio-stream count: the row does not say whether a recovered session had a
    // microphone, and a silently absent voice track is what the check catches.
    match finalise(bin, &out, &segments, encoder, true) {
        Ok(meta) => {
            let path = meta.path.display().to_string();
            match store.end_session(
                row.id,
                now_ms(),
                Some(&path),
                meta.size_bytes as i64,
                // The probed length of the concatenated file, which the timeline needs.
                meta.duration_ms as i64,
            ) {
                Ok(()) => {
                    tracing::info!(
                        "recovered session #{} left by a previous run: {} segment(s) ({bytes} \
                         bytes) concatenated into {} ({} bytes)",
                        row.id,
                        segments.len(),
                        path,
                        meta.size_bytes
                    );
                    report.recovered.push(row.id);
                }
                Err(err) => {
                    tracing::warn!(
                        "recovered session #{} into {} but could not close its row: {err:#}",
                        row.id,
                        path
                    );
                    report.unfinalised.push(row.id);
                    return;
                }
            }
            remove_session_dir(&dir);
        }
        Err(err) => {
            tracing::error!(
                "could not recover session #{} left by a previous run: {err:#}. Its {} \
                 segment(s) ({bytes} bytes) are kept in {}, the row is left running, and the \
                 next start tries again — a running session is never evicted, so nothing \
                 will delete this footage meanwhile. Free space if that was the reason.",
                row.id,
                segments.len(),
                dir.display()
            );
            report.unfinalised.push(row.id);
        }
    }
}

/// Split off the trailing segments ffprobe cannot open: `(readable prefix, unreadable files)`.
///
/// Walks back from the newest segment and stops at the first one that opens, so the cost is one probe
/// in the normal case (a crash leaves at most the segment being written damaged). Never looks at the
/// middle: see [`recover_sessions`].
fn split_unreadable_tail(
    bin: &FfmpegBinaries,
    mut segments: Vec<Segment>,
) -> (Vec<Segment>, Vec<PathBuf>) {
    let mut dropped = Vec::new();
    while let Some(last) = segments.last() {
        if MediaInfo::probe(bin, &last.file).is_ok() {
            break;
        }
        dropped.push(last.file.clone());
        segments.pop();
    }
    dropped.reverse();
    (segments, dropped)
}

/// The sequence number in a `seg-NNNNNN.mp4` file name (0 when the name is not one).
fn segment_seq(file: &Path) -> u64 {
    file.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix("seg-"))
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// Remove a session's segment directory, reporting what could not go.
pub fn remove_session_dir(dir: &Path) -> u64 {
    let bytes: u64 = segments_on_disk(dir, 0)
        .map(|segments| segments.iter().map(|s| s.bytes).sum())
        .unwrap_or(0);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => bytes,
        Err(err) => {
            tracing::warn!(
                "could not remove the session directory {}: {err} — its segments stay on \
                 disk; a later pass or the retention rule (through the session row) removes \
                 them",
                dir.display()
            );
            0
        }
    }
}

/// Write two one-second segments into `dir` with a real ffmpeg (1 video plus `audio` audio streams,
/// like the encoder's segment muxer). Test-only and shared so the engine's crash-recovery test can
/// build a session on disk that no live recording leaves behind (a clean stop removes it).
#[cfg(test)]
pub(crate) fn layout_test_segments(dir: &Path, audio: usize) -> Result<Vec<Segment>> {
    let bin = FfmpegBinaries::discover(None)
        .map_err(|err| anyhow::anyhow!("ffmpeg on PATH is needed for the test fixtures: {err}"))?;
    std::fs::create_dir_all(dir)?;
    for seq in 0..2u64 {
        let out = dir.join(format!("seg-{seq:06}.mp4"));
        let mut cmd = replayed_media::sidecar_command(&bin.ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
        ]);
        if audio == 2 {
            cmd.args(["-f", "lavfi", "-i", "sine=frequency=880:duration=1"]);
        }
        cmd.args([
            "-c:v", "libx264", "-g", "10", "-c:a", "aac", "-map", "0:v", "-map", "1:a",
        ]);
        if audio == 2 {
            cmd.args(["-map", "2:a"]);
        }
        cmd.args(["-t", "1"]).arg(&out);
        let output = cmd.output().context("running ffmpeg to build a segment")?;
        if !output.status.success() {
            bail!(
                "building {} failed: {}",
                out.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    segments_on_disk(dir, 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use replayed_store::SESSION_MODE_BUFFER;

    /// The ffmpeg on `PATH`.
    fn bin() -> FfmpegBinaries {
        FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
    }

    /// The ring configuration the tests open with: the engine's `BufferConfig` shape with these tests' segment length and window.
    fn session_cfg(clips_dir: &Path) -> replayed_replay::buffer::BufferConfig {
        replayed_replay::buffer::BufferConfig {
            pre_ms: 2_000,
            post_ms: 1_000,
            segment_ms: 1_000,
            clips_dir: clips_dir.to_path_buf(),
        }
    }

    /// [`layout_test_segments`], with the temp-dir conventions of this module's tests.
    fn write_segments(dir: &Path, audio: usize) -> Vec<Segment> {
        layout_test_segments(dir, audio).expect("laying out test segments")
    }

    /// A temp directory under `target/`, so a test run never leaves leftovers in the user's temp or home.
    fn temp_dir(what: &str) -> tempfile::TempDir {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/recorder-session-tests");
        std::fs::create_dir_all(&base).expect("creating the test scratch root");
        tempfile::Builder::new()
            .prefix(what)
            .tempdir_in(&base)
            .expect("a temp dir under target/")
    }

    fn store() -> Store {
        let store = Store::open_in_memory().expect("an in-memory index");
        store.migrate().expect("migrating it");
        store
    }

    /// A `sessions` row for a recording that is (or was) running over `dir`.
    fn running_session(store: &Store, dir: &Path, mode: &str) -> i64 {
        store
            .start_session(
                Some("Dota 2"),
                mode,
                1_700_000_000_000,
                &dir.display().to_string(),
                0,
            )
            .expect("opening a session row")
    }

    /// [`recover_sessions`] as the engine calls it: holding the data directory's lock, which the
    /// test directory stands in for.
    fn recover(store: &Store, sessions_dir: &Path) -> Recovery {
        let owner = tempfile::tempdir().expect("a lock directory");
        let lock = crate::lock::DataDirLock::acquire(owner.path()).expect("the lock");
        recover_sessions(store, sessions_dir, &bin(), 1_000, "libx264", &lock)
    }

    /// Cut a plain segment short, as a kill or a power loss does: the `moov` index is written last, so
    /// a segment that did not get to finish has none and ffprobe cannot open it.
    fn truncate(file: &Path) {
        let len = std::fs::metadata(file).unwrap().len();
        let handle = std::fs::OpenOptions::new().write(true).open(file).unwrap();
        handle.set_len(len / 2).unwrap();
    }

    /// Both lossless concatenation paths keep every audio stream.
    ///
    /// Regression test: `concat_lossless` once passed no `-map`, so ffmpeg kept one audio stream and a
    /// clip from a session recorded with a microphone silently lost the voice track. Both now keep every
    /// recorded track, and a clip also leads with the game and the voice mixed
    /// (`edit::concat_clip_sized`); the two halves below would disagree and show which path regressed.
    #[test]
    fn every_lossless_concat_path_keeps_a_second_audio_track() {
        let dir = temp_dir("session-");
        let segments = write_segments(&dir.path().join("session-1"), 2);
        assert_eq!(segments.len(), 2, "two segments");

        // The splicer: what a clip out of a session is cut with.
        let spliced = dir.path().join("spliced.mp4");
        let window = replayed_replay::window::SegmentWindow {
            segments: segments.clone(),
            truncated_front: false,
        };
        let meta =
            replayed_replay::splice::ClipSplicer::splice(&bin(), &window, &spliced, "libx264")
                .expect("the splice");
        assert_eq!(
            audio_stream_count(&bin(), &spliced),
            Some(3),
            "the splicer must keep the microphone track and the game audio, after their mix"
        );

        // The finalise: both tracks, and the file is as long as the two segments.
        let out = dir.path().join("session.mp4");
        let meta2 = finalise(&bin(), &out, &segments, "libx264", true).expect("the finalise");
        assert_eq!(
            audio_stream_count(&bin(), &out),
            Some(2),
            "both tracks survive"
        );
        let info = MediaInfo::probe(&bin(), &out).expect("probing the session file");
        assert!(info.video.is_some(), "the video stream is there");
        assert!(
            info.duration_ms >= 1_900,
            "two one-second segments make a ~2s session, got {}ms",
            info.duration_ms
        );
        assert_eq!(meta2.size_bytes, std::fs::metadata(&out).unwrap().len());
        assert!(meta.size_bytes > 0);

        // The clip must also be as long as its segments: a concat that kept both audio streams but truncated one would pass the count.
        let spliced_info = MediaInfo::probe(&bin(), &spliced).expect("probing the clip");
        assert!(
            spliced_info.duration_ms >= 1_900,
            "the spliced clip must be ~2s of media, got {}ms",
            spliced_info.duration_ms
        );
    }

    /// Without a microphone, `finalise` keeps exactly the streams the segments had.
    #[test]
    fn a_single_track_session_concatenates_to_one_video_and_one_audio_stream() {
        let dir = temp_dir("session-");
        let segments = write_segments(&dir.path().join("session-1"), 1);
        let out = dir.path().join("session.mp4");

        let meta = finalise(&bin(), &out, &segments, "libx264", false).expect("the finalise");

        assert!(out.is_file());
        assert_eq!(meta.size_bytes, std::fs::metadata(&out).unwrap().len());
        let info = MediaInfo::probe(&bin(), &out).expect("probing it");
        assert!(info.video.is_some() && info.audio.is_some());
        assert_eq!(audio_stream_count(&bin(), &out), Some(1));
        assert!(
            meta.duration_ms >= 1_900,
            "{}ms for two 1s segments",
            meta.duration_ms
        );

        // A finalise with nothing to concatenate is an error, not an empty file.
        let err = finalise(&bin(), &dir.path().join("empty.mp4"), &[], "libx264", false)
            .expect_err("no segments is not a session");
        assert!(err.to_string().contains("no segments"), "{err}");
    }

    /// The live scan skips the newest segment (ffmpeg is appending); the shutdown scan takes every file.
    #[test]
    fn the_newest_segment_is_taken_at_shutdown_and_not_before_it() {
        let dir = temp_dir("session-");
        let session_dir = dir.path().join("session-1");
        write_segments(&session_dir, 1);

        let mut ring = SessionRing::open(
            &bin(),
            &session_dir,
            &session_cfg(&dir.path().join("clips")),
            false,
            "libx264".into(),
        )
        .expect("opening the session ring");
        assert_eq!(
            ring.adopt_existing().expect("adopting"),
            1,
            "one complete segment"
        );
        assert_eq!(ring.segment_count(), 1);
        assert_eq!(ring.span_ms(), 1_000, "one second of completed footage");
        ring.scan().expect("scanning again changes nothing");
        assert_eq!(
            ring.segment_count(),
            1,
            "the newest file is still being written"
        );

        ring.scan_all().expect("the shutdown scan");
        assert_eq!(
            ring.segment_count(),
            2,
            "at shutdown the newest segment counts"
        );
        assert_eq!(ring.span_ms(), 2_000);
        assert_eq!(
            ring.bytes_on_disk(),
            segments_on_disk(&session_dir, 1_000)
                .unwrap()
                .iter()
                .map(|s| s.bytes)
                .sum::<u64>()
        );
        assert_eq!(
            ring.reserve_number().unwrap(),
            2,
            "segments 0 and 1 are on disk, so the encoder must continue at 2 (a fresh \
             directory would number from 0)"
        );
    }

    /// A directory that already holds segments is not written over: the encoder continues the numbering.
    #[test]
    fn a_session_directory_that_already_holds_segments_continues_the_numbering() {
        let dir = temp_dir("session-");
        let session_dir = dir.path().join("session-1");
        write_segments(&session_dir, 1);

        let mut ring = SessionRing::open(
            &bin(),
            &session_dir,
            &session_cfg(&dir.path().join("clips")),
            false,
            "libx264".into(),
        )
        .expect("opening");
        ring.adopt_existing().expect("adopting");
        assert_eq!(
            ring.reserve_number().expect("reserving"),
            2,
            "segments 0 and 1 exist"
        );
        assert_eq!(
            ring.span_ms(),
            0,
            "an adopted directory's footage is not this run's timeline (it has captured nothing)"
        );
    }

    /// After a crash the next start concatenates the segments into the session file, closes the row,
    /// removes the segments and reports it.
    #[test]
    fn a_crashed_session_is_recovered_into_one_file_and_closed() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        let segments = write_segments(&session_dir, 1);
        let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);
        store
            .set_session_size(id, bytes as i64)
            .expect("the running size");

        let report = recover(&store, &sessions_dir);

        assert_eq!(
            report.recovered,
            vec![id],
            "the crashed session was finalised: {report:?}"
        );
        let row = store
            .get_session(id)
            .unwrap()
            .expect("the row is still there");
        let final_path = row.final_path.expect("the row names the session file");
        assert!(Path::new(&final_path).is_file(), "{final_path} must exist");
        assert!(row.ended_at_ms.is_some(), "and the row is closed");
        assert!(row.size_bytes > 0, "with the real size: {}", row.size_bytes);
        assert_eq!(
            row.size_bytes as u64,
            std::fs::metadata(&final_path).unwrap().len()
        );
        assert_eq!(
            final_path,
            session_file_for_dir(&session_dir).display().to_string(),
            "the file is named from the session's own directory"
        );
        assert_eq!(audio_stream_count(&bin(), Path::new(&final_path)), Some(1));
        assert!(!session_dir.exists(), "the temporary segments are gone");

        // A second pass has nothing to recover, and must not touch the finished session.
        let again = recover(&store, &sessions_dir);
        assert!(again.is_empty(), "nothing left to do: {again:?}");
        assert!(store.get_session(id).unwrap().unwrap().final_path.is_some());
    }

    /// When concatenation fails (corrupt segments stand in for a full disk), every segment is kept and
    /// the row stays running, so retention cannot evict footage that has no session file.
    #[test]
    fn a_recovery_that_cannot_concatenate_keeps_the_segments_and_leaves_the_row_running() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        // A bad segment in the middle, not at the end: only a trailing one is dropped (it is the one a
        // kill leaves unfinished), so this is a session that cannot be assembled, as one on a full disk is.
        write_segments(&session_dir, 1);
        std::fs::write(session_dir.join("seg-000000.mp4"), b"this is not an mp4").unwrap();
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);

        let report = recover(&store, &sessions_dir);

        assert_eq!(report.unfinalised, vec![id], "{report:?}");
        assert!(report.recovered.is_empty());
        assert!(
            session_dir.join("seg-000000.mp4").is_file(),
            "the segments are kept"
        );
        let row = store.get_session(id).unwrap().unwrap();
        assert!(
            row.ended_at_ms.is_none(),
            "the row is left running on purpose: a running session is never evicted"
        );
        assert!(row.final_path.is_none());
        assert!(
            !session_file_for_dir(&session_dir).exists(),
            "and no partial file is left behind"
        );
    }

    /// After a kill or power loss the last plain segment has no `moov`, so the whole session used to
    /// fail to finalise at every start, forever, and the row never closed. Recovery now drops the
    /// unreadable tail, concatenates what is readable and says what it dropped.
    #[test]
    fn a_truncated_last_segment_is_dropped_and_the_rest_is_recovered() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        write_segments(&session_dir, 1);
        truncate(&session_dir.join("seg-000001.mp4"));
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);

        let report = recover(&store, &sessions_dir);

        assert_eq!(report.recovered, vec![id], "{report:?}");
        assert!(report.unfinalised.is_empty(), "{report:?}");
        assert_eq!(report.dropped_segments.len(), 1, "{report:?}");
        assert_eq!(report.dropped_segments[0].session_id, id);
        assert_eq!(report.dropped_segments[0].seq, 1);
        assert!(report.dropped_segments[0].bytes > 0);
        let row = store.get_session(id).unwrap().unwrap();
        assert!(row.ended_at_ms.is_some(), "the row is closed");
        let final_path = row.final_path.expect("the session file is named");
        let info = MediaInfo::probe(&bin(), Path::new(&final_path)).expect("a playable file");
        assert!(
            (900..1_500).contains(&info.duration_ms),
            "one good second survived, got {}ms",
            info.duration_ms
        );
        // Recovery is a one-shot: the next start finds nothing to do.
        assert!(recover(&store, &sessions_dir).is_empty());
    }

    /// A crash inside the first segment leaves nothing readable. The row must still close (it used to
    /// stay running forever), and the damaged bytes are kept for retention to remove with the row.
    #[test]
    fn a_session_with_no_readable_segment_is_closed_and_its_files_are_kept() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        write_segments(&session_dir, 1);
        truncate(&session_dir.join("seg-000000.mp4"));
        truncate(&session_dir.join("seg-000001.mp4"));
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);

        let report = recover(&store, &sessions_dir);

        assert_eq!(report.closed_empty, vec![id], "{report:?}");
        assert_eq!(report.dropped_segments.len(), 2, "{report:?}");
        let row = store.get_session(id).unwrap().unwrap();
        assert!(row.ended_at_ms.is_some() && row.final_path.is_none());
        assert!(
            session_dir.join("seg-000000.mp4").is_file(),
            "nothing is deleted when nothing was recovered"
        );
    }

    /// Two runs in one second used to share `session-<seconds>.mp4`: the second finalise overwrote the
    /// first run's file while its row still named it. The file now takes its directory's name, and a
    /// name whose file already exists is never handed out.
    #[test]
    fn two_sessions_in_the_same_second_get_different_files() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let first = create_session_dir(&sessions_dir, 1_700_000_000_500).unwrap();
        let second = create_session_dir(&sessions_dir, 1_700_000_000_900).unwrap();
        assert_ne!(first, second);
        assert_ne!(session_file_for_dir(&first), session_file_for_dir(&second));
        assert_eq!(
            session_file_for_dir(&first),
            sessions_dir.join("session-1700000000.mp4")
        );

        // The first session finished long ago: its directory is gone but its file stays.
        std::fs::remove_dir(&first).unwrap();
        std::fs::remove_dir(&second).unwrap();
        std::fs::write(session_file_for_dir(&first), b"finished").unwrap();
        let third = create_session_dir(&sessions_dir, 1_700_000_000_100).unwrap();
        assert_ne!(
            session_file_for_dir(&third),
            session_file_for_dir(&first),
            "a finished session's file is not reused for a new run in its second"
        );
    }

    #[test]
    fn the_output_is_synced_before_its_segments_may_go() {
        let dir = temp_dir("session-");
        let file = dir.path().join("out.mp4");
        assert!(
            sync_output(&file).is_err(),
            "a file that is not there cannot be durable"
        );
        std::fs::write(&file, b"x").unwrap();
        sync_output(&file).expect("an existing file syncs");
    }

    /// A row for a recording that wrote nothing is closed; a buffer-mode row left running is closed
    /// without touching its scratch directory; a directory no row names is reported, not deleted.
    #[test]
    fn recovery_closes_empty_rows_and_reports_directories_it_cannot_own() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // An empty session directory (crash before the first segment) and a buffer row whose scratch directory the ring manages.
        let empty_dir = sessions_dir.join("session-1");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("seg-000000.mp4"), b"the ring's own business").unwrap();
        let store = store();
        let empty = running_session(&store, &empty_dir, SESSION_MODE_SESSION);
        let buffered = running_session(&store, &scratch, SESSION_MODE_BUFFER);

        // A directory nobody names.
        let orphan = sessions_dir.join("session-nobody");
        write_segments(&orphan, 1);

        let report = recover(&store, &sessions_dir);

        let mut closed = report.closed_empty.clone();
        closed.sort_unstable();
        let mut expected = vec![empty, buffered];
        expected.sort_unstable();
        assert_eq!(closed, expected, "{report:?}");
        assert_eq!(report.orphan_dirs, vec![orphan.clone()], "{report:?}");
        assert!(store
            .get_session(empty)
            .unwrap()
            .unwrap()
            .ended_at_ms
            .is_some());
        assert!(store
            .get_session(buffered)
            .unwrap()
            .unwrap()
            .ended_at_ms
            .is_some());
        assert!(!empty_dir.exists(), "an empty session directory is removed");
        assert!(
            scratch.join("seg-000000.mp4").is_file(),
            "a buffer session's scratch directory is the ring's, and is never touched"
        );
        assert!(
            orphan.is_dir(),
            "an unnamed directory is reported, never deleted"
        );
    }

    /// A finished session whose segments remain is reported and left alone; retention removes the duplicate with the row.
    #[test]
    fn a_finished_session_with_leftover_segments_is_reported_not_deleted() {
        let dir = temp_dir("session-");
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-1700000000");
        write_segments(&session_dir, 1);
        let store = store();
        let id = running_session(&store, &session_dir, SESSION_MODE_SESSION);
        let file = session_file_for_dir(&session_dir);
        std::fs::write(&file, b"a finished session").unwrap();
        store
            .end_session(
                id,
                1_700_000_001_000,
                Some(&file.display().to_string()),
                20,
                0,
            )
            .expect("ending it");

        let report = recover(&store, &sessions_dir);

        assert_eq!(
            report.leftover_dirs,
            vec![session_dir.clone()],
            "{report:?}"
        );
        assert!(report.recovered.is_empty() && report.unfinalised.is_empty());
        assert!(session_dir.is_dir(), "reported, not removed");
        assert!(
            report.orphan_dirs.is_empty(),
            "the row names it, so it is not an orphan"
        );
    }

    /// Two recordings starting in the same second get different directories (the name is claimed by creating it).
    #[test]
    fn a_session_directory_is_created_and_never_shared() {
        let dir = temp_dir("session-");
        let sessions = dir.path().join("sessions");

        let first = create_session_dir(&sessions, 1_700_000_000_000).expect("the first");
        let second = create_session_dir(&sessions, 1_700_000_000_000).expect("the second");

        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        assert_eq!(
            first.file_name().unwrap(),
            "session-1700000000",
            "named from the session's start instant, like its file"
        );
        assert_eq!(
            session_file_for_dir(&first).file_name().unwrap(),
            "session-1700000000.mp4"
        );
    }
}
