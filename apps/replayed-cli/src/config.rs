//! Root configuration: TOML in the app data directory, never environment variables.
//!
//! The `[buffer]`, `[encode]` and `[storage]` sections are the recorder's own types
//! (`replayed_recorder::config`), shared with the desktop shell. What stays here is what a
//! headless front-end owns: the hotkey and the game-event sources.

use anyhow::{bail, Context, Result};
use replayed_events::process::GamesSection;
use replayed_recorder::config::{
    BufferSection, CaptureSection, EncodeSection, MicSection, RecorderSection, StorageSection,
};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub recorder: RecorderSection,
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    #[serde(default)]
    pub capture: CaptureSection,
    pub audio: AudioSection,
    pub storage: StorageSection,
    #[serde(default)]
    pub mic: MicSection,
    pub hotkeys: HotkeySection,
    pub events: EventsSection,
    /// `[games]`, the game watcher. Defaulted, so a file that does not mention it means "watch
    /// nothing": `auto_record` is `false` in `GamesSection::default`, and the recorder then starts
    /// no watcher.
    #[serde(default)]
    pub games: GamesSection,
}

/// `[audio]`, the game-audio track.
///
/// `enabled = false` is a video-only recording: no audio input, no loopback listener and no
/// audio stream in the container.
///
/// `source` and `codec` have one implemented value each (`"loopback"`, `"aac"`); anything else
/// is refused in [`Config::validate`]. `bitrate_kbps` reaches ffmpeg's `-b:a` and is unused
/// when `enabled` is false.
///
/// One combination is refused, by the recorder rather than here: `enabled = false` with a
/// microphone (`[mic] enabled = true`). Audio tracks are named by position
/// (`replayed_media::edit::audio_titles` calls the first `Game Audio`), so a one-track
/// recording of a voice would be labelled as the game audio.
#[derive(Debug, Deserialize)]
pub struct AudioSection {
    pub enabled: bool,
    pub source: String,
    pub codec: String,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Deserialize)]
pub struct HotkeySection {
    pub clip: String,
    /// Desktop-app chords, accepted here so one config file serves both front-ends; the CLI
    /// itself only listens for `clip`.
    #[serde(default)]
    pub toggle_recording: Option<String>,
    #[serde(default)]
    pub bookmark: Option<String>,
}

pub use replayed_recorder::event_driver::EventsSection;

impl Config {
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Self =
            replayed_recorder::config_schema::parse(text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &PathBuf) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&text)
    }

    /// Every rule the CLI enforces on a parsed file, in two groups.
    ///
    /// **Engine rules** protect the recording engine, not this front-end, so they live in
    /// `replayed-recorder` ([`replayed_recorder::config::validate`], the function behind
    /// [`replayed_recorder::RecorderConfig::validate`]) where every front-end is held to them. This
    /// calls the library on the parsed `buffer`, `encode` and `storage` sections rather than on a
    /// `RecorderConfig`, because a `RecorderConfig` needs the ffmpeg binaries and the data directory
    /// and a config file that is refused should be refused at load, before either is looked for;
    /// `Recorder::start` runs the same check again on what it is handed. The rules:
    ///
    /// | rule | refused |
    /// |---|---|
    /// | `encode.vendor` | `"software"` (no CPU fallback by design), and anything not `auto`/`nvenc`/`qsv`/`amf` |
    /// | `buffer.segment_time` | `0` (an all-intra stream, a segment file per frame) |
    /// | `buffer.post_seconds` | `0` (a clip with no post-roll to wait for) |
    /// | `encode.fps` | `0` (`1 / fps` is meaningless, and ffmpeg's `-framerate 0` is an error) |
    /// | `buffer.ram_cap_bytes` | outside 8 MiB..=64 GiB |
    /// | `storage.*` retention | a zero cap or a zero age, for the clips and the sessions |
    ///
    /// **CLI rules** are about what only this front-end parses or runs, and stay here
    /// ([`Config::validate_cli_rules`]):
    ///
    /// | rule | refused |
    /// |---|---|
    /// | `events.gsi_port` | anything but `0` (listener off) or `1024..=65535` |
    /// | `audio.codec` | anything but `"aac"` (the `[audio]` section's only parser is the CLI) |
    /// | `audio.source` | anything but `"loopback"` |
    ///
    /// Checked elsewhere in the CLI's startup, not in this function: the hotkey chord
    /// (`Hotkey::parse`, in `replayed-events`), `[events] mute_clips` (`Policy::new`), a
    /// schema version from the future (`config_schema::parse`), and microphone-on with game
    /// audio off (refused by the recorder itself).
    fn validate(&self) -> Result<()> {
        replayed_recorder::config::validate(&self.buffer, &self.encode, &self.storage)?;
        self.validate_cli_rules()
    }

    /// The rules about what only this front-end parses or runs; see [`Config::validate`].
    fn validate_cli_rules(&self) -> Result<()> {
        // 0 disables the listener (documented in `config.example.toml`). Anything else is a real port,
        // and the default sits above the ephemeral range so a temporarily bound listener elsewhere
        // cannot shadow it at startup.
        if self.events.gsi_port != 0 && !(1024..=65535).contains(&self.events.gsi_port) {
            bail!(
                "events.gsi_port must be 0 (disabled) or between 1024 and 65535, got {}",
                self.events.gsi_port
            );
        }
        // The pipeline records aac over the system loopback and nothing else (ffmpeg input 1 is a
        // loopback PCM stream, the audio codec fixed to aac in `replayed_encoder::ffmpeg`). A config
        // asking for anything else would be recorded as aac-over-loopback while claiming otherwise, so
        // it is refused, naming what is implemented.
        if self.audio.codec != "aac" {
            bail!(
                "audio.codec = {:?} is not implemented: this build records the game audio as \
                 aac only",
                self.audio.codec
            );
        }
        if self.audio.source != "loopback" {
            bail!(
                "audio.source = {:?} is not implemented: this build captures the system \
                 loopback only",
                self.audio.source
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_schema_version_is_refused_and_an_absent_one_is_accepted() {
        let example = include_str!("../../../config.example.toml");
        let newer = example.replace("schema_version = 1", "schema_version = 2");
        assert_ne!(newer, example);
        let err = Config::from_toml(&newer).expect_err("a layout from the future must not load");
        assert!(format!("{err:#}").contains("schema version 2"), "{err:#}");

        let legacy = example.replace("schema_version = 1\n", "");
        assert!(
            Config::from_toml(&legacy).is_ok(),
            "files from before the field are version 1"
        );
    }

    #[test]
    fn parses_the_example_config() {
        let text = include_str!("../../../config.example.toml");
        let cfg = Config::from_toml(text).expect("config.example.toml must stay valid");
        assert_eq!(cfg.buffer.pre_seconds, 30);
        assert_eq!(cfg.buffer.post_seconds, 5);
        assert!(cfg.audio.enabled);
        assert_eq!(
            cfg.audio.source, "loopback",
            "the example names the implemented source"
        );
        assert_eq!(cfg.audio.codec, "aac", "and the implemented codec");
        assert_eq!(
            cfg.audio.bitrate_kbps, 192,
            "and the shipped default bitrate"
        );
        assert_eq!(cfg.hotkeys.clip, "Ctrl+F8");
        // The sections a fresh install runs with: the replay buffer, no microphone, and nothing
        // watched, which is the safety posture.
        assert_eq!(
            cfg.recorder.mode,
            replayed_recorder::RecordingMode::ReplayBuffer
        );
        assert!(
            !cfg.mic.enabled,
            "the example must not turn the microphone on"
        );
        assert!(!cfg.games.auto_record, "the example must not auto-record");
        assert!(
            cfg.storage.sessions.max_total_bytes > 0,
            "the session rules have defaults"
        );
    }

    /// A `config.toml` without the optional newer sections parses and means buffer mode, no
    /// microphone, nothing watched.
    #[test]
    fn a_pre_phase_5_config_still_parses_and_means_what_it_did() {
        // The oldest surface, verbatim: the three recording sections, the CLI's own `[hotkeys]` and
        // `[events]`, and nothing else.
        let text = "\
[buffer]
pre_seconds = 30
post_seconds = 5
segment_time = 1
scratch_dir = \"\"

[encode]
vendor = \"auto\"
codec = \"h264\"
bitrate_kbps = 20000
fps = 60
adapt_fps = true
output_size = \"\"

[audio]
enabled = true
source = \"loopback\"
codec = \"aac\"
bitrate_kbps = 192

[storage]
clips_dir = \"\"
max_total_bytes = 53687091200
max_age_days = 7

[hotkeys]
clip = \"Ctrl+F8\"

[events]
lol_poll_enabled = true
gsi_port = 45671
";
        let cfg = Config::from_toml(text).expect("a pre-Phase-5 file must keep parsing");
        assert_eq!(
            cfg.recorder.mode,
            replayed_recorder::RecordingMode::ReplayBuffer
        );
        assert!(!cfg.mic.enabled, "no [mic] means no microphone");
        assert!(
            !cfg.games.auto_record,
            "no [games] means nothing is watched"
        );
        assert_eq!(
            cfg.games.watch,
            replayed_events::process::default_watch(),
            "with the default watch list, unused while auto_record is off"
        );
        assert_eq!(
            cfg.storage.sessions,
            replayed_recorder::config::SessionStorageRules::default(),
            "and the session rules are the documented default"
        );
    }

    /// The mode is settable from the file, which is what makes the CLI's `--mode` an override
    /// rather than the only way in.
    #[test]
    fn the_mode_can_be_set_from_the_file() {
        let text = include_str!("../../../config.example.toml")
            .replace("mode = \"buffer\"", "mode = \"session\"");
        let cfg = Config::from_toml(&text).expect("the session mode parses");
        assert_eq!(
            cfg.recorder.mode,
            replayed_recorder::RecordingMode::FullSession
        );
    }

    /// The microphone is opt-in, and `[mic] enabled = true` is the whole opt-in.
    #[test]
    fn the_microphone_is_opt_in_through_the_file() {
        let text = include_str!("../../../config.example.toml")
            .replace("[mic]\nenabled = false", "[mic]\nenabled = true");
        assert!(
            text.contains("[mic]\nenabled = true"),
            "the substitution must apply"
        );
        let cfg = Config::from_toml(&text).expect("mic.enabled = true parses");
        assert!(cfg.mic.enabled);
    }

    #[test]
    fn rejects_a_software_encoder_request() {
        let text = include_str!("../../../config.example.toml")
            .replace("vendor = \"auto\"", "vendor = \"software\"");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("software"),
            "must explain that CPU encoding is not available: {err}"
        );
    }

    /// `audio.codec` and `audio.source` have one implemented value each, and anything else must
    /// fail rather than be recorded as the aac-over-loopback the pipeline runs. The messages name
    /// what is implemented.
    #[test]
    fn rejects_an_audio_codec_that_is_not_aac() {
        let text = include_str!("../../../config.example.toml")
            .replace("codec = \"aac\"", "codec = \"opus\"");
        assert!(
            text.contains("codec = \"opus\""),
            "the substitution must apply"
        );
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("audio.codec") && err.to_string().contains("aac"),
            "the refusal must name the key and the implemented codec: {err}"
        );
    }

    #[test]
    fn rejects_an_audio_source_that_is_not_loopback() {
        let text = include_str!("../../../config.example.toml")
            .replace("source = \"loopback\"", "source = \"microphone\"");
        assert!(
            text.contains("source = \"microphone\""),
            "the substitution must apply"
        );
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("audio.source") && err.to_string().contains("loopback"),
            "the refusal must name the key and the implemented source: {err}"
        );
    }

    #[test]
    fn rejects_a_gsi_port_above_the_ephemeral_range_start() {
        let text = include_str!("../../../config.example.toml")
            .replace("gsi_port = 45671", "gsi_port = 80");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("gsi_port"), "got: {err}");
    }

    #[test]
    fn a_zero_gsi_port_means_the_listener_is_off() {
        // The documented way to disable an integration that would otherwise want a port.
        let text = include_str!("../../../config.example.toml")
            .replace("gsi_port = 45671", "gsi_port = 0");
        let cfg = Config::from_toml(&text).expect("0 is a valid setting");
        assert_eq!(cfg.events.gsi_port, 0);
    }

    /// A zero segment is an all-intra stream and a segment file per frame; the desktop shell, which
    /// reads the same file, does not refuse it (see `Config::validate`).
    #[test]
    fn rejects_a_zero_segment_time() {
        let text = include_str!("../../../config.example.toml")
            .replace("segment_time = 1", "segment_time = 0");
        assert!(
            text.contains("segment_time = 0"),
            "the substitution must apply"
        );
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("buffer.segment_time"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_zero_post_seconds() {
        let text = include_str!("../../../config.example.toml")
            .replace("post_seconds = 5", "post_seconds = 0");
        assert!(
            text.contains("post_seconds = 0"),
            "the substitution must apply"
        );
        let err = Config::from_toml(&text).unwrap_err();
        assert!(
            err.to_string().contains("buffer.post_seconds"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_an_unknown_vendor() {
        let text = include_str!("../../../config.example.toml")
            .replace("vendor = \"auto\"", "vendor = \"vulkan\"");
        assert!(
            text.contains("vendor = \"vulkan\""),
            "the substitution must apply"
        );
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("encode.vendor"), "got: {err}");
    }

    /// The retention and RAM-cap rules were never the CLI's: they arrive with the library's check, so
    /// a CLI-loaded file is held to what the desktop and `Recorder::start` hold it to.
    #[test]
    fn the_library_rules_beyond_the_old_four_apply_to_a_cli_file() {
        let example = include_str!("../../../config.example.toml");
        for (from, to, key) in [
            (
                "max_age_days = 7",
                "max_age_days = 0",
                "storage.max_age_days",
            ),
            (
                "max_total_bytes = 53687091200",
                "max_total_bytes = 0",
                "storage.max_total_bytes",
            ),
            (
                "ram_cap_bytes = 268435456",
                "ram_cap_bytes = 1024",
                "buffer.ram_cap_bytes",
            ),
        ] {
            let text = example.replacen(from, to, 1);
            assert_ne!(text, example, "the substitution must apply: {from}");
            let err = Config::from_toml(&text).unwrap_err();
            assert!(format!("{err:#}").contains(key), "{key}: {err:#}");
        }
    }

    /// The frame rate is what the pipeline declares and paces to, so zero must fail with a sentence
    /// rather than be silently clamped to 1 by the engine.
    #[test]
    fn rejects_a_zero_frame_rate() {
        let text = include_str!("../../../config.example.toml").replace("fps = 60", "fps = 0");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("encode.fps"), "got: {err}");
    }

    /// The example enables adaptation, so a fresh install measures what the machine can hold; a
    /// file without the key gets it too, and the opt-out is honoured. Parsed through the CLI's own
    /// reader, which is what runs.
    #[test]
    fn the_example_enables_adaptation_and_the_key_can_turn_it_off() {
        let example = Config::from_toml(include_str!("../../../config.example.toml"))
            .expect("the example is valid");
        assert!(
            example.encode.adapt_fps,
            "config.example.toml must enable adaptation"
        );

        let text = include_str!("../../../config.example.toml")
            .replace("adapt_fps = true", "adapt_fps = false");
        let off = Config::from_toml(&text).expect("false is a valid setting");
        assert!(!off.encode.adapt_fps);

        // A file that never mentions the key still adapts: the default lives in the shared section
        // type, which this reader deserialises.
        let text = include_str!("../../../config.example.toml")
            .lines()
            .filter(|l| !l.trim_start().starts_with("adapt_fps"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("fps = 60")
                && !text
                    .lines()
                    .any(|l| l.trim_start().starts_with("adapt_fps =")),
            "the setting must be gone from the text being parsed (a comment mentioning the \
             key is fine — the point is that no value is set)"
        );
        let default = Config::from_toml(&text).expect("the key is optional");
        assert!(
            default.encode.adapt_fps,
            "the default is measured behaviour, not silence"
        );
    }

    #[test]
    fn the_capture_sections_are_the_recorders_own_types() {
        // The CLI does not re-declare `[buffer]`, `[encode]` or `[storage]`: the desktop shell parses
        // the same sections into the same types, so one config file means one thing to both.
        let cfg = Config::from_toml(include_str!("../../../config.example.toml")).unwrap();
        let buffer: replayed_recorder::BufferSection = cfg.buffer;
        let encode: replayed_recorder::EncodeSection = cfg.encode;
        let storage: replayed_recorder::StorageSection = cfg.storage;
        assert_eq!(buffer.segment_time, 1);
        assert_eq!(encode.codec, "h264");
        assert_eq!(storage.max_age_days, 7);
        assert!(
            storage.sessions.max_age_days > 0,
            "the sessions rules are part of the same section"
        );
    }
}
