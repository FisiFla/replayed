//! One ffmpeg process, a pipe video input, up to two optional loopback-TCP audio inputs,
//! segmented MP4 output.
//!
//! Video arrives on `pipe:0` (the child's stdin). Audio arrives on `tcp://127.0.0.1:{port}`: we
//! bind a listener on an ephemeral loopback port and ffmpeg dials it (for an input URL ffmpeg's
//! tcp protocol is the client).
//!
//! ## Inputs
//!
//! | input | stream | transport |
//! |---|---|---|
//! | 0 | system/game audio ([`EncodeConfig::game_audio`]) | `tcp://127.0.0.1:{game_audio_port}` |
//! | 1 | microphone (`EncodeConfig::mic_audio`) | `tcp://127.0.0.1:{mic_audio_port}` |
//! | last | raw BGRA video | `pipe:0` |
//!
//! **The audio is declared first on purpose** (issue #78). ffmpeg opens its inputs in the order
//! they are declared, and the raw video input's *probe blocks*: a raw H.264 demuxer cannot probe
//! the stream until it has seen a keyframe carrying the sequence header. With the video declared
//! first, the child sat inside that probe and **never opened the audio socket**, so the recorder's
//! audio writer waited out its ten-second connect deadline for a connection that could not happen
//! and the recording died before it began. With the audio first, the child dials the audio socket
//! before it looks at the video at all.
//!
//! Input 0 exists only when `game_audio` is true and input 1 only when `mic_audio` is `Some`.
//! With `game_audio` off there is no audio input at all (no listener, no `-c:a`, no audio
//! stream). The output is mapped explicitly, and the mapping is what fixes the places the tracks
//! are named by: the video first, then the game audio (`Game Audio`), then the microphone
//! (`Microphone`) — the order the file has always had. A microphone with the game audio off is
//! refused at [`FfmpegEncoder::spawn`], because a one-track container would carry it as
//! `Game Audio`.
//!
//! The microphone is pumped by the same [`pump_audio`] on its own thread: PCM is written as
//! submitted, the media timeline is the sample count, and a full queue drops and counts a block
//! rather than blocking capture ([`enqueue_or_drop`]). One writer thread per input is
//! load-bearing: ffmpeg will not pull one input far ahead of another, so writing two inputs
//! from one thread deadlocks as soon as either sink's buffer fills.
//!
//! ## Why audio does not come in on `pipe:1`
//!
//! Reading the child's stdout (`-i pipe:1`) is a POSIX accident: ffmpeg calls a plain CRT
//! `read(fd, ...)`, Windows opens descriptor 1 write-only, so it fails with `EBADF` and the
//! input is dead. It passed on macOS while broken on the shipping platform. Loopback TCP is
//! supported on every platform, so it is one code path with no `#[cfg]` that the macOS suite
//! exercises.
//!
//! ## This is IPC, not egress
//!
//! The listener is bound to `127.0.0.1` (never a wildcard) and the accepted peer is checked to be
//! loopback, so neither the port nor the peer can leave the host. The zero-egress rule
//! (`crates/events/tests/no_egress.rs`) is about outbound traffic.
//!
//! ## The media timeline is the wall clock, not the frame count
//!
//! WGC delivers one frame per compositor tick, which need not be the configured `encode.fps`.
//! Stamping from a declared rate (`-r 30`) lets the media timeline advance at `frames / 30`
//! while the wall clock advances at `frames / 36`: 25.4s of wall clock produced 19.0s of media,
//! so `trigger_ms + post_ms` was never reached and every hotkey press timed out.
//!
//! Video timestamps are therefore the arrival time of each frame
//! (`-use_wallclock_as_timestamps 1`), so segments cover real seconds and a capture stall shows
//! as a gap, not a slower clock. The capture loop also rate-limits itself to `encode.fps`
//! (`FramePacer`), so normally the two agree.
//!
//! ## The wall clock at the moment the child *reads* the frame is not the frame's time
//!
//! `-use_wallclock_as_timestamps 1` stamps a frame with the wall clock at the moment the child
//! read it out of the pipe. That is a reading time, not a capture time, and the two come apart
//! as soon as the child reads in bursts: the hardware encoder finishes several frames at a time,
//! so the reads came in bursts too, and the frames of a burst shared a timestamp. A 4K recording
//! measured frame intervals swinging between 8.1 ms and 36.4 ms against the 16.7 ms a 60fps
//! video has — only 2.3% of them right — which is a picture that judders and lags.
//!
//! So the capture time of every frame is carried to the pipe ([`VideoChunk::captured_at`]) and
//! the pipe is written *at* those times ([`crate::timeline::FrameSchedule`]). The child then reads
//! the frames one at a time, spread out the way the picture was, and the wall clock it stamps them
//! with is the capture clock. The frames' gaps are the capture's own gaps, so the video is as
//! long as the picture was: the duration does not move, and neither does the A/V alignment.
//!
//! The nominal rate is declared with the rawvideo demuxer's own `-framerate`, never the CLI's
//! input `-r`: `-r` makes ffmpeg re-stamp every frame onto a rigid 1/fps grid, throwing the
//! arrival time away (`-debug_ts`: a 15fps pipe with `-r 30` reached the muxer as pts 0, 1/30,
//! 2/30; with `-framerate 30` as 0, 0.100, 0.233).
//!
//! Audio is not stamped that way: raw s16le PCM at 48kHz muxed from the running sample count is
//! exact, while socket-arrival time would add jitter. Audio is drained in full, never
//! rate-limited, because dropping it to pace video would desync the clip. The residual drift
//! between the audio device's clock and the system clock is what `MediaInfo::av_drift` measures.
//!
//! ## Threading
//!
//! Each input is drained by its own writer thread fed by a bounded `mpsc` channel, so
//! `submit_video`/`submit_audio` never block the caller. The payload's buffer is moved into the
//! channel, not cloned (a 3840x2160 BGRA frame is 33.2MB).

use crate::timeline::FrameSchedule;
use crate::{EncodeConfig, EncodeOutput, Encoder, RateControl, SampleFormat, VideoInput};
use anyhow::{bail, Context, Result};
use replayed_capture::{AudioBuffer, Frame};
use replayed_media::FfmpegBinaries;
use std::ffi::OsString;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the audio thread waits for ffmpeg to dial the loopback input. ffmpeg connects while
/// opening its inputs, so this only elapses when the child failed to start at all.
const AUDIO_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for that connection (the listener is non-blocking).
const AUDIO_CONNECT_POLL: Duration = Duration::from_millis(20);

/// How long a hardware encoder is waited on, for its next request for input and for its tail
/// after a drain.
///
/// A timeout here is a report, not a hang. Two seconds is far longer than either takes on a
/// healthy machine (90 frames of 4K through the loop took three seconds) and short enough that
/// a wedged encoder is reported during the recording rather than after it.
#[cfg(windows)]
pub(crate) const MFT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a failure report waits for ffmpeg's stderr. The text is normally already in the pipe
/// buffer, so this only bounds a pathological case (see `drain_stderr`).
const STDERR_DRAIN_BUDGET: Duration = Duration::from_secs(1);

/// How long a writer failure waits for ffmpeg to become reapable so its exit status and stderr
/// can be reported as the cause (see `FfmpegEncoder::ffmpeg_death`).
///
/// The broken pipe is noticed a few milliseconds before the process can be waited on. Spent only
/// on a path that has already failed; past the deadline the error carries the symptom alone.
const CHILD_DEATH_GRACE: Duration = Duration::from_secs(1);

/// Poll interval while waiting out that grace period.
const CHILD_DEATH_POLL: Duration = Duration::from_millis(10);

/// How many video frames may sit between the capture loop and the writer thread before submits
/// are dropped.
///
/// Bounded because an unbounded queue grows without limit whenever ffmpeg reads slower than
/// capture produces. Small because the frame dominates: at 3840x2160 BGRA one frame is
/// 33_177_600 B, so four queued frames plus the one the writer holds is ~166 MB worst case. At
/// 30fps four frames ride out ~133 ms of ffmpeg not reading, enough to cover a segment being
/// finalised, far short of letting a wedged encoder accumulate a gigabyte.
const VIDEO_QUEUE_FRAMES: usize = 4;

/// How many 10 ms audio blocks may be queued before audio submits are dropped.
///
/// Blocks are 1920 B at 48kHz stereo s16le, so this is ~576 KB: three seconds of slack. The drop
/// path exists for an encoder that is permanently behind. Windows ffmpeg stops reading for a
/// second or so at the first segment's boundary, and at 320 ms of slack that transient discarded
/// 760 ms of a 3 s feed for good, so the slack is measured in seconds. The microphone's queue
/// (`FfmpegEncoder::mic_tx`) is sized the same way.
const AUDIO_QUEUE_BLOCKS: usize = 300;

/// The format the game/system audio input is declared with: raw s16le PCM at 48kHz stereo.
///
/// Named because the microphone input is declared by the same function ([`audio_input_args`]),
/// so both inputs share one clock discipline mechanically. These are the pipeline's published
/// values (`replayed_capture::AudioFormat::default`), not settings.
const GAME_AUDIO_SAMPLE_RATE: u32 = 48_000;
const GAME_AUDIO_CHANNELS: u16 = 2;

/// What an audio input is called in error messages and the writer thread's name. The game audio
/// keeps the original wording; the second input names itself.
const GAME_AUDIO_LABEL: &str = "audio";
const MICROPHONE_LABEL: &str = "microphone";

/// Bytes queued by the caller, written to the child by a dedicated thread.
type WriterHandle = JoinHandle<std::io::Result<()>>;

/// One piece of video on its way to the child, with the time the frame it carries was captured.
///
/// The capture time is what the child's timeline is built from. ffmpeg stamps a frame with the
/// wall clock at the moment it *read* the frame from the pipe, so the pipe has to be written at
/// the times the frames were captured — see [`crate::timeline::FrameSchedule`].
///
/// `captured_at` is `None` for a chunk that is not a frame: the hardware encoder's sequence
/// header, which has to be in front of the first frame and is written as soon as it can be.
#[derive(Debug)]
pub struct VideoChunk {
    /// The bytes: raw pixels, or a piece of an encoded elementary stream.
    pub bytes: Vec<u8>,
    /// When the frame was captured, on the pipeline's clock.
    pub captured_at: Option<Duration>,
}

/// Seconds per scratch segment, which is also the forced-keyframe interval and so what makes a
/// clip a lossless concatenation of whole segments.
pub fn keyframe_seconds(cfg: &EncodeConfig) -> f64 {
    cfg.segment_ms as f64 / 1000.0
}

/// The GOP that puts a keyframe at every segment boundary.
fn gop_frames(cfg: &EncodeConfig) -> u32 {
    (cfg.fps as f64 * keyframe_seconds(cfg)).round().max(1.0) as u32
}

/// The ffmpeg arguments that declare the video input and its rate.
///
/// Shared by the recording child ([`FfmpegEncoder::spawn`]) and the startup throughput probe
/// (`crate::throughput::measure_sustainable_fps`): a measurement of a different invocation is not
/// a measurement of this one, so geometry, pixel format and declared rate must match.
///
/// `-framerate` is the rawvideo demuxer's own option, not the CLI's input `-r`, which would
/// re-stamp every frame onto a rigid 1/fps grid and discard the arrival timestamp (see the
/// module comment).
pub fn video_input_args(cfg: &EncodeConfig) -> Vec<String> {
    let mut args: Vec<String> = ["-f", "rawvideo", "-pix_fmt", "bgra"]
        .map(str::to_string)
        .to_vec();
    // `-s` sizes the incoming rawvideo stream, so it must be the SOURCE size, never the output size.
    args.extend([
        "-s".to_string(),
        format!("{}x{}", cfg.source_size.0, cfg.source_size.1),
    ]);
    // The nominal rate: one number for this argument and the pacer (`replayed_recorder::FpsDecision`).
    args.extend(["-framerate".to_string(), cfg.fps.to_string()]);
    // Timestamps are each frame's arrival time, which keeps `segments * segment_time` real seconds
    // even when capture delivers at a rate other than `cfg.fps`. Video only: audio's timeline is the
    // exact sample count and must not be jittered by socket arrival.
    args.extend(["-use_wallclock_as_timestamps".to_string(), "1".to_string()]);
    args.extend(["-i".to_string(), "pipe:0".to_string()]);
    args
}

/// The ffmpeg arguments for an already-encoded video input: an H.264 elementary stream on a pipe,
/// produced by this process rather than by ffmpeg.
///
/// The hybrid's video half: a hardware encoder MFT takes the captured texture from VRAM
/// (`replayed_encoder::mft`), so a few hundred kilobytes a second of H.264 cross the pipe
/// instead of ~2 GB/s of raw pixels.
///
/// The timeline flag is the same as in [`video_input_args`]: an elementary stream carries no
/// timestamps, so ffmpeg must stamp from arrival. Streaming the output as produced gave 965 ms of
/// container for 990 ms of paced frames; handing the same bytes over in one batch gave 196 ms.
pub fn video_input_args_bitstream(codec: crate::VideoCodec) -> Vec<String> {
    [
        "-f",
        codec.bitstream_format(),
        "-use_wallclock_as_timestamps",
        "1",
        "-i",
        "pipe:0",
    ]
    .map(str::to_string)
    .to_vec()
}

/// The ffmpeg arguments for what to do with that input: nothing. The video is copied through,
/// because the pixels were encoded on the GPU and never reached this process's memory.
///
/// The absences are the substance:
///
/// * no `-c:v h264_*` and no `-b:v`: nothing is left to encode, and the bitrate belongs to the MFT.
/// * no `-g` and no `-force_key_frames`: neither applies to a copied stream, so segment
///   boundaries come from the encoder (`MftEncoder` sets `MF_MT_MAX_KEYFRAME_SPACING`, measured
///   as three fragments a second apart, each a keyframe).
/// * no `-fps_mode`: a copied stream has no frames to convert.
/// * no `-vf scale`: scaling means decoding, which is what this path keeps out of this process.
///   A scaled output must be asked of the capture, not ffmpeg.
pub fn video_output_args_bitstream(codec: crate::VideoCodec) -> Vec<String> {
    let mut args = vec!["-c:v".to_string(), "copy".to_string()];
    // `hvc1`, not ffmpeg's default `hev1`: some players (Apple's among them) refuse `hev1`, and these
    // clips get played on a Mac. Meaningless for H.264, so not passed for it.
    if codec == crate::VideoCodec::Hevc {
        args.push("-tag:v".to_string());
        args.push("hvc1".to_string());
    }
    args
}

/// The ffmpeg arguments that describe the encoder for that video input: frame passing, the
/// scale filter when the output differs from the capture, the codec, bitrate and keyframe schedule.
///
/// Shared with the throughput probe, which writes to the null muxer so the forced keyframes and
/// the scale cost it what they cost the segmenter.
pub fn video_output_args(cfg: &EncodeConfig) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // THE load-bearing option of the whole timeline. Do not remove it.
    //
    // ffmpeg's default (`-fps_mode auto`) picks a constant-rate conversion, which resamples a stream
    // whose frames arrive at anything other than the declared `-framerate R` onto a rigid `1/R` grid
    // (duplicating missing frames, dropping surplus ones). With the segment muxer that is the media
    // clock: 4K libx264 with ~8 frames a second arriving against a declared 120 encoded 1631 frames
    // from 39 delivered (`dup=1592`), and the ring's media clock fell behind real time for ever.
    //
    // `passthrough` removes the conversion: each frame keeps its arrival timestamp
    // (`-use_wallclock_as_timestamps 1`, see [`video_input_args`]), so media time advances with the
    // wall clock by construction, and a machine that cannot keep up drops frames instead of
    // stretching the clock. `-vsync 0` is the older spelling; ffmpeg 5.1 renamed it and the pinned
    // Windows sidecar carries the new name.
    args.extend(["-fps_mode".to_string(), "passthrough".to_string()]);
    // Frames arrive at `source_size`; when the caller asked for a different output size,
    // scale to it. Equal sizes add no filter, which avoids a pointless pass.
    if cfg.source_size != cfg.output_size {
        args.extend([
            "-vf".to_string(),
            format!("scale={}:{}", cfg.output_size.0, cfg.output_size.1),
        ]);
    }
    args.extend(["-c:v".to_string(), cfg.encoder_name().to_string()]);
    // `hvc1`, not ffmpeg's default `hev1`, on this raw path too: some players refuse `hev1`, and
    // HEVC runs here where the hardware encoder refuses (`ffprobe` on such a clip reported
    // `codec_tag_string=hev1`).
    if cfg.codec == crate::VideoCodec::Hevc {
        args.extend(["-tag:v".to_string(), "hvc1".to_string()]);
    }
    // The encoder's own preset, when one was asked for and this encoder has one.
    args.extend(preset_args(cfg.encoder_name(), &cfg.preset));
    args.extend(rate_control_args(
        cfg.encoder_name(),
        cfg.rate_control,
        cfg.bitrate_kbps,
    ));
    args.extend(["-g".to_string(), gop_frames(cfg).to_string()]);
    // Forced keyframes make segment boundaries cuttable.
    args.extend([
        "-force_key_frames".to_string(),
        format!("expr:gte(t,n_forced*{})", keyframe_seconds(cfg)),
    ]);
    args
}

/// The arguments that make an ffmpeg encoder hold `bitrate_kbps` rather than aim near it.
///
/// `-b:v` alone is an average target and measured clips at ~1.9x the setting. Constant bitrate
/// needs three things together: the hardware encoders' rate-control switch (`-rc cbr`, named by
/// NVENC and AMF, inferred by QSV), a `-maxrate` equal to the target and a `-bufsize` of one
/// second of that rate. `-maxrate`/`-bufsize` are generic, so the software encoders the tests
/// use take them too.
///
/// The mode decides the switch and whether the rate is a target at all: a constant-quality mode
/// has no bitrate target, so `-b:v`/`-maxrate`/`-bufsize` are left out rather than describing a
/// number the encoder ignores.
///
/// Only the argument list is tested; whether a driver obeys it is a measurement on that hardware.
pub fn rate_control_args(encoder: &str, mode: RateControl, bitrate_kbps: u32) -> Vec<String> {
    let rate = format!("{bitrate_kbps}k");
    let mut args = Vec::new();
    if mode != RateControl::Quality {
        args.extend(["-b:v".to_string(), rate.clone()]);
    }
    args.extend(rate_control_mode_args(encoder, mode));
    if mode != RateControl::Quality {
        args.extend([
            "-maxrate".to_string(),
            rate.clone(),
            "-bufsize".to_string(),
            rate,
        ]);
    }
    args
}

/// The encoder's own rate-control switch, for the mode asked for.
///
/// **Per vendor, because the three families spell it differently.** The names are ffmpeg's:
///
/// | vendor | cbr | vbr | quality |
/// |---|---|---|---|
/// | nvenc | `-rc cbr` | `-rc vbr` | `-rc constqp` |
/// | qsv   | (its own default already is CBR) | `-rc vbr` | `-rc icq` |
/// | amf   | `-rc cbr` | `-rc vbr` | `-rc cqp` |
///
/// QSV gets **no** `-rc` for constant bitrate, which is exactly what every build before this
/// setting existed sent: ffmpeg's QSV encoder infers constant bitrate from the bitrate
/// arguments alone, and adding `-rc cbr` would change the argument list of every recording that
/// did not ask for a mode.
///
/// The software encoders (the test builds) have no `-rc` at all: x264 and x265 take `-crf`
/// instead, and their constant-quality value is ffmpeg's own default for each.
fn rate_control_mode_args(encoder: &str, mode: RateControl) -> Vec<String> {
    use crate::EncoderFamily;
    let (flag, value) = match (EncoderFamily::of(encoder), mode) {
        (EncoderFamily::Nvenc, RateControl::Cbr) => ("-rc", "cbr"),
        (EncoderFamily::Nvenc, RateControl::Vbr) => ("-rc", "vbr"),
        (EncoderFamily::Nvenc, RateControl::Quality) => ("-rc", "constqp"),
        (EncoderFamily::Qsv, RateControl::Cbr) => return Vec::new(),
        (EncoderFamily::Qsv, RateControl::Vbr) => ("-rc", "vbr"),
        (EncoderFamily::Qsv, RateControl::Quality) => ("-rc", "icq"),
        (EncoderFamily::Amf, RateControl::Cbr) => ("-rc", "cbr"),
        (EncoderFamily::Amf, RateControl::Vbr) => ("-rc", "vbr"),
        (EncoderFamily::Amf, RateControl::Quality) => ("-rc", "cqp"),
        // x264's default CRF is 23 and x265's is 28; passing them says the same thing ffmpeg
        // would have done, in a form a reader can see.
        (EncoderFamily::Software, RateControl::Quality) => {
            let crf = if encoder.contains("265") { "28" } else { "23" };
            return vec!["-crf".to_string(), crf.to_string()];
        }
        (EncoderFamily::Software, _) => return Vec::new(),
    };
    vec![flag.to_string(), value.to_string()]
}

/// The arguments that ask for the encoder's own preset (`-preset`).
///
/// Empty when no preset was asked for, and empty when the preset is not one this encoder family
/// accepts: ffmpeg refuses an unknown `-preset` value and the recording would not start at all,
/// so a preset that does not apply is dropped with a warning instead. The families' lists are
/// [`crate::EncoderFamily::presets`], which is also what the settings panel offers.
pub fn preset_args(encoder: &str, preset: &str) -> Vec<String> {
    if preset.is_empty() {
        return Vec::new();
    }
    let family = crate::EncoderFamily::of(encoder);
    if !family.presets().contains(&preset) {
        tracing::warn!(
            "the preset {preset:?} is not one the {family:?} encoder accepts, so it is using \
             its own default: the presets it takes are {:?}",
            family.presets()
        );
        return Vec::new();
    }
    vec!["-preset".to_string(), preset.to_string()]
}

/// The ffmpeg arguments that declare one raw-PCM loopback audio input: format, sample rate,
/// channel count and the URL ffmpeg dials.
///
/// Both audio inputs are declared here (the game audio from the constants above, the microphone
/// from [`crate::MicAudioSpec`]), so they cannot be declared differently. A rate or channel count
/// that disagrees with the PCM actually submitted shifts the track against the others instead of failing.
fn audio_input_args(
    url: &str,
    sample_rate: u32,
    channels: u16,
    sample_format: SampleFormat,
) -> Vec<String> {
    vec![
        "-f".to_string(),
        sample_format.ffmpeg_name().to_string(),
        "-ar".to_string(),
        sample_rate.to_string(),
        "-ac".to_string(),
        channels.to_string(),
        "-i".to_string(),
        url.to_string(),
    ]
}

/// The complete ffmpeg argument list for one recording child, program name excepted.
///
/// A pure function of the configuration and the two loopback URLs, so a test can assert the whole
/// list element for element.
///
/// **The input order is the fix for issue #78, and it is load-bearing.** ffmpeg opens its inputs
/// in the order they are declared, and the raw video input's probe *blocks*: a raw H.264 demuxer
/// cannot probe the stream until it has seen a keyframe carrying the sequence header. With the
/// video declared first, the child sat inside input 0's probe and **input 1, the audio socket, was
/// never opened**: the recorder's audio writer waited out its ten-second connect deadline for a
/// connection that could not happen, and the recording died before it began.
///
/// So the audio inputs are declared first, and the video last. The child then dials the audio
/// socket before it looks at the video at all, so the connection cannot be behind the probe, and
/// the video's probe may take as long as it needs — the sequence header the hardware encoder
/// publishes is prepended to the bitstream (`crate::mft`), so it does not take long.
///
/// The output is mapped explicitly, because the default stream selection cannot be relied on
/// once the inputs are in this order, and because the mapping is what fixes the *positions* the
/// audio tracks are named by (`replayed_media::edit::audio_titles`): the game audio is the first
/// audio stream and the microphone the second.
///
/// `OsString` because the segment pattern is a real path: a non-UTF-8 `scratch_dir` must reach
/// ffmpeg as the bytes it is.
fn ffmpeg_args(cfg: &EncodeConfig, audio_url: &str, mic_url: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(OsString::from)
        .collect();
    // `-nostdin` keeps ffmpeg from consuming our stdin for interactive commands, which would steal
    // raw video frames; `-loglevel error` is why a failure's reason is read from stderr by `drain_stderr`.

    // The input index each one gets, in the order they are declared. The video is last, so the
    // audio indices are 0 and 1 and the video's is whatever the audio left.
    let mut next_input = 0u32;
    let mut game_audio_input = None;
    let mut mic_input = None;

    // Input 0, the system/game audio: raw s16le PCM over loopback TCP (see the module comment).
    // Declared only when `game_audio` is on, since a declared input would have to be mapped,
    // tagged and ended.
    if cfg.game_audio {
        args.extend(
            audio_input_args(
                audio_url,
                GAME_AUDIO_SAMPLE_RATE,
                GAME_AUDIO_CHANNELS,
                SampleFormat::S16Le,
            )
            .into_iter()
            .map(OsString::from),
        );
        game_audio_input = Some(next_input);
        next_input = next_input.saturating_add(1);
    }

    // Input 1, the microphone, declared and pumped by the same functions as input 0 so the second
    // track cannot invent its own timing conventions; only the numbers come from `MicAudioSpec`.
    if let Some(mic_url) = mic_url {
        let spec = cfg
            .mic_audio
            .expect("a microphone URL is only ever passed when EncodeConfig::mic_audio is Some");
        args.extend(
            audio_input_args(mic_url, spec.sample_rate, spec.channels, spec.sample_format)
                .into_iter()
                .map(OsString::from),
        );
        mic_input = Some(next_input);
        next_input = next_input.saturating_add(1);
    }

    // The video input, last: raw BGRA frames copied out of VRAM ([`video_input_args`], shared
    // with the throughput probe), or an H.264 elementary stream encoded on the GPU
    // ([`video_input_args_bitstream`]). See [`VideoInput`].
    args.extend(
        match cfg.video {
            VideoInput::RawPixels => video_input_args(cfg),
            VideoInput::EncodedBitstream => video_input_args_bitstream(cfg.codec),
        }
        .into_iter()
        .map(OsString::from),
    );
    let video_input = next_input;

    // What to do with the video input: scale, codec, bitrate and keyframe schedule, or a plain
    // copy for a stream that arrived encoded. Shared with the probe.
    args.extend(
        match cfg.video {
            VideoInput::RawPixels => video_output_args(cfg),
            VideoInput::EncodedBitstream => video_output_args_bitstream(cfg.codec),
        }
        .into_iter()
        .map(OsString::from),
    );
    // The audio codec and rate only exist when there is an audio input to encode; a `-c:a` with no
    // audio stream is silently unused but misleading, so it is not passed.
    if cfg.game_audio {
        args.extend(["-c:a", "aac", "-b:a"].iter().map(OsString::from));
        args.push(format!("{}k", cfg.audio_bitrate_kbps).into());
    }

    // The mapping, written out rather than left to ffmpeg's default selection.
    //
    // The order here is the order the output has always had — the video first, then the game
    // audio, then the microphone — so the file is the same file it was, with the same tracks in
    // the same places. What the mapping fixes is *which* track is which: the video is the input
    // the video came from and the audio inputs are the ones the audio came from, and the audio
    // tracks keep the positions they are named by (the game audio is the first audio stream, the
    // microphone the second — `replayed_media::edit::audio_titles`).
    args.push("-map".into());
    args.push(format!("{video_input}:v").into());
    if let Some(index) = game_audio_input {
        args.push("-map".into());
        args.push(format!("{index}:a").into());
    }
    if let Some(index) = mic_input {
        args.push("-map".into());
        args.push(format!("{index}:a").into());
    }

    // The tag strings come from `replayed-media`, which applies the same names again when it
    // concatenates segments (`-c copy` does not carry per-stream metadata); two literals could drift
    // and leave a track anonymous.
    //
    // Only the two-track path is tagged, as it always was: the single-track path has no tags, and
    // the names are applied again when the segments are concatenated.
    if mic_input.is_some() {
        for (index, title) in replayed_media::edit::audio_titles(2).iter().enumerate() {
            args.push(format!("-metadata:s:a:{index}").into());
            args.push(format!("title={title}").into());
        }
    }

    match cfg.output {
        EncodeOutput::Segmented => {
            args.extend(["-f", "segment"].iter().map(OsString::from));
            args.extend(["-segment_time"].iter().map(OsString::from));
            args.push(keyframe_seconds(cfg).to_string().into());
            args.extend(["-segment_format", "mp4"].iter().map(OsString::from));
            // Each segment starts at zero, which the concat at clip time relies on.
            args.extend(["-reset_timestamps", "1"].iter().map(OsString::from));
            // Continue the numbering instead of restarting it. The segment muxer's option is
            // `segment_start_number`; plain `-start_number` is silently ignored (with `-start_number 5` the
            // first file was still `seg-000000.mp4`), which would overwrite files the adopted ledger names.
            args.extend(["-segment_start_number"].iter().map(OsString::from));
            args.push(cfg.start_number.to_string().into());
            args.push(cfg.scratch_dir.join(SEGMENT_PATTERN).into());
        }
        EncodeOutput::FragmentedStream => {
            // A stream, not files: fragmented MP4 on the child's stdout, so nothing is written to the SSD
            // while the user is only waiting for something worth clipping.
            //
            // `frag_keyframe` starts a new fragment on every keyframe, and `video_output_args` already
            // forces one per `segment_ms`, so a fragment boundary is a segment boundary with no second
            // keyframe argument to keep in step. `empty_moov` + `default_base_moof` make the stream
            // self-describing from the first byte, with no seek-back to patch the header (impossible on a
            // pipe): a 1249-byte `ftyp`+`moov`, then one `moof`+`mdat` per second with a monotonic `tfdt`.
            args.extend(["-f", "mp4"].iter().map(OsString::from));
            args.extend(
                ["-movflags", "empty_moov+frag_keyframe+default_base_moof"]
                    .iter()
                    .map(OsString::from),
            );
            args.push("pipe:1".into());
        }
    }

    args
}

/// The segment muxer's file-name pattern inside [`EncodeConfig::scratch_dir`].
const SEGMENT_PATTERN: &str = "seg-%06d.mp4";

pub struct FfmpegEncoder {
    child: Child,
    /// The child's stdout, when spawned with [`EncodeOutput::FragmentedStream`]. Taken by the first
    /// caller (`Encoder::take_output_stream`): a pipe has one reader, which should be the in-memory buffer.
    output_stream: Option<std::process::ChildStdout>,
    video_tx: Option<SyncSender<VideoChunk>>,
    audio_tx: Option<SyncSender<Vec<u8>>>,
    /// The microphone's queue into its own pump thread; `None` without a second audio input.
    mic_tx: Option<SyncSender<Vec<u8>>>,
    video_writer: Option<WriterHandle>,
    audio_writer: Option<WriterHandle>,
    mic_writer: Option<WriterHandle>,
    encoder_name: &'static str,
    /// The child's argument list, kept so a failure report can say what it was asked to do.
    ///
    /// A child that exits without a word on its stderr is otherwise unanswerable: the report can
    /// say that it failed, but not what it was told. The arguments are the only record of that.
    argv: Vec<OsString>,
    /// The geometry the rawvideo pipe was declared with (`-s {w}x{h}`), so a frame of any other size
    /// can be refused instead of sliced at the wrong stride (`Encoder::source_size`).
    source_size: (u32, u32),
    /// The rate the rawvideo pipe was declared with (`-framerate {fps}`), so the caller paces
    /// capture to exactly the number the child was told (`Encoder::input_fps`).
    input_fps: u32,
    /// Whether this encoder takes GPU textures rather than pixels, carried from its config; the
    /// caller decides what to ask capture for from it.
    accepts_textures: bool,
    /// The hardware encoder MFT, when this is a [`VideoInput::EncodedBitstream`] encoder.
    ///
    /// Opened lazily from the first frame, because it takes its device from the frame's texture
    /// (`MftEncoder::open_for_texture`), which guarantees it is the capture path's device.
    #[cfg(windows)]
    /// The hardware encoder, on its own thread (see [`crate::mft::MftFeed`]).
    mft: Option<crate::mft::MftFeed>,
    /// The loopback port the microphone input was bound to (`Encoder::mic_port`).
    mic_port: Option<u16>,
    /// What the child was told the microphone input is (`-f/-ar/-ac`), so
    /// [`Encoder::submit_mic_audio`] can refuse a block whose format disagrees with it.
    mic_spec: Option<crate::MicAudioSpec>,
    /// Whether this encoder has the game-audio input at all. `false` is video-only: no listener, no
    /// `-c:a`, no writer thread, and [`Encoder::submit_audio`] refuses a block.
    game_audio: bool,
    /// Frames dropped because a queue was full. Atomic: written on the submitting thread, read through `&self`.
    dropped_video: AtomicU64,
    dropped_audio: AtomicU64,
    /// Microphone blocks dropped, like [`FfmpegEncoder::dropped_audio`].
    dropped_mic_audio: AtomicU64,
    /// Bytes of game audio dropped and not yet replaced by silence (see [`enqueue_audio`]).
    owed_audio: usize,
    /// Bytes of microphone audio dropped and not yet replaced by silence.
    owed_mic_audio: usize,
    /// Text already read out of ffmpeg's stderr, which a pipe gives up only once (see
    /// [`FfmpegEncoder::drain_stderr`]); both the writer-failure report and [`Encoder::finish`] may want it.
    drained_stderr: Option<String>,
    /// The reader thread still blocked on ffmpeg's stderr, if one is in flight.
    ///
    /// Kept across calls so a budget that expired before the words arrived does not discard the
    /// reader; a later report can collect what the child eventually writes.
    stderr_drain: Option<mpsc::Receiver<String>>,
}

impl FfmpegEncoder {
    pub fn spawn(bin: &FfmpegBinaries, cfg: &EncodeConfig) -> Result<Self> {
        // Refused before anything is created, as a configuration contradiction. An encoded bitstream
        // cannot be scaled here (scaling means decoding, the cost this variant removes), and silently
        // emitting the capture's size would give the wrong geometry with no explanation.
        if cfg.video == VideoInput::EncodedBitstream && cfg.source_size != cfg.output_size {
            bail!(
                "an encoded bitstream cannot be scaled by ffmpeg, and this encoder was asked for \
                 {}x{} from {}x{} capture: scaling means decoding, and decoding means the pixels \
                 this mode exists to keep out of this process. Capture at the output size instead, \
                 or ask for an output of the capture's size.",
                cfg.output_size.0,
                cfg.output_size.1,
                cfg.source_size.0,
                cfg.source_size.1
            );
        }
        std::fs::create_dir_all(&cfg.scratch_dir)
            .with_context(|| format!("creating {}", cfg.scratch_dir.display()))?;

        // Audio comes in over loopback TCP. Bind before spawning the child: the port is in the argument
        // list, and `:0` lets the OS pick a free one, read back from the socket (no racing for a fixed port).
        //
        // `[audio] enabled = false` is handled first: a video-only recording binds no listener and
        // names no port, so there is no input to feed and no track to end.
        let game_audio_input: Option<(TcpListener, u16)> = if cfg.game_audio {
            let listener = TcpListener::bind("127.0.0.1:0")
                .context("binding the audio input listener on 127.0.0.1")?;
            let port = listener
                .local_addr()
                .context("reading the audio input listener's port")?
                .port();
            Some((listener, port))
        } else {
            None
        };
        // `ffmpeg_args` reads this only when `cfg.game_audio` is on: the URL exists exactly when the input does.
        let audio_url = game_audio_input
            .as_ref()
            .map(|(_, port)| format!("tcp://127.0.0.1:{port}"))
            .unwrap_or_default();

        // A microphone with no game audio is refused like the scaling case above. Audio tracks are named
        // by position (`replayed_media::edit::audio_titles`: `Game Audio`, then `Microphone`), so this
        // would produce a one-track file labelled `Game Audio` that carries a voice. Refused rather than
        // relabelled.
        if !cfg.game_audio && cfg.mic_audio.is_some() {
            bail!(
                "a microphone without the game audio cannot be labelled: this build names a \
                 container's first audio track `Game Audio`, so the recording would carry the \
                 microphone under the name of a track it does not have. Turn the microphone off \
                 for a video-only recording, or leave `[audio] enabled = true`."
            );
        }

        // The microphone is a second listener of the same kind, bound before the child exists because
        // its port is in the argument list. A failed bind fails the start rather than yielding a
        // recording that quietly carries one track.
        let mic: Option<(TcpListener, u16)> = match cfg.mic_audio {
            Some(_) => {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .context("binding the microphone input listener on 127.0.0.1")?;
                let port = listener
                    .local_addr()
                    .context("reading the microphone input listener's port")?
                    .port();
                Some((listener, port))
            }
            None => None,
        };
        let mic_url = mic
            .as_ref()
            .map(|(_, port)| format!("tcp://127.0.0.1:{port}"));

        let argv = ffmpeg_args(cfg, &audio_url, mic_url.as_deref());
        let mut cmd = replayed_media::sidecar_command(&bin.ffmpeg);
        cmd
            // The whole argument list is built in [`ffmpeg_args`] so what a test asserts is what the child
            // is spawned with.
            .args(argv.clone())
            .stdin(Stdio::piped())
            // Stdout carries the encoded stream in `FragmentedStream` mode. In `Segmented` mode it goes to
            // the null device so ffmpeg can never write into our stdout.
            .stdout(match cfg.output {
                EncodeOutput::Segmented => Stdio::null(),
                EncodeOutput::FragmentedStream => Stdio::piped(),
            })
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning {} (requested encoder: {})",
                bin.ffmpeg.display(),
                cfg.encoder_name()
            )
        })?;

        // Video goes to the child's stdin; the audio listeners move into their threads, which accept
        // ffmpeg's connections. In `FragmentedStream` mode stdout is taken here for the caller that owns
        // the in-memory buffer; in `Segmented` mode `take()` yields `None`.
        let output_stream = child.stdout.take();
        let video_in = child.stdin.take().context("child stdin unavailable")?;

        // ffmpeg's stderr is read from the moment the child starts, so a failure report never has
        // to wait for a thread to be spawned and scheduled before it can give the reason. The
        // reader blocks until the pipe closes, which happens when the child exits.
        let stderr_drain = child.stderr.take().map(stderr_reader);

        let (video_tx, video_rx) = mpsc::sync_channel::<VideoChunk>(VIDEO_QUEUE_FRAMES);
        // The hardware encoder's thread writes into this same video queue, so a frame reaches ffmpeg the same way either way.
        #[cfg(windows)]
        let mft = (cfg.video == VideoInput::EncodedBitstream).then(|| {
            crate::mft::MftFeed::spawn(
                crate::mft::MftEncoderParams {
                    codec: cfg.codec,
                    bitrate_kbps: cfg.bitrate_kbps,
                    fps: cfg.fps,
                    mode: cfg.rate_control,
                },
                video_tx.clone(),
            )
        });
        let video_writer = std::thread::Builder::new()
            .name("ffmpeg-video-in".into())
            .spawn(move || pump_video(video_rx, video_in))
            .context("spawning video writer thread")?;

        // The game-audio queue and writer exist only when the input does. `pump_audio` accepts the
        // child's connection under a deadline, so this thread's result is also how a declared input
        // ffmpeg never opened is reported (the `audio_res` branch in `finish`).
        let (audio_tx, audio_writer) = match game_audio_input {
            Some((listener, port)) => {
                let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_BLOCKS);
                let writer = std::thread::Builder::new()
                    .name("ffmpeg-audio-in".into())
                    .spawn(move || pump_audio(listener, rx, port, GAME_AUDIO_LABEL))
                    .context("spawning audio writer thread")?;
                (Some(tx), Some(writer))
            }
            None => (None, None),
        };

        // The microphone pump is a thread of its own: a single thread writing both audio inputs
        // deadlocks as soon as either socket's buffer fills. It runs the same `pump_audio` (accept with
        // deadline, blocking-mode fix, drop policy), so the tracks share timing conventions.
        let (mic_tx, mic_writer, mic_port) = match mic {
            Some((listener, port)) => {
                let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_BLOCKS);
                let writer = std::thread::Builder::new()
                    .name("ffmpeg-mic-in".into())
                    .spawn(move || pump_audio(listener, rx, port, MICROPHONE_LABEL))
                    .context("spawning microphone writer thread")?;
                (Some(tx), Some(writer), Some(port))
            }
            None => (None, None, None),
        };

        Ok(Self {
            child,
            output_stream,
            video_tx: Some(video_tx),
            audio_tx,
            mic_tx,
            video_writer: Some(video_writer),
            audio_writer,
            mic_writer,
            encoder_name: cfg.encoder_name(),
            source_size: cfg.source_size,
            input_fps: cfg.fps,
            // Carried from the config: capture is asked before a frame exists.
            accepts_textures: cfg.accepts_textures(),
            // Opened on the first frame, on its own thread (see the field).
            #[cfg(windows)]
            mft,
            mic_port,
            mic_spec: cfg.mic_audio,
            game_audio: cfg.game_audio,
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            dropped_mic_audio: AtomicU64::new(0),
            owed_audio: 0,
            owed_mic_audio: 0,
            drained_stderr: None,
            // The reader has been running since the child was spawned.
            stderr_drain,
            argv,
        })
    }
}

/// Copy queued chunks into a sink until the sender half is dropped, then close it (EOF to ffmpeg).
fn pump<W: Write>(rx: mpsc::Receiver<Vec<u8>>, mut sink: W) -> std::io::Result<()> {
    for chunk in rx {
        sink.write_all(&chunk)?;
    }
    sink.flush()
}

/// Copy queued video into the child's stdin, one frame at a time and at the frames' capture times.
///
/// The pacing is the load-bearing part. The child stamps a frame with the wall clock at the moment
/// it read the frame, so a child that reads a burst of frames gets a burst of timestamps and a
/// picture that judders. Writing the frames at their capture times spreads the reads out: the
/// child reads one frame per capture interval, and the wall clock it stamps them with is the
/// capture clock.
///
/// The audio is not paced: its timeline is the exact 48kHz sample count, and a socket's arrival
/// times would only add jitter to it.
fn pump_video<W: Write>(rx: mpsc::Receiver<VideoChunk>, mut sink: W) -> std::io::Result<()> {
    let mut schedule = FrameSchedule::default();
    for chunk in rx {
        if let Some(captured_at) = chunk.captured_at {
            let wait = schedule.wait_for(captured_at, Instant::now());
            if !wait.is_zero() {
                crate::timeline::wait_until(Instant::now() + wait);
            }
        }
        sink.write_all(&chunk.bytes)?;
    }
    sink.flush()
}

/// Accept ffmpeg's connection to the audio input, then stream the queued PCM into it.
///
/// Dropping the socket at the end is ffmpeg's EOF on the input. `what` names the input in
/// failures (`"audio"` or `"microphone"`); both run this same function, which is what makes the
/// microphone share the game audio's clock discipline.
fn pump_audio(
    listener: TcpListener,
    rx: mpsc::Receiver<Vec<u8>>,
    port: u16,
    what: &str,
) -> std::io::Result<()> {
    let stream = accept_within(&listener, port, AUDIO_CONNECT_TIMEOUT, what)?;
    // PCM arrives in small blocks (10ms = 1920 bytes at 48kHz stereo s16le) read in real time, so Nagle only adds latency.
    stream.set_nodelay(true).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("disabling Nagle on the {what} input socket: {e}"),
        )
    })?;
    pump(rx, stream)
}

/// Accept one connection, giving up after `timeout`.
///
/// The listener is non-blocking and polled rather than parked in `accept()`: if ffmpeg fails to
/// start nothing ever dials the port, and an indefinite `accept()` would hang `finish()`.
/// `WouldBlock` is the only "not yet" error; anything else is returned as-is. `what` only
/// shapes the failure messages.
fn accept_within(
    listener: &TcpListener,
    port: u16,
    timeout: Duration,
    what: &str,
) -> std::io::Result<TcpStream> {
    listener.set_nonblocking(true).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("making the {what} input listener non-blocking: {e}"),
        )
    })?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Belt and braces for the zero-egress note above: a non-loopback peer would mean PCM is going somewhere unexpected.
                if !peer.ip().is_loopback() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{what} input connection from non-loopback peer {peer}"),
                    ));
                }
                // The listener is non-blocking only so this poll loop can enforce a connect deadline; that must
                // not leak into the stream handed to the pump. BSD-derived platforms (macOS included) propagate
                // O_NONBLOCK from the listener to the accepted socket, Linux does not, so without this line the
                // socket is non-blocking only on macOS. A non-blocking socket makes `write_all` fail with
                // `WouldBlock` (`EAGAIN`, os error 35) the instant ffmpeg's receive buffer fills: an
                // intermittent failure that bit the game-audio input twice. The microphone listener is a second
                // copy of this accept path and needs the line for the same reason.
                // Blocking is safe: the pump has its own thread fed by a bounded channel, so backpressure never
                // reaches `submit_audio`/`submit_mic_audio`, which drop the block instead (`enqueue_or_drop`).
                stream.set_nonblocking(false).map_err(|e| {
                    std::io::Error::new(
                        e.kind(),
                        format!("making the accepted {what} input socket blocking: {e}"),
                    )
                })?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "ffmpeg never connected to the {what} input on 127.0.0.1:{port} \
                             within {timeout:?}"
                        ),
                    ));
                }
                std::thread::sleep(AUDIO_CONNECT_POLL);
            }
            Err(e) => return Err(e),
        }
    }
}

impl Encoder for FfmpegEncoder {
    /// Queue the frame for the rawvideo pipe, or drop it if the queue is full.
    ///
    /// The frame's pixel buffer is moved into the queue, not cloned (33.2MB per 3840x2160 BGRA frame).
    ///
    /// `try_send` and a drop rather than an error: this sits in the loop that must keep pulling
    /// frames from a live capture, so blocking would eat the latency budget and failing would stop
    /// recording because the GPU fell a few milliseconds behind. The timeline is the wall clock, so
    /// a missing frame is a brief repeat, not a shift of everything after it. The drop is counted
    /// (`Encoder::dropped_frames`) so a soak sees it.
    ///
    /// A disconnected channel means the writer thread is gone and nothing can be encoded, so that is
    /// an error, carrying ffmpeg's exit status and stderr when known (the writer error is only the
    /// symptom; see [`FfmpegEncoder::explain_dead_writer`]).
    fn submit_video(&mut self, frame: Frame) -> Result<()> {
        // The frame's geometry is checked by the caller (`pump_once_counted`), the only place holding
        // both the frame and the pipe's declared size.
        //
        // A GPU-texture frame is encoded here by the hardware MFT and the H.264 it produced reaches the
        // queue (kilobytes instead of tens of megabytes); the queue, writer thread and child arguments
        // are unchanged.
        //
        // The capture time travels with the frame on both paths: it is what the child's timeline is
        // built from, and the two paths differ only in what the bytes are.
        #[cfg(windows)]
        if let Some(texture) = frame.texture {
            self.encode_texture(texture, frame.pts)?;
            return Ok(());
        }

        // The caller cannot check whether the frame has pixels at all. A frame whose pixels stayed in
        // VRAM has an empty `data` by design (`replayed_capture::Frame`); writing it would feed ffmpeg
        // a zero-length frame that it reads as the next frame's leading bytes, a stream desync that
        // surfaces as corruption seconds later. Refuse it explicitly.
        if !frame.has_pixels() {
            bail!(
                "this frame's pixels are not in CPU memory, and this encoder feeds ffmpeg a raw                  byte stream: a zero-length frame would be read as the next frame's bytes. A                  frame must carry pixels to reach this encoder."
            );
        }
        let chunk = VideoChunk {
            bytes: frame.data,
            captured_at: Some(frame.pts),
        };
        let queued = match self.video_tx.as_ref() {
            Some(tx) => enqueue_or_drop(tx, chunk, &self.dropped_video, "video"),
            None => bail!("encoder already finished"),
        };
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    /// Queue an audio block, or drop it if the queue is full; same reasoning as
    /// [`Encoder::submit_video`], counted in `Encoder::dropped_audio_blocks`.
    fn submit_audio(&mut self, audio: AudioBuffer) -> Result<()> {
        // Refused, not dropped, when there is no audio input: feeding a track the recording lacks is a
        // wiring mistake, not a full queue (same as `submit_mic_audio`).
        refuse_audio_without_input(self.game_audio)?;
        let queued = match self.audio_tx.as_ref() {
            Some(tx) => enqueue_audio(
                tx,
                audio.data,
                &self.dropped_audio,
                &mut self.owed_audio,
                "audio",
            ),
            None => bail!("encoder already finished"),
        };
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    /// Queue a microphone block on the second input, or drop it if that queue is full; counted in
    /// `Encoder::dropped_mic_audio_blocks`.
    ///
    /// The microphone's format is what the caller declared in `EncodeConfig::mic_audio` and what the
    /// child was told (`-f/-ar/-ac`). A block whose own format disagrees is refused: its bytes
    /// would be read as a different length and the track would sit on a wrong timeline for the
    /// whole clip, undetectably (like a wrong-geometry frame, `Encoder::source_size`).
    fn submit_mic_audio(&mut self, audio: AudioBuffer) -> Result<()> {
        let Some(tx) = self.mic_tx.as_ref() else {
            bail!(
                "this encoder was spawned without a microphone input \
                 (EncodeConfig::mic_audio was None); the block was refused rather than \
                 silently dropped"
            );
        };
        let spec = self
            .mic_spec
            .expect("a microphone queue exists only when a microphone spec was configured");
        if audio.format.sample_rate != spec.sample_rate || audio.format.channels != spec.channels {
            bail!(
                "microphone block is {}Hz {}ch but the input was declared as {}Hz {}ch \
                 (EncodeConfig::mic_audio); accepting it would put the microphone track on \
                 a timeline of the wrong length",
                audio.format.sample_rate,
                audio.format.channels,
                spec.sample_rate,
                spec.channels
            );
        }
        let queued = enqueue_audio(
            tx,
            audio.data,
            &self.dropped_mic_audio,
            &mut self.owed_mic_audio,
            MICROPHONE_LABEL,
        );
        queued.map_err(|e| self.explain_dead_writer(e))
    }

    fn finish(&mut self) -> Result<()> {
        // The hardware encoder's tail goes into the queue before the senders are dropped: the drain
        // makes the last frames real, and the pipe is closing after this line. Best-effort with a
        // warning, since a recording missing a few frames beats none because a drain timed out.
        #[cfg(windows)]
        // The encoder's thread is closed and joined before the video sender is dropped: it holds a
        // clone of that sender, and dropping ours first would tell ffmpeg the video had ended while the
        // last frames were still on their way.
        #[cfg(windows)]
        if let Some(mut mft) = self.mft.take() {
            mft.shutdown();
            let dropped = mft.dropped();
            if dropped > 0 {
                tracing::warn!(
                    "{dropped} frames never reached the hardware encoder: its queue is two deep \
                     because capture's handover ring is three, and the encoder is slower than the \
                     frame rate it is being fed"
                );
            }
        }

        // Dropping the senders ends each pump loop, closing the video pipe and audio sockets, which tells ffmpeg the inputs ended.
        self.video_tx.take();
        self.audio_tx.take();
        self.mic_tx.take();

        // The child is waited for first, and with a deadline. This order bounds the shutdown; getting
        // it wrong caused an eight-minute hang.
        //
        // A writer thread ends when its pipe does, and the pipe only breaks when ffmpeg stops reading
        // or exits, so joining the writers before waiting on the child lets a child that never exits
        // take the whole shutdown with it. Close the inputs, bound the wait, then join: a child that
        // will not go is killed, its death breaks the pipes, and the writers follow.
        //
        // A killed ffmpeg is reported below, since its status and stderr explain a failed start.
        let end = wait_bounded(&mut self.child, SHUTDOWN_BUDGET);

        // Join every writer before reporting: an audio thread may still be waiting out its connect
        // deadline, and abandoning it would leave a listener a later connection could reach. Its own
        // deadline bounds it, and the child's death above bounds the video writer.
        let video_res = join_writer(self.video_writer.take(), "video");
        let audio_res = join_writer(self.audio_writer.take(), GAME_AUDIO_LABEL);
        let mic_res = join_writer(self.mic_writer.take(), MICROPHONE_LABEL);

        // The connect deadline is the one failure that leaves ffmpeg alive but silent: it never opened
        // the input, so it never read video either, and waiting on it would block forever. Kill it and
        // report the deadline with whatever it logged. Either audio input hitting it is handled the
        // same way; `pump_audio` puts the input's label in the message.
        let deadline = [audio_res.as_ref().err(), mic_res.as_ref().err()]
            .into_iter()
            .flatten()
            .find(|err| is_connect_deadline(err));
        if let Some(err) = deadline {
            // Already waited for (and killed if it would not go): this reports rather than waits.
            let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
            let how = if end.killed {
                "was still running and had to be killed (on Windows a killed process reports exit \
                 code 1, so the number alone does not say what happened)"
            } else {
                "had already exited on its own"
            };
            bail!(
                "{err:#}; ffmpeg status after the deadline: {status}; the child {how}; \
                 stderr: {stderr}\nit was asked to: {argv}",
                status = end
                    .status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "none (it could not be asked)".to_string()),
                how = how,
                stderr = stderr.trim(),
                argv = self.argv_text()
            );
        }

        // ffmpeg's exit status and stderr explain a failed start (a bad encoder name, say); the writer
        // threads only see a broken pipe. Report the cause first, then the writer errors.
        match end.status {
            Some(status) if status.success() => {}
            Some(status) => {
                let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
                bail!(
                    "ffmpeg exited with {status} using encoder '{}': {}\nit was asked to: {}",
                    self.encoder_name,
                    stderr.trim(),
                    self.argv_text()
                );
            }
            None => {
                let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
                bail!(
                    "ffmpeg did not exit within {SHUTDOWN_BUDGET:?} and had to be killed, using \
                     encoder '{}': {}\nit was asked to: {}",
                    self.encoder_name,
                    stderr.trim(),
                    self.argv_text()
                );
            }
        }
        video_res?;
        audio_res?;
        mic_res?;
        Ok(())
    }

    fn accepts_textures(&self) -> bool {
        self.accepts_textures
    }

    fn active_encoder(&self) -> &'static str {
        self.encoder_name
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    fn input_fps(&self) -> u32 {
        self.input_fps
    }

    fn dropped_frames(&self) -> u64 {
        // Both counts where there are two: to the caller they are one thing, frames captured and not
        // encoded. Off Windows there is no hardware-encoder queue.
        #[cfg(windows)]
        let from_the_encoder = self.mft.as_ref().map(|mft| mft.dropped()).unwrap_or(0);
        #[cfg(not(windows))]
        let from_the_encoder = 0;
        self.dropped_video.load(Ordering::Relaxed) + from_the_encoder
    }

    fn dropped_audio_blocks(&self) -> u64 {
        self.dropped_audio.load(Ordering::Relaxed)
    }

    fn dropped_mic_audio_blocks(&self) -> u64 {
        self.dropped_mic_audio.load(Ordering::Relaxed)
    }

    fn mic_port(&self) -> Option<u16> {
        self.mic_port
    }

    fn take_output_stream(&mut self) -> Option<std::process::ChildStdout> {
        self.output_stream.take()
    }
}

/// The encoder's backstop: a dropped encoder kills its child and reaps it.
///
/// [`Encoder::finish`] is the graceful path — it closes the inputs, waits for ffmpeg to exit and
/// reports why it could not. This is the path a *failed start* takes: `Prepared::begin` spawns
/// the child and then has fallible steps left (`store.start_session`, `capture.start()`), and when
/// one of them fails the encoder is dropped without `finish()` ever being called.
///
/// `std::process::Child`'s own `Drop` neither kills nor reaps the process. The child does end,
/// because the writer thread's pipe closes when the encoder's queues go, but nothing ever waits
/// for it: it lingers as a zombie on Unix and as an unowned process on Windows. The three writer
/// threads are detached too: they end on their own, but nothing joins them.
///
/// So the child is killed and reaped here, and the writers are joined. The order is the same one
/// [`Encoder::finish`] uses and for the same reason: the child first, because the video writer
/// is blocked writing into its stdin and only the child's death breaks that pipe; the writers
/// last, because a writer that has not ended yet has nothing to be joined from.
///
/// The hardware encoder's thread comes before all of it, for a sharper reason: it holds a clone
/// of the video queue's sender, so the queue only closes once that thread has ended. Joining the
/// video writer before shutting the hardware encoder down would wait for a thread that cannot
/// end until the join returns.
///
/// The join is bounded by the audio pump's own connect deadline ([`AUDIO_CONNECT_TIMEOUT`]): a
/// child that died before dialling leaves that thread waiting for a connection that will never
/// come, and waiting it out is the price of not leaving it detached. `finish()` pays the same
/// wait on the same failure.
///
/// None of this can fail the caller: a `Drop` has nowhere to report an error, and what it fixes
/// is a leak rather than a loss.
impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // The hardware encoder first: its thread holds a clone of the video queue's sender, so
        // the queue closes only once it has ended. `finish` has already taken it, so a finished
        // encoder shuts nothing down twice.
        #[cfg(windows)]
        if let Some(mut mft) = self.mft.take() {
            mft.shutdown();
        }

        // The senders next: a pump loop ends when its channel closes, and the writer thread then
        // lets go of the child's stdin.
        self.video_tx.take();
        self.audio_tx.take();
        self.mic_tx.take();

        // The child, killed rather than waited for. `try_wait` reaps a child that has already
        // exited, so an encoder that was finished properly is not killed a second time; `wait`
        // then returns the status it already has, without blocking.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();

        // The writers last. `finish` has already taken them, so a finished encoder joins
        // nothing twice; an unfinished one joins the threads it would otherwise detach.
        let _ = join_writer(self.video_writer.take(), "video");
        let _ = join_writer(self.audio_writer.take(), GAME_AUDIO_LABEL);
        let _ = join_writer(self.mic_writer.take(), MICROPHONE_LABEL);
    }
}

impl FfmpegEncoder {
    /// Hand a captured texture to the hardware encoder's thread. Returns nothing, always.
    ///
    /// Encoding here made the pump wait on a synchronous MFT, and the pump paces capture: a 56.9fps
    /// recording came out at 35.0 with 2288 of 5700 frames dropped. The texture is now handed over
    /// and the bytes travel from the encoder's thread straight into the video queue.
    ///
    /// `captured_at` is the frame's capture time, and it is handed over with the texture: the
    /// encoder's finished frames are matched back to the frames that went in (the encoder returns
    /// them in order), and the child's timeline is built from those times. Media Foundation's own
    /// timestamps are the encoder's business — the frames are stamped with the capture times here
    /// so that the stream the encoder sees is the capture's own timeline.
    #[cfg(windows)]
    fn encode_texture(
        &mut self,
        texture: replayed_capture::GpuTexture,
        captured_at: Duration,
    ) -> Result<()> {
        if let Some(mft) = self.mft.as_ref() {
            mft.submit(texture, captured_at);
        }
        Ok(())
    }

    /// Complete a writer failure with ffmpeg's own exit status and stderr, if it has died.
    ///
    /// The writer thread only sees the symptom (its `write` to the child's stdin failed). A Windows
    /// capture where `h264_amf` could not start reported only "video writer thread has stopped",
    /// with the real cause (a missing `amfrt64.dll`) in ffmpeg's stderr. This encoder owns the child,
    /// so it adds the cause.
    ///
    /// Returns the error untouched when the child has not exited yet (the pipe can break before the
    /// process is reapable); inventing a cause would be worse.
    fn explain_dead_writer(&mut self, err: anyhow::Error) -> anyhow::Error {
        match self.ffmpeg_death() {
            Some(cause) => anyhow::Error::msg(format!("{err}; {cause}")),
            None => err,
        }
    }

    /// ffmpeg's exit status and stderr, when it has exited.
    ///
    /// A short bounded grace period is spent waiting first: the broken pipe is observed the instant
    /// ffmpeg closes its stdin, which can precede the process being reapable. This only happens on a
    /// path that has already failed, and is bounded, so a surviving child still yields an error.
    ///
    /// `try_wait` reaps the child; a later [`Encoder::finish`] still gets the same status from
    /// `Child::wait`.
    fn ffmpeg_death(&mut self) -> Option<String> {
        let deadline = Instant::now() + CHILD_DEATH_GRACE;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(CHILD_DEATH_POLL),
                _ => return None,
            }
        };
        // The reader has been running since the child was spawned, so the text is already
        // waiting for us in all but one case: a descendant that inherited the pipe and is
        // holding it open, in which case the text will never arrive and the budget is what
        // stops the report from waiting for it.
        let stderr = self.drain_stderr(STDERR_DRAIN_BUDGET);
        Some(format!(
            "ffmpeg exited with {status} using encoder '{}': {}",
            self.encoder_name,
            stderr.trim()
        ))
    }

    /// Everything ffmpeg wrote to stderr, but never waiting longer than `budget` for it.
    ///
    /// A plain `read_to_string` blocks until every write end of the pipe closes. A descendant that
    /// inherited the pipe and outlives a killed child keeps it open (a stand-in that forked a
    /// sleeper stalled the report for its whole 60s). Draining on its own thread and taking
    /// whatever arrived keeps the report bounded; normally the text is already buffered.
    ///
    /// A budget that expires commits nothing: the reader stays in flight
    /// ([`FfmpegEncoder::stderr_drain`]) so a later report still gets the words. Caching the
    /// timeout's empty string would make every later report print nothing where ffmpeg's reason
    /// should be; an empty string is cached only as the reader's own verdict (the channel
    /// disconnecting without a value).
    ///
    /// The text is remembered because a pipe can be drained once and both the writer-failure report
    /// and [`Encoder::finish`] want it.
    /// The child's argument list, as one line, for a failure report.
    fn argv_text(&self) -> String {
        self.argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn drain_stderr(&mut self, budget: Duration) -> String {
        if let Some(text) = &self.drained_stderr {
            return text.clone();
        }
        // The reader has been running since the encoder was spawned, so the words are usually
        // already waiting. One that is not there yet is started now: the direct constructors in
        // the tests build the encoder without going through `spawn`.
        let rx = match self.stderr_drain.take() {
            Some(rx) => rx,
            None => {
                let Some(stderr) = self.child.stderr.take() else {
                    return String::new();
                };
                stderr_reader(stderr)
            }
        };
        // Wait for the reader's verdict rather than polling for it: the wait ends the instant the
        // text arrives, and a poll loop cannot out-run a machine that is busy running the tests
        // that are busy starting the encoders.
        match rx.recv_timeout(budget) {
            Ok(text) => {
                self.drained_stderr = Some(text.clone());
                text
            }
            // The reader ended without sending: nothing more can arrive, so an empty answer is its own verdict and may be pinned.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.drained_stderr = Some(String::new());
                String::new()
            }
            // Nothing yet and no budget left: keep the reader so a later, longer call can still collect it.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.stderr_drain = Some(rx);
                String::new()
            }
        }
    }
}

/// Read ffmpeg's stderr on its own thread and hand back the channel the words arrive on.
///
/// The reader starts when the encoder is spawned, not when a report asks for the text. A report
/// that has to wait for a *newly spawned* thread to be scheduled can lose ffmpeg's reason on a
/// loaded machine — and the reason is the one thing the report exists to give.
fn stderr_reader(stderr: std::process::ChildStderr) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    rx
}

/// Whether a writer error is an audio thread's connect deadline. `accept_within` is the only
/// raiser of `TimedOut` in this module, so the error kind is the signal (the message names the input).
fn is_connect_deadline(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::TimedOut)
}

/// Hand a payload to a writer thread, or drop it if the queue is full.
///
/// `try_send` and a drop rather than an error: this runs on the live capture path, where blocking
/// stalls the loop pulling frames and failing would stop recording because the GPU fell a few
/// milliseconds behind. The timeline is wall-clock, so a missing payload is a brief repeat. The
/// counter (`Encoder::dropped_frames` / `dropped_audio_blocks`, logged by the CLI) keeps the drop
/// from being silent.
///
/// A disconnected channel means the writer thread is gone and nothing can be encoded: an error.
fn enqueue_or_drop<T>(tx: &SyncSender<T>, item: T, dropped: &AtomicU64, what: &str) -> Result<()> {
    match tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => bail!("{what} writer thread has stopped"),
    }
}

/// [`enqueue_or_drop`] for a PCM audio input, replacing what it drops with silence.
///
/// An audio input is read as a continuous byte stream, so a dropped block does not leave a brief
/// repeat as video does: the bytes after it simply play earlier, and the track ends up shorter than
/// the video it belongs to (a drift the length of everything dropped). `owed` counts the dropped
/// bytes; the next block that fits is preceded by that much silence, which keeps the track on the
/// timeline. If the queue is still full the debt grows instead, and nothing blocks.
fn enqueue_audio(
    tx: &SyncSender<Vec<u8>>,
    data: Vec<u8>,
    dropped: &AtomicU64,
    owed: &mut usize,
    what: &str,
) -> Result<()> {
    if *owed > 0 {
        match tx.try_send(vec![0; *owed]) {
            Ok(()) => *owed = 0,
            Err(TrySendError::Full(_)) => {
                dropped.fetch_add(1, Ordering::Relaxed);
                *owed += data.len();
                return Ok(());
            }
            Err(TrySendError::Disconnected(_)) => bail!("{what} writer thread has stopped"),
        }
    }
    match tx.try_send(data) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(data)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            *owed += data.len();
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => bail!("{what} writer thread has stopped"),
    }
}

/// The refusal a video-only encoder gives an audio block, a function so it can be tested without
/// a child process.
///
/// With `EncodeConfig::game_audio` false there is no audio input at all, so a block is a wiring
/// mistake, not a full queue: refused and named rather than silently dropped (as
/// [`Encoder::submit_mic_audio`] does for a block with no microphone input).
fn refuse_audio_without_input(game_audio: bool) -> Result<()> {
    if game_audio {
        return Ok(());
    }
    bail!(
        "this encoder was spawned without a game-audio input (EncodeConfig::game_audio was \
         false); the block was refused rather than silently dropped"
    )
}

/// How long ffmpeg is given to exit on its own before it is killed. A child whose inputs have
/// closed exits in milliseconds; see [`FfmpegEncoder::finish`] for why the bound exists.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Wait for a child to exit, for at most `budget`, and kill it if it will not.
///
/// `Child::wait` has no timeout and an unbounded shutdown is a hang: a hybrid recording once sat
/// alive for eight minutes with its logger already stopped. Returns the status, or `None` when
/// the child had to be killed for not providing one.
/// How the child ended.
///
/// Whether the child exited on its own or had to be killed is kept apart from its status, because
/// on Windows a killed process reports "exit code: 1" — the same number a child that failed on
/// purpose reports. A message that says only "exit code: 1" therefore cannot say what happened,
/// and a child killed because it was still waiting has written nothing to its stderr.
struct ChildEnd {
    /// The child's exit status, when it had one.
    status: Option<std::process::ExitStatus>,
    /// Whether the child had to be killed to end the wait.
    killed: bool,
}

fn wait_bounded(child: &mut Child, budget: Duration) -> ChildEnd {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return ChildEnd {
                    status: Some(status),
                    killed: false,
                };
            }
            Ok(None) => {}
            // A child that cannot be asked at all is not something to wait on.
            Err(err) => {
                tracing::warn!("could not ask ffmpeg whether it had exited: {err}");
                return ChildEnd {
                    status: None,
                    killed: false,
                };
            }
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "ffmpeg did not exit within {budget:?}; killing it so the recording can stop"
            );
            let _ = child.kill();
            // Not a wait: the process is gone, and this only reaps it.
            return ChildEnd {
                status: child.wait().ok(),
                killed: true,
            };
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn join_writer(handle: Option<WriterHandle>, what: &str) -> Result<()> {
    let Some(handle) = handle else { return Ok(()) };
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            Err(anyhow::Error::new(e)).with_context(|| format!("writing {what} to ffmpeg"))
        }
        Err(_) => bail!("{what} writer thread panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A sink that remembers when each write happened, so a test can ask when the frames went out.
    struct StampedSink(Arc<std::sync::Mutex<Vec<Instant>>>);

    impl Write for StampedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(Instant::now());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The frames are written to the pipe at their capture times, not at the moments they arrive.
    ///
    /// The defect this prevents: the hardware encoder finishes several frames at once, the child
    /// reads them at once, and the frames of a burst share a timestamp — a picture that judders.
    /// The measured frame intervals of a 4K recording swung between 8.1 ms and 36.4 ms against the
    /// 16.7 ms a 60fps video has.
    ///
    /// The frames are handed over in one burst here, exactly as the hardware encoder hands them
    /// over, and the writes must still come out one capture interval apart.
    ///
    /// The *median* interval is what is asserted, not the worst one: a machine that is busy can
    /// take a thread off its schedule for longer than a frame interval, and one such frame in a
    /// dozen is a machine's noise, not the defect. The median is what tells the two apart — the
    /// defect puts every frame of a burst in the same instant.
    #[test]
    fn the_frames_are_written_at_their_capture_times() {
        /// The interval the capture delivered, 17.4ms apart: about 57.5fps.
        const INTERVAL: Duration = Duration::from_micros(17_400);
        const FRAMES: u64 = 12;

        let (tx, rx) = mpsc::sync_channel::<VideoChunk>(FRAMES as usize);
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&writes);
        let writer = std::thread::spawn(move || {
            pump_video(rx, StampedSink(sink)).expect("the writer finishes");
        });

        // The whole burst, handed over at once: every frame carries the time it was captured at.
        for frame in 0..FRAMES {
            tx.send(VideoChunk {
                bytes: vec![0u8; 16],
                captured_at: Some(Duration::from_micros(frame * INTERVAL.as_micros() as u64)),
            })
            .expect("the queue takes the frame");
        }
        drop(tx);
        writer.join().expect("the writer thread ends");

        let writes = writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(writes.len(), FRAMES as usize, "every frame was written");
        let mut deltas: Vec<Duration> = writes.windows(2).map(|w| w[1] - w[0]).collect();
        deltas.sort();
        let median = deltas[deltas.len() / 2];
        // The tolerance is a quarter of the interval, and it is that wide because the assertion is
        // about *how* the frames are timed, not about the precision of the machine. A defective
        // writer does not compress the intervals, it erases them: every frame of a burst is
        // written at the instant the burst arrived, which puts the median at nothing at all. A
        // busy machine takes the writer off its schedule and lets it catch up, which compresses
        // the intervals it writes — measured on a loaded CI runner, a 17.4ms interval came back
        // as 16.3ms — and that is the machine, not the defect.
        assert!(
            median.abs_diff(INTERVAL) < INTERVAL / 4,
            "the frames must be written one capture interval ({INTERVAL:?}) apart, not as they \
             arrive: the median interval was {median:?} ({deltas:?})",
        );
    }

    /// A stand-in child process, in the shell this platform has.
    ///
    /// These two tests used `sh`, which exists on the development host and not on Windows: on the
    /// box both failed with "program not found" before reaching an assertion, and had done since
    /// they were written. A test that cannot run is not a test, so the shell is chosen per
    /// platform and the child's own script lives in the case below.
    fn stand_in_script(windows: &str, unix: &str) -> std::process::Command {
        if cfg!(windows) {
            let mut cmd = std::process::Command::new("cmd");
            cmd.args(["/C", windows]);
            cmd
        } else {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", unix]);
            cmd
        }
    }

    /// A child that exits on its own is not a child that was killed, and on Windows the two report
    /// the same number. A report that cannot tell them apart cannot say what happened.
    #[test]
    fn a_child_that_exits_is_not_reported_as_killed() {
        let mut child = stand_in_script("exit 1", "exit 1")
            .spawn()
            .expect("the stand-in starts");
        let end = wait_bounded(&mut child, std::time::Duration::from_secs(5));
        assert!(!end.killed, "a child that exited on its own was not killed");
        assert!(
            end.status.is_some(),
            "a child that exited on its own has a status"
        );
    }

    #[test]
    fn a_child_that_will_not_go_is_reported_as_killed() {
        // `ping` is the Windows sleep that needs no console and no extra program: one echo per
        // second, thirty of them.
        let mut child = stand_in_script("ping -n 31 127.0.0.1 >NUL", "sleep 30")
            .spawn()
            .expect("the stand-in starts");
        let end = wait_bounded(&mut child, std::time::Duration::from_millis(100));
        assert!(
            end.killed,
            "a child that had to be killed is reported as killed"
        );
    }

    // The stand-in children are `#!/bin/sh` scripts and their liveness is checked with
    // `kill -0`, so both imports belong to the Unix-only tests.
    #[cfg(unix)]
    use std::path::Path;
    #[cfg(unix)]
    use std::process::Command;

    /// Constant-bitrate rate control reaches the argument list for every encoder family, with the
    /// vendor-specific switch only where the vendor names one.
    #[test]
    fn rate_control_holds_the_bitrate_for_each_encoder_family() {
        let get = |args: &[String], flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
        };
        for enc in ["h264_nvenc", "hevc_amf", "h264_qsv", "libx264"] {
            let args = rate_control_args(enc, RateControl::Cbr, 20_000);
            assert_eq!(get(&args, "-b:v").as_deref(), Some("20000k"), "{enc}");
            assert_eq!(get(&args, "-maxrate").as_deref(), Some("20000k"), "{enc}");
            assert_eq!(get(&args, "-bufsize").as_deref(), Some("20000k"), "{enc}");
        }
        assert_eq!(
            get(&rate_control_args("h264_nvenc", RateControl::Cbr, 1), "-rc").as_deref(),
            Some("cbr")
        );
        assert_eq!(
            get(&rate_control_args("hevc_amf", RateControl::Cbr, 1), "-rc").as_deref(),
            Some("cbr")
        );
        assert_eq!(
            get(&rate_control_args("h264_qsv", RateControl::Cbr, 1), "-rc"),
            None
        );
        assert_eq!(
            get(&rate_control_args("libx264", RateControl::Cbr, 1), "-rc"),
            None
        );
    }

    /// The mode asked for is the mode the encoder is told, per vendor.
    ///
    /// The three families spell the same three modes differently, and a mode that reached the
    /// wrong vendor's switch would be refused by ffmpeg — the recording would not start at all.
    /// Constant bitrate is the exception that keeps the old argument list: QSV is left alone
    /// because ffmpeg's QSV encoder already infers it.
    #[test]
    fn the_rate_control_mode_reaches_the_vendors_own_switch() {
        let rc = |enc: &str, mode| {
            let args = rate_control_args(enc, mode, 20_000);
            args.iter()
                .position(|a| a == "-rc")
                .map(|i| args[i + 1].clone())
        };
        assert_eq!(rc("h264_nvenc", RateControl::Vbr).as_deref(), Some("vbr"));
        assert_eq!(
            rc("hevc_nvenc", RateControl::Quality).as_deref(),
            Some("constqp")
        );
        assert_eq!(rc("h264_qsv", RateControl::Vbr).as_deref(), Some("vbr"));
        assert_eq!(rc("hevc_qsv", RateControl::Quality).as_deref(), Some("icq"));
        assert_eq!(rc("h264_amf", RateControl::Vbr).as_deref(), Some("vbr"));
        assert_eq!(rc("hevc_amf", RateControl::Quality).as_deref(), Some("cqp"));
        // Constant bitrate on QSV stays unsaid, as it has always been.
        assert_eq!(rc("h264_qsv", RateControl::Cbr), None);
    }

    /// A constant-quality mode has no bitrate target, so the bitrate arguments are left out.
    ///
    /// Passing `-b:v` next to `-rc constqp` would describe a number the encoder ignores, and the
    /// file's size would follow the picture rather than the setting — the user would see a
    /// bitrate they configured and a file of a different size.
    #[test]
    fn a_constant_quality_mode_carries_no_bitrate_target() {
        for enc in ["h264_nvenc", "h264_qsv", "h264_amf"] {
            let args = rate_control_args(enc, RateControl::Quality, 20_000);
            assert!(
                !args
                    .iter()
                    .any(|a| a == "-b:v" || a == "-maxrate" || a == "-bufsize"),
                "a constant-quality mode has no bitrate target: {enc} -> {args:?}"
            );
            assert!(
                args.iter().any(|a| a == "-rc"),
                "but it does say which mode: {enc} -> {args:?}"
            );
        }
    }

    /// A preset is passed on only where the encoder has one.
    ///
    /// ffmpeg refuses an unknown `-preset` value, so a preset that does not apply to the
    /// selected encoder must be dropped rather than passed: the recording would not start.
    #[test]
    fn a_preset_reaches_the_encoder_that_has_one_and_is_dropped_where_it_does_not() {
        assert_eq!(
            preset_args("h264_nvenc", "p4"),
            vec!["-preset".to_string(), "p4".to_string()]
        );
        assert_eq!(
            preset_args("h264_qsv", "medium"),
            vec!["-preset".to_string(), "medium".to_string()]
        );
        assert_eq!(
            preset_args("hevc_amf", "quality"),
            vec!["-preset".to_string(), "quality".to_string()]
        );
        // No preset asked for: nothing is said.
        assert!(preset_args("h264_nvenc", "").is_empty());
        // A preset that belongs to another family: dropped, not passed on.
        assert!(
            preset_args("h264_qsv", "p4").is_empty(),
            "a QSV encoder has no p4"
        );
        assert!(
            preset_args("h264_nvenc", "balanced").is_empty(),
            "an NVENC encoder has no balanced"
        );
    }

    /// The preset and the rate control reach the child's argument list.
    ///
    /// Both are optional refinements, so the list they produce has to be the old one plus what
    /// was asked for — and exactly the old one when nothing was.
    #[test]
    fn the_preset_and_the_mode_reach_the_childs_argument_list() {
        let cfg = args_cfg(30, 1000);
        assert!(
            !video_output_args(&cfg).iter().any(|a| a == "-preset"),
            "no preset asked for: none is said"
        );

        // A hardware encoder, so the vendor's own switch is the one that is spoken.
        let mut cfg = args_cfg(30, 1000);
        cfg.software_encoder = Some("h264_nvenc");
        cfg.preset = "p4".to_string();
        let args = video_output_args(&cfg);
        let at = args.iter().position(|a| a == "-preset").expect("-preset");
        assert_eq!(args[at + 1], "p4");

        cfg = args_cfg(30, 1000);
        cfg.software_encoder = Some("h264_nvenc");
        cfg.rate_control = RateControl::Vbr;
        let args = video_output_args(&cfg);
        let at = args.iter().position(|a| a == "-rc").expect("-rc");
        assert_eq!(args[at + 1], "vbr");
    }

    /// A configuration the argument builders can be exercised with; these tests are about the argument list only.
    fn args_cfg(fps: u32, segment_ms: u64) -> EncodeConfig {
        let mut cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            1280,
            720,
            fps,
            ".".into(),
            segment_ms,
        );
        cfg.bitrate_kbps = 20_000;
        cfg
    }

    /// The rate in the child's argument list is `EncodeConfig::fps` and nothing else: the pacer is
    /// built from the same number (`Encoder::input_fps`), so declaring a different rate must fail here.
    #[test]
    fn the_declared_rate_is_the_configurations_fps_and_it_is_declared_as_framerate() {
        for fps in [10u32, 24, 30, 60] {
            let cfg = args_cfg(fps, 1000);
            let args = video_input_args(&cfg);
            let at = |flag: &str| {
                let i = args
                    .iter()
                    .position(|a| a == flag)
                    .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
                args.get(i + 1).cloned().unwrap_or_default()
            };
            assert_eq!(
                at("-framerate"),
                fps.to_string(),
                "the declared rate: {args:?}"
            );
            assert_eq!(
                at("-s"),
                "1280x720",
                "the rawvideo pipe is the SOURCE size: {args:?}"
            );
            assert!(
                !args.iter().any(|a| a == "-r"),
                "`-r` before `-i` re-stamps every frame onto a rigid grid and undoes the \
                 arrival timestamps this option records (measured; see the module comment): \
                 {args:?}"
            );
            assert_eq!(at("-use_wallclock_as_timestamps"), "1", "{args:?}");
        }
    }

    /// The hybrid's argument pair: what must be there, and more importantly what must not.
    ///
    /// `-force_key_frames` and `-g` cannot apply to a copied stream, so including them would look
    /// right and impose nothing. `-r` and `-framerate` on the input would re-grid it. `-vf scale`
    /// would mean decoding the pixels this path exists to avoid.
    #[test]
    fn the_bitstream_args_copy_the_video_and_impose_nothing_on_it() {
        let input = video_input_args_bitstream(crate::VideoCodec::H264);
        let got: Vec<&str> = input.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            [
                "-f",
                "h264",
                "-use_wallclock_as_timestamps",
                "1",
                "-i",
                "pipe:0"
            ],
            "the input is an H.264 elementary stream, stamped on arrival"
        );
        for forbidden in ["-r", "-framerate"] {
            assert!(
                !input.iter().any(|a| a == forbidden),
                "{forbidden} would re-grid the input, which is the bug issue #2 was: {input:?}"
            );
        }

        let output = video_output_args_bitstream(crate::VideoCodec::H264);
        let got: Vec<&str> = output.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            ["-c:v", "copy"],
            "the video is copied, not encoded again"
        );
        for forbidden in ["-force_key_frames", "-g", "-b:v", "-fps_mode", "-vf"] {
            assert!(
                !output.iter().any(|a| a == forbidden),
                "{forbidden} cannot apply to a copied stream, and including it would impose \
                 nothing while looking correct: {output:?}"
            );
        }
    }

    /// The capability the caller asks capture with follows the configuration, both ways.
    ///
    /// The recorder tells capture to hand out textures based on what an encoder says it can take; a
    /// disagreement either way is a broken recording. `RawPixels` must stay the default.
    #[test]
    fn the_reported_texture_capability_follows_the_configuration() {
        let mut cfg = args_cfg(30, 1000);
        assert!(
            !cfg.accepts_textures(),
            "raw pixels is the default: capture must not be asked for textures by accident"
        );
        cfg.video = VideoInput::EncodedBitstream;
        assert!(
            cfg.accepts_textures(),
            "an encoded bitstream can only be produced from a texture, so the caller has to ask for one"
        );
    }

    /// A bitstream input produces the hybrid's pair, and a scaled one is refused.
    ///
    /// `-s` and `-framerate` describe a pipe of pixels and must be gone; `-b:v` and
    /// `-force_key_frames` describe work ffmpeg does not do here. A scaled output is a contradiction
    /// (scaling means decoding), so it must be an error, not a silently different geometry. The
    /// audio input and output packaging are unchanged.
    #[test]
    fn a_bitstream_input_copies_the_video_and_refuses_to_scale_it() {
        let mut cfg = args_cfg(30, 1000);
        cfg.video = VideoInput::EncodedBitstream;

        let args = ffmpeg_args(&cfg, "tcp://127.0.0.1:1", None);
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(text.windows(2).any(|w| w == ["-f", "h264"]), "{text:?}");
        assert!(
            text.windows(2)
                .any(|w| w == ["-use_wallclock_as_timestamps", "1"]),
            "an elementary stream carries no timestamps of its own: {text:?}"
        );
        assert!(text.windows(2).any(|w| w == ["-c:v", "copy"]), "{text:?}");
        for forbidden in ["-s", "-framerate", "-b:v", "-force_key_frames"] {
            assert!(
                !text.iter().any(|a| a == forbidden),
                "{forbidden} belongs to the rawvideo path and must not survive into this one: \
                 {text:?}"
            );
        }
        assert!(
            text.windows(2).any(|w| w == ["-c:a", "aac"]),
            "audio is unchanged: {text:?}"
        );
        assert!(
            text.windows(2).any(|w| w == ["-f", "segment"]),
            "so is the output: {text:?}"
        );

        // A scaled output is refused before anything is launched; the binary named here does not exist.
        cfg.source_size = (1280, 720);
        cfg.output_size = (640, 360);
        let bin = FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        // Matched rather than `expect_err`, which would need `Debug` on an encoder that owns a child process.
        let refused = match FfmpegEncoder::spawn(&bin, &cfg) {
            Ok(_) => panic!("a scaled bitstream must be refused, and this one was not"),
            Err(err) => err,
        };
        assert!(
            refused.to_string().contains("cannot be scaled"),
            "the refusal must name the reason rather than fail later: {refused}"
        );
    }

    /// The scale filter is added only when the output size differs, and the GOP follows the declared
    /// rate; the throughput probe inherits both.
    #[test]
    fn the_output_arguments_scale_only_when_asked_and_size_the_gop_from_the_rate() {
        let same = args_cfg(30, 1000);
        assert!(
            !video_output_args(&same).iter().any(|a| a == "-vf"),
            "equal sizes must not add a pointless scale pass"
        );

        let mut scaled = args_cfg(30, 1000);
        scaled.output_size = (1920, 1080);
        let args = video_output_args(&scaled);
        let vf = args
            .iter()
            .position(|a| a == "-vf")
            .expect("a scale filter");
        assert_eq!(args[vf + 1], "scale=1920:1080");
        assert!(args.contains(&"-c:v".to_string()));

        // One keyframe per segment: at 30fps and 1s segments the GOP is 30, at 2s it is 60.
        let gop = |cfg: &EncodeConfig| {
            let args = video_output_args(cfg);
            let i = args.iter().position(|a| a == "-g").expect("-g");
            args[i + 1].parse::<u32>().expect("a GOP number")
        };
        assert_eq!(gop(&args_cfg(30, 1000)), 30);
        assert_eq!(gop(&args_cfg(30, 2000)), 60);
        assert_eq!(gop(&args_cfg(24, 1000)), 24);
    }

    /// The audio bitrate in the child's argument list is `EncodeConfig::audio_bitrate_kbps` and nothing else.
    ///
    /// The byte-for-byte test below runs with the constructor's own 128 kbps, so a re-hardcoded
    /// `-b:a 128k` would still match it; asking for a value the constructor would never pick makes this bite.
    #[test]
    fn the_audio_bitrate_in_the_argument_list_is_the_configurations() {
        let mut cfg = args_cfg(30, 1_000);
        cfg.audio_bitrate_kbps = 96;
        let text = strings(&ffmpeg_args(&cfg, "tcp://127.0.0.1:41234", None));
        let at = text
            .iter()
            .position(|a| a == "-b:a")
            .unwrap_or_else(|| panic!("-b:a missing from {text:?}"));
        assert_eq!(
            text[at + 1],
            "96k",
            "the configured rate, not the constructor's 128: {text:?}"
        );
    }

    /// `[audio] enabled = false` is a video-only recording, visible in the argument list: no audio
    /// input and no audio encoder arguments.
    ///
    /// An audio input left in the list is a listener nothing dials, so ffmpeg waits out
    /// `AUDIO_CONNECT_TIMEOUT` at every start; a stray `-c:a` describes a stream no input can produce.
    /// Neither is an error, which is how the `-start_number` and `-r` mistakes read.
    #[test]
    fn the_argument_list_without_the_game_audio_declares_no_audio_input_or_codec() {
        let mut cfg = args_cfg(30, 1_000);
        cfg.game_audio = false;

        let text = strings(&ffmpeg_args(&cfg, "tcp://127.0.0.1:41234", None));
        assert!(
            !text.iter().any(|a| a.starts_with("tcp://")),
            "no audio input may be declared: {text:?}"
        );
        for audio_arg in ["-c:a", "-b:a", "-ac", "-ar"] {
            assert!(
                !text.iter().any(|a| a == audio_arg),
                "{audio_arg} has no audio stream to describe: {text:?}"
            );
        }
        // The video half is untouched, so this cannot pass by the list having gone empty.
        assert!(
            text.iter().any(|a| a == "pipe:0"),
            "the video input is still declared: {text:?}"
        );
        assert!(
            text.iter().any(|a| a == "-segment_time"),
            "and so is the segmenter: {text:?}"
        );
    }

    /// A block offered to an encoder with no audio input is refused, not queued and not silently
    /// dropped: a quiet drop would leave the caller believing the sound was captured. Same
    /// disposition as [`Encoder::submit_mic_audio`] with no microphone.
    #[test]
    fn a_block_offered_to_an_encoder_without_the_game_audio_is_refused() {
        let err = refuse_audio_without_input(false)
            .expect_err("an encoder with no audio input cannot accept a block");
        assert!(
            err.to_string().contains("game_audio"),
            "the refusal must name the configuration key: {err}"
        );
        assert!(
            err.to_string()
                .contains("refused rather than silently dropped"),
            "and say what it did with the block: {err}"
        );
        // The other direction, so this cannot pass by refusing everything.
        refuse_audio_without_input(true).expect("an encoder with the game audio accepts the block");
    }

    /// The child reaches the audio input before the video input blocks it — the behaviour the
    /// input order is for.
    ///
    /// The deadlock of issue #78: ffmpeg opens its inputs in the order they are declared, and the
    /// raw H.264 input's *probe blocks* until it has seen a keyframe carrying the sequence
    /// header. With the video declared first the child sat inside that probe and never opened the
    /// audio socket, so the recorder's audio writer waited out its ten-second connect deadline for
    /// a connection that could not happen and the recording died before it began.
    ///
    /// Asserting the order of the arguments does not prove the behaviour, so the stand-in child
    /// emulates it: it opens its inputs in the order it was given them, and the video input blocks
    /// until it has seen a keyframe — which never comes, because only raw pixels are on that pipe.
    /// So the stand-in reaches the audio input only if the audio input was declared first, and it
    /// records having reached it.
    ///
    /// Unix-only: the stand-in is a `#!/bin/bash` script.
    #[cfg(unix)]
    #[test]
    fn the_child_reaches_the_audio_input_before_the_video_input_blocks_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("audio-input-opened");
        let stub = dir.path().join("ffmpeg");
        std::fs::write(
            &stub,
            format!(
                r#"#!/bin/bash
for arg in "$@"; do
  case "$arg" in
    tcp://*)
      port="${{arg##*:}}"
      # Dial the recorder's listener, as ffmpeg does when it opens this input.
      ( exec 3<>/dev/tcp/127.0.0.1/"$port" ) 2>/dev/null
      echo "$arg" > '{marker}'
      ;;
    pipe:0)
      # The video input's probe: it cannot be opened until a keyframe has been seen.
      while IFS= read -r line; do :; done
      ;;
  esac
done
sleep 30
"#,
                marker = marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };
        let cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        let encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn");

        // Nothing is fed to the child, so the video input would block for ever if it came
        // first: the stand-in has to reach the audio input on its own.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "the child must reach the audio input before the video input's probe blocks it: it \
             never opened the audio input. It was asked to: {}",
            encoder.argv_text()
        );
    }

    /// The argument list as the os-string vector `Command::args` receives it. Every element is
    /// ASCII except the segment pattern path, which is derived the same way on both sides.
    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// With `mic_audio: None` the child is spawned with exactly the one-track arguments.
    ///
    /// The baseline is written out in full, deliberately: an expectation built from the builders
    /// under test would assert nothing. Adding, removing, reordering or re-spelling anything on the
    /// single-audio path fails here (the `-start_number` and `-r` mistakes looked equivalent; see
    /// [`ffmpeg_args`]).
    ///
    /// **This list changed on purpose** (issue #78): the audio input is declared before the video
    /// one, and the output is mapped explicitly because the default stream selection cannot be
    /// relied on once the inputs are in that order. What did *not* change is the file: the video
    /// is still the first output stream and the game audio the first audio stream, so the track
    /// names (`replayed_media::edit::audio_titles`) still land on the right tracks.
    #[test]
    fn the_argument_list_without_a_microphone_declares_the_audio_first_and_maps_it_explicitly() {
        let cfg = args_cfg(30, 1_000);

        let got = strings(&ffmpeg_args(&cfg, "tcp://127.0.0.1:41234", None));
        let mut expected: Vec<String> = vec![
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            // Input 0: raw s16le PCM over loopback TCP. FIRST, because the video input's probe
            // blocks and the audio socket must not be behind it (issue #78).
            "-f",
            "s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-i",
            "tcp://127.0.0.1:41234",
            // Input 1: raw BGRA video on stdin, at the declared rate, timed from arrival.
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-s",
            "1280x720",
            "-framerate",
            "30",
            "-use_wallclock_as_timestamps",
            "1",
            "-i",
            "pipe:0",
            // The video encoder: passthrough frame-rate mode, codec, bitrate, GOP and forced keyframes.
            "-fps_mode",
            "passthrough",
            "-c:v",
            "libx264",
            "-b:v",
            "20000k",
            "-maxrate",
            "20000k",
            "-bufsize",
            "20000k",
            "-g",
            "30",
            "-force_key_frames",
            "expr:gte(t,n_forced*1)",
            // The audio codec, then the segment muxer and its numbering.
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            // The mapping: the video is input 1 and the game audio is input 0. The order is the
            // order the output has always had — the video first, then the game audio — so the
            // file is the same file it was.
            "-map",
            "1:v",
            "-map",
            "0:a",
            "-f",
            "segment",
            "-segment_time",
            "1",
            "-segment_format",
            "mp4",
            "-reset_timestamps",
            "1",
            "-segment_start_number",
            "0",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        // The one real path, built the way the encoder builds it, so the comparison does not depend on the path separator.
        expected.push(
            cfg.scratch_dir
                .join("seg-%06d.mp4")
                .to_string_lossy()
                .into_owned(),
        );

        assert_eq!(
            got, expected,
            "the single-audio argument list must be exactly this: the audio input first, the \
             video second, and the two mapped explicitly onto the tracks they have always been"
        );
    }

    /// The audio input is declared before the video one, and the mapping says which input each
    /// track came from.
    ///
    /// The order is the fix for issue #78 and it is worth a test of its own, because it is the
    /// one thing in the argument list that is not visible in the file: ffmpeg opens its inputs in
    /// the order they are declared, and the raw video input's probe blocks until it has seen a
    /// keyframe carrying the sequence header. With the video first, the child never opened the
    /// audio socket and the recording died on the audio writer's connect deadline.
    ///
    /// The mapping is the other half: the input order changed, so the tracks cannot be left to
    /// ffmpeg's default selection. The game audio has to stay the *first audio stream* and the
    /// microphone the second, because that is the position they are named by
    /// (`replayed_media::edit::audio_titles`).
    #[test]
    fn the_audio_input_is_declared_before_the_video_one_and_the_tracks_are_mapped_explicitly() {
        let mut cfg = args_cfg(30, 1_000);
        cfg.mic_audio = Some(crate::MicAudioSpec::default());
        let text = strings(&ffmpeg_args(
            &cfg,
            "tcp://127.0.0.1:41234",
            Some("tcp://127.0.0.1:43210"),
        ));

        // The inputs, in the order they are declared.
        let inputs: Vec<&str> = text
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.starts_with("tcp://") || *arg == "pipe:0")
            .map(|(_, arg)| arg.as_str())
            .collect();
        assert_eq!(
            inputs,
            ["tcp://127.0.0.1:41234", "tcp://127.0.0.1:43210", "pipe:0"],
            "the audio inputs are declared before the video: the child's connection to them \
             cannot be stuck behind the video's probe"
        );

        // The mapping, in the order the output has.
        let maps: Vec<&str> = text
            .iter()
            .enumerate()
            .filter(|(_, arg)| *arg == "-map")
            .map(|(index, _)| text[index + 1].as_str())
            .collect();
        assert_eq!(
            maps,
            ["2:v", "0:a", "1:a"],
            "the video is the last input and the audio the first two, and the output keeps the \
             order it always had: the video, the game audio, the microphone"
        );

        // And the audio tracks are named by the position the mapping gives them: the game audio
        // is the first audio stream, the microphone the second.
        let titles: Vec<&str> = text
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.starts_with("-metadata:s:a:"))
            .map(|(index, _)| text[index + 1].as_str())
            .collect();
        assert_eq!(
            titles,
            ["title=Game Audio", "title=Microphone"],
            "the tags follow the audio streams, and the audio streams follow the mapping"
        );
    }

    /// Enabling the microphone adds its own input and the mapping/tagging, and moves nothing else.
    ///
    /// Input 1 is `tcp://127.0.0.1:{mic_audio_port}` declared from the caller's `MicAudioSpec`, and
    /// the output carries `-map 2:v -map 0:a -map 1:a` with `title=Game Audio` and
    /// `title=Microphone`. The video arguments are the same elements in the same order as the
    /// single-audio list.
    #[test]
    fn enabling_the_microphone_adds_its_own_input_its_mapping_and_its_tags() {
        let mut cfg = args_cfg(30, 1_000);
        // Deliberately not the default spec: the rate and channel count are numbers the child is told
        // (`-ar` / `-ac`), and a spec that never reached them would put the track on a wrong timeline.
        cfg.mic_audio = Some(crate::MicAudioSpec {
            sample_rate: 44_100,
            channels: 1,
            sample_format: SampleFormat::S16Le,
        });

        let got = strings(&ffmpeg_args(
            &cfg,
            "tcp://127.0.0.1:41234",
            Some("tcp://127.0.0.1:43210"),
        ));
        let mut expected: Vec<String> = vec![
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            // Input 0: the game audio, over loopback TCP.
            "-f",
            "s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-i",
            "tcp://127.0.0.1:41234",
            // Input 1, from the configuration's spec.
            "-f",
            "s16le",
            "-ar",
            "44100",
            "-ac",
            "1",
            "-i",
            "tcp://127.0.0.1:43210",
            // Input 2: the video, on stdin.
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-s",
            "1280x720",
            "-framerate",
            "30",
            "-use_wallclock_as_timestamps",
            "1",
            "-i",
            "pipe:0",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "libx264",
            "-b:v",
            "20000k",
            "-maxrate",
            "20000k",
            "-bufsize",
            "20000k",
            "-g",
            "30",
            "-force_key_frames",
            "expr:gte(t,n_forced*1)",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            // What the changed input order needs: the tracks mapped explicitly, in the order the
            // output has always had — the video, the game audio, the microphone.
            "-map",
            "2:v",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-metadata:s:a:0",
            "title=Game Audio",
            "-metadata:s:a:1",
            "title=Microphone",
            "-f",
            "segment",
            "-segment_time",
            "1",
            "-segment_format",
            "mp4",
            "-reset_timestamps",
            "1",
            "-segment_start_number",
            "0",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        expected.push(
            cfg.scratch_dir
                .join("seg-%06d.mp4")
                .to_string_lossy()
                .into_owned(),
        );

        assert_eq!(
            got, expected,
            "the microphone adds its input, its mapping and its tags"
        );
    }

    #[test]
    fn dropped_audio_is_replaced_by_silence_once_the_queue_has_room() {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let dropped = AtomicU64::new(0);
        let mut owed = 0usize;

        enqueue_audio(&tx, vec![1; 4], &dropped, &mut owed, "audio").unwrap();
        enqueue_audio(&tx, vec![2; 4], &dropped, &mut owed, "audio").unwrap();
        enqueue_audio(&tx, vec![3; 8], &dropped, &mut owed, "audio").unwrap();
        assert_eq!((dropped.load(Ordering::Relaxed), owed), (2, 12));

        assert_eq!(rx.try_recv().unwrap(), vec![1; 4]);
        // Room again: the debt goes first (silence), and the block that follows waits its turn.
        enqueue_audio(&tx, vec![4; 4], &dropped, &mut owed, "audio").unwrap();
        assert_eq!(
            owed, 4,
            "the new block did not fit behind the silence, so it is owed too"
        );
        assert_eq!(rx.try_recv().unwrap(), vec![0; 12]);
        enqueue_audio(&tx, vec![5; 4], &dropped, &mut owed, "audio").unwrap();
        assert_eq!(rx.try_recv().unwrap(), vec![0; 4]);
        assert_eq!(owed, 4, "the silence took the slot again");
    }

    #[test]
    fn a_payload_that_fits_is_queued_and_not_counted_as_dropped() {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);

        enqueue_or_drop(&tx, 7, &dropped, "video").expect("a free slot must accept the payload");

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(rx.try_recv().expect("the payload is queued"), 7);
    }

    #[test]
    fn a_full_queue_drops_the_payload_and_counts_it_instead_of_blocking_or_failing() {
        // Capacity 1, nothing drains it: the "encoder cannot keep up" state.
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);

        enqueue_or_drop(&tx, 1, &dropped, "video").expect("a free slot accepts the payload");
        // These must return immediately and successfully: blocking would stall capture, an error would stop recording.
        enqueue_or_drop(&tx, 2, &dropped, "video").expect("a full queue drops, it does not fail");
        enqueue_or_drop(&tx, 3, &dropped, "video").expect("a full queue drops, it does not fail");

        assert_eq!(
            dropped.load(Ordering::Relaxed),
            2,
            "both refused payloads are counted"
        );
        assert_eq!(
            rx.try_recv().expect("the queued payload is still there"),
            1,
            "the queued payload is untouched"
        );
        assert!(
            rx.try_recv().is_err(),
            "the dropped payloads were not smuggled in behind it"
        );
    }

    #[test]
    fn a_stopped_writer_thread_is_an_error_not_a_drop() {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let dropped = AtomicU64::new(0);
        drop(rx);

        let err = enqueue_or_drop(&tx, 1, &dropped, "video")
            .expect_err("nothing can be encoded once the writer thread is gone");
        assert!(
            err.to_string().contains("video writer thread has stopped"),
            "the error must name the dead writer: {err}"
        );
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            0,
            "a failure is not a drop"
        );
    }

    /// A pipe can be read once but two reports want ffmpeg's words (the writer failure and the
    /// failure [`Encoder::finish`] raises afterwards); the second must not be left with nothing.
    #[cfg(unix)]
    #[test]
    fn stderr_is_remembered_so_a_second_report_is_not_left_empty() {
        let child = Command::new("/bin/sh")
            .args(["-c", "echo 'DLL amfrt64.dll failed to open' >&2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Written by hand because `spawn` needs a real ffmpeg and audio listener; the smallest thing with a child that has stderr.
        let mut encoder = FfmpegEncoder {
            child,
            output_stream: None,
            video_tx: None,
            audio_tx: None,
            mic_tx: None,
            video_writer: None,
            audio_writer: None,
            mic_writer: None,
            encoder_name: "h264_amf",
            source_size: (320, 240),
            input_fps: 30,
            accepts_textures: false,
            mic_port: None,
            mic_spec: None,
            // No audio input: `audio_tx`/`audio_writer` above are `None` to match.
            game_audio: false,
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
            dropped_mic_audio: AtomicU64::new(0),
            owed_audio: 0,
            owed_mic_audio: 0,
            drained_stderr: None,
            // No report has asked for stderr yet, so no reader is in flight.
            stderr_drain: None,
            argv: Vec::new(),
        };
        encoder.child.wait().unwrap();

        let first = encoder.drain_stderr(Duration::from_secs(1));
        assert!(
            first.contains("DLL amfrt64.dll failed to open"),
            "got: {first:?}"
        );
        let second = encoder.drain_stderr(Duration::from_secs(1));
        assert_eq!(
            second, first,
            "the second report gets the same words, not nothing"
        );
    }

    /// A budget that runs out before the words arrive must not be mistaken for a child that said nothing.
    ///
    /// Caching the `String::default()` a timeout produced made one slow drain a permanent claim that
    /// ffmpeg was silent, so later reports printed an empty line where its reason should be. The stub
    /// holds its words back past a short budget so the timeout is certain, and the same encoder must
    /// still hand them to a later call that can outlast it.
    ///
    /// Unix-only: the stand-in is a `#!/bin/sh` script.
    #[cfg(unix)]
    #[test]
    fn a_drain_that_times_out_is_not_remembered_as_an_empty_report() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        // The words arrive after the first budget; the trailing sleep keeps the pipe open, which the reader waits on.
        std::fs::write(
            &stub,
            "#!/bin/sh\nsleep 0.5; echo 'late words from ffmpeg' >&2; sleep 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };
        let cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn");

        let early = encoder.drain_stderr(Duration::from_millis(50));
        assert_eq!(early, "", "a budget that expired reports nothing yet");
        let late = encoder.drain_stderr(Duration::from_secs(3));
        assert!(
            late.contains("late words from ffmpeg"),
            "the words written after the first budget must still be collected, not lost to it: \
             {late:?}"
        );
        let again = encoder.drain_stderr(Duration::from_secs(3));
        assert_eq!(
            again, late,
            "and they are remembered rather than drained a second time"
        );
    }

    /// A shutdown is bounded: a child that will not exit is killed rather than waited for.
    ///
    /// A hybrid recording once sat alive for eight minutes after its logger stopped, with nothing
    /// that would ever end it. The stub child never exits and never reads, like that wedged ffmpeg,
    /// so the assertion is about time: `finish` has to come back. An error naming the kill is the
    /// success case; the failure pinned is a call that never returns.
    ///
    /// The bound allows for the audio input's own connect deadline, the one other fixed wait on this
    /// path; everything else is bounded by `SHUTDOWN_BUDGET`.
    ///
    /// Unix-only: the stand-in child is a `#!/bin/sh` script, and without the gate the crate's test
    /// build fails on Windows.
    #[cfg(unix)]
    #[test]
    fn a_shutdown_does_not_wait_for_a_child_that_will_not_exit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        // `exec` so the process killed is the one that would not exit, not a shell in front of it.
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };
        let cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn");

        let ceiling = AUDIO_CONNECT_TIMEOUT + SHUTDOWN_BUDGET + Duration::from_secs(5);
        let started = Instant::now();
        let outcome = encoder.finish();
        let took = started.elapsed();

        assert!(
            took < ceiling,
            "a shutdown must end: the bound is {ceiling:?} and this took {took:?}, which is what \
             an unbounded wait on the child looks like"
        );
        // The child never dials the audio socket, so the audio connect deadline is what reports here;
        // the assertion is that the bounded wait's status is named, whichever path named it.
        if let Err(err) = outcome {
            let text = err.to_string();
            assert!(
                text.contains("ffmpeg status") || text.contains("did not exit"),
                "a shutdown that had to kill the child must say so rather than pass it over: {err:#}"
            );
        }
    }

    /// A dropped encoder kills its child and reaps it.
    ///
    /// The failure this prevents: `Prepared::begin` spawns the child and then has fallible steps
    /// left, and when one of them fails the encoder is dropped without `finish()` ever being
    /// called. `Child`'s own `Drop` neither kills nor reaps the process, so the child kept running
    /// and nothing ever waited for it.
    ///
    /// The stand-in records its own pid and then never exits, so the test can ask the operating
    /// system whether the process is still there. `kill -0` succeeds for a zombie as well — a
    /// reaped process is the only one that fails it — so this asserts the reaping, not merely the
    /// killing.
    ///
    /// Unix-only: the stand-in is a `#!/bin/sh` script and the liveness check is `kill -0`.
    #[cfg(unix)]
    #[test]
    fn a_dropped_encoder_kills_and_reaps_its_child() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("ffmpeg.pid");
        let stub = dir.path().join("ffmpeg");
        // `exec` so the process that is killed is the one that would not exit, and so the pid the
        // shell records is the pid that ends up owning it.
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho $$ > \"{}\"\nexec sleep 600\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };
        let mut cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        // No game audio: the audio pump would spend its whole connect deadline waiting for a dial
        // that never comes, and this test is about the child, not about that wait.
        cfg.game_audio = false;
        let encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn");

        let pid = wait_for_pid(&pidfile);
        assert!(process_exists(pid), "the stand-in is running");

        // The encoder is dropped here, exactly as a failed start drops it: `finish` is never
        // called.
        drop(encoder);

        assert!(
            !process_exists(pid),
            "the child must be gone, and reaped: `kill -0` still succeeds on a zombie"
        );
    }

    /// A finished encoder is not killed again when it is dropped.
    ///
    /// `finish` is the graceful path and `Drop` is the backstop, and the two must not fight over
    /// the same child. The stand-in exits on its own, so `finish` finds it already gone; the drop
    /// that follows has nothing left to kill, and the process is gone either way.
    ///
    /// Unix-only: the stand-in is a `#!/bin/sh` script.
    #[cfg(unix)]
    #[test]
    fn a_finished_encoder_is_not_killed_again_when_it_is_dropped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("ffmpeg.pid");
        let stub = dir.path().join("ffmpeg");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho $$ > \"{}\"\nsleep 0.2\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };
        let mut cfg = EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        cfg.game_audio = false;
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn");

        let pid = wait_for_pid(&pidfile);
        encoder
            .finish()
            .expect("the stand-in exits on its own, so the shutdown succeeds");

        drop(encoder);

        assert!(
            !process_exists(pid),
            "the child is gone, and the drop did not have to kill it a second time"
        );
    }

    /// The pid the stand-in child recorded, once it has written it.
    #[cfg(unix)]
    fn wait_for_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the stand-in child never recorded its pid in {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether a process is still there, the way the shell asks: `kill -0` sends no signal, it
    /// only reports whether the pid could be signalled. A zombie answers it too, so a process that
    /// fails it has been reaped, not merely killed.
    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// A frame with no pixels is refused rather than written as a zero-length frame.
    ///
    /// A frame whose pixels stayed in VRAM has an empty `data` by design; writing it would hand
    /// ffmpeg a zero-length rawvideo frame that it reads as the next frame's leading bytes, a stream
    /// desync that appears seconds later pointing at nothing in this function.
    #[test]
    fn a_frame_without_pixels_is_refused_rather_than_written_as_nothing() {
        use crate::VideoCodec;
        use replayed_capture::{Frame, PixelFormat};

        let dir = tempfile::tempdir().unwrap();
        let cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder");

        let pixel_less = Frame {
            data: Vec::new(),
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
            texture: None,
        };
        let err = encoder
            .submit_video(pixel_less)
            .expect_err("a frame with no pixels must be refused");
        assert!(
            err.to_string().contains("not in CPU memory"),
            "the refusal must name the reason rather than surfacing later as corruption: {err}"
        );

        // A frame with pixels still goes through, so the guard refuses a shape, not work.
        let honest = Frame {
            data: vec![0u8; 64 * 64 * 4],
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
            texture: None,
        };
        encoder
            .submit_video(honest)
            .expect("a frame with pixels is accepted");
        encoder.finish().expect("flush the encoder");
    }

    /// A dead writer carries the child's own exit status and words.
    ///
    /// A Windows capture once reported only `video writer thread has stopped` while the real cause
    /// (an `h264_amf` that could not initialise) sat in ffmpeg's stderr. The stand-in child is a
    /// script that says why on stderr and exits non-zero, as a hardware encoder does on a machine
    /// with no vendor runtime; no GPU is needed.
    ///
    /// Unix-only: the stand-in is a `#!/bin/sh` script.
    #[cfg(unix)]
    #[test]
    fn a_dead_writer_reports_ffmpegs_exit_status_and_stderr() {
        use crate::VideoCodec;
        use replayed_capture::{Frame, PixelFormat};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho 'DLL amfrt64.dll failed to open' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };
        let cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            64,
            64,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        // Spawning succeeds: the child starts and then fails, like an encoder that cannot initialise.
        let mut encoder = FfmpegEncoder::spawn(&bin, &cfg).unwrap();

        // The channel only disconnects once the pump thread tries to write into the dead child, so
        // submit until that happens; each attempt is an independent frame.
        let frame = || Frame {
            data: vec![0u8; 64 * 64 * 4],
            pts: Duration::ZERO,
            width: 64,
            height: 64,
            format: PixelFormat::Bgra8,
            texture: None,
        };
        // Bounded by time, not by a count of attempts: on a loaded machine the pump thread can take
        // longer than a fixed handful of 10ms sleeps to notice the dead child.
        let give_up = Instant::now() + Duration::from_secs(20);
        let mut failure = None;
        while Instant::now() < give_up {
            match encoder.submit_video(frame()) {
                Ok(()) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }

        let err = failure.expect("a payload cannot be encoded once the writer thread is gone");
        assert!(
            err.contains("video writer thread has stopped"),
            "the symptom survives: {err}"
        );
        assert!(
            err.contains("DLL amfrt64.dll failed to open"),
            "ffmpeg's own words are the point, and they were only in its stderr: {err}"
        );
        assert!(
            err.contains("exit status"),
            "and so is its exit status: {err}"
        );
    }

    /// On macOS and other BSD-derived platforms the socket `accept()` returns inherits the
    /// listener's `O_NONBLOCK`, so a write that should wait for ffmpeg to drain the socket fails
    /// immediately with `WouldBlock` (`EAGAIN`, os error 35) once the buffers fill. It flakes
    /// ("the pump fails only when it gets ahead of ffmpeg"), so this test makes the state certain
    /// instead of hoping to hit the race.
    ///
    /// [`accept_within`] forces the accepted socket back to blocking mode, and the microphone input
    /// is a second copy of that accept path (`pump_audio` calls it and nothing else before writing).
    /// The client is a real ffmpeg on the encoder's `s16le` transport, paced with `-re` and given a
    /// small receive buffer, so the writer has to wait for it.
    ///
    /// The two behaviours are told apart by the socket's write timeout: a blocking socket honours it
    /// (waits, then reports `WouldBlock`), a non-blocking one ignores it and reports `WouldBlock` at
    /// once. So the assertion is that the large write did not fail at once.
    ///
    /// The client is ffmpeg rather than a `TcpStream::connect` because outbound sockets are
    /// enumerated by `crates/events/tests/no_egress.rs`, and ffmpeg dialling our listener is the
    /// production shape anyway.
    #[cfg(feature = "test-encoders")]
    #[test]
    fn a_large_block_written_to_the_microphone_input_waits_instead_of_failing_with_eagain() {
        /// How long a write may wait for the reader; only has to be well above the few milliseconds a
        /// non-blocking socket needs to give up.
        const WRITE_PATIENCE: Duration = Duration::from_millis(300);

        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let port = listener
            .local_addr()
            .expect("the listener's address")
            .port();

        let mut client = replayed_media::sidecar_command(&bin.ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                // Real-time pacing: the reader cannot drain a burst faster than the socket buffers allow.
                "-re",
                // A small receive buffer keeps the burst decisively larger than the socket's capacity.
                "-recv_buffer_size",
                "32768",
                "-f",
                "s16le",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-i",
                &format!("tcp://127.0.0.1:{port}"),
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("ffmpeg can act as the client of a loopback listener");

        // The microphone input's own accept, with the microphone's own label.
        let mut stream = accept_within(&listener, port, AUDIO_CONNECT_TIMEOUT, MICROPHONE_LABEL)
            .expect("ffmpeg dials the microphone listener");
        stream
            .set_write_timeout(Some(WRITE_PATIENCE))
            .expect("the accepted socket takes a write timeout");
        // Four megabytes (22 s of 48kHz stereo s16le) written right after connect, like a pump catching
        // up after a stall. ffmpeg takes ~1 MiB at once and then reads at the PCM's own rate, so the
        // write has to wait rather than being swallowed whole.
        let burst = vec![0u8; 4 * 1024 * 1024];
        let started = Instant::now();
        let outcome = stream.write_all(&burst);
        let waited = started.elapsed();

        let _ = client.kill();
        let _ = client.wait();

        match outcome {
            // A reader that swallowed the burst would make this run inconclusive, not wrong: a non-blocking
            // socket fails at once and cannot reach this arm.
            Ok(()) => eprintln!(
                "note: the client drained the whole {}-byte burst within {WRITE_PATIENCE:?}; \
                 the write never had to wait",
                burst.len()
            ),
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock,
                    "a full socket buffer is not a write failure: {e}"
                );
                assert!(
                    waited >= WRITE_PATIENCE - Duration::from_millis(50),
                    "the write gave up after {waited:?} instead of waiting out its \
                     {WRITE_PATIENCE:?} timeout: the accepted socket is NON-BLOCKING, i.e. it \
                     inherited O_NONBLOCK from the listener — docs/platform-traps.md, trap 1 \
                     (this is the macOS/BSD inheritance `set_nonblocking(false)` exists for)"
                );
            }
        }
    }

    /// The bitstream arguments follow the codec: ffmpeg is told what it is reading, to copy it, and
    /// given the container tag that makes the result openable.
    #[test]
    fn the_bitstream_arguments_follow_the_codec() {
        let h264 = video_input_args_bitstream(crate::VideoCodec::H264);
        assert!(h264.windows(2).any(|w| w == ["-f", "h264"]), "{h264:?}");
        let hevc = video_input_args_bitstream(crate::VideoCodec::Hevc);
        assert!(hevc.windows(2).any(|w| w == ["-f", "hevc"]), "{hevc:?}");

        // Both outputs are copies, whichever encoder ran.
        for args in [
            video_output_args_bitstream(crate::VideoCodec::H264),
            video_output_args_bitstream(crate::VideoCodec::Hevc),
        ] {
            assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]), "{args:?}");
        }

        // `hvc1` is the container's business and only HEVC needs it. `hev1` and `hvc1` describe the same
        // stream, but some players refuse `hev1`, and these clips get played on a Mac. H.264 must not
        // get it: `hvc1` on an AVC track misstates the codec.
        let hevc_out = video_output_args_bitstream(crate::VideoCodec::Hevc);
        assert!(
            hevc_out.windows(2).any(|w| w == ["-tag:v", "hvc1"]),
            "{hevc_out:?}"
        );
        assert_eq!(
            video_output_args_bitstream(crate::VideoCodec::H264),
            vec!["-c:v".to_string(), "copy".to_string()],
            "H.264's output arguments should be what they always were"
        );
    }

    /// The container tag, on the raw path too.
    ///
    /// HEVC takes the raw path where the hardware encoder refuses, and `ffprobe` on a real clip from
    /// it reported `codec_tag_string=hev1`; an argument-only test would not have caught that. The
    /// tag is a muxer option and belongs to both paths.
    #[test]
    fn the_raw_output_args_carry_the_hevc_tag_too() {
        let dir = std::env::temp_dir();
        let hevc = video_output_args(&EncodeConfig::for_tests_software(
            crate::VideoCodec::Hevc,
            64,
            48,
            30,
            dir.clone(),
            1_000,
        ));
        assert!(hevc.windows(2).any(|w| w == ["-tag:v", "hvc1"]), "{hevc:?}");

        let h264 = video_output_args(&EncodeConfig::for_tests_software(
            crate::VideoCodec::H264,
            64,
            48,
            30,
            dir,
            1_000,
        ));
        assert!(
            !h264.iter().any(|arg| arg == "-tag:v"),
            "an HEVC tag on an AVC track is a lie about the codec: {h264:?}"
        );
    }
}
