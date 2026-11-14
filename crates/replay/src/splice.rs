//! Turning a segment window into a single lossless clip file.

use crate::window::SegmentWindow;
use anyhow::{bail, Result};
use replayed_media::{edit, FfmpegBinaries};
use std::path::{Path, PathBuf};

/// The concat-list path escaping, re-exported for existing importers. It lives in
/// `replayed_media::edit` beside the concat invocation whose list it renders; the splicer and
/// the recorder's session finalise both build lists, and two copies of the Windows escaping are
/// two places to get it wrong.
pub use replayed_media::edit::concat_list_path;

#[derive(Debug, Clone)]
pub struct ClipMetadata {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub encoder: String,
}

pub struct ClipSplicer;

impl ClipSplicer {
    /// Concatenate whole segments with `-c copy`, keeping every stream (plus the voice mix of
    /// `edit::concat_clip_sized` when there is a microphone).
    ///
    /// No leading-segment trim: segment boundaries are keyframes, so `-ss`/`-c copy` would resolve
    /// to the same frame and change nothing.
    ///
    /// The layout check and size-derived budget are shared with the recorder's session finalise,
    /// being the same operation. Without them this path called `concat_lossless` with no `-map` and
    /// silently dropped the microphone track from any clip cut from a session recorded with one,
    /// with exit status 0 (see `replayed_media::edit::concat_lossless`).
    pub fn splice(
        bin: &FfmpegBinaries,
        window: &SegmentWindow,
        out: &Path,
        encoder: &str,
    ) -> Result<ClipMetadata> {
        if window.segments.is_empty() {
            bail!("cannot splice an empty segment window");
        }
        let files: Vec<PathBuf> = window.segments.iter().map(|s| s.file.clone()).collect();

        // Before anything is written: every segment must share one stream layout, or the concatenation
        // drops or truncates a track without ffmpeg reporting it. The returned layout also lets the
        // concat re-name the audio tracks, since `-c copy` carries no per-stream metadata.
        let layout = edit::check_concat_layout(bin, &files)?;

        let list = out.with_extension("concat.txt");
        let total_bytes = edit::write_concat_list(&files, &list)?;
        // The list is removed whether the copy succeeded or not.
        let concat = edit::concat_clip_sized(bin, &list, out, total_bytes, layout.audio_count());
        let _ = std::fs::remove_file(&list);
        concat?;

        let info = probe_or_remove(bin, out)?;
        log_drift(out, &info);

        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }

    /// Concatenate a range of in-memory fragments into one lossless clip file.
    ///
    /// The counterpart of [`ClipSplicer::splice`] for a RAM ring, with the same `-c copy`, refusal
    /// to re-encode, A/V drift report and `ClipMetadata`. Only the footage source differs: bytes
    /// never written to disk instead of a list of files.
    ///
    /// The window is checked before ffmpeg is spawned, each being silent damage the muxer would
    /// not raise:
    ///
    /// * an empty window has nothing to cut;
    /// * a first fragment that is not a keyframe opens the clip mid-GOP and its first frames decode
    ///   as garbage. `frag_keyframe` prevents it from this ring, which is why it is asserted;
    /// * an unclosed window would put a fragment with an unproven length at the tail of a file whose
    ///   duration then lies. [`crate::MemoryRingBuffer::window`] selects only closed fragments;
    ///   this checks the invariant at the point of use.
    pub fn splice_from_memory(
        bin: &FfmpegBinaries,
        window: &crate::ram_buffer::MemoryWindow<'_>,
        out: &Path,
        encoder: &str,
    ) -> Result<ClipMetadata> {
        window.validate()?;
        let audio_tracks = window.audio_tracks();
        Self::splice_stream(bin, window.assemble(), out, encoder, audio_tracks)
    }

    /// The same save path, from bytes a caller has already assembled.
    ///
    /// A recorder holds its ring behind a lock and a [`crate::ram_buffer::MemoryWindow`] borrows
    /// the ring, so bytes must be lifted out before the lock drops (see
    /// [`crate::ram_buffer::MemoryWindow::validate`] for why holding it across the muxer would
    /// stall recording). Callers on this path run `validate` themselves.
    pub fn splice_stream(
        bin: &FfmpegBinaries,
        bytes: Vec<u8>,
        out: &Path,
        encoder: &str,
        audio_tracks: usize,
    ) -> Result<ClipMetadata> {
        replayed_media::edit::remux_stream_lossless(bin, bytes, out, audio_tracks)?;

        let info = probe_or_remove(bin, out)?;
        log_drift(out, &info);

        Ok(ClipMetadata {
            path: out.to_path_buf(),
            duration_ms: info.duration_ms,
            size_bytes: info.size_bytes,
            encoder: encoder.to_string(),
        })
    }
}

/// Probe the clip just written; a file ffprobe cannot read is removed before the error returns.
///
/// The caller gets an error and no `ClipMetadata`, so nothing would ever index the file: left in
/// place it is an unnamed, unaccounted file in the library directory.
fn probe_or_remove(bin: &FfmpegBinaries, out: &Path) -> Result<replayed_media::MediaInfo> {
    replayed_media::MediaInfo::probe(bin, out).inspect_err(|_| {
        let _ = std::fs::remove_file(out);
    })
}

/// Log the A/V offset of a clip just written. One definition for both splice paths, so a RAM
/// clip's log line and a file clip's report the same measurement.
///
/// The offset is within the produced clip (audio against video stream timelines of the muxed
/// file), not the QPC-clock divergence between the live capture sources, which would need
/// instrumenting the capture path.
fn log_drift(out: &Path, info: &replayed_media::MediaInfo) {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    match info.av_drift() {
        Some(d) => tracing::info!(
            "clip {stem}: video {}ms audio {}ms drift {}ms",
            d.video_ms,
            d.audio_ms,
            d.delta_ms
        ),
        None => tracing::warn!(
            "clip {stem}: A/V drift unavailable — ffprobe reported no per-stream \
             duration (video {}ms, audio {}ms); the streams are present but the \
             offset cannot be computed honestly",
            opt_ms(info.video.as_ref().and_then(|v| v.duration_ms)),
            opt_ms(info.audio.as_ref().and_then(|a| a.duration_ms)),
        ),
    }
}

/// Render an optional millisecond count for the log line, saying "unavailable" rather than
/// pretending a missing value is 0.
fn opt_ms(ms: Option<u64>) -> String {
    ms.map(|m| m.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip whose post-write probe fails is removed rather than left unindexed.
    #[test]
    fn a_clip_that_cannot_be_probed_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.mp4");
        std::fs::write(&out, b"not a video").unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        probe_or_remove(&bin, &out).expect_err("the probe cannot run");
        assert!(!out.exists(), "no file is left for nothing to index");
    }
}
