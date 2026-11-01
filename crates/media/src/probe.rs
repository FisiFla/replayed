//! `ffprobe` JSON into a typed `MediaInfo`.

use crate::binaries::{run_with_stdin, run_with_timeout, FfmpegBinaries};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bit_rate: Option<u64>,
    /// Per-stream duration (`stream.duration`), in ms. `None` when ffprobe reports none (some
    /// containers omit it).
    pub duration_ms: Option<u64>,
    /// Per-stream start time (`stream.start_time`), in ms; may be negative for a stream with an
    /// edit list. `None` when not reported.
    pub start_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStream {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Per-stream duration (`stream.duration`), in ms. `None` when absent.
    pub duration_ms: Option<u64>,
    /// Per-stream start time (`stream.start_time`), in ms. `None` when absent.
    pub start_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub video: Option<VideoStream>,
    pub audio: Option<AudioStream>,
}

/// One stream, reduced to the parameters a lossless concatenation depends on. Coarser than
/// [`VideoStream`]/[`AudioStream`]: not a description of a file but the shape two files must
/// share before `-f concat -c copy` can be trusted to treat them as one timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamShape {
    /// `codec_type` as ffprobe spells it: `video`, `audio`, `subtitle`, …
    pub kind: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u16,
}

impl StreamShape {
    /// One-line summary, for a refusal message a human has to read.
    pub fn summary(&self) -> String {
        match self.kind.as_str() {
            "video" => format!("video/{} {}x{}", self.codec, self.width, self.height),
            "audio" => {
                format!(
                    "audio/{} {}Hz {}ch",
                    self.codec, self.sample_rate, self.channels
                )
            }
            other => format!("{other}/{}", self.codec),
        }
    }
}

/// Every stream of a file, in muxer order.
///
/// [`MediaInfo`] holds one `video` and one `audio`, enough for A/V drift but not to see a
/// dropped track: a file that should carry 1 video + 2 audio and carries 1 + 1 looks fine to it.
/// Anything concatenating streams has to count them, which is what this type is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLayout {
    pub streams: Vec<StreamShape>,
}

impl StreamLayout {
    pub fn video_count(&self) -> usize {
        self.streams.iter().filter(|s| s.kind == "video").count()
    }

    pub fn audio_count(&self) -> usize {
        self.streams.iter().filter(|s| s.kind == "audio").count()
    }

    /// `1 video, 2 audio`: the shape in the words a refusal message wants.
    pub fn counts_summary(&self) -> String {
        let v = self.video_count();
        let a = self.audio_count();
        let other = self.streams.len() - v - a;
        let mut parts = vec![format!("{v} video"), format!("{a} audio")];
        if other > 0 {
            parts.push(format!("{other} other"));
        }
        parts.join(", ")
    }

    /// Parse the `streams` array of `ffprobe -show_streams -print_format json`.
    pub fn from_ffprobe_json(json: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct RawLayout {
            streams: Vec<RawStream>,
        }
        let raw: RawLayout = serde_json::from_str(json).context("parsing ffprobe json")?;
        Ok(Self {
            streams: raw
                .streams
                .iter()
                .map(|s| StreamShape {
                    kind: s.codec_type.clone(),
                    codec: s.codec_name.clone(),
                    width: s.width.unwrap_or(0),
                    height: s.height.unwrap_or(0),
                    sample_rate: s
                        .sample_rate
                        .as_deref()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    channels: s.channels.unwrap_or(0),
                })
                .collect(),
        })
    }
}

/// Probe the full stream layout of a file on disk.
pub fn stream_layout(bin: &FfmpegBinaries, path: &Path) -> Result<StreamLayout> {
    match probe_layout(bin, path)? {
        Ok(layout) => Ok(layout),
        Err(rejection) => bail!("{rejection}"),
    }
}

/// As [`stream_layout`], but tells "ffprobe ran and could not read this file" apart from "ffprobe
/// could not be run".
///
/// The outer `Err` is the second: ffprobe would not spawn or was killed at its deadline, which says
/// nothing about the file. The inner `Err` is the first, carrying ffprobe's own words: a file with
/// no `moov` atom (the newest segment of a session that was killed mid-write) or other bytes that
/// are not a media file. A caller deciding whether a file is *unreadable* must act on only the
/// inner one: dropping a good segment because a probe timed out would lose footage on a transient.
pub fn probe_layout(
    bin: &FfmpegBinaries,
    path: &Path,
) -> Result<std::result::Result<StreamLayout, String>> {
    let mut cmd = crate::sidecar_command(&bin.ffprobe);
    cmd.args(["-v", "error", "-print_format", "json", "-show_streams"])
        .arg(path);
    let out = run_with_timeout(cmd, PROBE_TIMEOUT)?;
    if !out.status.success() {
        return Ok(Err(format!(
            "ffprobe failed reading the stream layout of {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(Ok(StreamLayout::from_ffprobe_json(
        &String::from_utf8_lossy(&out.stdout),
    )
    .with_context(|| {
        format!("reading the stream layout of {}", path.display())
    })?))
}

/// A/V offset observed within one produced file: the audio timeline against the video timeline
/// in the muxed clip. Not a measurement of clock divergence between the live capture sources
/// (that would need instrumenting the capture path, QPC vs the audio device clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvDrift {
    /// Video stream duration, ms.
    pub video_ms: u64,
    /// Audio stream duration, ms.
    pub audio_ms: u64,
    /// Video stream start time when ffprobe reported one.
    pub video_start_ms: Option<i64>,
    /// Audio stream start time when ffprobe reported one.
    pub audio_start_ms: Option<i64>,
    /// `video_end - audio_end` (end = start + duration; start defaults to 0 when unreported).
    /// Negative when the audio runs longer than the video.
    pub delta_ms: i64,
}

#[derive(Deserialize)]
struct RawProbe {
    streams: Vec<RawStream>,
    format: RawFormat,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: String,
    codec_name: String,
    width: Option<u32>,
    height: Option<u32>,
    bit_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
    /// `stream.duration` as a decimal string of seconds, when present.
    duration: Option<String>,
    /// `stream.start_time` as a decimal string of seconds, when present.
    start_time: Option<String>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
    size: Option<String>,
}

impl MediaInfo {
    pub fn from_ffprobe_json(json: &str) -> Result<Self> {
        let raw: RawProbe = serde_json::from_str(json).context("parsing ffprobe json")?;
        if raw.streams.is_empty() {
            bail!("ffprobe reported no streams");
        }

        let video = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "video")
            .map(|s| VideoStream {
                codec: s.codec_name.clone(),
                width: s.width.unwrap_or(0),
                height: s.height.unwrap_or(0),
                bit_rate: s.bit_rate.as_deref().and_then(|v| v.parse().ok()),
                duration_ms: seconds_to_ms_opt(s.duration.as_deref()),
                start_ms: seconds_to_ms_i64_opt(s.start_time.as_deref()),
            });

        let audio = raw
            .streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .map(|s| AudioStream {
                codec: s.codec_name.clone(),
                sample_rate: s
                    .sample_rate
                    .as_deref()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                channels: s.channels.unwrap_or(0),
                duration_ms: seconds_to_ms_opt(s.duration.as_deref()),
                start_ms: seconds_to_ms_i64_opt(s.start_time.as_deref()),
            });

        Ok(Self {
            duration_ms: seconds_to_ms(raw.format.duration.as_deref()),
            size_bytes: raw
                .format
                .size
                .as_deref()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            video,
            audio,
        })
    }

    /// Run `ffprobe` against a file on disk.
    pub fn probe(bin: &FfmpegBinaries, path: &Path) -> Result<Self> {
        let mut cmd = crate::sidecar_command(&bin.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path);
        let out = run_with_timeout(cmd, PROBE_TIMEOUT)?;
        if !out.status.success() {
            bail!(
                "ffprobe failed on {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Self::from_ffprobe_json(&String::from_utf8_lossy(&out.stdout))
    }

    /// The A/V offset of this file: video end minus audio end, in ms.
    ///
    /// `None` unless both streams are present and ffprobe reported a duration for both; there is
    /// nothing honest to compute otherwise, and the caller should log what it has. End is
    /// `start_ms` (default `0`) plus `duration_ms`, so the result goes negative when the audio runs
    /// longer than the video. This measures drift within the produced clip, not the divergence
    /// between the live capture clocks.
    pub fn av_drift(&self) -> Option<AvDrift> {
        let video = self.video.as_ref()?;
        let audio = self.audio.as_ref()?;
        let video_dur = video.duration_ms?;
        let audio_dur = audio.duration_ms?;
        // The ends are saturating arithmetic on ffprobe's own numbers: a duration and a start
        // time both come out of the file, and a corrupt one must neither wrap into a negative
        // end nor overflow the sum into a panic.
        let video_end = video
            .start_ms
            .unwrap_or(0)
            .saturating_add(ms_i64(video_dur));
        let audio_end = audio
            .start_ms
            .unwrap_or(0)
            .saturating_add(ms_i64(audio_dur));
        Some(AvDrift {
            video_ms: video_dur,
            audio_ms: audio_dur,
            video_start_ms: video.start_ms,
            audio_start_ms: audio.start_ms,
            delta_ms: video_end.saturating_sub(audio_end),
        })
    }
}

/// A millisecond count as a signed number, clamped rather than wrapped.
///
/// ffprobe's durations are read out of a file, so an absurd one must not turn into a negative
/// number: `u64::MAX as i64` is `-1`, which would put a stream's end before its start.
fn ms_i64(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// Encode exactly one frame with `encoder` and throw the output away.
///
/// `Ok` means this machine can drive that encoder; `Err` carries ffmpeg's own words about why
/// not, the only actionable diagnosis (a missing `amfrt64.dll` names itself).
///
/// `ffmpeg -encoders` cannot answer this: it lists what ffmpeg was compiled with, not what its
/// runtime can initialise. On a Windows test machine with an NVIDIA GPU and no AMD hardware or
/// driver, ffmpeg advertised `h264_amf` and then died with `DLL amfrt64.dll failed to open` on
/// opening it. This smoke test is the difference between failing at startup and failing
/// mid-capture.
///
/// One 320x240 BGRA frame (307 200 bytes) goes in over `pipe:0` as rawvideo, as live capture
/// feeds the real encoder, and the output goes to the null muxer. There is no `-f lavfi` test
/// source: a stripped ffmpeg build may lack those filters, and the point is to exercise the
/// encoder, not a filter graph. A frame ffmpeg accepted proves the encoder opened.
///
/// Nothing here touches the screen or synthesises input.
pub fn smoke_test_encoder(bin: &FfmpegBinaries, encoder: &str) -> Result<(), String> {
    let size = format!("{}x{}", SMOKE_SIZE.0, SMOKE_SIZE.1);
    let mut cmd = crate::sidecar_command(&bin.ffmpeg);
    cmd.args([
        "-hide_banner",
        // Keeps ffmpeg from reading the probe's own stdin for interactive commands; the frame still
        // arrives on `pipe:0`.
        "-nostdin",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgra",
        "-s",
        size.as_str(),
        "-i",
        "pipe:0",
        "-frames:v",
        "1",
        "-c:v",
        encoder,
        "-f",
        "null",
        "-",
    ]);
    let frame = vec![0u8; (SMOKE_SIZE.0 * SMOKE_SIZE.1 * 4) as usize];
    let out = run_with_stdin(cmd, frame, SMOKE_TIMEOUT).map_err(|e| format!("{e:#}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(ffmpeg_reason(&String::from_utf8_lossy(&out.stderr)))
}

/// ffmpeg's explanation of a failed probe, condensed onto one line.
///
/// Shapes handled (measured on ffmpeg 9.0.2):
///
/// ```text
/// Input #0, rawvideo, from 'pipe:0':                     <- filtered: description
///   Stream #0:0: Video: rawvideo ..., bgra, 320x240       <- filtered: description
/// [h264_amf @ 0x55…] DLL amfrt64.dll failed to open      <- THE reason
/// [h264_amf @ 0x55…] Error initializing an external …    <- second reason
/// Error while opening encoder for output stream #0:0 …   <- ffmpeg's own summary
/// ```
///
/// Description lines are dropped. The first two reasons are kept because that is where the
/// missing DLL or driver is named, and the closing summary because it is ffmpeg's overall
/// statement; middle lines mostly restate the first two. The result is capped so it cannot
/// become a wall of text in a log or table.
///
/// Public because it is the only actionable diagnosis of a failing ffmpeg child, and the
/// encoder's probes (`replayed_encoder::probe`, `replayed_encoder::throughput`) must report
/// the same words rather than a paraphrase.
pub fn ffmpeg_reason(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_ffmpeg_chatter(l))
        .collect();
    let reason = match lines.as_slice() {
        [] => "ffmpeg said nothing on stderr".to_string(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} | {second}"),
        [first, second, .., last] => format!("{first} | {second} | {last}"),
    };
    if reason.chars().count() > REASON_CAP {
        let kept: String = reason.chars().take(REASON_CAP).collect();
        return format!("{kept}…");
    }
    reason
}

/// ffmpeg's non-diagnostic output: the description of the input and output it is about to
/// build, its stream mapping and the progress line. None says why an encoder failed, yet all
/// would be read as "the reason" because they come first.
///
/// `Stream #` and `Duration:` are matched after trimming, so ffmpeg's two-space indentation
/// inside a stream block does not defeat the match.
fn is_ffmpeg_chatter(line: &str) -> bool {
    const CHATTER: [&str; 9] = [
        "Input #",
        "Output #",
        "Stream mapping",
        "Metadata:",
        "Duration:",
        "Stream #",
        "Press [q]",
        "frame=",
        "size=",
    ];
    CHATTER.iter().any(|prefix| line.starts_with(prefix))
}

/// The frame a smoke test encodes: 320x240 BGRA, 307 200 bytes.
///
/// The size is not free to shrink. Hardware encoders have vendor minimum sizes, and a probe
/// tripping one reports a working encoder as unusable:
///
/// - NVENC rejects anything under 145x145 at the driver ("Frame Dimension less than the minimum
///   supported value"; ffmpeg trac #9251: 144x144 fails, 145x145 works).
/// - QSV has a minimum around 128x96.
/// - AMF documents 64x64 for H.264 but 192x128 for HEVC, and one probe size serves both codecs.
///
/// 320x240 clears all of them, is 16-pixel aligned both ways, and is real enough to be an
/// encode while cheap to feed.
const SMOKE_SIZE: (u32, u32) = (320, 240);

/// How long a smoke test may take before the encoder is declared unusable. An encoder that
/// cannot get the GPU runtime fails in milliseconds; this is a ceiling on a wedged child.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on the length of a returned reason, in characters.
const REASON_CAP: usize = 400;

/// ffprobe reports duration as a decimal string of seconds.
fn seconds_to_ms(seconds: Option<&str>) -> u64 {
    seconds_to_ms_opt(seconds).unwrap_or(0)
}

/// Like [`seconds_to_ms`], but keeps "ffprobe did not report this" as `None` instead of `0`.
fn seconds_to_ms_opt(seconds: Option<&str>) -> Option<u64> {
    seconds
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as u64)
}

/// Parse a decimal-seconds string (possibly negative, e.g. an edit-list start) into whole
/// milliseconds.
fn seconds_to_ms_i64_opt(seconds: Option<&str>) -> Option<i64> {
    seconds
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_combined_audio_video_probe() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":320,"height":240,"bit_rate":"500000","duration":"3.033333","start_time":"0.000000"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"duration":"3.050000","start_time":"0.021000"}
          ],
          "format": {"duration":"3.033333","size":"190000"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");

        assert_eq!(info.video.as_ref().map(|v| v.codec.as_str()), Some("h264"));
        assert_eq!(
            info.video.as_ref().map(|v| (v.width, v.height)),
            Some((320, 240))
        );
        assert_eq!(info.video.as_ref().and_then(|v| v.duration_ms), Some(3033));
        assert_eq!(info.video.as_ref().and_then(|v| v.start_ms), Some(0));
        assert_eq!(info.audio.as_ref().map(|a| a.codec.as_str()), Some("aac"));
        assert_eq!(info.audio.as_ref().map(|a| a.channels), Some(2));
        assert_eq!(info.audio.as_ref().and_then(|a| a.duration_ms), Some(3050));
        assert_eq!(info.audio.as_ref().and_then(|a| a.start_ms), Some(21));
        assert_eq!(info.duration_ms, 3033);
        assert_eq!(info.size_bytes, 190000);
    }

    #[test]
    fn rejects_a_probe_with_no_streams() {
        let json = r#"{"streams":[],"format":{"duration":"0.0","size":"0"}}"#;
        assert!(MediaInfo::from_ffprobe_json(json).is_err());
    }

    /// A missing per-stream duration stays `None` rather than becoming 0, so the drift computation
    /// can refuse to invent a number.
    #[test]
    fn missing_stream_duration_stays_none() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":320,"height":240},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
          ],
          "format": {"duration":"3.0","size":"100"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");
        assert_eq!(info.video.as_ref().and_then(|v| v.duration_ms), None);
        assert_eq!(info.audio.as_ref().and_then(|a| a.duration_ms), None);
    }

    fn stream_durations(video_ms: u64, audio_ms: u64) -> (VideoStream, AudioStream) {
        let video = VideoStream {
            codec: "h264".into(),
            width: 64,
            height: 48,
            bit_rate: None,
            duration_ms: Some(video_ms),
            start_ms: None,
        };
        let audio = AudioStream {
            codec: "aac".into(),
            sample_rate: 48_000,
            channels: 2,
            duration_ms: Some(audio_ms),
            start_ms: None,
        };
        (video, audio)
    }

    #[test]
    fn av_drift_is_zero_when_the_streams_are_equal() {
        let (video, audio) = stream_durations(4_000, 4_000);
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        let drift = info.av_drift().expect("both durations present");
        assert_eq!(drift.delta_ms, 0);
        assert_eq!((drift.video_ms, drift.audio_ms), (4_000, 4_000));
    }

    #[test]
    fn av_drift_positive_when_video_outlasts_audio() {
        let (video, audio) = stream_durations(4_000, 3_900);
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        assert_eq!(info.av_drift().unwrap().delta_ms, 100);
    }

    /// The negative case: audio longer than video makes the delta negative.
    #[test]
    fn av_drift_negative_when_audio_outlasts_video() {
        let (video, audio) = stream_durations(3_900, 4_000);
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        assert_eq!(info.av_drift().unwrap().delta_ms, -100);
    }

    /// Reported start times shift the end timestamps: a video starting 50 ms late with the same
    /// duration ends 50 ms after the audio.
    #[test]
    fn av_drift_accounts_for_stream_start_times() {
        let video = VideoStream {
            codec: "h264".into(),
            width: 64,
            height: 48,
            bit_rate: None,
            duration_ms: Some(4_000),
            start_ms: Some(50),
        };
        let audio = AudioStream {
            codec: "aac".into(),
            sample_rate: 48_000,
            channels: 2,
            duration_ms: Some(4_000),
            start_ms: Some(0),
        };
        let info = MediaInfo {
            duration_ms: 4_000,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        let drift = info.av_drift().unwrap();
        assert_eq!(drift.delta_ms, 50);
        assert_eq!(drift.video_start_ms, Some(50));
        assert_eq!(drift.audio_start_ms, Some(0));
    }

    #[test]
    fn av_drift_is_none_without_both_streams() {
        let json = r#"{
          "streams": [{"codec_type":"video","codec_name":"h264","width":320,"height":240,"duration":"3.0"}],
          "format": {"duration":"3.0","size":"100"}
        }"#;
        let info = MediaInfo::from_ffprobe_json(json).expect("valid probe json");
        assert!(info.av_drift().is_none(), "no audio stream means no drift");
    }

    /// A duration no file could hold does not make the drift arithmetic wrap.
    ///
    /// The durations come out of the file, so an absurd one is possible, and `u64::MAX as i64` is
    /// `-1`: the end of a stream would land before its start, and the sum of two such ends could
    /// overflow into a panic. The ends are clamped and the difference saturates instead.
    #[test]
    fn an_absurd_duration_does_not_wrap_the_drift() {
        let (video, audio) = stream_durations(u64::MAX, 0);
        let info = MediaInfo {
            duration_ms: u64::MAX,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        let drift = info.av_drift().expect("both durations present");
        assert_eq!(
            drift.delta_ms,
            i64::MAX,
            "the video's end is clamped, and the difference is not a wrapped negative"
        );

        let (video, audio) = stream_durations(0, u64::MAX);
        let info = MediaInfo {
            duration_ms: u64::MAX,
            size_bytes: 0,
            video: Some(video),
            audio: Some(audio),
        };
        let drift = info.av_drift().expect("both durations present");
        assert_eq!(
            drift.delta_ms,
            -i64::MAX,
            "and the other way round the difference saturates too"
        );
    }

    /// The probe size is a hardware constraint, not a preference (see [`SMOKE_SIZE`]). This guards
    /// against "shrink it, it is only a smoke test": NVENC fails under 145x145 at the driver, QSV
    /// under roughly 128x96, AMF's HEVC under 192x128, and a probe below any reports working
    /// hardware as unusable.
    #[test]
    fn the_smoke_frame_clears_every_vendors_minimum_size() {
        assert!(
            SMOKE_SIZE.0 >= 192 && SMOKE_SIZE.1 >= 145,
            "the smoke frame {}x{} is under a vendor's driver minimum; see the SMOKE_SIZE note",
            SMOKE_SIZE.0,
            SMOKE_SIZE.1
        );
        // 16-pixel alignment, which every one of these encoders also expects.
        assert_eq!(
            (SMOKE_SIZE.0 % 16, SMOKE_SIZE.1 % 16),
            (0, 0),
            "the smoke frame is not 16-pixel aligned"
        );
    }

    /// The real machinery end to end: a real child, a real frame over `pipe:0`, a real exit status
    /// and ffmpeg's real words. `libx264` is already required by this suite's software path
    /// (`crates/encoder/tests/segmenting.rs`), so it adds no new expectation of the test host. A
    /// hardware vendor's smoke test differs only in the exit status of this same child.
    #[test]
    fn a_working_encoder_encodes_one_frame() {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        smoke_test_encoder(&bin, "libx264").expect("libx264 can encode a 320x240 frame");
    }

    /// The other half: an encoder ffmpeg cannot open must fail with ffmpeg's reason, not "it did
    /// not work". On Windows the equivalent is `DLL amfrt64.dll failed to open`, only ever in the
    /// child's stderr.
    #[test]
    fn an_encoder_that_cannot_open_reports_ffmpegs_own_reason() {
        let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        let reason = smoke_test_encoder(&bin, "definitely_not_an_encoder")
            .expect_err("this encoder does not exist");
        assert!(
            reason.contains("definitely_not_an_encoder"),
            "ffmpeg's stderr names what it could not open: {reason}"
        );
        assert!(
            !reason.contains("Input #") && !reason.contains("Stream #"),
            "the input description is not the reason and must not be reported as one: {reason}"
        );
        assert!(!reason.contains('\n'), "a reason is one line: {reason}");
    }

    /// A hanging encoder must be bounded, not waited out. The stand-in never reads or exits; the
    /// probe must give up on the deadline instead of wedging startup. The frame is far larger than
    /// any pipe buffer, so the writer thread would block forever if the timeout were not real.
    #[cfg(unix)]
    #[test]
    fn an_encoder_that_hangs_is_bounded_by_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        // `sleep`ing longer than the probe's own deadline, with the frame arriving on a pipe nobody
        // reads.
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffmpeg");
        std::fs::write(&stub, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = FfmpegBinaries {
            ffmpeg: stub.clone(),
            ffprobe: stub,
        };

        let started = std::time::Instant::now();
        let err = smoke_test_encoder(&bin, "h264_amf").expect_err("a stalled child is not a pass");
        assert!(err.contains("timed out"), "the deadline is named: {err}");
        assert!(
            started.elapsed() < SMOKE_TIMEOUT + Duration::from_secs(5),
            "the probe waited {:?}, which is not bounded by {SMOKE_TIMEOUT:?}",
            started.elapsed()
        );
    }
}
