//! One frame from the real Windows capture backend, written as a PNG.
//!
//! **Ignored by default**, because it needs two things a hosted runner does not have: an
//! interactive desktop session (Windows Graphics Capture captures a session's desktop, and a
//! service or a plain SSH shell has none) and ffmpeg on `PATH`. Run it on the box from a desktop
//! session:
//!
//! ```text
//! cargo test -p replayed-capture --test wgc_frame -- --ignored --nocapture
//! ```
//!
//! This is the evidence the screenshot feature needs. Everything around the capture — the PNG
//! writer, the index row, the window — is tested on every host with the synthetic stub; this is
//! the one test that proves the pixels come from the screen, on the machine that has one.
//!
//! It opens a capture session, takes exactly one frame, and closes it. Nothing is synthesised: no
//! keystrokes, no mouse, no window, so it is safe on a machine that runs kernel-level anti-cheat.
//! The still image it compares against is the desktop itself.
#![cfg(windows)]

use replayed_capture::platform::default_video_backend;
use replayed_capture::stub::StubConfig;
use replayed_capture::target::CaptureTarget;
use std::collections::BTreeSet;
use std::time::Duration;

#[test]
#[ignore = "needs an interactive desktop session and ffmpeg on PATH"]
fn one_real_frame_is_captured_and_written_as_a_png() {
    let target = CaptureTarget::from_config(0, "");
    let mut backend = default_video_backend(
        StubConfig {
            width: 1920,
            height: 1080,
            fps: 60,
        },
        &target,
    )
    .expect("a capture backend on this machine");
    // A screenshot needs the pixels on the CPU; the zero-copy path exists for the encoder.
    backend.deliver_textures(false);

    let frame = replayed_capture::grab_frame(backend.as_mut(), Duration::from_secs(5))
        .expect("one frame from the desktop");

    println!(
        "captured {}x{} ({} bytes) at {:?}",
        frame.width,
        frame.height,
        frame.data.len(),
        frame.pts
    );
    assert!(frame.has_pixels(), "the frame must carry pixels");
    assert_eq!(
        frame.data.len(),
        (frame.width as usize) * (frame.height as usize) * 4,
        "a BGRA frame is width*height*4 bytes"
    );

    // A screen is not one colour. A flat frame means the capture returned an empty surface, which
    // is the failure mode this test exists to catch.
    let distinct: BTreeSet<[u8; 3]> = frame
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .take(5_000)
        .map(|px| [px[0], px[1], px[2]])
        .collect();
    assert!(
        distinct.len() > 1,
        "the captured frame is a single colour, which no desktop is"
    );

    let bin = replayed_media::FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    // By default the PNG goes in a temporary directory and dies with it. A verification run that
    // wants to *look* at the frame — the tray icon, the window, the empty library's checklist —
    // sets `REPLAYED_WGC_FRAME_OUT` to a path and gets to keep it.
    let keep = std::env::var_os("REPLAYED_WGC_FRAME_OUT").map(std::path::PathBuf::from);
    let dir = tempfile::tempdir().expect("a temporary directory");
    let dst = keep
        .clone()
        .unwrap_or_else(|| dir.path().join("screenshot.png"));
    if let Some(parent) = keep.as_ref().and_then(|path| path.parent()) {
        std::fs::create_dir_all(parent).expect("the frame's destination directory");
    }
    replayed_media::edit::screenshot_png(&bin, frame.width, frame.height, &frame.data, &dst)
        .expect("the frame written as a PNG");

    let info = replayed_media::MediaInfo::probe(&bin, &dst).expect("probing the PNG");
    let video = info.video.as_ref().expect("a PNG is a video stream");
    assert_eq!(
        (video.width, video.height),
        (frame.width, frame.height),
        "the PNG is the frame's own size"
    );
    assert_eq!(video.codec, "png");
    assert!(info.audio.is_none(), "a still image has no audio");
    let size = std::fs::metadata(&dst).expect("the PNG on disk").len();
    assert!(size > 1_000, "a {size}-byte PNG of a desktop is suspicious");
    println!(
        "PNG {}x{} written: {} bytes at {}",
        video.width,
        video.height,
        size,
        dst.display()
    );
}
