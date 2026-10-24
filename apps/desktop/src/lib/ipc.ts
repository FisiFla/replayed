/**
 * The one place the frontend talks to the Rust side.
 *
 * Two things are worth knowing about this file:
 *
 * 1. **Arrow arguments are snake_case.** The commands are declared
 *    `#[tauri::command(rename_all = "snake_case")]`, so `trim_clip`'s parameters arrive as
 *    `start_ms` / `end_ms` — not the camelCase Tauri would use by default. A mismatch here
 *    is a runtime error ("missing required key start_ms") rather than a compile error on
 *    either side, which is why `ipc.test.ts` asserts the exact key names against a mocked
 *    `invoke`.
 * 2. **Playback goes through the asset protocol, not through a command.** `convertFileSrc`
 *    turns an absolute path into the `asset:` URL that `<video>` can play, and Tauri's
 *    protocol handler checks it against the scope configured in `tauri.conf.json` (and
 *    extended at runtime in `src/lib.rs`). There is no file server in this application,
 *    and no command returns file contents.
 * 3. **Every failed command is written to the application's log here**, by the one wrapper
 *    below, before the rejection reaches the caller. The components turn failures into a
 *    banner for whoever is in front of the window; the log is for whoever is not, and a
 *    shipped window has no console anyone can read.
 */

import { convertFileSrc, invoke as tauriInvoke } from '@tauri-apps/api/core';
import type {
  AppStatus,
  ClipDto,
  CommandError,
  DeleteOutcome,
  DeleteSessionOutcome,
  ErrorCode,
  ExportOutcome,
  GsiGame,
  GsiInstallOutcome,
  AudioDevices,
  GsiTarget,
  Health,
  MatchDto,
  PruneOutcome,
  RecordedClip,
  RecordingStatus,
  SessionDto,
  SessionEvent,
  SettingsDto,
  SettingsUpdate,
  StorageStats,
  ThumbnailRef,
  UpdateSettingsOutcome,
} from './types';

/**
 * The encoder settings, as `encoder_settings` and `update_encoder_settings` return them.
 *
 * Mirrors the Rust side's `[encode]` table (see `config.example.toml`): the vendor asked
 * for, the codec, the bitrate and the frame-rate ceiling, the output size, whether the frame
 * rate adapts to what the machine can sustain, whether the zero-copy path is wanted, and the
 * rate control and preset.
 *
 * `zero_copy` is three-valued on purpose: `null` is "use the hardware path when it is
 * there", which is not the same as `false` ("never use it"). The difference is the difference
 * between a default that works on every machine and one that refuses to start.
 */
export interface EncoderSettings {
  /** `"auto"`, `"nvenc"`, `"qsv"` or `"amf"`. */
  vendor: string;
  /** `"h264"` or `"hevc"`. */
  codec: string;
  bitrate_kbps: number;
  /** A ceiling, not a promise: `adapt_fps` may record at less. */
  fps: number;
  /** `""` for the native capture size, else `"WIDTHxHEIGHT"`. */
  output_size: string;
  adapt_fps: boolean;
  /** `null` means "use the hardware path when it is there". */
  zero_copy: boolean | null;
  rate_control: string;
  /** `""` for the encoder's own default. */
  preset: string;
  audio_bitrate_kbps: number;
}

/**
 * What the encoder that was selected can actually do.
 *
 * The panel reads this instead of offering the union of every vendor's knobs: an encoder
 * that has no CRF has no CRF, and a control that offers one and quietly drops it is worse
 * than no control at all.
 */
export interface EncoderCapabilities {
  /** The rate-control modes this encoder has: `cbr`, `vbr`, `crf`, `quality`. */
  rate_control_modes: string[];
  /** The presets this encoder has. Empty means it has none of its own. */
  presets: string[];
  /** False when the chosen path cannot scale — the output is the capture size. */
  supports_scaling: boolean;
  max_bitrate_kbps: number;
}

/** Which encoder the backend would use, and what it can do. */
export interface EncoderStatus {
  /** The vendor the settings ask for — `"auto"` means "probe this machine". */
  requested_vendor: string;
  /** The vendor that was actually chosen; `null` when none is available. */
  selected_vendor: string | null;
  /** The codec that was actually chosen; `null` when none is available. */
  selected_codec: string | null;
  capabilities: EncoderCapabilities;
  /** The backend's own explanation of the choice, in words. */
  message: string;
}

/**
 * One display the machine has, in `[capture] monitor` order.
 *
 * `refresh_hz` is `null` when the OS does not report a real rate — Windows answers 0 or 1
 * for a mode that inherits the display's default rate — and that is "unknown", not a rate
 * a person could pick from.
 */
export interface CaptureDisplay {
  index: number;
  width: number;
  height: number;
  /** The mode's refresh rate in Hz, or null when the OS does not report a real one. */
  refresh_hz: number | null;
}

/** The displays this machine has, and the one the capture is pointed at. */
export interface CaptureDisplays {
  displays: CaptureDisplay[];
  /** The index the capture is pointed at, or null when the settings could not be read. */
  selected: number | null;
}

/**
 * A diagnostics report, as `copy_diagnostics` returns it.
 *
 * `config` is the effective configuration with the secrets already redacted by the backend;
 * `log_tail` is the end of the application's log. The report is put on the clipboard by the
 * same command and is never sent anywhere: this application has no telemetry, and the point
 * of the button is that the user decides where it goes.
 */
export interface DiagnosticsReport {
  version: string;
  platform: string;
  generated_at: string;
  config: string;
  problems: string[];
  log_tail: string;
}

/** The commands this window can call, as an interface. */
export interface ClipSource {
  listClips(): Promise<ClipDto[]>;
  storageStats(): Promise<StorageStats>;
  setFavourite(id: number, favourite: boolean): Promise<ClipDto>;
  /** Set the clip's display title (blank clears it). The file keeps its name. */
  setClipTitle(id: number, title: string): Promise<ClipDto>;
  /** Replace the clip's tags. */
  setClipTags(id: number, tags: string[]): Promise<ClipDto>;
  /** Put the clip's file on the system clipboard, as a file. */
  copyClipToClipboard(id: number): Promise<void>;
  /** Show the clip's file in the system file manager. */
  revealClip(id: number): Promise<void>;
  /** Start a native drag of the clip's file out of the window. */
  dragClipOut(id: number): Promise<void>;
  trimClip(id: number, startMs: number, endMs: number): Promise<ClipDto>;
  thumbnail(id: number, atMs: number): Promise<ThumbnailRef>;
  deleteClip(id: number): Promise<DeleteOutcome>;
  /**
   * Remove the index rows whose files are gone, from both libraries. This is the one command
   * that rewrites the index from the filesystem's evidence, so the Storage panel asks for
   * confirmation first; the row's files are already gone and nothing here deletes anything.
   *
   * Refused when the clips directory cannot be read (an offline or unmounted directory looks
   * exactly like a library whose clips were all deleted), and a session that is still recording
   * is never removed.
   */
  pruneMissing(): Promise<PruneOutcome>;
  /**
   * Combine whole clips into one, losslessly. At least two, each once; every one must be on disk
   * and inside the library, and they must share a stream layout — mixed codecs or sizes are
   * refused rather than concatenated into a file with a truncated track.
   */
  combineClips(ids: number[]): Promise<ClipDto>;
  /**
   * Re-encode a clip with an export preset into a new file beside it. Not added to the library:
   * an export is an artefact to upload, not more footage under the storage cap.
   */
  exportClip(id: number, preset: string): Promise<ExportOutcome>;
  /**
   * Take one frame of the configured capture target and save it as a PNG in the library.
   *
   * Async on the backend side: it waits for the display to present a frame. A target that never
   * presents (a minimised window, a display that went away) fails after a bounded wait.
   */
  screenshotNow(): Promise<ClipDto>;
  /**
   * Show the application's data directory in the system file manager — the folder holding the
   * clips, the sessions, the index, the log and `config.toml`.
   */
  openDataDir(): Promise<void>;
  /**
   * Sessions: one recording, the file it was concatenated into, and the markers on its
   * timeline. Read-only apart from the favourite flag and deletion — the engine that writes
   * a session is the recorder, not this window.
   */
  listSessions(): Promise<SessionDto[]>;
  sessionDetail(sessionId: number): Promise<SessionDto>;
  /**
   * A session's markers, in media-time order. `offset_ms` is media time from the session's
   * start, computed by the store; nothing here recomputes it against the wall clock.
   * An empty list is a real answer — a session nobody tagged has no markers.
   */
  sessionEvents(sessionId: number): Promise<SessionEvent[]>;
  /**
   * A session split into its matches (Full Match segments), each with its markers and, for
   * League, the end-of-game scoreboard. Empty for a session with no game markers.
   */
  sessionMatches(sessionId: number): Promise<MatchDto[]>;
  /** Cut one match out of a **finished** session as a clip. */
  extractMatch(sessionId: number, matchIndex: number): Promise<ClipDto>;
  setSessionFavourite(sessionId: number, favourite: boolean): Promise<SessionDto>;
  /** Delete a session: its row, then its segments and its recorded file. */
  deleteSession(sessionId: number): Promise<DeleteSessionOutcome>;
  /** Cut a clip out of a **finished** session, losslessly. Refused while it is recording. */
  extractClip(sessionId: number, startMs: number, endMs: number): Promise<ClipDto>;
  /**
   * The settings the window shows and edits (`config.toml`, read at startup everywhere
   * else). Read-only apart from `updateSettings` — the engine that writes a session is
   * the recorder, not this window.
   */
  getSettings(): Promise<SettingsDto>;
  /**
   * Validate and write edited keys. Every field optional; absent fields are left alone.
   * `storage.clips_dir` applies immediately (future writes); fps, output size, mic and
   * auto-record persist and take effect on the next recorder start.
   */
  updateSettings(update: SettingsUpdate): Promise<UpdateSettingsOutcome>;
  /**
   * The native folder picker, run by the backend, for the clips directory.
   *
   * The backend runs it rather than the window so that the folder it returns is one it
   * has *approved*: only a directory the user picked in the OS dialog is ever added to
   * the asset-protocol read scope, so a path typed into the panel cannot widen what the
   * webview may read. The chosen path comes back for the panel to show and apply.
   */
  chooseClipsDir(): Promise<string>;
  /**
   * Recording, through the same engine the CLI drives (`replayed-recorder`). These four
   * are the only commands that make the shell capture anything, and none of them is
   * called by this window on its own initiative: the user presses the button.
   */
  startRecording(): Promise<RecordingStatus>;
  stopRecording(): Promise<RecordingStatus>;
  recordingStatus(): Promise<RecordingStatus>;
  clipNow(): Promise<RecordedClip>;
  /**
   * The half of the shell that is not a window: the clip hotkey (the chord, and whether a
   * listener is really installed), where `config.toml` was read from, and what closing the
   * window does. Read once when the window opens — none of it changes while it runs.
   */
  appStatus(): Promise<AppStatus>;
  /**
   * Write a frontend error into the application's log, beside the backend's own.
   *
   * It is on this surface rather than called through `invoke` directly because the suite below
   * cross-checks these methods against the handler list in `lib.rs`: a command reached one way on
   * one side and another way on the other is exactly what that guard exists to catch.
   */
  /** Recorder, encoder, disk, event sources and the log's recent problems. */
  health(): Promise<Health>;
  /**
   * The encoder settings, as the backend reads them (`[encode]` in `config.toml`).
   *
   * Read-only apart from `updateEncoderSettings`: the engine that writes a recording is the
   * recorder, not this window.
   */
  encoderSettings(): Promise<EncoderSettings>;
  /**
   * Validate and write the encoder settings, and answer with the values that were written.
   *
   * The whole table travels in one object — there is no partial update — because the backend
   * validates it as a whole and refuses a combination it cannot record with. A refusal comes
   * back as a rejection carrying the reason, which the panel shows.
   */
  updateEncoderSettings(settings: EncoderSettings): Promise<EncoderSettings>;
  /**
   * Which encoder the backend would use, and what it can do.
   *
   * The capabilities belong to the encoder that was *selected*, not to the union of every
   * vendor's: the panel offers only the modes and presets this one has.
   */
  encoderStatus(): Promise<EncoderStatus>;
  /**
   * The displays this machine has, and the one the capture is pointed at.
   *
   * Read so the framerate setting can be made against the display's own refresh rate: a
   * capture rate above the display's rate buys nothing, because the display has nothing
   * new to show. The list is empty off Windows, where there is no screen to enumerate.
   */
  captureDisplays(): Promise<CaptureDisplays>;
  /**
   * Assemble a diagnostics report, put it on the clipboard, and answer with it.
   *
   * Nothing is sent anywhere: the report is on the clipboard and the user pastes it wherever
   * they want to. This application has no telemetry, and the report says so.
   */
  copyDiagnostics(): Promise<DiagnosticsReport>;
  /** Which of CS2 / Dota 2 Steam has installed, and whether the cfg is already there. */
  gsiTargets(): Promise<GsiTarget[]>;
  /** The active microphones and outputs, for the settings pickers. */
  listAudioDevices(): Promise<AudioDevices>;
  /**
   * Write the Game State Integration cfg into that game's detected folder. Only the game is
   * sent: the backend finds and validates the directory itself.
   */
  installGsiCfg(game: GsiGame): Promise<GsiInstallOutcome>;
  logFromFrontend(level: 'error' | 'warn' | 'info', message: string, detail: string): Promise<void>;
  /** An `asset:` URL for an absolute path, for `<video>` and `<img>`. */
  assetUrl(path: string): string;
}

/**
 * `invoke`, with every failure named in the application's log as it goes past.
 *
 * One wrapper rather than a `logFromFrontend` call at each `catch` in the components, and
 * for the reason this file exists at all: the boundary is the only place every command
 * passes through. `App.svelte` alone has sixteen `catch` blocks that turn a rejection into
 * a banner, the settings panel has another, and a `catch` written next month would have to
 * remember to log. Here it cannot be forgotten, and the command's own name goes with it.
 *
 * It still rejects. The banner belongs to the user, the log line to the maintainer, and a
 * failed command has to reach the code that decides what to say about it.
 */
async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    // Same arity as a direct `invoke`: `ipc.test.ts` asserts which commands are called with
    // no arguments at all, and passing a stray `undefined` would blur that.
    return args === undefined ? await tauriInvoke<T>(command) : await tauriInvoke<T>(command, args);
  } catch (err) {
    // Not through the log's own command: if `log_from_frontend` fails there is nowhere left
    // to say so, and reporting it through itself is a rejection loop.
    if (command !== 'log_from_frontend') {
      void tauriInvoke('log_from_frontend', {
        level: 'error',
        message: `${command} failed`,
        detail: errorMessage(err),
      }).catch(() => {});
    }
    throw err;
  }
}

/** The real IPC, backed by the Tauri runtime. */
export const tauriIpc: ClipSource = {
  listClips: () => invoke<ClipDto[]>('list_clips'),
  storageStats: () => invoke<StorageStats>('storage_stats'),
  setFavourite: (id, favourite) => invoke<ClipDto>('set_favourite', { id, favourite }),
  setClipTitle: (id, title) => invoke<ClipDto>('set_clip_title', { id, title }),
  setClipTags: (id, tags) => invoke<ClipDto>('set_clip_tags', { id, tags }),
  copyClipToClipboard: (id) => invoke<void>('copy_clip_to_clipboard', { id }),
  revealClip: (id) => invoke<void>('reveal_clip', { id }),
  dragClipOut: (id) => invoke<void>('drag_clip_out', { id }),
  trimClip: (id, startMs, endMs) =>
    invoke<ClipDto>('trim_clip', { id, start_ms: startMs, end_ms: endMs }),
  thumbnail: (id, atMs) => invoke<ThumbnailRef>('thumbnail', { id, at_ms: atMs }),
  deleteClip: (id) => invoke<DeleteOutcome>('delete_clip', { id }),
  pruneMissing: () => invoke<PruneOutcome>('prune_missing'),
  combineClips: (ids) => invoke<ClipDto>('combine_clips', { ids }),
  exportClip: (id, preset) => invoke<ExportOutcome>('export_clip', { id, preset }),
  openDataDir: () => invoke<void>('open_data_dir'),
  screenshotNow: () => invoke<ClipDto>('screenshot_now'),
  listSessions: () => invoke<SessionDto[]>('list_sessions'),
  sessionDetail: (sessionId) => invoke<SessionDto>('session_detail', { session_id: sessionId }),
  sessionEvents: (sessionId) =>
    invoke<SessionEvent[]>('session_events', { session_id: sessionId }),
  sessionMatches: (sessionId) =>
    invoke<MatchDto[]>('session_matches', { session_id: sessionId }),
  extractMatch: (sessionId, matchIndex) =>
    invoke<ClipDto>('extract_match', { session_id: sessionId, match_index: matchIndex }),
  setSessionFavourite: (sessionId, favourite) =>
    invoke<SessionDto>('set_session_favourite', { session_id: sessionId, favourite }),
  deleteSession: (sessionId) =>
    invoke<DeleteSessionOutcome>('delete_session', { session_id: sessionId }),
  extractClip: (sessionId, startMs, endMs) =>
    invoke<ClipDto>('extract_clip', { session_id: sessionId, start_ms: startMs, end_ms: endMs }),
  startRecording: () => invoke<RecordingStatus>('start_recording'),
  stopRecording: () => invoke<RecordingStatus>('stop_recording'),
  recordingStatus: () => invoke<RecordingStatus>('recording_status'),
  clipNow: () => invoke<RecordedClip>('clip_now'),
  appStatus: () => invoke<AppStatus>('app_status'),
  getSettings: () => invoke<SettingsDto>('get_settings'),
  updateSettings: (update: SettingsUpdate) =>
    invoke<UpdateSettingsOutcome>('update_settings', { update }),
  chooseClipsDir: () => invoke<string>('choose_clips_dir'),
  health: () => invoke<Health>('health'),
  encoderSettings: () => invoke<EncoderSettings>('encoder_settings'),
  updateEncoderSettings: (settings: EncoderSettings) =>
    invoke<EncoderSettings>('update_encoder_settings', { settings }),
  encoderStatus: () => invoke<EncoderStatus>('encoder_status'),
  captureDisplays: () => invoke<CaptureDisplays>('capture_displays'),
  copyDiagnostics: () => invoke<DiagnosticsReport>('copy_diagnostics'),
  gsiTargets: () => invoke<GsiTarget[]>('gsi_targets'),
  listAudioDevices: () => invoke<AudioDevices>('list_audio_devices'),
  installGsiCfg: (game) => invoke<GsiInstallOutcome>('install_gsi_cfg', { game }),
  logFromFrontend: (level, message, detail) =>
    invoke<void>('log_from_frontend', { level, message, detail }),
  assetUrl: (path) => convertFileSrc(path),
};

/**
 * The source a component talks to when it has none of its own.
 *
 * `App` takes its source as a prop, which is how the screenshot harness and the tests drive
 * the whole window. But the panels *below* it own commands of their own — the encoder
 * settings in the settings panel, the diagnostics export in the health panel — and the shell
 * that would hand them the source sits above them, so a prop cannot reach them without
 * threading it through every component in between.
 *
 * So the seam is resolved here, once, with the same two halves `main.ts` requires: the
 * `?test-clip-source` query string *and* the `globalThis.__replayedClipSource` object. A
 * shipped window is loaded with neither, so this is the real Tauri IPC there — exactly as it
 * is for `App`.
 */
export function activeSource(): ClipSource {
  if (typeof location === 'undefined') return tauriIpc;
  if (!new URLSearchParams(location.search).has('test-clip-source')) return tauriIpc;
  const injected = (globalThis as { __replayedClipSource?: ClipSource }).__replayedClipSource;
  return injected ?? tauriIpc;
}

/**
 * Whether a rejection is one of our structured command errors.
 *
 * Tauri hands the serialised error straight back to the caller, so the value reaching a
 * `catch` is a plain object with `code` and `message` — not an `Error` instance, and not
 * carrying a stack.
 */
export function isCommandError(err: unknown): err is CommandError {
  if (typeof err !== 'object' || err === null) return false;
  const candidate = err as { code?: unknown; message?: unknown };
  return typeof candidate.code === 'string' && typeof candidate.message === 'string';
}

/** The error class, or `null` if this is not one of ours. */
export function errorCode(err: unknown): ErrorCode | null {
  return isCommandError(err) ? err.code : null;
}

/**
 * A message worth putting in front of the user.
 *
 * Tauri rejections arrive in three shapes — our `CommandError`, a string, or a plain
 * `Error` from the runtime itself (a command that is not registered rejects with a string
 * like "Command not found") — and none of them should render as "[object Object]".
 */
export function errorMessage(err: unknown): string {
  if (isCommandError(err)) return err.message;
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  if (err === null || err === undefined) return 'The command failed for an unknown reason.';
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
