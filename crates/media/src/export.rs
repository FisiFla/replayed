//! Labelled re-encodes: the export presets.
//!
//! Deliberately a separate module from [`crate::edit`], whose contract is "these never re-encode".
//! An export is the opposite of an edit: the point is to change the pixels (a smaller frame, a
//! vertical frame, a GIF), which no stream copy can do. Keeping them apart is what makes the
//! promise checkable — a reader looking for a re-encode finds it here, and a reader looking at a
//! trim finds `-c copy` and nothing else.
//!
//! Every preset writes a **new file beside the clip** and leaves the original alone. Nothing is
//! indexed: an export is an artefact meant to be uploaded, not another entry in the library, and
//! indexing it would put a derived copy of the footage under the storage policy's cap.
//!
//! The size in a label is a target, not a guarantee: these are quality-based (`-crf`), because a
//! hard byte cap (`-fs`) truncates the file mid-frame and calls the result success.

use crate::binaries::{run_with_timeout, FfmpegBinaries};
use crate::edit::COPY_FLOOR;
use anyhow::{bail, Result};
use std::path::Path;
use std::time::Duration;

/// One of the exports the window offers.
///
/// The names are the wire format (`"discord"`, `"vertical"`, `"gif"`), so a command and a test
/// agree on them and a UI cannot invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportPreset {
    /// Small enough to paste into a chat: 720p at a quality-based rate, h264 + AAC.
    Discord,
    /// 9:16, cropped from the centre: what a phone-shaped feed shows. Horizontal clips lose
    /// their sides, which is the only honest way to fill a vertical frame without letterboxing.
    Vertical,
    /// An animated GIF, muted by definition (the format has no audio), at 15fps and 480px wide.
    Gif,
}

impl ExportPreset {
    /// Every preset, in the order the panel lists them.
    pub const ALL: [ExportPreset; 3] = [
        ExportPreset::Discord,
        ExportPreset::Vertical,
        ExportPreset::Gif,
    ];

    /// The wire name, and the suffix of the file it writes.
    pub fn as_str(self) -> &'static str {
        match self {
            ExportPreset::Discord => "discord",
            ExportPreset::Vertical => "vertical",
            ExportPreset::Gif => "gif",
        }
    }

    /// The preset a command names, or `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.as_str() == text)
    }

    /// What the panel shows.
    pub fn label(self) -> &'static str {
        match self {
            ExportPreset::Discord => "Discord (720p, small)",
            ExportPreset::Vertical => "Vertical 9:16 (1080x1920)",
            ExportPreset::Gif => "GIF (480px, 15fps)",
        }
    }

    /// The file extension the export is written with.
    pub fn extension(self) -> &'static str {
        match self {
            ExportPreset::Discord | ExportPreset::Vertical => "mp4",
            ExportPreset::Gif => "gif",
        }
    }
}

/// How long an export of `duration_ms` of media may take.
///
/// A re-encode is slower than a copy by an order of magnitude or two, and a long clip on a slow
/// CPU is minutes of work; the budget scales with the media rather than being a fixed ceiling
/// that a two-minute clip would exceed. The floor covers encoder start-up.
pub fn export_budget(duration_ms: u64) -> Duration {
    let media = Duration::from_millis(duration_ms);
    // Three times the media length, plus the copy floor for the encoder's own start-up.
    media.saturating_mul(3).saturating_add(COPY_FLOOR)
}

/// Re-encode `src` into `dst` with `preset`. The original is not read back or modified.
///
/// The video is always re-encoded; the audio is re-encoded with it (AAC) for the two video
/// presets, and dropped for the GIF, which cannot carry it. `-movflags +faststart` puts the index
/// first so the file can be played while it downloads, which is what a chat client does.
pub fn export(
    bin: &FfmpegBinaries,
    src: &Path,
    dst: &Path,
    preset: ExportPreset,
    duration_ms: u64,
) -> Result<()> {
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(preset_arguments(preset))
        .arg(dst);
    let describe = format!("the {} export of {}", preset.as_str(), src.display());
    let out = run_with_timeout(cmd, export_budget(duration_ms))
        .map_err(|e| e.context(format!("running {describe}")))?;
    if !out.status.success() {
        // A partial export is not a file to hand anyone; removing it keeps a failed attempt from
        // looking like a result.
        let _ = std::fs::remove_file(dst);
        bail!(
            "{describe} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !dst.is_file() {
        bail!(
            "{describe} reported success but {} does not exist",
            dst.display()
        );
    }
    Ok(())
}

/// The preset's own ffmpeg flags: one table, so the command that runs and the command a caller
/// can inspect ([`export_arguments`]) cannot disagree.
fn preset_arguments(preset: ExportPreset) -> &'static [&'static str] {
    match preset {
        // Fit inside 1280x720 without distorting: `min(1280,iw)` for the width, height by
        // aspect, and both even (h264's 4:2:0 needs even dimensions). CRF 26 is a
        // quality-based rate: a gameplay clip lands in the low single-digit MB per minute, and a
        // still scene lands far below — which a fixed bitrate would not do.
        ExportPreset::Discord => &[
            "-vf",
            "scale='min(1280,iw)':-2:flags=lanczos",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "26",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-movflags",
            "+faststart",
        ],
        // Crop to 9:16 from the centre at the source's own height, then scale to 1080x1920.
        // `trunc(.../2)*2` keeps the crop even, which 4:2:0 requires; the height is the source's,
        // so the crop only removes width.
        ExportPreset::Vertical => &[
            "-vf",
            "crop=trunc(ih*9/16/2)*2:ih,scale=1080:1920:flags=lanczos,setsar=1",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
        ],
        // The two-pass palette is what separates a GIF that looks like the clip from a dithered
        // mess: 256 colours chosen from the clip's own frames. `-an` because a GIF has no audio
        // track and ffmpeg would otherwise fail on the missing codec.
        ExportPreset::Gif => &[
            "-vf",
            "fps=15,scale=480:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse",
            "-an",
            "-loop",
            "0",
        ],
    }
}

/// The ffmpeg arguments an export would run, for a caller that has to show them.
///
/// Not used by [`export`] for its own sake — it runs [`preset_arguments`] directly — but a
/// diagnostics view or a test can inspect the shape without encoding anything, and the test below
/// pins that no preset is a stream copy.
pub fn export_arguments(preset: ExportPreset) -> Vec<String> {
    preset_arguments(preset)
        .iter()
        .map(|arg| (*arg).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_round_trips_through_its_wire_name() {
        for preset in ExportPreset::ALL {
            assert_eq!(ExportPreset::parse(preset.as_str()), Some(preset));
            assert!(!preset.label().is_empty());
        }
        assert_eq!(ExportPreset::parse("ultrawide"), None);
        assert_eq!(ExportPreset::parse(""), None);
        // Both video presets are mp4; the GIF is not.
        assert_eq!(ExportPreset::Discord.extension(), "mp4");
        assert_eq!(ExportPreset::Vertical.extension(), "mp4");
        assert_eq!(ExportPreset::Gif.extension(), "gif");
    }

    #[test]
    fn the_export_budget_grows_with_the_media_and_has_a_floor() {
        // Short clip: the floor, not a budget derived from a few hundred milliseconds.
        assert!(export_budget(0) >= COPY_FLOOR);
        assert!(export_budget(1_000) >= COPY_FLOOR);
        // A long clip gets more, not the same ceiling.
        assert!(export_budget(600_000) > export_budget(60_000));
        assert_eq!(
            export_budget(600_000),
            Duration::from_secs(1_800) + COPY_FLOOR
        );
    }

    #[test]
    fn no_preset_is_a_stream_copy() {
        // The one property that separates an export from an edit: it decodes and encodes. A copy
        // here would be a bug that no output size would necessarily reveal — and it would produce
        // a file with the wrong frame size, or a `.gif` full of h264.
        for preset in ExportPreset::ALL {
            let args = export_arguments(preset).join(" ");
            assert!(!args.contains("-c copy"), "{:?} copies: {args}", preset);
            assert!(!args.contains("-c:v copy"), "{:?} copies: {args}", preset);
        }
        // The two video presets name h264 explicitly; the GIF lets the extension choose the gif
        // encoder, and its palette filter is what makes it an encode rather than a re-mux.
        for preset in [ExportPreset::Discord, ExportPreset::Vertical] {
            assert!(
                export_arguments(preset).join(" ").contains("-c:v libx264"),
                "{:?} does not name an encoder",
                preset
            );
        }
        let gif = export_arguments(ExportPreset::Gif).join(" ");
        assert!(gif.contains("palettegen"), "{gif}");
        assert!(gif.contains("paletteuse"), "{gif}");
        // A GIF cannot carry audio, so it must drop it rather than fail on a missing codec.
        assert!(gif.contains("-an"), "{gif}");
    }
}
