//! Splitting ffmpeg's fragmented MP4 output into self-contained fragments.
//!
//! This is what makes an in-memory replay buffer possible. With `-f mp4 -movflags
//! empty_moov+frag_keyframe+default_base_moof` on a pipe, ffmpeg writes one small `ftyp`+`moov`
//! header and then `moof`+`mdat` pairs, one per forced keyframe. Measured on a 6s 320x180
//! capture: a 1249-byte header, then 6 fragments a second apart, each carrying a `tfdt` (base
//! media decode time) in its track's timescale.
//!
//! * A fragment is self-contained. `header + any contiguous range of fragments` is a valid
//!   fragmented MP4, and `ffmpeg -i that -c copy out.mp4` makes it a normal clip: no
//!   re-encoding, no temp segment files, nothing on disk until the user saves.
//! * A fragment starts at a keyframe, because ffmpeg was told `frag_keyframe` and the encoder
//!   forces one every `segment_ms`. A fragment boundary is a cut point with the file-based
//!   segmenter's granularity.
//!
//! The splitter is incremental: push whatever bytes were just read from the pipe and get back
//! any fragments now complete. Nothing is buffered twice; a fragment's bytes move out as soon as
//! its `mdat` has arrived.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

/// One fragmented-MP4 fragment: a `moof` and the `mdat` that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// Sequence number, counted from 0 in the order the splitter produced them.
    pub seq: u64,
    /// Where this fragment's earliest video sample decodes, in ms: its `tfdt`, converted with the
    /// video track's timescale. Run-relative media time, monotonic across a stream, used to measure
    /// the buffer's span and select a clip's range without decoding anything.
    pub start_ms: u64,
    /// How long this fragment's video samples span, in ms, from its `trun`/`tfhd` sample durations
    /// converted with the video track's timescale.
    ///
    /// `0` means unknown, not "instantaneous": a sample table this build cannot read leaves the
    /// fragment usable with its length unclaimed, and a caller needing a proven end must treat 0 as
    /// "not measured" (see `MemorySegment::is_closed`). Reading it here rather than from the next
    /// fragment's `tfdt` keeps a ring's `span_ms` from lagging a fragment behind the footage held.
    pub duration_ms: u64,
    /// The bytes: this fragment's `moof` and its `mdat`, and nothing else.
    pub bytes: Vec<u8>,
    /// Whether the fragment's first video sample is a sync (key) sample, read from the fragment's own
    /// `trun`/`tfhd` sample flags, so the code picking a clip's first fragment can assert it: a clip
    /// beginning mid-GOP would play garbage first.
    ///
    /// `frag_keyframe` makes this `true` for the stream this pipeline asks ffmpeg for, but it is
    /// measured rather than assumed: a constant would make the assertion built on it unable to fail,
    /// and a muxer flag change would then go unnoticed until clips opened on grey frames. A fragment
    /// whose flags cannot be read is `false`: a start that cannot be shown to be a keyframe is not
    /// claimed to be one.
    pub keyframe: bool,
}

/// An incremental splitter for ffmpeg's fragmented-MP4 output. Not `Clone`: it owns the
/// unconsumed tail of a stream, and two would be two readers.
#[derive(Debug, Default)]
pub struct FragmentSplitter {
    /// Bytes received but not yet consumed by a complete box.
    buf: Vec<u8>,
    /// `ftyp` + `moov`, once both have been seen. Kept whole: a clip is this plus a range of
    /// fragments.
    header: Option<Vec<u8>>,
    /// Track id to `(is_video, timescale)`, read out of the `moov`. A fragment's `tfdt` is in its
    /// track's own timescale (15360 for video, 48000 for audio in one measured stream), and the
    /// video one is the clock a clip's range is measured on.
    tracks: HashMap<u32, (bool, u32)>,
    /// A `moof` whose `mdat` has not arrived yet, with the start time, sample span and keyframe flag
    /// read out of it. The duration is read here because it is in the same `moof`, and reading it at
    /// assembly would leave the ring guessing.
    pending: Option<(Placement, Vec<u8>)>,
    next_seq: u64,
}

impl FragmentSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `ftyp`+`moov` header once seen, `None` until then. A clip needs it, and it is the one
    /// piece that must be remembered across evictions.
    pub fn header(&self) -> Option<&[u8]> {
        self.header.as_deref()
    }

    /// Bytes received that do not yet form a complete box, so a caller can bound its memory: a
    /// stream stopping mid-box must not grow a buffer for ever.
    pub fn pending_bytes(&self) -> usize {
        self.buf.len()
    }

    /// How many audio tracks the header declares (zero before the header is seen). The save path
    /// needs it to re-apply track names: a `-c copy` carries no per-stream metadata, so
    /// `edit::audio_titles` must be told how many tracks to name.
    pub fn audio_tracks(&self) -> usize {
        self.tracks
            .values()
            .filter(|(is_video, _)| !is_video)
            .count()
    }

    /// How many video tracks the header declares. Zero before the header has been seen.
    pub fn video_tracks(&self) -> usize {
        self.tracks
            .values()
            .filter(|(is_video, _)| *is_video)
            .count()
    }

    /// Feed bytes just read from the pipe; returns whatever fragments that completed.
    ///
    /// A fragment is returned only once its `mdat` has fully arrived, so each is immediately usable
    /// and droppable, letting a ring evict from the front without holding a half-box.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Fragment>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some((kind, size)) = peek_box(&self.buf)? else {
                break;
            };
            if self.buf.len() < size {
                break; // the rest of this box has not arrived yet
            }
            let body: Vec<u8> = self.buf.drain(..size).collect();
            match kind.as_str() {
                "ftyp" => {
                    // The header starts here; `moov` appends to it. Anything before it is not header and does not
                    // accumulate.
                    self.header = Some(body);
                }
                "moov" => {
                    self.tracks = parse_tracks(&body)
                        .context("reading the track table from ffmpeg's fragmented-MP4 header")?;
                    let mut header = self.header.take().unwrap_or_default();
                    header.extend_from_slice(&body);
                    self.header = Some(header);
                }
                "moof" => {
                    if self.pending.is_some() {
                        bail!(
                            "two `moof` boxes in a row: the fragment before this one never \
                             received its `mdat`, so its samples cannot be recovered"
                        );
                    }
                    let placement = self.placement(&body)?;
                    self.pending = Some((placement, body));
                }
                "mdat" => {
                    let Some((placement, moof)) = self.pending.take() else {
                        // A bare `mdat` with no `moof` is not a fragment. Not an error (`free`/`skip` boxes and other
                        // muxer furniture are legal at top level) but not footage, so it is dropped.
                        continue;
                    };
                    let mut bytes = moof;
                    bytes.extend_from_slice(&body);
                    out.push(Fragment {
                        seq: self.next_seq,
                        start_ms: placement.start_ms,
                        duration_ms: placement.duration_ms,
                        bytes,
                        keyframe: placement.keyframe,
                    });
                    self.next_seq += 1;
                }
                // `free`, `skip`, `sidx`, `styp`, `mfra`, `moov` after the first, …
                _ => {}
            }
        }
        Ok(out)
    }

    /// The `tfdt` of a fragment's **video** track (its start, in ms), how long its video samples
    /// span (its duration, in ms) and whether its first video sample is a keyframe.
    fn placement(&self, moof: &[u8]) -> Result<Placement> {
        for traf in children(moof, 8, "traf") {
            let Some(track_id) = track_id(traf) else {
                continue;
            };
            let Some((is_video, timescale)) = self.tracks.get(&track_id) else {
                continue;
            };
            if !is_video || *timescale == 0 {
                continue;
            }
            let Some(ticks) = base_media_decode_time(traf) else {
                continue;
            };
            let scale = u64::from(*timescale);
            let start_ms = ticks.saturating_mul(1_000) / scale;
            // A fragment whose length cannot be read is still usable, but its end must not be invented:
            // 0 makes `MemorySegment::is_closed` false, and the ring treats it as the file ledger treats
            // an untrusted segment: held, not measured.
            let duration_ms =
                sample_span_ticks(traf).map_or(0, |t| t.saturating_mul(1_000) / scale);
            return Ok(Placement {
                start_ms,
                duration_ms,
                keyframe: first_sample_is_sync(traf).unwrap_or(false),
            });
        }
        // A fragment with no video track is not something this pipeline produces (video is input 0 and
        // always mapped), so it is a malformed stream rather than a case to guess at.
        bail!("no video `tfdt` in this fragment: its start time cannot be placed on the timeline")
    }
}

/// What a `moof` says about where its fragment sits and how it starts.
#[derive(Debug, Clone, Copy)]
struct Placement {
    start_ms: u64,
    duration_ms: u64,
    keyframe: bool,
}

/// The kind and declared size of the box at the front of `buf`, or `None` if fewer than eight
/// bytes have arrived. Returns the total size, header included, which is what has to be drained.
fn peek_box(buf: &[u8]) -> Result<Option<(String, usize)>> {
    if buf.len() < 8 {
        return Ok(None);
    }
    let small = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let kind = String::from_utf8_lossy(&buf[4..8]).into_owned();
    let size = match small {
        0 => {
            // "To the end of the file" is meaningless on a live stream and ffmpeg does not emit it; treated
            // as an error rather than guessed at.
            bail!("a `{kind}` box declares size 0 (extend to end of file) on a live stream");
        }
        1 => {
            if buf.len() < 16 {
                return Ok(None);
            }
            let large = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            usize::try_from(large).context("a 64-bit box size does not fit in memory")?
        }
        n => n as usize,
    };
    if size < 8 {
        bail!("a `{kind}` box declares a size of {size}, which cannot hold its own header");
    }
    Ok(Some((kind, size)))
}

/// The boxes directly inside `buf[from..to]`, in order.
fn boxes(buf: &[u8]) -> Vec<(&[u8], String)> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 8 <= buf.len() {
        let small = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let kind = String::from_utf8_lossy(&buf[off + 4..off + 8]).into_owned();
        let (size, hdr) = match small {
            0 => (buf.len() - off, 8),
            1 if off + 16 <= buf.len() => {
                let large = u64::from_be_bytes([
                    buf[off + 8],
                    buf[off + 9],
                    buf[off + 10],
                    buf[off + 11],
                    buf[off + 12],
                    buf[off + 13],
                    buf[off + 14],
                    buf[off + 15],
                ]);
                (usize::try_from(large).unwrap_or(0), 16)
            }
            1 => return out,
            n => (n as usize, 8),
        };
        if size < hdr || off + size > buf.len() {
            break;
        }
        out.push((&buf[off + hdr..off + size], kind));
        off += size;
    }
    out
}

/// The child boxes of `buf` of a given kind, skipping its own header. Collected rather than
/// returned as an iterator: the lists hold a handful of boxes, and an `impl Iterator` would
/// carry `kind`'s lifetime as well as the buffer's for no useful reason.
fn children<'a>(buf: &'a [u8], header: usize, kind: &str) -> Vec<&'a [u8]> {
    let body = buf.get(header..).unwrap_or(&[]);
    boxes(body)
        .into_iter()
        .filter(|(_, k)| k == kind)
        .map(|(b, _)| b)
        .collect()
}

/// Track id → `(is_video, timescale)`, from a `moov`.
fn parse_tracks(moov: &[u8]) -> Result<HashMap<u32, (bool, u32)>> {
    let mut tracks = HashMap::new();
    for trak in children(moov, 8, "trak") {
        let Some(tkhd) = children(trak, 0, "tkhd").first().copied() else {
            continue;
        };
        // `tkhd`: version(1) flags(3) [v1: creation(8) modification(8) track_id(4)]
        //         [v0: creation(4) modification(4) track_id(4)]
        let version = tkhd.first().copied().unwrap_or(0);
        let id_at = if version == 1 { 20 } else { 12 };
        let Some(id_bytes) = tkhd.get(id_at..id_at + 4) else {
            continue;
        };
        let track_id = u32::from_be_bytes([id_bytes[0], id_bytes[1], id_bytes[2], id_bytes[3]]);

        let mdia = children(trak, 0, "mdia").first().copied().unwrap_or(&[]);
        let handler = children(mdia, 0, "hdlr").first().copied().unwrap_or(&[]);
        // `hdlr`: version(1) flags(3) pre_defined(4) handler_type(4)
        let is_video = handler.get(8..12) == Some(b"vide");
        let mdhd = children(mdia, 0, "mdhd").first().copied().unwrap_or(&[]);
        let ts_version = mdhd.first().copied().unwrap_or(0);
        let ts_at = if ts_version == 1 { 20 } else { 12 };
        let Some(ts_bytes) = mdhd.get(ts_at..ts_at + 4) else {
            continue;
        };
        let timescale = u32::from_be_bytes([ts_bytes[0], ts_bytes[1], ts_bytes[2], ts_bytes[3]]);
        tracks.insert(track_id, (is_video, timescale));
    }
    if tracks.is_empty() {
        bail!("the track table is empty: this is not an MP4 header this build understands");
    }
    Ok(tracks)
}

/// The `track_id` a `traf` refers to, from its `tfhd`.
fn track_id(traf: &[u8]) -> Option<u32> {
    let tfhd = *children(traf, 0, "tfhd").first()?;
    // `tfhd`: version(1) flags(3) track_id(4)
    let b = tfhd.get(4..8)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The `tfdt` (base media decode time) of a `traf`, in the track's timescale ticks.
fn base_media_decode_time(traf: &[u8]) -> Option<u64> {
    let tfdt = *children(traf, 0, "tfdt").first()?;
    let version = tfdt.first().copied().unwrap_or(0);
    if version == 1 {
        let b = tfdt.get(4..12)?;
        Some(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    } else {
        let b = tfdt.get(4..8)?;
        Some(u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    }
}

/// How many media ticks this `traf`'s samples span.
///
/// A fragment's own bytes carry its duration, so this is a measurement, not something to wait
/// for. Without it a ring borrows the file ledger's "the next segment proves this one's end",
/// costing a whole fragment of lag so `span_ms` under-reports by one fragment, the difference
/// between a clip of the requested length and one a fragment short (2200ms of a 3000ms request,
/// measured).
///
/// A duration can hide in two places, in ffmpeg's order:
///
/// * `trun`'s per-sample durations, when its flags say so (bit `0x000100`): the general case,
///   needed for variable frame rate;
/// * `tfhd`'s `default_sample_duration` (bit `0x000008`) times the sample count.
///
/// `None` when neither is present, which a caller must read as unknown, not zero.
fn sample_span_ticks(traf: &[u8]) -> Option<u64> {
    let tfhd = *children(traf, 0, "tfhd").first()?;
    // `tfhd`: version(1) flags(3) track_id(4), then whichever of base_data_offset (8 bytes),
    // sample_description_index, default_sample_duration, default_sample_size and
    // default_sample_flags are present, in that order, four bytes each except the first.
    let tf_flags = u32::from_be_bytes([tfhd[1], tfhd[2], tfhd[3], 0]) >> 8;
    let mut at = 8usize;
    if tf_flags & 0x00_0001 != 0 {
        at += 8; // base_data_offset
    }
    if tf_flags & 0x00_0002 != 0 {
        at += 4; // sample_description_index
    }
    let default_duration = if tf_flags & 0x00_0008 != 0 {
        let b = tfhd.get(at..at + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    } else {
        None
    };

    let trun = *children(traf, 0, "trun").first()?;
    let tr_flags = u32::from_be_bytes([trun[1], trun[2], trun[3], 0]) >> 8;
    let sample_count = u32::from_be_bytes([trun[4], trun[5], trun[6], trun[7]]);
    let mut at = 8usize;
    if tr_flags & 0x00_0001 != 0 {
        at += 4; // data_offset
    }
    if tr_flags & 0x00_0004 != 0 {
        at += 4; // first_sample_flags
    }

    if tr_flags & 0x00_0100 != 0 {
        // Per-sample durations, but not the only field: each `trun` entry carries whichever of
        // duration, size, flags and composition offset the flags select, in that order. A four-byte
        // stride sums sample sizes into the total (a one-second fragment once measured 3576ms), so the
        // entry width is what the flags say.
        let entry = 4
            + if tr_flags & 0x00_0200 != 0 { 4 } else { 0 } // sample_size
            + if tr_flags & 0x00_0400 != 0 { 4 } else { 0 } // sample_flags
            + if tr_flags & 0x00_0800 != 0 { 4 } else { 0 }; // sample_composition_time_offset
        let mut total = 0u64;
        for i in 0..sample_count as usize {
            // The index arithmetic saturates: a malformed header can declare a sample count
            // this build cannot walk, and an out-of-range read is a fragment whose length is
            // unknown, not a panic.
            let at = at.saturating_add(i.saturating_mul(entry));
            let b = trun.get(at..at.saturating_add(4))?;
            total = total.saturating_add(u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])));
        }
        return Some(total);
    }
    // No per-sample durations: every sample is `default_sample_duration` long.
    default_duration.map(|d| u64::from(d).saturating_mul(u64::from(sample_count)))
}

/// The `sample_is_non_sync_sample` bit of ISO/IEC 14496-12 sample flags.
const SAMPLE_IS_NON_SYNC: u32 = 0x0001_0000;

/// Whether the first sample of this `traf` is a sync (key) sample, from its sample flags.
///
/// The flags of the first sample can be in three places, and the spec's own order of precedence is
/// followed:
///
/// * `trun`'s `first_sample_flags` (bit `0x000004`), which is how ffmpeg marks a fragment that
///   starts on a keyframe followed by dependent frames;
/// * else the first entry's own flags, when the `trun` carries per-sample flags (`0x000400`);
/// * else `tfhd`'s `default_sample_flags` (`0x000020`).
///
/// `None` when none of them is present. The movie header's `trex` default would apply by the spec,
/// but this build does not read it, so it reports "unknown" and the caller treats that as not a
/// keyframe rather than claiming what it cannot show.
fn first_sample_is_sync(traf: &[u8]) -> Option<bool> {
    let tfhd = *children(traf, 0, "tfhd").first()?;
    let tf_flags = u32::from_be_bytes([*tfhd.get(1)?, *tfhd.get(2)?, *tfhd.get(3)?, 0]) >> 8;
    // After version/flags and track_id: the optional fields in order, four bytes each except the
    // eight-byte base_data_offset.
    let mut at = 8usize;
    if tf_flags & 0x00_0001 != 0 {
        at += 8; // base_data_offset
    }
    for bit in [0x00_0002, 0x00_0008, 0x00_0010] {
        // sample_description_index, default_sample_duration, default_sample_size
        if tf_flags & bit != 0 {
            at += 4;
        }
    }
    let default_flags = if tf_flags & 0x00_0020 != 0 {
        let b = tfhd.get(at..at + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    } else {
        None
    };

    let trun = *children(traf, 0, "trun").first()?;
    let tr_flags = u32::from_be_bytes([*trun.get(1)?, *trun.get(2)?, *trun.get(3)?, 0]) >> 8;
    let mut at = 8usize;
    if tr_flags & 0x00_0001 != 0 {
        at += 4; // data_offset
    }
    let flags = if tr_flags & 0x00_0004 != 0 {
        let b = trun.get(at..at + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    } else if tr_flags & 0x00_0400 != 0 {
        // The first entry: its duration and size come before its flags when present.
        let skip = if tr_flags & 0x00_0100 != 0 { 4 } else { 0 }
            + if tr_flags & 0x00_0200 != 0 { 4 } else { 0 };
        let b = trun.get(at + skip..at + skip + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    } else {
        default_flags
    };
    flags.map(|f| f & SAMPLE_IS_NON_SYNC == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real fragmented-MP4 stream from ffmpeg, in memory. Generated, not pasted: a hand-written
    /// box structure would test the parser against an idea of ffmpeg's output, not its actual
    /// output.
    fn stream(seconds: u32) -> Vec<u8> {
        let bin = replayed_media_ffmpeg();
        let out = crate::sidecar_command(&bin.ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=320x180:rate=30:duration={seconds}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:sample_rate=48000:duration={seconds}"),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "30",
                "-force_key_frames",
                "expr:gte(t,n_forced*1)",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-f",
                "mp4",
                "-movflags",
                "empty_moov+frag_keyframe+default_base_moof",
                "-",
            ])
            .output()
            .expect("spawning ffmpeg to generate a fragmented-MP4 stream");
        assert!(out.status.success(), "ffmpeg could not generate the stream");
        out.stdout
    }

    /// A real stream cut by time (`frag_duration`) rather than by keyframe, so fragments begin
    /// mid-GOP: 1s keyframes, a fragment every 0.5s.
    fn stream_cut_mid_gop(seconds: u32) -> Vec<u8> {
        let bin = replayed_media_ffmpeg();
        let out = crate::sidecar_command(&bin.ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=320x180:rate=30:duration={seconds}"),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "30",
                "-bf",
                "0",
                "-f",
                "mp4",
                "-frag_duration",
                "500000",
                "-movflags",
                "empty_moov+default_base_moof",
                "-",
            ])
            .output()
            .expect("spawning ffmpeg to generate a stream cut mid-GOP");
        assert!(out.status.success(), "ffmpeg could not generate the stream");
        out.stdout
    }

    /// `keyframe` is measured: a fragment that starts mid-GOP says so.
    ///
    /// It was the constant `true`, so the "the first fragment is a keyframe" check built on it could
    /// never fail, and a muxer change that cut fragments mid-GOP would have produced clips that
    /// open on garbage with every check passing.
    #[test]
    fn a_fragment_that_starts_mid_gop_is_not_claimed_to_be_a_keyframe() {
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter
            .push(&stream_cut_mid_gop(3))
            .expect("the stream splits");
        let flags: Vec<bool> = fragments.iter().map(|f| f.keyframe).collect();
        assert!(
            flags[0],
            "the first fragment opens on the first keyframe: {flags:?}"
        );
        assert!(
            flags.iter().any(|k| !k),
            "fragments cut on a timer start mid-GOP and must not claim otherwise: {flags:?}"
        );
        assert!(
            flags.iter().filter(|k| **k).count() >= 2,
            "while the ones that land on a keyframe still do: {flags:?}"
        );
    }

    /// Sample flags are found wherever the muxer put them.
    #[test]
    fn the_first_samples_flags_are_read_from_each_place_they_can_be() {
        fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out
        }
        const KEY: u32 = 0x0200_0000;
        const NON_SYNC: u32 = 0x0101_0000;
        let be = |v: u32| v.to_be_bytes();
        // `tfhd`: version/flags, track id, then the default_sample_flags field when 0x20.
        let tfhd = |default_flags: Option<u32>| {
            let flags: u32 = if default_flags.is_some() { 0x20 } else { 0 };
            let mut b = flags.to_be_bytes().to_vec();
            b.extend_from_slice(&be(1));
            if let Some(f) = default_flags {
                b.extend_from_slice(&be(f));
            }
            boxed(b"tfhd", &b)
        };
        // `trun` with `flags`, one sample, and whichever fields they select.
        let trun = |flags: u32, fields: &[u32]| {
            let mut b = flags.to_be_bytes().to_vec();
            b.extend_from_slice(&be(1));
            for f in fields {
                b.extend_from_slice(&be(*f));
            }
            boxed(b"trun", &b)
        };
        let traf = |parts: &[Vec<u8>]| parts.concat();

        // first_sample_flags (0x4) after data_offset (0x1).
        assert_eq!(
            first_sample_is_sync(&traf(&[tfhd(None), trun(0x5, &[0, KEY])])),
            Some(true)
        );
        assert_eq!(
            first_sample_is_sync(&traf(&[tfhd(None), trun(0x5, &[0, NON_SYNC])])),
            Some(false)
        );
        // The first entry's own flags (0x400), after its duration (0x100) and size (0x200).
        assert_eq!(
            first_sample_is_sync(&traf(&[tfhd(None), trun(0x700, &[10, 20, NON_SYNC])])),
            Some(false)
        );
        // Neither: tfhd's default_sample_flags.
        assert_eq!(
            first_sample_is_sync(&traf(&[tfhd(Some(KEY)), trun(0x0, &[])])),
            Some(true)
        );
        assert_eq!(
            first_sample_is_sync(&traf(&[tfhd(Some(NON_SYNC)), trun(0x0, &[])])),
            Some(false)
        );
        // Nowhere: unknown, which the splitter reads as "not shown to be a keyframe".
        assert_eq!(
            first_sample_is_sync(&traf(&[tfhd(None), trun(0x0, &[])])),
            None
        );
    }

    fn replayed_media_ffmpeg() -> crate::FfmpegBinaries {
        crate::FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
    }

    #[test]
    fn the_splitter_produces_the_header_then_one_fragment_per_second() {
        let data = stream(4);
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter.push(&data).expect("the stream splits");

        let header = splitter.header().expect("the header was seen");
        assert!(
            header.starts_with(&(28u32.to_be_bytes())),
            "the header must begin with its own `ftyp` box size"
        );
        assert_eq!(&header[4..8], b"ftyp", "and be the ftyp the muxer wrote");
        assert!(
            header.windows(4).any(|w| w == b"moov"),
            "the header must contain the moov, or a clip cannot be built from it"
        );
        assert!(
            !header.windows(4).any(|w| w == b"moof"),
            "and no fragment bytes: the header is the part that is written once"
        );

        // `-force_key_frames` is one per second, so a 4s stream is four fragments. The count is
        // ffmpeg's, so assert a band; a stream producing ONE fragment would leave a clip unable to cut
        // anything and must fail.
        assert!(
            (3..=5).contains(&fragments.len()),
            "a 4s stream at 1s fragments should be about four, got {}",
            fragments.len()
        );
        assert!(
            splitter.pending_bytes() == 0,
            "no partial box should be left over"
        );
    }

    #[test]
    fn fragment_start_times_are_monotonic_and_about_a_second_apart() {
        let data = stream(4);
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter.push(&data).expect("the stream splits");

        let starts: Vec<u64> = fragments.iter().map(|f| f.start_ms).collect();
        assert_eq!(starts[0], 0, "the first fragment starts at zero");
        assert!(
            starts.windows(2).all(|w| w[0] < w[1]),
            "media time must advance: {starts:?}"
        );
        for pair in starts.windows(2) {
            let step = pair[1] - pair[0];
            assert!(
                (700..=1_400).contains(&step),
                "fragments are forced on 1s keyframes, so a step should be about 1000ms: \
                 {starts:?}"
            );
        }
    }

    #[test]
    fn every_fragment_carries_a_moof_and_its_mdat_and_claims_a_keyframe() {
        let data = stream(3);
        let mut splitter = FragmentSplitter::new();
        for fragment in splitter.push(&data).expect("the stream splits") {
            assert!(
                fragment.keyframe,
                "frag_keyframe means every fragment starts on a keyframe, and a clip cut must \
                 start on one"
            );
            assert_eq!(
                &fragment.bytes[4..8],
                b"moof",
                "a fragment begins with its moof"
            );
            assert!(
                fragment.bytes.windows(4).any(|w| w == b"mdat"),
                "and carries the mdat with its samples, or it would be a header with no footage"
            );
        }
    }

    #[test]
    fn feeding_the_stream_one_byte_at_a_time_gives_the_same_fragments() {
        // A pipe does not deliver whole boxes. How the bytes arrive must not change what they mean.
        let data = stream(3);
        let mut all_at_once = FragmentSplitter::new();
        let expected = all_at_once.push(&data).expect("whole stream");

        let mut drip = FragmentSplitter::new();
        let mut got = Vec::new();
        for byte in &data {
            got.extend(drip.push(&[*byte]).expect("one byte at a time"));
        }

        assert_eq!(got.len(), expected.len(), "the same number of fragments");
        assert_eq!(
            got.iter().map(|f| (f.seq, f.start_ms)).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|f| (f.seq, f.start_ms))
                .collect::<Vec<_>>(),
            "with the same sequence numbers and start times"
        );
        assert_eq!(
            drip.header(),
            all_at_once.header(),
            "and the same header, however the bytes arrived"
        );
        assert_eq!(drip.pending_bytes(), 0, "and nothing left over");
    }

    #[test]
    fn a_stream_that_stops_mid_box_is_held_rather_than_guessed_at() {
        // A reader acting on a half-received `mdat` would hand the buffer a fragment with truncated
        // samples: a clip that plays and then tears.
        let data = stream(3);
        let cut = data.len() - data.len() / 4;
        let mut splitter = FragmentSplitter::new();
        let fragments = splitter
            .push(&data[..cut])
            .expect("a partial stream is not an error");
        assert!(
            splitter.pending_bytes() > 0,
            "the tail of the last box must be waiting, not consumed"
        );
        let completed = fragments.len();

        // The rest arrives: the held bytes complete into a fragment, and nothing is lost.
        let rest = splitter.push(&data[cut..]).expect("the rest of the stream");
        assert_eq!(
            completed + rest.len(),
            FragmentSplitter::new().push(&data).unwrap().len(),
            "finishing the stream must produce exactly the fragments it always had"
        );
        assert_eq!(splitter.pending_bytes(), 0);
    }
}
