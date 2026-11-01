//! The shell's side of the recording engine: the recorder slot and its status DTOs.

use super::settings::recorder_options;
use super::*;
use crate::config::{EngineOptionsConfig, RecordingConfig};
use crate::health::InstalledGsi;
#[cfg(test)]
use replayed_capture::stub::StubConfig;
use replayed_media::FfmpegBinaries;
use replayed_recorder::event_driver::{EventDriver, Policy};
use replayed_recorder::{
    GsiBinding, RecordedClip, Recorder, RecorderConfig, RecorderStatus, Sources,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

// ---------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------
//
// The engine is `replayed-recorder`, the one the CLI drives. Here: one slot that holds the
// recording this window started, and the plain functions the `#[tauri::command]` wrappers
// delegate to.
//
// * `recording_status` never blocks the recording loop. The engine publishes its counters
//   through atomics (`Recorder::status`), so a poll every 500ms is a handful of loads and
//   cannot disturb a capture keeping up with 60fps.
// * A clip trigger does block: `clip_now` waits for the post-roll to be written
//   (`post_seconds` of media plus a margin). The slot lock is released before the wait: the
//   caller clones a handle out of the slot and blocks on that, so the window's status poll keeps
//   answering while a clip is being saved.
// * **A start and a stop block too, and the polls must not notice.** They run the encoder
//   smoke test and the session concatenation, seconds each. What serialises them is the
//   `transition` lock; the slot (`current`) is only ever held for a pointer swap, so the
//   status poll, the tray and the health page never wait behind either. The same goes for the
//   event driver: it is taken out of its lock before it is dropped, because dropping it joins
//   its thread.

/// The live recording status, as the frontend sees it.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the exact
/// JSON keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingStatusDto {
    /// Whether a recording is running. `false` also means "never started".
    pub running: bool,
    /// Video frames submitted to the encoder this session.
    pub frames: u64,
    /// Completed segments in the scratch ring.
    pub segments: u64,
    /// Bytes the ring holds on disk.
    pub bytes: u64,
    /// Media time on disk, in ms: how much footage a clip can be cut from.
    pub span_ms: u64,
    /// Encoder frames dropped because its queue was full (the machine cannot keep up).
    pub dropped: u64,
    /// The same for audio blocks.
    pub dropped_audio: u64,
    /// Frames the capture backend offered and the pacer skipped without reading back.
    pub skipped: u64,
    /// The achieved frame rate over the last second; 0.0 before one has been measured.
    pub fps: f64,
    /// What `encode.fps` asked for, to show the two side by side.
    pub configured_fps: u32,
    /// The rate the pipeline is actually running at: the pacer's rate and the encoder child's
    /// `-framerate`, one number chosen at startup from what the throughput probe measured. Equal to
    /// `configured_fps` unless this machine could not hold the configured rate at the captured
    /// resolution (the engine has logged why). It is the rate the media timeline is recorded at, so
    /// an achieved-rate readout belongs next to it.
    pub effective_fps: u32,
    /// Wall clock minus media time, in ms. Positive means the media timeline is behind
    /// real time, which is why a trigger is taken from `span_ms` rather than the clock.
    pub drift_ms: i64,
    /// Clips this session has written.
    pub clips: u64,
    /// Why the engine stopped, when it stopped for a failure rather than a `stop`.
    pub error: Option<String>,
    /// Whether a game watcher is armed for this recorder: `false` unless `[games] auto_record` was
    /// on at start. True with `running` false means armed and waiting for a game.
    pub watching_games: bool,
    /// The game being recorded, when a watched game triggered it. `None` while nothing
    /// is being recorded, and for a recording a person started.
    pub matched_game: Option<String>,
    /// The mode this recorder was started in (`"buffer"` or `"session"`).
    pub recorder_mode: String,
    /// Whether this recording carries a microphone track (`[mic] enabled`).
    pub mic_enabled: bool,
    /// Why the recording ended on its own without failing (the captured window was closed, or a
    /// resize this mode cannot follow). Kept apart from `error` so the window does not report a
    /// fault that did not happen.
    pub ended: Option<String>,
    /// How long the capture has delivered no frames, in ms, once that is long enough to say.
    pub stalled_for_ms: Option<u64>,
}

impl From<RecorderStatus> for RecordingStatusDto {
    fn from(status: RecorderStatus) -> Self {
        Self {
            running: status.running,
            frames: status.frames,
            segments: status.segments,
            bytes: status.bytes,
            span_ms: status.span_ms,
            dropped: status.dropped,
            dropped_audio: status.dropped_audio,
            skipped: status.skipped,
            fps: status.fps,
            configured_fps: status.configured_fps,
            effective_fps: status.effective_fps,
            drift_ms: status.drift_ms,
            clips: status.clips,
            error: status.error,
            watching_games: status.watching_games,
            matched_game: status.game,
            recorder_mode: status.mode.as_str().to_string(),
            mic_enabled: status.mic,
            ended: status.ended,
            stalled_for_ms: status.stalled_for_ms,
        }
    }
}

/// What a clip trigger produced.
///
/// Not a [`ClipDto`]: a clip that was written but could not be indexed has no row, and reporting
/// it as a clip with a made-up id would hide the failure the index write has to be honest about.
/// `id` is the row when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedClipDto {
    /// The `clips` row, or `null` when the file was written but not indexed, in which case the UI
    /// must say so because the clip will not appear in the list.
    pub id: Option<i64>,
    pub path: String,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    /// The clip's first frame on the engine's media timeline.
    pub started_at_ms: u64,
}

/// What [`RecorderHost::prepare_for_update`] did.
///
/// Mirrored by hand in `src/lib/types.ts` (`UpdatePreparation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePreparationDto {
    /// Whether a recording was running when the call arrived.
    pub was_recording: bool,
    /// Whether this call stopped and finalised it. True only when `was_recording` was and the
    /// caller had said it might be stopped.
    pub stopped: bool,
}

/// The one recording this shell can have running, plus what a start is built from.
///
/// The engine is an `Arc<Recorder>` so a command can take a handle and drop the slot lock before
/// doing slow work (see `clip_now` above). `Mutex` because commands run on several threads.
pub struct RecorderHost {
    /// The directory `scratch/`, `clips/` and `replayed.db` resolve against, and where
    /// `config.toml` is read from when a recording starts.
    app_data_dir: PathBuf,
    /// ffmpeg, or `None` when it could not be found at startup, in which case recording is
    /// impossible and every start says so rather than failing somewhere deeper.
    bins: Option<FfmpegBinaries>,
    ///
    /// **Held only to read or swap the handle, never across slow work.** The window's status
    /// poll, the tray's poll and the health page all read this, and they must answer while a
    /// start is running the encoder smoke test or a stop is concatenating a session: a lock
    /// held across either froze the whole app, because the callers were on the UI thread.
    /// What serialises a start against a stop is [`Self::transition`], not this.
    current: Mutex<Option<Arc<Recorder>>>,
    /// Held for the whole of a start or a stop, so "stop then start" is a sequence rather than a
    /// race between a closing and an opening capture session, and so a start cannot finish
    /// wiring the game-event sources onto a recorder a stop has already taken. Nothing reads
    /// through it: it guards no data, which is why it is separate from [`Self::current`].
    transition: Mutex<()>,
    /// The game-event sources and the thread that turns their events into clips, alive for exactly
    /// as long as the recording they serve.
    events: Mutex<Option<EventDriver>>,
    /// Told about every clip that lands, whichever trigger took it (button, tray, hotkey or a
    /// game event). Set once by the shell, which owns the notification; empty in tests.
    clip_hook: Arc<OnceLock<ClipSavedHook>>,
    /// The GSI cfgs the shell installed into games' own directories, and the binding each
    /// was written from.
    ///
    /// The port the listener binds can change between runs (it is configurable, and it may be
    /// an ephemeral one), so an installed file that names an old port is a game posting into
    /// the void. This record is what lets a start rewrite it. Shared with the shell's state,
    /// which owns the installations.
    installed_gsi: Arc<Mutex<Vec<InstalledGsi>>>,
    /// **TEST-ONLY.** The hooks a test hangs on a stop.
    #[cfg(test)]
    probe: Mutex<StopProbe>,
}

/// What the shell does when a clip is saved (a toast and a sound, in the app).
pub type ClipSavedHook = Box<dyn Fn(&RecordedClipDto) + Send + Sync>;

/// **TEST-ONLY.** The hooks a test hangs on a stop.
///
/// `entered` runs at the start of the stop and `leaving` at the end, and both run with the
/// transition lock held. That the lock is held at both ends is the point: it is what makes a
/// start that arrives mid-stop wait instead of opening a second capture session. The slot is
/// *not* held, which is the other half of the point: a status poll mid-stop answers.
#[cfg(test)]
#[derive(Default)]
pub struct StopProbe {
    /// Called at the start of the stop, with the transition lock held.
    pub entered: Option<Box<dyn Fn() + Send + Sync>>,
    /// Called at the end of the stop, with the transition lock still held.
    pub leaving: Option<Box<dyn Fn() + Send + Sync>>,
}

impl RecordedClipDto {
    fn from_clip(clip: &RecordedClip) -> Self {
        Self {
            id: clip.id,
            path: clip.metadata.path.to_string_lossy().into_owned(),
            duration_ms: clip.metadata.duration_ms,
            size_bytes: clip.metadata.size_bytes,
            codec: clip.metadata.encoder.clone(),
            started_at_ms: clip.started_at_ms,
        }
    }
}

impl RecorderHost {
    pub fn new(
        app_data_dir: PathBuf,
        bins: Option<FfmpegBinaries>,
        installed_gsi: Arc<Mutex<Vec<InstalledGsi>>>,
    ) -> Self {
        Self {
            app_data_dir,
            bins,
            current: Mutex::new(None),
            transition: Mutex::new(()),
            events: Mutex::new(None),
            clip_hook: Arc::new(OnceLock::new()),
            installed_gsi,
            #[cfg(test)]
            probe: Mutex::new(StopProbe::default()),
        }
    }

    /// **TEST-ONLY.** Hang the test's hooks on the next stop.
    #[cfg(test)]
    pub fn watch_stop(&self, probe: StopProbe) {
        *self.probe.lock().unwrap_or_else(|err| err.into_inner()) = probe;
    }

    /// **TEST-ONLY.** The test's "a stop has begun" hook, with the transition lock held.
    #[cfg(test)]
    fn probe_entered(&self) {
        let probe = self.probe.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(hook) = probe.entered.as_ref() {
            hook();
        }
    }

    /// **TEST-ONLY.** The test's "a stop is about to finish" hook, with the transition lock held.
    #[cfg(test)]
    fn probe_leaving(&self) {
        let probe = self.probe.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(hook) = probe.leaving.as_ref() {
            hook();
        }
    }

    /// Register what happens when a clip is saved. Only the first call takes effect.
    pub fn on_clip_saved(&self, hook: impl Fn(&RecordedClipDto) + Send + Sync + 'static) {
        let _ = self.clip_hook.set(Box::new(hook));
    }

    fn announce(hook: &OnceLock<ClipSavedHook>, dto: &RecordedClipDto) {
        if let Some(hook) = hook.get() {
            hook(dto);
        }
    }

    /// The `config.toml` a recording would be started from.
    pub fn config_path(&self) -> PathBuf {
        self.app_data_dir.join("config.toml")
    }

    /// How each game-event source came out of the running recording's startup; empty when
    /// no recording (and so no driver) is up.
    pub fn event_reports(&self) -> Vec<replayed_recorder::event_driver::SourceReport> {
        self.events
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|d| d.reports().to_vec()))
            .unwrap_or_default()
    }

    /// The games the GSI listener serves right now, with the live port and token.
    ///
    /// Empty when nothing is recording, or when the listener is off or failed to start:
    /// there is then nothing serving, so there is nothing for a game to post to and
    /// nothing for the installer to write.
    pub fn gsi_bindings(&self) -> Vec<GsiBinding> {
        self.events
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|driver| driver.gsi_bindings()))
            .unwrap_or_default()
    }

    /// Rewrite the installed GSI cfgs whose live binding has moved.
    ///
    /// The port the listener binds can change between runs, and the token with it, so a
    /// `.cfg` left in a game's folder naming the old ones is exactly the stale file this
    /// exists to prevent: the game posts to a port nobody listens on, or with a token the
    /// listener refuses, and every POST is answered `403` until the application restarts.
    ///
    /// An installation that cannot be rewritten is dropped rather than retried at every
    /// start (see [`crate::health::refresh_installed_gsi`]).
    fn refresh_installed_gsi(&self, bindings: &[GsiBinding]) {
        let Ok(mut installed) = self.installed_gsi.lock() else {
            tracing::warn!("the installed GSI cfgs could not be refreshed: the record is locked");
            return;
        };
        let roots = crate::health::steam_roots();
        crate::health::refresh_installed_gsi(&mut installed, bindings, &roots);
        Self::adopt_installed_gsi(&mut installed, bindings, &roots);
    }

    /// Take over the cfgs an earlier run of the shell installed.
    ///
    /// The record of installed cfgs lives in memory, so after a restart it is empty while the
    /// files are still in the games' folders, naming whatever port and token the previous run's
    /// listener had. With the record empty the refresh above had nothing to rewrite, and the
    /// first run on an ephemeral port (or after the token file was replaced) left CS2 and Dota 2
    /// posting into the void, answered `403` or not answered at all, until the user found the
    /// Settings button and installed again.
    ///
    /// So for every game the listener serves and the record does not know, the disk is asked:
    /// is there a cfg of ours in that game's folder? The file's name is the marker of ownership
    /// ([`crate::health::GSI_CFG_NAME`] is not a name a game or another tool writes), and a file
    /// that is there is rewritten through the ordinary install, which holds it to the same
    /// checks as the first install (the folder has to be a Steam library's, resolved through
    /// symlinks). It is rewritten even when it already names the live binding: the content is
    /// identical, the write is atomic, and the alternative is a second code path that compares
    /// files and can disagree with the first.
    ///
    /// A game with no cfg of ours is left alone: installing into a game folder is the user's
    /// decision, not something a recording start does uninvited.
    fn adopt_installed_gsi(
        installed: &mut Vec<InstalledGsi>,
        bindings: &[GsiBinding],
        roots: &[PathBuf],
    ) {
        let unknown: Vec<_> = crate::health::GsiGame::ALL
            .iter()
            .filter(|game| installed.iter().all(|old| old.game != **game))
            .filter_map(|game| {
                let binding = bindings.iter().find(|b| b.game == game.game_id())?;
                Some((*game, binding))
            })
            .collect();
        if unknown.is_empty() {
            return;
        }
        let libraries = crate::health::steam_libraries(roots);
        let targets = crate::health::gsi_targets(&libraries);
        for (game, binding) in unknown {
            let Some(target) = targets.iter().find(|t| t.game == game && t.installed) else {
                continue;
            };
            let Some(dir) = target.install_dir.as_deref().map(PathBuf::from) else {
                continue;
            };
            match crate::health::install_gsi_cfg(
                game,
                &dir,
                crate::health::GsiListener::Running(binding),
                &libraries,
            ) {
                Ok(outcome) => crate::health::remember_install(
                    installed,
                    game,
                    &dir,
                    std::path::Path::new(&outcome.path),
                    binding,
                ),
                Err(err) => tracing::warn!(
                    "the {} GSI cfg left by an earlier run could not be refreshed: {}",
                    game.label(),
                    err.message
                ),
            }
        }
    }

    /// The engine's own status, undigested (the health page wants the encoder name).
    pub fn raw_status(&self) -> RecorderStatus {
        self.handle()
            .ok()
            .flatten()
            .map_or_else(RecorderStatus::stopped, |r| r.status())
    }

    /// The slot, or the error a poisoned slot deserves.
    ///
    /// Poisoning means a command panicked while holding the slot. Nothing here panics, so it would
    /// be a bug, and the honest answer is to refuse rather than pretend no recording is running.
    ///
    /// The guard is for a read or a swap and nothing slower: see [`Self::current`].
    fn slot(&self) -> Result<MutexGuard<'_, Option<Arc<Recorder>>>, CommandError> {
        self.current.lock().map_err(|_| {
            CommandError::new(
                ErrorCode::Recording,
                "the recording state is unusable: an earlier command panicked while \
                 holding it",
            )
        })
    }

    /// A handle on the running recording, with the slot lock already released.
    fn handle(&self) -> Result<Option<Arc<Recorder>>, CommandError> {
        Ok(self.slot()?.as_ref().map(Arc::clone))
    }

    /// What the recorder is doing right now — never blocks the recording loop, and answers
    /// "not running" before anything has been started.
    pub fn status(&self) -> Result<RecordingStatusDto, CommandError> {
        Ok(match self.handle()? {
            Some(recorder) => RecordingStatusDto::from(recorder.status()),
            None => RecordingStatusDto::from(RecorderStatus::stopped()),
        })
    }

    /// Start recording, and return the first status.
    ///
    /// Everything the engine decides at startup (encoder smoke test, capture backend, ring, ffmpeg)
    /// happens here on the caller's thread, so a machine that cannot record says so now instead of
    /// failing inside a background thread.
    pub fn start(&self, settings: &RecordingConfig) -> Result<RecordingStatusDto, CommandError> {
        self.start_with(settings, Sources::Platform, false, true)
    }

    /// Start from the `config.toml` this host reads, as the window's Start button does.
    ///
    /// The tray's "Start recording" item and the hotkey's own recording both go through here, so
    /// there is one place that decides what a start means, including that the file is re-read at
    /// start time and not at window-open time (see `config.rs`).
    pub fn start_from_config(&self) -> Result<RecordingStatusDto, CommandError> {
        let settings = RecordingConfig::load(&self.config_path())?;
        self.start(&settings)
    }

    /// The engine's configuration, built from the file's settings.
    ///
    /// A function of its own so the mapping can be checked without a start: what the file says
    /// and what the engine is asked for have to agree, and a start is a long way to find out
    /// that they do not.
    fn engine_config(
        &self,
        settings: &RecordingConfig,
        sources: Sources,
        dev_software_encoder: bool,
        bins: &FfmpegBinaries,
    ) -> RecorderConfig {
        RecorderConfig {
            bin: bins.clone(),
            app_data_dir: self.app_data_dir.clone(),
            buffer: settings.buffer.clone(),
            encode: settings.encode.clone(),
            storage: settings.storage.clone(),
            capture: settings.capture.clone(),
            // The `[audio]` bitrate is carried across (the CLI's own value, and the encoder
            // panel's): it reaches ffmpeg's `-b:a`, so the file records what the file says.
            // `enabled` is not read: the shell has no switch for a video-only recording, and
            // absent means the game audio is recorded. Setting either here would be a second
            // source of truth for what the file owns.
            audio_bitrate_kbps: settings.audio.bitrate_kbps,
            audio_enabled: None,
            // WGC + WASAPI on Windows, synthetic stubs elsewhere. This shell has no switch for it and must
            // not: the engine refuses to substitute a stub on Windows, so no path here captures nothing
            // while looking like a recording.
            sources,
            dev_software_encoder,
        }
    }

    /// The body of a start. `sources` and `dev_software_encoder` are the engine's own choices,
    /// passed through unchanged: shipping code takes [`Self::start`], which is exactly
    /// [`Sources::Platform`] with the software encoder off.
    fn start_with(
        &self,
        settings: &RecordingConfig,
        sources: Sources,
        dev_software_encoder: bool,
        with_events: bool,
    ) -> Result<RecordingStatusDto, CommandError> {
        let bins = self.bins.as_ref().ok_or_else(|| {
            CommandError::new(
                ErrorCode::FfmpegUnavailable,
                "ffmpeg was not found, so a recording cannot be started. Install ffmpeg on \
                 PATH or place the sidecar binaries next to the application.",
            )
        })?;

        // The transition lock is held across the start so two concurrent starts cannot both win, and
        // so a stop cannot slip in between the recorder appearing and its event sources being wired
        // onto it (that interleaving left a driver nobody would ever stop). The slot is not held: a
        // start is a handshake with the encoder (a one-frame smoke test at most), and the status
        // poll must answer while it runs.
        let _turn = self.turn();
        // A poisoned slot is refused up front, before anything is started: refusing afterwards would
        // leave a recorder running that nothing could stop.
        if self.handle()?.is_some_and(|recorder| recorder.is_running()) {
            return Err(CommandError::new(
                ErrorCode::Recording,
                "a recording is already running: stop it before starting another",
            ));
        }

        let cfg = self.engine_config(settings, sources, dev_software_encoder, bins);

        // The file's [mic] and [games], not defaults: a start that ignored them would record without
        // the configured microphone and watcher. Malformed sections refuse the start, like the capture
        // sections: a typo'd mic flag must not record silently without it.
        let engine = EngineOptionsConfig::load(&self.config_path())?;
        // **Everything the file can refuse is refused before the recorder exists.** The clip policy
        // used to be built after the start, so a typo in `[events] mute_clips` returned an error with
        // the capture session already running and no handle anyone held to stop it: the user was told
        // the start failed while the screen was being recorded.
        let policy = with_events
            .then(|| Policy::new(&engine.events, settings.buffer.pre_seconds))
            .transpose()
            .map_err(|err| CommandError::new(ErrorCode::InvalidInput, format!("{err:#}")))?;

        // A driver left over from the previous recording (its engine died on its own, or its stop
        // failed) still holds the GSI port. It goes *before* the new one is spawned: the new
        // listener binds the same configured port, and it cannot while the old one is alive, which
        // left CS2 and Dota 2 auto-clips dead for the whole session with only a log line to say why.
        self.retire_events();

        let recorder =
            Recorder::start_with_options(cfg, recorder_options(&engine)).map_err(|err| {
                CommandError::new(
                    ErrorCode::Recording,
                    format!("could not start recording: {err:#}"),
                )
            })?;
        let status = RecordingStatusDto::from(recorder.status());
        let recorder = Arc::new(recorder);
        // A swap, not a hold: the data is a pointer, so a poisoned guard is still a usable one.
        *self.current.lock().unwrap_or_else(|err| err.into_inner()) = Some(Arc::clone(&recorder));

        // Best effort, like everywhere the events are started: a taken port or a missing League install
        // is a warning, never a reason to refuse to record.
        if let Some(policy) = policy {
            match EventDriver::spawn(recorder, &self.app_data_dir, &engine.events, policy, {
                let hook = Arc::clone(&self.clip_hook);
                Box::new(move |clip| Self::announce(&hook, &RecordedClipDto::from_clip(clip)))
            }) {
                Ok(driver) => {
                    // The live bindings, taken before the driver moves: the port the
                    // listener bound is the port the game's cfg has to name, and an
                    // installed cfg that names an older one is rewritten here.
                    let bindings = driver.gsi_bindings();
                    *self.events.lock().unwrap_or_else(|err| err.into_inner()) = Some(driver);
                    self.refresh_installed_gsi(&bindings);
                }
                Err(err) => tracing::warn!("the game-event sources could not start: {err:#}"),
            }
        }
        Ok(status)
    }

    /// **TEST-ONLY.** Start with the engine's synthetic sources and its software encoder.
    ///
    /// The tray and the hotkey both end at [`Self::clip_now`], and the convincing test of that path
    /// is a real recording: a real encoder child, a real ring, a real splice and a real index row.
    /// [`Sources::Platform`] is a live Windows Graphics Capture session on Windows, which no test
    /// may open; [`Sources::Stub`] is the same engine over synthetic frames and is unreachable from
    /// a configuration file (see `replayed_recorder::Sources`). Gated on `cfg(test)`, so a shipped
    /// binary has no such entry point; the CLI's equivalent is `--dev-software-encoder`.
    #[cfg(test)]
    pub fn start_for_test_with_stub_sources(
        &self,
        settings: &RecordingConfig,
    ) -> Result<RecordingStatusDto, CommandError> {
        let stub = StubConfig {
            width: 1280,
            height: 720,
            fps: settings.encode.fps,
        };
        self.start_with(settings, Sources::Stub(stub), true, false)
    }

    /// **TEST-ONLY.** [`Self::start_for_test_with_stub_sources`] with the game-event sources
    /// started too, so a test can reach the clip policy and the GSI listener without a capture
    /// session on the host's display.
    #[cfg(test)]
    pub fn start_for_test_with_stub_sources_and_events(
        &self,
        settings: &RecordingConfig,
    ) -> Result<RecordingStatusDto, CommandError> {
        let stub = StubConfig {
            width: 1280,
            height: 720,
            fps: settings.encode.fps,
        };
        self.start_with(settings, Sources::Stub(stub), true, true)
    }

    /// Stop the recording, flush the encoder and return the final status.
    ///
    /// Idempotent: with nothing running it reports the idle status rather than failing, so a stop
    /// from a window whose recording already died is not an error.
    pub fn stop(&self) -> Result<RecordingStatusDto, CommandError> {
        // Held across the stop, and that is the whole point of it: a start that arrives while the
        // encoder is still flushing and the capture session is still closing waits for the lock
        // instead of opening a second capture session beside the one that is closing. (The window's
        // buttons are serialised by the frontend's `busy` flag; the tray's item and the recording
        // hotkey are not, and they reach this same code.) The slot is *not* held, so the status poll
        // answers for as long as the concatenation takes.
        let _turn = self.turn();
        self.stop_in_turn()
    }

    /// The body of a stop, for a caller that already holds the transition lock.
    fn stop_in_turn(&self) -> Result<RecordingStatusDto, CommandError> {
        // The sources go first: nothing should ask a recorder for a clip while it flushes.
        self.retire_events();
        let taken = self.slot()?.take();
        let Some(recorder) = taken else {
            return Ok(RecordingStatusDto::from(RecorderStatus::stopped()));
        };
        #[cfg(test)]
        self.probe_entered();
        let outcome = recorder.stop().map_err(|err| {
            CommandError::new(
                ErrorCode::Recording,
                format!("stopping the recorder failed: {err:#}"),
            )
        });
        let status = RecordingStatusDto::from(recorder.status());
        // The status is reported either way (a recorder that failed while stopping still has counters
        // worth showing), but a failed stop is still a failure.
        #[cfg(test)]
        self.probe_leaving();
        outcome.map(|()| status)
    }

    /// Get the recording out of the way of an update install, and say what that took.
    ///
    /// The updater's installer ends this process: whatever is recording is cut off where it
    /// stands, a session's scratch directory is left unconcatenated and its row never gets an
    /// end. So the window asks *here* first, and installs only when this has returned `Ok`.
    ///
    /// * Nothing recording: `Ok` with both flags false.
    /// * Recording, `stop_recording` false: **refused** with [`ErrorCode::RecordingActive`]. This
    ///   is the default the window should use until the user has agreed to lose the recording's
    ///   continuity, and a caller that ignores the answer cannot install by accident, because the
    ///   refusal is an error and not a flag to forget to read.
    /// * Recording, `stop_recording` true: stopped and finalised exactly as the Stop button does
    ///   (the session is concatenated and closed), then `Ok` with `stopped` true. A stop that
    ///   fails is an error: an install must not go ahead over a recording that could not be
    ///   flushed.
    ///
    /// The check and the stop are one step under the transition lock, so a start cannot arrive
    /// between them from the tray or the hotkey. One that arrives *after* this returns is the
    /// user's own press during the download; the installer is the only thing left to stop it.
    pub fn prepare_for_update(
        &self,
        stop_recording: bool,
    ) -> Result<UpdatePreparationDto, CommandError> {
        let _turn = self.turn();
        let was_recording = self.handle()?.is_some_and(|recorder| recorder.is_running());
        if !was_recording {
            // A recorder that died by itself still holds the slot and the event sources; it is
            // not a recording, but it is cleared so the install does not outlive half of one.
            self.stop_in_turn()?;
            return Ok(UpdatePreparationDto {
                was_recording: false,
                stopped: false,
            });
        }
        if !stop_recording {
            return Err(CommandError::new(
                ErrorCode::RecordingActive,
                "a recording is running, and installing an update ends it: stop the recording first, or confirm that it may be stopped",
            ));
        }
        self.stop_in_turn()?;
        Ok(UpdatePreparationDto {
            was_recording: true,
            stopped: true,
        })
    }

    /// Stop and finalise whatever is recording because the process is about to end, and say
    /// whether there was a live recording to stop.
    ///
    /// The process ends through paths that never press Stop: the tray's Quit, the OS closing the
    /// application, an installer. Whatever is recording then is cut off where it stands, the
    /// in-flight segment is never closed, the session is left unconcatenated and its row never
    /// gets an end. This is the same stop as the Stop button's (`stop_in_turn`), taken under the
    /// transition lock, so it also **waits for a start or a stop already in flight** instead of
    /// racing it: a Quit that arrives while Stop is flushing must not end the process mid-flush.
    ///
    /// Blocking (seconds, for the concatenation), so the caller keeps it off the UI thread. A
    /// stop that fails is an error for the caller to log, not a reason to refuse to exit.
    pub fn stop_for_exit(&self) -> Result<bool, CommandError> {
        let _turn = self.turn();
        let was_recording = self.handle()?.is_some_and(|recorder| recorder.is_running());
        self.stop_in_turn()?;
        Ok(was_recording)
    }

    /// The transition lock: one start or stop at a time. It guards no data, so poisoning is not
    /// an error worth refusing over.
    fn turn(&self) -> MutexGuard<'_, ()> {
        self.transition
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// Stop the game-event sources, if there are any.
    ///
    /// The driver is taken out of its lock *before* it is dropped: dropping it joins its thread,
    /// and the health page reads the same lock every five seconds from a command the window is
    /// waiting on. Dropping inside the guard made every poll wait for the join.
    fn retire_events(&self) {
        let driver = self
            .events
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        drop(driver);
    }

    /// Take a clip now.
    ///
    /// Blocks for the post-roll (seconds), so the caller must be off the UI thread: the footage is
    /// on disk before this returns.
    pub fn clip_now(&self) -> Result<RecordedClipDto, CommandError> {
        let recorder = self.handle()?.ok_or_else(|| {
            CommandError::new(
                ErrorCode::Recording,
                "nothing is recording, so there is no buffer to take a clip from",
            )
        })?;
        let clip = recorder.clip_now().map_err(|err| {
            CommandError::new(
                ErrorCode::Recording,
                format!("taking a clip failed: {err:#}"),
            )
        })?;
        let dto = RecordedClipDto::from_clip(&clip);
        Self::announce(&self.clip_hook, &dto);
        Ok(dto)
    }

    /// Drop a marker on the running session's timeline (the bookmark hotkey). No clip is cut: it is
    /// a note recorded where a game event would be.
    pub fn bookmark(&self) -> Result<(), CommandError> {
        let recorder = self.handle()?.ok_or_else(|| {
            CommandError::new(
                ErrorCode::Recording,
                "nothing is recording, so there is no timeline to bookmark",
            )
        })?;
        recorder
            .note_event(replayed_events::GameEvent::bare(
                replayed_events::Source::Manual,
                replayed_events::EventKind::Bookmark,
            ))
            .map(|_| ())
            .map_err(|err| {
                CommandError::new(
                    ErrorCode::Recording,
                    format!("the bookmark was not saved: {err:#}"),
                )
            })
    }

    /// Whether a recording currently holds the slot (and the engine says it is running).
    pub fn is_running(&self) -> Result<bool, CommandError> {
        Ok(self.handle()?.is_some_and(|recorder| recorder.is_running()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;
    use replayed_media::probe::MediaInfo;
    use std::time::{Duration, Instant};

    // -- recording -----------------------------------------------------------------------

    /// A recorder host over the fixture's temporary directory.
    ///
    /// `with_ffmpeg` is false for the "no sidecars installed" case; when true the host holds the
    /// real binaries, but no test here lets a start reach capturing (see
    /// `a_start_the_engine_refuses_fails_before_any_capture_starts`).
    fn host(f: &Fixture, with_ffmpeg: bool) -> RecorderHost {
        let app_data_dir = f
            .paths
            .db_path
            .parent()
            .expect("the db lives in the app dir");
        RecorderHost::new(
            app_data_dir.to_path_buf(),
            with_ffmpeg.then(|| f.bins.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    #[test]
    fn recording_status_is_the_idle_status_before_anything_is_started() {
        let f = Fixture::new();
        let status = host(&f, true).status().unwrap();

        assert_eq!(status, RecordingStatusDto::from(RecorderStatus::stopped()));
        assert!(
            !status.running,
            "a shell that has never recorded reports not running"
        );
        assert_eq!(status.configured_fps, 0, "and claims no configured rate");
        assert_eq!(status.effective_fps, 0, "nor a rate in use");
        assert_eq!(status.error, None);
    }

    #[test]
    fn a_start_without_ffmpeg_fails_with_the_code_the_ui_explains() {
        let f = Fixture::new();
        let host = host(&f, false);

        let err = host
            .start(&RecordingConfig::example().unwrap())
            .unwrap_err();

        assert_eq!(err.code, ErrorCode::FfmpegUnavailable);
        assert!(
            err.message.contains("ffmpeg was not found"),
            "got: {}",
            err.message
        );
        assert!(!host.status().unwrap().running, "and nothing was started");
    }

    #[test]
    fn a_start_the_engine_refuses_fails_before_any_capture_starts() {
        // The only test that reaches `RecorderHost::start` with real binaries: the engine resolves the
        // encoder (codec, then vendor) before it creates the capture backend, which on Windows is a
        // live Windows Graphics Capture session. This test fails the start at the codec on every
        // platform, so no capture session ever exists. A start that succeeded would record the screen
        // of whoever ran the suite.
        let f = Fixture::new();
        let host = host(&f, true);
        let mut settings = RecordingConfig::example().unwrap();
        settings.encode.codec = "vp9".to_string();

        let err = host.start(&settings).unwrap_err();

        assert_eq!(err.code, ErrorCode::Recording);
        assert!(
            err.message.contains("unsupported encode.codec"),
            "the message must name the setting, through the command error: {}",
            err.message
        );
        assert!(
            !host.status().unwrap().running,
            "a failed start leaves nothing running"
        );
    }

    /// **The integration test for the feature the background half exists for.**
    ///
    /// One press of the configured chord calls `RecorderHost::clip_now` (the call the window's Save
    /// clip button and the CLI's driver loop make), and that really produces a clip: a real encoder
    /// child, a real ring, a real lossless splice and a real index row.
    ///
    /// The trigger is an in-process function call. **No key, button or mouse event is synthesised,
    /// simulated or injected anywhere in this test or the code it drives**; only `RegisterHotKey`
    /// needs Windows. The capture source is the engine's synthetic stub, so the suite opens no
    /// capture session on the host's display, and the encoder is libx264 because a CI host has no
    /// GPU encoder.
    #[test]
    fn one_hotkey_press_writes_exactly_one_clip_through_the_recorder() {
        const PRE_SECONDS: u64 = 2;
        const POST_SECONDS: u64 = 1;

        let f = Fixture::new();
        let host = host(&f, true);

        // The example config shrunk to run in seconds: 2s pre-roll, 1s post-roll, 10fps, and the
        // capture backend's own size.
        let mut settings = RecordingConfig::example().unwrap();
        settings.buffer.pre_seconds = PRE_SECONDS;
        settings.buffer.post_seconds = POST_SECONDS;
        settings.buffer.segment_time = 1;
        settings.encode.fps = 10;
        settings.encode.output_size = String::new();
        settings.storage.max_total_bytes = u64::MAX;
        settings.storage.max_age_days = 3_650;

        let started = host.start_for_test_with_stub_sources(&settings).unwrap();
        assert!(started.running, "the engine is capturing before any press");

        // Wait for the ring to hold the whole window, as the CLI's self-test does before its trigger:
        // the engine refuses a trigger that asks for footage it does not have yet.
        let need_ms = (PRE_SECONDS + POST_SECONDS) * 1_000;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = host.status().unwrap();
            if status.span_ms >= need_ms {
                break;
            }
            assert!(
                status.running,
                "the engine stopped while the ring was filling: {:?}",
                status.error
            );
            assert!(
                Instant::now() < deadline,
                "the ring never held {need_ms}ms of media (last: {}ms)",
                status.span_ms
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // THE PRESS. `on_hotkey_press` is what the hotkey thread calls; the closure is the call the
        // button makes.
        let outcome = crate::background::on_hotkey_press(|| host.clip_now());

        let status = host.status().unwrap();
        assert!(
            outcome.saved_a_clip(),
            "the press saved no clip: {outcome:?}"
        );
        assert_eq!(
            status.clips, 1,
            "the engine counted exactly one clip, not two"
        );

        // One file on disk, named as the trigger path names them.
        let clips: Vec<PathBuf> = std::fs::read_dir(&f.paths.clips_dir)
            .expect("the clips directory exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("clip-") && name.ends_with(".mp4"))
            })
            .collect();
        assert_eq!(
            clips.len(),
            1,
            "expected exactly one clip file, got {clips:?}"
        );

        // And it is real media of the configured window, produced by the real splice.
        //
        // The assertion is on the clip's video stream, not the container's duration: the splice
        // concatenates whole fragments on the encoder's grid, so the window quantises to that grid, and
        // the container also carries the audio track and stitched timestamps. Measured over fourteen
        // runs, plain and under load, the video stream is 3000ms (the window exactly) or 3100ms when the
        // window's edge falls inside a slightly longer fragment, while the container ran 3187-3396ms;
        // CI once reported 4393ms on a commit that changed no code. A container-duration assertion
        // measures the machine rather than the code.
        //
        // This pins that the video carries both sides of the window (a clip that stopped at the
        // pre-roll, ~2s, or was empty is the failure ruled out) and took the fragments around the
        // window rather than the whole ring, which is the overhang bound below. That bound is one
        // fragment at each end; a fragment can exceed `segment_time` when the host starves the encoder,
        // so it is not tighter.
        let info = MediaInfo::probe(&f.bins, &clips[0]).expect("the clip is readable media");
        let expected_ms = (PRE_SECONDS + POST_SECONDS) * 1_000;
        let segment_ms = settings.buffer.segment_time * 1_000;
        let video = info.video.as_ref().expect("a video stream");
        let video_ms = video
            .duration_ms
            .expect("ffprobe reports the video stream's duration");
        assert!(
            video_ms >= expected_ms * 3 / 4 && video_ms <= expected_ms + 2 * segment_ms,
            "the clip's video is {video_ms}ms, which is not the configured window \
             ({expected_ms}ms ± the segment grid): a clip that carries only one side of the \
             window, or is empty, is the failure this rules out"
        );
        assert_eq!(
            video.codec, "h264",
            "the software encoder is libx264 in a test build"
        );

        // One row in the index: the row `clip_now` wrote, in the store this shell reads.
        let rows = f.store.list_clips().unwrap();
        assert_eq!(rows.len(), 1, "the clip was indexed exactly once");
        assert_eq!(rows[0].path, clips[0], "and the row names the file on disk");

        // A stop flushes the encoder and closes the capture session.
        let stopped = host.stop().unwrap();
        assert!(!stopped.running);
    }

    #[test]
    fn stopping_nothing_is_idempotent_and_says_so() {
        let f = Fixture::new();
        let host = host(&f, true);

        let first = host.stop().unwrap();
        let second = host.stop().unwrap();

        assert_eq!(
            first, second,
            "a stop with nothing running is a value, not a failure"
        );
        assert!(!first.running);
        assert_eq!(first.error, None);
    }

    /// A start that arrives while a stop is in flight must not open a second capture session.
    ///
    /// The stop holds the recorder slot's lock while the encoder flushes and the capture session
    /// closes, so the start waits for it: "stop then start" is a sequence, not two sessions
    /// running beside each other. The tray's item and the recording hotkey reach this code
    /// without the window's `busy` flag between them, which is how the race was reachable.
    ///
    /// The capture source is the engine's synthetic stub, so no session is opened on the host's
    /// display, and the encoder is the software one, as in the integration test above.
    #[test]
    fn a_start_during_a_stop_waits_for_the_stop_to_finish() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        let f = Fixture::new();
        let host = std::sync::Arc::new(host(&f, true));
        let mut settings = RecordingConfig::example().unwrap();
        settings.buffer.pre_seconds = 2;
        settings.buffer.post_seconds = 1;
        settings.buffer.segment_time = 1;
        settings.encode.fps = 10;
        settings.encode.output_size = String::new();

        let started = host
            .start_for_test_with_stub_sources(&settings)
            .expect("the stub start opens no capture session on the host's display");
        assert!(started.running);

        // The stop is held open at its first hook, which runs with the transition lock held. Only the
        // first stop is held: the test stops the recorder again at the end, and that one must
        // not wait for a release that is never sent.
        let (release_tx, release_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        // The release channel lives behind a mutex: a `Receiver` is not `Sync`, and the hook has
        // to be.
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        let held_once = std::sync::Arc::new(AtomicBool::new(false));
        let stop_finished = std::sync::Arc::new(AtomicBool::new(false));
        host.watch_stop(StopProbe {
            entered: Some(Box::new(move || {
                let _ = entered_tx.send(());
                if !held_once.swap(true, Ordering::SeqCst) {
                    // The test holds the stop here, with the transition lock held.
                    let _ = release_rx
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .recv();
                }
            })),
            leaving: Some(Box::new({
                let stop_finished = std::sync::Arc::clone(&stop_finished);
                move || stop_finished.store(true, Ordering::SeqCst)
            })),
        });

        let stopper = {
            let host = std::sync::Arc::clone(&host);
            std::thread::spawn(move || host.stop())
        };
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the stop began");

        // A start, now, while the stop is in flight.
        let start_returned = std::sync::Arc::new(AtomicBool::new(false));
        let start_saw_the_stop_finish = std::sync::Arc::new(AtomicBool::new(false));
        let starter = {
            let host = std::sync::Arc::clone(&host);
            let settings = settings.clone();
            let start_returned = std::sync::Arc::clone(&start_returned);
            let start_saw_the_stop_finish = std::sync::Arc::clone(&start_saw_the_stop_finish);
            let stop_finished = std::sync::Arc::clone(&stop_finished);
            std::thread::spawn(move || {
                let outcome = host.start_for_test_with_stub_sources(&settings);
                start_returned.store(true, Ordering::SeqCst);
                start_saw_the_stop_finish
                    .store(stop_finished.load(Ordering::SeqCst), Ordering::SeqCst);
                outcome
            })
        };

        // The start is waiting for the lock the stop holds: it cannot have opened a capture
        // session of its own while the first one is still closing.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !start_returned.load(Ordering::SeqCst),
            "a start completed while the stop was still in flight: two capture sessions at once"
        );

        // Let the stop finish. The lock is released after the last hook, so the start may only
        // proceed once the stop is done with it.
        release_tx.send(()).unwrap();
        let stopped = stopper.join().unwrap().unwrap();
        assert!(!stopped.running, "the recording stopped");

        let started_again = starter
            .join()
            .unwrap()
            .expect("the start went through after the stop");
        assert!(started_again.running);
        assert!(
            start_saw_the_stop_finish.load(Ordering::SeqCst),
            "the start ran before the stop had released the transition lock"
        );

        host.stop().unwrap();
    }

    /// A small recording config for the tests that start the stub engine.
    fn stub_settings() -> RecordingConfig {
        let mut settings = RecordingConfig::example().unwrap();
        settings.buffer.pre_seconds = 2;
        settings.buffer.post_seconds = 1;
        settings.buffer.segment_time = 1;
        settings.encode.fps = 10;
        settings.encode.output_size = String::new();
        settings
    }

    /// A TCP port nothing holds right now.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// **The UI-thread finding: the status poll must not wait for a stop.**
    ///
    /// `recording_status`, the health page and the tray's 2s poll all read the recorder slot,
    /// and a stop used to hold that slot's lock while the encoder flushed and the session was
    /// concatenated: seconds, during which every one of them blocked, and two of them run on the
    /// main thread. The stop now serialises on the transition lock, so a poll answers while the
    /// stop is held open at its first hook.
    #[test]
    fn a_status_poll_answers_while_a_stop_is_in_flight() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        let f = Fixture::new();
        let host = Arc::new(host(&f, true));
        host.start_for_test_with_stub_sources(&stub_settings())
            .expect("the stub start opens no capture session on the host's display");

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let held_once = Arc::new(AtomicBool::new(false));
        host.watch_stop(StopProbe {
            entered: Some(Box::new(move || {
                let _ = entered_tx.send(());
                if !held_once.swap(true, Ordering::SeqCst) {
                    let _ = release_rx
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .recv();
                }
            })),
            leaving: None,
        });
        let stopper = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.stop())
        };
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the stop began");

        // The polls run on their own thread so a regression fails the test instead of hanging it.
        let (done_tx, done_rx) = mpsc::channel();
        {
            let host = Arc::clone(&host);
            std::thread::spawn(move || {
                let status = host.status();
                let raw = host.raw_status();
                let running = host.is_running();
                let _ = done_tx.send((status, raw, running));
            });
        }
        let answered = done_rx.recv_timeout(Duration::from_secs(3));
        release_tx.send(()).unwrap();
        stopper.join().unwrap().unwrap();
        let (status, _raw, running) = answered
            .expect("the status poll waited behind the stop: the slot lock is held across it");
        assert!(status.is_ok());
        assert!(running.is_ok());
    }

    /// **A start the file refuses must not leave a recorder running.**
    ///
    /// The clip policy used to be built after the recorder started, so a typo in
    /// `[events] mute_clips` returned an error with the capture session already up and no handle
    /// in the slot to stop it: the window said the start failed while the screen was recorded.
    #[test]
    fn an_invalid_mute_clips_refuses_the_start_before_anything_runs() {
        let f = Fixture::new();
        let host = host(&f, true);
        std::fs::write(
            host.config_path(),
            "[events]\nlol_poll_enabled = false\ngsi_port = 0\nmute_clips = [\"not-an-event\"]\n",
        )
        .unwrap();

        let err = host
            .start_for_test_with_stub_sources_and_events(&stub_settings())
            .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("mute_clips"),
            "the message must name the setting: {}",
            err.message
        );
        assert!(
            !host.status().unwrap().running && !host.is_running().unwrap(),
            "the refused start left a recorder behind"
        );
        assert!(host.slot().unwrap().is_none(), "and nothing is in the slot");
    }

    /// **A stale driver must not block the next start's GSI listener.**
    ///
    /// When the engine died by itself (or a stop failed), its driver stayed in the host and kept
    /// the configured GSI port; the next start spawned its new driver *before* dropping the old
    /// one, the bind failed, and auto-clips for CS2 and Dota 2 were dead for the session with
    /// only a log line to say so.
    #[test]
    fn a_stale_event_driver_does_not_block_the_next_starts_gsi_port() {
        let f = Fixture::new();
        let host = host(&f, true);
        let port = free_port();
        std::fs::write(
            host.config_path(),
            format!("[events]\nlol_poll_enabled = false\ngsi_port = {port}\n"),
        )
        .unwrap();
        let settings = stub_settings();

        host.start_for_test_with_stub_sources_and_events(&settings)
            .unwrap();
        assert!(
            !host.gsi_bindings().is_empty(),
            "the first start's listener is up on port {port}"
        );

        // The engine "dies by itself": the recorder is stopped behind the host's back, and its
        // driver is left in place, still holding the port.
        let dead = host.slot().unwrap().take().expect("a recorder was running");
        dead.stop().unwrap();
        assert!(!host.is_running().unwrap());
        assert!(
            !host.gsi_bindings().is_empty(),
            "the stale driver is still there"
        );

        host.start_for_test_with_stub_sources_and_events(&settings)
            .unwrap();
        let bindings = host.gsi_bindings();
        assert!(
            !bindings.is_empty(),
            "the new listener could not bind the port the stale driver was holding"
        );
        assert!(bindings.iter().all(|b| b.port == port));

        host.stop().unwrap();
        assert!(host.gsi_bindings().is_empty(), "a stop retires the driver");
    }

    /// **The update install contract** ([`RecorderHost::prepare_for_update`]).
    #[test]
    fn prepare_for_update_refuses_stops_or_does_nothing_by_the_recording_state() {
        let f = Fixture::new();
        let host = host(&f, true);

        // Nothing recording: nothing to do, and no flag asked for.
        assert_eq!(
            host.prepare_for_update(false).unwrap(),
            UpdatePreparationDto {
                was_recording: false,
                stopped: false
            }
        );

        host.start_for_test_with_stub_sources(&stub_settings())
            .unwrap();

        // Recording and the caller has not agreed to stop: refused, and the recording is untouched.
        let err = host.prepare_for_update(false).unwrap_err();
        assert_eq!(err.code, ErrorCode::RecordingActive);
        assert!(
            host.is_running().unwrap(),
            "a refusal must not stop anything"
        );

        // Recording and the caller agreed: stopped and finalised.
        assert_eq!(
            host.prepare_for_update(true).unwrap(),
            UpdatePreparationDto {
                was_recording: true,
                stopped: true
            }
        );
        assert!(!host.is_running().unwrap());
        assert!(!host.status().unwrap().running);
    }

    /// **Ending the process stops and finalises the recording** ([`RecorderHost::stop_for_exit`]).
    #[test]
    fn stopping_for_exit_finalises_a_live_recording_and_is_a_no_op_otherwise() {
        let f = Fixture::new();
        let host = host(&f, true);

        assert!(
            !host.stop_for_exit().unwrap(),
            "nothing recording: nothing to stop"
        );

        host.start_for_test_with_stub_sources(&stub_settings())
            .unwrap();
        assert!(host.is_running().unwrap());

        assert!(
            host.stop_for_exit().unwrap(),
            "a live recording was stopped"
        );
        assert!(!host.is_running().unwrap());
        assert!(!host.status().unwrap().running);
        assert!(
            !host.stop_for_exit().unwrap(),
            "and a second exit request finds nothing to do"
        );
    }

    /// **An installed GSI cfg from an earlier run is rewritten for this run's binding.**
    ///
    /// The shell's record of installed cfgs is memory-only, so after a restart the files were
    /// still in the games' folders, naming the previous run's port and token, and nothing
    /// refreshed them.
    #[test]
    fn a_cfg_an_earlier_run_installed_is_adopted_and_rewritten() {
        use crate::health::{GsiGame, GSI_CFG_NAME};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Steam");
        let cfg_dir = root.join("steamapps/common/dota 2 beta/game/dota/cfg/gamestate_integration");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg = cfg_dir.join(GSI_CFG_NAME);
        std::fs::write(&cfg, "stale: an earlier run's port and token").unwrap();

        let binding = GsiBinding {
            game: GsiGame::Dota2.game_id().to_string(),
            path: "/gsi/dota2".to_string(),
            port: 51_234,
            app_id: GsiGame::Dota2.app_id(),
            token: "live-token".to_string(),
        };
        let mut installed = Vec::new();

        RecorderHost::adopt_installed_gsi(
            &mut installed,
            std::slice::from_ref(&binding),
            std::slice::from_ref(&root),
        );

        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            binding.cfg(),
            "the stale file was not rewritten for the live binding"
        );
        assert_eq!(installed.len(), 1, "and it is now in the record");
        assert_eq!(installed[0].binding, binding);

        // A game the listener serves but that has no cfg of ours is left alone: installing into a
        // game folder is the user's decision.
        let cs2 = GsiBinding {
            game: GsiGame::Cs2.game_id().to_string(),
            app_id: GsiGame::Cs2.app_id(),
            ..binding
        };
        let mut none = Vec::new();
        let other_root = tmp.path().join("Other");
        std::fs::create_dir_all(
            other_root.join("steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg"),
        )
        .unwrap();
        RecorderHost::adopt_installed_gsi(&mut none, &[cs2], std::slice::from_ref(&other_root));
        assert!(none.is_empty());
        assert!(!other_root
            .join("steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg")
            .join(GSI_CFG_NAME)
            .exists());
    }

    /// **A cfg installed under the legacy shared token is moved to the game's own token.**
    ///
    /// The recorder no longer reads `gsi-token.txt`: each game has a token of its own, so a cfg
    /// an earlier build installed (and copied the one shared token into) is refused with `403` by
    /// the listener until it is rewritten. The adoption that follows a restart is what rewrites
    /// it, so an existing user's GSI keeps working without finding the Settings button.
    #[test]
    fn a_cfg_installed_under_the_legacy_shared_token_is_rewritten_with_the_games_own_token() {
        use crate::health::{GsiGame, GSI_CFG_NAME};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Steam");
        let cfg_dir = root.join("steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg = cfg_dir.join(GSI_CFG_NAME);
        let legacy = "1111111111111111111111111111111111111111111111111111111111111111"; // gitleaks:allow
        let fresh = "2222222222222222222222222222222222222222222222222222222222222222"; // gitleaks:allow
        std::fs::write(&cfg, replayed_events::gsi::integration_cfg(40_000, legacy)).unwrap();

        let binding = GsiBinding {
            game: GsiGame::Cs2.game_id().to_string(),
            path: "/gsi/cs2".to_string(),
            port: 51_234,
            app_id: GsiGame::Cs2.app_id(),
            token: fresh.to_string(),
        };
        let mut installed = Vec::new();
        RecorderHost::adopt_installed_gsi(
            &mut installed,
            std::slice::from_ref(&binding),
            std::slice::from_ref(&root),
        );

        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains(fresh), "the game's own token is in the file");
        assert!(
            !text.contains(legacy),
            "and the legacy one is gone:\n{text}"
        );
        assert!(text.contains(":51234/"), "with the live port:\n{text}");
    }

    #[test]
    fn a_clip_with_nothing_recording_is_refused_rather_than_waited_for() {
        let f = Fixture::new();
        let host = host(&f, true);

        let err = host.clip_now().unwrap_err();

        assert_eq!(err.code, ErrorCode::Recording);
        assert!(
            err.message.contains("nothing is recording"),
            "the message must say why there is no clip: {}",
            err.message
        );
    }

    /// The game-audio rate the file names reaches the engine's configuration.
    ///
    /// The encoder panel offers the rate, so it has to travel: a rate the panel can change and
    /// the recorder ignores would be a lie in the panel.
    #[test]
    fn the_configured_audio_bitrate_reaches_the_engines_configuration() {
        let f = Fixture::new();
        let host = host(&f, true);
        let mut settings = RecordingConfig::example().unwrap();
        settings.audio.bitrate_kbps = Some(160);

        let cfg = host.engine_config(&settings, Sources::Platform, false, &f.bins);

        assert_eq!(
            cfg.audio_bitrate_kbps,
            Some(160),
            "the configured rate must reach the engine"
        );

        // Absent: nothing is carried, and the engine's own default applies.
        settings.audio.bitrate_kbps = None;
        let cfg = host.engine_config(&settings, Sources::Platform, false, &f.bins);
        assert_eq!(cfg.audio_bitrate_kbps, None);
    }

    #[test]
    fn a_host_with_no_recording_serves_no_gsi_bindings() {
        // Nothing is listening, so there is nothing for a game to post to: the installer
        // must refuse rather than write a cfg naming a port nobody holds.
        let f = Fixture::new();
        let host = host(&f, true);

        assert!(host.gsi_bindings().is_empty());
        assert!(host.event_reports().is_empty());
    }

    #[test]
    fn the_recording_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these two structs by hand, like the clip and storage DTOs: without
        // this, a rename on either side is found by a user watching a status readout that never moves.
        let status =
            serde_json::to_value(RecordingStatusDto::from(RecorderStatus::stopped())).unwrap();
        let mut keys: Vec<&str> = status
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes",
                "clips",
                "configured_fps",
                "drift_ms",
                "dropped",
                "dropped_audio",
                "effective_fps",
                "ended",
                "error",
                "fps",
                "frames",
                "matched_game",
                "mic_enabled",
                "recorder_mode",
                "running",
                "segments",
                "skipped",
                "span_ms",
                "stalled_for_ms",
                "watching_games"
            ]
        );

        let clip = serde_json::to_value(RecordedClipDto {
            id: Some(3),
            path: "/clips/clip-1.mp4".to_string(),
            duration_ms: 12_000,
            size_bytes: 1_024,
            codec: "libx264".to_string(),
            started_at_ms: 4_000,
        })
        .unwrap();
        let mut keys: Vec<&str> = clip
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "codec",
                "duration_ms",
                "id",
                "path",
                "size_bytes",
                "started_at_ms"
            ]
        );
        assert_eq!(clip["id"], 3, "an indexed clip carries its row id");
    }

    #[test]
    fn the_status_dto_carries_the_watcher_the_game_and_the_microphone() {
        // The engine tracks all four; the window needs them to say whether anything is being watched.
        let status = RecorderStatus {
            watching_games: true,
            game: Some("Dota 2".to_string()),
            mode: replayed_recorder::RecordingMode::FullSession,
            mic: true,
            ..RecorderStatus::stopped()
        };

        let dto = RecordingStatusDto::from(status);

        assert!(dto.watching_games);
        assert_eq!(dto.matched_game.as_deref(), Some("Dota 2"));
        assert_eq!(dto.recorder_mode, "session");
        assert!(dto.mic_enabled);

        let idle = RecordingStatusDto::from(RecorderStatus::stopped());
        assert!(!idle.watching_games);
        assert_eq!(idle.matched_game, None);
        assert_eq!(idle.recorder_mode, "buffer");
        assert!(!idle.mic_enabled);
    }

    #[test]
    fn a_clean_end_and_a_stall_reach_the_window_apart_from_the_error() {
        // A closed game window ends the recording without a failure; the window must be able to
        // say why it stopped without showing it as one.
        let status = RecorderStatus {
            ended: Some("the captured window was closed".to_string()),
            stalled_for_ms: Some(12_000),
            ..RecorderStatus::stopped()
        };

        let dto = RecordingStatusDto::from(status);

        assert_eq!(dto.ended.as_deref(), Some("the captured window was closed"));
        assert_eq!(dto.stalled_for_ms, Some(12_000));
        assert_eq!(dto.error, None);
    }
}
