/**
 * The shapes the Rust command layer returns, mirrored by hand from
 * `src-tauri/src/commands.rs`.
 *
 * The mirroring is checked from the other side: `commands.rs` has a test
 * (`the_dto_json_matches_the_typescript_interface`) that asserts the exact JSON key set of
 * every DTO here, so a rename on either side fails a Rust test rather than producing an
 * empty list in a window nobody is watching.
 *
 * `null` is used where Rust returns `Option<T>`: serde serialises `None` as `null`, not as
 * an absent key, so these are `| null` rather than optional properties.
 */

/** One clip as the index records it. */
export interface ClipDto {
  id: number;
  /** Absolute path on disk. Playback goes through the asset protocol, never this string. */
  path: string;
  /** Media-time position of the clip's first frame. Not comparable across captures. */
  started_at_ms: number;
  duration_ms: number;
  size_bytes: number;
  codec: string;
  favourite: boolean;
  /** Wall-clock instant the row was written, ms since the Unix epoch. */
  created_at_ms: number;
  /** User-chosen display name; `null` shows the file name. The file is never renamed. */
  title: string | null;
  /** The game the recording matched; `null` when none was detected. */
  game: string | null;
  /** User tags: lower-case, sorted. */
  tags: string[];
}

/** The storage panel's data. */
export interface StorageStats {
  /** Clips whose file is on disk — not the number of rows in the index. */
  clip_count: number;
  /** Bytes those clips occupy, not the total the index's rows claim. */
  total_bytes: number;
  /** Indexed rows whose file is gone: not counted above, and named so the panel can say so. */
  missing_count: number;
  /**
   * Session rows whose recorded file is gone. No clip figure covers a session, so this is the
   * other library's phantom count and the panel offers one reconciliation for both.
   */
  missing_sessions: number;
  favourite_count: number;
  favourite_bytes: number;
  cap_bytes: number;
  max_age_days: number;
  /** `false` means the favourites alone exceed the cap and no pass can meet it. */
  cap_met: boolean;
  over_cap_by_bytes: number;
  planned_deletions: number;
  bytes_after: number;
  clips_dir: string;
  /** Problems found at startup, such as a missing ffmpeg. */
  warnings: string[];
}

/**
 * Where an export went, and what it is.
 *
 * No clip id: an export is a file beside the clip, not a library entry. It is re-encoded on
 * purpose — that is what makes it an export — so it is not managed by the storage policy.
 */
export interface ExportOutcome {
  /** The preset that produced it, by its wire name: `discord`, `vertical` or `gif`. */
  preset: string;
  /** The file written, which the window offers to reveal. */
  path: string;
  size_bytes: number;
  /** The exported file's own length; a re-encode may shift it by a frame or two. */
  duration_ms: number;
}

/** Where a clip's cached thumbnail is. */
export interface ThumbnailRef {
  clip_id: number;
  path: string;
  at_ms: number;
  cached: boolean;
}

/** What removing the index rows whose files are gone did. */
export interface PruneOutcome {
  /** Clip rows removed because their file is gone. */
  clips_removed: number;
  /** Session rows removed because their recorded file is gone. */
  sessions_removed: number;
  /** Cached thumbnails removed for those clips. */
  thumbnails_removed: number;
  /**
   * Session rows the store refused because they are still recording. The plan never selects one,
   * so a non-zero count means a session started between the plan and the delete.
   */
  sessions_refused: number;
  /** Rows that no longer existed when the prune ran. */
  rows_already_gone: number;
  /** Rows whose delete failed; the library is unchanged for them. */
  failed: number;
}

/** What a delete did, in the vocabulary of spec §8.2. */
export interface DeleteOutcome {
  id: number;
  row_deleted: boolean;
  file_removed: boolean;
  /** Set when the row went but the file could not be unlinked. */
  orphaned_path: string | null;
  already_missing: boolean;
  bytes_reclaimed: number;
  thumbnails_removed: number;
}

/**
 * One recording session, as the Sessions list sees it.
 *
 * `media_epoch_ms` is deliberately absent, and its absence is part of the contract: the
 * timeline offsets arrive already computed by the store, and publishing the anchor as well
 * would invite a second subtraction here — which is exactly the cross-clock defect the
 * column exists to fix.
 */
export interface SessionDto {
  id: number;
  /** The game the watcher matched, or `null` for a session started by hand. */
  game: string | null;
  /** `'buffer'` or `'session'`; a buffer session has no concatenated file. */
  mode: string;
  /** Wall clock, ms since the Unix epoch. **Not** comparable with an event's `offset_ms`. */
  started_at_ms: number;
  /** `null` means it is still recording. */
  ended_at_ms: number | null;
  /** The concatenated file, once finalised. `null` while running, and for buffer mode. */
  final_path: string | null;
  size_bytes: number;
  favourite: boolean;
  /** Where the segments are, so the footage can be found without the database. */
  scratch_dir: string;
  /**
   * How long the recorded media is, in ms — the axis a review timeline is drawn against.
   *
   * `0` means **unknown**, not "zero length": a session recovered from a crash has no
   * concatenated file to probe, and rows written before schema v4 predate the column. Disable
   * the scrubber on 0 rather than drawing an axis of no length.
   *
   * Not derivable from `started_at_ms`/`ended_at_ms`, which are the wall-clock window: a
   * session whose encoder could not keep up is shorter than the clock says.
   */
  duration_ms: number;
}

/** One marker on a session's timeline, as the scrubber plots it. */
export interface SessionEvent {
  id: number;
  /**
   * The integration's tag: `'kill'`, `'death'`, `'round_start'`, or the `'bookmark'` a hotkey
   * clip writes. Passed through verbatim — the UI colours an unknown tag neutrally rather
   * than hiding it, because a tag this build has never heard of is a marker that exists.
   */
  kind: string;
  /** Position on the session timeline: media time in ms from the session's start. */
  offset_ms: number;
  /** The integration's detail, as JSON text, verbatim. `null` for a bookmark. */
  payload: string | null;
  /** The clip this marker produced, when it produced one. */
  clip_id: number | null;
}

/** One row of a League end-of-game scoreboard, captured from the Live Client (GET only). */
export interface ScoreRow {
  name: string | null;
  champion: string | null;
  team: string | null;
  kills: number;
  deaths: number;
  assists: number;
  cs: number;
  ward_score: number;
  /** The watched player. */
  is_local: boolean;
}

/** One match inside a session (Full Match splitting), on the session's media timeline. */
export interface MatchDto {
  index: number;
  start_ms: number;
  end_ms: number;
  /** A `game_end` marker closed it; `false` means the end is an inferred boundary. */
  complete: boolean;
  /** No `game_start` marker preceded the end; the start is an inferred boundary. */
  start_inferred: boolean;
  game_mode: string | null;
  map: string | null;
  /** `'Win'` / `'Lose'` when the game reported it; `null` is not a guess of either. */
  result: string | null;
  /** Empty for games and matches without a captured scoreboard. */
  scoreboard: ScoreRow[];
  events: SessionEvent[];
}

/** What deleting a session did, in the vocabulary of spec §8.2. */
export interface DeleteSessionOutcome {
  id: number;
  /** False when nothing named that id: a race with a retention pass is not an error. */
  row_deleted: boolean;
  /** The scratch directory of segments is gone. */
  scratch_removed: boolean;
  /** The concatenated session file is gone. */
  final_file_removed: boolean;
  bytes_reclaimed: number;
  /** Paths the row delete named that could not be removed afterwards. */
  orphaned: string[];
}

/** The live recording status, as `recording_status` returns it. */
export interface RecordingStatus {
  /** False also means "never started": there is one status shape, not two. */
  running: boolean;
  /** Video frames submitted to the encoder this session. */
  frames: number;
  /** Completed segments in the scratch ring. */
  segments: number;
  /**
   * Bytes the ring is holding: **RAM** for a replay buffer (which writes nothing while it is
   * only buffering), **disk** for a full session. Which one follows from `mode`, so a caller
   * showing this to a user should show the mode beside it.
   */
  bytes: number;
  /** Media time on disk, in ms: how much footage a clip can be cut from. */
  span_ms: number;
  /** Encoder frames dropped because its queue was full. */
  dropped: number;
  /** The same for audio blocks. */
  dropped_audio: number;
  /** Frames the capture source offered that were skipped without being read back. */
  skipped: number;
  /** Achieved frame rate over the last second; 0 until one has been measured. */
  fps: number;
  /** What `encode.fps` asked for. */
  configured_fps: number;
  /**
   * The rate the pipeline is running at: the pacer's interval and the encoder child's
   * `-framerate`, decided at startup from what the throughput probe measured. Equal to
   * `configured_fps` unless this machine could not hold the configured rate at the captured
   * resolution, in which case the engine logged why (`encode.adapt_fps`, spec §10.1).
   */
  effective_fps: number;
  /**
   * Wall clock minus media time, ms. Positive means the footage the ring can prove it has is
   * behind the wall clock — the ring counts finished segments, so this includes ffmpeg's lag
   * as well as any clock divergence. Steady on a healthy machine, not a rate to watch.
   */
  drift_ms: number;
  /** Clips written this session. */
  clips: number;
  /** Why the engine stopped, when it stopped for a failure. */
  error: string | null;
  /**
   * Whether a game watcher is armed: false unless `[games] auto_record` was on at
   * start. While true and `running` is false, the recorder is armed and waiting.
   */
  watching_games: boolean;
  /** The game being recorded, when a watched game triggered it; else null. */
  matched_game: string | null;
  /** The mode this recorder was started in (`"buffer"` or `"session"`). */
  recorder_mode: string;
  /** Whether this recording carries a microphone track (`[mic] enabled`). */
  mic_enabled: boolean;
}

/** One active audio endpoint, as `list_audio_devices` returns it. */
export interface AudioDevice {
  /** The WASAPI endpoint id, what the config stores. */
  id: string;
  /** The name Windows shows for it. */
  name: string;
  /** Whether it is the current Windows default for its kind. */
  is_default: boolean;
}

/** The microphones and outputs a person can choose between; both empty off Windows. */
export interface AudioDevices {
  inputs: AudioDevice[];
  outputs: AudioDevice[];
}

/**
 * One `[[games.watch]]` entry: a title the watcher looks for.
 *
 * `exe` is the identity — the watcher compares executable file names, case-insensitively, and
 * nothing else — so two entries with the same executable are one game and the backend refuses the
 * list. `signal` says how the title is detected: by process name, or through the Live Client Data
 * API that only Riot titles publish.
 */
export interface WatchGame {
  /** Human name shown in the panel and the tray: "Rainbow Six Siege". */
  name: string;
  /** Executable file name compared against the process list: `RainbowSix.exe`. */
  exe: string;
  /** `"process"` or `"live_client_api"`. */
  signal: string;
}

/** The settings the window shows, as `get_settings` returns them. */
export interface SettingsDto {
  /** `[storage] clips_dir`, verbatim — empty means the default. */
  clips_dir: string;
  /** Where that resolves: the clips directory itself, for display. */
  clips_dir_resolved: string;
  /** `[encode] fps`. */
  fps: number;
  /** `[encode] output_size` — empty is the native capture size. */
  output_size: string;
  /** `[mic] enabled`. */
  mic_enabled: boolean;
  /** `[mic] device` — a capture endpoint id; empty is the Windows default microphone. */
  mic_device: string;
  /** `[capture] output_device` — an output endpoint id; empty is the Windows default output. */
  output_device: string;
  /** `[recorder] save_session` — also keep the whole recording as a session file. */
  save_session: boolean;
  /** `[games] auto_record`. */
  auto_record: boolean;
  /**
   * `[app] start_with_system`: what the file asks for. Whether Windows really has an entry is
   * the registry's answer, reported after an apply (`UpdateSettingsOutcome.autostart`).
   */
  start_with_system: boolean;
  /** `[encode] bitrate_kbps`. */
  bitrate_kbps: number;
  /** The preset that bitrate is (`low`, `balanced`, `high`) or `custom`. */
  quality: string;
  /** `[capture] monitor` — 0 is the primary monitor. */
  capture_monitor: number;
  /** `[capture] window` — a window title to record instead of a monitor; empty is the monitor. */
  capture_window: string;
  /** `[[games.watch]]`: the titles watched, or the default list when the file names none. */
  watch_games: WatchGame[];
  /** Keys that fell back to the example because the file does not set them. */
  defaulted: string[];
  /** False when no file exists and every value is the example's. */
  config_exists: boolean;
  /** The file that was read. */
  config_path: string;
}

/** A partial settings update: every field optional, absent fields are left alone. */
export interface SettingsUpdate {
  clips_dir?: string;
  fps?: number;
  output_size?: string;
  mic_enabled?: boolean;
  mic_device?: string;
  output_device?: string;
  save_session?: boolean;
  auto_record?: boolean;
  /** `[app] start_with_system`: the writer then reports what Windows did with it. */
  start_with_system?: boolean;
  /** The whole `[[games.watch]]` list; absent leaves it alone. */
  watch_games?: WatchGame[];
  /** A preset name (`low`, `balanced`, `high`); sets `[encode] bitrate_kbps`. */
  quality?: string;
  capture_monitor?: number;
  capture_window?: string;
}

/** What applying `[app] start_with_system` did, when an update changed it. */
export interface AutostartReport {
  /** True when Windows now agrees with the file. */
  applied: boolean;
  /** What was done, or why not — the sentence the notice shows. */
  detail: string;
}

/** What writing settings did, and what it means for a running recorder. */
export interface UpdateSettingsOutcome {
  /** Read back after the write, so the window refreshes from this. */
  settings: SettingsDto;
  /** The keys that changed, e.g. `"encode.fps"`. */
  applied: string[];
  /** True only when a recording is running and an engine key changed value. */
  restart_required: boolean;
  /** Names the changed engine keys when a restart is required, else null. */
  restart_reason: string | null;
  /**
   * What happened to the start-with-Windows entry, when this update changed it. A failure here
   * is a report, not an error: the setting was saved and the next launch re-syncs the registry.
   */
  autostart: AutostartReport | null;
}
/** What a clip trigger produced, as `clip_now` returns it. */
export interface RecordedClip {
  /**
   * The `clips` row, or `null` when the file was written but the index write failed —
   * the clip exists on disk and will not appear in the list, and the UI has to say so.
   */
  id: number | null;
  path: string;
  duration_ms: number;
  size_bytes: number;
  codec: string;
  /** The clip's first frame on the engine's media timeline. */
  started_at_ms: number;
}

/**
 * What the shell can say about the clip hotkey, as `app_status` returns it.
 *
 * `installed: false` is the interesting case: the chord is still named (it is what the user
 * was told to press) and `error` says why nothing is listening — the chord is taken by
 * another application or another replayed, or this build has no global hotkey at all.
 */
export interface HotkeyStatus {
  /** The configured chord, normalised by the Rust side's `Hotkey` display (`Ctrl+F8`). */
  chord: string;
  /** True only when a listener is really installed in the running process. */
  installed: boolean;
  /** Why it is not installed; `null` exactly when `installed` is true. */
  error: string | null;
}

/**
 * The half of the shell that is not a window, as `app_status` returns it.
 *
 * This is the "the config is a file" contract: the window names the file this process read
 * rather than offering a settings panel it does not have.
 */
export interface AppStatus {
  hotkey: HotkeyStatus;
  /** The `config.toml` this process read, `<app data>/config.toml`. */
  config_path: string;
  /** False when that file does not exist and the example's values are in force. */
  config_exists: boolean;
  /** What closing the window does, in the words the panel shows. */
  close_hint: string;
}

/** The machine-readable half of a command failure. */
export type ErrorCode =
  | 'clip_not_found'
  | 'session_not_found'
  | 'session_still_recording'
  | 'invalid_range'
  | 'out_of_range'
  | 'invalid_input'
  | 'ffmpeg_unavailable'
  | 'store'
  | 'media'
  | 'io'
  | 'recording';

/** A structured command failure, as `CommandError` serialises it. */
export interface CommandError {
  code: ErrorCode;
  message: string;
}

/** A trim selection, in milliseconds from the start of a clip. */
export interface TrimRange {
  startMs: number;
  endMs: number;
}

/** The recorder in one word, as `health` returns it. */
export type RecorderState = 'recording' | 'waiting' | 'idle' | 'failed';

export interface DiskHealth {
  path: string;
  free_bytes: number;
  total_bytes: number;
}

export interface EventSourceHealth {
  source: string;
  state: 'running' | 'disabled' | 'failed';
  detail: string;
}

/** The health page, as `health` returns it. Mirrors `health::HealthDto`. */
export interface Health {
  recorder_state: RecorderState;
  recorder_error: string | null;
  /** The encoder the latest recording ran; `null` before any recording. */
  encoder: string | null;
  ffmpeg_available: boolean;
  disk: DiskHealth | null;
  /** Empty until a recording starts: the sources live as long as it does. */
  event_sources: EventSourceHealth[];
  /** WARN/ERROR lines from the log, newest last. */
  recent_errors: string[];
  log_path: string;
}

export type GsiGame = 'cs2' | 'dota2';

/** One game in the settings panel's integration section. */
export interface GsiTarget {
  game: GsiGame;
  label: string;
  /** `null` when no Steam library has the game. */
  install_dir: string | null;
  installed: boolean;
}

export interface GsiInstallOutcome {
  game: GsiGame;
  path: string;
  replaced: boolean;
}
