//! The recorder's `config.toml` sections: `[recorder]`, `[mic]`, `[buffer]`, `[capture]`,
//! `[encode]`, `[storage]`.
//!
//! They live with the engine they configure; the CLI keeps `[hotkeys]` and `[events]` and the
//! desktop shell its own `[storage]` view. Names, types and defaults follow `config.example.toml`.
//! Paths are `String` because an empty one means "the default under the application data
//! directory", which [`crate::RecorderConfig`] applies when resolving them.

use anyhow::Result;
use serde::Deserialize;

/// `[recorder]`: which recording this run is.
///
/// Its own section because the mode is what the engine is asked to do, not a property of the ring
/// or the storage policy: the RAM cap applies to one mode only, and only one produces a session file.
///
/// ```toml
/// [recorder]
/// mode = "buffer"   # or "session"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct RecorderSection {
    /// [`RecordingMode::ReplayBuffer`] (the default) or [`RecordingMode::FullSession`].
    pub mode: RecordingMode,
    /// In [`RecordingMode::ReplayBuffer`], also write the whole recording to a session file, off the
    /// same encoder (`false` by default). Ignored in full-session mode, which records a session already.
    pub save_session: bool,
}

/// What the engine records.
///
/// * [`RecordingMode::ReplayBuffer`]: a bounded RAM ring of the last few minutes; a hotkey or game
///   event splices a clip out of it, older footage is dropped at `buffer.ram_cap_bytes`, and
///   nothing is written until a clip is asked for.
/// * [`RecordingMode::FullSession`]: segments go into a per-session directory that the RAM cap
///   does not apply to (evicting the oldest segment of a long recording would delete footage the
///   user asked for); stopping losslessly concatenates them into one `session-<timestamp>.mp4`.
///
/// The strings are those `sessions.mode` stores ([`RecordingMode::store_mode`]); a test pins the
/// spellings together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum RecordingMode {
    /// The rolling replay buffer (the default).
    #[default]
    #[serde(rename = "buffer", alias = "replay_buffer", alias = "replaybuffer")]
    ReplayBuffer,
    /// Record the whole session and concatenate it at stop.
    #[serde(rename = "session", alias = "full_session", alias = "fullsession")]
    FullSession,
}

impl RecordingMode {
    /// The `config.toml` spelling (`[recorder] mode`), also the string [`RecordingMode::store_mode`] maps onto.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingMode::ReplayBuffer => "buffer",
            RecordingMode::FullSession => "session",
        }
    }

    /// The `sessions.mode` value this mode opens its row with; the store owns the vocabulary.
    pub fn store_mode(self) -> &'static str {
        match self {
            RecordingMode::ReplayBuffer => replayed_store::SESSION_MODE_BUFFER,
            RecordingMode::FullSession => replayed_store::SESSION_MODE_SESSION,
        }
    }

    /// Whether this mode writes a session file and keeps its segments free of the RAM cap.
    pub fn is_full_session(self) -> bool {
        matches!(self, RecordingMode::FullSession)
    }
}

impl std::fmt::Display for RecordingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RecordingMode {
    type Err = anyhow::Error;

    /// The CLI's `--mode` parsing, and the one place the accepted spellings are listed.
    fn from_str(spec: &str) -> anyhow::Result<Self> {
        match spec.trim().to_ascii_lowercase().as_str() {
            "buffer" | "replay" | "replay_buffer" => Ok(RecordingMode::ReplayBuffer),
            "session" | "full" | "full_session" => Ok(RecordingMode::FullSession),
            other => anyhow::bail!(
                "unknown recording mode {other:?}: expected \"buffer\" (rolling replay \
                 buffer) or \"session\" (record the whole session)"
            ),
        }
    }
}

/// `[mic]`: the optional microphone track, off by default.
///
/// Opt-in because a microphone is not the game's audio, a machine may have no capture endpoint, and
/// a silently added voice track is a privacy surprise. When on, the engine sets `mic_audio` on the
/// encode config (a second audio input, two output tracks) and feeds it from the microphone backend.
/// If it cannot (no endpoint, or no backend on this platform) the recording fails at start rather
/// than carrying a silent track (see `Recorder::start`).
///
/// Which device is recorded is `device`: a WASAPI endpoint id from the settings picker, or empty
/// for the default communications capture endpoint (the device the user talks into). A chosen
/// device that is missing at start refuses the recording rather than recording another one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct MicSection {
    /// Record a microphone track alongside the game audio. Default `false`.
    pub enabled: bool,
    /// The capture endpoint id to record; empty is the default communications microphone.
    pub device: String,
}

/// `[buffer]`: the ring's window, segment length and budgets.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BufferSection {
    /// Media time kept before a trigger.
    #[serde(deserialize_with = "clamp_seconds")]
    pub pre_seconds: u64,
    /// Media time kept after a trigger.
    #[serde(deserialize_with = "clamp_seconds")]
    pub post_seconds: u64,
    /// Length of one scratch segment, also the keyframe interval, so a clip is a lossless
    /// concatenation of whole segments.
    #[serde(deserialize_with = "clamp_seconds")]
    pub segment_time: u64,
    /// Empty means `<app data dir>/scratch`.
    pub scratch_dir: String,
    /// Bytes of RAM the in-memory replay ring may occupy. Default 256 MiB.
    ///
    /// Unclipped footage lives in RAM (see `replayed_replay::MemoryRingBuffer`), so this keeps the OS
    /// from killing the application. It is a fraction of a typical machine because the encoder and
    /// capture backends need room too. Defaulted so an older `config.toml` still parses.
    ///
    /// Clamped when it is parsed ([`MAX_RAM_CAP_BYTES`]): this is the only bound on the ring, and
    /// a bound that a hand-edited file can raise past the machine's memory is not a bound.
    #[serde(
        default = "default_ram_cap_bytes",
        deserialize_with = "clamp_ram_cap_bytes"
    )]
    pub ram_cap_bytes: u64,
    /// The longest pre-roll a single clip may ask for, in seconds; `0` means `pre_seconds`. It sizes
    /// the ring so a long-clip hotkey can reach further back. Larger requests are clamped, not errors.
    #[serde(default, deserialize_with = "clamp_seconds")]
    pub max_pre_seconds: u64,
    /// The longest post-roll a single clip may ask for, in seconds; `0` means `post_seconds`.
    #[serde(default, deserialize_with = "clamp_seconds")]
    pub max_post_seconds: u64,
}

impl BufferSection {
    /// The pre-roll ceiling in milliseconds: the larger of `pre_seconds` and `max_pre_seconds`.
    ///
    /// Saturating: the seconds come from a file, and a hand-edited one must not wrap the
    /// millisecond arithmetic into the past.
    pub fn max_pre_ms(&self) -> u64 {
        self.pre_seconds
            .max(self.max_pre_seconds)
            .saturating_mul(1000)
    }

    /// The post-roll ceiling in milliseconds: the larger of `post_seconds` and `max_post_seconds`.
    ///
    /// Saturating, like [`BufferSection::max_pre_ms`].
    pub fn max_post_ms(&self) -> u64 {
        self.post_seconds
            .max(self.max_post_seconds)
            .saturating_mul(1000)
    }
}

/// The longest window the configuration may ask for, in seconds.
///
/// A day. The number is a *request*: what can actually be held is bounded by the ring's caps
/// (the RAM budget, the pre-roll window), so a larger one changes nothing but the arithmetic. The
/// clamp is here so a hand-edited file cannot overflow the millisecond arithmetic downstream.
pub const MAX_WINDOW_SECONDS: u64 = 86_400;

/// The largest RAM budget the replay buffer may be given, in bytes.
///
/// The RAM cap is the only bound on the ring: the footage it holds is unclipped and in memory,
/// so a cap that is not bounded is a cap that is not a cap. 64 GiB is far more than any
/// machine's worth of footage — at 40 Mbit/s it is more than a day of recording — and far
/// less than the machine has, so a ring that reaches the cap evicts rather than starving the
/// operating system.
pub const MAX_RAM_CAP_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// A window length from the configuration, clamped to [`MAX_WINDOW_SECONDS`].
///
/// The clamp is arithmetic on a number that has already been parsed, and the point is to bound
/// it before the millisecond arithmetic downstream sees it.
fn clamp_seconds<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let seconds = u64::deserialize(deserializer)?;
    Ok(seconds.min(MAX_WINDOW_SECONDS))
}

/// A RAM budget from the configuration, clamped to [`MAX_RAM_CAP_BYTES`].
///
/// Clamped when it is parsed, for the same reason the window lengths are: the number is a
/// memory budget, and a hand-edited file must not be able to ask for more memory than exists.
/// The arithmetic downstream is saturating, so the budget itself has to be a number that can be
/// carried.
fn clamp_ram_cap_bytes<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let bytes = u64::deserialize(deserializer)?;
    Ok(bytes.min(MAX_RAM_CAP_BYTES))
}

/// 256 MiB. See [`BufferSection::ram_cap_bytes`] for why it is not larger.
fn default_ram_cap_bytes() -> u64 {
    256 * 1024 * 1024
}

/// `[capture]`: what is recorded, a monitor or one window.
///
/// Both keys are optional and default to the primary monitor. Choosing is the capture crate's
/// ([`replayed_capture::target`]); only Windows Graphics Capture honours it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct CaptureSection {
    /// Monitor index; 0 is the primary. An index with no monitor refuses to start rather than record another display.
    pub monitor: usize,
    /// A window title (case-insensitive; exact beats substring) to capture instead of a monitor.
    /// Empty captures the monitor; no match refuses to start.
    pub window: String,
    /// `"app"`: only the running watched game (the whole output while none runs); `"system"`:
    /// everything the default output plays.
    pub game_audio: replayed_capture::GameAudioSource,
    /// The output endpoint id whose audio is recorded when the whole output is captured (`"system"`,
    /// or `"app"` with no game running). Empty is the Windows default output. A game's own process
    /// audio follows the game whatever device it plays on, so this does not apply to it.
    pub output_device: String,
}

impl CaptureSection {
    pub fn target(&self) -> replayed_capture::target::CaptureTarget {
        replayed_capture::target::CaptureTarget::from_config(self.monitor, &self.window)
    }
}

/// `[encode]` — what the encoder is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EncodeSection {
    /// `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`, resolved by a smoke test.
    pub vendor: String,
    /// `"h264"` or `"hevc"`.
    pub codec: String,
    pub bitrate_kbps: u32,
    /// The frame rate asked for: a ceiling. The sustainable rate is measured at startup (see
    /// `adapt_fps`) and the lower of the two drives both the pacer and the encoder child.
    pub fps: u32,
    /// Hand captured frames to the hardware encoder as GPU textures so pixels never reach this
    /// process's memory (default: unset).
    ///
    /// This removes the per-frame VRAM readback (33.2 MB at 4K), which dominated the pipeline: a
    /// hardware encoder MFT takes the texture directly and ffmpeg copies the H.264 that comes out.
    ///
    /// * Unset: use it when the hardware can (a detected hardware encoder on Windows), otherwise the
    ///   ordinary path with a log line. Refusing to start by default would break the recorder on any
    ///   other machine and the test suite.
    /// * `true`: the hardware path is required; a run that cannot have it is refused at startup with
    ///   the reason rather than silently downgraded. This is why it is an `Option`, not a `bool`.
    /// * `false`: off everywhere.
    ///
    /// Needs a hardware encoder (a software one has no MFT to take a texture) and is Windows-only.
    #[serde(default)]
    pub zero_copy: Option<bool>,
    /// Measure the sustainable encode rate at startup and record at the lower of that and `fps`
    /// (default `true`).
    ///
    /// A pacing decision, not the timeline guarantee: on Windows every captured frame costs a
    /// GPU-to-CPU readback (33 MB at 4K) whether or not it is encoded, so a rate the machine cannot
    /// hold is wasted work and a choppier picture. The media timeline tracks real time regardless,
    /// because frames carry their arrival timestamps (`replayed_encoder::ffmpeg`). With
    /// `adapt_fps = false` the unencodable frames are still dropped (the picture holds them) and the
    /// status line's drop counter says how many.
    ///
    /// The probe costs about 1.5s of startup; `false` skips it and declares `fps` exactly.
    #[serde(default = "adapt_fps_default")]
    pub adapt_fps: bool,
    /// `"1920x1080"`, or empty for the capture backend's native size.
    pub output_size: String,
    /// The rate-control mode: `"cbr"`, `"vbr"` or `"quality"`. Default `"cbr"`.
    ///
    /// * `cbr`: a rate to hold. What every build before this key existed did.
    /// * `vbr`: a rate to aim near, with the peak bounded.
    /// * `quality`: a constant quality, at the encoder's own value — this build carries no QP
    ///   setting, so the number is whatever the encoder defaults to and the file's size follows
    ///   the picture rather than the configured bitrate.
    ///
    /// **Not every path can do every mode.** The Media Foundation path (the zero-copy one) can be
    /// told constant bitrate and variable bitrate; its constant-quality mode needs a QP value
    /// this app does not carry, so it is not offered there (see
    /// [`replayed_encoder::MFT_RATE_CONTROLS`]).
    ///
    /// Defaulted, so a file written before this key existed keeps parsing and keeps recording the
    /// way it did.
    #[serde(default = "default_rate_control")]
    pub rate_control: String,
    /// The encoder's own preset (ffmpeg's `-preset`), or empty for the encoder's own default.
    ///
    /// The names are per vendor — NVENC takes `p1`…`p7`, QSV takes `veryfast`…`veryslow`, AMF
    /// takes `speed`/`balanced`/`quality` — and the list is
    /// [`replayed_encoder::EncoderFamily::presets`]. A preset that does not apply to the encoder
    /// that was selected is dropped with a warning rather than passed on, because ffmpeg refuses
    /// an unknown value and the recording would not start at all.
    ///
    /// Only the ffmpeg-child path can use one: the Media Foundation path has no generic preset.
    #[serde(default)]
    pub preset: String,
}

impl EncodeSection {
    /// The rate-control mode the file asks for.
    ///
    /// The error names the accepted values, because a hand-edited `rate_control = "crf"` has to
    /// say what to write instead: `crf` is a software-encoder mode, and this app has no software
    /// encoder.
    pub fn rate_control_mode(&self) -> Result<replayed_encoder::RateControl> {
        replayed_encoder::RateControl::parse(&self.rate_control)
    }
}

/// `encode.rate_control` when the file does not mention it: constant bitrate, which is what every
/// build before this key existed did.
fn default_rate_control() -> String {
    replayed_encoder::RateControl::Cbr.as_str().to_string()
}

/// `encode.adapt_fps` when the file does not mention it: on, since skipping readbacks for frames
/// the encoder would drop is the better default and an unholdable rate deserves a warning up front.
fn adapt_fps_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// The `[encode]` section alone, so a test does not have to carry the whole file.
    #[derive(Debug, Deserialize)]
    struct File {
        encode: EncodeSection,
    }

    /// The same for `[recorder]`, with the section optional so "absent" can be tested.
    #[derive(Debug, Deserialize)]
    struct RecorderFile {
        #[serde(default)]
        recorder: RecorderSection,
    }

    fn encode_section(text: &str) -> EncodeSection {
        toml::from_str::<File>(text)
            .expect("the section parses")
            .encode
    }

    const MINIMAL: &str = "\
[encode]
vendor = \"auto\"
codec = \"h264\"
bitrate_kbps = 20000
fps = 30
output_size = \"\"
";

    /// The shipped example enables adaptation, and it is the file a fresh install runs.
    #[test]
    fn the_example_config_asks_for_adaptation() {
        let section = encode_section(include_str!("../../../config.example.toml"));
        assert!(
            section.adapt_fps,
            "config.example.toml must enable adaptation"
        );
    }

    /// A `[buffer]` section written before `ram_cap_bytes` existed must still parse; it is the one key
    /// the section can be missing, and an untaken default is a claim rather than a behaviour.
    #[test]
    fn a_config_without_the_ram_cap_parses_and_gets_the_default() {
        #[derive(Debug, Deserialize)]
        struct File {
            buffer: BufferSection,
        }
        let text = format!(
            "{MINIMAL}\n[buffer]\npre_seconds = 30\npost_seconds = 5\nsegment_time = 1\n\
             scratch_dir = \"\"\n"
        );
        let file = toml::from_str::<File>(&text).expect("a config from before this key parses");
        assert_eq!(
            file.buffer.ram_cap_bytes,
            256 * 1024 * 1024,
            "an absent key must default, not fail and not be zero"
        );
    }

    /// The RAM cap is the only bound on the ring, so a hand-edited file must not be able to ask
    /// for more memory than the machine has: the budget is clamped when it is parsed.
    #[test]
    fn an_absurd_ram_cap_is_clamped_when_it_is_parsed() {
        #[derive(Debug, Deserialize)]
        struct File {
            buffer: BufferSection,
        }
        let text = format!(
            "{MINIMAL}\n[buffer]\npre_seconds = 30\npost_seconds = 5\nsegment_time = 1\n\
             scratch_dir = \"\"\nram_cap_bytes = {huge}\n",
            huge = u64::MAX
        );
        let file: File = toml::from_str(&text).expect("a file with an absurd RAM cap parses");
        assert_eq!(
            file.buffer.ram_cap_bytes, MAX_RAM_CAP_BYTES,
            "the budget is clamped at parse time: a cap that is not bounded is not a cap"
        );
        // And the shipped budget is inside the bound, so the clamp never touches a real file.
        assert!(
            default_ram_cap_bytes() <= MAX_RAM_CAP_BYTES,
            "the default must be inside the bound"
        );
    }

    /// The opt-out is honoured; it is the only way to declare a rate the machine may not hold.
    #[test]
    fn the_key_can_turn_adaptation_off() {
        assert!(encode_section(MINIMAL).adapt_fps, "absent means on");
        let text = MINIMAL.replace("fps = 30", "fps = 30\nadapt_fps = false");
        assert!(!encode_section(&text).adapt_fps);
    }

    /// A file written before the rate-control and preset keys existed must still parse, and must
    /// keep recording the way it did: constant bitrate, the encoder's own preset.
    ///
    /// The default is the *behaviour*, not a value: the argument list every existing recording
    /// sends is unchanged (the encoder crate pins that byte for byte).
    #[test]
    fn a_config_without_the_rate_control_or_the_preset_parses_and_gets_the_old_behaviour() {
        let section = encode_section(MINIMAL);
        assert_eq!(
            section.rate_control_mode().unwrap(),
            replayed_encoder::RateControl::Cbr,
            "absent means constant bitrate, as every build before the key did"
        );
        assert_eq!(section.preset, "", "absent means the encoder's own preset");
    }

    /// The mode the file asks for, and what an unknown one says.
    #[test]
    fn the_rate_control_mode_is_read_and_an_unknown_one_is_refused() {
        let text = MINIMAL.replace("fps = 30", "fps = 30\nrate_control = \"vbr\"");
        assert_eq!(
            encode_section(&text).rate_control_mode().unwrap(),
            replayed_encoder::RateControl::Vbr
        );

        let text = MINIMAL.replace("fps = 30", "fps = 30\nrate_control = \"crf\"");
        let err = encode_section(&text)
            .rate_control_mode()
            .expect_err("crf is a software-encoder mode this app has no encoder for");
        assert!(
            err.to_string().contains("crf"),
            "the message names the value: {err}"
        );
    }

    /// The preset is carried as written; the encoder is what knows whether it applies.
    #[test]
    fn the_preset_is_carried_as_written() {
        let text = MINIMAL.replace("fps = 30", "fps = 30\npreset = \"p4\"");
        assert_eq!(encode_section(&text).preset, "p4");
    }

    /// A typo is a parse error rather than a silent default: `adapt_fps = "yes"` must not leave a user misled.
    #[test]
    fn a_non_boolean_value_is_a_parse_error() {
        let text = MINIMAL.replace("fps = 30", "fps = 30\nadapt_fps = \"yes\"");
        let err = toml::from_str::<File>(&text).expect_err("only true/false are accepted");
        assert!(
            err.to_string().contains("adapt_fps"),
            "the error names the key that is wrong: {err}"
        );
    }

    /// A hand-edited file cannot overflow the millisecond arithmetic: the window lengths are
    /// clamped when they are parsed, not when they are multiplied.
    #[test]
    fn an_absurd_window_is_clamped_when_it_is_parsed() {
        #[derive(Debug, Deserialize)]
        struct File {
            buffer: BufferSection,
        }
        let text = format!(
            "{MINIMAL}\n[buffer]\npre_seconds = {huge}\npost_seconds = {huge}\n\
             segment_time = {huge}\nscratch_dir = \"\"\n",
            huge = u64::MAX
        );
        let file: File = toml::from_str(&text).expect("a file with absurd windows parses");
        assert_eq!(
            file.buffer.pre_seconds, MAX_WINDOW_SECONDS,
            "the request is clamped at parse time"
        );
        assert_eq!(file.buffer.post_seconds, MAX_WINDOW_SECONDS);
        assert_eq!(file.buffer.segment_time, MAX_WINDOW_SECONDS);
        // And the arithmetic below it cannot overflow: a day of milliseconds is far inside u64.
        assert_eq!(
            file.buffer.max_pre_ms(),
            MAX_WINDOW_SECONDS * 1000,
            "the ceiling in milliseconds is exact"
        );
    }

    /// The mode parses from `[recorder]`; absent, it is the replay buffer.
    #[test]
    fn the_recording_mode_parses_and_defaults_to_the_replay_buffer() {
        let absent: RecorderFile = toml::from_str("buffer_thing = 1\n").expect("no [recorder]");
        assert_eq!(absent.recorder.mode, RecordingMode::ReplayBuffer);

        let section: RecorderFile =
            toml::from_str("[recorder]\nmode = \"buffer\"\n").expect("the buffer spelling");
        assert_eq!(section.recorder.mode, RecordingMode::ReplayBuffer);

        let session: RecorderFile =
            toml::from_str("[recorder]\nmode = \"session\"\n").expect("the session spelling");
        assert_eq!(session.recorder.mode, RecordingMode::FullSession);
        assert!(session.recorder.mode.is_full_session());
    }

    /// An unknown mode is a parse error naming the key, not a silent fallback to the other mode.
    #[test]
    fn an_unknown_mode_is_refused() {
        let err = toml::from_str::<RecorderFile>("[recorder]\nmode = \"everything\"\n")
            .expect_err("only the two known modes are accepted");
        assert!(
            err.to_string().contains("mode"),
            "the error names the key: {err}"
        );
    }

    /// The config's `"session"` must be the store's `SESSION_MODE_SESSION`, or a row and the config would disagree.
    #[test]
    fn the_mode_spellings_match_the_stores_vocabulary() {
        for mode in [RecordingMode::ReplayBuffer, RecordingMode::FullSession] {
            assert_eq!(mode.as_str(), mode.store_mode(), "{mode:?}");
            assert_eq!(mode.to_string(), mode.store_mode());
            assert_eq!(mode.as_str().parse::<RecordingMode>().unwrap(), mode);
        }
        assert_eq!(
            RecordingMode::FullSession.store_mode(),
            replayed_store::SESSION_MODE_SESSION
        );
        assert_eq!(
            RecordingMode::ReplayBuffer.store_mode(),
            replayed_store::SESSION_MODE_BUFFER
        );
    }

    /// The microphone is opt-in, and the section's absence is the same answer as `false`.
    #[test]
    fn the_microphone_is_off_unless_the_file_turns_it_on() {
        #[derive(Debug, Deserialize)]
        struct File {
            #[serde(default)]
            mic: MicSection,
        }
        let absent: File = toml::from_str("nothing = 1\n").expect("no [mic]");
        assert!(!absent.mic.enabled, "the microphone is off by default");
        let empty: File = toml::from_str("[mic]\n").expect("[mic] with nothing in it");
        assert!(!empty.mic.enabled);
        let on: File = toml::from_str("[mic]\nenabled = true\n").expect("the opt-in");
        assert!(on.mic.enabled);
    }

    /// An older `[storage]` table still parses, keeps its values and gets the default session rules.
    #[test]
    fn the_session_storage_rules_are_optional_and_separate() {
        #[derive(Debug, Deserialize)]
        struct File {
            storage: StorageSection,
        }
        let old: File = toml::from_str(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1000\nmax_age_days = 3\n",
        )
        .expect("a pre-Phase-5 [storage] table must keep parsing");
        assert_eq!(old.storage.max_total_bytes, 1_000);
        assert_eq!(old.storage.max_age_days, 3);
        assert_eq!(
            old.storage.sessions,
            SessionStorageRules::default(),
            "the default rules"
        );
        assert!(old.storage.sessions.max_total_bytes > old.storage.max_total_bytes);

        let extended: File = toml::from_str(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1000\nmax_age_days = 3\n\n\
             [storage.sessions]\nmax_total_bytes = 500\nmax_age_days = 1\n",
        )
        .expect("the session table");
        assert_eq!(extended.storage.sessions.max_total_bytes, 500);
        assert_eq!(extended.storage.sessions.max_age_days, 1);
        assert_eq!(
            extended.storage.sessions.sessions_dir, "",
            "empty means the default"
        );
        assert_eq!(
            extended.storage.max_total_bytes, 1_000,
            "the clips rule is untouched"
        );
    }

    /// The example file, parsed into the three sections [`validate`] looks at.
    #[derive(Debug, Deserialize)]
    struct Sections {
        buffer: BufferSection,
        encode: EncodeSection,
        storage: StorageSection,
    }

    fn example_sections() -> Sections {
        toml::from_str(include_str!("../../../config.example.toml")).expect("the example parses")
    }

    fn check(sections: &Sections) -> Result<()> {
        validate(&sections.buffer, &sections.encode, &sections.storage)
    }

    #[test]
    fn the_example_config_validates() {
        check(&example_sections()).expect("config.example.toml must stay valid");
    }

    /// A zero retention rule used to mean "delete every non-favourite clip" at the next cleanup pass.
    #[test]
    fn a_zero_retention_rule_is_refused_at_load() {
        for (what, edit) in [
            (
                "storage.max_age_days",
                (|s: &mut Sections| s.storage.max_age_days = 0) as fn(&mut Sections),
            ),
            ("storage.max_total_bytes", |s| s.storage.max_total_bytes = 0),
            ("storage.sessions.max_age_days", |s| {
                s.storage.sessions.max_age_days = 0
            }),
            ("storage.sessions.max_total_bytes", |s| {
                s.storage.sessions.max_total_bytes = 0
            }),
        ] {
            let mut sections = example_sections();
            edit(&mut sections);
            let err = check(&sections).expect_err(what);
            assert!(format!("{err:#}").contains(what), "{what}: {err:#}");
        }
    }

    #[test]
    fn the_config_check_and_the_stores_policy_check_agree() {
        // One rule, two doors: the config check asks the store, so every combination of zero and
        // non-zero values gets the same answer from both, and a huge day count (clamped, not
        // wrapped to zero) is accepted by both.
        for bits in 0..16u32 {
            let mut sections = example_sections();
            let pick = |bit: u32| if bits & bit == 0 { 5 } else { 0 };
            sections.storage.max_total_bytes = pick(1);
            sections.storage.max_age_days = pick(2);
            sections.storage.sessions.max_total_bytes = pick(4);
            sections.storage.sessions.max_age_days = pick(8);
            assert_eq!(
                validate_retention(&sections.storage).is_ok(),
                retention_policy(&sections.storage).validate().is_ok(),
                "bits {bits:04b}"
            );
            assert_eq!(check(&sections).is_ok(), bits == 0, "bits {bits:04b}");
        }
        let mut sections = example_sections();
        sections.storage.max_age_days = u64::from(u32::MAX) + 1;
        sections.storage.sessions.max_age_days = u64::MAX;
        assert!(check(&sections).is_ok(), "above u32::MAX clamps to 'never'");
        assert_eq!(
            retention_policy(&sections.storage).clips.max_age_days,
            u32::MAX
        );
    }

    #[test]
    fn the_cli_checks_now_live_in_the_library() {
        for (what, edit) in [
            (
                "buffer.segment_time",
                (|s: &mut Sections| s.buffer.segment_time = 0) as fn(&mut Sections),
            ),
            ("buffer.post_seconds", |s| s.buffer.post_seconds = 0),
            ("encode.fps", |s| s.encode.fps = 0),
            ("encode.vendor", |s| s.encode.vendor = "software".into()),
            ("encode.vendor", |s| s.encode.vendor = "bogus".into()),
        ] {
            let mut sections = example_sections();
            edit(&mut sections);
            let err = check(&sections).expect_err(what);
            assert!(format!("{err:#}").contains(what), "{what}: {err:#}");
        }
    }

    #[test]
    fn the_ram_cap_has_a_floor_and_a_ceiling() {
        let mut sections = example_sections();
        sections.buffer.ram_cap_bytes = 0;
        assert!(check(&sections).is_err(), "a ring that holds nothing");
        sections.buffer.ram_cap_bytes = MIN_RAM_CAP_BYTES - 1;
        assert!(check(&sections).is_err());
        sections.buffer.ram_cap_bytes = MIN_RAM_CAP_BYTES;
        assert!(check(&sections).is_ok());
        sections.buffer.ram_cap_bytes = MAX_RAM_CAP_BYTES + 1;
        assert!(
            check(&sections).is_err(),
            "a struct built in code is bounded like a parsed file"
        );
    }
}

/// `[storage]`: the clips directory and the policy applied to it. [`StorageSection::sessions`] is additive,
/// so older files still parse.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StorageSection {
    /// Empty means `<app data dir>/clips`.
    pub clips_dir: String,
    /// Total bytes the clips directory may occupy, favourites included. Nothing is
    /// deleted to satisfy a cap the favourites alone exceed.
    pub max_total_bytes: u64,
    /// A non-favourited clip older than this many days is deleted.
    pub max_age_days: u64,
    /// `[storage.sessions]`: the same two rules for the session store. Optional; absent gets
    /// [`SessionStorageRules::default`].
    #[serde(default)]
    pub sessions: SessionStorageRules,
}

/// `[storage.sessions]`: the session store's own rules.
///
/// Field names mirror the clips rules because sessions and clips are evicted independently by the
/// same two rules with their own numbers ([`crate::index::cleanup_pass`] applies both).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SessionStorageRules {
    /// Empty means `<app data dir>/sessions`.
    pub sessions_dir: String,
    /// Total bytes the session store may occupy, favourites and running sessions included. A running
    /// session is never an eviction candidate; a live recording that alone exceeds the cap is reported
    /// as an unsatisfiable cap rather than paid for out of finished sessions.
    pub max_total_bytes: u64,
    /// A non-favourited session that started longer ago than this many days is deleted.
    pub max_age_days: u64,
}

impl Default for SessionStorageRules {
    /// 200 GiB and a week. Generous because a session is one file the user recorded deliberately end
    /// to end; users who record a lot should raise it.
    fn default() -> Self {
        Self {
            sessions_dir: String::new(),
            max_total_bytes: 214_748_364_800,
            max_age_days: 7,
        }
    }
}

/// The smallest RAM budget the replay buffer may be given: 8 MiB.
///
/// A budget below one fragment of footage (a second of high-bitrate video is several MiB) makes the
/// ring evict each fragment as it arrives, so a clip request finds nothing and the user sees
/// "nothing covers that range" with no hint that their own setting caused it. Better refused at start.
pub const MIN_RAM_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// Check the recorder's own sections, the one place the CLI and the desktop shell share.
///
/// These rules used to live in the CLI's config loader only, so a desktop-recorded session with
/// `segment_time = 0` or a zero retention rule skipped them. The recorder calls this at start
/// ([`crate::RecorderConfig::validate`]), so a front-end that forgets still cannot start from a
/// bad file; a front-end may call it earlier to report the problem at load.
///
/// Every refusal names the key, and none is quietly repaired: a value that is wrong is wrong.
///
/// * `max_age_days = 0` and `max_total_bytes = 0` are refused for both the clips and the sessions
///   rule. Zero has no "disabled" meaning in the retention policy: a zero cap is exceeded by
///   every file and a zero age by every clip, so the next cleanup pass deletes every
///   non-favourite item. A user who wants no retention sets a very large number.
/// * `ram_cap_bytes` is held between [`MIN_RAM_CAP_BYTES`] and [`MAX_RAM_CAP_BYTES`]. The upper
///   bound is also clamped when a file is parsed; checking it here covers a struct built in code.
pub fn validate(
    buffer: &BufferSection,
    encode: &EncodeSection,
    storage: &StorageSection,
) -> Result<()> {
    use anyhow::bail;
    // Fail closed: there is no CPU encoder to fall back to.
    match encode.vendor.as_str() {
        "auto" | "nvenc" | "qsv" | "amf" => {}
        "software" => bail!(
            "encode.vendor = \"software\" is not supported: replayed requires a GPU hardware \
             encoder (nvenc, qsv or amf) so that capturing does not cost game performance"
        ),
        other => bail!("unknown encode.vendor: {other}"),
    }
    if buffer.segment_time == 0 {
        bail!("buffer.segment_time must be at least 1 second");
    }
    if buffer.post_seconds == 0 {
        bail!("buffer.post_seconds must be at least 1 second");
    }
    // The rate declared to the encoder (`-framerate`) and capture is paced to. Zero is not a rate:
    // it makes `1 / fps` meaningless and ffmpeg's `-framerate 0` an error.
    if encode.fps == 0 {
        bail!(
            "encode.fps must be at least 1: it is the ceiling on the frame rate the pipeline \
             paces to (it measures itself against this - see encode.adapt_fps)"
        );
    }
    if !(MIN_RAM_CAP_BYTES..=MAX_RAM_CAP_BYTES).contains(&buffer.ram_cap_bytes) {
        bail!(
            "buffer.ram_cap_bytes must be between {MIN_RAM_CAP_BYTES} (8 MiB) and \
             {MAX_RAM_CAP_BYTES} (64 GiB), got {}",
            buffer.ram_cap_bytes
        );
    }
    validate_retention(storage)
}

/// The storage section as the retention rules the store plans with.
///
/// The one conversion, used by the check ([`validate_retention`]) and by the pass that deletes
/// ([`crate::index::cleanup_pass`]), so what is validated is exactly what is applied. Days are `u32`
/// in the retention rules and `u64` in the config; above `u32::MAX` means "never", so clamp rather
/// than wrap (a wrap could turn a huge number into the refused zero, or worse into a tiny age).
pub fn retention_policy(storage: &StorageSection) -> replayed_store::retention::RetentionPolicy {
    use replayed_store::retention::{RetentionPolicy, RetentionRules};
    let days = |days: u64| days.min(u64::from(u32::MAX)) as u32;
    RetentionPolicy {
        clips: RetentionRules {
            max_total_bytes: storage.max_total_bytes,
            max_age_days: days(storage.max_age_days),
        },
        sessions: RetentionRules {
            max_total_bytes: storage.sessions.max_total_bytes,
            max_age_days: days(storage.sessions.max_age_days),
        },
    }
}

/// The retention half of [`validate`]: no zero cap and no zero age, for the clips and the sessions.
///
/// There is one rule, [`replayed_store::retention::RetentionPolicy::validate`], the check the store's own
/// planner makes before it plans anything; this asks it about the policy the configuration
/// converts to ([`retention_policy`]) and says which keys to look at. Two copies of "zero is
/// refused" drift (one gains a rule, the other keeps deleting). Also called by
/// [`crate::index::cleanup_pass`] where the deleting happens.
pub fn validate_retention(storage: &StorageSection) -> Result<()> {
    retention_policy(storage).validate().map_err(|err| {
        // The store's refusal does not know the config's key names, so say which of them are zero.
        // Only the message: whether to refuse was the store's decision.
        let zero: Vec<&str> = [
            ("storage.max_total_bytes", storage.max_total_bytes),
            ("storage.max_age_days", storage.max_age_days),
            (
                "storage.sessions.max_total_bytes",
                storage.sessions.max_total_bytes,
            ),
            (
                "storage.sessions.max_age_days",
                storage.sessions.max_age_days,
            ),
        ]
        .into_iter()
        .filter(|(_, value)| *value == 0)
        .map(|(key, _)| key)
        .collect();
        err.context(format!(
            "invalid retention rule: {} must be at least 1",
            zero.join(", ")
        ))
    })
}
