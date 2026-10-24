//! Where the desktop shell reads its settings from, and where it keeps its files.
//!
//! # Why this is not `replayed_cli::config::Config`
//!
//! The CLI owns the full configuration model and this module deliberately does not duplicate
//! it:
//!
//! 1. The shell reads only what it needs: `[storage]` for the review window, plus `[encode]`,
//!    `[mic]` and `[games]` for the settings panel and the options a start is built with. It
//!    captures nothing, so `[buffer]`, `[audio]`, `[hotkeys]` and `[events]` are none of its
//!    business, and re-validating them here would let a capture-only mistake (say
//!    `vendor = "software"`) refuse to open the review UI.
//! 2. Depending on `replayed-cli` for its `Config` would pull the whole capture stack (`wgc`,
//!    `wasapi`, the ffmpeg encoder child) into the GUI binary for a handful of fields.
//!
//! # The invariant that keeps the window usable
//!
//! **Reading applies the engine's rules; writing may add the panel's own.**
//! [`read_effective_settings`] must accept every value a start accepts, because the settings
//! panel renders only once it succeeds: a reader stricter than the engine is a panel that will
//! not open, and so cannot repair the file that stopped it. The panel may refuse more on the way
//! in (`validate_edit`: the fps band, and the `clips_dir` hygiene that keeps the asset-protocol
//! scope off whole volumes), because those refuse a value the user already has on screen.
//!
//! This has gone wrong before: the panel's own copy of the `output_size` rule added a frame
//! bound the engine does not have and trimmed nothing, so a file carrying `" 1920x1080"` recorded
//! perfectly while leaving the settings panel permanently blank and silent.
//!
//! What must not drift is the location the two binaries agree on, which is why [`app_data_dir`]
//! repeats the CLI's rule verbatim: the CLI writes `replayed.db` there and the shell reads the
//! same file. A test pins the parse of the real `config.example.toml` against these sections.
//!
//! Reading the whole file through the CLI's own type would let the engine's rules be borrowed
//! rather than mirrored by hand; that is a bigger change than this module has needed.

use crate::commands::{CommandError, EncoderSettings, ErrorCode};
use replayed_events::process::{
    default_watch, validate_watch_list, GamesSection, Signal, WatchedGame,
};
use replayed_recorder::config::{
    BufferSection, CaptureSection, EncodeSection, MicSection, RecorderSection, StorageSection,
};
use replayed_recorder::config_schema;
use replayed_recorder::event_driver::EventsSection;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The repository's example config, compiled in as the fallback for a fresh install. The path
/// is relative to this file.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../../config.example.toml");

/// The `[storage]` section of the config file.
///
/// **Deliberately more forgiving than the CLI's own `StorageSection`**, which requires all
/// three keys: a key the file omits falls back to the example's value here. The asymmetry runs
/// one way only — the desktop accepts everything the CLI accepts and a little more — because
/// the shell opens a window that can show the problem and repair it, and the CLI has nothing
/// to show one in. The fallback is the example's value and never zero: a zero cap would make
/// every clip "over the cap", which is a far worse failure than a missing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    /// Empty means "the `clips` directory under the application data directory".
    pub clips_dir: String,
    /// Total bytes the clips directory may occupy, favourites included.
    pub max_total_bytes: u64,
    /// A non-favourited clip older than this many days is deleted.
    pub max_age_days: u64,
}

/// The cap a file that omits `max_total_bytes` falls back to: the example's 50 GiB.
const DEFAULT_MAX_TOTAL_BYTES: u64 = 53_687_091_200;

/// The age a file that omits `max_age_days` falls back to: the example's seven days.
const DEFAULT_MAX_AGE_DAYS: u64 = 7;

impl StorageConfig {
    /// The settings from the repository's example config, the fallback when no `config.toml`
    /// exists yet.
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        // Only `[storage]` is required: an otherwise capture-shaped file is valid here, and a file
        // whose `[storage]` is missing or mis-typed is not. A *key* the section omits falls back
        // to the example's value (see the type's docs); a key it mistypes is still an error, so a
        // typo'd cap is never read as "not set".
        #[derive(Deserialize)]
        struct File {
            storage: PartialStorage,
        }
        #[derive(Deserialize)]
        struct PartialStorage {
            clips_dir: Option<String>,
            max_total_bytes: Option<u64>,
            max_age_days: Option<u64>,
        }
        config_schema::parse::<File>(text)
            .map(|f| f.storage)
            .map(|storage| Self {
                clips_dir: storage.clips_dir.unwrap_or_default(),
                max_total_bytes: storage.max_total_bytes.unwrap_or(DEFAULT_MAX_TOTAL_BYTES),
                max_age_days: storage.max_age_days.unwrap_or(DEFAULT_MAX_AGE_DAYS),
            })
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("the config file's [storage] section could not be read: {err}"),
                )
            })
    }

    /// Read the config at `path`, or fall back to the example when it does not exist.
    ///
    /// A file that exists but is malformed is an error, not a silent fallback: a user who typo'd
    /// their cap must not be shown an app quietly behaving as if they had not. The CLI also
    /// refuses to start on a bad file.
    pub fn load(path: &Path) -> Result<Self, CommandError> {
        Self::from_toml(&Self::text_or_example(path)?)
    }

    /// The file's text, or the example's when there is no file.
    ///
    /// The example is compiled in, so a fresh install has the values the repository documents.
    fn text_or_example(path: &Path) -> Result<String, CommandError> {
        if !path.is_file() {
            tracing::info!(
                "no config at {}; using the values from config.example.toml",
                path.display()
            );
            return Ok(EXAMPLE_CONFIG.to_string());
        }
        std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })
    }

    /// The settings a shell that could not read the file starts with.
    ///
    /// The example's values, or the documented defaults if the compiled-in example cannot be
    /// read (which a test pins against the file). Never zero: a zero cap would make every clip
    /// "over the cap".
    pub fn fallback() -> Self {
        Self::example().unwrap_or_else(|_| Self {
            clips_dir: String::new(),
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
        })
    }

    /// The settings to start the shell with, and the warning to show when the file's could not
    /// be read.
    ///
    /// A malformed config must not stop the shell: the review window, the tray and the hotkey
    /// all work whatever the file says, and the settings panel is where a user repairs it. The
    /// CLI refuses to start on a bad file — it has no window to say why in — so the desktop
    /// starts with the example's values and puts the reason where the user will see it.
    ///
    /// A key the file omits is a warning too, not silence: the cap in force is the example's,
    /// and the user should hear that theirs was not read.
    pub fn load_or_example(path: &Path) -> (Self, Option<String>) {
        let text = match Self::text_or_example(path) {
            Ok(text) => text,
            Err(err) => {
                tracing::error!("{err}");
                return (
                    Self::fallback(),
                    Some(format!(
                        "the config file could not be read, so the values in config.example.toml \
                         are in force: {}",
                        err.message
                    )),
                );
            }
        };
        match Self::from_toml(&text) {
            Ok(storage) => {
                let missing = missing_storage_keys(&text);
                if missing.is_empty() {
                    (storage, None)
                } else {
                    (
                        storage,
                        Some(format!(
                            "the config file does not set {}, so the values in \
                             config.example.toml are in force for them",
                            missing.join(", ")
                        )),
                    )
                }
            }
            Err(err) => {
                tracing::error!("{err}");
                (
                    Self::fallback(),
                    Some(format!(
                        "the config file could not be read, so the values in config.example.toml \
                         are in force: {}",
                        err.message
                    )),
                )
            }
        }
    }
}

/// The `[storage]` keys the file does not set.
///
/// Each of them falls back to the example's value, and each is named: a user whose cap is not
/// the one they set should hear about it rather than find out when a clip is deleted.
fn missing_storage_keys(text: &str) -> Vec<&'static str> {
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let Some(storage) = doc.get("storage").and_then(|item| item.as_table_like()) else {
        return Vec::new();
    };
    ["clips_dir", "max_total_bytes", "max_age_days"]
        .into_iter()
        .filter(|key| storage.get(key).is_none())
        .collect()
}

/// The `[buffer]`, `[encode]` and `[storage]` sections: everything the recorder needs to start.
///
/// Separate from [`StorageConfig`] because the review pane reads `[storage]` when the window
/// opens, while a recording needs `[buffer]` and `[encode]` too. The shell must open even when
/// the capture sections are missing or wrong (a file carrying only `[storage]`, a typo'd
/// `vendor`), so this is parsed when recording starts, and a failure there fails that command
/// rather than the window.
///
/// The section types are the recorder's own, which the CLI deserialises too.
#[derive(Debug, Clone, Deserialize)]
pub struct RecordingConfig {
    pub buffer: BufferSection,
    pub encode: EncodeSection,
    pub storage: StorageSection,
    #[serde(default)]
    pub capture: CaptureSection,
    /// The `[audio]` section: only the game-audio track's rate.
    ///
    /// The CLI owns the whole section (the track's codec and whether it exists at all), and the
    /// shell has no switch for a video-only recording. The **bitrate** is read because the
    /// encoder panel offers it: a rate the panel can change has to reach the encoder, and the
    /// encoder's own default is 192 kbps when nothing is configured.
    #[serde(default)]
    pub audio: AudioSection,
}

/// The `[audio]` section, as far as the shell cares: the game-audio track's rate.
///
/// Absent means the encoder's own default, not zero: a track recorded at 0 kbps is not a track.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AudioSection {
    /// The game-audio track's rate, in kilobits per second.
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
}

impl RecordingConfig {
    /// The capture sections of the repository's example config, the fallback for a fresh install.
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        config_schema::parse::<Self>(text).map_err(|err| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "the config file's [buffer], [encode] and [storage] sections could not \
                     be read, so a recording cannot be started: {err}"
                ),
            )
        })
    }

    /// Read the recording settings from `path`, or fall back to the example when the file does not
    /// exist.
    ///
    /// A file that exists but is malformed is an error, not a silent fallback: a user who typo'd
    /// `encode.fps` must not be recorded at a rate they did not ask for.
    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            tracing::info!(
                "no config at {}; recording with the values from config.example.toml",
                path.display()
            );
            return Self::example();
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?;
        Self::from_toml(&text)
    }
}

/// The `[hotkeys]` and `[app]` sections: what the shell's background half needs.
///
/// The third reader of the same file: `[storage]` opens the review window,
/// `[buffer]`/`[encode]`/`[storage]` start a recording, and these two install the global clip
/// hotkey and, optionally, the start-with-system entry. Read once at startup, not when a window
/// opens: a hotkey is registered once and owned until exit, which is why the tray offers "open
/// config file" and the panel shows the path it came from.
///
/// Both sections are optional here: a hand-trimmed file without them is still a working window
/// with the documented default chord. The CLI's own `Config` requires both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundConfig {
    /// `[hotkeys] clip`, verbatim. Parsed by `replayed_events::hotkey::Hotkey`, the CLI's parser,
    /// so one file means one chord to both front-ends.
    pub clip_hotkey: String,
    /// `[hotkeys] toggle_recording`: start/stop the recording. Off unless set.
    pub toggle_recording_hotkey: Option<String>,
    /// `[hotkeys] bookmark`: drop a marker on the session timeline. Off unless set.
    pub bookmark_hotkey: Option<String>,
    /// `[app] start_with_system`: whether the shell should own a start-with-Windows entry.
    pub start_with_system: bool,
}

/// The chord used when `config.toml` has no `[hotkeys]` section.
///
/// A copy of `config.example.toml`'s value, pinned to it by a test, because two spellings of
/// "the default" that disagree is the drift a default exists to prevent.
pub const DEFAULT_CLIP_HOTKEY: &str = "Ctrl+F8";

impl BackgroundConfig {
    /// The settings from the repository's example config, the fallback when no `config.toml`
    /// exists yet.
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        #[derive(Deserialize)]
        struct File {
            hotkeys: Option<HotkeySection>,
            #[serde(default)]
            app: Option<AppSection>,
        }
        #[derive(Deserialize)]
        struct HotkeySection {
            clip: String,
            #[serde(default)]
            toggle_recording: Option<String>,
            #[serde(default)]
            bookmark: Option<String>,
        }
        #[derive(Deserialize)]
        struct AppSection {
            #[serde(default)]
            start_with_system: bool,
        }

        let file: File = config_schema::parse(text).map_err(|err| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "the config file's [hotkeys] section could not be read, so the clip \
                     hotkey cannot be installed: {err}"
                ),
            )
        })?;
        let (toggle_recording_hotkey, bookmark_hotkey) = file
            .hotkeys
            .as_ref()
            .map(|h| (h.toggle_recording.clone(), h.bookmark.clone()))
            .unwrap_or_default();
        Ok(Self {
            toggle_recording_hotkey,
            bookmark_hotkey,
            clip_hotkey: file.hotkeys.map(|h| h.clip).unwrap_or_else(|| {
                tracing::info!(
                    "config.toml has no [hotkeys] section; using the documented default \
                     \"{DEFAULT_CLIP_HOTKEY}\""
                );
                DEFAULT_CLIP_HOTKEY.to_string()
            }),
            start_with_system: file.app.map(|a| a.start_with_system).unwrap_or(false),
        })
    }

    /// Read the background settings from `path`, or fall back to the example when the file does not
    /// exist. A malformed file is an error: a typo'd hotkey must be reported, not replaced.
    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            tracing::info!(
                "no config at {}; the clip hotkey and [app] settings come from \
                 config.example.toml",
                path.display()
            );
            return Self::example();
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?;
        Self::from_toml(&text)
    }
}

/// The `[mic]` and `[games]` sections: what a recording is started with.
///
/// Read at start time like the capture sections: a review pane needs neither a microphone nor a
/// game watcher. The types are the engine's own. Both sections default when absent (`MicSection`
/// off, `GamesSection` with `auto_record = false` and the default watch list); a present but
/// mis-typed section is an error naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOptionsConfig {
    /// `[recorder]`; the window records a replay buffer, so only `save_session` is read from it.
    pub recorder: RecorderSection,
    pub mic: MicSection,
    pub games: GamesSection,
    /// Which game-event sources to run. Absent means the example's values (League polling on,
    /// the GSI listener on its default port), the same as the CLI.
    pub events: EventsSection,
}

impl EngineOptionsConfig {
    pub fn example() -> Result<Self, CommandError> {
        Self::from_toml(EXAMPLE_CONFIG)
    }

    pub fn from_toml(text: &str) -> Result<Self, CommandError> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            recorder: RecorderSection,
            #[serde(default)]
            mic: MicSection,
            #[serde(default)]
            games: GamesSection,
            #[serde(default)]
            events: EventsSection,
        }
        let config = config_schema::parse::<File>(text)
            .map(|f| Self {
                recorder: f.recorder,
                mic: f.mic,
                games: f.games,
                events: f.events,
            })
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "the config file's [recorder], [mic], [games] or [events] section could not be read: \
                         {err}"
                    ),
                )
            })?;
        // The GSI port is the one key in these sections the CLI bounds, so the desktop bounds it
        // too: a cfg that points the game at port 80 would have the game talk to whatever else
        // is listening there.
        validate_gsi_port(config.events.gsi_port)?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, CommandError> {
        if !path.is_file() {
            // The siblings log this path too; a quiet one would hide "the panel is showing example values"
            // from the log exactly when it matters.
            tracing::info!(
                "no config file at {}: starting with the example's [mic] and [games]",
                path.display()
            );
            return Self::example();
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?;
        Self::from_toml(&text)
    }
}

/// Bounds for `[encode] fps`: the panel's own policy, not the engine's. Nothing in the engine
/// bounds `fps` (a bare `u32`; the capture rate adapts to what the machine measures), so these
/// apply on the write path only, refusing a value while the user's hand is still on it.
///
/// They do not apply to [`read_effective_settings`]: a reader stricter than a start is a panel
/// that never renders, with no way to repair the file from the UI.
pub const FPS_MIN: u32 = 1;
pub const FPS_MAX: u32 = 240;

/// The ceiling on `[encode] bitrate_kbps`: 250 Mbit/s.
///
/// The panel's own policy, not the engine's: nothing in the engine bounds the bitrate, and a
/// number a user can type by accident is not a number to write into the file. 250 Mbit/s is far
/// above any real recording (a 4K60 HEVC capture is a tenth of it) and far below the point where
/// the value stops being a rate and starts being a typo.
pub const MAX_BITRATE_KBPS: u32 = 250_000;

/// The ceiling on `[audio] bitrate_kbps`: 512 kbit/s.
///
/// AAC tops out well below that; the ceiling is here to catch a mistyped number, not to describe
/// a limit the codec has.
pub const MAX_AUDIO_BITRATE_KBPS: u32 = 512;

/// The GSI listener's port rule, the CLI's (`apps/replayed-cli/src/config.rs`).
///
/// `0` turns the listener off; anything else is a real port, and the default sits above the
/// ephemeral range so a temporarily bound listener elsewhere cannot shadow it. The desktop
/// applies the same rule the CLI does, because the desktop writes the GSI cfg the game reads:
/// a cfg pointing at port 80 would have the game talk to whatever else is listening there.
pub fn validate_gsi_port(port: u16) -> Result<(), CommandError> {
    if port == 0 || port >= 1024 {
        Ok(())
    } else {
        Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "events.gsi_port must be 0 (the listener off) or between 1024 and 65535, but it \
                 is {port}"
            ),
        ))
    }
}

/// One edited key. Typed by construction, so the writer never parses user text into TOML
/// values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingEdit {
    /// `[storage] clips_dir`. Empty resets to the application-data default.
    ClipsDir(String),
    /// `[encode] fps`.
    Fps(u32),
    /// `[encode] output_size`. Empty means the native capture size.
    OutputSize(String),
    /// `[mic] enabled`.
    MicEnabled(bool),
    /// `[mic] device`: a capture endpoint id. Empty is the Windows default microphone.
    MicDevice(String),
    /// `[capture] output_device`: an output endpoint id. Empty is the Windows default output.
    OutputDevice(String),
    /// `[recorder] save_session`: also keep the whole recording as a session file.
    SaveSession(bool),
    /// `[games] auto_record`.
    AutoRecord(bool),
    /// `[app] start_with_system`.
    StartWithSystem(bool),
    /// `[hotkeys] clip`: the chord, as the user typed it (normalised by `Hotkey`'s `Display` when
    /// it is read back). Validated by the same parser the installer uses.
    ClipHotkey(String),
    /// `[[games.watch]]`: the whole list, replacing whatever the file had.
    ///
    /// The list is the feature (`[games] auto_record` watches these), so it is written as one
    /// value rather than per-entry edits: a rename and a removal are one atomic change, and an
    /// entry is never half-written. An empty list is accepted and means "the default titles",
    /// because that is what an absent list means to the engine — the panel removes entries down
    /// to it rather than pretending the game list can be emptied.
    WatchGames(Vec<WatchedGame>),
    /// A quality preset ([`QUALITY_PRESETS`]), written as `[encode] bitrate_kbps`.
    Quality(String),
    /// `[capture] monitor`.
    CaptureMonitor(usize),
    /// `[capture] window`. Empty captures the monitor.
    CaptureWindow(String),
    /// `[encode] vendor`: `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`.
    EncodeVendor(String),
    /// `[encode] codec`: `"h264"` or `"hevc"`.
    EncodeCodec(String),
    /// `[encode] bitrate_kbps`, in kilobits per second.
    EncodeBitrate(u32),
    /// `[encode] adapt_fps`: measure what the machine can sustain and record at the lower of
    /// that and `fps`.
    EncodeAdaptFps(bool),
    /// `[encode] zero_copy`: hand the captured texture to the hardware encoder.
    ///
    /// `None` **removes** the key, which is what "let the recorder decide" means: an absent key
    /// is the hardware path when the machine has one.
    EncodeZeroCopy(Option<bool>),
    /// `[encode] rate_control`: `"cbr"`, `"vbr"` or `"quality"`.
    EncodeRateControl(String),
    /// `[encode] preset`: the encoder's own preset, or empty for its default.
    EncodePreset(String),
    /// `[audio] bitrate_kbps`: the game-audio track's rate.
    AudioBitrate(u32),
}

/// The quality presets the panel offers, as `[encode] bitrate_kbps`.
///
/// A preset is only a name for a bitrate: nothing but `bitrate_kbps` is stored, so the engine
/// and CLI need no second key. `balanced` is the example config's 20 Mbit/s. A bitrate matching
/// no preset reads back as `custom`.
pub const QUALITY_PRESETS: [(&str, u32); 3] =
    [("low", 8_000), ("balanced", 20_000), ("high", 40_000)];

/// The preset name a bitrate corresponds to, or `"custom"`.
pub fn quality_for_bitrate(kbps: u32) -> &'static str {
    QUALITY_PRESETS
        .iter()
        .find(|(_, b)| *b == kbps)
        .map_or("custom", |(name, _)| name)
}

fn preset_bitrate(name: &str) -> Option<u32> {
    QUALITY_PRESETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
}

impl SettingEdit {
    pub(crate) fn key(&self) -> &'static str {
        match self {
            SettingEdit::ClipsDir(_) => "storage.clips_dir",
            SettingEdit::Fps(_) => "encode.fps",
            SettingEdit::OutputSize(_) => "encode.output_size",
            SettingEdit::MicEnabled(_) => "mic.enabled",
            SettingEdit::MicDevice(_) => "mic.device",
            SettingEdit::OutputDevice(_) => "capture.output_device",
            SettingEdit::SaveSession(_) => "recorder.save_session",
            SettingEdit::AutoRecord(_) => "games.auto_record",
            SettingEdit::WatchGames(_) => "games.watch",
            SettingEdit::StartWithSystem(_) => "app.start_with_system",
            SettingEdit::ClipHotkey(_) => "hotkeys.clip",
            SettingEdit::Quality(_) => "encode.bitrate_kbps",
            SettingEdit::CaptureMonitor(_) => "capture.monitor",
            SettingEdit::CaptureWindow(_) => "capture.window",
            SettingEdit::EncodeVendor(_) => "encode.vendor",
            SettingEdit::EncodeCodec(_) => "encode.codec",
            SettingEdit::EncodeBitrate(_) => "encode.bitrate_kbps",
            SettingEdit::EncodeAdaptFps(_) => "encode.adapt_fps",
            SettingEdit::EncodeZeroCopy(_) => "encode.zero_copy",
            SettingEdit::EncodeRateControl(_) => "encode.rate_control",
            SettingEdit::EncodePreset(_) => "encode.preset",
            SettingEdit::AudioBitrate(_) => "audio.bitrate_kbps",
        }
    }
}

/// The settings the window shows and edits, with per-key provenance.
///
/// Values come from the file when it sets them and from the compiled-in example otherwise;
/// `defaulted` names the keys that fell back (e.g. `"encode.fps"`), so the panel does not
/// present example values as configured ones.
///
/// A mis-typed key is an error naming it, as is an `output_size` the engine's parser refuses.
/// Anything else is reported as the file has it, including values the panel would refuse to
/// write (an out-of-band fps, a relative clips_dir): the panel must render to repair them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSettings {
    pub clips_dir: String,
    pub fps: u32,
    pub output_size: String,
    pub mic_enabled: bool,
    /// `[mic] device`; empty is the Windows default microphone.
    pub mic_device: String,
    /// `[capture] output_device`; empty is the Windows default output.
    pub output_device: String,
    /// `[recorder] save_session`.
    pub save_session: bool,
    pub auto_record: bool,
    /// `[encode] bitrate_kbps`; [`quality_for_bitrate`] names it.
    pub bitrate_kbps: u32,
    /// `[capture] monitor`.
    pub capture_monitor: usize,
    /// `[capture] window`; empty captures the monitor.
    pub capture_window: String,
    /// The file's `[[games.watch]]` list, or the default watch list when the file names none
    /// (which is what the engine watches then, too). Names, executables and signals: the panel
    /// edits the list, so a display name is not enough.
    pub watch: Vec<WatchedGame>,
    /// `[hotkeys] clip`: the chord the shell installs at startup, verbatim from the file (or the
    /// documented default when it sets none). The panel edits it; the shell is the thing that has
    /// to hold it, so a change takes effect at the next start.
    pub clip_hotkey: String,
    /// `[app] start_with_system`: whether this application should own a start-with-Windows entry.
    ///
    /// The file says what the user wants; the registry says what is. Reading them apart is the
    /// panel's whole job here: the checkbox shows the file's value, and applying it reports what
    /// Windows did (see `background::sync_autostart`).
    pub start_with_system: bool,
    pub defaulted: Vec<&'static str>,
    pub config_exists: bool,
}

/// The message for a key that is present but the wrong shape. `what` names the shape it should
/// have had: a bool for the two switches, a string for the two paths, an integer for the rate,
/// an array of tables for `games.watch`.
fn mistyped(key: &'static str) -> CommandError {
    CommandError::new(
        ErrorCode::InvalidInput,
        format!(
            "the config file's [{section}] {key} is not a {what}, so it cannot be used",
            section = key.split('.').next().unwrap_or("config"),
            what = match key {
                "storage.clips_dir"
                | "hotkeys.clip"
                | "encode.output_size"
                | "encode.vendor"
                | "encode.codec"
                | "encode.rate_control"
                | "encode.preset"
                | "capture.window"
                | "mic.device"
                | "capture.output_device" => "string",
                "encode.fps" | "encode.bitrate_kbps" | "audio.bitrate_kbps" | "capture.monitor" =>
                    "integer",
                "mic.enabled"
                | "games.auto_record"
                | "recorder.save_session"
                | "encode.adapt_fps"
                | "encode.zero_copy"
                | "app.start_with_system" => "true/false value",
                "games.watch" => "list of [[games.watch]] tables",
                _ => "value of the shape this key takes",
            },
        ),
    )
}

fn table<'a>(doc: &'a toml_edit::DocumentMut, key: &str) -> Option<&'a toml_edit::Item> {
    doc.get(key)
}

fn get_str(
    doc: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<String>, CommandError> {
    match table(doc, table_key).and_then(|t| t.get(value_key)) {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(str::to_string)
            .map(Some)
            .ok_or_else(|| mistyped(full_key)),
    }
}

fn get_int(
    doc: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<i64>, CommandError> {
    match table(doc, table_key).and_then(|t| t.get(value_key)) {
        None => Ok(None),
        Some(item) => item
            .as_integer()
            .map(Some)
            .ok_or_else(|| mistyped(full_key)),
    }
}

fn get_bool(
    doc: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<bool>, CommandError> {
    match table(doc, table_key).and_then(|t| t.get(value_key)) {
        None => Ok(None),
        Some(item) => item.as_bool().map(Some).ok_or_else(|| mistyped(full_key)),
    }
}

/// An absolute path on either platform the application ships to: a POSIX root, an
/// extended/UNC prefix, or a drive letter. The development host is macOS, where
/// `Path::is_absolute` would reject a Windows path, so both shapes are checked explicitly.
fn is_absolute_either(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some('/') | Some('\\') => true,
        Some(drive) if drive.is_ascii_alphabetic() => {
            matches!((chars.next(), chars.next()), (Some(':'), Some('/' | '\\')))
        }
        _ => false,
    }
}

/// The panel's own band on `[encode] fps` (see `FPS_MIN`). Write-path only.
fn validate_fps(fps: u32) -> Result<(), CommandError> {
    if (FPS_MIN..=FPS_MAX).contains(&fps) {
        Ok(())
    } else {
        Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "encode.fps is {fps}, but the capture rate must be between {FPS_MIN} and \
                 {FPS_MAX}"
            ),
        ))
    }
}

/// `[encode] output_size`, decided by the engine's own parser so the panel refuses exactly what
/// a start refuses. Reused rather than re-implemented: a private copy once trimmed nothing and
/// added a `16..=16384` bound the engine lacks, so `output_size = " 1920x1080"` recorded fine
/// while `get_settings` failed and the panel never rendered.
fn validate_output_size(size: &str) -> Result<(), CommandError> {
    replayed_recorder::parse_output_size(size)
        .map(|_| ())
        .map_err(|err| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!("the config file's encode.output_size is not usable: {err:#}"),
            )
        })
}

/// Why a clips directory is refused, or `None` when it is acceptable.
///
/// `[storage] clips_dir` is user-supplied and handed to `allow_directory(&dir, true)`, which
/// widens the asset protocol's scope, the window's only read channel (`capabilities/default.json`
/// grants no filesystem permission). A filesystem root, a bare share, or a `..` segment would
/// widen it far past the named directory: `clips_dir = "C:\\"` serves the whole drive to the
/// webview.
///
/// So it must be absolute and inside a volume:
///   * no `..` segment: the scope is resolved, not canonicalised, so a path that climbs back
///     out is not the directory it appears to be;
///   * not a root: `/`, `\`, `C:/`, and (behind its prefix) `\\?\C:/`;
///   * for a share, not the bare `\\server\share`; it must name a directory inside one.
///
/// This is not the engine's rule (`resolve_dir` validates nothing), so like the fps band it
/// belongs to the write path alone; on read it would make the panel unrenderable for a file the
/// recorder runs.
fn clips_dir_problem(dir: &str) -> Option<&'static str> {
    // Both platforms separate with these, and which one the string used says nothing about whether
    // it names a whole volume.
    let slashed = dir.replace('\\', "/");
    // `\\?\C:\...` is the extended-length spelling of the same path; the root behind the prefix
    // must not slip past the check below.
    let stripped = slashed.strip_prefix("//?/").unwrap_or(slashed.as_str());
    let segments: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();

    if segments.contains(&"..") {
        return Some("names a parent-directory segment");
    }
    // A UNC path is `//server/share/...`; fewer than three segments names a host or a share, not a
    // directory inside one.
    let is_share = stripped.starts_with("//");
    let is_root = if is_share {
        segments.len() < 3
    } else {
        // `/` leaves no segment at all; `C:/` leaves the drive designator alone.
        segments.is_empty() || (segments.len() == 1 && segments[0].ends_with(':'))
    };
    if is_root {
        return Some(if is_share {
            "a share with no directory inside it"
        } else {
            "a filesystem root"
        });
    }
    None
}

/// `[storage] clips_dir` as the panel will write it: empty (the default), or an absolute path
/// to a directory inside a volume. See `clips_dir_problem`.
fn validate_clips_dir(dir: &str) -> Result<(), CommandError> {
    if dir.is_empty() {
        return Ok(());
    }
    let problem = if is_absolute_either(dir) {
        clips_dir_problem(dir)
    } else {
        Some("a relative path")
    };
    match problem {
        None => Ok(()),
        Some(what) => Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "storage.clips_dir is {dir:?}, but it {what}: it must be empty (the clips \
                 directory under the application data directory) or an absolute path to a \
                 directory below a filesystem root"
            ),
        )),
    }
}

/// Everything the writer refuses. The reader applies only the engine's rules (see
/// `read_effective_settings`), so any value refused here is still readable.
fn validate_edit(edit: &SettingEdit) -> Result<(), CommandError> {
    match edit {
        SettingEdit::ClipsDir(dir) => validate_clips_dir(dir),
        SettingEdit::Fps(fps) => validate_fps(*fps),
        SettingEdit::OutputSize(size) => validate_output_size(size),
        SettingEdit::MicEnabled(_)
        | SettingEdit::AutoRecord(_)
        | SettingEdit::SaveSession(_)
        | SettingEdit::StartWithSystem(_) => Ok(()),
        // The engine's own rule (a nameless entry can never match; one executable twice is one
        // game), so the file written here cannot be one a start refuses.
        SettingEdit::WatchGames(watch) => validate_watch_list(watch).map_err(|err| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!("games.watch was not changed: {err:#}"),
            )
        }),
        SettingEdit::Quality(name) => preset_bitrate(name).map(|_| ()).ok_or_else(|| {
            CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "quality {name:?} is not a preset (low, balanced or high); \
                     encode.bitrate_kbps is set by hand for anything else"
                ),
            )
        }),
        SettingEdit::CaptureMonitor(_) => Ok(()),
        SettingEdit::MicDevice(id) | SettingEdit::OutputDevice(id) => {
            if id.contains(['\n', '\r']) {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("{} is a single-line device id", edit.key()),
                ))
            } else {
                Ok(())
            }
        }
        SettingEdit::ClipHotkey(chord) => replayed_events::hotkey::Hotkey::parse(chord)
            .map(|_| ())
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "hotkeys.clip is {chord:?}, which is not a chord this build can \
                             install ({err}); it wants something like \"Ctrl+F8\""
                    ),
                )
            }),
        SettingEdit::CaptureWindow(title) => {
            if title.contains(['\n', '\r']) {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    "capture.window is a single-line window title",
                ))
            } else {
                Ok(())
            }
        }
        SettingEdit::EncodeVendor(vendor) => {
            if ["auto", "nvenc", "qsv", "amf"].contains(&vendor.as_str()) {
                Ok(())
            } else {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "encode.vendor is {vendor:?}, but it must be \"auto\" (the first vendor \
                         that can encode a frame), \"nvenc\", \"qsv\" or \"amf\""
                    ),
                ))
            }
        }
        SettingEdit::EncodeCodec(codec) => {
            if ["h264", "hevc"].contains(&codec.as_str()) {
                Ok(())
            } else {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("encode.codec is {codec:?}, but it must be \"h264\" or \"hevc\""),
                ))
            }
        }
        SettingEdit::EncodeBitrate(kbps) => {
            if *kbps == 0 || *kbps > MAX_BITRATE_KBPS {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "encode.bitrate_kbps is {kbps}, but the rate must be between 1 and \
                         {MAX_BITRATE_KBPS} kbit/s"
                    ),
                ))
            } else {
                Ok(())
            }
        }
        SettingEdit::EncodeAdaptFps(_) | SettingEdit::EncodeZeroCopy(_) => Ok(()),
        SettingEdit::EncodeRateControl(mode) => replayed_encoder::RateControl::parse(mode)
            .map(|_| ())
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("encode.rate_control is not usable: {err:#}"),
                )
            }),
        SettingEdit::EncodePreset(preset) => {
            // The shape only: which presets exist depends on the encoder that will be selected,
            // and that is not known until the smoke test has run (see `commands::encoder`).
            if preset.is_empty() || known_presets().contains(&preset.as_str()) {
                Ok(())
            } else {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("encode.preset is {preset:?}, which is not an encoder preset"),
                ))
            }
        }
        SettingEdit::AudioBitrate(kbps) => {
            if *kbps == 0 || *kbps > MAX_AUDIO_BITRATE_KBPS {
                Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "audio.bitrate_kbps is {kbps}, but the rate must be between 1 and \
                         {MAX_AUDIO_BITRATE_KBPS} kbit/s"
                    ),
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Every preset name any encoder family takes.
///
/// The union, because which family will be selected is not known until the smoke test has run
/// (`encode.vendor = "auto"`). A preset that belongs to another family is dropped by the
/// encoder with a warning, so this list only has to catch typos.
fn known_presets() -> Vec<&'static str> {
    [
        replayed_encoder::EncoderFamily::Nvenc,
        replayed_encoder::EncoderFamily::Qsv,
        replayed_encoder::EncoderFamily::Amf,
    ]
    .into_iter()
    .flat_map(|family| family.presets().iter().copied())
    .collect()
}

/// Read the effective settings: the file's values where it sets them, the example's where it
/// does not.
///
/// A file that is not TOML is an error. Absent keys fall back per key (recorded in
/// `defaulted`); present but mistyped keys are errors naming the key.
///
/// **It applies the engine's rules and nothing more.** A value the recorder loads must be
/// returned however odd it looks, because the settings panel renders only once this succeeds
/// and is the only surface that could repair the file. The panel's stricter policy lives on the
/// write path (`validate_edit`), as a helpful refusal rather than a lockout.
pub fn read_effective_settings(path: &Path) -> Result<EffectiveSettings, CommandError> {
    let config_exists = path.is_file();
    let text = if config_exists {
        std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?
    } else {
        // The siblings log this path (`StorageConfig::load`, `RecordingConfig::load`,
        // `BackgroundConfig::load`); a quiet one hides that the panel shows example values.
        tracing::info!(
            "no config file at {}: showing the example's values",
            path.display()
        );
        EXAMPLE_CONFIG.to_string()
    };
    config_schema::check(&text).map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "the config file {} could not be read: {err}",
                path.display()
            ),
        )
    })?;
    let doc: toml_edit::DocumentMut = text.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "the config file {} could not be parsed: {err}",
                path.display()
            ),
        )
    })?;
    let example: toml_edit::DocumentMut = EXAMPLE_CONFIG.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("config.example.toml could not be parsed: {err}"),
        )
    })?;

    let mut defaulted = Vec::new();
    // A key from the file when present, else the example's, recording the fallback.
    //
    // **Only the engine's own rules run here.** A mistyped value is an error, and `output_size`
    // goes through the parser a start uses. The panel's stricter policy (the fps band, clips_dir
    // hygiene) is deliberately absent: a reader stricter than a start refuses a file the recorder
    // runs, and a panel that will not render cannot repair anything.
    let clips_dir = match get_str(&doc, "storage", "clips_dir", "storage.clips_dir")? {
        Some(dir) => dir,
        None => {
            if config_exists {
                defaulted.push("storage.clips_dir");
            }
            get_str(&example, "storage", "clips_dir", "storage.clips_dir")?.unwrap_or_default()
        }
    };
    let fps = match get_int(&doc, "encode", "fps", "encode.fps")? {
        Some(raw) => {
            let fps: u32 = raw.try_into().map_err(|_| mistyped("encode.fps"))?;
            fps
        }
        None => {
            if config_exists {
                defaulted.push("encode.fps");
            }
            get_int(&example, "encode", "fps", "encode.fps")?
                .and_then(|raw| u32::try_from(raw).ok())
                .unwrap_or(60)
        }
    };
    let output_size = match get_str(&doc, "encode", "output_size", "encode.output_size")? {
        Some(size) => {
            validate_output_size(&size)?;
            size
        }
        None => {
            if config_exists {
                defaulted.push("encode.output_size");
            }
            get_str(&example, "encode", "output_size", "encode.output_size")?.unwrap_or_default()
        }
    };
    let mic_enabled = match get_bool(&doc, "mic", "enabled", "mic.enabled")? {
        Some(enabled) => enabled,
        None => {
            if config_exists {
                defaulted.push("mic.enabled");
            }
            get_bool(&example, "mic", "enabled", "mic.enabled")?.unwrap_or(false)
        }
    };
    let auto_record = match get_bool(&doc, "games", "auto_record", "games.auto_record")? {
        Some(auto) => auto,
        None => {
            if config_exists {
                defaulted.push("games.auto_record");
            }
            get_bool(&example, "games", "auto_record", "games.auto_record")?.unwrap_or(false)
        }
    };
    let bitrate_kbps = match get_int(&doc, "encode", "bitrate_kbps", "encode.bitrate_kbps")? {
        Some(raw) => u32::try_from(raw).map_err(|_| mistyped("encode.bitrate_kbps"))?,
        None => {
            if config_exists {
                defaulted.push("encode.bitrate_kbps");
            }
            get_int(&example, "encode", "bitrate_kbps", "encode.bitrate_kbps")?
                .and_then(|raw| u32::try_from(raw).ok())
                .unwrap_or(20_000)
        }
    };
    let capture_monitor = match get_int(&doc, "capture", "monitor", "capture.monitor")? {
        Some(raw) => usize::try_from(raw).map_err(|_| mistyped("capture.monitor"))?,
        None => {
            if config_exists {
                defaulted.push("capture.monitor");
            }
            0
        }
    };
    let capture_window = match get_str(&doc, "capture", "window", "capture.window")? {
        Some(window) => window,
        None => {
            if config_exists {
                defaulted.push("capture.window");
            }
            String::new()
        }
    };
    let mic_device = match get_str(&doc, "mic", "device", "mic.device")? {
        Some(id) => id,
        None => {
            if config_exists {
                defaulted.push("mic.device");
            }
            String::new()
        }
    };
    let output_device = match get_str(&doc, "capture", "output_device", "capture.output_device")? {
        Some(id) => id,
        None => {
            if config_exists {
                defaulted.push("capture.output_device");
            }
            String::new()
        }
    };
    let save_session = match get_bool(&doc, "recorder", "save_session", "recorder.save_session")? {
        Some(enabled) => enabled,
        None => {
            if config_exists {
                defaulted.push("recorder.save_session");
            }
            false
        }
    };
    let start_with_system =
        match get_bool(&doc, "app", "start_with_system", "app.start_with_system")? {
            Some(enabled) => enabled,
            None => {
                if config_exists {
                    defaulted.push("app.start_with_system");
                }
                false
            }
        };
    let clip_hotkey = match get_str(&doc, "hotkeys", "clip", "hotkeys.clip")? {
        Some(chord) => chord,
        None => {
            if config_exists {
                defaulted.push("hotkeys.clip");
            }
            DEFAULT_CLIP_HOTKEY.to_string()
        }
    };
    let watch = read_watch(&doc)?;

    Ok(EffectiveSettings {
        clips_dir,
        fps,
        output_size,
        mic_enabled,
        mic_device,
        output_device,
        save_session,
        auto_record,
        bitrate_kbps,
        capture_monitor,
        capture_window,
        watch,
        clip_hotkey,
        start_with_system,
        defaulted,
        config_exists,
    })
}

/// The `[[games.watch]]` list, or the default watch list when the file names none (an absent
/// list deserialises to the default, not to nothing), as the engine does.
///
/// `signal` is optional and defaults to `process`, the engine's own default. A writer that leaves
/// it out therefore means "detect by process name", which is what every title without a local
/// API needs; League's `live_client_api` has to be written explicitly, as the example does.
fn read_watch(doc: &toml_edit::DocumentMut) -> Result<Vec<WatchedGame>, CommandError> {
    let Some(games) = table(doc, "games") else {
        return Ok(default_watch());
    };
    let Some(watch) = games.get("watch") else {
        return Ok(default_watch());
    };
    // `[[games.watch]]` is an array of tables, which `as_array` does not see; a `watch = "..."`
    // string is a mistype either way.
    let tables = watch
        .as_array_of_tables()
        .ok_or_else(|| mistyped("games.watch"))?;
    tables
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string)
                .ok_or_else(|| mistyped("games.watch"))?;
            let exe = entry
                .get("exe")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string)
                .ok_or_else(|| mistyped("games.watch"))?;
            let signal = match entry.get("signal") {
                None => Signal::Process,
                Some(item) => {
                    let text = item.as_str().ok_or_else(|| mistyped("games.watch"))?;
                    Signal::parse(text).ok_or_else(|| mistyped("games.watch"))?
                }
            };
            Ok(WatchedGame { name, exe, signal })
        })
        .collect()
}

/// The lock that serialises the settings file's read-modify-write.
///
/// Two concurrent applies both read the same text and both write it back: the file itself is
/// never corrupted (the temp name carries the pid and a counter), but one call's edit is
/// silently lost, because the second write is based on a read that predates the first write.
/// The window serialises its own buttons behind a `busy` flag, so this is the belt to that
/// braces: nothing in the backend may depend on a flag that lives in the webview.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// The settings write lock, held for as long as the caller needs it.
///
/// A poisoned lock is taken anyway: a panic in one write must not stop the next one, and the
/// file is written atomically either way.
pub(crate) fn settings_write_lock() -> MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// Write edited keys into the file at `path`, preserving everything else.
///
/// Comments, blank lines and the formatting of untouched keys survive: the file is parsed to a
/// document, only the edited values are replaced (missing parent tables are created), and it is
/// written back atomically. All edits are validated before anything is written, so a failure
/// writes nothing. A missing file is created from the example first. An empty edit list is a
/// no-op.
///
/// **This is where the panel's stricter policy belongs** (`validate_edit`): the fps band and
/// clips_dir hygiene may be stricter than the engine because the reader is not, and a value
/// refused here has already rendered in the panel.
///
/// The temp file is named uniquely per call: the window serialises applies behind its `busy`
/// flag, but the runtime dispatches invokes concurrently, and a shared name would let the
/// surviving rename publish a half-overwritten document.
///
/// The whole read-modify-write happens under [`settings_write_lock`], so two applies cannot
/// lose one another's edit.
pub fn write_settings(path: &Path, edits: &[SettingEdit]) -> Result<(), CommandError> {
    let _guard = settings_write_lock();
    write_settings_locked(path, edits)
}

/// [`write_settings`], with [`settings_write_lock`] already held.
///
/// The caller holds the lock so that a read, a write and the read back that follows them are
/// one sequence: the settings panel reports what changed by reading the file twice, and the
/// two reads must bracket the write.
/// Write `[[games.watch]]` as an array of tables, or remove the key when the list is empty.
///
/// A removed key is what the engine reads as the default titles, so "empty" is expressed the way
/// the configuration already expresses it. Everything else in the file is left alone: this is the
/// panel editing a list, not rewriting the configuration.
fn write_watch_games(
    doc: &mut toml_edit::DocumentMut,
    watch: &[WatchedGame],
) -> Result<(), CommandError> {
    match doc.get_mut("games") {
        None => {
            doc["games"] = toml_edit::table();
        }
        Some(item) if item.is_table() => {}
        Some(_) => {
            return Err(CommandError::new(
                ErrorCode::InvalidInput,
                "the config file's [games] is not a table, so nothing was written",
            ))
        }
    }
    let games = doc["games"]
        .as_table_mut()
        .expect("the [games] table was just ensured");
    if watch.is_empty() {
        games.remove("watch");
        return Ok(());
    }
    let mut tables = toml_edit::ArrayOfTables::new();
    for game in watch {
        let mut table = toml_edit::Table::new();
        table["name"] = toml_edit::value(game.name.clone());
        table["exe"] = toml_edit::value(game.exe.clone());
        // Written even when it is the default, so a file states what it means: a reader that does
        // not know the default (a person) sees the signal rather than having to infer it.
        table["signal"] = toml_edit::value(match game.signal {
            Signal::Process => "process",
            Signal::LiveClientApi => "live_client_api",
        });
        tables.push(table);
    }
    games["watch"] = toml_edit::Item::ArrayOfTables(tables);
    Ok(())
}

pub(crate) fn write_settings_locked(
    path: &Path,
    edits: &[SettingEdit],
) -> Result<(), CommandError> {
    for edit in edits {
        validate_edit(edit)?;
    }
    if edits.is_empty() {
        return Ok(());
    }

    let text = if path.is_file() {
        std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?
    } else {
        EXAMPLE_CONFIG.to_string()
    };
    // Refuse to rewrite a layout this build does not understand: an edit would stamp it as current
    // and corrupt whatever the newer version meant.
    config_schema::check(&text).map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the config file {} was not changed: {err}", path.display()),
        )
    })?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "the config file {} could not be parsed: {err}",
                path.display()
            ),
        )
    })?;

    // The layout version the file is brought to *before* it is stamped with it: a file that
    // claims a version its content does not follow is worse than a file that is refused,
    // because the CLI (which does migrate on read) would then skip the step the migration
    // existed for and the settings it carried would be silently lost.
    migrate_to_current(&mut doc, &text)?;

    for edit in edits {
        // "Let the recorder decide" is an absent key, so `zero_copy = None` removes it rather
        // than writing a value: the two are not the same thing, and writing `false` would turn
        // the hardware path off.
        if let SettingEdit::EncodeZeroCopy(None) = edit {
            if let Some(table) = doc
                .get_mut("encode")
                .and_then(|item| item.as_table_like_mut())
            {
                table.remove("zero_copy");
            }
            continue;
        }
        // An array of tables is whole subtables, not a value, so it cannot use the
        // `doc[table][key] = value` path below.
        if let SettingEdit::WatchGames(watch) = edit {
            write_watch_games(&mut doc, watch)?;
            continue;
        }
        let (table_key, value_key, section) = match edit {
            SettingEdit::ClipsDir(_) => ("storage", "clips_dir", "storage"),
            SettingEdit::Fps(_) => ("encode", "fps", "encode"),
            SettingEdit::OutputSize(_) => ("encode", "output_size", "encode"),
            SettingEdit::MicEnabled(_) => ("mic", "enabled", "mic"),
            SettingEdit::MicDevice(_) => ("mic", "device", "mic"),
            SettingEdit::OutputDevice(_) => ("capture", "output_device", "capture"),
            SettingEdit::SaveSession(_) => ("recorder", "save_session", "recorder"),
            SettingEdit::AutoRecord(_) => ("games", "auto_record", "games"),
            SettingEdit::StartWithSystem(_) => ("app", "start_with_system", "app"),
            SettingEdit::ClipHotkey(_) => ("hotkeys", "clip", "hotkeys"),
            // Handled above, before the table lookup: an array of tables has no single value.
            SettingEdit::WatchGames(_) => continue,
            SettingEdit::Quality(_) => ("encode", "bitrate_kbps", "encode"),
            SettingEdit::CaptureMonitor(_) => ("capture", "monitor", "capture"),
            SettingEdit::CaptureWindow(_) => ("capture", "window", "capture"),
            SettingEdit::EncodeVendor(_) => ("encode", "vendor", "encode"),
            SettingEdit::EncodeCodec(_) => ("encode", "codec", "encode"),
            SettingEdit::EncodeBitrate(_) => ("encode", "bitrate_kbps", "encode"),
            SettingEdit::EncodeAdaptFps(_) => ("encode", "adapt_fps", "encode"),
            SettingEdit::EncodeZeroCopy(_) => ("encode", "zero_copy", "encode"),
            SettingEdit::EncodeRateControl(_) => ("encode", "rate_control", "encode"),
            SettingEdit::EncodePreset(_) => ("encode", "preset", "encode"),
            SettingEdit::AudioBitrate(_) => ("audio", "bitrate_kbps", "audio"),
        };
        match doc.get(table_key) {
            None => {
                doc[table_key] = toml_edit::table();
            }
            Some(item) if item.is_table() => {}
            Some(_) => {
                return Err(CommandError::new(
                    ErrorCode::InvalidInput,
                    format!("the config file's [{section}] is not a table, so nothing was written"),
                ));
            }
        }
        let value = match edit {
            SettingEdit::ClipsDir(dir) => toml_edit::value(dir.clone()),
            SettingEdit::Fps(fps) => toml_edit::value(*fps as i64),
            SettingEdit::OutputSize(size) => toml_edit::value(size.clone()),
            SettingEdit::MicEnabled(enabled) => toml_edit::value(*enabled),
            SettingEdit::MicDevice(id) | SettingEdit::OutputDevice(id) => {
                toml_edit::value(id.clone())
            }
            SettingEdit::SaveSession(save) => toml_edit::value(*save),
            SettingEdit::AutoRecord(auto) => toml_edit::value(*auto),
            SettingEdit::StartWithSystem(enabled) => toml_edit::value(*enabled),
            // The parser's own spelling: `ctrl+f8` in the file is stored as `Ctrl+F8`, so what the
            // panel shows and what the installer will register are the same string.
            SettingEdit::ClipHotkey(chord) => toml_edit::value(
                replayed_events::hotkey::Hotkey::parse(chord)
                    .map(|hk| hk.to_string())
                    .unwrap_or_else(|_| chord.trim().to_string()),
            ),
            // Validated above, so the preset exists; the fallback is the balanced rate.
            SettingEdit::Quality(name) => {
                toml_edit::value(preset_bitrate(name).unwrap_or(20_000) as i64)
            }
            SettingEdit::CaptureMonitor(index) => toml_edit::value(*index as i64),
            SettingEdit::CaptureWindow(title) => toml_edit::value(title.clone()),
            SettingEdit::EncodeVendor(vendor) => toml_edit::value(vendor.clone()),
            SettingEdit::EncodeCodec(codec) => toml_edit::value(codec.clone()),
            SettingEdit::EncodeBitrate(kbps) => toml_edit::value(*kbps as i64),
            SettingEdit::EncodeAdaptFps(adapt) => toml_edit::value(*adapt),
            SettingEdit::EncodeZeroCopy(Some(on)) => toml_edit::value(*on),
            // The `None` case was handled above and never reaches here.
            SettingEdit::EncodeZeroCopy(None) => continue,
            // Handled above, before the table lookup: an array of tables has no single value.
            SettingEdit::WatchGames(_) => continue,
            SettingEdit::EncodeRateControl(mode) => toml_edit::value(mode.clone()),
            SettingEdit::EncodePreset(preset) => toml_edit::value(preset.clone()),
            SettingEdit::AudioBitrate(kbps) => toml_edit::value(*kbps as i64),
        };
        doc[table_key][value_key] = value;
    }

    // A uniquely-named temp beside the target, written, flushed to disk, then renamed over it:
    // either the new file is there whole or the old one is.
    //
    // The name carries the pid and a counter so overlapping applies cannot share it. `sync_all`
    // makes that hold against a power loss, not only a crash of this process: without it a rename
    // can publish a zero-length file.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("toml.{}.{seq}.tmp", std::process::id()));
    /// The same failure for both steps, clearing the temp on the way out: a partial file left
    /// behind would be misleading.
    fn could_not_write(path: &Path, tmp: &Path, err: std::io::Error) -> CommandError {
        let _ = std::fs::remove_file(tmp);
        CommandError::new(
            ErrorCode::Io,
            format!("could not write the config file {}: {err}", path.display()),
        )
    }
    let mut file = std::fs::File::create(&tmp).map_err(|e| could_not_write(path, &tmp, e))?;
    std::io::Write::write_all(&mut file, doc.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| could_not_write(path, &tmp, e))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| could_not_write(path, &tmp, e))?;
    // A settings write that leaves no trace is unanswerable in support; this line says one happened
    // and to which keys.
    tracing::info!(
        "settings written to {}: {}",
        path.display(),
        edits
            .iter()
            .map(SettingEdit::key)
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

/// The `[encode]` section and the game-audio bitrate, as the encoder panel sees them.
///
/// **The same rule the rest of this module follows:** the file's values where it sets them, the
/// example's where it does not, and the engine's rules and nothing more. A mistyped key is an
/// error naming it, and an `output_size` the engine's own parser refuses is an error too — but a
/// value the recorder would accept is returned however odd it looks, because the panel renders
/// only once this succeeds and is the one surface that can repair the file.
pub fn read_encoder_settings(path: &Path) -> Result<EncoderSettings, CommandError> {
    let config_exists = path.is_file();
    let text = if config_exists {
        std::fs::read_to_string(path).map_err(|err| {
            CommandError::new(
                ErrorCode::Io,
                format!("could not read the config file {}: {err}", path.display()),
            )
        })?
    } else {
        EXAMPLE_CONFIG.to_string()
    };
    let doc: toml_edit::DocumentMut = text.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "the config file {} could not be parsed: {err}",
                path.display()
            ),
        )
    })?;
    let example: toml_edit::DocumentMut = EXAMPLE_CONFIG.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("config.example.toml could not be parsed: {err}"),
        )
    })?;

    let vendor = string_key(&doc, &example, "encode", "vendor", "encode.vendor")?
        .unwrap_or_else(|| "auto".to_string());
    let codec = string_key(&doc, &example, "encode", "codec", "encode.codec")?
        .unwrap_or_else(|| "h264".to_string());
    let bitrate_kbps = int_key(
        &doc,
        &example,
        "encode",
        "bitrate_kbps",
        "encode.bitrate_kbps",
    )?
    .unwrap_or(20_000);
    let fps = int_key(&doc, &example, "encode", "fps", "encode.fps")?.unwrap_or(60);
    let output_size = string_key(
        &doc,
        &example,
        "encode",
        "output_size",
        "encode.output_size",
    )?
    .unwrap_or_default();
    // The one key the engine validates itself, so the reader keeps refusing it: the panel must
    // not show a value a start would reject.
    if !output_size.trim().is_empty() {
        validate_output_size(&output_size)?;
    }
    let adapt_fps =
        bool_key(&doc, &example, "encode", "adapt_fps", "encode.adapt_fps")?.unwrap_or(true);
    // Absent means "let the recorder decide", which is what an absent key says. There is no
    // example value to fall back to: the example leaves the key out on purpose.
    let zero_copy = get_bool(&doc, "encode", "zero_copy", "encode.zero_copy")?;
    let rate_control = string_key(
        &doc,
        &example,
        "encode",
        "rate_control",
        "encode.rate_control",
    )?
    .unwrap_or_else(|| replayed_encoder::RateControl::Cbr.as_str().to_string());
    let preset =
        string_key(&doc, &example, "encode", "preset", "encode.preset")?.unwrap_or_default();
    let audio_bitrate_kbps = int_key(
        &doc,
        &example,
        "audio",
        "bitrate_kbps",
        "audio.bitrate_kbps",
    )?
    .unwrap_or(replayed_encoder::DEFAULT_AUDIO_BITRATE_KBPS);

    Ok(EncoderSettings {
        vendor,
        codec,
        bitrate_kbps,
        fps,
        output_size,
        adapt_fps,
        zero_copy,
        rate_control,
        preset,
        audio_bitrate_kbps,
    })
}

/// One string key: the file's value, else the example's, else nothing.
fn string_key(
    doc: &toml_edit::DocumentMut,
    example: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<String>, CommandError> {
    Ok(get_str(doc, table_key, value_key, full_key)?
        .or(get_str(example, table_key, value_key, full_key)?))
}

/// One integer key: the file's value, else the example's, else nothing.
fn int_key(
    doc: &toml_edit::DocumentMut,
    example: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<u32>, CommandError> {
    let raw = get_int(doc, table_key, value_key, full_key)?
        .or(get_int(example, table_key, value_key, full_key)?);
    match raw {
        Some(raw) => Ok(Some(u32::try_from(raw).map_err(|_| mistyped(full_key))?)),
        None => Ok(None),
    }
}

/// One boolean key: the file's value, else the example's, else nothing.
fn bool_key(
    doc: &toml_edit::DocumentMut,
    example: &toml_edit::DocumentMut,
    table_key: &str,
    value_key: &'static str,
    full_key: &'static str,
) -> Result<Option<bool>, CommandError> {
    Ok(get_bool(doc, table_key, value_key, full_key)?
        .or(get_bool(example, table_key, value_key, full_key)?))
}

/// Write the encoder panel's settings into the file.
///
/// Every key is written, in one write: the panel sends the whole configuration, so the file ends
/// up saying exactly what the panel shows. The write is atomic and validated first, so a
/// rejected setting leaves the file as it was.
pub fn write_encoder_settings(path: &Path, settings: &EncoderSettings) -> Result<(), CommandError> {
    write_settings(path, &encoder_edits(settings))
}

/// [`write_encoder_settings`], with [`settings_write_lock`] already held.
///
/// The caller holds the lock so that the write and the read back that follows it are one
/// sequence: the panel is told what was stored, and that answer must come from the file the
/// write produced.
pub(crate) fn write_encoder_settings_locked(
    path: &Path,
    settings: &EncoderSettings,
) -> Result<(), CommandError> {
    write_settings_locked(path, &encoder_edits(settings))
}

/// The edits that carry the encoder panel's settings into the file.
fn encoder_edits(settings: &EncoderSettings) -> Vec<SettingEdit> {
    vec![
        SettingEdit::EncodeVendor(settings.vendor.clone()),
        SettingEdit::EncodeCodec(settings.codec.clone()),
        SettingEdit::EncodeBitrate(settings.bitrate_kbps),
        SettingEdit::Fps(settings.fps),
        SettingEdit::OutputSize(settings.output_size.clone()),
        SettingEdit::EncodeAdaptFps(settings.adapt_fps),
        SettingEdit::EncodeZeroCopy(settings.zero_copy),
        SettingEdit::EncodeRateControl(settings.rate_control.clone()),
        SettingEdit::EncodePreset(settings.preset.clone()),
        SettingEdit::AudioBitrate(settings.audio_bitrate_kbps),
    ]
}

/// Bring the document to the layout version this build writes, and stamp it with that version.
fn migrate_to_current(doc: &mut toml_edit::DocumentMut, text: &str) -> Result<(), CommandError> {
    migrate_document(
        doc,
        text,
        config_schema::CURRENT_SCHEMA_VERSION,
        config_schema::MIGRATIONS,
    )
}

/// The document, brought to `current` by `steps` and stamped with it.
///
/// The stamp is the last thing that happens, and only after the file has actually been brought
/// to the version it is stamped with. A file that claims a version its content does not follow
/// is worse than a file that is refused: the CLI migrates on read, so it would take the stamp
/// at its word, skip the step the migration existed for, and lose exactly the settings that
/// migration was written to carry.
///
/// Two rules keep that from happening:
///
/// * one migration per version step. A build that stamps a version it has no migration for
///   refuses to write at all, loudly, rather than writing a file that lies about its layout;
/// * the migration is applied to the document key by key, so the comments and the formatting of
///   everything the migration did not touch survive.
///
/// The migration steps work on a `toml::Table` (`config_schema::Migration`) and the document is
/// a `toml_edit::DocumentMut`: the two value types have no conversion between them, so a
/// migrated value is turned into a document item by rendering it as TOML and parsing it back.
fn migrate_document(
    doc: &mut toml_edit::DocumentMut,
    text: &str,
    current: u32,
    steps: &[config_schema::Migration],
) -> Result<(), CommandError> {
    if steps.len() as u32 + 1 != current {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "this build writes schema version {current} but has {} migration(s) for it, so \
                 the config file was not changed",
                steps.len()
            ),
        ));
    }
    let mut table: toml::Table = text.parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the config file could not be parsed: {err}"),
        )
    })?;
    let before = table.clone();
    config_schema::migrate_with(&mut table, current, steps).map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the config file was not changed: {err}"),
        )
    })?;
    if !same_content(&before, &table) {
        merge_migrated(doc.as_table_mut(), &before, &table)?;
    }
    doc[config_schema::SCHEMA_VERSION_KEY] = toml_edit::value(i64::from(current));
    Ok(())
}

/// Whether two tables hold the same content, ignoring the version stamp.
///
/// The stamp is the version's own business: comparing it would say every file changed.
fn same_content(a: &toml::Table, b: &toml::Table) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.remove(config_schema::SCHEMA_VERSION_KEY);
    b.remove(config_schema::SCHEMA_VERSION_KEY);
    a == b
}

/// The migrated table, merged into the document.
///
/// Only what the migration changed is touched: a key it added is added, a key it changed is
/// replaced, a key it removed is removed, and everything else keeps its comments and its
/// formatting.
fn merge_migrated(
    table: &mut dyn toml_edit::TableLike,
    before: &toml::Table,
    after: &toml::Table,
) -> Result<(), CommandError> {
    // A key the migration dropped goes with it: the document has to end up holding what the
    // migrated table holds, not what it held plus the new shape.
    for key in before.keys() {
        if after.contains_key(key) {
            continue;
        }
        table.remove(key);
    }
    for (key, value) in after {
        if key == config_schema::SCHEMA_VERSION_KEY {
            continue;
        }
        let existing = table.get(key).cloned();
        match (existing, before.get(key), value) {
            (
                Some(toml_edit::Item::Table(mut existing)),
                Some(toml::Value::Table(old)),
                toml::Value::Table(new),
            ) => {
                merge_migrated(&mut existing, old, new)?;
            }
            (Some(existing), _, value) => {
                if item_value(&existing).as_ref() != Some(value) {
                    table.insert(key, item_from_value(value)?);
                }
            }
            (None, _, value) => {
                table.insert(key, item_from_value(value)?);
            }
        }
    }
    Ok(())
}

/// The value a document item holds, as the `toml` crate's own value type.
///
/// The two crates have no conversion between their values, so the item is rendered as TOML and
/// read back. The comparison the merge needs is "did this value change", not "is this the same
/// formatting".
fn item_value(item: &toml_edit::Item) -> Option<toml::Value> {
    toml::from_str::<toml::Value>(&item.to_string()).ok()
}

/// A migrated value, as a document item.
fn item_from_value(value: &toml::Value) -> Result<toml_edit::Item, CommandError> {
    value.to_string().parse().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the migrated value could not be written back: {err}"),
        )
    })
}

/// The application data directory, by the same rule the CLI uses.
///
/// Duplicated on purpose: the two binaries must agree on this path or the shell would show an
/// empty library while the recorder writes clips elsewhere. Windows uses `%LOCALAPPDATA%`; the
/// macOS development host uses `~/Library/Application Support` via `XDG_DATA_HOME` if set, and
/// the current directory as the last resort, as the CLI does.
pub fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("replayed");
    migrate_legacy_data_dir(&base, &dir);
    dir
}

/// Move the data directory over from the name the project had before the rename.
///
/// The product was renamed; the directory holding the index, the clips, the sessions and the
/// settings was not, and a user who had been running the older build would have found an
/// empty library.
///
/// The new directory usually **already exists** by the time this runs, and that is the whole
/// subtlety: the installer puts the application in `%LOCALAPPDATA%\replayed`, and the data
/// directory is that same path, so the installer creates the directory before the application
/// has ever started. A rule of "the old directory is adopted only if the new one is absent"
/// therefore never fires on a real upgrade, and the user gets an empty library with their old
/// one stranded next to it.
///
/// What decides it is not the directory but the *data in it*. If the new directory already
/// holds an index or a configuration of its own, this build has been used, and the old
/// directory is left exactly where it is: merging two libraries is not something to do to
/// somebody's clips without being asked.
///
/// Otherwise each entry of the old directory is moved across one at a time, skipping anything
/// whose destination already exists, and the index is renamed with the project. Nothing is
/// copied, so a failure part way through loses nothing: whatever did not move is still in the
/// old directory, which is left in place, and the next start tries again.
fn migrate_legacy_data_dir(base: &std::path::Path, dir: &std::path::Path) {
    use std::ffi::{OsStr, OsString};

    // The name before the rename, spelled out rather than derived: it must stay this
    // literal forever, because it is the name of a directory that exists on disk.
    const LEGACY: &str = "localplay";
    // The index was renamed with the project, so the old file has to be renamed on the way.
    const LEGACY_INDEX: &str = "localplay.db";
    const INDEX: &str = "replayed.db";

    let legacy = base.join(LEGACY);
    if !legacy.is_dir() {
        return;
    }
    if dir.join(INDEX).exists() || dir.join("config.toml").exists() {
        // The new directory has data of its own. It is not ours to merge into.
        return;
    }
    let entries = match std::fs::read_dir(&legacy) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                "the old data directory at `{}` could not be read: {err}",
                legacy.display()
            );
            return;
        }
    };
    let mut moved: Vec<String> = Vec::new();
    let mut left: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let target_name: OsString = if name.as_os_str() == OsStr::new(LEGACY_INDEX) {
            OsString::from(INDEX)
        } else {
            name.clone()
        };
        let target = dir.join(&target_name);
        if target.exists() {
            left.push(name.to_string_lossy().into_owned());
            continue;
        }
        // The new directory may not exist yet, when the application is run from a build
        // rather than from the installer.
        let _ = std::fs::create_dir_all(dir);
        match std::fs::rename(entry.path(), &target) {
            Ok(()) => moved.push(name.to_string_lossy().into_owned()),
            Err(err) => {
                tracing::warn!(
                    "`{}` could not be moved to `{}`: {err}",
                    name.to_string_lossy(),
                    target.display()
                );
                left.push(name.to_string_lossy().into_owned());
            }
        }
    }
    // Only an empty directory is removed, and `remove_dir` refuses anything else — so this
    // cannot delete a leftover the user still needs.
    let _ = std::fs::remove_dir(&legacy);
    if !moved.is_empty() {
        tracing::info!(
            "the data from the old `{}` directory was moved to `{}`: {}",
            legacy.display(),
            dir.display(),
            moved.join(", ")
        );
    }
    if !left.is_empty() {
        tracing::warn!(
            "these are still in the old `{}` directory and were not moved: {}",
            legacy.display(),
            left.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A base directory holding the pre-rename data directory beside the new one, with the new
    /// one **already there** — which is what the installer leaves behind, because it puts the
    /// application in `%LOCALAPPDATA%\replayed` and that is also where the data lives.
    fn legacy_case() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let base = tmp.path().to_path_buf();
        let legacy = base.join("localplay");
        let dir = base.join("replayed");
        std::fs::create_dir_all(&legacy).expect("the legacy directory");
        std::fs::create_dir_all(&dir).expect("the new directory, as the installer leaves it");
        (tmp, base, dir)
    }

    #[test]
    fn the_legacy_data_is_adopted_even_when_the_new_directory_already_exists() {
        // The regression this whole function exists for: the installer creates the new directory
        // before the application has ever run, so a rule of "adopt the old one only if the new one
        // is absent" never fires on a real upgrade, and the user's clips and settings stay behind
        // in the old directory while the application reads and writes the new one.
        let (_tmp, base, dir) = legacy_case();
        let legacy = base.join("localplay");
        std::fs::write(legacy.join("config.toml"), b"[storage]\n").expect("the old settings");
        std::fs::write(legacy.join("localplay.db"), b"index").expect("the old index");
        std::fs::create_dir_all(legacy.join("clips")).expect("the old clips");

        migrate_legacy_data_dir(&base, &dir);

        assert!(
            dir.join("config.toml").is_file(),
            "the settings moved across"
        );
        assert!(
            dir.join("replayed.db").is_file(),
            "the index arrived under its new name"
        );
        assert!(dir.join("clips").is_dir(), "the clips moved across");
        assert!(
            !legacy.join("config.toml").exists(),
            "the old settings are gone"
        );
        assert!(
            !legacy.join("localplay.db").exists(),
            "the old index is gone"
        );
    }

    #[test]
    fn a_new_directory_with_its_own_data_is_left_alone() {
        // The new build has been used and has settings of its own. Merging somebody's library
        // into it without being asked is not something to do.
        let (_tmp, base, dir) = legacy_case();
        let legacy = base.join("localplay");
        std::fs::write(legacy.join("config.toml"), b"old").expect("the old settings");
        std::fs::write(dir.join("config.toml"), b"new").expect("the new settings");

        migrate_legacy_data_dir(&base, &dir);

        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("the new settings"),
            "new",
            "the new directory's own settings are untouched"
        );
        assert!(
            legacy.join("config.toml").is_file(),
            "the old settings are still where they were"
        );
    }

    #[test]
    fn an_entry_whose_target_already_exists_is_skipped_and_the_rest_moves() {
        // One file's name is taken; the rest still moves, and the skipped file is left exactly
        // where it was rather than being overwritten.
        let (_tmp, base, dir) = legacy_case();
        let legacy = base.join("localplay");
        std::fs::write(legacy.join("notes.txt"), b"old").expect("the old file");
        std::fs::write(dir.join("notes.txt"), b"new").expect("the new file");
        std::fs::write(legacy.join("localplay.db"), b"index").expect("the old index");

        migrate_legacy_data_dir(&base, &dir);

        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).expect("the new file"),
            "new",
            "an existing file is not overwritten"
        );
        assert!(dir.join("replayed.db").is_file(), "the rest moved across");
        assert!(
            legacy.join("notes.txt").is_file(),
            "the skipped file is still where it was"
        );
    }

    #[test]
    fn reads_the_storage_section_of_the_example_config() {
        // The example config is the fallback for a fresh install, so this is exactly what a first run
        // gets. It also pins this module to the repository's real file: if `[storage]` is renamed or
        // re-typed there, this fails.
        let cfg = StorageConfig::example().expect("config.example.toml must stay readable");
        assert_eq!(
            cfg.clips_dir, "",
            "the example leaves the clips directory to the app dir"
        );
        assert_eq!(
            cfg.max_total_bytes, 53_687_091_200,
            "50 GiB, as spec §10 documents"
        );
        assert_eq!(cfg.max_age_days, 7);
    }

    #[test]
    fn accepts_a_file_that_only_carries_the_storage_section() {
        // The other sections are the recorder's business; their absence must not stop the review UI
        // from opening.
        let cfg = StorageConfig::from_toml(
            r#"
            [storage]
            clips_dir = "/tmp/clips"
            max_total_bytes = 1024
            max_age_days = 2
            "#,
        )
        .unwrap();
        assert_eq!(cfg.clips_dir, "/tmp/clips");
        assert_eq!(cfg.max_total_bytes, 1024);
        assert_eq!(cfg.max_age_days, 2);
    }

    #[test]
    fn a_missing_storage_section_is_an_error_not_a_silent_default() {
        // Defaulting `max_total_bytes` to 0 would make every clip "over the cap", a far worse failure
        // than refusing to start.
        let err = StorageConfig::from_toml("[buffer]\npre_seconds = 30\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("storage"),
            "the message must name the section: {}",
            err.message
        );
    }

    #[test]
    fn a_storage_key_the_file_omits_falls_back_to_the_examples_value() {
        // The CLI's own `StorageSection` requires all three keys; the desktop's reader does not,
        // and the asymmetry runs one way only: the desktop accepts everything the CLI accepts.
        // The fallback is the example's value and never zero — a zero cap would make every clip
        // "over the cap", which is a far worse failure than a missing key.
        let cfg = StorageConfig::from_toml("[storage]\nclips_dir = \"\"\n").unwrap();

        assert_eq!(cfg.clips_dir, "");
        assert_eq!(cfg.max_total_bytes, 53_687_091_200, "the example's cap");
        assert_eq!(cfg.max_age_days, 7, "the example's age");

        // A key that is *mistyped* is still an error: a typo'd cap must not read as "not set".
        let err = StorageConfig::from_toml("[storage]\nmax_total_bytes = \"lots\"\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn the_keys_a_file_omits_are_named() {
        // The shell puts these in a warning: a user whose cap is not the one they set should
        // hear about it rather than find out when a clip is deleted.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 2048\n",
        )
        .unwrap();
        let (storage, warning) = StorageConfig::load_or_example(&path);
        assert_eq!(
            storage.max_total_bytes, 2048,
            "the file's own value is used"
        );
        let warning = warning.expect("the key that fell back is a warning");
        assert!(warning.contains("max_age_days"), "{warning}");
        assert!(!warning.contains("max_total_bytes"), "{warning}");

        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 2048\n\
             max_age_days = 1\n",
        )
        .unwrap();
        let (_storage, warning) = StorageConfig::load_or_example(&path);
        assert!(
            warning.is_none(),
            "a file that sets everything says nothing"
        );
    }

    #[test]
    fn a_config_that_cannot_be_read_starts_the_shell_with_the_examples_values() {
        // The CLI refuses to start on a bad file; the desktop has a window to show the problem
        // in, so it starts with the example's values and says so.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage\nclips_dir = ").unwrap();

        let (storage, warning) = StorageConfig::load_or_example(&path);

        assert_eq!(storage, StorageConfig::fallback(), "the example's values");
        let warning = warning.expect("a file that could not be read is a warning");
        assert!(warning.contains("config file"), "{warning}");
    }

    #[test]
    fn a_malformed_config_file_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage\nclips_dir = ").unwrap();

        let err = StorageConfig::load(&path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[storage]"), "got: {}", err.message);

        // One file, one verdict, whichever part of the shell reads it.
        assert!(BackgroundConfig::load(&path).is_err());
    }

    #[test]
    fn the_recording_settings_come_from_the_same_file_the_review_pane_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[buffer]\npre_seconds = 12\npost_seconds = 3\nsegment_time = 1\n\
             scratch_dir = \"\"\n\n\
             [encode]\nvendor = \"nvenc\"\ncodec = \"hevc\"\nbitrate_kbps = 20000\n\
             fps = 60\noutput_size = \"1920x1080\"\n\n\
             [storage]\nclips_dir = \"\"\nmax_total_bytes = 2048\nmax_age_days = 1\n",
        )
        .unwrap();

        let recording = RecordingConfig::load(&path).unwrap();
        assert_eq!(recording.buffer.pre_seconds, 12);
        assert_eq!(recording.encode.vendor, "nvenc");
        assert_eq!(recording.storage.max_total_bytes, 2048);

        // The same file, read by the review pane's own type: both readers agree about where clips go.
        let review = StorageConfig::load(&path).unwrap();
        assert_eq!(review.max_total_bytes, recording.storage.max_total_bytes);
    }

    #[test]
    fn a_file_without_the_capture_sections_fails_recording_but_not_the_shell() {
        // `a_capture_only_config_file_does_not_stop_the_shell_from_opening` (lib.rs) is the other
        // half: the review pane opens on this file, and only a recording refuses it, naming what is
        // missing.
        let err = RecordingConfig::from_toml(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\nmax_age_days = 1\n",
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("[buffer]"),
            "the message must name the sections: {}",
            err.message
        );
    }

    #[test]
    fn the_app_data_directory_ends_in_replayed() {
        // Whatever the host provides, the two binaries must land in the same subdirectory.
        assert_eq!(app_data_dir().file_name().unwrap(), "replayed");
    }

    // -- the [hotkeys] / [app] half -------------------------------------------------------

    #[test]
    fn every_reader_falls_back_to_the_example_when_there_is_no_file() {
        // A fresh install has no `config.toml`: a first recording behaves as `config.example.toml`
        // documents (as the CLI does), the hotkey is Ctrl+F8 and start-with-system is off.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        assert_eq!(
            StorageConfig::load(&path).unwrap(),
            StorageConfig::example().unwrap()
        );

        let cfg = BackgroundConfig::load(&path).unwrap();
        assert_eq!(cfg, BackgroundConfig::example().unwrap());
        assert_eq!(cfg.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert!(
            !cfg.start_with_system,
            "start_with_system is off unless it is asked for"
        );

        let rec = RecordingConfig::load(&path).unwrap();
        assert_eq!(rec.buffer.pre_seconds, 30);
        assert_eq!(rec.buffer.post_seconds, 5);
        assert_eq!(rec.encode.fps, 60, "the example's capture rate");
        assert_eq!(rec.encode.codec, "h264");
        assert_eq!(rec.storage.max_total_bytes, 53_687_091_200);
    }

    #[test]
    fn a_file_without_the_hotkey_section_still_gets_the_default_chord() {
        // The CLI would reject this file; the window must not. The chord is the documented default, not
        // an empty string that would fail to parse later.
        let cfg = BackgroundConfig::from_toml(
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\nmax_age_days = 1\n",
        )
        .unwrap();

        assert_eq!(cfg.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert!(!cfg.start_with_system);
    }

    #[test]
    fn reads_the_hotkey_and_the_app_section_when_the_file_sets_them() {
        let cfg = BackgroundConfig::from_toml(
            "[hotkeys]\nclip = \"Alt+F9\"\n\n[app]\nstart_with_system = true\n",
        )
        .unwrap();

        assert_eq!(
            cfg.clip_hotkey, "Alt+F9",
            "the user's chord is used verbatim"
        );
        assert!(cfg.start_with_system);
    }

    #[test]
    fn a_malformed_hotkey_or_app_section_is_reported_rather_than_defaulted() {
        // `clip = 8` is a typo a user will make. Falling back to Ctrl+F8 would leave them pressing a
        // key that is not the one they configured.
        for (toml, section) in [
            ("[hotkeys]\nclip = 8\n", "[hotkeys]"),
            ("[app]\nstart_with_system = \"yes\"\n", "start_with_system"),
        ] {
            let err = BackgroundConfig::from_toml(toml).unwrap_err();

            assert_eq!(err.code, ErrorCode::InvalidInput);
            assert!(
                err.message.contains(section),
                "the message must name the section: {}",
                err.message
            );
        }
    }

    // -- the [mic] / [games] half --------------------------------------------------------

    #[test]
    fn the_engine_options_default_to_off_and_unwatched() {
        // Absent sections behave like the engine's defaults: no microphone, and no watcher at all
        // (`watching_games: false`).
        let opts = EngineOptionsConfig::from_toml("[storage]\nclips_dir = \"\"\n").unwrap();

        assert!(!opts.mic.enabled);
        assert!(!opts.games.auto_record);
        assert_eq!(opts.games.watch, default_watch());
    }

    #[test]
    fn the_engine_options_come_from_the_file_when_it_sets_them() {
        let opts = EngineOptionsConfig::from_toml(
            "[mic]\nenabled = true\n\n[games]\nauto_record = true\npoll_ms = 1000\n",
        )
        .unwrap();

        assert!(opts.mic.enabled);
        assert!(opts.games.auto_record);
        // No watch list given: the six documented defaults, which is what the engine
        // watches then too.
        assert_eq!(opts.games.watch, default_watch());
    }

    #[test]
    fn the_events_section_defaults_to_the_example_and_comes_from_the_file_when_set() {
        // Absent: League polling on and the GSI listener on its default port, as the CLI's example
        // config has it, so a desktop user with an old config file still gets events.
        let absent = EngineOptionsConfig::from_toml("[storage]\nclips_dir = \"\"\n").unwrap();
        assert_eq!(absent.events, EventsSection::default());
        assert!(absent.events.lol_poll_enabled);

        let set =
            EngineOptionsConfig::from_toml("[events]\nlol_poll_enabled = false\ngsi_port = 0\n")
                .unwrap();
        assert!(!set.events.lol_poll_enabled);
        assert_eq!(set.events.gsi_port, 0);
    }

    #[test]
    fn a_gsi_port_the_cli_would_refuse_is_refused_here_too() {
        // The CLI refuses to start when `events.gsi_port` is neither 0 nor in the ephemeral
        // range's complement; the desktop writes the GSI cfg the game reads, so a port below
        // 1024 would point the game at whatever else is listening there.
        for bad in [80, 443, 1023] {
            let err = EngineOptionsConfig::from_toml(&format!(
                "[events]\nlol_poll_enabled = true\ngsi_port = {bad}\n"
            ))
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "port {bad}");
            assert!(
                err.message.contains("events.gsi_port"),
                "got: {}",
                err.message
            );
        }

        // 0 is the documented way to turn the listener off, and a real port is fine.
        for good in [0, 1024, 45671] {
            assert!(
                EngineOptionsConfig::from_toml(&format!(
                    "[events]\nlol_poll_enabled = true\ngsi_port = {good}\n"
                ))
                .is_ok(),
                "port {good}"
            );
        }
    }

    #[test]
    fn a_mistyped_engine_section_is_an_error_naming_it() {
        let err = EngineOptionsConfig::from_toml("[games]\nauto_record = \"yes\"\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[games]"), "got: {}", err.message);
    }

    // -- the effective-settings reader --------------------------------------------------

    fn settings_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# a comment the writer must keep\n\
             [storage]\nclips_dir = \"D:\\\\clips\"\n\n\
             [encode]\nfps = 30\nbitrate_kbps = 8000\noutput_size = \"1280x720\"\n\n\
             [capture]\nmonitor = 0\nwindow = \"\"\noutput_device = \"\"\n\n\
             [mic]\nenabled = true\ndevice = \"\"\n\n\
             [recorder]\nsave_session = false\n\n\
             [games]\nauto_record = true\n\
             [[games.watch]]\nname = \"Dota 2\"\nexe = \"dota2.exe\"\n\
             [hotkeys]\nclip = \"Ctrl+F8\"\n\
             [app]\nstart_with_system = false\n",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn the_reader_reports_the_files_values_and_the_default_watch_list() {
        let (_dir, path) = settings_file();
        let settings = read_effective_settings(&path).unwrap();

        assert_eq!(settings.clips_dir, "D:\\clips");
        assert_eq!(settings.fps, 30);
        assert_eq!(settings.output_size, "1280x720");
        assert!(settings.mic_enabled);
        assert!(settings.auto_record);
        assert_eq!(settings.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert_eq!(
            settings.watch,
            vec![WatchedGame {
                name: "Dota 2".into(),
                exe: "dota2.exe".into(),
                signal: Signal::Process,
            }],
            "the file's one entry is read with its executable and signal"
        );
        assert!(settings.defaulted.is_empty(), "the file sets everything");
        assert!(settings.config_exists);
    }

    #[test]
    fn the_clip_hotkey_is_read_from_the_file_and_defaulted_when_it_is_not_there() {
        let (_dir, path) = settings_file();
        assert_eq!(
            read_effective_settings(&path).unwrap().clip_hotkey,
            "Ctrl+F8"
        );

        // A file that sets it, in any case, is read verbatim; the writer is what normalises.
        write_settings(&path, &[SettingEdit::ClipHotkey("alt+f9".into())]).unwrap();
        assert_eq!(
            read_effective_settings(&path).unwrap().clip_hotkey,
            "Alt+F9"
        );

        // A file with no [hotkeys] at all falls back to the documented default and says so.
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("config.toml");
        std::fs::write(&bare, "[storage]\nclips_dir = \"\"\n").unwrap();
        let settings = read_effective_settings(&bare).unwrap();
        assert_eq!(settings.clip_hotkey, DEFAULT_CLIP_HOTKEY);
        assert!(settings.defaulted.contains(&"hotkeys.clip"));
    }

    #[test]
    fn start_with_system_round_trips_through_the_writer() {
        let (_dir, path) = settings_file();
        assert!(!read_effective_settings(&path).unwrap().start_with_system);

        write_settings(&path, &[SettingEdit::StartWithSystem(true)]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[app]"), "{text}");
        assert!(text.contains("start_with_system = true"), "{text}");
        assert!(
            read_effective_settings(&path).unwrap().start_with_system,
            "the file the writer produced is the file the reader sees"
        );

        write_settings(&path, &[SettingEdit::StartWithSystem(false)]).unwrap();
        assert!(!read_effective_settings(&path).unwrap().start_with_system);
    }

    #[test]
    fn absent_keys_fall_back_to_the_example_and_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();

        let settings = read_effective_settings(&path).unwrap();

        assert_eq!(settings.clips_dir, "");
        assert_eq!(settings.fps, 60, "the example's capture rate");
        assert!(settings.defaulted.contains(&"encode.fps"));
        assert!(settings.defaulted.contains(&"mic.enabled"));
        assert!(settings.defaulted.contains(&"games.auto_record"));
        assert!(settings.defaulted.contains(&"app.start_with_system"));
        assert!(!settings.defaulted.contains(&"storage.clips_dir"));
        assert_eq!(settings.watch, default_watch());
    }

    #[test]
    fn quality_presets_round_trip_through_the_bitrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (name, kbps) in QUALITY_PRESETS {
            write_settings(&path, &[SettingEdit::Quality(name.to_string())]).unwrap();
            let settings = read_effective_settings(&path).unwrap();
            assert_eq!(settings.bitrate_kbps, kbps);
            assert_eq!(quality_for_bitrate(settings.bitrate_kbps), name);
        }
        assert_eq!(quality_for_bitrate(12_345), "custom");
    }

    #[test]
    fn an_unknown_preset_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for bad in ["custom", "ultra", ""] {
            let err = write_settings(&path, &[SettingEdit::Quality(bad.to_string())]).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput);
        }
        assert!(!path.exists(), "a refused edit must not create the file");
    }

    #[test]
    fn capture_target_is_written_read_and_defaults_to_the_primary_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();
        let before = read_effective_settings(&path).unwrap();
        assert_eq!(
            (before.capture_monitor, before.capture_window.as_str()),
            (0, "")
        );
        assert!(before.defaulted.contains(&"capture.monitor"));

        write_settings(
            &path,
            &[
                SettingEdit::CaptureMonitor(1),
                SettingEdit::CaptureWindow("League".into()),
            ],
        )
        .unwrap();
        let after = read_effective_settings(&path).unwrap();
        assert_eq!(
            (after.capture_monitor, after.capture_window.as_str()),
            (1, "League")
        );
        assert!(!after.defaulted.contains(&"capture.monitor"));
    }

    #[test]
    fn the_recorders_parser_reads_the_capture_the_panel_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // No file yet: the writer starts from the example, so this is a complete config.
        write_settings(
            &path,
            &[
                SettingEdit::CaptureMonitor(1),
                SettingEdit::CaptureWindow("League".into()),
            ],
        )
        .unwrap();
        let rec = RecordingConfig::load(&path).unwrap();
        assert_eq!(
            (rec.capture.monitor, rec.capture.window.as_str()),
            (1, "League")
        );
        let default = RecordingConfig::example().unwrap();
        assert!(
            default.capture.target().is_default(),
            "the example records the primary monitor"
        );
    }

    #[test]
    fn the_chosen_audio_devices_are_written_and_read_by_both_parsers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_settings(
            &path,
            &[
                SettingEdit::MicDevice("{0.0.1.00000000}.{mic}".into()),
                SettingEdit::OutputDevice("{0.0.0.00000000}.{speakers}".into()),
            ],
        )
        .unwrap();
        let settings = read_effective_settings(&path).unwrap();
        assert_eq!(settings.mic_device, "{0.0.1.00000000}.{mic}");
        assert_eq!(settings.output_device, "{0.0.0.00000000}.{speakers}");
        let rec = RecordingConfig::load(&path).unwrap();
        assert_eq!(rec.capture.output_device, "{0.0.0.00000000}.{speakers}");
        let engine = EngineOptionsConfig::load(&path).unwrap();
        assert_eq!(engine.mic.device, "{0.0.1.00000000}.{mic}");

        // Empty goes back to the Windows default.
        write_settings(&path, &[SettingEdit::MicDevice(String::new())]).unwrap();
        assert_eq!(read_effective_settings(&path).unwrap().mic_device, "");
    }

    #[test]
    fn save_session_is_written_and_read_by_both_parsers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_settings(&path, &[SettingEdit::SaveSession(true)]).unwrap();
        assert!(read_effective_settings(&path).unwrap().save_session);
        assert!(
            EngineOptionsConfig::load(&path)
                .unwrap()
                .recorder
                .save_session
        );
        write_settings(&path, &[SettingEdit::SaveSession(false)]).unwrap();
        assert!(!read_effective_settings(&path).unwrap().save_session);
        assert!(
            !EngineOptionsConfig::example()
                .unwrap()
                .recorder
                .save_session
        );
    }

    #[test]
    fn a_mistyped_capture_monitor_is_an_error_naming_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[capture]\nmonitor = -1\n").unwrap();
        let err = read_effective_settings(&path).unwrap_err();
        assert!(
            err.message.contains("capture.monitor"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn a_missing_file_reads_the_example_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let settings = read_effective_settings(&dir.path().join("config.toml")).unwrap();

        assert!(!settings.config_exists);
        assert!(
            settings.defaulted.is_empty(),
            "no file, no per-key fallback to name"
        );
        assert_eq!(settings.fps, 60);
        assert!(!settings.auto_record);
    }

    #[test]
    fn a_mistyped_key_is_an_error_naming_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[encode]\nfps = \"fast\"\n").unwrap();

        let err = read_effective_settings(&path).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("encode.fps"), "got: {}", err.message);
    }

    #[test]
    fn the_reader_accepts_every_value_the_recorder_accepts() {
        // A reader stricter than the engine is a settings panel that never renders, with no way to
        // repair the file from the UI. `resolve_dir` takes `[storage] clips_dir` as written and nothing
        // bounds `fps`, so a relative directory and `fps = 1000` are both values a start accepts.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"clips\"\n\n\
             [encode]\nfps = 1000\noutput_size = \" 1920 x 1080\"\n",
        )
        .unwrap();

        let settings = read_effective_settings(&path).unwrap();

        assert_eq!(settings.fps, 1000, "the engine bounds nothing");
        assert_eq!(
            settings.clips_dir, "clips",
            "the engine takes the string as given"
        );
        assert_eq!(
            settings.output_size, " 1920 x 1080",
            "the engine's parser trims"
        );
    }

    #[test]
    fn the_reader_still_refuses_an_output_size_the_recorder_refuses() {
        // The one key the engine does validate, so the reader keeps refusing it: the panel must not
        // show a value a start would reject.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[encode]\noutput_size = \"wide\"\n").unwrap();

        let err = read_effective_settings(&path).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("encode.output_size"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn a_mistyped_watch_list_says_what_it_should_have_been() {
        // `games.watch` is an array of tables. Reporting it as "not a true/false value" names the wrong
        // shape and sends the reader looking at the wrong key.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[games]\nwatch = \"Dota 2\"\n").unwrap();

        let err = read_effective_settings(&path).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("games.watch"), "got: {}", err.message);
        assert!(
            !err.message.contains("true/false"),
            "the message must not name a bool: {}",
            err.message
        );
    }

    // -- the writer ---------------------------------------------------------------------

    #[test]
    fn the_writer_changes_only_what_it_is_asked_and_keeps_the_comments() {
        let (_dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        write_settings(
            &path,
            &[SettingEdit::Fps(60), SettingEdit::AutoRecord(false)],
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# a comment the writer must keep"),
            "comments survive"
        );
        assert!(after.contains("fps = 60"));
        assert!(after.contains("auto_record = false"));
        assert!(
            after.contains("clips_dir = \"D:\\\\clips\""),
            "untouched keys survive"
        );
        assert!(after.contains("output_size = \"1280x720\""));
        assert!(after.contains("enabled = true"));
        assert!(
            after.lines().count() <= before.lines().count() + 1,
            "no reformatting sprawl:\n{after}"
        );
        // And the file still reads back as what was written.
        let settings = read_effective_settings(&path).unwrap();
        assert_eq!(settings.fps, 60);
        assert!(!settings.auto_record);
    }

    #[test]
    fn a_watch_list_is_written_as_tables_and_reads_back_entry_for_entry() {
        let (_dir, path) = settings_file();
        let watch = vec![
            WatchedGame {
                name: "Rainbow Six Siege".into(),
                exe: "RainbowSix.exe".into(),
                signal: Signal::Process,
            },
            WatchedGame {
                name: "League of Legends".into(),
                exe: "League of Legends.exe".into(),
                signal: Signal::LiveClientApi,
            },
        ];

        write_settings(&path, &[SettingEdit::WatchGames(watch.clone())]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# a comment the writer must keep"), "{text}");
        // Both entries, each as its own table, and the signal stated rather than implied.
        assert_eq!(text.matches("[[games.watch]]").count(), 2, "{text}");
        assert!(text.contains("exe = \"RainbowSix.exe\""), "{text}");
        assert!(text.contains("signal = \"live_client_api\""), "{text}");
        assert_eq!(read_effective_settings(&path).unwrap().watch, watch);
    }

    #[test]
    fn an_empty_watch_list_removes_the_key_so_the_defaults_apply() {
        // The engine reads an absent list as the default titles, so "no entries" is expressed the
        // way the configuration already expresses it rather than as an empty array the engine
        // would have to special-case.
        let (_dir, path) = settings_file();
        write_settings(&path, &[SettingEdit::WatchGames(Vec::new())]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("games.watch"), "{text}");
        assert_eq!(
            read_effective_settings(&path).unwrap().watch,
            default_watch()
        );
    }

    #[test]
    fn a_watch_list_the_engine_would_refuse_is_not_written() {
        let (_dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        let duplicate = vec![
            WatchedGame::by_process("Dota 2", "dota2.exe"),
            WatchedGame::by_process("Dota 2 again", "DOTA2.EXE"),
        ];
        let err = write_settings(&path, &[SettingEdit::WatchGames(duplicate)]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.to_lowercase().contains("dota2.exe"),
            "the duplicate is named, case-insensitively as the matcher compares it: {}",
            err.message
        );

        // An entry with no executable can never match, so a title the user believes is watched is
        // not; the engine refuses it and so does the panel.
        let no_executable = vec![WatchedGame::by_process("Dota 2", "")];
        let err = write_settings(&path, &[SettingEdit::WatchGames(no_executable)]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );
    }

    #[test]
    fn a_watch_entry_missing_its_executable_or_naming_an_unknown_signal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"\"\n[[games.watch]]\nname = \"Valorant\"\n",
        )
        .unwrap();
        let err = read_effective_settings(&path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("games.watch"), "got: {}", err.message);

        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"\"\n[[games.watch]]\nname = \"Valorant\"\n\
             exe = \"VALORANT-Win64-Shipping.exe\"\nsignal = \"by magic\"\n",
        )
        .unwrap();
        let err = read_effective_settings(&path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("games.watch"), "got: {}", err.message);
    }

    #[test]
    fn the_writer_stamps_the_schema_version_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[storage]\nclips_dir = \"\"\n").unwrap();

        write_settings(&path, &[SettingEdit::MicEnabled(true)]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("schema_version = 1"), "stamped: {text}");
        assert!(
            text.contains("# mine"),
            "a hand-written comment survives: {text}"
        );
        assert!(read_effective_settings(&path).is_ok());
    }

    /// A file is migrated to the version it is stamped with, or the write is refused.
    ///
    /// The bug this rules out: the desktop stamped the current version onto a file it had not
    /// migrated, and the CLI — which does migrate on read — then took the stamp at its word,
    /// skipped the step the migration existed for, and lost exactly the settings that step
    /// carried.
    #[test]
    fn a_file_is_migrated_before_it_is_stamped() {
        // A stand-in for the v1 -> v2 step, so the rule is pinned while `MIGRATIONS` is empty.
        fn v1_to_v2(table: &mut toml::Table) {
            let old = table.remove("old_name").unwrap();
            table.insert("new_name".into(), old);
        }
        let text = "old_name = 7\n[storage]\n# a comment the migration did not touch\n\
             clips_dir = \"\"\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();

        migrate_document(&mut doc, text, 2, &[v1_to_v2]).unwrap();

        let migrated = doc.to_string();
        assert!(migrated.contains("new_name = 7"), "{migrated}");
        assert!(!migrated.contains("old_name"), "{migrated}");
        assert!(migrated.contains("schema_version = 2"), "{migrated}");
        assert!(
            migrated.contains("# a comment the migration did not touch"),
            "the comments of what the migration did not touch survive: {migrated}"
        );
    }

    /// A build that has no migration for the version it would stamp refuses to write.
    ///
    /// Silently stamping a version the file was not migrated to is the failure this is about, so
    /// the refusal is loud and the file is left alone.
    #[test]
    fn a_build_that_cannot_migrate_to_the_version_it_stamps_refuses_to_write() {
        let text = "[storage]\nclips_dir = \"\"\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();

        let err = migrate_document(&mut doc, text, 2, &[]).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("schema version 2"), "{}", err.message);
        assert!(
            !doc.to_string().contains("schema_version"),
            "and nothing was stamped: {doc}"
        );
    }

    /// The writer migrates the file it writes, with this build's own migrations.
    #[test]
    fn the_writer_leaves_a_file_at_the_current_version_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\nmax_age_days = 1\n",
        )
        .unwrap();

        write_settings(&path, &[SettingEdit::Fps(30)]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("schema_version = 1"), "{text}");
        assert!(text.contains("fps = 30"), "{text}");
        assert!(text.contains("max_total_bytes = 1"), "and the rest: {text}");
    }

    #[test]
    fn a_newer_schema_is_refused_by_every_reader_and_the_writer_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let newer = "schema_version = 99\n[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\nmax_age_days = 1\n";
        std::fs::write(&path, newer).unwrap();

        for err in [
            StorageConfig::load(&path).unwrap_err(),
            RecordingConfig::load(&path).unwrap_err(),
            BackgroundConfig::load(&path).unwrap_err(),
            EngineOptionsConfig::load(&path).unwrap_err(),
            read_effective_settings(&path).unwrap_err(),
            write_settings(&path, &[SettingEdit::Fps(30)]).unwrap_err(),
        ] {
            assert_eq!(err.code, ErrorCode::InvalidInput);
            assert!(err.message.contains("schema version 99"), "{}", err.message);
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            newer,
            "the file is untouched"
        );
    }

    #[test]
    fn the_writer_creates_missing_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();

        write_settings(&path, &[SettingEdit::MicEnabled(true)]).unwrap();

        let settings = read_effective_settings(&path).unwrap();
        assert!(settings.mic_enabled);
        assert!(
            settings.defaulted.contains(&"encode.fps"),
            "nothing else was invented"
        );
    }

    #[test]
    fn the_writer_creates_a_missing_file_from_the_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        write_settings(&path, &[SettingEdit::OutputSize("1920x1080".to_string())]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("output_size = \"1920x1080\""));
        assert!(
            text.contains("[storage]"),
            "the example's sections came along"
        );
        let settings = read_effective_settings(&path).unwrap();
        assert!(settings.config_exists);
        assert_eq!(settings.output_size, "1920x1080");
    }

    /// Everything in the config's directory except the config itself, i.e. any temp file a write
    /// failed to clear.
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_rejected_edit_writes_nothing() {
        let (dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = write_settings(&path, &[SettingEdit::Fps(60), SettingEdit::Fps(0)]).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("encode.fps"), "got: {}", err.message);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "all or nothing"
        );
        assert_eq!(
            leftovers(dir.path()),
            Vec::<String>::new(),
            "no temp file is left behind"
        );
    }

    #[test]
    fn a_write_that_worked_leaves_no_temp_file_either() {
        // The temp name carries the pid and a counter, so a test cannot guess it by name; it has to be
        // looked for.
        let (dir, path) = settings_file();

        write_settings(&path, &[SettingEdit::Fps(60)]).unwrap();

        assert_eq!(leftovers(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn an_empty_edit_list_is_a_no_op() {
        let (_dir, path) = settings_file();
        let before = std::fs::read_to_string(&path).unwrap();

        write_settings(&path, &[]).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// Two applies at once must not lose one another's edit.
    ///
    /// Each write is atomic on its own, so the file is never half-written — but two concurrent
    /// applies that both read the same text would both write, and the second write would be
    /// based on a file the first had already changed. The write lock makes the read, the edit
    /// and the write one sequence.
    #[test]
    fn two_concurrent_applies_both_survive() {
        let (_dir, path) = settings_file();

        let first = std::thread::spawn({
            let path = path.clone();
            move || write_settings(&path, &[SettingEdit::Fps(30)])
        });
        let second = std::thread::spawn({
            let path = path.clone();
            move || write_settings(&path, &[SettingEdit::MicEnabled(true)])
        });
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("fps = 30"), "the first edit survived: {text}");
        assert!(
            text.contains("enabled = true"),
            "and so did the second: {text}"
        );
    }

    /// A write waits for the write lock, and goes through when it is released.
    ///
    /// The serialisation itself, pinned: the read, the edit and the write are one sequence, so
    /// a second apply reads the file the first one wrote.
    #[test]
    fn a_write_waits_for_the_write_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (_dir, path) = settings_file();
        let guard = settings_write_lock();
        let (tx, rx) = mpsc::channel();
        let writer = std::thread::spawn({
            let path = path.clone();
            move || {
                write_settings(&path, &[SettingEdit::Fps(30)]).unwrap();
                let _ = tx.send(());
            }
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the write is waiting for the lock this thread holds"
        );

        drop(guard);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the write went through once the lock was released");
        writer.join().unwrap();
    }

    #[test]
    fn a_section_that_is_not_a_table_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "encode = 5\n").unwrap();

        let err = write_settings(&path, &[SettingEdit::Fps(30)]).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("[encode]"), "got: {}", err.message);
    }

    #[test]
    fn the_fps_band_is_the_panels_own_and_applies_only_when_writing() {
        // Nothing in the engine bounds `fps`, so this band is the panel's ergonomics, not a shared
        // rule: it lives on the write path, where the user's hand is on the value, not on the reader,
        // where a bound would refuse a file the recorder runs.
        for bad in [0, 241, 1000] {
            let err = validate_edit(&SettingEdit::Fps(bad)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "fps {bad}");
            assert!(err.message.contains("encode.fps"), "got: {}", err.message);
        }
        assert!(validate_edit(&SettingEdit::Fps(1)).is_ok());
        assert!(validate_edit(&SettingEdit::Fps(240)).is_ok());
    }

    #[test]
    fn clips_dir_accepts_empty_or_an_absolute_directory_with_something_in_it() {
        assert!(
            validate_clips_dir("").is_ok(),
            "empty resets to the default"
        );
        assert!(validate_clips_dir("/media/clips").is_ok());
        assert!(validate_clips_dir("C:\\clips").is_ok());
        assert!(validate_clips_dir("C:/clips").is_ok());
        assert!(validate_clips_dir("\\\\server\\share\\clips").is_ok());

        for bad in ["clips", "replayed/clips", "C:clips"] {
            assert!(
                validate_clips_dir(bad).is_err(),
                "{bad:?} is relative and must be refused"
            );
        }
    }

    #[test]
    fn clips_dir_is_refused_when_it_would_widen_the_asset_scope_to_a_whole_volume() {
        // The directory is handed to `allow_directory(.., true)`, the webview's only read channel, so a
        // root, a bare share, or a path that climbs back out would expose far more than the clips the
        // user meant to share.
        for bad in [
            "/",
            "\\",
            "C:\\",
            "C:/",
            "\\\\server",
            "\\\\server\\share",
            "C:\\clips\\..\\..\\Windows",
            "/media/../etc",
        ] {
            let err = validate_clips_dir(bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "{bad:?} must be refused");
            assert!(
                err.message.contains("storage.clips_dir"),
                "got: {}",
                err.message
            );
        }
    }

    #[test]
    fn the_extra_hotkeys_are_off_unless_the_file_sets_them() {
        let off = BackgroundConfig::from_toml("[hotkeys]\nclip = \"Ctrl+F8\"\n").unwrap();
        assert_eq!(
            (off.toggle_recording_hotkey, off.bookmark_hotkey),
            (None, None)
        );
        let on = BackgroundConfig::from_toml(
            "[hotkeys]\nclip = \"Ctrl+F8\"\ntoggle_recording = \"Ctrl+F9\"\nbookmark = \"Ctrl+F10\"\n",
        )
        .unwrap();
        assert_eq!(on.toggle_recording_hotkey.as_deref(), Some("Ctrl+F9"));
        assert_eq!(on.bookmark_hotkey.as_deref(), Some("Ctrl+F10"));
    }

    // -- the encoder panel's read and write path -----------------------------------------

    /// A file with an `[encode]` section and nothing else: the panel reads what is there and
    /// the example's values for the rest.
    #[test]
    fn the_encoder_settings_come_from_the_file_and_the_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[encode]\nvendor = \"qsv\"\ncodec = \"hevc\"\nbitrate_kbps = 8000\n\
             rate_control = \"vbr\"\npreset = \"medium\"\nzero_copy = true\n",
        )
        .unwrap();

        let settings = read_encoder_settings(&path).unwrap();

        assert_eq!(settings.vendor, "qsv");
        assert_eq!(settings.codec, "hevc");
        assert_eq!(settings.bitrate_kbps, 8_000);
        assert_eq!(settings.rate_control, "vbr");
        assert_eq!(settings.preset, "medium");
        assert_eq!(settings.zero_copy, Some(true));
        // Not in the file: the example's.
        assert_eq!(settings.fps, 60);
        assert_eq!(settings.audio_bitrate_kbps, 192);
        assert!(settings.adapt_fps);
    }

    /// The panel's settings are written to the file and read back.
    ///
    /// The round trip is the point: what the panel shows after a write has to be what the file
    /// says, not what the panel sent.
    #[test]
    fn the_encoder_settings_are_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();

        let wanted = EncoderSettings {
            vendor: "amf".to_string(),
            codec: "hevc".to_string(),
            bitrate_kbps: 30_000,
            fps: 30,
            output_size: "1920x1080".to_string(),
            adapt_fps: false,
            zero_copy: Some(false),
            rate_control: "quality".to_string(),
            preset: "speed".to_string(),
            audio_bitrate_kbps: 160,
        };
        write_encoder_settings(&path, &wanted).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("vendor = \"amf\""), "{text}");
        assert!(text.contains("rate_control = \"quality\""), "{text}");
        assert!(text.contains("preset = \"speed\""), "{text}");
        assert!(text.contains("adapt_fps = false"), "{text}");
        assert!(text.contains("bitrate_kbps = 160"), "{text}");
        // The rest of the file survives.
        assert!(text.contains("[storage]"), "{text}");

        assert_eq!(read_encoder_settings(&path).unwrap(), wanted);
    }

    /// "Let the recorder decide" is an absent key, so writing it removes the key.
    ///
    /// Writing `false` instead would turn the hardware path **off**, which is the opposite of
    /// what the panel asked for.
    #[test]
    fn an_unset_zero_copy_removes_the_key_rather_than_writing_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[encode]\nzero_copy = true\n").unwrap();

        let mut wanted = read_encoder_settings(&path).unwrap();
        assert_eq!(wanted.zero_copy, Some(true));
        wanted.zero_copy = None;
        write_encoder_settings(&path, &wanted).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("zero_copy"),
            "the key is gone, not set to false: {text}"
        );
        assert_eq!(read_encoder_settings(&path).unwrap().zero_copy, None);
    }

    /// The game-audio rate reaches the recorder's own configuration.
    ///
    /// The encoder panel offers it, so it has to travel: a rate the panel can change that the
    /// recorder ignores would be a lie.
    #[test]
    fn the_audio_bitrate_reaches_the_recorders_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[buffer]\npre_seconds = 30\npost_seconds = 5\nsegment_time = 1\nscratch_dir = \"\"\n\n\
             [encode]\nvendor = \"auto\"\ncodec = \"h264\"\nbitrate_kbps = 20000\nfps = 60\n\
             output_size = \"\"\n\n[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\n\
             max_age_days = 1\n\n[audio]\nbitrate_kbps = 160\n",
        )
        .unwrap();

        let recording = RecordingConfig::load(&path).unwrap();
        assert_eq!(
            recording.audio.bitrate_kbps,
            Some(160),
            "the configured rate is carried across"
        );

        // Absent: nothing is carried, and the encoder's own default applies.
        std::fs::write(
            &path,
            "[buffer]\npre_seconds = 30\npost_seconds = 5\nsegment_time = 1\nscratch_dir = \"\"\n\n\
             [encode]\nvendor = \"auto\"\ncodec = \"h264\"\nbitrate_kbps = 20000\nfps = 60\n\
             output_size = \"\"\n\n[storage]\nclips_dir = \"\"\nmax_total_bytes = 1\n\
             max_age_days = 1\n",
        )
        .unwrap();
        let absent = RecordingConfig::load(&path).unwrap();
        assert_eq!(absent.audio.bitrate_kbps, None);
    }

    #[test]
    fn output_size_follows_the_engines_own_parser() {
        // Reused: `replayed_recorder::parse_output_size` trims each component and sets no bounds, so
        // the panel accepts exactly what a start accepts. A private copy that did neither let
        // `output_size = " 1920x1080"` record fine while `get_settings` refused it.
        assert!(validate_output_size("").is_ok(), "empty is the native size");
        assert!(validate_output_size("1920x1080").is_ok());
        assert!(validate_output_size("640x480").is_ok());
        assert!(
            validate_output_size("1920 x 1080").is_ok(),
            "the engine trims"
        );
        assert!(
            validate_output_size("99999x1080").is_ok(),
            "the engine bounds nothing"
        );

        for bad in ["wide", "1920x", "x1080", "1920x1080x2"] {
            assert!(
                validate_output_size(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }
}
