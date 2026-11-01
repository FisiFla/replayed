//! Real frames through each GPU vendor's encoder, by the path that works whichever GPU capture ran
//! on: ffmpeg's raw-pixel pipe.
//!
//! **Why this test exists.** `encode.vendor = "qsv"` on a machine whose capture device is on an
//! NVIDIA GPU cannot use the zero-copy path (an encoder is only handed textures on its own
//! adapter, see `replayed_encoder::mft::textures_refusal`), and the recorder answers that by
//! recording through `h264_qsv` over the pipe instead. That fallback is the product on such a
//! machine, and until this test nothing in the repository encoded a frame with it: the vendor
//! selection was exercised only through mocks, and CI's software encoder shares none of its
//! arguments. Each vendor here goes through the same two steps a recording does, the vendor
//! probe (`select_vendor`) and then `FfmpegEncoder` with real frames, and the file that comes out
//! is read back as H.264 of the size that went in.
//!
//! **Skipping is for absent hardware only.** A vendor whose GPU is not in the machine has nothing
//! to encode with and is skipped, saying so. A vendor whose GPU is here must work, and a failure
//! is a failure. The one exception is Quick Sync in session 0 (an SSH login, a service): Intel's
//! encoder cannot be created there and answers `0x80004005`, which is a property of the session
//! and not of the product, so that case says so and skips. Run it from the desktop session to
//! exercise Quick Sync.

#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use replayed_capture::stub::{StubCapture, StubConfig};
use replayed_capture::CaptureBackend;
use replayed_encoder::{EncodeConfig, Encoder, FfmpegEncoder, Vendor, VideoCodec};
use replayed_media::FfmpegBinaries;
use std::time::{Duration, Instant};

const SIZE: (u32, u32) = (1280, 720);
const FPS: u32 = 30;

/// Whether this process is in session 0, where Intel's encoder MFTs and `h264_qsv` cannot start.
fn in_session_zero() -> bool {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut session = 0u32;
    // SAFETY: `session` is the out-parameter and the process id is this process's own.
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }.is_ok() && session == 0
}

/// Whether a GPU whose name starts with `maker` is in this machine (DXGI's own description).
fn has_gpu(maker: &str) -> bool {
    replayed_encoder::mft::describe_adapters()
        .iter()
        .any(|name| name.starts_with(maker))
}

/// Record about two seconds of a moving test pattern through `vendor`'s ffmpeg encoder and read
/// the segments back.
fn record_through_ffmpeg(requested: &str, vendor: Vendor) {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    // The recording's own vendor check: an encoder that is listed but cannot encode a frame is
    // refused here, with ffmpeg's words, and that refusal is a failure on a machine that has the
    // GPU.
    let selected = replayed_encoder::select_vendor(&bin, requested, VideoCodec::H264)
        .unwrap_or_else(|err| panic!("{requested} is not usable on this machine: {err:#}"));
    assert_eq!(
        selected, vendor,
        "a named vendor is the vendor that was chosen"
    );

    let dir = tempfile::tempdir().unwrap();
    let mut cfg = EncodeConfig::hardware(
        VideoCodec::H264,
        vendor,
        SIZE,
        SIZE,
        FPS,
        8_000,
        1_000,
        dir.path().to_path_buf(),
    );
    // Video only: the audio track is not what this proves, and a loopback listener needs nothing
    // from the GPU.
    cfg.game_audio = false;
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the hardware encoder");

    let mut video = StubCapture::new(StubConfig {
        width: SIZE.0,
        height: SIZE.1,
        fps: FPS,
    });
    video.start().unwrap();
    // Real time, because the media timeline is the wall clock.
    let until = Instant::now() + Duration::from_secs(4);
    let mut frames = 0u64;
    while Instant::now() < until {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)).unwrap() {
            encoder.submit_video(frame).expect("submit a frame");
            frames += 1;
        }
    }
    // Not the full 120: ffmpeg's Quick Sync start-up takes most of a second before the pipe is read,
    // and the capture stub drops the frames that piled up behind it rather than replaying them.
    assert!(
        frames >= 30,
        "30 fps for 4 s is about 120 frames, got {frames}"
    );
    encoder
        .finish()
        .unwrap_or_else(|err| panic!("{requested} did not finish cleanly: {err:#}"));

    let mut segments: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
        .collect();
    segments.sort();
    assert!(
        !segments.is_empty(),
        "{requested} wrote no segment from {frames} frames"
    );
    let mut media_ms = 0;
    for segment in &segments {
        let info = replayed_media::MediaInfo::probe(&bin, segment).unwrap();
        let video = info.video.expect("a segment carries video");
        assert_eq!(video.codec, "h264", "{requested} writes H.264");
        assert_eq!(
            (video.width, video.height),
            SIZE,
            "at the size that went in"
        );
        media_ms += info.duration_ms;
    }
    assert!(
        media_ms >= 1_000,
        "{requested}: {frames} frames over 4 s came out as {media_ms} ms of video"
    );
    eprintln!(
        "{requested} ({}): {frames} frames in, {} segment(s), {media_ms} ms of H.264 out",
        VideoCodec::H264.hw_encoder_name(vendor),
        segments.len()
    );
}

#[test]
fn nvenc_encodes_real_frames_through_ffmpeg() {
    if !has_gpu("NVIDIA") {
        eprintln!("no NVIDIA GPU here: nothing for NVENC to encode with");
        return;
    }
    record_through_ffmpeg("nvenc", Vendor::Nvenc);
}

#[test]
fn quick_sync_encodes_real_frames_through_ffmpeg() {
    if !has_gpu("Intel") {
        eprintln!("no Intel GPU here: nothing for Quick Sync to encode with");
        return;
    }
    if in_session_zero() {
        eprintln!(
            "skipping Quick Sync: Intel's encoder cannot be created in session 0 (0x80004005); \
             run this from the desktop session"
        );
        return;
    }
    record_through_ffmpeg("qsv", Vendor::Qsv);
}
