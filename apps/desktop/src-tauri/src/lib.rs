//! The Tauri shell: state, wiring, and the `#[tauri::command]` wrappers that delegate to
//! [`commands`].
//!
//! There is deliberately no logic in this file. Each wrapper resolves the application state and
//! hands the call to a plain function in `commands`; the background half (tray, hotkey thread,
//! window close) is wired here but decided in [`background`]. This is the one place that knows
//! Tauri's types, which keeps the command layer and background logic testable without a Tauri
//! runtime or a window. The wiring itself (the tray appearing, the window hiding, a key
//! arriving) needs a desktop and is not covered by tests.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

pub mod background;
pub mod clip_files;
pub mod commands;
pub mod config;
pub mod health;
mod log_file;
pub mod paths;

use background::{
    AppStatus, CloseAction, HotkeyStatus, MenuAction, Shell, TrayRendering, TrayState, TrayView,
};
use commands::{
    AppPaths, ClipDto, CommandError, DeleteOutcome, DeleteSessionOutcome, Deps, DiagnosticsReport,
    EncoderSettings, EncoderStatus, ErrorCode, ExportOutcome, PruneOutcome, RecordedClipDto,
    RecorderHost, RecordingStatusDto, SessionDto, SessionEventDto, StorageConfigView, StorageStats,
    ThumbnailRef,
};
use config::{BackgroundConfig, RecordingConfig, StorageConfig};
use replayed_media::FfmpegBinaries;
use replayed_recorder::GsiBinding;
use replayed_store::Store;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State, WindowEvent, Wry};

/// The one window this application has (`tauri.conf.json` declares it).
const MAIN_WINDOW: &str = "main";

/// How often the tray re-derives itself from the recorder.
///
/// The same half-second the window polls at: the counters are atomics, and the tray touches the
/// OS only when what it would show has changed.
const TRAY_POLL: Duration = Duration::from_millis(500);

/// Everything the commands run against, resolved once at startup.
pub struct AppState {
    /// The SQLite index the CLI writes and this shell reads. Behind a `Mutex` because
    /// `rusqlite::Connection` is `Send` but not `Sync`, and commands can run on several threads.
    store: Mutex<Store>,
    /// `None` when ffmpeg could not be found: the list and storage panel still work, and the two
    /// commands that need ffmpeg report why they cannot.
    bins: Option<FfmpegBinaries>,
    /// Where clips, thumbnails and the index live. Behind a `Mutex` because `update_settings` can
    /// move the clips directory while the app runs.
    paths: Mutex<AppPaths>,
    /// The storage policy and the clips directory setting it carries. Same lock story as `paths`.
    storage: Mutex<StorageConfig>,
    /// The application data directory the paths resolve against (and `config.toml` lives in). The
    /// recorder host holds its own copy for starts; this one is for settings.
    app_data_dir: PathBuf,
    warnings: Vec<String>,
    /// The recording this window started, if any: the engine the CLI also drives
    /// (`replayed-recorder`). A host rather than a bare handle because a start must resolve the
    /// application data directory and the ffmpeg sidecar binaries, both already here.
    recorder: RecorderHost,
    /// What installing the clip hotkey concluded: the chord, whether a listener is
    /// really there, and why not when it is not. The window shows this instead of assuming the
    /// hotkey works, since a hotkey that silently does nothing is the worst outcome of the feature.
    ///
    /// Behind a `Mutex` because it is not known when the state is built: the hotkey is installed
    /// in `setup`, after the single-instance plugin has decided whether this process is the one
    /// that stays (see `run`).
    hotkey: Mutex<HotkeyStatus>,
    /// The directories the clips directory may be: the ones the user approved.
    ///
    /// The asset protocol is the window's only read channel (`capabilities/default.json` grants
    /// no filesystem permission), so a command that widened the scope to a path the webview named
    /// would let the webview read any directory on the disk. A clips directory outside the
    /// application data directory is therefore one the user picked in the native folder picker
    /// (`choose_clips_dir`) or one the config file already names.
    approved_roots: Mutex<Vec<PathBuf>>,
    /// The log file this process writes, and the health page reads.
    log_path: PathBuf,
    /// The GSI cfgs this shell installed into games' own directories, and the binding each
    /// was written from.
    ///
    /// The port the GSI listener binds can change between runs (it is configurable, and it
    /// may be an ephemeral one the operating system picked), so an installed `.cfg` naming
    /// an old port is a game posting into the void. This record is what lets a recording
    /// start rewrite it. Shared with the recorder host, which does the rewriting when the
    /// listener comes up.
    installed_gsi: Arc<Mutex<Vec<health::InstalledGsi>>>,
    /// The encoder the last recording ran, remembered so the encoder panel can name it after
    /// the recording has stopped.
    ///
    /// The engine publishes the encoder's name in its status (`h264_nvenc`), but only while
    /// it is running: the status belongs to the recorder, and the recorder is gone once the
    /// recording stopped. The panel asks "which encoder did the smoke test choose", and that
    /// answer has to outlive the recording.
    last_encoder: Mutex<Option<String>>,
}

impl AppState {
    /// Open the index and resolve the paths, creating what is missing.
    ///
    /// The one step that fails hard: an index that cannot be opened can neither list nor delete
    /// anything, so the caller logs the reason and stops, as the CLI does.
    pub fn open(
        app_data_dir: &Path,
        storage: StorageConfig,
        log_path: PathBuf,
    ) -> Result<Self, CommandError> {
        std::fs::create_dir_all(app_data_dir).map_err(|e| {
            CommandError::new(
                ErrorCode::Io,
                format!(
                    "could not create the application data directory {}: {e}",
                    app_data_dir.display()
                ),
            )
        })?;

        let paths = AppPaths::resolve(app_data_dir, &storage.clips_dir);
        std::fs::create_dir_all(&paths.clips_dir).map_err(|e| {
            CommandError::new(
                ErrorCode::Io,
                format!(
                    "could not create the clips directory {}: {e}",
                    paths.clips_dir.display()
                ),
            )
        })?;

        let store = Store::open(&paths.db_path).map_err(|e| {
            CommandError::new(
                ErrorCode::Store,
                format!(
                    "could not open the clip index {}: {e:#}",
                    paths.db_path.display()
                ),
            )
        })?;
        store.migrate().map_err(|e| {
            CommandError::new(
                ErrorCode::Store,
                format!(
                    "could not migrate the clip index {}: {e:#}",
                    paths.db_path.display()
                ),
            )
        })?;

        // ffmpeg is a startup warning, not a failure: everything except trimming and thumbnails works
        // without it, and the user can fix a missing sidecar while the app runs. The warning goes to
        // the storage panel, where it is visible, not only to a log nobody reads.
        let (bins, warnings) = match FfmpegBinaries::discover(None) {
            Ok(bins) => {
                tracing::info!("using ffmpeg at {}", bins.ffmpeg.display());
                (Some(bins), Vec::new())
            }
            Err(err) => {
                tracing::warn!("{err:#}");
                let warning = format!(
                    "ffmpeg was not found, so clips cannot be trimmed or thumbnailed: {err:#}"
                );
                (None, vec![warning])
            }
        };

        // The startup summary says what is on disk, not what the index claims: a row whose file is
        // gone holds no bytes and is left out of both lists, and the count of them is what tells a
        // user why the library is smaller than the index (the storage panel offers the prune).
        let clips = store.list_clips().unwrap_or_default();
        let sessions = store.list_sessions().unwrap_or_default();
        let missing_clips = clips
            .iter()
            .filter(|clip| replayed_store::prune::clip_is_missing(clip))
            .count();
        let missing_sessions = sessions
            .iter()
            .filter(|session| replayed_store::prune::session_is_missing(session))
            .count();
        let live_bytes: u64 = clips
            .iter()
            .filter(|clip| !replayed_store::prune::clip_is_missing(clip))
            .map(|clip| clip.size_bytes)
            .sum();
        tracing::info!(
            "clip index {} (schema v{}): {} clip(s) on disk ({} row(s) name a file that is gone), \
             {} bytes on disk, {} session(s) ({} row(s) name a file that is gone)",
            paths.db_path.display(),
            store.schema_version().unwrap_or(-1),
            clips.len() - missing_clips,
            missing_clips,
            live_bytes,
            sessions.len() - missing_sessions,
            missing_sessions
        );

        // One record of the GSI cfgs this shell installed, shared with the recorder host:
        // the host rewrites an installed cfg when the listener's port moves, and the
        // install command is what records it.
        let installed_gsi = Arc::new(Mutex::new(Vec::new()));
        let recorder = RecorderHost::new(
            app_data_dir.to_path_buf(),
            bins.clone(),
            Arc::clone(&installed_gsi),
        );
        Ok(Self {
            store: Mutex::new(store),
            bins,
            paths: Mutex::new(paths),
            storage: Mutex::new(storage),
            app_data_dir: app_data_dir.to_path_buf(),
            warnings,
            recorder,
            hotkey: Mutex::new(HotkeyStatus::not_installed(
                config::DEFAULT_CLIP_HOTKEY,
                "the hotkey is installed when the shell has finished starting",
            )),
            approved_roots: Mutex::new(Vec::new()),
            log_path,
            installed_gsi,
            last_encoder: Mutex::new(None),
        })
    }

    /// The recording engine this window drives. No UI is built unless it can record.
    pub fn recorder(&self) -> &RecorderHost {
        &self.recorder
    }

    /// Record a GSI installation, so a later recording start can rewrite it.
    ///
    /// The port the GSI listener binds can change between runs, and a `.cfg` left in a game's
    /// folder naming an old one is a game posting into the void.
    pub fn remember_gsi_install(
        &self,
        game: health::GsiGame,
        install: &Path,
        path: &Path,
        binding: &GsiBinding,
    ) {
        let mut installed = self
            .installed_gsi
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        health::remember_install(&mut installed, game, install, path, binding);
    }

    /// What installing the clip hotkey concluded at startup.
    pub fn hotkey(&self) -> HotkeyStatus {
        self.hotkey
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    /// Record what installing the clip hotkey concluded.
    ///
    /// The hotkey is installed in `setup`, because that is where the single-instance plugin has
    /// already decided whether this process stays; the state is built before that.
    pub fn set_hotkey(&self, hotkey: HotkeyStatus) {
        *self.hotkey.lock().unwrap_or_else(|err| err.into_inner()) = hotkey;
    }

    /// Record a problem the window should show.
    ///
    /// The startup warnings are how the shell says "this works, but something is wrong": ffmpeg
    /// is missing, the config file could not be read. They go to the storage panel, where a user
    /// sees them, not only to a log nobody reads.
    pub fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    /// The log file this process writes.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The application's data directory: the one holding `clips/`, `sessions/`, `thumbnails/`,
    /// the index, the log and `config.toml`.
    ///
    /// Exposed for the window's "open the data folder" button, which is the answer to "where is
    /// everything" that does not require copying a path out of a page.
    pub fn data_dir(&self) -> &Path {
        &self.app_data_dir
    }

    /// The encoder the last recording ran: the live one while a recording is up, the remembered
    /// one after it has stopped.
    ///
    /// The engine's status carries the name (`h264_nvenc`) only while the recorder exists, so
    /// the live value is remembered on the way past. `None` before the first recording: the
    /// vendor is chosen by the startup smoke test, and that has not run yet.
    pub fn last_encoder(&self) -> Option<String> {
        if let Some(name) = self.recorder.raw_status().encoder {
            *self
                .last_encoder
                .lock()
                .unwrap_or_else(|err| err.into_inner()) = Some(name.clone());
            return Some(name);
        }
        self.last_encoder
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    /// The default clips directory: the one under the application data directory.
    pub fn default_clips_dir(&self) -> PathBuf {
        self.app_data_dir.join("clips")
    }

    /// Approve a directory the user picked, so a later update may name it.
    pub fn approve_clips_root(&self, dir: &Path) {
        let mut roots = self
            .approved_roots
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !roots.iter().any(|root| root == dir) {
            tracing::info!("the clips directory {} is approved", dir.display());
            roots.push(dir.to_path_buf());
        }
    }

    /// Whether `dir` is a clips directory this shell may use.
    ///
    /// Empty is the default and always allowed. Anything else must be an approved root or live
    /// inside one: the webview may not widen the asset protocol's scope to a path it named.
    ///
    /// A path with a `..` segment is refused whatever it starts with: the scope is resolved and
    /// not canonicalised, so `<approved>/../elsewhere` is a directory the approval does not name.
    pub fn clips_dir_allowed(&self, dir: &str) -> bool {
        if dir.is_empty() {
            return true;
        }
        let wanted = PathBuf::from(dir);
        if wanted
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return false;
        }
        self.approved_roots
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .any(|root| wanted == *root || wanted.starts_with(root))
    }

    /// The `config.toml` this process read: the file the tray's "open config file" item reveals and
    /// the window names, since configuration is a file rather than a settings dialog.
    pub fn config_path(&self) -> PathBuf {
        self.recorder.config_path()
    }

    /// The paths the asset protocol has to be allowed to serve.
    pub fn asset_roots(&self) -> Vec<PathBuf> {
        self.paths
            .lock()
            .map(|paths| {
                paths
                    .asset_roots()
                    .iter()
                    .map(|p| p.to_path_buf())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn poisoned(what: &str) -> CommandError {
        CommandError::new(
            ErrorCode::Store,
            format!("the clip index is unusable: an earlier command panicked while holding {what}"),
        )
    }

    /// Build the explicit dependencies for one command call and run it.
    ///
    /// The guard lives for the whole call, so a concurrent command cannot mutate the store halfway
    /// through; no command awaits, so holding it is cheap. Lock order is store, then paths, then
    /// storage; the settings command takes the latter two in that order and never the store, so
    /// nothing can deadlock.
    fn with_deps<T>(
        &self,
        run: impl FnOnce(&Deps<'_>) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        let store = self
            .store
            .lock()
            .map_err(|_| Self::poisoned("the store lock"))?;
        let paths = self
            .paths
            .lock()
            .map_err(|_| Self::poisoned("the paths lock"))?;
        let storage = StorageConfigView::from(
            &*self
                .storage
                .lock()
                .map_err(|_| Self::poisoned("the storage lock"))?,
        );
        let deps = Deps {
            store: &store,
            bins: self.bins.as_ref(),
            paths: &paths,
            storage: &storage,
            warnings: &self.warnings,
        };
        run(&deps)
    }
}

// ---------------------------------------------------------------------------------------
// The IPC commands. Every one of these is a delegation: resolve the state, call the plain
// function in `commands.rs`, hand back its result. Nothing is decided here.
// ---------------------------------------------------------------------------------------

/// Every clip in the index, newest first.
#[tauri::command(rename_all = "snake_case")]
fn list_clips(state: State<'_, AppState>) -> Result<Vec<ClipDto>, CommandError> {
    state.with_deps(commands::list_clips)
}

/// Usage, the configured cap and age limit, and the cleanup policy's verdict.
#[tauri::command(rename_all = "snake_case")]
fn storage_stats(state: State<'_, AppState>) -> Result<StorageStats, CommandError> {
    state.with_deps(commands::storage_stats)
}

/// Remove the index rows whose files are gone. The user confirms this in the Storage panel: it is
/// the one place the shell rewrites the index from the filesystem's evidence.
#[tauri::command(rename_all = "snake_case")]
fn prune_missing(state: State<'_, AppState>) -> Result<PruneOutcome, CommandError> {
    state.with_deps(commands::prune_missing)
}

/// Combine whole clips into one file, losslessly.
#[tauri::command(rename_all = "snake_case")]
fn combine_clips(state: State<'_, AppState>, ids: Vec<i64>) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::combine_clips(deps, &ids))
}

/// Re-encode a clip with one of the export presets, into a new file beside it.
#[tauri::command(rename_all = "snake_case")]
fn export_clip(
    state: State<'_, AppState>,
    id: i64,
    preset: String,
) -> Result<ExportOutcome, CommandError> {
    state.with_deps(|deps| commands::export_clip(deps, id, &preset))
}

/// Mark a clip as a favourite, or clear it.
#[tauri::command(rename_all = "snake_case")]
fn set_favourite(
    state: State<'_, AppState>,
    id: i64,
    favourite: bool,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_favourite(deps, id, favourite))
}

/// Rename a clip (its display title; the file keeps its name). Blank clears the title.
#[tauri::command(rename_all = "snake_case")]
fn set_clip_title(
    state: State<'_, AppState>,
    id: i64,
    title: String,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_clip_title(deps, id, &title))
}

/// Replace a clip's tags.
#[tauri::command(rename_all = "snake_case")]
fn set_clip_tags(
    state: State<'_, AppState>,
    id: i64,
    tags: Vec<String>,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_clip_tags(deps, id, &tags))
}

/// Put the clip's file on the system clipboard, as a file.
///
/// Async because it waits for PowerShell: the call blocks until the child process is done, and
/// the window must not stop answering while it does.
#[tauri::command(async, rename_all = "snake_case")]
fn copy_clip_to_clipboard(state: State<'_, AppState>, id: i64) -> Result<(), CommandError> {
    let path = state.with_deps(|deps| commands::clip_file(deps, id))?;
    clip_files::copy_file_to_clipboard(&path)
        .map_err(|msg| CommandError::new(commands::ErrorCode::Io, msg))
}

/// Show the clip's file in the system file manager, selected where the platform allows.
#[tauri::command(rename_all = "snake_case")]
fn reveal_clip(state: State<'_, AppState>, id: i64) -> Result<(), CommandError> {
    let path = state.with_deps(|deps| commands::clip_file(deps, id))?;
    let (program, args) = background::reveal_command(&path, background::FileManager::current());
    replayed_media::sidecar_command(&program)
        .args(&args)
        .spawn()
        .map(drop)
        .map_err(|err| {
            CommandError::new(
                commands::ErrorCode::Io,
                format!("could not run {program}: {err}"),
            )
        })
}

/// Show the application's data directory in the system file manager.
///
/// The folder that holds the clips, the sessions, the thumbnails, the index, the log and
/// `config.toml`: one button for "where is everything", instead of three paths printed on pages
/// for the user to copy.
#[tauri::command(rename_all = "snake_case")]
fn open_data_dir(state: State<'_, AppState>) -> Result<(), CommandError> {
    let dir = state.data_dir().to_path_buf();
    let (program, args) = background::reveal_command(&dir, background::FileManager::current());
    replayed_media::sidecar_command(&program)
        .args(&args)
        .spawn()
        .map(drop)
        .map_err(|err| {
            CommandError::new(
                commands::ErrorCode::Io,
                format!("could not run {program}: {err}"),
            )
        })
}

/// Begin a native drag of the clip's file out of the window (to Explorer, Discord, a browser
/// upload). Called when the pointer has started dragging a library row; the OS drag loop
/// then runs on the main thread until the drop or a cancel.
#[tauri::command(rename_all = "snake_case")]
fn drag_clip_out(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: i64,
) -> Result<(), CommandError> {
    let path = state.with_deps(|deps| commands::clip_file(deps, id))?;
    start_native_drag(&window, path)
}

#[cfg(not(target_os = "linux"))]
fn start_native_drag(window: &tauri::WebviewWindow, path: PathBuf) -> Result<(), CommandError> {
    let to_error = |msg: String| CommandError::new(commands::ErrorCode::Io, msg);
    let (tx, rx) = std::sync::mpsc::channel();
    let target = window.clone();
    window
        .run_on_main_thread(move || {
            let result = drag::start_drag(
                &target,
                drag::DragItem::Files(vec![path]),
                drag::Image::Raw(include_bytes!("../icons/64x64.png").to_vec()),
                |_result, _cursor| {},
                drag::Options::default(),
            );
            let _ = tx.send(result.map_err(|err| format!("could not start the drag: {err}")));
        })
        .map_err(|err| to_error(format!("could not reach the UI thread: {err}")))?;
    rx.recv()
        .map_err(|_| to_error("the drag never started".to_string()))?
        .map_err(to_error)
}

#[cfg(target_os = "linux")]
fn start_native_drag(_window: &tauri::WebviewWindow, _path: PathBuf) -> Result<(), CommandError> {
    Err(CommandError::new(
        commands::ErrorCode::Io,
        "dragging a clip out of the window is not supported on this platform".to_string(),
    ))
}

/// Cut `[start_ms, end_ms)` into a new clip file with a lossless stream copy.
///
/// Async because it runs ffmpeg: the cut takes as long as the footage does, up to a minute.
#[tauri::command(async, rename_all = "snake_case")]
fn trim_clip(
    state: State<'_, AppState>,
    id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::trim_clip(deps, id, start_ms, end_ms))
}

/// A cached JPEG thumbnail for one clip at one timestamp.
///
/// Async because it runs ffmpeg per clip, and a list of clips is a list of ffmpeg runs.
#[tauri::command(async, rename_all = "snake_case")]
fn thumbnail(
    state: State<'_, AppState>,
    id: i64,
    at_ms: u64,
) -> Result<ThumbnailRef, CommandError> {
    state.with_deps(|deps| commands::thumbnail(deps, id, at_ms))
}

/// Delete a clip: the row first, then the file.
#[tauri::command(rename_all = "snake_case")]
fn delete_clip(state: State<'_, AppState>, id: i64) -> Result<DeleteOutcome, CommandError> {
    state.with_deps(|deps| commands::delete_clip(deps, id))
}

// -- sessions ----------------------------------------------------------------------------
//
// The session recorder: one scratch directory per recording, concatenated when it stops. The
// timeline these commands read is computed by the store from the session's media epoch; see
// `commands::session_events` for why it must not be recomputed here.

/// Every session the index knows, newest first.
#[tauri::command(rename_all = "snake_case")]
fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionDto>, CommandError> {
    state.with_deps(commands::list_sessions)
}

/// One session's row, for the detail panel.
#[tauri::command(rename_all = "snake_case")]
fn session_detail(state: State<'_, AppState>, session_id: i64) -> Result<SessionDto, CommandError> {
    state.with_deps(|deps| commands::session_detail(deps, session_id))
}

/// A session's timeline markers, in media-time order.
#[tauri::command(rename_all = "snake_case")]
fn session_events(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<Vec<SessionEventDto>, CommandError> {
    state.with_deps(|deps| commands::session_events(deps, session_id))
}

/// A session split into its matches (Full Match segments), with markers and scoreboards.
#[tauri::command(rename_all = "snake_case")]
fn session_matches(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<Vec<commands::MatchDto>, CommandError> {
    state.with_deps(|deps| commands::session_matches(deps, session_id))
}

/// Cut one match out of a finished session as a clip.
#[tauri::command(rename_all = "snake_case")]
fn extract_match(
    state: State<'_, AppState>,
    session_id: i64,
    match_index: usize,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::extract_match(deps, session_id, match_index))
}

/// Mark a session as a favourite, or clear it.
#[tauri::command(rename_all = "snake_case")]
fn set_session_favourite(
    state: State<'_, AppState>,
    session_id: i64,
    favourite: bool,
) -> Result<SessionDto, CommandError> {
    state.with_deps(|deps| commands::set_session_favourite(deps, session_id, favourite))
}

/// Delete a session: the row first, then its segments and its recorded file.
///
/// Async because deleting a session removes a whole directory of footage, which takes as long as
/// the disk needs.
#[tauri::command(async, rename_all = "snake_case")]
fn delete_session(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<DeleteSessionOutcome, CommandError> {
    state.with_deps(|deps| commands::delete_session(deps, session_id))
}

/// Cut a clip out of a finished session, losslessly. Refused while it is still recording.
#[tauri::command(rename_all = "snake_case")]
fn extract_clip(
    state: State<'_, AppState>,
    session_id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::extract_clip(deps, session_id, start_ms, end_ms))
}

// -- recording ---------------------------------------------------------------------------
//
// The recording commands are `async`, unlike most of the others, because tauri runs a
// synchronous command on the webview's own thread and these block for real time: `clip_now`
// waits for the post-roll (seconds), and a start runs the encoder smoke test. An `async fn`
// runs on the runtime's workers, keeping the window responsive while a clip is saved.
//
// The same applies to the commands that are not about recording but block just as long: a
// `trim_clip` runs ffmpeg over the whole window, a `thumbnail` runs ffmpeg per clip, a
// `delete_session` removes a directory of footage, `copy_clip_to_clipboard` waits for
// PowerShell, `list_audio_devices` waits for the thread that owns the COM apartment, and
// `update_settings` writes the file and flushes it to disk. Each of those carries
// `#[tauri::command(async)]`, which dispatches a synchronous body to a thread pool — the
// signature does not have to change for that.

/// Start recording through the shared engine.
#[tauri::command(rename_all = "snake_case")]
async fn start_recording(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    // Read at start time, not window-open time: a capture-only mistake must not stop the review
    // pane from opening (see `config.rs`).
    let settings = RecordingConfig::load(&state.recorder.config_path())?;
    state.recorder.start(&settings)
}

/// Stop recording and flush the encoder. Idempotent.
#[tauri::command(rename_all = "snake_case")]
async fn stop_recording(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    state.recorder.stop()
}

/// The live recording status. Answers "not running" before anything has been started.
#[tauri::command(rename_all = "snake_case")]
async fn recording_status(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    state.recorder.status()
}

/// Take a clip now: wait for the post-roll, splice it losslessly, index it.
#[tauri::command(rename_all = "snake_case")]
async fn clip_now(state: State<'_, AppState>) -> Result<RecordedClipDto, CommandError> {
    state.recorder.clip_now()
}

/// What the window shows about the half of the shell that is not a window: the clip hotkey,
/// where the configuration was read from, and what closing the window does. It puts on screen
/// which chord to press and whether it is actually installed, and makes "the config is a file"
/// a readable path.
#[tauri::command(rename_all = "snake_case")]
fn app_status(state: State<'_, AppState>) -> Result<AppStatus, CommandError> {
    let config_path = state.config_path();
    Ok(AppStatus {
        hotkey: state.hotkey(),
        config_exists: config_path.is_file(),
        config_path: config_path.to_string_lossy().into_owned(),
        close_hint: background::CLOSE_HINT.to_string(),
    })
}

/// The health page: recorder, encoder, disk, event sources and the last problems the log holds.
#[tauri::command(rename_all = "snake_case")]
fn health(state: State<'_, AppState>) -> Result<health::HealthDto, CommandError> {
    let status = state.recorder.raw_status();
    let log_path = state.log_path().to_path_buf();
    let clips_dir = state
        .paths
        .lock()
        .map_err(|_| AppState::poisoned("the paths lock"))?
        .clips_dir
        .clone();
    Ok(health::HealthDto {
        recorder_state: health::recorder_state(&status),
        recorder_error: status.error.clone(),
        encoder: status.encoder.clone(),
        ffmpeg_available: state.bins.is_some(),
        disk: health::disk_health(&clips_dir),
        event_sources: health::event_source_dtos(&state.recorder.event_reports()),
        recent_errors: health::read_recent_problems(&log_path, 20),
        log_path: log_path.to_string_lossy().into_owned(),
    })
}

/// Which of CS2 / Dota 2 are installed in a Steam library, and whether the cfg is in place.
#[tauri::command(rename_all = "snake_case")]
fn gsi_targets() -> Vec<health::GsiTarget> {
    health::gsi_targets(&health::steam_libraries(&health::steam_roots()))
}

/// Assemble a diagnostics report, copy it to the clipboard, and hand it back.
///
/// The report carries the version, the platform, the effective configuration (with every
/// credential redacted), the recent problems and the tail of the log. It is copied to the
/// clipboard so a user can paste it into a bug report without attaching anything by hand, and
/// returned so the panel can show what was copied.
///
/// Async because it waits for the clipboard: the call blocks until the child process is done,
/// and the window must not stop answering while it does.
#[tauri::command(async, rename_all = "snake_case")]
fn copy_diagnostics(state: State<'_, AppState>) -> Result<DiagnosticsReport, CommandError> {
    commands::copy_diagnostics(&state.app_data_dir, &state.config_path(), state.log_path())
}

/// The effective encoder settings, for the encoder panel.
#[tauri::command(rename_all = "snake_case")]
fn encoder_settings(state: State<'_, AppState>) -> Result<EncoderSettings, CommandError> {
    commands::encoder_settings(&state.config_path())
}

/// Validate the encoder settings, write them, and report what was actually stored.
///
/// The settings are baked into the encoder when a recording starts, so a change takes effect on
/// the next start — the panel says so; the backend does not restart anything.
#[tauri::command(async, rename_all = "snake_case")]
fn update_encoder_settings(
    state: State<'_, AppState>,
    settings: EncoderSettings,
) -> Result<EncoderSettings, CommandError> {
    commands::update_encoder_settings(&state.config_path(), &settings)
}

/// Which encoder was chosen, and what it can do.
///
/// The capabilities depend on the path the recording will take, not only on the vendor: the
/// zero-copy Media Foundation path cannot scale and has no presets, and the ffmpeg-child path
/// can. See `commands::encoder`.
#[tauri::command(rename_all = "snake_case")]
fn encoder_status(state: State<'_, AppState>) -> Result<EncoderStatus, CommandError> {
    let last = state.last_encoder();
    commands::encoder_status(&state.config_path(), last.as_deref())
}

/// Write the GSI cfg for one game into its detected install folder.
///
/// The webview names only the game; the directory is found here, never supplied (see
/// `health.rs`).
///
/// The file is written from the *live* binding the running listener serves: the port it
/// bound and the token it accepts. Nothing is re-read from the token file, because the
/// listener read that file once, when it started, and a file replaced since would leave the
/// game posting a token the listener refuses — every POST answered `403` until the
/// application restarts.
///
/// With no listener running there is nothing for the game to post to, so the install is
/// refused with the reason rather than writing a file that cannot work.
#[tauri::command(rename_all = "snake_case")]
fn install_gsi_cfg(
    state: State<'_, AppState>,
    game: health::GsiGame,
) -> Result<health::GsiInstallOutcome, CommandError> {
    let roots = health::steam_roots();
    // The libraries Steam knows about: the roots the operating system names, plus the
    // secondary libraries the user's own `libraryfolders.vdf` lists.
    let libraries = health::steam_libraries(&roots);
    let install = health::find_install(game, &libraries).ok_or_else(|| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("{} was not found in any Steam library", game.label()),
        )
    })?;
    let configured_port = config::EngineOptionsConfig::load(&state.config_path())?
        .events
        .gsi_port;
    let binding = state
        .recorder
        .gsi_bindings()
        .into_iter()
        .find(|binding| binding.game == game.game_id());
    let listener = match binding.as_ref() {
        Some(binding) => health::GsiListener::Running(binding),
        // The listener is off in the configuration: say so with the setting's name.
        None if configured_port == 0 => health::GsiListener::SwitchedOff,
        None => health::GsiListener::NotRunning,
    };
    let outcome = health::install_gsi_cfg(game, &install, listener, &libraries)?;
    // Remembered so a later start can rewrite it when the listener's port moves.
    if let Some(binding) = binding {
        state.remember_gsi_install(game, &install, Path::new(&outcome.path), &binding);
    }
    Ok(outcome)
}

/// The effective settings, for the settings panel: the file's values where it sets them,
/// the example's where it does not.
#[tauri::command(rename_all = "snake_case")]
fn get_settings(state: State<'_, AppState>) -> Result<commands::SettingsDto, CommandError> {
    commands::read_settings(&state.app_data_dir, &state.config_path())
}

/// The active microphones and outputs, for the device pickers. Empty off Windows.
///
/// Listed on a thread of its own: enumeration takes a COM apartment, and the UI thread's is not
/// ours to change. Async because the join waits for that thread: a synchronous command would
/// wait on the UI thread for it.
#[tauri::command(async, rename_all = "snake_case")]
fn list_audio_devices() -> Result<replayed_capture::audio_devices::AudioDevices, CommandError> {
    std::thread::spawn(replayed_capture::audio_devices::list_audio_devices)
        .join()
        .map_err(|_| CommandError::new(ErrorCode::Recording, "listing audio devices panicked"))?
        .map_err(|e| CommandError::new(ErrorCode::Recording, format!("{e:#}")))
}

/// The displays this machine has, and the one the capture is pointed at.
///
/// Read so the framerate setting can be made against the display's own refresh rate: a capture
/// rate above the display's rate buys nothing, because the display has nothing new to show.
/// Empty off Windows, where there is no screen to enumerate.
#[tauri::command(async, rename_all = "snake_case")]
fn capture_displays(
    state: State<'_, AppState>,
) -> Result<commands::CaptureDisplaysDto, CommandError> {
    // The settings may be unreadable — a malformed config starts the shell degraded — and the
    // displays are worth reporting either way, so a failure here costs the selection, not the
    // answer. An unknown target is honest; a guessed one is not.
    let selected = commands::read_settings(&state.app_data_dir, &state.config_path())
        .ok()
        .map(|settings| settings.capture_monitor);
    Ok(commands::CaptureDisplaysDto {
        displays: replayed_capture::displays(),
        selected,
    })
}

/// Pick the clips directory, in the native folder picker, and approve it.
///
/// The picker runs here rather than in the webview because its answer decides what the webview
/// may read: the asset protocol's scope is the window's only read channel (`capabilities/default.json`
/// grants no filesystem permission), so a directory the webview names is a directory the webview
/// could have named for itself. What the user picks in a native dialog is an approval, and it is
/// recorded on this side.
///
/// The path this returns may be handed to `update_settings` as `clips_dir`: it is approved, so
/// the write is allowed.
#[tauri::command(async, rename_all = "snake_case")]
fn choose_clips_dir(app: AppHandle, state: State<'_, AppState>) -> Result<String, CommandError> {
    let path = pick_clips_folder(&app, None)?;
    state.approve_clips_root(Path::new(&path));
    Ok(path)
}

/// The native folder picker, and what the user picked in it.
///
/// `start_at` is where the dialog opens, when the caller knows a folder the user was aiming at.
/// The picker runs on the main thread and the answer comes back on this one, so the command
/// that waits for it must not be the UI thread's.
fn pick_clips_folder(app: &AppHandle, start_at: Option<&str>) -> Result<String, CommandError> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut builder = app.dialog().file().set_title("Choose the clips directory");
    if let Some(start) = start_at.map(Path::new).filter(|start| start.is_dir()) {
        builder = builder.set_directory(start);
    }
    builder.pick_folder(move |folder| {
        let _ = tx.send(folder);
    });
    let picked = rx.recv().map_err(|_| {
        CommandError::new(
            ErrorCode::Io,
            "the folder picker did not answer: the window may be closing",
        )
    })?;
    let Some(folder) = picked else {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            "no folder was chosen, so the clips directory was not changed".to_string(),
        ));
    };
    let path = folder.into_path().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("the chosen folder is not a path this shell can use: {err}"),
        )
    })?;
    Ok(path.to_string_lossy().into_owned())
}

/// Edit settings: validate, prove the clips directory usable, write the file, apply what
/// applies live.
///
/// `storage.clips_dir` moves the shell's directories immediately (future writes only); fps,
/// output size, mic and auto-record persist and take effect on the next recorder start, and
/// `restart_required` says so when a recording is running and one of them changed. Never an
/// implicit restart.
///
/// The clips directory is created before the file is written, and the order is load-bearing:
/// the next launch reads the file before it can open a window, so a directory that cannot be
/// made must never get that far. See `commands::update_settings_and_prepare`.
///
/// Async because it writes the file and creates a directory: both are filesystem work, and the
/// write is flushed to disk before it returns.
#[tauri::command(async, rename_all = "snake_case")]
fn update_settings(
    state: State<'_, AppState>,
    app: AppHandle,
    update: commands::SettingsUpdate,
) -> Result<commands::UpdateSettingsOutcome, CommandError> {
    // The clips directory decides what the window may read: the asset protocol is its only read
    // channel, so a directory it may name is a directory it may read. A path the user has not
    // approved is therefore asked for in the native folder picker, and what the user picks
    // there is what is written — the file is the user's own, and the next launch trusts it.
    let mut update = update;
    if let Some(dir) = update.clips_dir.clone() {
        if !state.clips_dir_allowed(&dir) {
            let picked = pick_clips_folder(&app, Some(&dir))?;
            state.approve_clips_root(Path::new(&picked));
            update.clips_dir = Some(picked);
        }
    }
    let running = state.recorder().is_running()?;
    // `update_settings_and_prepare` creates the new clips directory before it writes anything, so
    // what is left here, committing it to the running shell and re-scoping the asset protocol, is
    // all that needs a window.
    let (outcome, prepared) = commands::update_settings_and_prepare(
        &state.app_data_dir,
        &state.config_path(),
        &update,
        running,
    )?;
    if let Some(paths) = prepared {
        commit_clips_dir(&state, &app, paths, &outcome.settings.clips_dir)?;
    }
    Ok(outcome)
}

/// Point the running shell at a clips directory that exists and is already persisted.
///
/// The index and store handle are untouched (the database lives at the application data root
/// regardless); only future writes go to the new directory. A scope failure is a warning, not a
/// rollback, as in `setup`: the config already names the directory.
///
/// `prepare_clips_dir` has already made the directory and the value is in the file, so a failure
/// here must not look like a failed update.
fn commit_clips_dir(
    state: &AppState,
    app: &AppHandle,
    paths: AppPaths,
    clips_dir: &str,
) -> Result<(), CommandError> {
    let previous = {
        let mut guard = state
            .paths
            .lock()
            .map_err(|_| AppState::poisoned("the paths lock"))?;
        let previous = guard.clips_dir.clone();
        *guard = paths.clone();
        previous
    };
    state
        .storage
        .lock()
        .map_err(|_| AppState::poisoned("the storage lock"))?
        .clips_dir = clips_dir.to_string();

    // The scope is rebuilt, not widened: the directory the window could read before is taken out
    // of it, so moving the clips directory does not leave the old one readable for the rest of
    // the session. The default directory is the exception — the static scope in `tauri.conf.json`
    // covers it, and a forbidden pattern cannot be taken back, so forbidding it would break the
    // default for good.
    let scope = app.asset_protocol_scope();
    if previous != paths.clips_dir && previous != state.default_clips_dir() {
        if let Err(err) = scope.forbid_directory(&previous, true) {
            tracing::warn!(
                "the asset protocol was not taken off {}: {err}. Files in that directory may \
                 still be readable.",
                previous.display()
            );
        }
    }
    if let Err(err) = scope.allow_directory(&paths.clips_dir, true) {
        tracing::warn!(
            "the asset protocol was not scoped to {}: {err}. Clips in that directory will \
             not play.",
            paths.clips_dir.display()
        );
    }
    Ok(())
}

/// The clip hotkey the config asks for, installed, and the channel a press arrives on.
///
/// Split out of `setup` so that what it decides can be tested without a window: the chord, the
/// optional chords beside it, and the reason when nothing could be installed.
fn install_clip_hotkey(
    cfg: &BackgroundConfig,
) -> (
    HotkeyStatus,
    Option<std::sync::mpsc::Receiver<background::HotkeyAction>>,
) {
    let mut extras = Vec::new();
    if let Some(chord) = &cfg.toggle_recording_hotkey {
        extras.push((background::HotkeyAction::ToggleRecording, chord.as_str()));
    }
    if let Some(chord) = &cfg.bookmark_hotkey {
        extras.push((background::HotkeyAction::Bookmark, chord.as_str()));
    }
    background::install_hotkey(&cfg.clip_hotkey, &extras)
}

/// Open the shell's state, falling back to the example's settings when the config file cannot be
/// read.
///
/// A config that cannot be parsed is a warning, not a refusal: the review window, the tray and
/// the hotkey all work whatever the file says, and the settings panel is where a user repairs
/// it. The CLI refuses to start on a bad file, because it has no window to say why in.
///
/// The one thing that ends the process is an index that cannot be opened: there is nothing to
/// show and nothing to repair it from.
fn open_state(
    app_data_dir: &Path,
    config_path: &Path,
    log_path: PathBuf,
) -> Result<AppState, CommandError> {
    let (storage, warning) = StorageConfig::load_or_example(config_path);
    let mut state = match AppState::open(app_data_dir, storage.clone(), log_path.clone()) {
        Ok(state) => state,
        Err(err) => {
            tracing::error!("{err}");
            // The clips directory the file named could not be made: a relative path resolved
            // against a working directory that is not there, say (a Start-Menu launch has
            // `C:\Windows\System32` as its working directory). The default directory is under
            // the application data directory, which is where the index lives too, so it is the
            // one that can be relied on.
            let mut state = AppState::open(app_data_dir, StorageConfig::fallback(), log_path)
                .map_err(|err| {
                    tracing::error!("{err}");
                    err
                })?;
            state.warn(format!(
                "the clips directory in the config file could not be used, so the default one \
                 under the application data directory is in force: {}",
                err.message
            ));
            state
        }
    };
    if let Some(warning) = warning {
        state.warn(warning);
    }
    // The directories the clips directory may be: the application data directory, and whatever
    // the config file names. The config file is the user's own, and the shell only ever writes
    // a directory into it that the user approved (see `update_settings`).
    state.approve_clips_root(app_data_dir);
    let configured = state
        .storage
        .lock()
        .map_err(|_| AppState::poisoned("the storage lock"))?
        .clips_dir
        .clone();
    if !configured.is_empty() {
        state.approve_clips_root(Path::new(&configured));
    }
    Ok(state)
}

/// Start the desktop application.
///
/// No test calls this: it opens a window. Everything it wires together is verified through the
/// command layer and `background`'s pure functions (see `background.rs` for what that does and
/// does not prove).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let log_path = init_tracing();

    let app_data_dir = config::app_data_dir();
    let config_path = app_data_dir.join("config.toml");

    // The config story, said once at startup as the CLI does: which file, and whether it was there.
    // A user who just edited `[hotkeys] clip` reads the path here.
    let background_cfg = BackgroundConfig::load(&config_path);
    if config_path.is_file() {
        tracing::info!("config: {}", config_path.display());
    } else {
        tracing::info!(
            "config: {} does not exist; the values in config.example.toml are in force",
            config_path.display()
        );
    }

    // The state is built before the window, so a command has something to answer from the moment
    // the window exists. A config file that could not be read is a warning on the state, not a
    // reason to end the process: see `open_state`.
    let mut state = match open_state(&app_data_dir, &config_path, log_path) {
        Ok(state) => state,
        Err(err) => {
            // The index could not be opened. There is no window to show that in and no way to
            // repair it from, so the process ends; the reason is in the log.
            tracing::error!("{err}");
            eprintln!("replayed: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = &background_cfg {
        // Said out loud, because the two things this config decides — the hotkey and the
        // start-with-Windows entry — are both off, and nothing else says so.
        state.warn(format!(
            "the config file could not be read, so no hotkey is installed and the \
             start-with-Windows setting was not applied: {}",
            err.message
        ));
    }
    let asset_roots = state.asset_roots();

    tauri::Builder::default()
        // A second launch must hand over to the running instance (which owns the hotkey, tray and
        // capture session) instead of fighting it. Registered first, as the plugin requires.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        // The settings panel's folder picker (open dialogs only; see capabilities/default.json).
        .plugin(tauri_plugin_dialog::init())
        // The "clip saved" toast and sound (see `RecorderHost::on_clip_saved`).
        .plugin(tauri_plugin_notification::init())
        // The updater: the one plugin in this shell that reaches another machine, and it
        // does so only when the user presses "Check for updates" in the settings panel.
        // It is registered here, not started here — the plugin has no background work at
        // all: the endpoint and the minisign public key come from `plugins.updater` in
        // tauri.conf.json, and every artifact is verified against that key before it is
        // installed.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            // The hotkey is installed here, and not before the builder: Tauri runs the plugins'
            // setup before the application's own, and the single-instance plugin's check is one
            // of them. A second launch is therefore gone before it can reach `RegisterHotKey` —
            // which is the whole point, because the chord belongs to the instance that stays.
            // (Registering it earlier handed the chord to whichever process lost the race, and
            // the survivor was left with no hotkey and a message naming the wrong cause.)
            let (hotkey_status, presses) = match &background_cfg {
                Ok(cfg) => install_clip_hotkey(cfg),
                Err(err) => (
                    HotkeyStatus::not_installed(
                        "(none)",
                        format!(
                            "the config file could not be read, so no hotkey is installed: {}",
                            err.message
                        ),
                    ),
                    None,
                ),
            };
            match &hotkey_status.error {
                // Loud on purpose: otherwise invisible until a user presses the key in a game and nothing
                // happens.
                Some(error) => tracing::error!("the clip hotkey is NOT installed — {error}"),
                None => tracing::info!("the clip hotkey {} is installed", hotkey_status.chord),
            }
            state.set_hotkey(hotkey_status.clone());

            // Playback is served by Tauri's asset protocol, scoped to the clips directory; there is no
            // file server. `tauri.conf.json` carries the static scope for the default layout; this adds
            // the directories this process resolved, which makes a configured `storage.clips_dir` outside
            // the application data directory playable at all.
            let scope = app.asset_protocol_scope();
            for dir in &asset_roots {
                if let Err(err) = scope.allow_directory(dir, true) {
                    tracing::warn!(
                        "the asset protocol was not scoped to {}: {err}. Clips in that \
                         directory will not play.",
                        dir.display()
                    );
                }
            }
            // A toast and system sound whenever a clip lands, from any trigger. Best effort: a
            // notification that cannot show is a warning, never a reason to lose the clip it announces.
            let notifier = app.handle().clone();
            state.recorder().on_clip_saved(move |clip| {
                use tauri_plugin_notification::NotificationExt;
                let name = background::clip_file_name(&clip.path);
                if let Err(err) = notifier
                    .notification()
                    .builder()
                    .title("Clip saved")
                    .body(name)
                    .sound("default")
                    .show()
                {
                    tracing::warn!("the clip-saved notification could not be shown: {err}");
                }
            });
            app.manage(state);

            // The start-with-Windows entry, if the config asks for it. Best effort and never fatal: a
            // failed autostart write must not stop a recording session. Windows only; elsewhere it reports
            // why it did nothing. A config file that could not be read is said out loud too: the entry
            // is not applied, and nothing else would say so.
            match &background_cfg {
                Ok(cfg) => match std::env::current_exe() {
                    Ok(exe) => match background::apply_autostart(cfg.start_with_system, &exe) {
                        Ok(outcome) => tracing::info!(
                            "start_with_system = {}: {}",
                            cfg.start_with_system,
                            outcome.describe()
                        ),
                        Err(err) => tracing::warn!("autostart was not applied: {err}"),
                    },
                    Err(err) => tracing::warn!("autostart was not applied: {err}"),
                },
                Err(err) => tracing::warn!(
                    "autostart was not applied: the config file could not be read: {}",
                    err.message
                ),
            }

            if let Err(err) = wire_background(app.handle().clone(), hotkey_status.clone(), presses)
            {
                // A shell with no tray and no hotkey is still a review window, so this is a warning, not a
                // refusal to start, but never silent.
                tracing::error!("the tray and the hotkey thread were not installed: {err}");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_clips,
            storage_stats,
            prune_missing,
            combine_clips,
            export_clip,
            open_data_dir,
            set_favourite,
            set_clip_title,
            set_clip_tags,
            copy_clip_to_clipboard,
            reveal_clip,
            drag_clip_out,
            trim_clip,
            thumbnail,
            delete_clip,
            list_sessions,
            session_detail,
            session_events,
            session_matches,
            extract_match,
            set_session_favourite,
            delete_session,
            extract_clip,
            start_recording,
            stop_recording,
            recording_status,
            clip_now,
            app_status,
            get_settings,
            list_audio_devices,
            capture_displays,
            update_settings,
            choose_clips_dir,
            health,
            gsi_targets,
            install_gsi_cfg,
            copy_diagnostics,
            encoder_settings,
            update_encoder_settings,
            encoder_status,
            log_from_frontend
        ])
        .run(tauri::generate_context!())
        .expect("error while running the replayed desktop shell");
}

/// Record an error the frontend hit, on the same stream as the backend's own.
///
/// A JavaScript error (a failed `invoke`, a render exception, a rejected promise) otherwise
/// lives only in a console nobody can open on a windowed app started from the Start Menu.
///
/// Deliberately trivial: what arrives from the webview is untrusted text, so it is only logged,
/// never parsed or acted on.
///
/// Untrusted text still has to be *safe* text. A newline in the message would end the log line
/// and let the rest of it read as a backend line, and an unbounded message would churn the
/// rotation set. So the text is flattened to one line and capped, and the line is tagged as the
/// webview's so a forged backend line is distinguishable from a real one.
#[tauri::command]
fn log_from_frontend(level: String, message: String, detail: Option<String>) {
    let detail = detail.unwrap_or_default();
    let detail = detail.trim();
    let message = flatten_for_log(&message);
    let detail = flatten_for_log(detail);
    match level.as_str() {
        "error" => {
            tracing::error!(target: "replayed_desktop::frontend", "[webview] {message}{}{detail}", if detail.is_empty() { "" } else { " — " })
        }
        "warn" => {
            tracing::warn!(target: "replayed_desktop::frontend", "[webview] {message}{}{detail}", if detail.is_empty() { "" } else { " — " })
        }
        _ => {
            tracing::info!(target: "replayed_desktop::frontend", "[webview] {message}{}{detail}", if detail.is_empty() { "" } else { " — " })
        }
    }
}

/// The longest webview line the log will take, in characters.
const FRONTEND_LOG_MAX_CHARS: usize = 2_000;

/// Flatten untrusted text into one safe log line: control characters (a newline among them)
/// become spaces, runs of whitespace collapse, and the result is capped.
fn flatten_for_log(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(FRONTEND_LOG_MAX_CHARS));
    let mut last_was_space = false;
    for ch in text.chars() {
        if out.chars().count() >= FRONTEND_LOG_MAX_CHARS {
            out.push('…');
            break;
        }
        if ch.is_control() || ch == '\u{7f}' {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        out.push(ch);
        last_was_space = false;
    }
    out.trim().to_string()
}

/// Install the tray and start the two threads that keep it (and the hotkey) live. Called once,
/// from `setup`; every decision it makes is `background`'s.
fn wire_background(
    app: AppHandle,
    hotkey: HotkeyStatus,
    presses: Option<std::sync::mpsc::Receiver<background::HotkeyAction>>,
) -> tauri::Result<()> {
    let shell = AppShell { app };
    let icons = TrayIcons::load()?;
    let wiring = Arc::new(TrayWiring::install(&shell, &hotkey, icons)?);
    // The menu callback only gets the handle, so the wiring is managed too and looked up by type.
    // (`AppState` was managed by the caller, before any of this.)
    shell.app.manage(Arc::clone(&wiring));

    // The tray follows the recorder on its own: a hotkey press, a tray action and the engine
    // stopping by itself all move the icon, and none goes through the webview. This thread lives
    // for the process.
    {
        let shell = shell.clone();
        let wiring = Arc::clone(&wiring);
        let hotkey = hotkey.clone();
        std::thread::Builder::new()
            .name("replayed-tray".into())
            .spawn(move || loop {
                std::thread::sleep(TRAY_POLL);
                wiring.refresh(&shell, &hotkey);
            })?;
    }

    // The press loop. `on_hotkey_press` is the single trigger: one press, one
    // `RecorderHost::clip_now`, the call the window's Save clip button makes.
    if let Some(presses) = presses {
        let shell = shell.clone();
        let wiring = Arc::clone(&wiring);
        let hotkey = hotkey.clone();
        std::thread::Builder::new()
            .name("replayed-hotkey-press".into())
            .spawn(move || {
                // `recv` blocks until a press or the channel closes (the listener thread ending, at exit).
                while let Ok(action) = presses.recv() {
                    match action {
                        background::HotkeyAction::Clip => {
                            let outcome = background::on_hotkey_press(|| shell.clip_now());
                            if outcome.saved_a_clip() {
                                tracing::info!("{} pressed: {}", hotkey.chord, outcome.describe());
                            } else {
                                tracing::warn!("{} pressed: {}", hotkey.chord, outcome.describe());
                            }
                            wiring.set_last_press(outcome.describe());
                        }
                        background::HotkeyAction::ToggleRecording => {
                            let outcome = background::dispatch(
                                background::MenuAction::ToggleRecording,
                                &shell,
                            );
                            tracing::info!("the recording hotkey: {}", outcome.describe());
                            wiring.set_last_press(outcome.describe());
                        }
                        background::HotkeyAction::Bookmark => {
                            let note = match shell.state().and_then(|s| s.recorder().bookmark()) {
                                Ok(()) => "bookmarked".to_string(),
                                Err(err) => err.message,
                            };
                            tracing::info!("the bookmark hotkey: {note}");
                            wiring.set_last_press(note);
                        }
                    }
                    wiring.refresh(&shell, &hotkey);
                }
            })?;
    }

    // The window's close hides rather than quits (`background::close_action`), which keeps a
    // recording alive when a user tidies their screen.
    if let Some(window) = shell.window() {
        let shell = shell.clone();
        let wiring = Arc::clone(&wiring);
        let hotkey = hotkey.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if background::close_action() == CloseAction::Hide {
                    api.prevent_close();
                    if let Err(err) = shell.hide_window() {
                        tracing::warn!("the window was closed but could not be hidden: {err}");
                    } else {
                        tracing::info!(
                            "the window is hidden: recording continues, and replayed stays in \
                             the tray. Quit from the tray menu (there is no other way out)."
                        );
                    }
                    wiring.refresh(&shell, &hotkey);
                }
            }
        });
    }

    wiring.refresh(&shell, &hotkey);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The Tauri half of `background`: the window, the recorder, the tray and the file manager.
// Nothing here decides anything; every branch is a value from `background`.
// ---------------------------------------------------------------------------------------

/// The real [`Shell`]: the window, the recorder and the platform's file manager.
#[derive(Clone)]
struct AppShell {
    app: AppHandle,
}

impl AppShell {
    fn window(&self) -> Option<tauri::WebviewWindow> {
        self.app.get_webview_window(MAIN_WINDOW)
    }

    /// The application state, or the reason there is none.
    ///
    /// `try_state` rather than `state`: it is managed during `setup`, and a tray event arriving
    /// earlier must be reported, not panic.
    fn state(&self) -> Result<State<'_, AppState>, CommandError> {
        self.app.try_state::<AppState>().ok_or_else(|| {
            CommandError::new(
                ErrorCode::Recording,
                "the application state is not available yet",
            )
        })
    }

    /// Whether `config.toml` exists; the tray's "open config" label says which it opens.
    fn config_exists(&self) -> bool {
        self.app
            .try_state::<AppState>()
            .map(|state| state.config_path().is_file())
            .unwrap_or(false)
    }
}

impl Shell for AppShell {
    fn window_visible(&self) -> Result<bool, String> {
        match self.window() {
            Some(window) => window.is_visible().map_err(|err| err.to_string()),
            None => Ok(false),
        }
    }

    fn show_window(&self) -> Result<(), String> {
        let window = self
            .window()
            .ok_or_else(|| "the window is gone".to_string())?;
        // Unminimise first: a window restored from the taskbar is minimised as often as hidden, and
        // `show` alone would leave it in the tray area.
        let _ = window.unminimize();
        window
            .show()
            .and_then(|()| window.set_focus())
            .map_err(|err| err.to_string())
    }

    fn hide_window(&self) -> Result<(), String> {
        self.window()
            .ok_or_else(|| "the window is gone".to_string())?
            .hide()
            .map_err(|err| err.to_string())
    }

    fn status(&self) -> Result<RecordingStatusDto, CommandError> {
        self.state()?.recorder().status()
    }

    fn start_recording(&self) -> Result<RecordingStatusDto, CommandError> {
        // What the window's Start button does: re-read `config.toml`, then start
        // (`RecorderHost::start_from_config`).
        self.state()?.recorder().start_from_config()
    }

    fn stop_recording(&self) -> Result<RecordingStatusDto, CommandError> {
        self.state()?.recorder().stop()
    }

    fn clip_now(&self) -> Result<RecordedClipDto, CommandError> {
        self.state()?.recorder().clip_now()
    }

    fn open_config(&self) -> Result<PathBuf, String> {
        let state = self
            .app
            .try_state::<AppState>()
            .ok_or_else(|| "the application state is not available yet".to_string())?;
        let path = state.config_path();
        let (program, args) = background::reveal_command(&path, background::FileManager::current());
        replayed_media::sidecar_command(&program)
            .args(&args)
            .spawn()
            .map_err(|err| format!("could not run {program}: {err}"))?;
        Ok(path)
    }

    fn quit(&self) -> Result<(), String> {
        // `exit` rather than `close`: every window is destroyed and the process ends; the recorder was
        // already flushed by `background::dispatch`'s Quit arm.
        self.app.exit(0);
        Ok(())
    }
}

/// The three tray icons, decoded once at startup.
///
/// Generated from the application icon (`icons/icon.png`) with the background keyed out and a
/// state badge added; `cargo test` decodes them, so a corrupt file fails a test, not a tray.
struct TrayIcons {
    idle: Image<'static>,
    recording: Image<'static>,
    attention: Image<'static>,
}

impl TrayIcons {
    fn load() -> tauri::Result<Self> {
        Ok(Self {
            idle: Image::from_bytes(include_bytes!("../icons/tray-idle.png"))?,
            recording: Image::from_bytes(include_bytes!("../icons/tray-recording.png"))?,
            attention: Image::from_bytes(include_bytes!("../icons/tray-attention.png"))?,
        })
    }

    fn for_state(&self, state: TrayState) -> Image<'static> {
        match state {
            TrayState::Idle => self.idle.clone(),
            TrayState::Recording => self.recording.clone(),
            TrayState::Attention => self.attention.clone(),
        }
    }
}

/// The tray and the menu items whose labels follow the state.
///
/// `rendering` is the whole visible surface as one comparable value ([`TrayView::rendering`]),
/// so "did anything change?" is an equality check and the OS is touched only on a real change;
/// this runs every half second.
struct TrayWiring {
    tray: tauri::tray::TrayIcon<Wry>,
    window_item: MenuItem<Wry>,
    recording_item: MenuItem<Wry>,
    clip_item: MenuItem<Wry>,
    config_item: MenuItem<Wry>,
    icons: TrayIcons,
    rendering: Mutex<TrayRendering>,
    /// One short line about the last hotkey press, for the tooltip, so a user in a game with the
    /// window hidden learns that the press did nothing.
    last_press: Mutex<Option<String>>,
}

impl TrayWiring {
    fn install(shell: &AppShell, hotkey: &HotkeyStatus, icons: TrayIcons) -> tauri::Result<Self> {
        let view = TrayView {
            status: &RecordingStatusDto::from(replayed_recorder::RecorderStatus::stopped()),
            hotkey,
            window_visible: true,
            config_exists: shell.config_exists(),
            last_press: None,
        };

        let mut builder = MenuBuilder::new(&shell.app);
        let mut items: Vec<MenuItem<Wry>> = Vec::new();
        for action in MenuAction::ALL {
            if action.separator_before() {
                builder = builder.separator();
            }
            let item = MenuItemBuilder::with_id(action.id(), action.label(&view))
                .enabled(action.enabled(&view))
                .build(&shell.app)?;
            builder = builder.item(&item);
            items.push(item);
        }
        let menu = builder.build()?;

        let tray = TrayIconBuilder::with_id("replayed")
            .icon(icons.for_state(view.tray_state()))
            .tooltip(view.tooltip())
            .menu(&menu)
            // Left click opens the menu (the Windows convention), where "Show the window" lives. There is
            // no separate click handler to get out of step with it.
            .show_menu_on_left_click(true)
            .on_menu_event(move |app, event| {
                let shell = AppShell { app: app.clone() };
                let id = event.id().as_ref().to_string();
                let Some(action) = MenuAction::from_id(&id) else {
                    tracing::warn!("the tray sent an id this build did not create: {id}");
                    return;
                };
                let outcome = background::dispatch(action, &shell);
                tracing::info!("tray: {id} → {}", outcome.describe());
                // The wiring's own refresh, from the callback: the poll thread would catch up within half a
                // second, but a menu already stale when it reopens lies twice.
                if let Some(wiring) = app.try_state::<Arc<TrayWiring>>() {
                    if let Some(state) = app.try_state::<AppState>() {
                        wiring.refresh(&shell, &state.hotkey());
                    }
                }
            })
            .build(&shell.app)?;

        let find = |action: MenuAction| {
            items
                .iter()
                .find(|item| item.id().as_ref() == action.id())
                .expect("every action was built above")
                .clone()
        };

        Ok(Self {
            tray,
            window_item: find(MenuAction::ShowHideWindow),
            recording_item: find(MenuAction::ToggleRecording),
            clip_item: find(MenuAction::SaveClip),
            config_item: find(MenuAction::OpenConfig),
            icons,
            rendering: Mutex::new(view.rendering()),
            last_press: Mutex::new(None),
        })
    }

    fn set_last_press(&self, line: String) {
        *self
            .last_press
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(line);
    }

    /// Re-derive the tray from the shell and apply what changed.
    fn refresh(&self, shell: &AppShell, hotkey: &HotkeyStatus) {
        let status = match shell.status() {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(
                    "the tray could not read the recorder's status: {}",
                    err.message
                );
                return;
            }
        };
        let last_press = self
            .last_press
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let view = TrayView {
            status: &status,
            hotkey,
            window_visible: shell.window_visible().unwrap_or(true),
            config_exists: shell.config_exists(),
            last_press: last_press.as_deref(),
        };
        let wanted = view.rendering();

        let mut current = self.rendering.lock().unwrap_or_else(|err| err.into_inner());
        if *current == wanted {
            return;
        }
        if current.state != wanted.state {
            if let Err(err) = self.tray.set_icon(Some(self.icons.for_state(wanted.state))) {
                tracing::warn!("the tray icon could not be changed: {err}");
            }
        }
        if current.tooltip != wanted.tooltip {
            if let Err(err) = self.tray.set_tooltip(Some(wanted.tooltip.clone())) {
                tracing::warn!("the tray tooltip could not be changed: {err}");
            }
        }
        for (item, label) in [
            (&self.window_item, &wanted.window_label),
            (&self.recording_item, &wanted.recording_label),
            (&self.clip_item, &wanted.clip_label),
            (&self.config_item, &wanted.config_label),
        ] {
            if let Err(err) = item.set_text(label) {
                tracing::warn!("a tray menu label could not be changed: {err}");
            }
        }
        if let Err(err) = self.clip_item.set_enabled(wanted.clip_enabled) {
            tracing::warn!("the tray's clip item could not be enabled or disabled: {err}");
        }
        *current = wanted;
    }
}

/// Set up the log file and the tracing subscriber, and return the file this process writes.
///
/// The file is the one the health page reads, so it is returned rather than recomputed: a
/// process that could not take the log's lock writes to a file of its own, and the health page
/// must show the file that holds *this* process's problems.
fn init_tracing() -> PathBuf {
    // **A windowed application has no stderr.** `fmt()` writes to stderr by default and Windows
    // gives a GUI process nowhere for it to go, so every `warn!` and `error!` would be discarded.
    // The file is what makes the app diagnosable over SSH instead of by screenshot.
    //
    // It sits beside the data (the directory of clips and the index) so one path answers "where are
    // the logs" on every platform. `RUST_LOG` still overrides the level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,replayed_desktop=debug".into());

    let log_dir = crate::config::app_data_dir().join("logs");
    // Only when that path is real: `app_data_dir` falls back to `.` when neither LOCALAPPDATA nor
    // XDG_DATA_HOME is set, and a test must not write ./logs into the checkout.
    let log_file = log_dir
        .is_absolute()
        .then(|| std::fs::create_dir_all(&log_dir).ok())
        .flatten()
        .and_then(|()| {
            // 5 MB x (1 live + 3 rotated): a background app must not grow its log forever.
            const MAX_BYTES: u64 = 5 * 1024 * 1024;
            const KEEP: usize = 3;
            let shared = log_dir.join("replayed.log");
            // One process writes the shared file: the lock keeps a second launch out of it, so the
            // two cannot interleave their lines and the rotation cannot rename a file the other
            // process holds open (which fails on Windows). The loser writes a file of its own.
            match log_file::RotatingFile::open_locked(shared.clone(), MAX_BYTES, KEEP) {
                Ok((file, true)) => Some((file, shared)),
                Ok((_file, false)) => {
                    let own = log_dir.join(format!("replayed-{}.log", std::process::id()));
                    tracing::warn!(
                        "another process is writing {}; this one writes {}",
                        shared.display(),
                        own.display()
                    );
                    log_file::RotatingFile::open(own.clone(), MAX_BYTES, KEEP)
                        .ok()
                        .map(|file| (file, own))
                }
                Err(err) => {
                    tracing::warn!("the log file could not be opened: {err}");
                    None
                }
            }
        });

    // A panic is silent in a windowed process too, and explains a window that stops answering.
    // Record it, then let the previous hook run.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        previous(info);
    }));

    // `try_init` rather than `init`: a second initialisation (a test harness, an embedding process)
    // must not panic the application.
    match log_file {
        Some((file, path)) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
            path
        }
        None => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
            // The file the health page names even though nothing could be written to it: the
            // path is where the log *would* be, which is the answer a user needs.
            log_dir.join("replayed.log")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> StorageConfig {
        StorageConfig::example().unwrap()
    }

    /// The log file a test's shell claims: the name the health page would show. Nothing writes
    /// to it, and the path does not have to exist.
    fn log_path() -> PathBuf {
        PathBuf::from("replayed.log")
    }

    /// The directory holding this crate's manifest (`apps/desktop/src-tauri`), from which
    /// `tauri.conf.json` and the platform configs resolve their relative paths.
    fn tauri_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn config(file: &str) -> serde_json::Value {
        let path = tauri_dir().join(file);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {file}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {file}: {e}"))
    }

    /// The tray icons, decoded. They are committed binary assets, and a corrupt one is a tray with
    /// no icon. Same decode as the tray's startup, no runtime involved.
    #[test]
    fn the_tray_icons_are_decodable_pngs_of_one_size() {
        let icons = TrayIcons::load().expect("the embedded tray icons must decode");

        for (state, image) in [
            ("idle", &icons.idle),
            ("recording", &icons.recording),
            ("attention", &icons.attention),
        ] {
            assert_eq!(
                (image.width(), image.height()),
                (32, 32),
                "the {state} icon is 32x32"
            );
            assert_eq!(
                image.rgba().len(),
                32 * 32 * 4,
                "the {state} icon is RGBA, four bytes to a pixel"
            );
            let transparent = image.rgba().chunks_exact(4).filter(|px| px[3] == 0).count();
            assert!(
                transparent > 500,
                "the {state} icon must keep the app icon's keyed-out background, not sit on \
                 the taskbar as an opaque block ({transparent} transparent pixels)"
            );
        }

        // The state badge is the point: three identical icons would make "recording" a claim the
        // taskbar cannot back up.
        assert_ne!(
            icons.idle.rgba(),
            icons.recording.rgba(),
            "idle and recording differ"
        );
        assert_ne!(
            icons.recording.rgba(),
            icons.attention.rgba(),
            "recording and attention differ"
        );
    }

    /// The config story from the state a command would answer from: the path the window shows and
    /// the tray's "open config" item reveals is the one this process would read.
    #[test]
    fn the_state_names_the_config_file_this_process_reads() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let state = AppState::open(&app_dir, storage(), log_path()).unwrap();

        assert_eq!(state.config_path(), app_dir.join("config.toml"));
        assert!(
            !state.config_path().is_file(),
            "nothing has written it in this test"
        );
        assert!(state
            .asset_roots()
            .contains(&state.paths.lock().unwrap().clips_dir));
    }

    /// The half of the packaging contract that `crates/media` cannot check by itself.
    ///
    /// An installed app finds ffmpeg in `<resource dir>/binaries/`, and the bundler writes each
    /// `bundle.resources` entry to `<resource dir>/<destination>`. If the two disagree the installer
    /// ships sidecars to a path nothing reads and the app falls back to `PATH`, failing on a user's
    /// machine. The directory name is read from the media crate, so renaming it there fails here.
    #[test]
    fn the_bundle_maps_the_sidecars_where_discovery_reads_them() {
        let config = config("tauri.conf.json");

        assert_eq!(
            config["bundle"]["active"],
            serde_json::Value::Bool(true),
            "bundling must be on, or there is no distributable artifact at all"
        );

        let resources = config["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources is a path-to-destination map");
        // The sidecar directory is the rule that matters here; the bundle may map other files
        // beside it (the licence and its notices), and what must not happen is the sidecars
        // landing somewhere discovery does not read.
        let (source, destination) = resources
            .iter()
            .find(|(_, destination)| {
                destination.as_str().is_some_and(|destination| {
                    destination.trim_end_matches('/') == replayed_media::binaries::SIDECAR_DIR_NAME
                })
            })
            .expect("the sidecar directory is mapped");
        let destination = destination.as_str().expect("the destination is a string");

        assert_eq!(
            destination.trim_end_matches('/'),
            replayed_media::binaries::SIDECAR_DIR_NAME,
            "the bundler must write the sidecars into the directory `FfmpegBinaries::discover` \
             reads: `binaries/` under the resource directory"
        );

        // The source is the directory `cargo xtask sidecars fetch` writes, which
        // `FfmpegBinaries::discover`'s development fallback also reads from a checkout.
        assert_eq!(source, "../../../binaries/");
        assert!(
            tauri_dir().join(source).is_dir(),
            "{} must exist for the bundle to be buildable at all: `tauri-build` refuses to \
             compile when a mapped resource path is missing, which is why the directory \
             carries a committed `.gitkeep`",
            tauri_dir().join(source).display()
        );
    }

    /// The metadata the installers are built from, and the icon files they name. A bundle silently
    /// gets both wrong: a non-existent icon path fails bundling, and Tauri rejects the placeholder
    /// identifier (`com.tauri.dev`).
    #[test]
    fn the_bundle_metadata_and_icons_are_present_and_plausible() {
        let config = config("tauri.conf.json");
        let bundle = &config["bundle"];

        // These two differ on purpose: Tauri derives the per-user install directory from `productName`
        // and the installed binary's name from `mainBinaryName`, so the mismatch keeps the installation
        // out of %LOCALAPPDATA%\replayed, the directory holding the user's clips and index. Changing
        // either alone moves something the other should not.
        assert_eq!(
            config["productName"], "replayed",
            "the install directory's name, and the bundle's"
        );
        assert_eq!(
            config["mainBinaryName"], "replayed",
            "the installed exe is replayed, as the crates and the CLI are"
        );
        assert_eq!(
            config["version"], "0.2.0",
            "in step with the workspace version"
        );

        let identifier = config["identifier"].as_str().expect("identifier");
        assert_ne!(
            identifier, "com.tauri.dev",
            "Tauri refuses to bundle the placeholder id"
        );
        assert!(
            identifier.matches('.').count() >= 2
                && identifier
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "identifier must be a reverse-DNS id: {identifier}"
        );

        for key in [
            "publisher",
            "copyright",
            "category",
            "homepage",
            "shortDescription",
        ] {
            let value = bundle[key]
                .as_str()
                .unwrap_or_else(|| panic!("bundle.{key} is set"));
            assert!(!value.trim().is_empty(), "bundle.{key} is empty");
        }
        assert_eq!(
            bundle["publisher"], "FisiFla",
            "the copyright holder in LICENSE-MIT"
        );

        let icons = bundle["icon"].as_array().expect("bundle.icon is a list");
        assert!(!icons.is_empty(), "a bundle needs at least one icon");
        for icon in icons {
            let path = tauri_dir().join(icon.as_str().expect("icon paths are strings"));
            assert!(
                path.is_file(),
                "bundle.icon names {}, which does not exist — the bundling step would fail",
                path.display()
            );
        }
    }

    /// The installer targets, the only reason the platform configs exist: a build must not try to
    /// produce another platform's artifact.
    #[test]
    fn every_platform_config_narrows_the_bundle_targets() {
        for (file, expected) in [
            ("tauri.windows.conf.json", vec!["nsis"]),
            ("tauri.macos.conf.json", vec!["app"]),
        ] {
            let config = config(file);
            let targets: Vec<&str> = config["bundle"]["targets"]
                .as_array()
                .unwrap_or_else(|| panic!("{file} sets an explicit list of bundle targets"))
                .iter()
                .map(|t| t.as_str().expect("targets are strings"))
                .collect();
            assert_eq!(targets, expected, "{file}");
        }
    }

    #[test]
    fn opening_the_shell_creates_the_index_and_the_clips_directory() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");

        let state = AppState::open(&app_dir, storage(), log_path()).unwrap();

        assert!(
            app_dir.join("replayed.db").is_file(),
            "the index is created"
        );
        assert!(
            app_dir.join("clips").is_dir(),
            "and so is the clips directory"
        );
        assert!(state.bins.is_some(), "ffmpeg is on PATH on this host");
        assert!(state.warnings.is_empty());

        // The button's path: the folder the shell was opened with, not a guess at `%LOCALAPPDATA%`
        // or at the clips directory's parent (a configured `clips_dir` can be anywhere).
        assert_eq!(state.data_dir(), app_dir);

        // The wrappers' shared entry point works against the real state, with no runtime.
        let stats = state.with_deps(commands::storage_stats).unwrap();
        assert_eq!(stats.clip_count, 0);
        assert_eq!(stats.cap_bytes, 53_687_091_200, "the example's 50 GiB cap");
        assert!(state.with_deps(commands::list_clips).unwrap().is_empty());
    }

    #[test]
    fn reopening_the_shell_reuses_the_index_instead_of_recreating_it() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");

        let first = AppState::open(&app_dir, storage(), log_path()).unwrap();
        // The listing only names files that are there, so the row needs its file: an empty stand-in,
        // never opened, like the commands tests' `add_row`.
        std::fs::write(app_dir.join("clips").join("kept.mp4"), b"").unwrap();
        let id = {
            let store = first.store.lock().unwrap();
            store
                .insert_clip(&replayed_store::NewClip {
                    path: app_dir.join("clips").join("kept.mp4"),
                    started_at_ms: 0,
                    duration_ms: 1_000,
                    size_bytes: 1,
                    codec: "h264".to_string(),
                })
                .unwrap()
        };
        drop(first);

        let second = AppState::open(&app_dir, storage(), log_path()).unwrap();
        let clips = second.with_deps(commands::list_clips).unwrap();
        assert_eq!(clips.len(), 1, "the second open reads what the first wrote");
        assert_eq!(clips[0].id, id);
    }

    #[test]
    fn the_asset_roots_are_the_clips_directory_and_the_thumbnail_cache() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let state = AppState::open(&app_dir, storage(), log_path()).unwrap();

        assert_eq!(
            state.asset_roots(),
            vec![app_dir.join("clips"), app_dir.join("thumbnails")]
        );
    }

    #[test]
    fn a_configured_clips_directory_is_used_and_added_to_the_asset_scope() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let elsewhere = dir.path().join("on-another-drive").join("clips");
        let configured = StorageConfig {
            clips_dir: elsewhere.to_string_lossy().into_owned(),
            ..storage()
        };

        let state = AppState::open(&app_dir, configured, log_path()).unwrap();

        assert!(elsewhere.is_dir(), "the configured directory is created");
        assert_eq!(
            state.asset_roots()[0],
            elsewhere,
            "and it is what playback is scoped to"
        );
        assert_eq!(
            state.paths.lock().unwrap().db_path,
            app_dir.join("replayed.db"),
            "the index stays put"
        );
    }

    #[test]
    fn a_capture_only_config_file_does_not_stop_the_shell_from_opening() {
        // The shell reads `[storage]` and nothing else (see `config.rs`), so a file with a capture
        // setting the CLI would reject still opens the review UI.
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            "[encode]\nvendor = \"software\"\n\n[storage]\nclips_dir = \"\"\n\
             max_total_bytes = 2048\nmax_age_days = 1\n",
        )
        .unwrap();

        let storage = StorageConfig::load(&app_dir.join("config.toml")).unwrap();
        let state = AppState::open(&app_dir, storage, log_path()).unwrap();

        assert_eq!(
            state.with_deps(commands::storage_stats).unwrap().cap_bytes,
            2048
        );
    }

    /// A config file the shell cannot read must not take the shell down with it.
    ///
    /// A release build has no console, so the old `eprintln!` went nowhere: the app exited with
    /// no window, no tray and no message, and the only trace was a line in a log file. The shell
    /// starts with the example's values instead and says what happened in the storage panel,
    /// where a user can see it and repair the file.
    #[test]
    fn a_config_missing_a_storage_key_still_opens_the_shell_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        std::fs::create_dir_all(&app_dir).unwrap();
        // `max_age_days` is missing: a key the desktop's reader falls back on, a section the CLI
        // would refuse.
        std::fs::write(
            app_dir.join("config.toml"),
            "[storage]\nclips_dir = \"\"\nmax_total_bytes = 2048\n",
        )
        .unwrap();

        let state = open_state(&app_dir, &app_dir.join("config.toml"), log_path())
            .expect("the shell opens whatever the config file says");

        assert!(
            state.warnings.iter().any(|w| w.contains("max_age_days")),
            "the key that fell back is named: {:?}",
            state.warnings
        );
        assert!(
            state.with_deps(commands::storage_stats).unwrap().cap_bytes > 0,
            "and the storage policy is the example's, not a zero cap that would delete everything"
        );
    }

    /// A config file that cannot be read at all — not even as TOML — is the same story.
    #[test]
    fn a_config_that_is_not_toml_still_opens_the_shell_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "[storage\nclips_dir = ").unwrap();

        let state = open_state(&app_dir, &app_dir.join("config.toml"), log_path())
            .expect("the shell opens whatever the config file says");

        assert!(
            state.warnings.iter().any(|w| w.contains("config file")),
            "the reason is a warning the window shows: {:?}",
            state.warnings
        );
    }

    /// The hotkey is installed after the single-instance check, so the state is built before it
    /// exists.
    ///
    /// Tauri runs the plugins' setup before the application's own, and the single-instance
    /// plugin's check is one of them: a second launch is gone before it can reach
    /// `RegisterHotKey`. A state that carried a hotkey would have had to register the chord
    /// before that check, which is how the chord ended up on the process that was leaving.
    #[test]
    fn the_state_is_built_before_the_hotkey_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let state = AppState::open(&app_dir, storage(), log_path()).unwrap();

        assert!(
            !state.hotkey().installed,
            "nothing is listening yet: the chord is registered in setup, after the \
             single-instance check"
        );

        state.set_hotkey(HotkeyStatus::installed("Ctrl+F9"));
        assert_eq!(state.hotkey().chord, "Ctrl+F9");
        assert!(state.hotkey().installed);
    }

    /// The chord the config names is the one that is installed, and a chord that cannot be read
    /// installs nothing at all.
    #[test]
    fn the_hotkey_comes_from_the_config_and_a_typo_installs_nothing() {
        let cfg = BackgroundConfig::from_toml("[hotkeys]\nclip = \"Alt+F9\"\n").unwrap();
        let (status, _presses) = install_clip_hotkey(&cfg);
        assert_eq!(status.chord, "Alt+F9", "the chord the user wrote is used");

        let cfg = BackgroundConfig::from_toml("[hotkeys]\nclip = \"Ctrl+Shift\"\n").unwrap();
        let (status, presses) = install_clip_hotkey(&cfg);
        assert!(
            !status.installed,
            "a chord that does not parse installs nothing"
        );
        assert!(presses.is_none());
    }

    /// The webview may not widen the asset protocol's scope to a path it names.
    ///
    /// The asset protocol is the window's only read channel (`capabilities/default.json` grants
    /// no filesystem permission), so a clips directory the webview could name for itself is a
    /// directory the webview could read. Only the default, the application data directory, and
    /// the directories the user picked in the native folder picker are allowed; a path the
    /// webview names is asked for in that picker instead (`update_settings`).
    #[test]
    fn only_an_approved_clips_directory_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let state = AppState::open(&app_dir, storage(), log_path()).unwrap();
        state.approve_clips_root(&app_dir);

        assert!(state.clips_dir_allowed(""), "the default is always allowed");
        assert!(
            state.clips_dir_allowed(&app_dir.join("clips").to_string_lossy()),
            "and so is the application data directory"
        );
        assert!(
            !state.clips_dir_allowed(r"C:\Users\victim\Documents"),
            "a path the webview named is not"
        );

        let picked = dir.path().join("picked");
        std::fs::create_dir_all(&picked).unwrap();
        state.approve_clips_root(&picked);
        assert!(
            state.clips_dir_allowed(&picked.to_string_lossy()),
            "a directory the user picked in the native picker is"
        );
        assert!(
            !state.clips_dir_allowed(&picked.join("..").join("elsewhere").to_string_lossy()),
            "and not a path that climbs out of it"
        );
    }

    #[test]
    fn a_frontend_log_line_cannot_smuggle_a_second_line() {
        let forged =
            flatten_for_log("clip failed\n2026-10-19 12:00:00 ERROR backend: the disk is full");
        assert!(
            !forged.contains('\n') && !forged.contains('\r'),
            "a newline in the webview's text must not end the log line: {forged:?}"
        );
        assert!(
            forged.contains("the disk is full"),
            "but the text itself is kept: {forged:?}"
        );
        assert_eq!(
            forged.matches("  ").count(),
            0,
            "and runs of space collapse"
        );
    }

    #[test]
    fn a_frontend_log_line_is_capped() {
        let long = "x".repeat(FRONTEND_LOG_MAX_CHARS * 2);
        let capped = flatten_for_log(&long);
        assert!(
            capped.chars().count() <= FRONTEND_LOG_MAX_CHARS + 1,
            "the cap plus the ellipsis: {} characters",
            capped.chars().count()
        );
    }
}
