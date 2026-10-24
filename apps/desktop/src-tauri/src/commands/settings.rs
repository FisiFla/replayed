//! The settings the window shows and edits (`config.toml`).

use super::*;
use crate::background::AutostartOutcome;
use crate::config::{EngineOptionsConfig, SettingEdit};
use replayed_recorder::RecorderOptions;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------
//
// Two commands: one reads the effective values, one writes edited keys and says what the write
// means for a running recorder. Both are plain functions over explicit paths (no `State`, no
// `AppHandle`) so tests drive them against temporary directories. Only re-scoping the asset
// protocol after a clips-directory change cannot run headless; that lives in the `lib.rs`
// wrapper.

/// The settings the window shows, as the frontend sees them.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the
/// exact JSON keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsDto {
    /// `[storage] clips_dir`, verbatim — empty means the default.
    pub clips_dir: String,
    /// Where that resolves: the clips directory itself, for display.
    pub clips_dir_resolved: String,
    /// `[encode] fps`.
    pub fps: u32,
    /// `[encode] output_size` — empty is the native capture size.
    pub output_size: String,
    /// `[mic] enabled`.
    pub mic_enabled: bool,
    /// `[mic] device` — a capture endpoint id; empty is the Windows default microphone.
    pub mic_device: String,
    /// `[capture] output_device` — an output endpoint id; empty is the Windows default output.
    pub output_device: String,
    /// `[recorder] save_session` — also keep the whole recording as a session file.
    pub save_session: bool,
    /// `[games] auto_record`.
    pub auto_record: bool,
    /// `[hotkeys] clip`: the chord the shell will install at the next start. The chord *in force
    /// now* is the app status's, not this one — a saved chord takes effect at the next start.
    pub clip_hotkey: String,
    /// `[app] start_with_system`: the file's wish. Whether Windows really has an entry is the
    /// registry's answer, reported when the setting is applied — the panel shows this value, not
    /// a claim about the registry it has not read.
    pub start_with_system: bool,
    /// `[encode] bitrate_kbps`.
    pub bitrate_kbps: u32,
    /// The quality preset that bitrate is (`low`, `balanced`, `high`) or `custom`.
    pub quality: String,
    /// `[capture] monitor` — 0 is the primary monitor.
    pub capture_monitor: usize,
    /// `[capture] window` — a window title to capture instead; empty captures the monitor.
    pub capture_window: String,
    /// `[[games.watch]]`: the titles the watcher looks for, or the default list when the file
    /// names none (what the engine watches then, too). Names, executables and signals, because
    /// the panel edits this list.
    pub watch_games: Vec<WatchGameDto>,
    /// Keys that fell back to the example because the file does not set them, e.g. `"encode.fps"`;
    /// the panel says so rather than presenting defaults as configured.
    pub defaulted: Vec<String>,
    /// False when no file exists and every value is the example's.
    pub config_exists: bool,
    /// The file that was read.
    pub config_path: String,
}

/// One `[[games.watch]]` entry, as the panel edits it.
///
/// `exe` is the identity: the watcher compares executable names case-insensitively, so the same
/// executable twice is one game and the store of the list refuses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchGameDto {
    /// Human name shown in the tray and the panel: "Rainbow Six Siege".
    pub name: String,
    /// Executable file name compared against the process list: `RainbowSix.exe`.
    pub exe: String,
    /// `"process"` or `"live_client_api"`: how the title is detected. Riot titles that publish
    /// the Live Client Data API are the only ones that can use the second.
    pub signal: String,
}

impl From<&replayed_events::process::WatchedGame> for WatchGameDto {
    fn from(game: &replayed_events::process::WatchedGame) -> Self {
        Self {
            name: game.name.clone(),
            exe: game.exe.clone(),
            signal: signal_name(game.signal).to_string(),
        }
    }
}

/// The serde name of a [`Signal`](replayed_events::process::Signal), which is also the string a
/// config file writes.
fn signal_name(signal: replayed_events::process::Signal) -> &'static str {
    match signal {
        replayed_events::process::Signal::Process => "process",
        replayed_events::process::Signal::LiveClientApi => "live_client_api",
    }
}

impl TryFrom<&WatchGameDto> for replayed_events::process::WatchedGame {
    type Error = CommandError;

    fn try_from(game: &WatchGameDto) -> Result<Self, CommandError> {
        let signal =
            replayed_events::process::Signal::parse(game.signal.trim()).ok_or_else(|| {
                CommandError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "{} has signal {:?}, but it must be \"process\" or \"live_client_api\"",
                        game.name, game.signal
                    ),
                )
            })?;
        Ok(Self {
            name: game.name.trim().to_string(),
            exe: game.exe.trim().to_string(),
            signal,
        })
    }
}

/// A partial settings update from the window: every field optional, absent fields left alone.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsUpdate {
    pub clips_dir: Option<String>,
    pub fps: Option<u32>,
    pub output_size: Option<String>,
    pub mic_enabled: Option<bool>,
    pub mic_device: Option<String>,
    pub output_device: Option<String>,
    pub save_session: Option<bool>,
    pub auto_record: Option<bool>,
    /// `[app] start_with_system`; the writer reports what Windows did with it.
    pub start_with_system: Option<bool>,
    /// `[hotkeys] clip`; checked free (on Windows) before it is written, and installed at the
    /// next start.
    pub clip_hotkey: Option<String>,
    /// The whole `[[games.watch]]` list; absent leaves it alone.
    pub watch_games: Option<Vec<WatchGameDto>>,
    /// A preset name; sets `[encode] bitrate_kbps`.
    pub quality: Option<String>,
    pub capture_monitor: Option<usize>,
    pub capture_window: Option<String>,
}

/// What writing settings did, and what it means for a running recorder.
///
/// `settings` is read back after the write, so the window refreshes from this rather than
/// issuing a second call against a file that may have changed under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSettingsOutcome {
    pub settings: SettingsDto,
    /// The keys that changed, e.g. `"encode.fps"`.
    pub applied: Vec<String>,
    /// True only when a recording holds the slot and an engine key changed: fps, output size, mic
    /// and auto-record are baked in per start, so they take effect on the next one. Never an
    /// implicit restart; the window offers Stop.
    pub restart_required: bool,
    /// Names the changed engine keys when a restart is required, else null.
    pub restart_reason: Option<String>,
    /// What happened to the start-with-Windows entry, when this update changed it.
    ///
    /// Filled in by [`apply_autostart_update`] after the file is written: the writer here has no
    /// executable path and no `reg.exe`, and the settings file is the only switch either way.
    /// A failure is reported here rather than as an error, because the setting *was* saved and
    /// a failed registry write must not make the panel claim nothing happened.
    pub autostart: Option<AutostartReport>,
    /// What a saved clip-hotkey change means, when this update changed it.
    ///
    /// The chord is installed once, at startup, and owned until the process exits, so a saved
    /// change cannot take effect now: this says which chord stays in force until a restart.
    pub hotkey: Option<String>,
}

/// What applying `[app] start_with_system` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartReport {
    /// True when Windows now agrees with the file. False when the file was written and the
    /// registry was not changed (it is not Windows, the entry belongs to something else, or
    /// `reg.exe` refused).
    pub applied: bool,
    /// The sentence to show: what was done, or why not.
    pub detail: String,
}

/// The engine options a start is built with: the file's `[mic]` and `[games]`, not defaults.
///
/// A start that ignored them would silently record without the configured microphone and
/// watcher, both of which just look like an idle recorder. Mode stays the default (replay
/// buffer); the window has no mode switch.
pub(super) fn recorder_options(engine: &EngineOptionsConfig) -> RecorderOptions {
    RecorderOptions {
        mode: Default::default(),
        save_session: engine.recorder.save_session,
        mic: engine.mic.clone(),
        games: engine.games.clone(),
        game: None,
    }
}

/// Read the effective settings for display.
pub fn read_settings(app_data_dir: &Path, config_path: &Path) -> Result<SettingsDto, CommandError> {
    let effective = crate::config::read_effective_settings(config_path)?;
    Ok(SettingsDto {
        clips_dir_resolved: AppPaths::resolve(app_data_dir, &effective.clips_dir)
            .clips_dir
            .to_string_lossy()
            .into_owned(),
        clips_dir: effective.clips_dir,
        fps: effective.fps,
        output_size: effective.output_size,
        mic_enabled: effective.mic_enabled,
        mic_device: effective.mic_device,
        output_device: effective.output_device,
        save_session: effective.save_session,
        auto_record: effective.auto_record,
        clip_hotkey: effective.clip_hotkey,
        start_with_system: effective.start_with_system,
        quality: crate::config::quality_for_bitrate(effective.bitrate_kbps).to_string(),
        bitrate_kbps: effective.bitrate_kbps,
        capture_monitor: effective.capture_monitor,
        capture_window: effective.capture_window,
        watch_games: effective.watch.iter().map(WatchGameDto::from).collect(),
        defaulted: effective.defaulted.iter().map(|s| s.to_string()).collect(),
        config_exists: effective.config_exists,
        config_path: config_path.to_string_lossy().into_owned(),
    })
}

/// Validate and write edited keys, then report what the write means.
///
/// A validation failure writes nothing. Engine keys (fps, output size, mic, auto-record) persist
/// immediately but take effect on the next recorder start; `restart_required` says so when a
/// recording is running and one of them changed value. `storage.clips_dir` is applied by the
/// caller (paths rebuild, asset scope); this function reports it in `applied` but touches no
/// state.
///
/// The read before the write and the read after it are one sequence with the write itself, under
/// the settings write lock: two concurrent applies would otherwise both read the same text, and
/// the second write would be based on a file the first had already changed.
pub fn update_settings_file(
    app_data_dir: &Path,
    config_path: &Path,
    update: &SettingsUpdate,
    recorder_running: bool,
) -> Result<UpdateSettingsOutcome, CommandError> {
    let _guard = crate::config::settings_write_lock();
    let before = crate::config::read_effective_settings(config_path)?;
    let mut edits = Vec::new();
    if let Some(dir) = &update.clips_dir {
        edits.push(SettingEdit::ClipsDir(dir.clone()));
    }
    if let Some(fps) = update.fps {
        edits.push(SettingEdit::Fps(fps));
    }
    if let Some(size) = &update.output_size {
        edits.push(SettingEdit::OutputSize(size.clone()));
    }
    if let Some(enabled) = update.mic_enabled {
        edits.push(SettingEdit::MicEnabled(enabled));
    }
    if let Some(id) = &update.mic_device {
        edits.push(SettingEdit::MicDevice(id.clone()));
    }
    if let Some(id) = &update.output_device {
        edits.push(SettingEdit::OutputDevice(id.clone()));
    }
    if let Some(save) = update.save_session {
        edits.push(SettingEdit::SaveSession(save));
    }
    if let Some(auto) = update.auto_record {
        edits.push(SettingEdit::AutoRecord(auto));
    }
    if let Some(enabled) = update.start_with_system {
        edits.push(SettingEdit::StartWithSystem(enabled));
    }
    if let Some(chord) = &update.clip_hotkey {
        edits.push(SettingEdit::ClipHotkey(chord.clone()));
    }
    if let Some(watch) = &update.watch_games {
        let mut games = Vec::with_capacity(watch.len());
        for game in watch {
            games.push(replayed_events::process::WatchedGame::try_from(game)?);
        }
        edits.push(SettingEdit::WatchGames(games));
    }
    if let Some(quality) = &update.quality {
        edits.push(SettingEdit::Quality(quality.clone()));
    }
    if let Some(index) = update.capture_monitor {
        edits.push(SettingEdit::CaptureMonitor(index));
    }
    if let Some(title) = &update.capture_window {
        edits.push(SettingEdit::CaptureWindow(title.clone()));
    }
    let applied: Vec<String> = edits.iter().map(|edit| edit.key().to_string()).collect();

    if !edits.is_empty() {
        crate::config::write_settings_locked(config_path, &edits)?;
    }
    let after = read_settings(app_data_dir, config_path)?;

    // Only a value that actually changed can demand a restart: re-applying the same fps is a no-op,
    // not a reason to stop a recording.
    let mut engine_changed = Vec::new();
    if update.fps.is_some() && after.fps != before.fps {
        engine_changed.push("encode.fps");
    }
    if update.output_size.is_some() && after.output_size != before.output_size {
        engine_changed.push("encode.output_size");
    }
    if update.mic_enabled.is_some() && after.mic_enabled != before.mic_enabled {
        engine_changed.push("mic.enabled");
    }
    if update.mic_device.is_some() && after.mic_device != before.mic_device {
        engine_changed.push("mic.device");
    }
    if update.output_device.is_some() && after.output_device != before.output_device {
        engine_changed.push("capture.output_device");
    }
    if update.save_session.is_some() && after.save_session != before.save_session {
        engine_changed.push("recorder.save_session");
    }
    if update.auto_record.is_some() && after.auto_record != before.auto_record {
        engine_changed.push("games.auto_record");
    }
    // `before` is the file's own shape (`WatchedGame`), `after` the panel's (`WatchGameDto`): the
    // comparison is between what the file said and what it says now, so one side is mapped.
    let before_watch: Vec<WatchGameDto> = before.watch.iter().map(WatchGameDto::from).collect();
    if update.watch_games.is_some() && after.watch_games != before_watch {
        engine_changed.push("games.watch");
    }
    if update.quality.is_some() && after.bitrate_kbps != before.bitrate_kbps {
        engine_changed.push("encode.bitrate_kbps");
    }
    if update.capture_monitor.is_some() && after.capture_monitor != before.capture_monitor {
        engine_changed.push("capture.monitor");
    }
    if update.capture_window.is_some() && after.capture_window != before.capture_window {
        engine_changed.push("capture.window");
    }
    let restart_required = recorder_running && !engine_changed.is_empty();
    let restart_reason = restart_required.then(|| {
        format!(
            "a recording is running: {} takes effect when the next recording starts",
            engine_changed.join(", ")
        )
    });

    Ok(UpdateSettingsOutcome {
        settings: after,
        applied,
        restart_required,
        restart_reason,
        // The caller that can reach `reg.exe` fills this in (see `apply_autostart_update`).
        autostart: None,
        // And the caller that knows which chord is really installed fills this in
        // (see `hotkey_change_note`): the file's previous value is not the same thing.
        hotkey: None,
    })
}

/// Apply `[app] start_with_system` after the file has recorded it, and say what happened.
///
/// The split is deliberate. [`update_settings_file`] has no executable path and must not run
/// `reg.exe`; this function has both and does nothing else, so the "the file says one thing, the
/// registry says another" case has one place to be reported from. `apply` is the boundary (the
/// real `background::apply_autostart` in the window, a closure in tests).
///
/// A failure is a report, not an error: the setting is saved and takes effect at the next start
/// even when the registry write failed (the shell re-syncs on every launch), so making the whole
/// command fail would claim the file was not written.
pub fn apply_autostart_update(
    enabled: bool,
    exe: &Path,
    apply: impl FnOnce(bool, &Path) -> Result<AutostartOutcome, String>,
) -> AutostartReport {
    match apply(enabled, exe) {
        Ok(outcome) => AutostartReport {
            applied: !matches!(outcome, AutostartOutcome::NotApplicable(_)),
            detail: outcome.describe(),
        },
        Err(err) => AutostartReport {
            applied: false,
            detail: err,
        },
    }
}

/// What a requested clip-hotkey change needs before anything is written.
///
/// The chord is registered once at startup and owned until the process exits, so a panel cannot
/// make a new one take effect immediately; what it can do is refuse a chord something else owns,
/// before writing a file that would fail to install at the next start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyCheck {
    /// The requested chord is the one already in force: nothing to probe, nothing to register.
    AlreadyInForce,
    /// This build has no global hotkey (not Windows): the value is saved and nothing is probed.
    NotSupported,
    /// The chord is free as of right now.
    Free,
    /// Something owns it, and this is the sentence saying so.
    Taken(String),
}

/// Decide whether a requested chord may be written.
///
/// `in_force` is the chord the running process actually holds (not the file's value: a hand edit
/// can make them differ). `probe` is the platform boundary — the real
/// [`replayed_events::hotkey::probe`] in the window, a closure in tests — so the decision is
/// testable on any host.
///
/// A chord this build cannot parse is `Free` on purpose: the writer's own validation refuses it
/// with a message about the chord's shape, and two refusals for one typo is one too many.
pub fn check_clip_hotkey(
    requested: &str,
    in_force: Option<&str>,
    supported: bool,
    probe: impl FnOnce(replayed_events::hotkey::Hotkey) -> Result<(), String>,
) -> HotkeyCheck {
    let Ok(chord) = replayed_events::hotkey::Hotkey::parse(requested) else {
        return HotkeyCheck::Free;
    };
    let normalised = chord.to_string();
    if in_force == Some(normalised.as_str()) {
        return HotkeyCheck::AlreadyInForce;
    }
    if !supported {
        return HotkeyCheck::NotSupported;
    }
    match probe(chord) {
        Ok(()) => HotkeyCheck::Free,
        Err(err) => HotkeyCheck::Taken(err),
    }
}

/// The sentence a saved clip-hotkey change produces.
///
/// `in_force` is the chord the running process holds, `None` when nothing is installed (a
/// registration failure at startup, or a build without a global hotkey). The chord is saved
/// either way — the file is the switch, and the next start reads it — so every branch says what
/// is saved *and* what will answer a keypress until then.
pub fn hotkey_change_note(requested: &str, in_force: Option<&str>) -> String {
    let requested = replayed_events::hotkey::Hotkey::parse(requested)
        .map(|chord| chord.to_string())
        .unwrap_or_else(|_| requested.trim().to_string());
    match in_force {
        Some(current) if current == requested => {
            format!("the clip hotkey {requested} is already in force")
        }
        Some(current) => format!(
            "the clip hotkey is saved as {requested}; {current} stays in force until replayed \
             restarts"
        ),
        None => format!(
            "the clip hotkey is saved as {requested}; it is registered at the next start, when \
             replayed can hold it"
        ),
    }
}

/// Resolve the clips directory a configuration names, and create it on disk.
///
/// Split out so the ordering that matters is testable without a window: this must succeed
/// before `[storage] clips_dir` is written. The next launch reads that value before it can open
/// a window, and when the directory cannot be made `AppState::open` logs and calls
/// `std::process::exit(1)`, so a directory persisted before it is proved usable leaves an
/// application with no UI left to repair it.
pub fn prepare_clips_dir(app_data_dir: &Path, clips_dir: &str) -> Result<AppPaths, CommandError> {
    let paths = AppPaths::resolve(app_data_dir, clips_dir);
    std::fs::create_dir_all(&paths.clips_dir).map_err(|e| {
        CommandError::new(
            ErrorCode::Io,
            format!(
                "could not create the clips directory {}: {e}",
                paths.clips_dir.display()
            ),
        )
    })?;
    Ok(paths)
}

/// [`update_settings_file`], plus the directory the shell has to move to.
///
/// Returns the resolved (and already created) paths when the update carried a `clips_dir`, so
/// the caller can commit them to the running shell and re-scope the asset protocol. The
/// ordering is the reason this exists: `prepare_clips_dir` runs first, so a directory that
/// cannot be made never reaches the file and the write never persists a value the next launch
/// would die on.
///
/// Writing the file first and creating the directory second would leave `config.toml` naming a
/// missing directory while the running shell kept the old one, with the failure deferred to the
/// next launch, which exits before it can show a window.
///
/// Creating the directory even when the write is then rejected is harmless (an empty directory,
/// at worst), and checking without creating would race.
pub fn update_settings_and_prepare(
    app_data_dir: &Path,
    config_path: &Path,
    update: &SettingsUpdate,
    recorder_running: bool,
) -> Result<(UpdateSettingsOutcome, Option<AppPaths>), CommandError> {
    let prepared = match update.clips_dir.as_deref() {
        Some(dir) => Some(prepare_clips_dir(app_data_dir, dir)?),
        None => None,
    };
    let outcome = update_settings_file(app_data_dir, config_path, update, recorder_running)?;
    Ok((outcome, prepared))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- settings ----------------------------------------------------------------------

    fn settings_case() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        (dir, path)
    }

    #[test]
    fn the_settings_dto_json_matches_the_typescript_interface() {
        // Like the clip, storage and status DTOs: a rename on either side is found here, not by a
        // settings panel that silently shows nothing.
        let (dir, path) = settings_case();
        let dto = read_settings(dir.path(), &path).unwrap();
        let dto_value = serde_json::to_value(&dto).unwrap();
        let mut keys: Vec<&str> = dto_value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "auto_record",
                "bitrate_kbps",
                "capture_monitor",
                "capture_window",
                "clip_hotkey",
                "clips_dir",
                "clips_dir_resolved",
                "config_exists",
                "config_path",
                "defaulted",
                "fps",
                "mic_device",
                "mic_enabled",
                "output_device",
                "output_size",
                "quality",
                "save_session",
                "start_with_system",
                "watch_games"
            ]
        );
        assert!(!dto.config_exists, "no file: everything is the example's");
        assert_eq!(dto.fps, 60);
        assert!(!dto.auto_record);

        let outcome = UpdateSettingsOutcome {
            settings: dto,
            applied: vec!["encode.fps".to_string()],
            restart_required: true,
            restart_reason: Some("a recording is running".to_string()),
            autostart: Some(AutostartReport {
                applied: false,
                detail: "this build is not Windows".to_string(),
            }),
            hotkey: Some("the clip hotkey is saved as Ctrl+F9".to_string()),
        };
        let outcome_value = serde_json::to_value(&outcome).unwrap();
        let mut keys: Vec<&str> = outcome_value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "applied",
                "autostart",
                "hotkey",
                "restart_reason",
                "restart_required",
                "settings"
            ]
        );
    }

    #[test]
    fn update_settings_applies_a_subset_and_leaves_the_rest() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[encode]\nfps = 30\n").unwrap();

        let outcome = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                fps: Some(60),
                mic_enabled: Some(true),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        assert_eq!(outcome.applied, vec!["encode.fps", "mic.enabled"]);
        assert!(!outcome.restart_required, "nothing is recording");
        assert_eq!(outcome.restart_reason, None);
        assert_eq!(outcome.settings.fps, 60);
        assert!(outcome.settings.mic_enabled);
        // Untouched keys survive, from the file where set and the example elsewhere.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("fps = 60"));
        assert!(text.contains("enabled = true"));
    }

    #[test]
    fn quality_and_capture_updates_apply_and_need_a_restart_while_recording() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[encode]\nbitrate_kbps = 20000\n").unwrap();

        let outcome = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                quality: Some("high".into()),
                capture_monitor: Some(1),
                capture_window: Some("Game".into()),
                ..Default::default()
            },
            true,
        )
        .unwrap();

        assert_eq!(
            outcome.applied,
            vec!["encode.bitrate_kbps", "capture.monitor", "capture.window"]
        );
        assert_eq!(outcome.settings.bitrate_kbps, 40_000);
        assert_eq!(outcome.settings.quality, "high");
        assert_eq!(
            (
                outcome.settings.capture_monitor,
                outcome.settings.capture_window.as_str()
            ),
            (1, "Game")
        );
        assert!(outcome.restart_required);

        // Re-applying the same preset is not a change.
        let again = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                quality: Some("high".into()),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(!again.restart_required);
    }

    #[test]
    fn a_watch_list_round_trips_through_the_update_and_the_file() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[games]\nauto_record = true\n").unwrap();

        let watch = vec![
            WatchGameDto {
                name: "Rainbow Six Siege".into(),
                exe: "RainbowSix.exe".into(),
                signal: "process".into(),
            },
            WatchGameDto {
                name: "League of Legends".into(),
                exe: "League of Legends.exe".into(),
                signal: "live_client_api".into(),
            },
        ];
        let outcome = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                watch_games: Some(watch.clone()),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        assert_eq!(outcome.applied, vec!["games.watch"]);
        assert_eq!(outcome.settings.watch_games, watch);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[[games.watch]]").count(), 2, "{text}");
        // A fresh read agrees with the file, which is what the panel renders.
        assert_eq!(read_settings(dir.path(), &path).unwrap().watch_games, watch);
    }

    #[test]
    fn a_watch_list_with_an_unknown_signal_is_refused_and_nothing_is_written() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[games]\nauto_record = true\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                watch_games: Some(vec![WatchGameDto {
                    name: "Valorant".into(),
                    exe: "VALORANT-Win64-Shipping.exe".into(),
                    signal: "telepathy".into(),
                }]),
                ..Default::default()
            },
            false,
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("live_client_api"), "{}", err.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_changed_watch_list_names_the_restart_a_running_recording_needs() {
        // The watcher is built per start, so a list edited during a recording takes effect next
        // time; the outcome says so rather than letting the panel imply it is already watching.
        let (dir, path) = settings_case();
        std::fs::write(&path, "[games]\nauto_record = true\n").unwrap();

        let outcome = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                watch_games: Some(vec![WatchGameDto {
                    name: "Rainbow Six Siege".into(),
                    exe: "RainbowSix.exe".into(),
                    signal: "process".into(),
                }]),
                ..Default::default()
            },
            true,
        )
        .unwrap();

        assert!(outcome.restart_required);
        assert!(
            outcome.restart_reason.unwrap().contains("games.watch"),
            "the reason names the key"
        );
    }

    #[test]
    fn an_autostart_toggle_is_written_to_the_file_and_reported_separately() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[app]\nstart_with_system = false\n").unwrap();

        let outcome = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                start_with_system: Some(true),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        assert_eq!(outcome.applied, vec!["app.start_with_system"]);
        assert!(outcome.settings.start_with_system);
        assert_eq!(
            outcome.autostart, None,
            "writing the file touches no registry; that is `apply_autostart_update`"
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("start_with_system = true"));
    }

    #[test]
    fn a_clip_hotkey_change_is_probed_before_it_is_saved() {
        use replayed_events::hotkey::Hotkey;

        // Free: the probe runs and says so.
        let mut probed = None;
        assert_eq!(
            check_clip_hotkey("Ctrl+F9", Some("Ctrl+F8"), true, |chord| {
                probed = Some(chord);
                Ok(())
            }),
            HotkeyCheck::Free
        );
        assert_eq!(probed, Some(Hotkey::parse("Ctrl+F9").unwrap()));

        // Taken: the registration error is carried through verbatim, so the caller can name the
        // chord and the likely owner.
        let taken = check_clip_hotkey("Ctrl+F9", None, true, |_| {
            Err("Ctrl+F9 is already held by another application".to_string())
        });
        assert!(matches!(&taken, HotkeyCheck::Taken(msg) if msg.contains("another application")));

        // Already in force: nothing is probed (it would fail against our own registration).
        let mut probed = false;
        assert_eq!(
            check_clip_hotkey("ctrl+f8", Some("Ctrl+F8"), true, |_| {
                probed = true;
                Ok(())
            }),
            HotkeyCheck::AlreadyInForce
        );
        assert!(!probed, "the chord this process holds cannot be probed");

        // Not Windows: saved, not probed.
        assert_eq!(
            check_clip_hotkey("Ctrl+F9", None, false, |_| panic!("must not probe")),
            HotkeyCheck::NotSupported
        );

        // Unparseable: the writer refuses it with its own message; this check stays out of it.
        assert_eq!(
            check_clip_hotkey("Ctrl", None, true, |_| panic!("must not probe")),
            HotkeyCheck::Free
        );
    }

    #[test]
    fn the_hotkey_change_note_says_what_is_saved_and_what_is_in_force() {
        assert_eq!(
            hotkey_change_note("ctrl+f9", Some("Ctrl+F8")),
            "the clip hotkey is saved as Ctrl+F9; Ctrl+F8 stays in force until replayed restarts"
        );
        assert_eq!(
            hotkey_change_note("Ctrl+F8", Some("Ctrl+F8")),
            "the clip hotkey Ctrl+F8 is already in force"
        );
        assert!(
            hotkey_change_note("Ctrl+F9", None).contains("registered at the next start"),
            "nothing installed: the note says when it will be"
        );
    }

    #[test]
    fn a_clip_hotkey_is_written_as_the_spelling_the_installer_will_register() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[hotkeys]\nclip = \"Ctrl+F8\"\n").unwrap();

        let outcome = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                clip_hotkey: Some("alt+f9".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        assert_eq!(outcome.applied, vec!["hotkeys.clip"]);
        assert_eq!(outcome.settings.clip_hotkey, "Alt+F9");
        assert!(
            outcome.hotkey.is_none(),
            "the caller with the state fills it in"
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("clip = \"Alt+F9\""));

        // A chord this build cannot install is refused and nothing is written.
        let before = std::fs::read_to_string(&path).unwrap();
        let err = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                clip_hotkey: Some("Ctrl".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("hotkeys.clip"), "{}", err.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn applying_autostart_reports_what_windows_did() {
        let mut seen = None;
        let report = apply_autostart_update(true, Path::new("/opt/replayed"), |enabled, exe| {
            seen = Some((enabled, exe.to_path_buf()));
            Ok(AutostartOutcome::Enabled)
        });
        assert_eq!(seen, Some((true, PathBuf::from("/opt/replayed"))));
        assert!(report.applied);
        assert!(report.detail.contains("is set"), "{}", report.detail);

        let removed = apply_autostart_update(false, Path::new("/opt/replayed"), |_, _| {
            Ok(AutostartOutcome::Removed)
        });
        assert!(removed.applied);
        assert!(removed.detail.contains("removed"), "{}", removed.detail);

        let nothing_to_do = apply_autostart_update(true, Path::new("/opt/replayed"), |_, _| {
            Ok(AutostartOutcome::Unchanged)
        });
        assert!(nothing_to_do.applied);
    }

    #[test]
    fn a_failed_or_impossible_autostart_write_is_reported_not_raised() {
        // The setting is saved either way, so the command must not fail: the next launch
        // re-syncs the registry from the file.
        let failed = apply_autostart_update(true, Path::new("/x"), |_, _| {
            Err("could not write the Windows autostart entry: access denied".to_string())
        });
        assert!(!failed.applied);
        assert!(failed.detail.contains("access denied"), "{}", failed.detail);

        let not_this_os = apply_autostart_update(false, Path::new("/x"), |_, _| {
            Ok(AutostartOutcome::NotApplicable(
                "this build is not Windows".to_string(),
            ))
        });
        assert!(!not_this_os.applied);
        assert!(
            not_this_os.detail.contains("not Windows"),
            "{}",
            not_this_os.detail
        );
    }

    #[test]
    fn restart_is_required_only_for_a_running_recorder_with_changed_engine_keys() {
        let (dir, path) = settings_case();
        let clips = SettingsUpdate {
            clips_dir: Some("/tmp/clips".to_string()),
            ..Default::default()
        };

        let idle = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                fps: Some(30),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert!(!idle.restart_required, "idle: the next start picks it up");
        assert_eq!(idle.restart_reason, None);

        let running = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                fps: Some(60),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(running.restart_required);
        assert!(
            running
                .restart_reason
                .as_deref()
                .unwrap()
                .contains("encode.fps"),
            "the reason names the key: {:?}",
            running.restart_reason
        );

        let storage_only = update_settings_file(dir.path(), &path, &clips, true).unwrap();
        assert!(
            !storage_only.restart_required,
            "the clips directory applies without the engine"
        );

        // Re-applying the value already on disk changes nothing, so it demands nothing.
        let same = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                fps: Some(60),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert_eq!(
            same.applied,
            vec!["encode.fps"],
            "applied still says what was written"
        );
        assert!(!same.restart_required, "same value: no restart to demand");
    }

    #[test]
    fn a_rejected_update_writes_nothing_and_names_the_key() {
        let (dir, path) = settings_case();
        std::fs::write(&path, "[encode]\nfps = 30\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = update_settings_file(
            dir.path(),
            &path,
            &SettingsUpdate {
                fps: Some(0),
                ..Default::default()
            },
            false,
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("encode.fps"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn an_empty_clips_dir_resets_to_the_app_data_default() {
        let (dir, path) = settings_case();
        let custom = SettingsUpdate {
            clips_dir: Some("/tmp/clips".to_string()),
            ..Default::default()
        };
        let outcome = update_settings_file(dir.path(), &path, &custom, false).unwrap();
        assert_eq!(outcome.settings.clips_dir, "/tmp/clips");
        assert_eq!(outcome.settings.clips_dir_resolved, "/tmp/clips");

        let reset = SettingsUpdate {
            clips_dir: Some(String::new()),
            ..Default::default()
        };
        let outcome = update_settings_file(dir.path(), &path, &reset, false).unwrap();
        assert_eq!(outcome.settings.clips_dir, "");
        assert_eq!(
            outcome.settings.clips_dir_resolved,
            dir.path().join("clips").to_string_lossy(),
            "empty means the clips directory under the application data directory"
        );
    }

    // -- the ordering that keeps an unusable clips_dir out of the file -----------------

    #[test]
    fn a_clips_dir_that_cannot_be_created_is_never_written_to_the_config() {
        // The blocker this guards: writing the file first and creating the directory second would
        // persist `clips_dir = "Z:\\clips"` and fail the next launch in `AppState::open`
        // (`std::process::exit(1)`), leaving no window, no tray and no way back except hand-editing the
        // file.
        let (dir, path) = settings_case();
        std::fs::write(&path, "[storage]\nclips_dir = \"\"\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // A regular file standing where a directory must go: `create_dir_all` cannot succeed here on
        // any platform, as any user, which a permissions trick could not promise without root.
        let blocked = dir.path().join("a-file");
        std::fs::write(&blocked, "not a directory").unwrap();
        let wanted = blocked.join("clips").to_string_lossy().into_owned();

        let err = update_settings_and_prepare(
            dir.path(),
            &path,
            &SettingsUpdate {
                clips_dir: Some(wanted.clone()),
                ..Default::default()
            },
            false,
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::Io);
        assert!(
            err.message.contains("a-file"),
            "the path is named: {}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "an unusable directory must not reach the file the next launch reads"
        );
    }

    #[test]
    fn a_settings_update_hands_back_the_directory_it_created() {
        // The caller commits these paths to the running shell and re-scopes the asset protocol with
        // them. What matters here is that the directory exists by then.
        let (dir, path) = settings_case();
        let wanted = dir.path().join("clips-here");

        let (outcome, prepared) = update_settings_and_prepare(
            dir.path(),
            &path,
            &SettingsUpdate {
                clips_dir: Some(wanted.to_string_lossy().into_owned()),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        assert!(wanted.is_dir(), "created before the write returned");
        assert_eq!(prepared.expect("a clips_dir moved").clips_dir, wanted);
        assert_eq!(outcome.settings.clips_dir, wanted.to_string_lossy());
    }

    #[test]
    fn an_update_without_a_clips_dir_prepares_nothing() {
        // The shell's paths and the asset scope are touched only when the directory actually moved:
        // editing fps must not re-create or re-scope anything.
        let (dir, path) = settings_case();

        let (outcome, prepared) = update_settings_and_prepare(
            dir.path(),
            &path,
            &SettingsUpdate {
                fps: Some(30),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        assert!(prepared.is_none());
        assert_eq!(outcome.applied, vec!["encode.fps"]);
    }

    #[test]
    fn an_empty_clips_dir_reset_prepares_the_default_directory_too() {
        // "Empty resets to the default" is a clips_dir move like any other, so it goes through the same
        // create: the default directory must exist too, or the reset would persist a path the next
        // launch cannot open a window on.
        let (dir, path) = settings_case();

        let (_, prepared) = update_settings_and_prepare(
            dir.path(),
            &path,
            &SettingsUpdate {
                clips_dir: Some(String::new()),
                ..Default::default()
            },
            false,
        )
        .unwrap();

        let paths = prepared.expect("a clips_dir moved");
        assert_eq!(paths.clips_dir, dir.path().join("clips"));
        assert!(
            paths.clips_dir.is_dir(),
            "the default directory was created"
        );
    }

    #[test]
    fn a_start_is_built_with_the_files_microphone_and_watcher() {
        // The options a start is built with come from the file, not `RecorderOptions::default()` (no
        // mic, nothing watched).
        let engine =
            EngineOptionsConfig::from_toml("[mic]\nenabled = true\n[games]\nauto_record = true\n")
                .unwrap();

        let opts = recorder_options(&engine);

        assert!(
            opts.mic.enabled,
            "the configured microphone is passed through"
        );
        assert!(opts.games.auto_record, "and the configured watcher");
        assert!(
            opts.game.is_none(),
            "a person starting a recording is no detected game"
        );
        assert_eq!(
            opts.mode.as_str(),
            "buffer",
            "the window has no mode switch"
        );
    }

    #[test]
    fn the_asset_roots_are_the_clips_directory_and_the_thumbnail_cache() {
        // What `lib.rs` hands to the runtime asset-protocol scope: playback is scoped to the clips
        // directory, with the thumbnails cache served alongside. Nothing else in the filesystem is
        // reachable from the webview.
        let dir = PathBuf::from("/app-data/replayed");
        let paths = AppPaths::resolve(&dir, "");
        assert_eq!(
            paths.asset_roots(),
            [
                Path::new("/app-data/replayed/clips"),
                Path::new("/app-data/replayed/thumbnails")
            ]
        );
        assert_eq!(
            paths.db_path,
            dir.join("replayed.db"),
            "the same index file the CLI uses"
        );

        // A configured clips directory is honoured, and is still the first asset root.
        let custom = AppPaths::resolve(&dir, "/mnt/games/replayed-clips");
        assert_eq!(custom.clips_dir, PathBuf::from("/mnt/games/replayed-clips"));
        assert_eq!(
            custom.asset_roots()[0],
            Path::new("/mnt/games/replayed-clips")
        );
    }
}
