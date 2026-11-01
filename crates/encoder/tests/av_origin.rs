//! The audio and the video start at the same instant.
//!
//! ffmpeg zeroes each input at the first thing it reads from it, so audio that began before the
//! first frame (or after it) ended up shifted by the difference for the whole file: a permanent
//! offset, not a drift. The encoder now cuts the audio at the first frame's capture time, or pads
//! it with silence up to it (`timeline::AudioOrigin`).
//!
//! What is measured is the one thing an offset cannot hide from: the length of the audio track
//! against the length of the video track. Both inputs end together, so if they start together the
//! two tracks are as long as each other, and an audio track that kept its 400 ms head start (or
//! lost 300 ms of its tail to a late start) is 300-400 ms off. The tolerance is far inside that
//! and far outside AAC's priming and the container's rounding.
//!
//! Fed in real time like every other end-to-end test here: the video is timed by when the
//! schedule writes it, and the audio by its sample count.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use replayed_capture::{AudioBuffer, AudioFormat, Frame, PixelFormat};
use replayed_encoder::{EncodeConfig, Encoder, FfmpegEncoder, VideoCodec};
use replayed_media::FfmpegBinaries;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 30;
/// One segment holds the whole run, so the two durations are measured inside one file.
const SEGMENT_MS: u64 = 30_000;
/// How long the picture runs.
const PICTURE: Duration = Duration::from_millis(1_200);
/// The most the two tracks' lengths may differ. A start-up offset is 300 ms or more here.
const TOLERANCE: Duration = Duration::from_millis(150);

fn frame_at(pts: Duration) -> Frame {
    Frame {
        data: vec![128; (WIDTH * HEIGHT * 4) as usize],
        pts,
        width: WIDTH,
        height: HEIGHT,
        format: PixelFormat::Bgra8,
        texture: None,
    }
}

fn block_at(pts: Duration) -> AudioBuffer {
    let format = AudioFormat::default();
    AudioBuffer {
        data: vec![0u8; format.bytes_per_10ms()],
        frames: format.sample_rate as usize / 100,
        pts,
        format,
    }
}

/// The length, in seconds, of the first stream of `kind` (`v` or `a`) in `path`.
fn track_seconds(bin: &FfmpegBinaries, path: &Path, kind: &str) -> f64 {
    let out = Command::new(&bin.ffprobe)
        .args(["-v", "error", "-select_streams", kind])
        .args(["-show_entries", "stream=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "ffprobe failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or_else(|| panic!("no {kind} duration in {out:?}"))
}

/// Run one recording whose audio starts `audio_starts_at` after its picture does (negative: the
/// audio starts first, by that much), and return (video, audio) lengths in seconds.
///
/// The capture clock is the test's own: it starts at `picture_origin`, which is not zero, as a
/// real monotonic clock is not.
fn record(audio_starts_at: i64) -> (f64, f64) {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch temp dir");
    let cfg = EncodeConfig::for_tests_software(
        VideoCodec::H264,
        WIDTH,
        HEIGHT,
        FPS,
        dir.path().to_path_buf(),
        SEGMENT_MS,
    );
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");

    let picture_origin = Duration::from_secs(1_000);
    let audio_origin = if audio_starts_at < 0 {
        picture_origin - Duration::from_millis(audio_starts_at.unsigned_abs())
    } else {
        picture_origin + Duration::from_millis(audio_starts_at.unsigned_abs())
    };

    // Audio that came first: delivered before any frame exists, as a capture that started early
    // would. It is held until the picture says where it begins.
    if audio_starts_at < 0 {
        let blocks = audio_starts_at.unsigned_abs() / 10;
        for index in 0..blocks {
            encoder
                .submit_audio(block_at(audio_origin + Duration::from_millis(index * 10)))
                .expect("submit early audio");
        }
    }
    let early_blocks = if audio_starts_at < 0 {
        audio_starts_at.unsigned_abs() / 10
    } else {
        0
    };

    let started = Instant::now();
    let mut next_frame = 0u32;
    let mut next_block = early_blocks;
    let frame_gap = Duration::from_micros(1_000_000 / u64::from(FPS));
    while started.elapsed() < PICTURE {
        let elapsed = started.elapsed();
        if elapsed >= frame_gap * next_frame {
            encoder
                .submit_video(frame_at(picture_origin + frame_gap * next_frame))
                .expect("submit video");
            next_frame += 1;
        }
        // Block `k` is captured, in real time, `audio_starts_at + 10k` ms after the picture began.
        while audio_starts_at + 10 * next_block as i64 <= elapsed.as_millis() as i64 {
            let pts = audio_origin + Duration::from_millis(next_block * 10);
            encoder.submit_audio(block_at(pts)).expect("submit audio");
            next_block += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    encoder.finish().expect("flush the encoder");

    let mut segments: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .collect();
    segments.sort();
    assert_eq!(segments.len(), 1, "one segment: {segments:?}");
    (
        track_seconds(&bin, &segments[0], "v:0"),
        track_seconds(&bin, &segments[0], "a:0"),
    )
}

/// Audio that began 400 ms before the first frame is cut at the frame, not kept.
#[test]
fn audio_that_started_before_the_picture_does_not_run_ahead_of_it() {
    let (video, audio) = record(-400);
    assert!(
        (video - audio).abs() < TOLERANCE.as_secs_f64(),
        "video {video:.3}s, audio {audio:.3}s: the audio kept its 400 ms head start"
    );
}

/// Audio that begins 300 ms after the first frame is padded with silence up to it.
#[test]
fn audio_that_started_after_the_picture_is_padded_up_to_it() {
    let (video, audio) = record(300);
    assert!(
        (video - audio).abs() < TOLERANCE.as_secs_f64(),
        "video {video:.3}s, audio {audio:.3}s: the audio started at the picture's start instead \
         of 300 ms into it"
    );
}
