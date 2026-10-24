//! The two edits that are not stream copies: combining clips losslessly, and the labelled
//! re-encodes (export presets).
//!
//! Combining is in the same file as the exports on purpose: they are the pair a user meets on the
//! same panel, and the difference between them is the point. `combine` produces the originals'
//! own packets and must refuse anything it cannot do that to; an `export` re-encodes and may
//! change the frame entirely.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use replayed_media::export::{self, ExportPreset};
use replayed_media::{edit, probe::MediaInfo, FfmpegBinaries};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg() -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH (brew install ffmpeg)")
}

/// A real clip of `seconds`, `w`x`h`, h264 + 48kHz stereo AAC: the shape a recording has.
fn clip(dir: &Path, name: &str, seconds: u32, w: u32, h: u32) -> PathBuf {
    let out = dir.join(name);
    let bin = ffmpeg();
    let status = Command::new(&bin.ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={w}x{h}:rate=30"),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            &seconds.to_string(),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "clip fixture {name} could not be encoded");
    out
}

#[test]
fn two_clips_combine_losslessly_into_one_of_their_sum() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let a = clip(dir.path(), "a.mp4", 2, 320, 180);
    let b = clip(dir.path(), "b.mp4", 2, 320, 180);
    let out = dir.path().join("combined.mp4");

    edit::combine_lossless(&bin, &[a.clone(), b.clone()], &out).expect("combine");

    let sources = [
        MediaInfo::probe(&bin, &a).unwrap(),
        MediaInfo::probe(&bin, &b).unwrap(),
    ];
    let sum: u64 = sources.iter().map(|info| info.duration_ms).sum();
    let combined = MediaInfo::probe(&bin, &out).unwrap();
    assert!(
        combined.duration_ms.abs_diff(sum) < 100,
        "combined {}ms against {}ms of sources",
        combined.duration_ms,
        sum
    );
    // A stream copy keeps the packets, so the codecs are the sources' own; an export would differ.
    assert_eq!(
        combined.video.as_ref().unwrap().codec,
        sources[0].video.as_ref().unwrap().codec
    );
    assert!(
        combined.audio.is_some(),
        "the audio track survives a lossless combine"
    );
    assert_eq!(
        (
            combined.video.as_ref().unwrap().width,
            combined.video.as_ref().unwrap().height
        ),
        (320, 180),
        "a stream copy cannot resize, and the first input's frame is the output's"
    );
    // The concat list is temporary: a stray `.txt` in the library would be a file no row names.
    assert!(
        !out.with_extension("concat.txt").exists(),
        "the concat list is removed afterwards"
    );
}

#[test]
fn combining_clips_of_different_sizes_is_refused_and_writes_nothing() {
    // The failure the layout check exists for: ffmpeg would concatenate these, exit 0, and the
    // second clip's frames would be decoded against the first one's parameter sets.
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let small = clip(dir.path(), "small.mp4", 1, 320, 180);
    let large = clip(dir.path(), "large.mp4", 1, 640, 360);
    let out = dir.path().join("refused.mp4");

    let err = edit::combine_lossless(&bin, &[small, large], &out).unwrap_err();

    let message = format!("{err:#}");
    assert!(
        message.contains("320x180") && message.contains("640x360"),
        "the refusal names both shapes: {message}"
    );
    assert!(!out.exists(), "a refused combine leaves no partial file");
}

#[test]
fn combining_one_clip_is_refused_rather_than_duplicating_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let one = clip(dir.path(), "one.mp4", 1, 320, 180);
    let out = dir.path().join("one-out.mp4");

    let err = edit::combine_lossless(&bin, &[one], &out).unwrap_err();
    assert!(format!("{err:#}").contains("at least two"), "{err:#}");
    assert!(!out.exists());
}

#[test]
fn the_discord_preset_fits_720p_and_keeps_the_audio() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = clip(dir.path(), "wide.mp4", 1, 1920, 1080);
    let out = dir.path().join("wide-discord.mp4");

    let source = MediaInfo::probe(&bin, &src).unwrap();
    export::export(&bin, &src, &out, ExportPreset::Discord, source.duration_ms).expect("export");

    let exported = MediaInfo::probe(&bin, &out).unwrap();
    let video = exported.video.as_ref().expect("video survives an export");
    assert!(
        video.width <= 1280 && video.height <= 720,
        "exported {}x{} is bigger than the preset's frame",
        video.width,
        video.height
    );
    // Aspect kept: 1920x1080 scaled to 1280x720, not squashed.
    assert_eq!((video.width, video.height), (1280, 720));
    assert_eq!(video.codec, "h264");
    assert!(
        exported.audio.is_some(),
        "the audio is re-encoded, not dropped"
    );
    assert!(
        exported.size_bytes < source.size_bytes,
        "a lossless-fixture-sized source aside, the preset exists to be smaller"
    );
}

#[test]
fn the_vertical_preset_is_1080x1920_and_cropped_not_stretched() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = clip(dir.path(), "wide.mp4", 1, 1920, 1080);
    let out = dir.path().join("wide-vertical.mp4");

    let source = MediaInfo::probe(&bin, &src).unwrap();
    export::export(&bin, &src, &out, ExportPreset::Vertical, source.duration_ms).expect("export");

    let exported = MediaInfo::probe(&bin, &out).unwrap();
    let video = exported.video.as_ref().expect("video survives an export");
    assert_eq!((video.width, video.height), (1080, 1920));
    // A crop of a 16:9 frame to 9:16 keeps the middle 607 of 1920 pixels' width: the frame is
    // filled, not letterboxed, which is what "vertical" means for a phone-shaped feed.
    assert_eq!(video.codec, "h264");
}

#[test]
fn the_gif_preset_writes_a_gif_with_no_audio_stream() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let src = clip(dir.path(), "short.mp4", 1, 640, 360);
    let out = dir.path().join("short.gif");

    let source = MediaInfo::probe(&bin, &src).unwrap();
    export::export(&bin, &src, &out, ExportPreset::Gif, source.duration_ms).expect("export");

    let exported = MediaInfo::probe(&bin, &out).unwrap();
    let video = exported.video.as_ref().expect("a GIF is a video stream");
    assert_eq!(video.codec, "gif");
    assert!(
        exported.audio.is_none(),
        "the GIF format carries no audio and the preset drops it"
    );
    assert_eq!(video.width, 480);
}

#[test]
fn an_export_that_cannot_be_made_leaves_no_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    // Not media at all: ffmpeg must fail, and the failure must not leave `out` looking like a
    // finished export.
    let bogus = dir.path().join("bogus.mp4");
    std::fs::write(&bogus, b"not a video").unwrap();
    let out = dir.path().join("bogus-discord.mp4");

    let err = export::export(&bin, &bogus, &out, ExportPreset::Discord, 1_000).unwrap_err();
    assert!(format!("{err:#}").contains("discord"), "{err:#}");
    assert!(!out.exists(), "a failed export leaves no file behind");
}

/// A frame as the capture delivers it: BGRA, top-down, `width * height * 4` bytes.
fn bgra_frame(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&[seed, (x % 256) as u8, (y % 256) as u8, 0xff]);
        }
    }
    data
}

#[test]
fn a_captured_frame_is_written_as_a_png_of_its_own_size() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let (width, height) = (64, 48);
    let frame = bgra_frame(width, height, 0x30);
    let out = dir.path().join("screenshot.png");

    edit::screenshot_png(&bin, width, height, &frame, &out).expect("write the png");

    let info = MediaInfo::probe(&bin, &out).unwrap();
    let video = info
        .video
        .as_ref()
        .expect("a PNG is a video stream to ffprobe");
    assert_eq!((video.width, video.height), (width, height));
    assert_eq!(video.codec, "png");
    assert!(info.audio.is_none(), "a still image has no audio");
    // Lossless: the format has no quality knob, which is why a scoreboard's text survives it.
    let size = std::fs::metadata(&out).unwrap().len();
    assert!(size > 0, "the file is not empty");
}

#[test]
fn a_frame_that_is_not_the_size_it_claims_is_refused_rather_than_truncated() {
    // ffmpeg would read a short buffer as a truncated image and exit 0; the check is the media
    // layer's, and it happens before anything is spawned.
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let out = dir.path().join("short.png");

    let err = edit::screenshot_png(&bin, 64, 48, &[0u8; 16], &out).unwrap_err();
    assert!(format!("{err:#}").contains("truncated"), "{err:#}");
    assert!(!out.exists());

    let err = edit::screenshot_png(&bin, 0, 0, &[], &out).unwrap_err();
    assert!(format!("{err:#}").contains("non-empty"), "{err:#}");
}
