//! Lossless (stream-copy) media edits. These never re-encode.

use crate::binaries::{run_with_stdin, run_with_timeout, FfmpegBinaries};
use crate::probe::{stream_layout, StreamLayout};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const EDIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The floor a lossless copy is given, on top of the time its own size implies.
pub const COPY_FLOOR: Duration = Duration::from_secs(60);

/// The throughput the size-derived part of a copy's budget assumes, in bytes/second.
///
/// A stream copy is I/O bound, so wall time scales with the bytes moved, not the media
/// duration. A fixed budget is wrong both ways: too generous for a 30-second clip, far too
/// small for a multi-gigabyte session, and the latter hurts because the copy fails only after
/// doing all the work.
///
/// Deliberately pessimistic (10 MB/s): too large costs a hung ffmpeg the caller kills, too
/// small costs a concatenation that fails after the work. A slow spinning disk clears it.
pub const COPY_MIN_BYTES_PER_SEC: u64 = 10_000_000;

/// Wall-clock budget for copying `bytes` with `-c copy`.
pub fn copy_budget(bytes: u64) -> Duration {
    // Saturating, because a budget that cannot be represented is a budget that runs out rather
    // than a panic in the middle of a copy that has already done its work.
    Duration::from_secs(bytes / COPY_MIN_BYTES_PER_SEC.max(1)).saturating_add(COPY_FLOOR)
}

/// Remux without re-encoding, moving the index to the front for fast seeking.
pub fn remux_lossless(bin: &FfmpegBinaries, src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "remux", dst, EDIT_TIMEOUT)
}

/// Trim `[start_ms, end_ms)` with `-c copy`. Cuts snap to the nearest preceding keyframe,
/// inherent to stream copy.
pub fn trim_lossless(
    bin: &FfmpegBinaries,
    src: &Path,
    dst: &Path,
    start_ms: u64,
    end_ms: u64,
) -> Result<()> {
    if end_ms <= start_ms {
        bail!("trim range is empty: start={start_ms}ms end={end_ms}ms");
    }
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", start_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src)
        .arg("-t")
        .arg(format!("{:.3}", (end_ms - start_ms) as f64 / 1000.0))
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst);
    expect_success(cmd, "trim", dst, EDIT_TIMEOUT)
}

/// Single-frame JPEG at `at_ms`, for timelines.
pub fn thumbnail(bin: &FfmpegBinaries, src: &Path, at_ms: u64, dst: &Path) -> Result<()> {
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", at_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src)
        .args(["-frames:v", "1", "-q:v", "4"])
        .arg(dst);
    expect_success(cmd, "thumbnail", dst, EDIT_TIMEOUT)
}

/// Concatenate segments into one file with `-c copy`.
///
/// Every segment must share identical codec parameters, which they do since they come from one
/// encoder invocation.
///
/// `-map 0` is what keeps this correct. Without a `-map`, ffmpeg's default stream selection
/// keeps exactly one audio stream: concatenating segments with 1 video + 2 audio streams
/// produced a 2-stream file, silently dropping the microphone track with exit status 0
/// (measured on ffmpeg 9.0.2).
pub fn concat_lossless(bin: &FfmpegBinaries, list_file: &Path, dst: &Path) -> Result<()> {
    // One audio stream: the ordinary recording with no microphone. A caller concatenating a
    // two-track recording must use [`concat_lossless_sized`] with the real count, or the
    // microphone's track would come out named "Game Audio".
    concat_lossless_sized(bin, list_file, dst, 0, 1)
}

/// Remux a fragmented-MP4 byte stream into an ordinary clip file with `-c copy`.
///
/// This is how a clip is saved out of an in-memory ring. The footage has only been in RAM, and
/// the file written here, named by the caller, is the only thing that touches the disk during
/// the whole buffer-and-save cycle.
///
/// `-i pipe:0` on purpose: spilling the stream to a temp file and remuxing that would
/// reintroduce the scratch churn this path exists to remove, at the moment the user is waiting.
/// [`run_with_stdin`] writes the bytes on its own thread, as the encoder does: a pipe buffer is
/// 64KiB here and as little as 4KiB for a Windows anonymous pipe, so a blocking write on this
/// thread could stall before the deadline bounding it ever ran.
///
/// The audio tracks are re-named from [`audio_titles`], since a `-c copy` to a file carries no
/// per-stream metadata and the clip's tracks would otherwise be anonymous.
pub fn remux_stream_lossless(
    bin: &FfmpegBinaries,
    bytes: Vec<u8>,
    dst: &Path,
    audio_streams: usize,
) -> Result<()> {
    if bytes.is_empty() {
        bail!("refusing to remux an empty stream: there is no footage to write");
    }
    // The budget is derived from the bytes being moved, like the concat's: the same copy, from a
    // pipe.
    let budget = copy_budget(bytes.len() as u64);
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args([
        "-v", "error", "-y", "-i", "pipe:0", "-map", "0", "-c", "copy",
    ]);
    for (index, title) in audio_titles(audio_streams).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={title}"));
    }
    cmd.args(["-movflags", "+faststart"]).arg(dst);

    let out = run_with_stdin(cmd, bytes, budget)?;
    if !out.status.success() {
        bail!(
            "remuxing the in-memory stream into {} failed: {}",
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !dst.is_file() {
        bail!(
            "remuxing the in-memory stream reported success but {} does not exist",
            dst.display()
        );
    }
    Ok(())
}

/// Remux a fragmented MP4 *file* into an ordinary seekable one, without re-encoding.
///
/// The file twin of [`remux_stream_lossless`]: a session recorded off the replay buffer's encoder is
/// written as the fragments arrive, so it is a file by the time it is finished. The audio tracks are
/// re-named from [`audio_titles`] for the same reason (`-c copy` carries no per-stream names), and
/// the budget is derived from the file's size like a concatenation's.
pub fn remux_file_lossless_titled(
    bin: &FfmpegBinaries,
    src: &Path,
    dst: &Path,
    audio_streams: usize,
) -> Result<()> {
    let bytes = src
        .metadata()
        .with_context(|| format!("statting {}", src.display()))?
        .len();
    if bytes == 0 {
        bail!(
            "refusing to remux an empty file: {} holds no footage",
            src.display()
        );
    }
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(["-map", "0", "-c", "copy"]);
    for (index, title) in audio_titles(audio_streams).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={title}"));
    }
    cmd.args(["-movflags", "+faststart"]).arg(dst);
    expect_success(cmd, "remux", dst, copy_budget(bytes))
}

/// One chapter mark: a named position in a clip, in ms from its first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    pub start_ms: u64,
    pub title: String,
}

/// The title of the chapter that covers a clip's lead-in when its first mark is later than 0.
pub const LEAD_IN_TITLE: &str = "Start";

/// Render `chapters` as an ffmetadata file. Each chapter runs to the next one's start and the last
/// to `duration_ms`; one at or past `duration_ms` is dropped. `chapters` must be sorted.
///
/// A [`LEAD_IN_TITLE`] chapter is put in front of a first mark later than 0: the MP4 chapter track
/// always starts at 0, so without it the first mark would be read back at 0 and seeking to it
/// would land on the clip's start rather than the moment.
pub fn ffmetadata(chapters: &[Chapter], duration_ms: u64) -> String {
    let mut kept: Vec<(u64, &str)> = chapters
        .iter()
        .filter(|c| c.start_ms < duration_ms)
        .map(|c| (c.start_ms, c.title.as_str()))
        .collect();
    if kept.first().is_some_and(|&(start, _)| start > 0) {
        kept.insert(0, (0, LEAD_IN_TITLE));
    }
    let mut out = String::from(";FFMETADATA1\n");
    for (i, &(start, title)) in kept.iter().enumerate() {
        let end = kept.get(i + 1).map_or(duration_ms, |&(next, _)| next);
        out.push_str(&format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={}\n",
            escape_ffmetadata(title)
        ));
    }
    out
}

/// ffmetadata reads `=`, `;`, `#`, `\`, newlines and carriage returns as syntax, so a title must
/// escape them.
///
/// A carriage return is in the set for the same reason a newline is, and it is the one that
/// matters: ffmetadata's demuxer treats a carriage return as a line break. Measured on ffmpeg
/// 9.0.2, a title of `raw\rtitle=injected` came back with the title `injected` — the carriage
/// return ended the line and the rest of it became a key of its own, which is the chapter's own
/// title. Escaped, the same text comes back as one title.
///
/// The brackets are escaped for the same reason `=` is: `[CHAPTER]` opens a section, and a title
/// that opened one would end the chapter it belongs to.
///
/// The titles are `EventKind::as_tag()` constants in practice, so this is defence in depth: it
/// is what a free-text title would need, and it costs nothing while it is true.
fn escape_ffmetadata(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n' | '\r' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Rewrite the MP4 at `path` with `chapters` as chapter marks, which players such as VLC and mpv
/// offer for seeking. A no-op when no chapter falls inside the clip.
///
/// The original is replaced only after the remux succeeded, so an error leaves it intact. The
/// audio-track names are re-applied because `-c copy` drops them (see [`audio_titles`]). The MP4
/// muxer writes the chapters both as a `chpl` atom and as a text track, so the file gains one
/// `data` stream.
pub fn add_chapters(
    bin: &FfmpegBinaries,
    path: &Path,
    chapters: &[Chapter],
    duration_ms: u64,
) -> Result<()> {
    if !chapters.iter().any(|c| c.start_ms < duration_ms) {
        return Ok(());
    }
    let audio_streams = stream_layout(bin, path)?.audio_count();
    let meta = path.with_extension("chapters.txt");
    let tmp = path.with_extension("chapters.mp4");
    std::fs::write(&meta, ffmetadata(chapters, duration_ms))
        .with_context(|| format!("writing {}", meta.display()))?;

    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(path)
        .arg("-i")
        .arg(&meta)
        .args(["-map", "0", "-map_chapters", "1", "-c", "copy"]);
    for (index, title) in audio_titles(audio_streams).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={title}"));
    }
    cmd.args(["-movflags", "+faststart"]).arg(&tmp);
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let remuxed = expect_success(cmd, "adding chapters", &tmp, copy_budget(bytes)).and_then(|()| {
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {} with its chaptered copy", path.display()))
    });
    let _ = std::fs::remove_file(&meta);
    if remuxed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    remuxed
}

/// The `title` the game-audio track is tagged with, the encoder's own string.
pub const GAME_AUDIO_TITLE: &str = "Game Audio";

/// The `title` the microphone track is tagged with, the encoder's own string.
pub const MICROPHONE_TITLE: &str = "Microphone";

/// The titles for a lossless concatenation's audio tracks, in stream order.
///
/// A `-c copy` through the concat demuxer does not carry per-stream metadata. The encoder's
/// segments carry it (`tags: { "name": "Game Audio" }`, since the MP4 muxer stores `title=` in
/// the track's `name` atom) but after `-map 0 -c copy` neither name survives, with or without
/// `-map_metadata 0`, so clips and session files had anonymous audio tracks. The concat
/// re-applies the names.
///
/// The convention is the encoder's: game audio first, microphone second. One audio stream is
/// the game audio; two are the game audio plus the microphone. The encoder maps at most two, and
/// any beyond are left as the muxer named them rather than mislabelled.
pub fn audio_titles(audio_streams: usize) -> Vec<&'static str> {
    match audio_streams {
        0 => Vec::new(),
        1 => vec![GAME_AUDIO_TITLE],
        _ => vec![GAME_AUDIO_TITLE, MICROPHONE_TITLE],
    }
}

/// As [`concat_lossless`], with the copy's budget derived from the bytes being moved and the
/// audio tracks re-named.
///
/// A whole session needs the budget: at a few hundred MB the floor is fine, at several GB it is
/// not, and a copy that runs out of time fails only after doing the work. `total_bytes` is the
/// summed size of the listed segments; pass 0 for the floor.
///
/// `audio_streams` is how many audio streams the segments carry (the count
/// [`check_concat_layout`] returns). It changes only the names, never the mapping: `-map 0`
/// keeps every stream. See [`audio_titles`].
pub fn concat_lossless_sized(
    bin: &FfmpegBinaries,
    list_file: &Path,
    dst: &Path,
    total_bytes: u64,
    audio_streams: usize,
) -> Result<()> {
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(list_file)
        .args(["-map", "0", "-c", "copy"]);
    for (index, title) in audio_titles(audio_streams).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={title}"));
    }
    cmd.args(["-movflags", "+faststart"]).arg(dst);
    expect_success(cmd, "concat", dst, copy_budget(total_bytes))
}

/// Concatenate whole clips into one, losslessly, refusing clips that do not share one layout.
///
/// This is [`trim_lossless`]'s sibling for more than one input: `-c copy`, so the result is the
/// originals' own packets in order, not a re-encode. It is the same concat the replay splicer and
/// the session recorder use ([`concat_lossless_sized`], [`check_concat_layout`]), so a fix to
/// `-map 0` (a microphone track) lands in all three callers.
///
/// The layout check is what makes the result trustworthy rather than merely written: `-map 0`
/// takes the first input's streams, and ffmpeg exits 0 while silently truncating a track when a
/// later input disagrees ([`check_concat_layout`] has the measurement). Mixed codecs, sizes or
/// audio-track counts are therefore a refusal naming the offending clip, never a file with a
/// missing microphone or a frozen last clip.
///
/// The list is written beside the output and removed afterwards, as the session finaliser does,
/// so a failed concat does not leave a stray `.txt` in the library.
pub fn combine_lossless(bin: &FfmpegBinaries, sources: &[PathBuf], dst: &Path) -> Result<()> {
    if sources.len() < 2 {
        bail!(
            "combining needs at least two clips; {} was given",
            sources.len()
        );
    }
    let layout = check_concat_layout(bin, sources)?;
    let list = dst.with_extension("concat.txt");
    let total_bytes = write_concat_list(sources, &list)?;
    let concat = concat_lossless_sized(bin, &list, dst, total_bytes, layout.audio_count());
    let _ = std::fs::remove_file(&list);
    if let Err(err) = concat {
        // A partial file is not a clip: it occupies the space the retry needs and no row names it.
        let _ = std::fs::remove_file(dst);
        return Err(err);
    }
    Ok(())
}

/// Write a concat list naming `segments` in order; returns their total size in bytes. The size
/// is returned because this list is the authoritative statement of what will be read, and
/// [`concat_lossless_sized`] needs it for the budget.
pub fn write_concat_list(segments: &[PathBuf], list: &Path) -> Result<u64> {
    let mut file =
        std::fs::File::create(list).with_context(|| format!("creating {}", list.display()))?;
    let mut total = 0u64;
    for seg in segments {
        // Absolute, so the list does not depend on the working directory.
        let path =
            std::fs::canonicalize(seg).with_context(|| format!("resolving {}", seg.display()))?;
        // The total is the budget the copy will be given, so it saturates rather than
        // overflowing: a list of segments cannot hold more bytes than a `u64` can say.
        total = total.saturating_add(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
        writeln!(file, "file '{}'", concat_list_path(&path))
            .with_context(|| format!("writing {}", list.display()))?;
    }
    Ok(total)
}

/// Refuse a concatenation whose inputs do not all share one stream layout.
///
/// This makes `-map 0` safe. `-map 0` takes every stream of the first input, so the first
/// segment's layout decides the output's and a later disagreeing segment is silently wrong
/// (measured on ffmpeg 9.0.2): a 3-stream segment followed by a 2-stream one exits 0 and still
/// writes the file, and a 1-stream segment in the middle truncates that track (1.02s inside a
/// 2s file). ffmpeg reports neither.
///
/// So layouts are compared first and a mismatch is refused, naming the position and both
/// shapes. The caller keeps its segments and loses only the automatic step: a visible failure
/// beats an invisible fragment.
pub fn check_concat_layout(bin: &FfmpegBinaries, segments: &[PathBuf]) -> Result<StreamLayout> {
    let Some((first, rest)) = segments.split_first() else {
        bail!("cannot check the stream layout of an empty segment list");
    };
    let expected = stream_layout(bin, first)
        .with_context(|| format!("reading the stream layout of {}", first.display()))?;
    for (i, seg) in rest.iter().enumerate() {
        let got = stream_layout(bin, seg)
            .with_context(|| format!("reading the stream layout of {}", seg.display()))?;
        if got != expected {
            bail!(
                "segment {} of {} differs in stream layout from the first segment, so \
                 concatenating them would silently drop or truncate a track: first is [{}] \
                 ({}) but this one is [{}] ({}). Refused; the segments are left in place",
                i + 2,
                segments.len(),
                describe(&expected),
                expected.counts_summary(),
                describe(&got),
                got.counts_summary(),
            );
        }
    }
    Ok(expected)
}

/// `video/h264 64x48, audio/aac 48000Hz 2ch`: the shapes, for a refusal message.
fn describe(layout: &StreamLayout) -> String {
    layout
        .streams
        .iter()
        .map(|s| s.summary())
        .collect::<Vec<_>>()
        .join(", ")
}

fn expect_success(cmd: Command, what: &str, dst: &Path, budget: Duration) -> Result<()> {
    let out = run_with_timeout(cmd, budget)?;
    if !out.status.success() {
        bail!(
            "{what} failed writing {}: {}",
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !dst.is_file() {
        bail!(
            "{what} reported success but {} does not exist",
            dst.display()
        );
    }
    dst.metadata()
        .map(|_| ())
        .with_context(|| format!("statting {}", dst.display()))
}

/// Render `path` as the path field of one `file '...'` line of an ffmpeg concat list.
///
/// The concat demuxer parses the list itself, so an OS-native path cannot be written as-is.
///
/// * Verbatim prefixes. On Windows `std::fs::canonicalize` returns `\\?\C:\Users\...` (or
///   `\\?\UNC\server\share\...`), a prefix ffmpeg cannot open, so it is stripped. The UNC form
///   keeps its meaning by becoming `//server/share/...`.
/// * Backslashes. The list is parsed as an escaped string and a backslash next to a quote
///   mangles the field (on ffmpeg 9.0.2 a `file '.../it's dir/...'` line truncated at the
///   quote and a `\'` spelling resolved to neither). The separator is converted to `/`, which
///   ffmpeg also accepts on Windows.
/// * Single quotes end the quoted path early. The demuxer's spelling for a literal quote is the
///   shell's `'\''` (`file '.../it'\''s.mp4'` opens the file; a bare quote and `\'` fail).
///
/// Order is load-bearing: backslashes are converted before quote escaping, so the backslash the
/// escape introduces is not rewritten into a slash (which would end the path at the quote).
///
/// On Unix a backslash is an ordinary filename byte, so the conversion is a Windows-shaped
/// choice; scratch-segment paths generated here contain none.
///
/// It lives beside the concat invocation whose list it renders because the replay splicer and
/// the recorder's session finalise both build lists, and a second copy of this escaping is a
/// second place for the Windows handling to go wrong. [`crate::edit::write_concat_list`] is the
/// only caller that should exist.
pub fn concat_list_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let unverbatim = match raw.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share\dir` — a UNC path in verbatim guise.
        Some(rest) => format!("//{rest}"),
        None => raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string(),
    };
    unverbatim.replace('\\', "/").replace('\'', r"'\''")
}

#[cfg(test)]
mod tests {
    use super::{
        concat_list_path, copy_budget, ffmetadata, trim_lossless, Chapter, COPY_FLOOR,
        COPY_MIN_BYTES_PER_SEC, LEAD_IN_TITLE,
    };
    use std::path::Path;
    use std::time::Duration;

    // These paths are never canonicalised here: on the Unix host a Windows path is just a string,
    // which makes the prefix and backslash handling (the part that breaks on Windows) testable off
    // Windows.

    #[test]
    fn a_verbatim_windows_path_loses_its_prefix_and_its_backslashes() {
        assert_eq!(
            concat_list_path(Path::new(r"\\?\C:\Users\a\videos\seg-000001.mp4")),
            "C:/Users/a/videos/seg-000001.mp4"
        );
    }

    #[test]
    fn a_verbatim_unc_path_becomes_a_plain_unc_path() {
        assert_eq!(
            concat_list_path(Path::new(r"\\?\UNC\server\share\videos\seg-000001.mp4")),
            "//server/share/videos/seg-000001.mp4"
        );
    }

    #[test]
    fn a_plain_unix_path_is_untouched() {
        assert_eq!(
            concat_list_path(Path::new("/tmp/a/seg.mp4")),
            "/tmp/a/seg.mp4"
        );
    }

    #[test]
    fn a_single_quote_is_escaped_so_the_list_stays_valid() {
        assert_eq!(
            concat_list_path(Path::new("/tmp/it's a dir/seg.mp4")),
            r"/tmp/it'\''s a dir/seg.mp4"
        );
    }

    #[test]
    fn quote_escaping_survives_backslash_conversion() {
        // The escape `'\''` contains a backslash: converted in the other order it would become `/` and
        // the demuxer would read the path as ending at the quote.
        let rendered = concat_list_path(Path::new(r"\\?\C:\a b\it's\seg.mp4"));
        assert_eq!(rendered, r"C:/a b/it'\''s/seg.mp4");
        // The only backslashes left are the ones inside the quote escape; any other
        // one would be read by the demuxer as an escape character.
        assert!(
            !rendered.replace(r"'\''", "").contains('\\'),
            "a path separator survived as a backslash: {rendered}"
        );
    }

    #[test]
    fn a_copy_budget_grows_with_the_bytes_and_never_drops_below_the_floor() {
        // The failure this prevents is a multi-gigabyte session copy killed at 60s, so the budget must
        // never be smaller than the floor and must rise with size.
        assert_eq!(copy_budget(0), COPY_FLOOR, "no bytes means the floor alone");
        assert_eq!(copy_budget(1), COPY_FLOOR, "a byte is not a second");
        assert!(
            copy_budget(COPY_MIN_BYTES_PER_SEC) > COPY_FLOOR,
            "10MB buys a second"
        );

        // 4 GB is ~429 seconds on top of the floor. Asserted as a range so the test does not restate
        // the arithmetic it checks.
        let four_gb = 4_000_000_000u64;
        let budget = copy_budget(four_gb);
        assert!(
            (Duration::from_secs(460)..=Duration::from_secs(500)).contains(&budget),
            "4GB should buy ~429s on top of the 60s floor, got {budget:?}"
        );
    }

    fn chapter(start_ms: u64, title: &str) -> Chapter {
        Chapter {
            start_ms,
            title: title.to_string(),
        }
    }

    #[test]
    fn each_chapter_runs_to_the_next_and_the_last_to_the_clip_end() {
        let meta = ffmetadata(&[chapter(0, "Kill"), chapter(1500, "Death")], 4000);
        assert_eq!(
            meta,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1500\ntitle=Kill\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=1500\nEND=4000\ntitle=Death\n"
        );
    }

    #[test]
    fn a_first_mark_after_zero_gets_a_lead_in_chapter() {
        let meta = ffmetadata(&[chapter(1000, "Kill")], 4000);
        assert!(
            meta.contains(&format!("START=0\nEND=1000\ntitle={LEAD_IN_TITLE}\n")),
            "{meta}"
        );
        assert!(
            meta.contains("START=1000\nEND=4000\ntitle=Kill\n"),
            "{meta}"
        );
    }

    #[test]
    fn a_chapter_past_the_clip_end_is_dropped_and_does_not_extend_the_last() {
        let meta = ffmetadata(&[chapter(0, "Kill"), chapter(4000, "Late")], 4000);
        assert!(meta.contains("START=0\nEND=4000\n"), "{meta}");
        assert!(!meta.contains("Late"), "{meta}");
    }

    #[test]
    fn ffmetadata_syntax_in_a_title_is_escaped() {
        let meta = ffmetadata(&[chapter(0, "a=b;c#d\\e\nf\rg[h]i")], 10);
        assert!(
            meta.contains("title=a\\=b\\;c\\#d\\\\e\\\nf\\\rg\\[h\\]i\n"),
            "{meta}"
        );
    }

    /// A trim range that is not strictly positive is refused before anything is spawned.
    ///
    /// The range's length is `end_ms - start_ms`, so a range that is not positive would
    /// underflow. The check is what keeps an extreme pair of timestamps — the largest a `u64`
    /// can hold — from wrapping into an enormous duration instead of being refused.
    #[test]
    fn an_empty_trim_range_is_refused_before_anything_is_spawned() {
        // The binaries do not exist: the refusal must come before they are used.
        let bin = crate::FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        for (start_ms, end_ms) in [(u64::MAX, u64::MAX), (u64::MAX, 0), (10, 10)] {
            let err = trim_lossless(
                &bin,
                Path::new("/nonexistent/in.mp4"),
                Path::new("/nonexistent/out.mp4"),
                start_ms,
                end_ms,
            )
            .expect_err("an empty range is refused");
            assert!(
                err.to_string().contains("trim range is empty"),
                "the refusal must name the case: {err}"
            );
        }
    }
}
