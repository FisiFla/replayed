//! The zero-copy path at a real recording's pace: 4K at 60fps, long enough to cross segment
//! boundaries.
//!
//! **Why these tests exist.** §56 of `docs/verification-status.md` records the owner's own 4K
//! recording keeping 45 to 50 of the 60 frames a second the capture offered ("222 frames never
//! reached the hardware encoder"), while the start-up probe measured 156fps on the same encoder.
//! A probe that spins and a recording that waits are two different pipelines, and these run the
//! recording's:
//!
//! * [`a_4k_60fps_feed_keeps_up_with_the_hardware_encoder`] is the encoder's own feed, with a
//!   writer that paces each frame to the time it was captured, exactly as `pump_video` does.
//! * [`a_4k_60fps_recording_drops_nothing_across_a_segment_boundary`] is the whole thing: the same
//!   feed, the real ffmpeg child, and the segment muxer writing a file a second, which is what a
//!   recording does and what a quiet encoder never sees.
//! * [`a_stalled_output_reader_does_not_cost_the_encoder_frames`] is the same recording with the
//!   reader of ffmpeg's output stalled for two seconds, which is what the replay buffer's ring
//!   does while it assembles a clip under its lock. It is the reproduction of the owner's shortfall:
//!   without the muxer queue below, a 2 s stall dropped 375 of 602 frames, and the drops did not
//!   stop when the stall did.
//!
//! A pipeline that keeps up drops nothing: the writer burns the time the capture gives it, so the
//! encoder's queue (two deep) fills only in a burst. A shortfall shows in
//! `Encoder::dropped_frames`, the count behind "frames never reached the hardware encoder".
//!
//! **Skipping is for absent hardware only.** A machine with no hardware encoder that takes
//! textures has nothing to feed and is skipped, saying so. A machine that has one must keep up at
//! 4K60, and a failure is a failure.

#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use replayed_capture::{Frame, GpuTexture, PixelFormat};
use replayed_encoder::ffmpeg::VideoChunk;
use replayed_encoder::mft::{
    create_recording_device, synthetic_capture_texture, MftEncoderParams, MftFeed,
};
use replayed_encoder::timeline::{wait_until, FrameSchedule};
use replayed_encoder::{
    EncodeConfig, EncodeOutput, Encoder, FfmpegEncoder, RateControl, Vendor, VideoCodec, VideoInput,
};
use replayed_media::FfmpegBinaries;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

/// The recording these tests are about: the owner's 3840x2160 at 60fps.
const SIZE: (u32, u32) = (3840, 2160);
const FPS: u32 = 60;
/// The bitrate the panel asks for at 4K, so rate control budgets the real stream.
const BITRATE_KBPS: u32 = 40_000;
/// Capture hands out three textures in turn (`crates/capture/src/wgc.rs`).
const HANDOVER_RING: usize = 3;

/// How long the encoder gets to open before anything is counted.
///
/// The hardware encoder opens on the first texture handed over, so the first frames wait in the
/// two-deep queue while a driver enumerates and negotiates. A recording pays that once at start-up;
/// this is not what a shortfall during a recording looks like.
const WARM_UP: Duration = Duration::from_secs(2);

/// The hardware encoders are one shared resource: three 4K60 feeds at once exceed what one GPU can
/// encode, and every test then reports the others' drops as its own. Run them one at a time.
static HARDWARE: Mutex<()> = Mutex::new(());

/// The recording's own pace: `fps` frames a second, spread by `wait_until`.
struct Capture {
    started: Instant,
    index: u64,
    fps: u32,
}

impl Capture {
    fn new(fps: u32) -> Self {
        Self {
            started: Instant::now(),
            index: 0,
            fps,
        }
    }

    /// Wait for the next frame's slot and return its capture time.
    fn next(&mut self) -> Duration {
        let offset = Duration::from_nanos(self.index * 1_000_000_000 / u64::from(self.fps));
        self.index += 1;
        wait_until(self.started + offset);
        offset
    }
}

/// A recording device and the three textures capture's handover ring holds.
fn capture_inputs() -> Option<(
    Vec<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    windows::Win32::Graphics::Direct3D11::ID3D11Device,
)> {
    if let Some(reason) = replayed_encoder::mft::textures_refusal(
        Vendor::Nvenc,
        VideoCodec::H264,
        SIZE,
        BITRATE_KBPS,
        FPS,
    ) {
        eprintln!("no NVENC that takes capture's textures here: {reason}");
        return None;
    }
    let device = create_recording_device().expect("the device capture creates");
    let textures = (0..HANDOVER_RING)
        .map(|_| {
            synthetic_capture_texture(&device, SIZE, DXGI_FORMAT_B8G8R8A8_UNORM)
                .expect("a synthetic captured frame")
        })
        .collect::<Vec<_>>();
    Some((textures, device))
}

/// A texture carrying a detailed, moving picture rather than zeros.
///
/// The zero-filled texture the other tests use compresses to almost nothing (~22 KB/s at 4K), so
/// ffmpeg's stdout never fills the pipe and a stalled reader is invisible. A recording of a game
/// puts several megabits a second through that pipe; diagonal bands shifted per frame stand in for
/// it, and are cheap to paint once because the same three textures are reused as capture's ring.
fn painted_texture(device: &ID3D11Device, phase: u32) -> ID3D11Texture2D {
    let (width, height) = SIZE;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 4) as usize;
            pixels[at] = ((x + y + phase * 37) % 256) as u8;
            pixels[at + 1] = ((x * 2 + phase * 91) % 256) as u8;
            pixels[at + 2] = ((y * 2 + phase * 53) % 256) as u8;
            pixels[at + 3] = 255;
        }
    }
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `initial` points at `pixels`, which outlives the call, and `texture` is the out-parameter.
    unsafe { device.CreateTexture2D(&description, Some(&initial), Some(&mut texture)) }
        .expect("CreateTexture2D");
    texture.expect("CreateTexture2D returned no texture")
}

/// Hand the next captured frame to the encoder, at the capture's own pace.
fn submit_frame(
    encoder: &mut FfmpegEncoder,
    capture: &mut Capture,
    textures: &[windows::Win32::Graphics::Direct3D11::ID3D11Texture2D],
) {
    let pts = capture.next();
    let texture = textures[capture.index as usize % textures.len()].clone();
    encoder
        .submit_video(Frame {
            data: Vec::new(),
            pts,
            width: SIZE.0,
            height: SIZE.1,
            format: PixelFormat::Bgra8,
            texture: Some(GpuTexture::new(texture)),
        })
        .unwrap_or_else(|err| panic!("the encoder refused a frame: {err:#}"));
}

/// The encoder's own feed, with the recording's writer on the other side.
#[test]
fn a_4k_60fps_feed_keeps_up_with_the_hardware_encoder() {
    let _hardware = HARDWARE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((textures, _device)) = capture_inputs() else {
        return;
    };

    let (tx, rx) = mpsc::sync_channel::<VideoChunk>(4);
    let mut feed = MftFeed::spawn(
        MftEncoderParams {
            codec: VideoCodec::H264,
            bitrate_kbps: BITRATE_KBPS,
            fps: FPS,
            mode: RateControl::Cbr,
            vendor: Some(Vendor::Nvenc),
        },
        tx,
    );

    // The recording's own writer: `pump_video`, over a sink that discards the bytes, so this test
    // is about the encoder's feed and not about ffmpeg or a disk.
    let writer = std::thread::spawn(move || {
        let mut schedule = FrameSchedule::default();
        let mut chunks = 0u64;
        for chunk in rx {
            if let Some(captured_at) = chunk.captured_at {
                let wait = schedule.wait_for(captured_at, Instant::now());
                if !wait.is_zero() {
                    wait_until(Instant::now() + wait);
                }
            }
            chunks += 1;
        }
        chunks
    });

    let mut capture = Capture::new(FPS);
    // Start-up: the encoder opens on the first frame. Not counted (see WARM_UP).
    while capture.started.elapsed() < WARM_UP {
        let pts = capture.next();
        let texture = textures[capture.index as usize % textures.len()].clone();
        feed.submit(GpuTexture::new(texture), pts)
            .unwrap_or_else(|err| panic!("the feed refused a warm-up frame: {err:#}"));
    }
    let baseline = feed.dropped();

    // Measured: eight seconds of 4K60, the picture the owner's recording was.
    let measured = Instant::now();
    while measured.elapsed() < Duration::from_secs(8) {
        let pts = capture.next();
        let texture = textures[capture.index as usize % textures.len()].clone();
        feed.submit(GpuTexture::new(texture), pts)
            .unwrap_or_else(|err| panic!("the feed refused a frame: {err:#}"));
    }
    let dropped = feed.dropped() - baseline;
    let fed = capture.index;
    feed.shutdown();
    let chunks = writer.join().expect("the writer thread");

    eprintln!(
        "4K60 feed: {fed} frames fed over {:.2}s, {dropped} dropped after warm-up, \
         {chunks} chunk(s) written",
        measured.elapsed().as_secs_f64()
    );
    assert_eq!(
        dropped, 0,
        "the feed dropped {dropped} of {fed} frames after warm-up: at 4K60 the encoder's queue is \
         two deep, and a pipeline that keeps up fills it only in a burst"
    );
    assert!(
        chunks > 0,
        "no encoded frame reached the writer, so the encoder never produced anything"
    );
}

/// The whole recording, segmenting as it goes.
#[test]
fn a_4k_60fps_recording_drops_nothing_across_a_segment_boundary() {
    let _hardware = HARDWARE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((textures, _device)) = capture_inputs() else {
        return;
    };
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut cfg = EncodeConfig::hardware(
        VideoCodec::H264,
        Vendor::Nvenc,
        SIZE,
        SIZE,
        FPS,
        BITRATE_KBPS,
        // A second a segment: the shipped default (`config.example.toml`), and what puts a file
        // close, a file open and a container header in the middle of a recording every second.
        1_000,
        dir.path().to_path_buf(),
    );
    cfg.video = VideoInput::EncodedBitstream;
    // Video only: the audio path has its own queue and its own tests.
    cfg.game_audio = false;
    cfg.output = EncodeOutput::Segmented;
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the zero-copy encoder");
    assert!(
        encoder.accepts_textures(),
        "a zero-copy configuration is spawned for textures"
    );

    let mut capture = Capture::new(FPS);
    while capture.started.elapsed() < WARM_UP {
        submit_frame(&mut encoder, &mut capture, &textures);
    }
    let baseline = encoder.dropped_frames();

    let measured = Instant::now();
    while measured.elapsed() < Duration::from_secs(10) {
        submit_frame(&mut encoder, &mut capture, &textures);
    }
    let dropped = encoder.dropped_frames() - baseline;
    let fed = capture.index;
    encoder
        .finish()
        .unwrap_or_else(|err| panic!("the recording did not finish cleanly: {err:#}"));

    let segments = std::fs::read_dir(dir.path())
        .expect("the scratch directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "mp4"))
        .count();
    eprintln!(
        "4K60 recording: {fed} frames fed over {:.2}s, {dropped} dropped after warm-up, \
         {segments} segment(s) written",
        measured.elapsed().as_secs_f64()
    );
    assert_eq!(
        dropped, 0,
        "the recording dropped {dropped} of {fed} frames after warm-up: the segment muxer closing \
         and opening a file must not cost the picture a frame"
    );
    assert!(
        segments >= 8,
        "ten measured seconds at one second a segment wrote {segments} segment(s), so the segment \
         boundaries this test is about were not crossed"
    );
}

/// A reader that stops draining ffmpeg's output must not cost the encoder a frame.
///
/// This is the recording the owner made: the replay buffer's ring reads ffmpeg's stdout on its own
/// thread, and a clip request assembles the window's bytes under the ring's lock. That leaves the
/// pipe unread for as long as the assembly takes, ffmpeg's stdout write blocks, and — with the
/// muxer's packet queue at ffmpeg's default of eight packets — ffmpeg stops reading `pipe:0` too.
/// The video queue (four encoded chunks, ~67ms at 60fps) then fills, the hardware encoder's
/// two-deep feed fills behind it, and the recording drops a burst of frames while the user is only
/// taking a clip.
///
/// The stall here is deliberate and in the reader: the encoder side is exactly the shipping path,
/// and nothing is asked of the GPU that a quiet desktop cannot do.
#[test]
fn a_stalled_output_reader_does_not_cost_the_encoder_frames() {
    let _hardware = HARDWARE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((_plain, device)) = capture_inputs() else {
        return;
    };
    let textures = (0..HANDOVER_RING)
        .map(|phase| painted_texture(&device, phase as u32))
        .collect::<Vec<_>>();
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut cfg = EncodeConfig::hardware(
        VideoCodec::H264,
        Vendor::Nvenc,
        SIZE,
        SIZE,
        FPS,
        BITRATE_KBPS,
        1_000,
        dir.path().to_path_buf(),
    );
    cfg.video = VideoInput::EncodedBitstream;
    cfg.game_audio = false;
    // The replay buffer's output: fragmented MP4 on stdout, read by the in-memory ring.
    cfg.output = EncodeOutput::FragmentedStream;
    let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the zero-copy encoder");
    let mut stdout = encoder
        .take_output_stream()
        .expect("fragmented stream output on stdout");

    // The stall begins once the recording is under way (three seconds of output), lasts long
    // enough to bury the queues, and the reader then drains normally. It is announced so the feed
    // can tell the frames it cost from the frames a busy machine cost on its own.
    let stalling = Arc::new(AtomicBool::new(false));
    let reader_stalling = Arc::clone(&stalling);
    let reader = std::thread::spawn(move || {
        let start = Instant::now();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut bytes = 0u64;
        let mut stall_at = Some(start + Duration::from_secs(3));
        loop {
            if stall_at.is_some_and(|at| Instant::now() >= at) {
                stall_at = None;
                reader_stalling.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2_000));
                reader_stalling.store(false, Ordering::SeqCst);
            }
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => bytes += read as u64,
            }
        }
        bytes
    });

    let mut capture = Capture::new(FPS);
    while capture.started.elapsed() < WARM_UP {
        submit_frame(&mut encoder, &mut capture, &textures);
    }
    let baseline = encoder.dropped_frames();

    let mut at_stall = None;
    let mut at_resume = None;
    let measured = Instant::now();
    while measured.elapsed() < Duration::from_secs(8) {
        submit_frame(&mut encoder, &mut capture, &textures);
        let dropped = encoder.dropped_frames();
        if at_stall.is_none() && stalling.load(Ordering::SeqCst) {
            at_stall = Some(dropped);
        } else if at_stall.is_some() && at_resume.is_none() && !stalling.load(Ordering::SeqCst) {
            at_resume = Some(dropped);
        }
    }
    let dropped = encoder.dropped_frames() - baseline;
    let stalled = at_stall.expect("the reader's stall fell inside the measured window") - baseline;
    let resumed = at_resume.expect("the reader came back inside the measured window") - baseline;
    let after = dropped - resumed;
    let fed = capture.index;
    encoder
        .finish()
        .unwrap_or_else(|err| panic!("the recording did not finish cleanly: {err:#}"));
    let bytes = reader.join().expect("the output reader");

    eprintln!(
        "4K60 recording with a 2s reader stall: {fed} frames fed over {:.2}s, \
         {dropped} dropped after warm-up ({stalled} before the stall, {} during it, {after} \
         after the reader resumed), {bytes} byte(s) read ({:.2} MB/s)",
        measured.elapsed().as_secs_f64(),
        resumed - stalled,
        bytes as f64 / measured.elapsed().as_secs_f64() / 1_000_000.0
    );
    assert!(
        bytes > 0,
        "the reader got nothing, so the stall was not the one this test is about"
    );
    // The defect is the cascade: once the stall overflowed the queue, the schedule kept the
    // writer a whole stall behind and the input queue dropped every frame for the rest of the
    // recording (375 of 602 in the reproduction). A busy machine may cost a frame or two during
    // the stall itself; it must not cost any once the reader is back.
    assert_eq!(
        after,
        0,
        "the reader resumed and the recording still dropped {after} of {fed} frames afterwards: \
         a stall must not become a shortfall that never recovers (before {stalled}, during {}, \
         after {after})",
        resumed - stalled
    );
    // A four-packet queue overflows within ~70ms and loses every frame of a 2 s stall (~120);
    // the muxer queue holds four seconds, so anything near that count means it is not in effect.
    assert!(
        resumed - stalled <= 60,
        "the 2 s stall dropped {} of {fed} frames while the reader was away: ffmpeg's muxer queue \
         must keep the demuxer reading the video pipe while the output is blocked",
        resumed - stalled
    );
}
