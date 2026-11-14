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
mod window_focus;

use background::{
    AppStatus, CloseAction, HotkeyStatus, MenuAction, Shell, TrayRendering, TrayState, TrayView,
};
use commands::{
    AppPaths, ClipDto, CommandError, DeleteOutcome, DeleteSessionOutcome, Deps, DiagnosticsReport,
    EncoderSettings, EncoderStatus, ErrorCode, ExportOutcome, PruneOutcome, RecordedClipDto,
    RecorderHost, RecordingStatusDto, SessionDto, SessionEventDto, StorageConfigView, StorageStats,
    ThumbnailRef, UpdatePreparationDto,
};
use config::{BackgroundConfig, RecordingConfig, StorageConfig};
use replayed_media::FfmpegBinaries;
use replayed_recorder::GsiBinding;
use replayed_store::Store;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent, Wry};

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
    /// Clips directories taken out of the asset protocol's scope during this run.
    ///
    /// The scope can only be narrowed for good: a forbidden directory stays forbidden until the
    /// process ends, and the forbid wins over any later allow. Moving the clips directory away
    /// from one and then back to it would therefore leave a library whose clips do not play and no
    /// message saying why. The list is what lets `update_settings` refuse that move up front, with
    /// the reason, before anything is written.
    withdrawn_roots: Mutex<Vec<PathBuf>>,
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

        // An index brought across from the old `localplay` directory still names its clips there.
        // Repaired before anything reads it, so the library lists them as present, and never at the
        // cost of the start: a failure is a warning, and the rows stay as they were.
        if let Some(base) = app_data_dir.parent() {
            match config::repair_legacy_index_paths(&store, base, app_data_dir, &paths.db_path) {
                Ok(0) => {}
                Ok(changed) => tracing::info!(
                    "{changed} path(s) in the clip index were repaired after the data directory moved"
                ),
                Err(err) => tracing::warn!("the clip index paths were not repaired: {err}"),
            }
        }

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
        let clips_read = store.list_clips();
        // The thumbnail cache is swept against the index once per start: retention and the CLI
        // delete clips without telling it, so its files otherwise outlive their clips for ever.
        // Only against an index that was actually read: a failed read must not look like "no clip
        // owns anything".
        if let Ok(listed) = &clips_read {
            let swept = commands::sweep_stale_thumbnails(&paths.thumbnails_dir, listed);
            if swept > 0 {
                tracing::info!("removed {swept} cached thumbnails whose clips are gone");
            }
        }
        let clips = clips_read.unwrap_or_default();
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
            withdrawn_roots: Mutex::new(Vec::new()),
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

    /// Record that `dir` was taken out of the asset protocol's scope, for good.
    fn withdraw_clips_root(&self, dir: &Path) {
        let mut withdrawn = self
            .withdrawn_roots
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !withdrawn.iter().any(|old| old == dir) {
            withdrawn.push(dir.to_path_buf());
        }
    }

    /// Refuse a clips directory that overlaps one withdrawn earlier in this run.
    ///
    /// Overlap in both directions: a directory inside a withdrawn one is forbidden with it (the
    /// forbid is recursive), and a directory that contains a withdrawn one would be only partly
    /// readable. The remedy is a restart, which starts with an empty scope, and the message says
    /// so rather than leaving the user with clips that will not play.
    ///
    /// `dir` is the value the user typed or picked; empty means the default directory, which is
    /// never withdrawn (the static scope in `tauri.conf.json` covers it and it is never forbidden).
    pub fn check_clips_dir_not_withdrawn(&self, dir: &str) -> Result<(), CommandError> {
        if dir.is_empty() {
            return Ok(());
        }
        let wanted = AppPaths::resolve(&self.app_data_dir, dir).clips_dir;
        let withdrawn = self
            .withdrawn_roots
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match withdrawn
            .iter()
            .find(|old| wanted.starts_with(old) || old.starts_with(&wanted))
        {
            None => Ok(()),
            Some(old) => Err(CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "{} cannot be the clips directory again until replayed is restarted: \
                     {} was taken away from the window's file access when the directory was \
                     moved earlier in this session, and that cannot be undone while it runs. \
                     Nothing was changed.",
                    wanted.display(),
                    old.display()
                ),
            )),
        }
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
    /// through. Lock order is store, then paths, then storage, the last two only for as long as it
    /// takes to copy them; the settings command takes those two in that order and never the store,
    /// so nothing can deadlock.
    ///
    /// **For work that is quick.** The store's lock is held for the whole of `run`, so a command
    /// that runs ffmpeg in here keeps every other command that reads the index waiting for as long as
    /// the encode takes (a minute, for a long clip), and the window's list, the storage panel and the
    /// drag lookup all stop answering. Anything that runs a child process or walks a directory of
    /// footage goes through [`Self::with_deps_detached`] instead.
    fn with_deps<T>(
        &self,
        run: impl FnOnce(&Deps<'_>) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        let store = self
            .store
            .lock()
            .map_err(|_| Self::poisoned("the store lock"))?;
        // Copied out, not held: the store's lock is the one this call needs for its whole run, and
        // keeping the paths' and the settings' as well made a quick command that waited on the index
        // also keep `update_settings` and every detached command waiting.
        let paths = self
            .paths
            .lock()
            .map_err(|_| Self::poisoned("the paths lock"))?
            .clone();
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

    /// [`Self::with_deps`] for work that runs for a while, with **no lock held while it runs**.
    ///
    /// The paths and the storage settings are copied out under their locks and the locks are let go;
    /// the index is opened on a connection of its own for the call. The CLI and the recorder already
    /// share the database file this way (the busy timeout is what lets two writers take turns), and
    /// a connection is a file open, not a migration, so this costs nothing next to the ffmpeg run it
    /// exists for.
    ///
    /// What is given up is the serialisation `with_deps` gave for free: two such commands can now
    /// overlap, and one can delete a clip another is reading. That is the same race the recorder's
    /// own clip writes and the CLI have always had with the window, and it ends in an ffmpeg error
    /// that names the missing file, not in a corrupt index: every write is one SQL statement.
    fn with_deps_detached<T>(
        &self,
        run: impl FnOnce(&Deps<'_>) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        let paths = self
            .paths
            .lock()
            .map_err(|_| Self::poisoned("the paths lock"))?
            .clone();
        let storage = StorageConfigView::from(
            &*self
                .storage
                .lock()
                .map_err(|_| Self::poisoned("the storage lock"))?,
        );
        let store = Store::open(&paths.db_path).map_err(|e| {
            CommandError::new(
                ErrorCode::Store,
                format!(
                    "could not open the clip index {}: {e:#}",
                    paths.db_path.display()
                ),
            )
        })?;
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

/// Run a command's work on the blocking pool, off the UI thread and off the async workers.
///
/// A synchronous `#[tauri::command]` runs on the thread that owns the window, and an `async`
/// command whose body blocks holds one of the runtime's few workers for as long as it blocks. The
/// commands that run ffmpeg, wait for the encoder or walk a directory of footage take seconds to
/// minutes, so they run here: the pool grows to what the work needs and the window keeps painting.
///
/// A panic in `work` comes back as an error instead of taking the command's task down silently.
async fn blocking<T, F>(app: &AppHandle, work: F) -> Result<T, CommandError>
where
    T: Send + 'static,
    F: FnOnce(&AppState) -> Result<T, CommandError> + Send + 'static,
{
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        work(&state)
    })
    .await
    .map_err(|err| CommandError::new(ErrorCode::Io, format!("the command did not finish: {err}")))?
}

/// Run `work` on a thread of its own, so a callback the event loop is waiting on can return.
///
/// **The event loop's callbacks (a tray menu click, a window event) run on the thread that
/// paints every window.** Anything slow in them freezes the window: a start runs the encoder
/// smoke test and a stop concatenates the session, seconds each. And anything that waits for
/// that thread from another thread, while holding a lock the callback also wants, is a deadlock:
/// the tray refresh does exactly that (its icon and menu updates are round trips to the UI
/// thread, made while the refresh holds the lock the callback would take), so a refresh from a
/// callback was a hang waiting for the two to meet. Both end when the callbacks hand their work
/// to this and return.
fn off_event_loop(name: &str, work: impl FnOnce() + Send + 'static) {
    if let Err(err) = std::thread::Builder::new().name(name.into()).spawn(work) {
        tracing::error!("the {name} thread could not be started: {err}");
    }
}

/// Tray menu actions, one at a time and in the order they were clicked.
///
/// They used to run one after another on the event loop; moved to threads they would overlap,
/// and a double-clicked "Start recording" would race itself. The lock restores the order.
static TRAY_ACTIONS: Mutex<()> = Mutex::new(());

/// Thumbnails, one ffmpeg run at a time.
///
/// They used to queue behind the index's lock without anyone meaning them to. Now that the lock
/// is not held across ffmpeg, a window that asks for forty visible clips' thumbnails would start
/// forty ffmpeg processes at once on a machine that is also recording; this keeps it at the one
/// the lock allowed.
static THUMBNAIL_GATE: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------------------
// The IPC commands. Every one of these is a delegation: resolve the state, call the plain
// function in `commands.rs`, hand back its result. Nothing is decided here.
// ---------------------------------------------------------------------------------------

/// Every clip in the index, newest first.
#[tauri::command(async, rename_all = "snake_case")]
fn list_clips(state: State<'_, AppState>) -> Result<Vec<ClipDto>, CommandError> {
    state.with_deps(commands::list_clips)
}

/// Usage, the configured cap and age limit, and the cleanup policy's verdict.
#[tauri::command(async, rename_all = "snake_case")]
fn storage_stats(state: State<'_, AppState>) -> Result<StorageStats, CommandError> {
    state.with_deps(commands::storage_stats)
}

/// Remove the index rows whose files are gone. The user confirms this in the Storage panel: it is
/// the one place the shell rewrites the index from the filesystem's evidence.
#[tauri::command(async, rename_all = "snake_case")]
fn prune_missing(state: State<'_, AppState>) -> Result<PruneOutcome, CommandError> {
    state.with_deps(commands::prune_missing)
}

/// Combine whole clips into one file, losslessly.
///
/// Runs ffmpeg over every clip, so it is off the UI thread and holds no lock while it runs.
#[tauri::command(rename_all = "snake_case")]
async fn combine_clips(app: AppHandle, ids: Vec<i64>) -> Result<ClipDto, CommandError> {
    blocking(&app, move |state| {
        state.with_deps_detached(|deps| commands::combine_clips(deps, &ids))
    })
    .await
}

/// Re-encode a clip with one of the export presets, into a new file beside it.
///
/// A full re-encode: minutes for a long clip. Off the UI thread, and no lock held.
#[tauri::command(rename_all = "snake_case")]
async fn export_clip(
    app: AppHandle,
    id: i64,
    preset: String,
) -> Result<ExportOutcome, CommandError> {
    blocking(&app, move |state| {
        state.with_deps_detached(|deps| commands::export_clip(deps, id, &preset))
    })
    .await
}

/// Mark a clip as a favourite, or clear it.
#[tauri::command(async, rename_all = "snake_case")]
fn set_favourite(
    state: State<'_, AppState>,
    id: i64,
    favourite: bool,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_favourite(deps, id, favourite))
}

/// Rename a clip (its display title; the file keeps its name). Blank clears the title.
#[tauri::command(async, rename_all = "snake_case")]
fn set_clip_title(
    state: State<'_, AppState>,
    id: i64,
    title: String,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_clip_title(deps, id, &title))
}

/// Replace a clip's tags.
#[tauri::command(async, rename_all = "snake_case")]
fn set_clip_tags(
    state: State<'_, AppState>,
    id: i64,
    tags: Vec<String>,
) -> Result<ClipDto, CommandError> {
    state.with_deps(|deps| commands::set_clip_tags(deps, id, &tags))
}

/// Put the clip's file on the system clipboard, as a file.
///
/// On the blocking pool because it waits for PowerShell: the call blocks until the child process
/// is done, and neither the window nor the runtime's workers may stop while it does.
#[tauri::command(rename_all = "snake_case")]
async fn copy_clip_to_clipboard(app: AppHandle, id: i64) -> Result<(), CommandError> {
    blocking(&app, move |state| {
        let path = state.with_deps(|deps| commands::clip_file(deps, id))?;
        clip_files::copy_file_to_clipboard(&path)
            .map_err(|msg| CommandError::new(commands::ErrorCode::Io, msg))
    })
    .await
}

/// Show `path` in the system file manager: the file selected where the platform allows, else its
/// folder.
///
/// Spawns [`background::reveal_process`] and not a command built from [`background::reveal_command`]:
/// on Windows Explorer parses its own command line and wants `/select,"C:\a b\c.mp4"` with plain
/// quotes, which `Command::arg` would have escaped into something Explorer does not recognise as a
/// selection (it then opened the default folder, or nothing, for any path with a space in it).
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    let mut command = background::reveal_process(path, background::FileManager::current());
    command.spawn().map(drop).map_err(|err| {
        format!(
            "could not run {}: {err}",
            command.get_program().to_string_lossy()
        )
    })
}

/// Show the clip's file in the system file manager, selected where the platform allows.
#[tauri::command(async, rename_all = "snake_case")]
fn reveal_clip(state: State<'_, AppState>, id: i64) -> Result<(), CommandError> {
    let path = state.with_deps(|deps| commands::clip_file(deps, id))?;
    reveal_in_file_manager(&path).map_err(|msg| CommandError::new(commands::ErrorCode::Io, msg))
}

/// Show the application's data directory in the system file manager.
///
/// The folder that holds the clips, the sessions, the thumbnails, the index, the log and
/// `config.toml`: one button for "where is everything", instead of three paths printed on pages
/// for the user to copy.
#[tauri::command(async, rename_all = "snake_case")]
fn open_data_dir(state: State<'_, AppState>) -> Result<(), CommandError> {
    let dir = state.data_dir().to_path_buf();
    reveal_in_file_manager(&dir).map_err(|msg| CommandError::new(commands::ErrorCode::Io, msg))
}

/// How long a screenshot waits for the display to present a frame.
///
/// Long enough for a 30 Hz display and a compositor that is busy with a game; short enough that a
/// minimised window or a disconnected display reports rather than hangs the window that asked.
const SCREENSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Take one frame from the configured capture target and save it as a PNG in the library.
///
/// A separate path from the recorder on purpose: a screenshot must work while nothing is recording
/// (that is most of the time it is useful), and it needs one frame rather than a session. The
/// backend is opened for this call and stopped at the end of it — [`replayed_capture::grab_frame`]
/// is what guarantees the second half even when the first fails.
fn take_screenshot(state: &AppState) -> Result<ClipDto, CommandError> {
    let settings = commands::read_settings(&state.app_data_dir, &state.config_path())?;
    let target = replayed_capture::target::CaptureTarget::from_config(
        settings.capture_monitor,
        &settings.capture_window,
    );
    // Off Windows the synthetic source's size is what a frame is; on Windows WGC reports the
    // monitor and this is ignored.
    let stub = replayed_capture::stub::StubConfig {
        width: 1920,
        height: 1080,
        fps: settings.fps,
    };
    let mut backend = replayed_capture::platform::default_video_backend(stub, &target)
        .map_err(|err| CommandError::new(commands::ErrorCode::Io, format!("{err:#}")))?;
    // A screenshot needs the pixels on the CPU; the zero-copy path exists for the encoder, and a
    // texture here would be a frame with nothing to write.
    backend.deliver_textures(false);
    let frame = replayed_capture::grab_frame(backend.as_mut(), SCREENSHOT_TIMEOUT)
        .map_err(|err| CommandError::new(commands::ErrorCode::Io, format!("{err:#}")))?;
    state.with_deps_detached(|deps| commands::save_screenshot(deps, &frame))
}

/// Take a screenshot of the configured capture target.
///
/// On the blocking pool because it waits for a frame: at 60 Hz that is a few milliseconds, but a
/// target that presents nothing waits the whole [`SCREENSHOT_TIMEOUT`], and the window must keep
/// answering.
#[tauri::command(rename_all = "snake_case")]
async fn screenshot_now(app: AppHandle) -> Result<ClipDto, CommandError> {
    blocking(&app, take_screenshot).await
}

/// Begin a native drag of the clip's file out of the window (to Explorer, Discord, a browser
/// upload). Called when the pointer has started dragging a library row; the OS drag loop
/// then runs on the main thread until the drop or a cancel.
///
/// **The one command that stays synchronous**, on purpose: it begins an OS drag loop, which has to
/// start on the UI thread while the button is still down. The only thing that could make it wait
/// is therefore the lookup, which uses [`AppState::with_deps_detached`] so that it never queues
/// behind another command's hold on the index.
#[tauri::command(rename_all = "snake_case")]
fn drag_clip_out(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: i64,
) -> Result<(), CommandError> {
    let path = state.with_deps_detached(|deps| commands::clip_file(deps, id))?;
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
/// On the blocking pool because it runs ffmpeg: the cut takes as long as the footage does, up to
/// a minute, and no lock is held while it does.
#[tauri::command(rename_all = "snake_case")]
async fn trim_clip(
    app: AppHandle,
    id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    blocking(&app, move |state| {
        state.with_deps_detached(|deps| commands::trim_clip(deps, id, start_ms, end_ms))
    })
    .await
}

/// A cached JPEG thumbnail for one clip at one timestamp.
///
/// On the blocking pool because it runs ffmpeg per clip, and a list of clips is a list of ffmpeg
/// runs; [`THUMBNAIL_GATE`] keeps those to one at a time.
#[tauri::command(rename_all = "snake_case")]
async fn thumbnail(app: AppHandle, id: i64, at_ms: u64) -> Result<ThumbnailRef, CommandError> {
    blocking(&app, move |state| {
        let _one_at_a_time = THUMBNAIL_GATE.lock().unwrap_or_else(|err| err.into_inner());
        state.with_deps_detached(|deps| commands::thumbnail(deps, id, at_ms))
    })
    .await
}

/// Delete a clip: the row first, then the file.
#[tauri::command(async, rename_all = "snake_case")]
fn delete_clip(state: State<'_, AppState>, id: i64) -> Result<DeleteOutcome, CommandError> {
    state.with_deps(|deps| commands::delete_clip(deps, id))
}

// -- sessions ----------------------------------------------------------------------------
//
// The session recorder: one scratch directory per recording, concatenated when it stops. The
// timeline these commands read is computed by the store from the session's media epoch; see
// `commands::session_events` for why it must not be recomputed here.

/// Every session the index knows, newest first.
#[tauri::command(async, rename_all = "snake_case")]
fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionDto>, CommandError> {
    state.with_deps(commands::list_sessions)
}

/// One session's row, for the detail panel.
#[tauri::command(async, rename_all = "snake_case")]
fn session_detail(state: State<'_, AppState>, session_id: i64) -> Result<SessionDto, CommandError> {
    state.with_deps(|deps| commands::session_detail(deps, session_id))
}

/// A session's timeline markers, in media-time order.
#[tauri::command(async, rename_all = "snake_case")]
fn session_events(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<Vec<SessionEventDto>, CommandError> {
    state.with_deps(|deps| commands::session_events(deps, session_id))
}

/// A session split into its matches (Full Match segments), with markers and scoreboards.
#[tauri::command(async, rename_all = "snake_case")]
fn session_matches(
    state: State<'_, AppState>,
    session_id: i64,
) -> Result<Vec<commands::MatchDto>, CommandError> {
    state.with_deps(|deps| commands::session_matches(deps, session_id))
}

/// Cut one match out of a finished session as a clip.
///
/// A lossless cut over a long session: ffmpeg, so off the UI thread and with no lock held.
#[tauri::command(rename_all = "snake_case")]
async fn extract_match(
    app: AppHandle,
    session_id: i64,
    match_index: usize,
) -> Result<ClipDto, CommandError> {
    blocking(&app, move |state| {
        state.with_deps_detached(|deps| commands::extract_match(deps, session_id, match_index))
    })
    .await
}

/// Mark a session as a favourite, or clear it.
#[tauri::command(async, rename_all = "snake_case")]
fn set_session_favourite(
    state: State<'_, AppState>,
    session_id: i64,
    favourite: bool,
) -> Result<SessionDto, CommandError> {
    state.with_deps(|deps| commands::set_session_favourite(deps, session_id, favourite))
}

/// Delete a session: the row first, then its segments and its recorded file.
///
/// On the blocking pool because deleting a session removes a whole directory of footage, which
/// takes as long as the disk needs, and no lock is held while it does.
#[tauri::command(rename_all = "snake_case")]
async fn delete_session(
    app: AppHandle,
    session_id: i64,
) -> Result<DeleteSessionOutcome, CommandError> {
    blocking(&app, move |state| {
        state.with_deps_detached(|deps| commands::delete_session(deps, session_id))
    })
    .await
}

/// Cut a clip out of a finished session, losslessly. Refused while it is still recording.
///
/// ffmpeg over the span asked for, so off the UI thread and with no lock held.
#[tauri::command(rename_all = "snake_case")]
async fn extract_clip(
    app: AppHandle,
    session_id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    blocking(&app, move |state| {
        state.with_deps_detached(|deps| commands::extract_clip(deps, session_id, start_ms, end_ms))
    })
    .await
}

// -- recording ---------------------------------------------------------------------------
//
// **No command runs on the UI thread, except the one that has to.** Tauri runs a synchronous
// command on the thread that owns the window, so a command that blocks (on ffmpeg, on a child
// process, on the encoder, or merely on a lock another command holds) freezes every window for
// as long as it blocks. There are three shapes here, and a test
// (`every_command_but_the_drag_runs_off_the_ui_thread`) fails when a fourth appears:
//
// * `async fn` over [`blocking`], for the commands that run for real time: the ones that run
//   ffmpeg (`trim_clip`, `thumbnail`, `combine_clips`, `export_clip`, `extract_clip`,
//   `extract_match`, the screenshot), remove a directory of footage (`delete_session`), wait for
//   PowerShell, or are a recording's start, stop or clip. They run on the blocking pool, which
//   grows to the work, and the index ones hold no lock while they run.
// * `#[tauri::command(async)]` over a synchronous body, for the quick ones that still touch the
//   disk or a lock (every other command). The body runs on the runtime's workers and the window
//   keeps painting; the signature does not have to change for that.
// * `recording_status`, an `async fn` with no blocking in it: a few atomic loads.
//
// `drag_clip_out` is the exception, and says why where it is defined.

/// Start recording through the shared engine.
#[tauri::command(rename_all = "snake_case")]
async fn start_recording(app: AppHandle) -> Result<RecordingStatusDto, CommandError> {
    blocking(&app, |state| {
        // Read at start time, not window-open time: a capture-only mistake must not stop the review
        // pane from opening (see `config.rs`).
        let settings = RecordingConfig::load(&state.recorder.config_path())?;
        state.recorder.start(&settings)
    })
    .await
}

/// Stop recording and flush the encoder. Idempotent.
#[tauri::command(rename_all = "snake_case")]
async fn stop_recording(app: AppHandle) -> Result<RecordingStatusDto, CommandError> {
    blocking(&app, |state| state.recorder.stop()).await
}

/// Get the recording out of the way of an update install.
///
/// The updater's installer ends this process, so whatever is recording is cut off where it stands:
/// the session's scratch directory is left unconcatenated and its row never gets an end. The
/// window calls this **before** it installs, and installs only when it returned `Ok`:
///
/// * nothing recording: `Ok({was_recording: false, stopped: false})`;
/// * recording and `stop_recording` is `false`: an error with code `recording_active`, and the
///   recording is untouched. This is what the window passes first, to learn whether it has to ask
///   the user;
/// * recording and `stop_recording` is `true`: the recording is stopped and finalised exactly as
///   the Stop button does it, and the answer is `Ok({was_recording: true, stopped: true})`. A stop
///   that fails is an error, and the install must not go ahead.
#[tauri::command(rename_all = "snake_case")]
async fn prepare_for_update(
    app: AppHandle,
    stop_recording: bool,
) -> Result<UpdatePreparationDto, CommandError> {
    blocking(&app, move |state| {
        state.recorder.prepare_for_update(stop_recording)
    })
    .await
}

/// The live recording status. Answers "not running" before anything has been started.
#[tauri::command(rename_all = "snake_case")]
async fn recording_status(state: State<'_, AppState>) -> Result<RecordingStatusDto, CommandError> {
    state.recorder.status()
}

/// Take a clip now: wait for the post-roll, splice it losslessly, index it.
#[tauri::command(rename_all = "snake_case")]
async fn clip_now(app: AppHandle) -> Result<RecordedClipDto, CommandError> {
    blocking(&app, |state| state.recorder.clip_now()).await
}

/// What the window shows about the half of the shell that is not a window: the clip hotkey,
/// where the configuration was read from, and what closing the window does. It puts on screen
/// which chord to press and whether it is actually installed, and makes "the config is a file"
/// a readable path.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
/// On the blocking pool because it waits for the clipboard: the call blocks until the child
/// process is done, and the window must not stop answering while it does.
#[tauri::command(rename_all = "snake_case")]
async fn copy_diagnostics(app: AppHandle) -> Result<DiagnosticsReport, CommandError> {
    blocking(&app, |state| {
        commands::copy_diagnostics(&state.app_data_dir, &state.config_path(), state.log_path())
    })
    .await
}

/// The effective encoder settings, for the encoder panel.
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
#[tauri::command(async, rename_all = "snake_case")]
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
    // The clip hotkey is registered once, at startup. A change cannot take effect now, but a
    // chord another application already owns must be refused *before* it is written: a saved
    // chord that cannot install is a startup that reports a dead hotkey for ever after.
    let installed_chord = state.hotkey().chord.clone();
    let installed = state.hotkey().installed;
    if let Some(chord) = update.clip_hotkey.clone() {
        let check = commands::check_clip_hotkey(
            &chord,
            // Only a chord the process really holds is "in force"; a failed installation holds
            // nothing, so the new value still has to be probed.
            if installed {
                Some(installed_chord.as_str())
            } else {
                None
            },
            replayed_events::hotkey::supported(),
            replayed_events::hotkey::probe,
        );
        if let commands::HotkeyCheck::Taken(err) = check {
            return Err(CommandError::new(
                commands::ErrorCode::InvalidInput,
                format!("the clip hotkey was not saved: {err}"),
            ));
        }
    }
    // A directory the asset protocol was taken off earlier in this run stays unreadable until a
    // restart, so moving back to it is refused before anything is written (see
    // `AppState::withdrawn_roots`).
    if let Some(dir) = &update.clips_dir {
        state.check_clips_dir_not_withdrawn(dir)?;
    }
    let running = state.recorder().is_running()?;
    // `update_settings_and_prepare` creates the new clips directory before it writes anything, so
    // what is left here, committing it to the running shell and re-scoping the asset protocol, is
    // all that needs a window.
    let (mut outcome, prepared) = commands::update_settings_and_prepare(
        &state.app_data_dir,
        &state.config_path(),
        &update,
        running,
    )?;
    if let Some(paths) = prepared {
        commit_clips_dir(&state, &app, paths, &outcome.settings.clips_dir)?;
    }
    // The file is written; now make Windows agree with it, and report what it said. Failing the
    // whole command would claim the setting was not saved, and it was — the next launch re-syncs
    // the registry anyway (see `background::sync_autostart`).
    if update.clip_hotkey.is_some() {
        outcome.hotkey = Some(commands::hotkey_change_note(
            update.clip_hotkey.as_deref().unwrap_or_default(),
            installed.then_some(installed_chord.as_str()),
        ));
    }
    if let Some(enabled) = update.start_with_system {
        outcome.autostart = Some(match std::env::current_exe() {
            Ok(exe) => commands::apply_autostart_update(enabled, &exe, background::apply_autostart),
            Err(err) => commands::AutostartReport {
                applied: false,
                detail: format!(
                    "the setting is saved, but this application's own path could not be \
                     resolved ({err}), so the start-with-Windows entry was left alone"
                ),
            },
        });
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
        match scope.forbid_directory(&previous, true) {
            // Remembered, because it cannot be undone: see `AppState::withdrawn_roots`.
            Ok(()) => state.withdraw_clips_root(&previous),
            Err(err) => tracing::warn!(
                "the asset protocol was not taken off {}: {err}. Files in that directory may \
                 still be readable.",
                previous.display()
            ),
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
    if let Some(chord) = &cfg.screenshot_hotkey {
        extras.push((background::HotkeyAction::Screenshot, chord.as_str()));
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
    // The move of the old data directory ran before the subscriber existed (it runs inside the
    // first `app_data_dir` call, which is the log's own set-up), so its report is replayed here.
    config::log_migration_report();

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
    // The Run entry's `--background` says this start came from the login and not from the user, so
    // the review window stays out of the way. Read from this process's own arguments only: a second
    // launch is handed to the running instance by the single-instance plugin below, which shows the
    // window, because that one *is* the user.
    let background_launch = background::is_background_launch(std::env::args_os());
    // Set once the recorder has been flushed for the exit in progress (see `exit_action`).
    let exit_flushed = Arc::new(AtomicBool::new(false));

    let app = tauri::Builder::default()
        // A second launch must hand over to the running instance (which owns the hotkey, tray and
        // capture session) instead of fighting it. Registered first, as the plugin requires.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                // Not the window's own focus call: on Windows it sends a synthetic Alt key, which an
                // anti-cheat treats as automation (see `window_focus`).
                if let Err(err) = window_focus::bring_window_forward(&window) {
                    tracing::warn!(
                        "the running window could not be shown for a second launch: {err}"
                    );
                }
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

            if background_launch {
                // Before the tray is wired, so its first refresh already sees a hidden window.
                match app.get_webview_window(MAIN_WINDOW).map(|w| w.hide()) {
                    Some(Err(err)) => {
                        tracing::warn!("the window could not be hidden for a background start: {err}")
                    }
                    Some(Ok(())) => tracing::info!(
                        "started with --background: the window is hidden, and replayed is in the tray"
                    ),
                    None => {}
                }
            }

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
            screenshot_now,
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
            prepare_for_update,
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
        .build(tauri::generate_context!())
        .expect("error while building the replayed desktop shell");
    app.run(move |app, event| handle_run_event(app, &event, &exit_flushed));
}

/// What an exit request has to do before the process may end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitAction {
    /// The recorder is already flushed (or this is the request that follows the flush): exit.
    Proceed,
    /// Hold the exit, flush the recorder on a thread of its own, then ask again.
    FlushThenExit,
    /// The exit cannot be held (Tauri ignores `prevent_exit` for a restart), so flush here, now.
    FlushInline,
}

/// Decide an exit request. `code` is the request's exit code, `None` when a user asked.
fn exit_action(code: Option<i32>, flushed: bool) -> ExitAction {
    if flushed {
        ExitAction::Proceed
    } else if code == Some(tauri::RESTART_EXIT_CODE) {
        ExitAction::FlushInline
    } else {
        ExitAction::FlushThenExit
    }
}

/// Stop and finalise the recording before the process ends.
///
/// Everything that ends the process without pressing Stop (the tray's Quit, the system closing the
/// application, a restart) comes through the run loop's exit events, so the recorder is flushed
/// *here*: otherwise the in-flight segment is never closed, the session's scratch directory is left
/// unconcatenated and its row never gets an end. A failure is logged and the exit goes ahead; an
/// application that cannot be quit is worse than a recording that could not be finalised.
fn flush_recorder_for_exit(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match state.recorder().stop_for_exit() {
        Ok(true) => tracing::info!("the recording was stopped and finalised for the exit"),
        Ok(false) => {}
        Err(err) => tracing::error!("the recording could not be finalised for the exit: {err}"),
    }
}

/// The run loop's exit handling.
///
/// `ExitRequested` runs on the thread that paints the windows, and the flush takes seconds (the
/// session is concatenated), so the exit is held, the flush runs on a thread of its own (the same
/// rule as [`off_event_loop`]), and that thread asks to exit again once it is done. `Exit` is the
/// last word: it flushes only if nothing earlier did, and may block, because there is no window
/// left to freeze.
///
/// The updater's installer ends the process without any of these events, which is why the window
/// asks `prepare_for_update` before it installs.
fn handle_run_event(app: &AppHandle, event: &RunEvent, flushed: &Arc<AtomicBool>) {
    match event {
        RunEvent::ExitRequested { code, api, .. } => {
            match exit_action(*code, flushed.load(Ordering::Acquire)) {
                ExitAction::Proceed => {}
                ExitAction::FlushInline => {
                    flush_recorder_for_exit(app);
                    flushed.store(true, Ordering::Release);
                }
                ExitAction::FlushThenExit => {
                    let (handle, done) = (app.clone(), Arc::clone(flushed));
                    let code = code.unwrap_or(0);
                    let spawned = std::thread::Builder::new()
                        .name("replayed-exit-flush".into())
                        .spawn(move || {
                            flush_recorder_for_exit(&handle);
                            done.store(true, Ordering::Release);
                            handle.exit(code);
                        });
                    match spawned {
                        Ok(_) => api.prevent_exit(),
                        Err(err) => {
                            // No thread, so no hold: flush on this one and let the exit through.
                            tracing::error!("the exit flush thread could not be started: {err}");
                            flush_recorder_for_exit(app);
                            flushed.store(true, Ordering::Release);
                        }
                    }
                }
            }
        }
        RunEvent::Exit if !flushed.swap(true, Ordering::AcqRel) => flush_recorder_for_exit(app),
        _ => {}
    }
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
#[tauri::command(async)]
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
                        background::HotkeyAction::Screenshot => {
                            let note = match shell.state().and_then(|state| take_screenshot(&state))
                            {
                                Ok(clip) => {
                                    let name = Path::new(&clip.path)
                                        .file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| clip.path.clone());
                                    format!("screenshot saved: {name}")
                                }
                                Err(err) => err.message,
                            };
                            tracing::info!("the screenshot hotkey: {note}");
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
                    // Off the event loop: this callback is the UI thread, and a refresh from it
                    // can wait on itself (see [`off_event_loop`]).
                    let (shell, wiring, hotkey) =
                        (shell.clone(), Arc::clone(&wiring), hotkey.clone());
                    off_event_loop("replayed-tray-refresh", move || {
                        wiring.refresh(&shell, &hotkey);
                    });
                }
            }
        });
    }

    // `setup` runs on the UI thread too, with the poll thread already running.
    {
        let (shell, wiring, hotkey) = (shell.clone(), Arc::clone(&wiring), hotkey.clone());
        off_event_loop("replayed-tray-refresh", move || {
            wiring.refresh(&shell, &hotkey);
        });
    }
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
        // Restores, shows and asks for the foreground without synthesising input; a refused
        // foreground flashes the taskbar button instead (see `window_focus`).
        window_focus::bring_window_forward(&window).map(drop)
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
        reveal_in_file_manager(&path)?;
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
                let id = event.id().as_ref().to_string();
                let Some(action) = MenuAction::from_id(&id) else {
                    tracing::warn!("the tray sent an id this build did not create: {id}");
                    return;
                };
                // **This callback runs on the thread that paints the window, so it only hands the
                // action off.** A start or a stop takes seconds, and the refresh after it waits on
                // this very thread for the icon and menu updates while holding the lock this
                // callback would take (see [`off_event_loop`]).
                let app = app.clone();
                off_event_loop("replayed-tray-action", move || {
                    let _in_order = TRAY_ACTIONS.lock().unwrap_or_else(|err| err.into_inner());
                    let shell = AppShell { app: app.clone() };
                    let outcome = background::dispatch(action, &shell);
                    tracing::info!("tray: {id} → {}", outcome.describe());
                    // The wiring's own refresh: the poll thread would catch up within half a second,
                    // but a menu already stale when it reopens lies twice.
                    if let Some(wiring) = app.try_state::<Arc<TrayWiring>>() {
                        if let Some(state) = app.try_state::<AppState>() {
                            wiring.refresh(&shell, &state.hotkey());
                        }
                    }
                });
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
    ///
    /// **Never call this from the event loop** (a menu or window callback, `setup`). The icon,
    /// tooltip and label updates below are round trips to the UI thread, made while `rendering` is
    /// held so two refreshes cannot interleave their writes; a UI-thread caller waiting for that
    /// lock while the holder waits for the UI thread is a deadlock. Callers there go through
    /// [`off_event_loop`].
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

    /// **The webview's permissions are exactly the three it needs, and a new one is a decision.**
    ///
    /// The capability file used to grant the folder picker (`dialog:allow-open`) and the notification
    /// plugin's defaults, neither of which the page uses: the picker and the toast are driven from
    /// Rust, which the permission system does not gate. A permission the page does not use is only
    /// reach for a page that has been compromised, and the dialog's `open` is the worse kind: the
    /// plugin widens the asset protocol's read scope to whatever the user picks, which is exactly
    /// the widening `choose_clips_dir` exists to keep out of the webview's hands.
    ///
    /// Adding a permission here has to be a change to this list, and so to the security notes that
    /// describe it.
    #[test]
    fn the_webview_holds_only_the_permissions_it_uses() {
        let capability = config("capabilities/default.json");
        let permissions: Vec<&str> = capability["permissions"]
            .as_array()
            .expect("permissions is a list")
            .iter()
            .map(|p| p.as_str().expect("a permission is a string"))
            .collect();

        assert_eq!(
            permissions,
            [
                "core:default",
                "updater:allow-check",
                "updater:allow-download-and-install"
            ],
            "the capability changed: update docs/SECURITY.md's account of what the webview may do"
        );
        let description = capability["description"].as_str().unwrap();
        for claimed_absent in ["dialog", "notification"] {
            assert!(
                description.contains(claimed_absent),
                "the description must say that {claimed_absent} is not granted"
            );
        }
    }

    /// The part of this file that ships: everything before the test module.
    fn production_source() -> &'static str {
        let source = include_str!("lib.rs");
        let end = source
            .find("\n#[cfg(test)]\nmod tests")
            .expect("the test module follows the production code");
        &source[..end]
    }

    /// **An exit request flushes the recorder first, off the event loop where it can be held.**
    #[test]
    fn an_exit_is_held_for_the_flush_unless_it_cannot_be_or_already_happened() {
        // A quit, a close of the application, a request from code: hold, flush, ask again.
        assert_eq!(exit_action(None, false), ExitAction::FlushThenExit);
        assert_eq!(exit_action(Some(0), false), ExitAction::FlushThenExit);
        // The request the flush thread makes itself must go through, or the app never ends.
        assert_eq!(exit_action(None, true), ExitAction::Proceed);
        assert_eq!(exit_action(Some(0), true), ExitAction::Proceed);
        // Tauri ignores `prevent_exit` for a restart, so the flush cannot be deferred.
        assert_eq!(
            exit_action(Some(tauri::RESTART_EXIT_CODE), false),
            ExitAction::FlushInline
        );
        assert_eq!(
            exit_action(Some(tauri::RESTART_EXIT_CODE), true),
            ExitAction::Proceed
        );
    }

    /// **The glue the wave-1 modules were written for is called.** The wiring needs a desktop to
    /// run, so this reads the source, as the guard above does.
    #[test]
    fn the_startup_and_exit_glue_is_wired() {
        let source = production_source();
        assert!(
            !source.contains("reveal_command("),
            "reveal sites must spawn `background::reveal_process`, which quotes for Explorer"
        );
        assert!(source.contains("reveal_process("));
        let tracing = source.find("let log_path = init_tracing();").unwrap();
        let report = source.find("config::log_migration_report();").unwrap();
        let open = source.find("open_state(&app_data_dir").unwrap();
        assert!(
            tracing < report && report < open,
            "the migration report is replayed once logging is up, before the state opens"
        );
        assert!(source.contains("background::is_background_launch(std::env::args_os())"));
        assert!(
            source.contains("RunEvent::ExitRequested") && source.contains("RunEvent::Exit if"),
            "the run loop must flush the recorder on both exit events"
        );
        let migrate = source.find("store.migrate()").unwrap();
        let repair = source.find("config::repair_legacy_index_paths(").unwrap();
        let ffmpeg = source.find("FfmpegBinaries::discover(None)").unwrap();
        assert!(
            migrate < repair && repair < ffmpeg,
            "the index paths are repaired after the migration and before the library is read"
        );
    }

    /// **No command may block the UI thread, except the one that has to start a drag there.**
    ///
    /// Tauri runs a plain `fn` command on the thread that owns the window. A command that blocks
    /// (on ffmpeg, on the encoder, on a lock another command holds across ffmpeg) froze every
    /// window for as long as it blocked; the health page's poll behind a stopping recording was the
    /// reported case, and `combine_clips`, `export_clip` and `extract_clip` ran their ffmpeg right
    /// there. So every command is either an `async fn` or carries `#[tauri::command(async)]`, and
    /// this reads the source to keep it so.
    ///
    /// The one exception, `drag_clip_out`, begins an OS drag loop that has to start on the UI
    /// thread, and says so where it is defined.
    #[test]
    fn every_command_but_the_drag_runs_off_the_ui_thread() {
        let marker = format!("#[{}::command(", "tauri");
        let source = include_str!("lib.rs");
        let mut lines = source.lines().peekable();
        let mut on_ui_thread = Vec::new();
        while let Some(line) = lines.next() {
            let attribute = line.trim_start();
            if !attribute.starts_with(&marker) {
                continue;
            }
            let off_thread = attribute.contains("async");
            // The item the attribute is on: the next line that is not another attribute or a doc line.
            let item = lines
                .by_ref()
                .find(|l| {
                    let l = l.trim_start();
                    !l.starts_with("#[") && !l.starts_with("///")
                })
                .unwrap_or_default()
                .trim_start();
            if !off_thread && !item.starts_with("async fn") {
                on_ui_thread.push(item.to_string());
            }
        }

        assert_eq!(
            on_ui_thread.len(),
            1,
            "commands left on the UI thread: {on_ui_thread:?}"
        );
        assert!(
            on_ui_thread[0].starts_with("fn drag_clip_out("),
            "the only command allowed on the UI thread is the drag, not {:?}",
            on_ui_thread[0]
        );
    }

    /// The tray's callbacks run on the UI thread and must not call the refresh.
    ///
    /// `TrayWiring::refresh` holds the `rendering` lock across round trips to the UI thread, so a
    /// refresh from the UI thread that has to wait for the lock while another refresh holds it and
    /// waits for the UI thread is a deadlock of the whole window. The menu callback and the window
    /// event callback hand their refresh to a thread (`off_event_loop`) instead. This reads those
    /// two closures out of the source and checks that every refresh in them is inside one.
    #[test]
    fn the_event_loop_callbacks_never_refresh_the_tray_themselves() {
        let source = include_str!("lib.rs");
        let code = &source[..source.find("#[cfg(test)]\nmod tests").unwrap()];
        for (name, start, end) in [
            (
                "the tray menu callback",
                ".on_menu_event(",
                ".build(&shell.app)?;",
            ),
            (
                "the window event callback",
                "window.on_window_event(",
                "wiring.refresh(&shell",
            ),
        ] {
            let from = code.find(start).unwrap_or_else(|| panic!("{name} moved"));
            let region = &code[from..];
            let to = region.find(end).unwrap_or_else(|| panic!("{name} moved")) + end.len();
            let region = &region[..to];
            let mut rest = region;
            while let Some(at) = rest.find(".refresh(") {
                let before = &region[..region.len() - rest.len() + at];
                let handed_off = before.rfind("off_event_loop(");
                assert!(
                    handed_off.is_some_and(|open| !before[open..].contains("});")),
                    "{name} calls the tray refresh on the event loop: wrap it in `off_event_loop`"
                );
                rest = &rest[at + ".refresh(".len()..];
            }
        }
    }

    /// `off_event_loop` returns at once, whatever the work does.
    #[test]
    fn handing_work_off_the_event_loop_does_not_wait_for_it() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (finished, done) = std::sync::mpsc::channel::<()>();

        off_event_loop("test-off-loop", move || {
            let _ = wait.recv();
            let _ = finished.send(());
        });

        // Still here, and the work is still waiting: the caller was not held for it.
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the work ran on its own thread");
    }

    /// **A slow command must not hold the index.**
    ///
    /// `with_deps` holds the store's lock for the whole call, so `combine_clips`, `export_clip` and
    /// `extract_clip` (each running ffmpeg inside it) kept every other index command waiting for as
    /// long as the encode took. `with_deps_detached` copies what it needs, lets go of every lock and
    /// opens its own connection, so the two neither wait for each other nor deadlock.
    #[test]
    fn a_detached_command_neither_waits_for_nor_holds_the_index_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let state =
            Arc::new(AppState::open(&dir.path().join("replayed"), storage(), log_path()).unwrap());

        // A command holding the index lock, as the ffmpeg commands used to.
        let (holding_tx, holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                state
                    .with_deps(|_| {
                        holding_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .unwrap();
            })
        };
        holding_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // A detached command answers while the lock is held.
        let (answered_tx, answered_rx) = mpsc::channel();
        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let clips = state.with_deps_detached(commands::list_clips);
                let _ = answered_tx.send(clips.map(|c| c.len()));
            });
        }
        let answered = answered_rx.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(
            answered
                .expect("the detached command waited for the index lock")
                .unwrap(),
            0
        );

        // And while a detached command is running, the locked path is not held up either.
        let (inside_tx, inside_rx) = mpsc::channel();
        let (leave_tx, leave_rx) = mpsc::channel::<()>();
        let detached = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                state
                    .with_deps_detached(|_| {
                        inside_tx.send(()).unwrap();
                        leave_rx.recv().unwrap();
                        Ok(())
                    })
                    .unwrap();
            })
        };
        inside_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (listed_tx, listed_rx) = mpsc::channel();
        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let _ = listed_tx.send(state.with_deps(commands::list_clips).map(|c| c.len()));
            });
        }
        let listed = listed_rx.recv_timeout(Duration::from_secs(5));
        leave_tx.send(()).unwrap();
        detached.join().unwrap();
        assert_eq!(
            listed
                .expect("a detached command held a lock the index needs")
                .unwrap(),
            0
        );
    }

    /// **A clips directory the asset scope was taken off cannot silently come back.**
    ///
    /// Taking a directory out of the asset protocol's scope is irreversible for the life of the
    /// process, and the forbid beats a later allow. Moving the clips directory away and back used to
    /// leave a library whose files would not play with nothing to say why. The state now remembers
    /// what it withdrew and refuses the move back, naming the cause.
    #[test]
    fn a_withdrawn_clips_directory_is_refused_until_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let state = AppState::open(&app_dir, storage(), log_path()).unwrap();
        let old = dir.path().join("old-clips");

        state
            .check_clips_dir_not_withdrawn(old.to_str().unwrap())
            .unwrap();
        state.withdraw_clips_root(&old);

        let again = state
            .check_clips_dir_not_withdrawn(old.to_str().unwrap())
            .unwrap_err();
        assert_eq!(again.code, ErrorCode::InvalidInput);
        assert!(again.message.contains("restart"), "{}", again.message);
        assert!(
            state
                .check_clips_dir_not_withdrawn(old.join("sub").to_str().unwrap())
                .is_err(),
            "the forbid is recursive, so a folder inside it is refused too"
        );
        assert!(
            state
                .check_clips_dir_not_withdrawn(dir.path().to_str().unwrap())
                .is_err(),
            "a folder that contains it would be only partly readable"
        );
        // Another directory, and the default (empty), are untouched.
        state
            .check_clips_dir_not_withdrawn(dir.path().join("other").to_str().unwrap())
            .unwrap();
        state.check_clips_dir_not_withdrawn("").unwrap();
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
            config["version"], "0.2.1",
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

    /// The thumbnail cache is cleaned of files no clip owns when the shell opens, because retention
    /// and the CLI delete clips without telling it (the cache was keyed by rowid and never cleared).
    #[test]
    fn opening_the_shell_sweeps_thumbnails_no_clip_owns() {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("replayed");
        let first = AppState::open(&app_dir, storage(), log_path()).unwrap();
        let thumbs = first.paths.lock().unwrap().thumbnails_dir.clone();
        std::fs::create_dir_all(&thumbs).unwrap();
        let orphan = thumbs.join(commands::thumbnail_file_name(41, 1, 0));
        std::fs::write(&orphan, b"x").unwrap();
        drop(first);

        let _second = AppState::open(&app_dir, storage(), log_path()).unwrap();

        assert!(!orphan.exists(), "a thumbnail of a clip that is gone stays");
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
