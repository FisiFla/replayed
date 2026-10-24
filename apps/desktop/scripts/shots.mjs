#!/usr/bin/env node
/**
 * `npm run shots` — render the built frontend headlessly and photograph it.
 *
 * WHAT THIS IS FOR
 * ----------------
 * This application's window has never been opened on any machine: the Rust half is tested,
 * the frontend logic is unit-tested with vitest, and `svelte-check` type-checks the
 * components — but none of that can tell you whether the clip list is legible, whether the
 * trim handles are on top of the playhead, or whether the storage panel's "this cap cannot
 * be met" verdict reads as a warning. Those are properties of pixels, so this script makes
 * pixels: it builds the frontend, serves `dist/` over HTTP, loads it in headless Chromium
 * with the Tauri IPC replaced by a mock (the seam in `src/main.ts`, armed with
 * `?test-clip-source=1` plus a `globalThis.__replayedClipSource`), drives the real UI with
 * real pointer events, and writes PNGs to `<repo>/target/desktop-shots/`.
 *
 * Every PNG is then decoded (`./png-stats.mjs`) and asserted to be non-blank, and every
 * state's DOM is asserted to contain what the screenshot is supposed to show. A blank,
 * white, transparent or error-page screenshot fails the script — it does not pass quietly.
 *
 * WHAT THIS DOES NOT PROVE (say it out loud, or the screenshots will be read as more than
 * they are)
 *   * The Tauri shell. No window was opened and no webview was launched: this is Chromium
 *     loading the same built bundle the webview loads. Window chrome, the OS title bar,
 *     `tauri.conf.json` sizing (1280x800 is imposed here), DPI scaling and the real
 *     WebKit/WebView2 engines are all unverified.
 *   * The asset protocol. The `<video>` and the thumbnails are served by a throwaway static
 *     server in this file from files it generates with ffmpeg; the real application serves
 *     them through Tauri's `asset:` protocol against a scoped path. Playback and range
 *     requests through *that* protocol are not demonstrated here.
 *   * Any Rust behaviour. Every command returns a fixture. A trim here is a mock mutating
 *     an array, not an ffmpeg stream copy.
 *
 * Pointer dragging IS exercised (`05-trim-partial`): Playwright drives real mouse input, so
 * the handle geometry and the pointer→millisecond mapping are genuinely checked. Keyboard
 * nudging of a focused handle is not.
 */

import { spawnSync } from 'node:child_process';
import { createServer } from 'node:http';
import { existsSync, mkdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import { extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { pngStats } from './png-stats.mjs';

const appDir = resolve(fileURLToPath(new URL('..', import.meta.url))); // apps/desktop
const repoDir = resolve(appDir, '..', '..'); // the repository root
const shotsDir = join(repoDir, 'target', 'desktop-shots');
const mediaDir = join(shotsDir, 'media');
const browsersDir = join(shotsDir, 'browsers');
const distDir = join(appDir, 'dist');

/** The window the screenshots are taken at. Tauri's configured default, 1x scale. */
const VIEWPORT = { width: 1280, height: 800 };

/** Floors a real render clears by an order of magnitude; see `png-stats.mjs`. A blank pane
 * has one distinct colour, and the emptiest state this script photographs has ~550. */
const MIN_UNIQUE_COLORS = 200;
const MIN_BYTES = 8_000;
const MAX_MODAL_SHARE = 0.97;
const MAX_MEAN_LUMINANCE = 0.5;

const argv = new Set(process.argv.slice(2));
const installBrowserOnly = argv.has('--install-browser-only');
/** Set once `buildMedia` has run, because a missing ffmpeg makes the player checks advisory. */
let mediaAvailable = false;
// The browser cache lives under `target/` — gitignored, and inside the workspace, which is
// the only place this script is allowed to write.
process.env.PLAYWRIGHT_BROWSERS_PATH ||= browsersDir;

// ---------------------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------------------

const failures = [];
let checks = 0;

function log(line = '') {
  process.stdout.write(`${line}\n`);
}

function check(state, ok, description) {
  checks += 1;
  if (!ok) failures.push(`${state}: ${description}`);
  log(`    ${ok ? '✓' : '✗ FAIL'} ${description}`);
}

function report(state, description) {
  log(`    · ${description}`);
}

/**
 * Errors this script expects, each with the reason it cannot be avoided.
 *
 * A real trim runs ffmpeg and writes a file; the mock only mutates an array, so the clip it
 * returns names a file nothing wrote. The player for that clip is therefore black and its
 * thumbnail is missing — a gap in the *mock*, not a defect in the window. Everything else
 * is a failure.
 */
const EXPECTED_ERRORS = [
  {
    pattern: /reading 'invoke'/,
    why: 'state 00 has no Tauri runtime and no test seam, so the first IPC call throws by design',
  },
  {
    pattern: /\.trim-\d+-\d+\.[a-z0-9]+/i,
    why: 'the trimmed clip names the file a real ffmpeg stream copy would have written; the mock does not run ffmpeg, so that clip plays black',
  },
];

/** Split what the page reported into "explained" and "must not happen". */
function partitionErrors(errors) {
  const explained = [];
  const surprises = [];
  for (const error of errors) {
    const match = EXPECTED_ERRORS.find((expected) => expected.pattern.test(error));
    if (match) explained.push({ error, why: match.why });
    else surprises.push(error);
  }
  return { explained, surprises };
}

// ---------------------------------------------------------------------------------------
// fixtures — the mock ClipSource's data
// ---------------------------------------------------------------------------------------

const CLIPS_DIR = 'C:\\Users\\player\\Videos\\replayed\\clips';
const THUMBS_DIR = `${CLIPS_DIR}\\thumbs`;
/** Fixed so two runs produce the same dates; the panel renders these in local time. */
const NEWEST_MS = Date.parse('2026-09-23T19:24:00Z');
const MINUTE = 60_000;
const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

const CODECS = { h264: 'h264', h265: 'h265', vp8: 'vp8', vp9: 'vp9' };

/** One `ClipDto`, with the fields the fixtures do not care about filled in plausibly. */
function clip(id, file, durationMs, sizeBytes, codec, favourite, agoMs) {
  return {
    id,
    path: `${CLIPS_DIR}\\${file}`,
    started_at_ms: 12_000 + (id % 7) * 1_500,
    duration_ms: durationMs,
    size_bytes: sizeBytes,
    codec: CODECS[codec] ?? codec,
    favourite,
    created_at_ms: NEWEST_MS - agoMs,
    title: null,
    game: null,
    tags: [],
  };
}

/** The clip whose media this script actually generates; also the newest, so it is selected. */
const FEATURE_ID = 42;
const FEATURE_DURATION_MS = 12_960;

const listClips = [
  clip(FEATURE_ID, 'clip-2026-09-23_19-22-04_ace_clutch.webm', FEATURE_DURATION_MS, 1_782_336, 'vp8', true, 2 * MINUTE),
  clip(41, 'clip-2026-09-23_19-14-37_round_23_defuse.mp4', 42_300, 148_912_128, 'h264', false, 9 * MINUTE),
  clip(40, 'clip-2026-09-23_19-02-16_full_match_overtime.mp4', 4_265_000, 3_113_002_188, 'h265', false, 22 * MINUTE),
  clip(39, 'clip-2026-09-23_18-58-49_pistol_round.mp4', 12_960, 31_457_280, 'vp9', false, 25 * MINUTE),
  clip(38, 'clip-2026-09-22_23-41-02_bug_repro_scoped_shot_with_a_name_that_has_to_ellipsise_somewhere.mp4', 30_000, 87_000_000, 'h265', true, 16 * 60 * MINUTE),
  clip(37, 'clip-2026-09-22_22-07-33_last_second_whiff.mp4', 900, 780_000, 'h264', false, 17 * 60 * MINUTE),
];

/** Sum of the sizes above — computed so the panel's arithmetic and its rows agree. */
function totalBytes(clips) {
  return clips.reduce((sum, c) => sum + c.size_bytes, 0);
}

function statsFor(clips, overrides = {}) {
  const favourites = clips.filter((c) => c.favourite);
  return {
    clip_count: clips.length,
    total_bytes: totalBytes(clips),
    // Every fixture's clips are on disk; the one state that is about rows without files
    // overrides both counts.
    missing_count: 0,
    missing_sessions: 0,
    favourite_count: favourites.length,
    favourite_bytes: totalBytes(favourites),
    cap_bytes: 50 * GIB,
    max_age_days: 14,
    cap_met: true,
    over_cap_by_bytes: 0,
    planned_deletions: 0,
    bytes_after: totalBytes(clips),
    clips_dir: CLIPS_DIR,
    warnings: [],
    ...overrides,
  };
}

/**
 * The shell's background half, as `app_status` returns it: the clip hotkey, the file it was
 * read from, and the sentence that explains closing the window.
 *
 * The chord, the close hint and the failure text are the *Rust* side's own strings (see
 * `src-tauri/src/background.rs` and `crates/events/src/hotkey.rs`), which is exactly why
 * photographing them is worth doing: this is the text a user reads before pressing a key or
 * clicking the X of a window that does not quit when it closes.
 */
const APP_STATUS = {
  hotkey: { chord: 'Ctrl+F8', installed: true, error: null },
  config_path: 'C:\\Users\\player\\AppData\\Local\\replayed\\config.toml',
  config_exists: true,
  close_hint:
    'Closing this window hides replayed in the tray; recording keeps running. Quit from the ' +
    'tray menu when you are done.',
};

/**
 * The same, on a machine where the chord is already taken — the failure this feature exists
 * to make visible rather than silent. `config_exists: false` as well, so one screenshot
 * carries both facts: a dead hotkey, and no `config.toml` to change it in.
 */
const APP_STATUS_NO_HOTKEY = {
  ...APP_STATUS,
  hotkey: {
    chord: 'Ctrl+F8',
    installed: false,
    error:
      'the chord Ctrl+F8 could not be registered (RegisterHotKey failed: the hot key is ' +
      'already registered). Another application — or another replayed (the CLI and this app ' +
      'cannot both hold one chord) — already owns it. Nothing will happen when Ctrl+F8 is ' +
      'pressed. Close the other application, or set a different [hotkeys] clip in config.toml ' +
      'and restart.',
  },
  config_exists: false,
};

const EMPTY_FIXTURE = {
  clips: [],
  appStatus: APP_STATUS,
  clipsDir: CLIPS_DIR,
  thumbsDir: THUMBS_DIR,
  thumbnails: 'ok',
  nextTrimId: 43,
  trimmedAtMs: NEWEST_MS,
  stats: statsFor([]),
};

const LIST_FIXTURE = {
  clips: listClips,
  appStatus: APP_STATUS,
  clipsDir: CLIPS_DIR,
  thumbsDir: THUMBS_DIR,
  thumbnails: 'ok',
  nextTrimId: 43,
  trimmedAtMs: NEWEST_MS,
  stats: statsFor(listClips),
};

// The same library, but two favourite full-match recordings have pushed the favourites
// alone past the cap — the verdict that says no cleanup pass can help (§8.1).
const TWO_MATCHES = [
  // The newest clip stays the one this script has real media for, so the player in this
  // state is a picture rather than a 404.
  { ...listClips[0], id: 61, created_at_ms: NEWEST_MS },
  clip(60, 'clip-2026-09-23_21-10-55_ranked_match_one.mp4', 3_600_000, 26_500_000_000, 'h265', true, 2 * MINUTE),
  clip(59, 'clip-2026-09-23_20-04-12_ranked_match_two.mp4', 3_420_000, 28_500_000_000, 'h265', true, 6 * MINUTE),
  ...listClips.slice(1).map((c, i) => ({ ...c, id: 58 - i })),
];
const FAVOURITES_OVER_CAP_FIXTURE = {
  clips: TWO_MATCHES,
  clipsDir: CLIPS_DIR,
  thumbsDir: THUMBS_DIR,
  thumbnails: 'ok',
  nextTrimId: 90,
  trimmedAtMs: NEWEST_MS,
  stats: {
    ...statsFor(TWO_MATCHES),
    cap_met: false,
    // Derived, not invented: the verdict's "exceed it by …" has to agree with the
    // "favourite bytes" line above it, or the panel contradicts itself on screen.
    over_cap_by_bytes: totalBytes(TWO_MATCHES.filter((c) => c.favourite)) - 50 * GIB,
  },
};

// A state the index can be left in: it still held rows whose
// files a cleanup had deleted. The clips list filtered them out and the storage panel did
// not, so the panel read "Indexed clips 12 (0 favourite), In use 1.2 GiB" directly above
// "No clips are indexed yet". Both count the clips that are on disk now, and the rows they
// leave out are named rather than dropped.
const MISSING_FILES_FIXTURE = {
  clips: listClips.slice(0, 2),
  clipsDir: CLIPS_DIR,
  thumbsDir: THUMBS_DIR,
  thumbnails: 'ok',
  nextTrimId: 43,
  trimmedAtMs: NEWEST_MS,
  stats: statsFor(listClips.slice(0, 2), { missing_count: 9 }),
};

/**
 * A running recorder's status, exactly as `recording_status` publishes it.
 *
 * The numbers are the shape `replayed-recorder` gives (see its `RecorderStatus`): 12
 * completed one-second segments, 480 frames submitted, a rate measured over the last
 * second, and the 320 frames the pacer skipped *without* a GPU readback. `drift_ms` is
 * positive because media time lags real time on capture hardware — the reason the trigger
 * is taken from media time at all.
 */
const RECORDING_STATUS = {
  running: true,
  frames: 480,
  segments: 12,
  bytes: 41_943_040,
  span_ms: 12_000,
  dropped: 0,
  dropped_audio: 0,
  skipped: 320,
  fps: 29.83,
  configured_fps: 30,
  effective_fps: 30,
  drift_ms: 1_258,
  clips: 1,
  error: null,
};

/**
 * The clip `clip_now` returns: the engine's own metadata plus the row it was indexed as.
 *
 * The path is Windows-shaped because that is the machine this application ships to; the id
 * is 43, which is in the thumbnail set this script generates, so the row the list gains has
 * a real picture in it.
 */
const NEW_CLIP = {
  id: 43,
  path: `${CLIPS_DIR}\\clip-2026-09-23_19-31-02_saved_from_the_window.mp4`,
  duration_ms: 12_021,
  size_bytes: 26_624,
  codec: 'h264_nvenc',
  started_at_ms: 42_000,
};

/** The library, with a recorder running and a clip ready to be taken. */
const RECORDING_FIXTURE = {
  ...LIST_FIXTURE,
  recording: RECORDING_STATUS,
  clipNow: NEW_CLIP,
};

/** The same library, but the clip hotkey could not be registered. */
const NO_HOTKEY_FIXTURE = {
  ...LIST_FIXTURE,
  appStatus: APP_STATUS_NO_HOTKEY,
};

/** No ffmpeg: the placeholder thumbnails, the error banner and `warnings[]` all at once. */
const NO_FFMPEG_FIXTURE = {
  ...LIST_FIXTURE,
  thumbnails: 'placeholder',
  stats: {
    ...statsFor(listClips),
    planned_deletions: 2,
    bytes_after: totalBytes(listClips) - 300_000_000,
    warnings: ['ffmpeg was not found'],
  },
  warning: 'ffmpeg was not found',
};

/**
 * Two sessions for the review UI: a finished full-session recording with markers, and a
 * buffer-mode session that is still running — the row that reads "length unknown".
 *
 * The clips are the first two of the list fixture, so their thumbnails are generated media
 * rather than 404s, and the bookmark points at the clip the hotkey press wrote.
 */
const SESSION_SESSIONS = [
  {
    id: 7,
    game: 'Dota 2',
    mode: 'session',
    started_at_ms: NEWEST_MS - 125 * MINUTE,
    ended_at_ms: NEWEST_MS - 63 * MINUTE,
    final_path: 'C:\\Users\\player\\AppData\\Local\\replayed\\sessions-out\\session-7.mp4',
    size_bytes: 2_254_000_000,
    favourite: false,
    scratch_dir: 'C:\\Users\\player\\AppData\\Local\\replayed\\sessions\\session-7',
    duration_ms: 3_723_000,
  },
  {
    id: 8,
    game: null,
    mode: 'buffer',
    started_at_ms: NEWEST_MS - 20 * MINUTE,
    ended_at_ms: null,
    final_path: null,
    size_bytes: 41_943_040,
    favourite: false,
    scratch_dir: 'C:\\Users\\player\\AppData\\Local\\replayed\\sessions\\session-8',
    duration_ms: 0,
  },
];
const SESSION_EVENTS = {
  7: [
    { id: 11, kind: 'round_start', offset_ms: 0, payload: null, clip_id: null },
    { id: 12, kind: 'death', offset_ms: 184_000, payload: '{"mentees":["axe"]}', clip_id: null },
    { id: 13, kind: 'bookmark', offset_ms: 402_000, payload: null, clip_id: 42 },
  ],
};

/** The library as the settings states see it: clips, plus armed-but-idle watching. */
const SETTINGS_FIXTURE = {
  ...LIST_FIXTURE,
  recording: {
    running: false,
    frames: 0,
    segments: 0,
    bytes: 0,
    span_ms: 0,
    dropped: 0,
    dropped_audio: 0,
    skipped: 0,
    fps: 0,
    configured_fps: 0,
    effective_fps: 0,
    drift_ms: 0,
    clips: 0,
    error: null,
    watching_games: false,
    matched_game: null,
    recorder_mode: 'buffer',
    mic_enabled: false,
  },
  settings: {
    fps: 30,
    output_size: '',
    mic_enabled: false,
    save_session: false,
    auto_record: false,
    clip_hotkey: 'Ctrl+F8',
    start_with_system: false,
    watch_games: [
      { name: 'League of Legends', exe: 'League of Legends.exe', signal: 'live_client_api' },
      { name: 'Counter-Strike 2', exe: 'cs2.exe', signal: 'process' },
      { name: 'Dota 2', exe: 'dota2.exe', signal: 'process' },
    ],
    defaulted: ['encode.output_size'],
    config_exists: true,
  },
};
/** The library as the session-review states see it: two clips, two sessions. */
const SESSION_FIXTURE = {
  clips: listClips.slice(0, 2),
  sessions: SESSION_SESSIONS,
  sessionEvents: SESSION_EVENTS,
  appStatus: APP_STATUS,
  clipsDir: CLIPS_DIR,
  thumbsDir: THUMBS_DIR,
  thumbnails: 'ok',
  nextTrimId: 43,
  trimmedAtMs: NEWEST_MS,
  stats: statsFor(listClips.slice(0, 2)),
};

/**
 * A library after months of play: thirty clips and ten sessions. The sidebar has to stay
 * usable — both lists scroll inside their own space, and the storage panel stays reachable
 * instead of being pushed below the fold.
 */
const LONG_CLIPS = Array.from({ length: 30 }, (_, i) => {
  const id = 200 + i;
  const long = i === 7;
  // The newest row is selected on load, so its file has to be one the mock's static
  // server can actually play — otherwise the player 404s and the state fails on noise.
  const file =
    i === 0
      ? 'clip-2026-09-23_19-22-04_ace_clutch.webm'
      : long
        ? 'clip-2026-08-02_17-11-53_ranked_overtime_loss_with_a_name_that_keeps_going_and_going.mp4'
        : `clip-2026-09-${String(23 - (i % 9)).padStart(2, '0')}_19-${String(i % 60).padStart(2, '0')}-00_play_${i}.mp4`;
  return clip(
    id,
    file,
    5_000 + i * 1_000,
    50_000_000 + i * 10_000_000,
    ['h264', 'h265', 'vp8', 'vp9'][i % 4],
    i % 5 === 0,
    i * 37 * MINUTE,
  );
});
const LONG_SESSIONS = Array.from({ length: 10 }, (_, i) => {
  const running = i === 9;
  return {
    id: 300 + i,
    game: i % 3 === 0 ? null : ['Dota 2', 'Counter-Strike 2'][i % 2],
    mode: i % 4 === 0 ? 'buffer' : 'session',
    started_at_ms: NEWEST_MS - (i + 1) * 5 * 60 * MINUTE,
    ended_at_ms: running ? null : NEWEST_MS - i * 5 * 60 * MINUTE,
    final_path: running || i % 4 === 0 ? null : `C:\\Users\\player\\AppData\\Local\\replayed\\sessions-out\\session-${300 + i}.mp4`,
    size_bytes: 500_000_000 + i * 100_000_000,
    favourite: i === 2,
    scratch_dir: `C:\\Users\\player\\AppData\\Local\\replayed\\sessions\\session-${300 + i}`,
    duration_ms: running ? 0 : 3_000_000 + i * 60_000,
  };
});
const LONG_FIXTURE = {
  clips: LONG_CLIPS,
  sessions: LONG_SESSIONS,
  sessionEvents: {},
  appStatus: APP_STATUS,
  clipsDir: CLIPS_DIR,
  thumbsDir: THUMBS_DIR,
  thumbnails: 'ok',
  nextTrimId: 400,
  trimmedAtMs: NEWEST_MS,
  stats: statsFor(LONG_CLIPS),
};

/**
 * The mock: an implementation of `ClipSource` that lives entirely in the page.
 *
 * It is serialised into the browser by `page.addInitScript`, so it may not close over
 * anything here — the fixture argument is all it gets. The mutation methods are real
 * mutations of the in-page array, so the UI's refresh-after-mutate path is exercised too.
 */
function installMockSource(fixture) {
  const clips = fixture.clips.map((c) => ({ ...c }));
  const sessions = (fixture.sessions ?? []).map((s) => ({ ...s }));
  const eventsBySession = fixture.sessionEvents ?? {};
  // The recorder's status is state, not a constant: `save_clip` bumps the clip counter and
  // a stop flips `running`, exactly as the engine's own counters would.
  let recording = {
    running: false,
    frames: 0,
    segments: 0,
    bytes: 0,
    span_ms: 0,
    dropped: 0,
    dropped_audio: 0,
    skipped: 0,
    fps: 0,
    configured_fps: 0,
    effective_fps: 0,
    drift_ms: 0,
    clips: 0,
    error: null,
    watching_games: false,
    matched_game: null,
    recorder_mode: 'buffer',
    mic_enabled: false,
    ...(fixture.recording ?? {}),
  };
  // The effective settings, mutated by `updateSettings` the way the file would be.
  // (The install scope forbids closing over module constants, so the default clips
  // directory comes from the fixture's own stats, which every fixture carries.)
  const defaultClipsDir = (fixture.stats ?? {}).clips_dir ?? '';
  // The encoder settings, mutated by `updateEncoderSettings` the same way. The values are
  // the shipped defaults, so the settings view photographs a real configuration rather than
  // a row of blanks.
  let encoderSettings = {
    vendor: 'auto',
    codec: 'h264',
    bitrate_kbps: 20000,
    fps: 60,
    output_size: '',
    adapt_fps: true,
    zero_copy: null,
    rate_control: 'cbr',
    preset: '',
    audio_bitrate_kbps: 192,
    ...(fixture.encoderSettings ?? {}),
  };
  // What the backend would use: NVENC, chosen because it is the fastest encoder this
  // machine has. The capabilities are the chosen encoder's own, which is what the panel
  // filters its choices by.
  const encoderStatus = {
    requested_vendor: 'auto',
    selected_vendor: 'nvenc',
    selected_codec: 'h264',
    capabilities: {
      rate_control_modes: ['cbr', 'vbr', 'quality'],
      presets: ['p1', 'p2', 'p3', 'p4', 'p5', 'p6', 'p7'],
      supports_scaling: true,
      max_bitrate_kbps: 100000,
    },
    message: 'NVENC was chosen: it is the fastest encoder this machine has.',
    ...(fixture.encoderStatus ?? {}),
  };
  const settings = {
    clips_dir: '',
    clips_dir_resolved: defaultClipsDir,
    fps: 60,
    output_size: '',
    mic_enabled: false,
    auto_record: false,
    watch_games: [
      { name: 'League of Legends', exe: 'League of Legends.exe', signal: 'live_client_api' },
      { name: 'Counter-Strike 2', exe: 'cs2.exe', signal: 'process' },
      { name: 'Dota 2', exe: 'dota2.exe', signal: 'process' },
    ],
    defaulted: [],
    config_exists: true,
    config_path: 'C:\\Users\\player\\replayed\\config.toml',
    ...(fixture.settings ?? {}),
  };
  const find = (id) => clips.find((c) => c.id === id);
  const notFound = (id) => ({ code: 'clip_not_found', message: `clip #${id} is not in the index` });
  const basename = (p) => String(p).split(/[\\/]/).pop() ?? '';

  globalThis.__replayedClipSource = {
    async listClips() {
      return clips.map((c) => ({ ...c }));
    },
    async storageStats() {
      return { ...fixture.stats, warnings: [...fixture.stats.warnings] };
    },
    async setFavourite(id, favourite) {
      const target = find(id);
      if (!target) throw notFound(id);
      target.favourite = favourite;
      return { ...target };
    },
    async setClipTitle(id, title) {
      const target = find(id);
      if (!target) throw notFound(id);
      // A blank title clears it, exactly as the store does.
      const trimmed = String(title ?? '').trim();
      target.title = trimmed === '' ? null : trimmed;
      return { ...target };
    },
    async setClipTags(id, tags) {
      const target = find(id);
      if (!target) throw notFound(id);
      target.tags = [...tags];
      return { ...target };
    },
    async copyClipToClipboard(id) {
      // A fixture cannot put a file on the system clipboard, and must not pretend to: the
      // window shows nothing of its own afterwards, so an empty promise is the honest answer.
      const target = find(id);
      if (!target) throw notFound(id);
    },
    async revealClip(id) {
      // Likewise: there is no file manager behind this harness, and opening one would
      // take the screenshot run outside the browser.
      const target = find(id);
      if (!target) throw notFound(id);
    },
    async dragClipOut(id) {
      // A native drag needs a real window and a real file; the harness has neither.
      const target = find(id);
      if (!target) throw notFound(id);
    },
    async trimClip(id, startMs, endMs) {
      const source = find(id);
      if (!source) throw notFound(id);
      if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) {
        throw { code: 'invalid_range', message: 'The selection is empty.' };
      }
      const asked = endMs - startMs;
      // A stream copy cuts where the stream allows: the file comes out slightly short of
      // what was asked for, which is what the notice text has to report.
      const written = {
        ...source,
        id: fixture.nextTrimId,
        path: `${fixture.clipsDir}\\${basename(source.path).replace(/\.[^.]+$/, '')}.trim-${startMs}-${endMs}.webm`,
        duration_ms: asked - 240,
        size_bytes: Math.round((source.size_bytes * asked) / Math.max(1, source.duration_ms)),
        favourite: false,
        created_at_ms: fixture.trimmedAtMs,
      };
      fixture.nextTrimId += 1;
      clips.unshift(written);
      return { ...written };
    },
    async thumbnail(id, atMs) {
      if (fixture.thumbnails === 'placeholder') {
        throw { code: 'ffmpeg_unavailable', message: fixture.warning };
      }
      return {
        clip_id: id,
        // A Windows path, as the Rust side would return, run through `assetUrl` below.
        path: `${fixture.thumbsDir}\\thumb-${id}.png`,
        at_ms: atMs,
        cached: true,
      };
    },
    async deleteClip(id) {
      const index = clips.findIndex((c) => c.id === id);
      const [removed] = index >= 0 ? clips.splice(index, 1) : [];
      return {
        id,
        row_deleted: index >= 0,
        file_removed: index >= 0,
        orphaned_path: null,
        already_missing: false,
        bytes_reclaimed: removed ? removed.size_bytes : 0,
        thumbnails_removed: removed ? 1 : 0,
      };
    },
    // Combining and the export presets: the harness has no ffmpeg, so a combine answers the
    // clip it would have produced (an in-memory row over the two source files) and an export
    // answers a path that does not exist yet. Both are the shapes the window reads, which is
    // what these screenshots are for; the media work is `crates/media`'s tests.
    async combineClips(ids) {
      const sources = ids.map((id) => find(id)).filter(Boolean);
      if (sources.length < 2) {
        throw { code: 'invalid_input', message: 'combining needs at least two different clips' };
      }
      const written = {
        ...sources[0],
        id: fixture.nextTrimId,
        path: `${fixture.clipsDir}\\combine-${fixture.trimmedAtMs}.mp4`,
        duration_ms: sources.reduce((sum, clip) => sum + clip.duration_ms, 0),
        size_bytes: sources.reduce((sum, clip) => sum + clip.size_bytes, 0),
        favourite: false,
        title: null,
        created_at_ms: fixture.trimmedAtMs,
      };
      fixture.nextTrimId += 1;
      clips.unshift(written);
      return { ...written };
    },
    async exportClip(id, preset) {
      if (!['discord', 'vertical', 'gif'].includes(preset)) {
        throw { code: 'invalid_input', message: `${preset} is not an export preset` };
      }
      const source = find(id);
      if (!source) throw notFound(id);
      const ext = preset === 'gif' ? 'gif' : 'mp4';
      const stem = basename(source.path).replace(/\.[^.]+$/, '');
      return {
        preset,
        path: `${fixture.clipsDir}\\${stem}-${preset}.${ext}`,
        size_bytes: Math.round(source.size_bytes / 3),
        duration_ms: source.duration_ms,
      };
    },
    async pruneMissing() {
      // The fixture's phantom rows are exactly the ones the storage panel was told about:
      // they are not in `clips`, which is the whole point of the `missingfiles` scenario (the
      // two views count the same set). Removing them leaves every on-disk row alone.
      const stats = fixture.stats ?? {};
      return {
        clips_removed: stats.missing_count ?? 0,
        sessions_removed: stats.missing_sessions ?? 0,
        thumbnails_removed: 0,
        sessions_refused: 0,
        rows_already_gone: 0,
        failed: 0,
      };
    },
    // Sessions: the review UI the `d356b9b` feature added after this mock was written —
    // without these, `refresh()` rejects and every state photographs an error banner.
    async listSessions() {
      return sessions.map((s) => ({ ...s }));
    },
    async sessionDetail(id) {
      const session = sessions.find((s) => s.id === id);
      if (!session) throw { code: 'session_not_found', message: `no session with id ${id} is in the index` };
      return { ...session };
    },
    async sessionEvents(id) {
      return [...(eventsBySession[id] ?? [])];
    },
    // No fixture game splits into matches; an empty answer is what an untagged session returns.
    async sessionMatches() {
      return [];
    },
    async extractMatch(sessionId, matchIndex) {
      // The fixture has no matches — `sessionMatches` above returns an empty list — so
      // there is nothing to cut, and the backend's answer is the honest one.
      throw { code: 'match_not_found', message: `session ${sessionId} has no match ${matchIndex}` };
    },
    async setSessionFavourite(id, favourite) {
      const session = sessions.find((s) => s.id === id);
      if (!session) throw { code: 'session_not_found', message: `no session with id ${id} is in the index` };
      session.favourite = favourite;
      return { ...session };
    },
    async deleteSession(id) {
      const index = sessions.findIndex((s) => s.id === id);
      const [removed] = index >= 0 ? sessions.splice(index, 1) : [];
      return {
        id,
        row_deleted: index >= 0,
        scratch_removed: index >= 0,
        final_file_removed: index >= 0 && removed.final_path !== null,
        bytes_reclaimed: removed ? removed.size_bytes : 0,
        orphaned: [],
      };
    },
    async extractClip(sessionId, startMs, endMs) {
      const session = sessions.find((s) => s.id === sessionId);
      if (!session) throw { code: 'session_not_found', message: `no session with id ${sessionId} is in the index` };
      if (session.ended_at_ms === null || session.final_path === null) {
        throw { code: 'invalid_input', message: `session #${sessionId} has no finished file to cut from` };
      }
      if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) {
        throw { code: 'invalid_range', message: 'The selection is empty.' };
      }
      const written = {
        id: fixture.nextTrimId,
        path: `${fixture.clipsDir}\\${basename(session.final_path).replace(/\.[^.]+$/, '')}.extract-${startMs}-${endMs}.mp4`,
        started_at_ms: startMs,
        duration_ms: endMs - startMs,
        size_bytes: 12_000_000,
        codec: 'h264',
        favourite: false,
        created_at_ms: fixture.trimmedAtMs,
      };
      fixture.nextTrimId += 1;
      clips.unshift(written);
      return { ...written };
    },
    async health() {
      return {
        recorder_state: recording.running ? 'recording' : 'idle',
        recorder_error: null,
        encoder: 'h264_nvenc',
        ffmpeg_available: true,
        disk: { path: fixture.clipsDir, free_bytes: 200 * 1024 ** 3, total_bytes: 931 * 1024 ** 3 },
        event_sources: [],
        recent_errors: [],
        log_path: 'C:\\Users\\player\\AppData\\Local\\replayed\\logs\\replayed.log',
      };
    },
    async gsiTargets() {
      return [
        { game: 'cs2', label: 'Counter-Strike 2', install_dir: null, installed: false },
        { game: 'dota2', label: 'Dota 2', install_dir: null, installed: false },
      ];
    },
    async installGsiCfg(game) {
      return { game, path: 'unused in the harness', replaced: false };
    },
    async logFromFrontend() {
      return;
    },
    async startRecording() {
      // Nothing here starts a capture — a fixture cannot, and must not: the real
      // `start_recording` opens a session on the display. The state it would move to is
      // what this returns.
      recording = fixture.recordingAfterStart ?? fixture.recording ?? { ...recording, running: true };
      return { ...recording };
    },
    async stopRecording() {
      recording = { ...recording, running: false };
      return { ...recording };
    },
    async recordingStatus() {
      return { ...recording };
    },
    // Read once, when the window opens: the chord is registered at startup and the config
    // path does not move while the process runs.
    async appStatus() {
      return { ...fixture.appStatus, hotkey: { ...fixture.appStatus.hotkey } };
    },
    async listAudioDevices() {
      const long = 'Speakers (Realtek High Definition Audio(R) with a very long vendor name)';
      return {
        inputs: [{ id: 'mic-1', name: 'Microphone (USB Audio Device with an equally long name)', is_default: true }],
        outputs: [{ id: 'out-1', name: long, is_default: true }],
      };
    },
    async getSettings() {
      return {
        ...settings,
        watch_games: settings.watch_games.map((game) => ({ ...game })),
        defaulted: [...settings.defaulted],
      };
    },
    async updateSettings(update) {
      // Mirror the backend's contract: validation first (a rejection writes nothing),
      // then the merge, then the restart verdict for a running recorder.
      if (update.fps !== undefined && !(Number.isInteger(update.fps) && update.fps >= 1 && update.fps <= 240)) {
        throw { code: 'invalid_input', message: `encode.fps is ${update.fps}, but the capture rate must be between 1 and 240` };
      }
      const before = { ...settings };
      const applied = [];
      for (const [field, key] of [['clips_dir', 'storage.clips_dir'], ['fps', 'encode.fps'], ['output_size', 'encode.output_size'], ['mic_enabled', 'mic.enabled'], ['save_session', 'recorder.save_session'], ['auto_record', 'games.auto_record'], ['start_with_system', 'app.start_with_system'], ['clip_hotkey', 'hotkeys.clip'], ['watch_games', 'games.watch']]) {
        if (update[field] !== undefined) {
          settings[field] = update[field];
          applied.push(key);
        }
      }
      if (settings.clips_dir === '') {
        settings.clips_dir_resolved = defaultClipsDir;
      } else {
        settings.clips_dir_resolved = settings.clips_dir;
      }
      const engineChanged = [];
      if (update.fps !== undefined && settings.fps !== before.fps) engineChanged.push('encode.fps');
      if (update.output_size !== undefined && settings.output_size !== before.output_size) {
        engineChanged.push('encode.output_size');
      }
      if (update.mic_enabled !== undefined && settings.mic_enabled !== before.mic_enabled) {
        engineChanged.push('mic.enabled');
      }
      if (update.auto_record !== undefined && settings.auto_record !== before.auto_record) {
        engineChanged.push('games.auto_record');
      }
      if (update.save_session !== undefined && settings.save_session !== before.save_session) {
        engineChanged.push('recorder.save_session');
      }
      if (update.watch_games !== undefined) {
        // Compared by value, as the backend does: the panel sends the list only when it differs.
        const a = JSON.stringify(update.watch_games);
        const b = JSON.stringify(before.watch_games);
        if (a !== b) engineChanged.push('games.watch');
      }
      const restartRequired = recording.running && engineChanged.length > 0;
      // The idle mock has no engine restart to wait for: the next start would arm what
      // was saved, so mirror it straight away and the recorder line stays truthful.
      recording.watching_games = settings.auto_record;
      return {
        settings: await this.getSettings(),
        applied,
        restart_required: restartRequired,
        restart_reason: restartRequired
          ? `a recording is running: ${engineChanged.join(', ')} takes effect when the next recording starts`
          : null,
        // The harness is not Windows either: the file is saved and the registry is untouched,
        // which is exactly the sentence the real backend produces off Windows.
        hotkey:
          update.clip_hotkey === undefined
            ? null
            : `the clip hotkey is saved as ${update.clip_hotkey}; Ctrl+F8 stays in force until replayed restarts`,
        autostart:
          update.start_with_system === undefined
            ? null
            : {
                applied: false,
                detail: `[app] start_with_system is a Windows feature; this build targets ${process.platform} and has nothing to register`,
              },
      };
    },
    async screenshotNow() {
      const written = {
        ...clips[0],
        id: fixture.nextTrimId,
        kind: 'screenshot',
        path: `${fixture.clipsDir}\\screenshot-${fixture.trimmedAtMs}.png`,
        duration_ms: 0,
        codec: 'png',
        size_bytes: 240_000,
        favourite: false,
        title: null,
        created_at_ms: fixture.trimmedAtMs,
      };
      fixture.nextTrimId += 1;
      clips.unshift(written);
      return { ...written };
    },
    async openDataDir() {
      // No file manager behind the harness; the command's real job is to spawn one.
      return undefined;
    },
    async chooseClipsDir() {
      // The backend runs the native picker; there is no OS dialog behind this harness, so
      // the pick is dismissed. The panel treats that as "no directory" and leaves its
      // draft alone, which is what the panel's own contract says a cancelled picker does.
      throw { code: 'cancelled', message: 'the folder picker was dismissed' };
    },
    // The encoder section's three commands. The settings are state, like the panel's
    // other settings: `updateEncoderSettings` writes them the way the backend does.
    async encoderSettings() {
      return { ...encoderSettings };
    },
    async encoderStatus() {
      return { ...encoderStatus, capabilities: { ...encoderStatus.capabilities } };
    },
    async updateEncoderSettings(settings) {
      encoderSettings = { ...encoderSettings, ...settings };
      return { ...encoderSettings };
    },
    // The displays this machine has, and the one the capture is pointed at. The fixture
    // machine has a single 60 Hz screen, so the framerate picker has one display's rate to
    // be made against.
    async captureDisplays() {
      return {
        displays: [{ index: 0, width: 1920, height: 1080, refresh_hz: 60 }],
        selected: 0,
      };
    },
    // The diagnostics export. A harness cannot put anything on the system clipboard, but
    // it can answer with the report the backend would have assembled — which is what the
    // panel shows the user.
    async copyDiagnostics() {
      return {
        version: '0.2.0',
        platform: 'windows 11 x86_64',
        generated_at: '2026-09-23T19:24:00Z',
        config: '[encode]\nvendor = "auto"\ncodec = "h264"\nbitrate_kbps = 20000\nfps = 60',
        problems: ['ffmpeg was not found on PATH'],
        log_tail: 'INFO the recorder started\nWARN the encoder was asked for 60 fps and can hold 45',
      };
    },
    async clipNow() {
      const written = { ...fixture.clipNow };
      // The engine writes the file and indexes it, so the list gains a row — which is what
      // makes the window's refresh-after-mutate path visible in the screenshot.
      clips.unshift({
        id: written.id,
        path: written.path,
        started_at_ms: written.started_at_ms,
        duration_ms: written.duration_ms,
        size_bytes: written.size_bytes,
        codec: written.codec,
        favourite: false,
        created_at_ms: fixture.trimmedAtMs,
        title: null,
        game: null,
        tags: [],
      });
      recording = { ...recording, clips: recording.clips + 1 };
      return { ...written };
    },
    // Where Tauri would hand back an `asset:` URL, this hands back the static server's.
    assetUrl: (path) => `/media/${basename(path)}`,
  };
}

// ---------------------------------------------------------------------------------------
// ffmpeg fixtures — a real video, so the player and the thumbnails are real pictures
// ---------------------------------------------------------------------------------------

function hasFfmpeg() {
  return spawnSync('ffmpeg', ['-version'], { stdio: 'ignore' }).status === 0;
}

function ffmpeg(args) {
  const run = spawnSync('ffmpeg', ['-hide_banner', '-v', 'error', '-y', ...args], {
    stdio: 'inherit',
  });
  if (run.status !== 0) throw new Error(`ffmpeg ${args.join(' ')} failed`);
}

const FEATURE_MEDIA = 'clip-2026-09-23_19-22-04_ace_clutch.webm';

/**
 * Generate the media the mock serves.
 *
 * `testsrc` is chosen for a reason: it draws a burnt-in frame counter and a moving pattern,
 * so a screenshot of the player at 5.1s is visibly *that frame* rather than a black
 * rectangle that could equally be a broken element.
 *
 * Without ffmpeg on PATH the script still runs — the player stays black and the list shows
 * placeholders — and says so loudly, because a screenshot of a failed load must not be
 * mistaken for a design decision.
 */
function buildMedia() {
  if (!hasFfmpeg()) {
    log('  ! ffmpeg is not on PATH: no media or thumbnails will be generated.');
    log('    The player will render black and the list will show placeholders.');
    return false;
  }

  mkdirSync(mediaDir, { recursive: true });
  const video = join(mediaDir, FEATURE_MEDIA);
  if (!existsSync(video)) {
    log(`  generating ${FEATURE_MEDIA} (13s VP8, 640x360, burnt-in frame counter)`);
    ffmpeg([
      '-f', 'lavfi',
      '-i', `testsrc=size=640x360:rate=25:duration=13`,
      '-c:v', 'libvpx',
      '-b:v', '600k',
      '-pix_fmt', 'yuv420p',
      '-an',
      video,
    ]);
  }

  // One still per clip id in every fixture, taken from the same source at a different
  // offset so no two rows show the same picture. The trimmed clip's id is included because
  // `06-trimmed-notice` asks the mock for a thumbnail for the row the trim just created.
  const ids = new Set([
    ...listClips.map((c) => c.id),
    ...TWO_MATCHES.map((c) => c.id),
    ...LONG_CLIPS.map((c) => c.id),
    LIST_FIXTURE.nextTrimId,
  ]);
  for (const id of ids) {
    const still = join(mediaDir, `thumb-${id}.png`);
    if (existsSync(still)) continue;
    ffmpeg([
      '-f', 'lavfi',
      '-i', `testsrc=size=128x72:rate=1:duration=1`,
      '-vf', `select='eq(n\\,0)'`,
      '-frames:v', '1',
      still,
    ]);
  }
  log(`  media ready: ${FEATURE_MEDIA} + ${ids.size} thumbnails in target/desktop-shots/media`);
  return true;
}

// ---------------------------------------------------------------------------------------
// a throwaway static server for dist/ (with Range support, so <video> can seek)
// ---------------------------------------------------------------------------------------

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.png': 'image/png',
  '.webm': 'video/webm',
  '.mp4': 'video/mp4',
  '.json': 'application/json; charset=utf-8',
};

function startServer() {
  const server = createServer((request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    if (url.pathname === '/favicon.ico') {
      response.writeHead(204).end();
      return;
    }

    const wantsMedia = url.pathname.startsWith('/media/');
    const relative = wantsMedia ? url.pathname.slice('/media/'.length) : url.pathname.slice(1);
    const root = wantsMedia ? mediaDir : distDir;
    const file = resolve(root, relative === '' ? 'index.html' : relative);

    if (!file.startsWith(root) || !existsSync(file) || statSync(file).isDirectory()) {
      response.writeHead(404, { 'content-type': 'text/plain' }).end(`not found: ${url.pathname}`);
      return;
    }

    const type = MIME[extname(file)] ?? 'application/octet-stream';
    const body = readFileSync(file);
    const range = /^bytes=(\d*)-(\d*)$/.exec(request.headers.range ?? '');

    if (range) {
      const start = range[1] === '' ? undefined : Number(range[1]);
      const end = range[2] === '' ? undefined : Number(range[2]);
      const from = start ?? Math.max(0, body.length - (end ?? 0));
      const to = Math.min(end ?? body.length - 1, body.length - 1);
      response.writeHead(206, {
        'content-type': type,
        'content-range': `bytes ${from}-${to}/${body.length}`,
        'accept-ranges': 'bytes',
        'content-length': to - from + 1,
      });
      response.end(body.subarray(from, to + 1));
      return;
    }

    response.writeHead(200, {
      'content-type': type,
      'content-length': body.length,
      'accept-ranges': 'bytes',
    });
    response.end(body);
  });

  return new Promise((ok) => {
    server.listen(0, '127.0.0.1', () => ok({ server, port: server.address().port }));
  });
}

// ---------------------------------------------------------------------------------------
// audits: DOM geometry, colour contrast
// ---------------------------------------------------------------------------------------

/**
 * Everything the timeline paints, in page coordinates.
 *
 * The grab bar of a handle is its `::before` — a real painted rectangle that can be measured
 * through `getComputedStyle(el, '::before')`. That matters: the handle *button* is a 14px
 * hit area that deliberately overflows the track, so measuring the button would hide the one
 * defect worth catching at the ends of the trim range, which is a bar half outside the
 * track.
 */
async function timelineGeometry(page) {
  return page.evaluate(() => {
    const rectOf = (el) => {
      const r = el.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width, height: r.height };
    };
    const track = document.querySelector('.track');
    if (!track) return null;

    const handleOf = (el) => {
      const r = rectOf(el);
      const style = getComputedStyle(el, '::before');
      const offset = parseFloat(style.left) || 0;
      const width = parseFloat(style.width) || 0;
      return {
        label: el.getAttribute('aria-label'),
        value: Number(el.getAttribute('aria-valuenow')),
        disabled: el.disabled,
        box: r,
        bar: { left: r.left + offset, right: r.left + offset + width, width },
        barColor: style.backgroundColor,
      };
    };

    const playhead = document.querySelector('.playhead');
    const selection = document.querySelector('.selection');
    return {
      viewport: { width: window.innerWidth, height: window.innerHeight },
      track: rectOf(track),
      handles: [...document.querySelectorAll('.handle')].map(handleOf),
      playhead: playhead
        ? { ...rectOf(playhead), color: getComputedStyle(playhead).backgroundColor }
        : null,
      selection: selection
        ? { ...rectOf(selection), color: getComputedStyle(selection).backgroundColor }
        : null,
      trackBackground: getComputedStyle(track).backgroundColor,
      trackGradient: getComputedStyle(track).backgroundImage,
    };
  });
}

/** Contrast ratios for the text that has to be readable on this dark surface. */
async function contrastAudit(page, labels) {
  return page.evaluate((labelMap) => {
    const parse = (value) => {
      const match = /rgba?\(([^)]+)\)/.exec(value ?? '');
      if (!match) return null;
      const parts = match[1].split(/[,\s/]+/).filter((p) => p !== '').map(Number);
      if (parts.length < 3 || parts.some((n) => Number.isNaN(n))) return null;
      return { r: parts[0], g: parts[1], b: parts[2], a: parts.length > 3 ? parts[3] : 1 };
    };
    // What actually paints behind the text: every translucent layer composited down to
    // the first opaque one. Reading only the nearest layer with alpha > 0 would grade a
    // 7%-tint verdict as white-on-amber, when the eye sees near-white on near-black.
    const effectiveBackground = (el) => {
      const paints = [];
      for (let node = el; node; node = node.parentElement) {
        const style = getComputedStyle(node);
        // background-image paints above background-color, so within one node the image
        // comes later in paint order.
        const gradient = /gradient\(([^)]+)\)/.exec(style.backgroundImage ?? '');
        if (gradient) {
          const stop = parse(gradient[1]);
          if (stop) paints.push(stop);
        }
        const flat = parse(style.backgroundColor);
        if (flat && flat.a > 0) paints.push(flat);
      }
      // Collected leaf-first; composite root-first over transparency.
      let out = { r: 0, g: 0, b: 0, a: 0 };
      for (let i = paints.length - 1; i >= 0; i--) {
        const layer = paints[i];
        const a = layer.a ?? 1;
        out = {
          r: layer.r * a + out.r * (1 - a),
          g: layer.g * a + out.g * (1 - a),
          b: layer.b * a + out.b * (1 - a),
          a: a + out.a * (1 - a),
        };
      }
      return out.a <= 0 ? { r: 255, g: 255, b: 255, a: 1 } : out;
    };
    const luminance = ({ r, g, b }) => {
      const channel = (v) => {
        const c = v / 255;
        return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
      };
      return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
    };
    const ratio = (a, b) => {
      const la = luminance(a);
      const lb = luminance(b);
      return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
    };

    const out = [];
    for (const [selector, label] of Object.entries(labelMap)) {
      const el = document.querySelector(selector);
      if (!el) {
        out.push({ selector, label, missing: true });
        continue;
      }
      const style = getComputedStyle(el);
      const foreground = parse(style.color) ?? { r: 0, g: 0, b: 0, a: 1 };
      const background = effectiveBackground(el);
      const size = parseFloat(style.fontSize);
      const weight = Number(style.fontWeight) || 400;
      const large = size >= 24 || (size >= 18.66 && weight >= 700);
      out.push({
        selector,
        label,
        foreground: `rgb(${foreground.r}, ${foreground.g}, ${foreground.b})`,
        background: `rgb(${background.r}, ${background.g}, ${background.b})`,
        fontSize: size,
        ratio: Number(ratio(foreground, background).toFixed(2)),
        threshold: large ? 3 : 4.5,
        text: (el.textContent ?? '').trim().slice(0, 48),
      });
    }
    return out;
  }, labels);
}

/** The page's own layout arithmetic: anything overflowing, scrolled or zero-sized. */
async function layoutAudit(page) {
  return page.evaluate(() => {
    const rect = (selector) => {
      const el = document.querySelector(selector);
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, height: r.height, width: r.width };
    };
    const scroller = (selector) => {
      const el = document.querySelector(selector);
      if (!el) return null;
      return {
        clientHeight: el.clientHeight,
        scrollHeight: el.scrollHeight,
        clientWidth: el.clientWidth,
        scrollWidth: el.scrollWidth,
        scrollsVertically: el.scrollHeight > el.clientHeight + 1,
        scrollsHorizontally: el.scrollWidth > el.clientWidth + 1,
      };
    };
    return {
      viewport: { width: window.innerWidth, height: window.innerHeight },
      document: {
        scrollWidth: document.documentElement.scrollWidth,
        scrollHeight: document.documentElement.scrollHeight,
      },
      // Every region is addressed by its `data-region` hook, never by a layout class, so a
      // view can be restyled without this audit (or any state below) noticing. A region is
      // `null` while its view is not the open one.
      rail: rect('[data-region=rail]'),
      view: rect('.view'),
      viewScroll: scroller('.view'),
      clipList: scroller('[data-region=clip-grid] ul'),
      sessionList: scroller('[data-region=session-list] ul'),
      sessionPage: rect('[data-region=session-page]'),
      // `.storage`, not `.panel`: the view's root is the storage panel, and this audit is
      // about its arithmetic.
      storage: rect('[data-region=storage] .storage'),
      storageScroll: scroller('[data-region=storage] .storage'),
      recorder: rect('[data-region=recorder]'),
      timeline: rect('.timeline'),
      track: rect('.track'),
      player: rect('.player'),
      trimSection: rect('[data-region=clip-page] .panel.trim'),
      trimButton: rect('[data-region=clip-page] .panel.trim button.primary'),
    };
  });
}

const insideViewport = (r, viewport) =>
  r !== null && r.top >= -0.5 && r.bottom <= viewport.height + 0.5 && r.left >= -0.5 && r.right <= viewport.width + 0.5;

// ---------------------------------------------------------------------------------------
// the states
// ---------------------------------------------------------------------------------------

const DETAIL_GROUP = 'detail';
const featureName = 'clip-2026-09-23_19-22-04_ace_clutch.webm';

/** Where a fraction of the track lands, in client coordinates. */
const xAt = (track, fraction) => track.left + track.width * fraction;

/** Rail label → the `data-region` its view carries. */
const VIEW_REGIONS = {
  Library: 'library',
  Sessions: 'sessions',
  Storage: 'storage',
  Health: 'health',
  Settings: 'settings',
};

/** Open a rail destination the way a user does: click its item, wait for its view. */
async function showView(page, label) {
  await page.locator('[data-region=rail]').getByRole('button', { name: label }).click();
  await page.locator(`[data-region=${VIEW_REGIONS[label]}]`).waitFor({ state: 'visible' });
}

/** From the library, open the first (newest) clip's page. */
async function openFirstClip(page) {
  await showView(page, 'Library');
  await page.locator('[data-region=clip-grid] button.row').first().click();
  await page.locator('[data-region=clip-page]').waitFor({ state: 'visible' });
}

/** The clip page's Back button, to the library. */
async function backToLibrary(page) {
  await page.locator('[data-region=clip-page]').getByRole('button', { name: 'Library' }).click();
  await page.locator('[data-region=library]').waitFor({ state: 'visible' });
}

const states = [
  {
    name: '00-no-tauri-ipc',
    title: 'the production default: no seam, no Tauri runtime',
    group: 'nomock',
    url: '/',
    note: 'the two halves of the seam are absent, so `main.ts` mounts with its default `tauriIpc`',
    async verify(page) {
      const banner = page.locator('.banner.error');
      await banner.waitFor({ state: 'visible', timeout: 10_000 });
      check(this.name, true, 'an error banner (role=alert) is shown after `invoke` fails');
      check(this.name, (await page.locator('.banner.error').getAttribute('role')) === 'alert', 'the banner carries role=alert');
      const text = (await banner.textContent()) ?? '';
      report(this.name, `banner text: ${JSON.stringify(text.replace(/\s+/g, ' ').trim().slice(0, 110))}`);
      check(this.name, /Error\./.test(text), 'the banner names the failure');
      check(
        this.name,
        await page.getByText('No clips are indexed yet').isVisible(),
        'the clip list is empty rather than absent',
      );
      // The rail is the app's frame: it is there even when every command failed.
      check(this.name, await page.locator('[data-region=rail]').isVisible(), 'the navigation rail is still rendered');
      await showView(page, 'Storage');
      check(
        this.name,
        (await page.locator('[data-region=storage] .storage').count()) === 0,
        'no storage panel: `storage_stats` failed too, and the panel is not invented',
      );
      await showView(page, 'Library');
      // The recorder panel is structural (it is where recording is driven the moment the
      // IPC answers), and with no status it says so rather than showing invented numbers.
      const recorder = page.locator('[data-region=recorder]');
      check(this.name, await recorder.isVisible(), 'the recorder bar is still rendered');
      check(
        this.name,
        ((await recorder.locator('.badge').textContent()) ?? '').trim() === 'not recording',
        'and reports that nothing is recording',
      );
    },
  },
  {
    name: '01-empty',
    title: 'empty state: nothing indexed yet',
    group: 'empty',
    fixture: EMPTY_FIXTURE,
    async verify(page) {
      check(this.name, await page.getByText('No clips are indexed yet').isVisible(), 'the list explains how to record a clip');
      check(
        this.name,
        (await page.locator('[data-region=library]').getByText('0 indexed').count()) === 1,
        'the library header counts zero',
      );
      await showView(page, 'Sessions');
      check(this.name, (await page.getByText('0 indexed').count()) === 1, 'and so does the sessions header');
      check(
        this.name,
        await page.getByText('Select a session to see its markers').isVisible(),
        'the sessions detail pane says what to do instead of showing a blank pane',
      );
      await showView(page, 'Storage');
      // Storage is a full view, not a disclosure: the numbers and the arithmetic are both on
      // screen the moment the view opens.
      check(
        this.name,
        await page.locator('.storage .figures').getByText('0 B / 50 GiB').isVisible(),
        'the storage header carries the numbers',
      );
      check(this.name, await page.locator('.storage dl').isVisible(), 'and the arithmetic is on screen with them');
      check(this.name, await page.getByText('50 GiB').first().isVisible(), 'the storage panel names the cap');
      // The default view is the library; the screenshot is of it.
      await showView(page, 'Library');
    },
  },
  {
    name: '02-clip-list',
    title: 'clip list: six clips, varied durations, sizes and codecs',
    group: 'list',
    fixture: LIST_FIXTURE,
    async verify(page) {
      const names = await page.locator('.name').allTextContents();
      check(this.name, names.length === 6, `six rows are rendered (${names.length})`);
      check(
        this.name,
        names[0] === featureName,
        'the newest clip is first',
      );
      check(
        this.name,
        (await page.locator('.row[aria-current="true"]').count()) === 0,
        'no row is marked as selected: the library is a list, a clip opens on its own page',
      );
      const pressed = await page.locator('[data-region=clip-grid] .star').evaluateAll((els) =>
        els.map((el) => el.getAttribute('aria-pressed')),
      );
      check(this.name, pressed.filter((p) => p === 'true').length === 2, 'two rows are favourited');
      check(this.name, pressed.filter((p) => p === 'false').length === 4, 'four rows are not');
      const body = (await page.locator('[data-region=clip-grid] ul').textContent()) ?? '';
      for (const expected of ['1:11:05.0', '0:00.9', '142 MiB', '2.9 GiB']) {
        check(this.name, body.includes(expected), `the list shows ${expected}`);
      }

      await waitForThumbnails(page, 6);
      const images = await page.locator('.thumb').evaluateAll((els) =>
        els.map((el) => ({ tag: el.tagName, width: el.naturalWidth ?? 0 })),
      );
      const real = images.filter((i) => i.tag === 'IMG' && i.width > 0).length;
      if (mediaAvailable) {
        check(this.name, real === 6, `six thumbnails decoded to real pixels (${real}); none is a broken image`);
      } else {
        report(this.name, `ffmpeg was missing, so ${real} of ${images.length} thumbnails decoded`);
      }

      const layout = await layoutAudit(page);
      check(
        this.name,
        layout.document.scrollWidth <= layout.viewport.width,
        `the library does not overflow horizontally (${layout.document.scrollWidth} of ${layout.viewport.width})`,
      );
      if (layout.clipList.scrollsVertically) {
        report(this.name, `the clip list scrolls (${layout.clipList.scrollHeight}px of rows in ${layout.clipList.clientHeight}px)`);
      }

      // Opening the newest clip: its page, and its favourite button agrees with its row.
      await openFirstClip(page);
      check(
        this.name,
        (await page.locator('[data-region=clip-page] .star').getAttribute('aria-pressed')) === 'true',
        "the clip page's favourite button agrees with the row it was opened from",
      );
      check(
        this.name,
        (await page.locator('[data-region=rail]').getByRole('button', { name: 'Library' }).getAttribute('aria-current')) === 'page',
        'the clip page lights the Library item on the rail',
      );
      await backToLibrary(page);
      check(this.name, (await page.locator('[data-region=clip-grid] .name').count()) === 6, 'Back returns to the full list');
      const columns = () =>
        page.locator('[data-region=clip-grid] ul').evaluate((el) => getComputedStyle(el).gridTemplateColumns.split(' ').length);
      check(this.name, (await columns()) === 5, `the grid has five columns at 1280 (${await columns()})`);
      // Keyboard path: the card's menu opens from the keyboard, Escape closes it and returns focus.
      const more = page.locator('[data-region=clip-grid] .more').first();
      await more.focus();
      await page.keyboard.press('Enter');
      check(this.name, (await page.getByRole('menuitem', { name: 'Show in folder' }).count()) === 1, 'the card menu opens from the keyboard');
      await page.keyboard.press('Escape');
      check(this.name, (await page.locator('[role=menu]').count()) === 0, 'Escape closes the card menu');
      check(this.name, await more.evaluate((el) => el === document.activeElement), 'and focus returns to the menu button');
      await page.setViewportSize({ width: 940, height: 600 });
      await page.waitForTimeout(150);
      check(this.name, (await columns()) === 3, `the grid has three columns at 940 (${await columns()})`);
      await page.setViewportSize({ width: 1280, height: 800 });
      await page.waitForTimeout(150);
      // The library at rest is where the heading, count, name and metadata labels live.
      await auditContrast(page, this.name);
    },
  },
  {
    name: '03-detail-full-range',
    title: 'clip detail: player, timeline, playhead and both handles at the full range',
    group: DETAIL_GROUP,
    fixture: LIST_FIXTURE,
    async verify(page, context) {
      await waitForThumbnails(page, 6);
      await openFirstClip(page);
      await waitForMedia(page);
      check(this.name, await page.getByRole('heading', { name: featureName }).isVisible(), 'the detail header names the clip');
      const chips = await page.locator('.facts .chip').allTextContents();
      check(this.name, chips.length === 4, `four fact chips are rendered (${chips.join(' | ')})`);

      const geometry = await timelineGeometry(page);
      context.geometry = geometry;
      check(this.name, geometry !== null, 'the timeline is in the DOM');
      check(this.name, geometry.handles.length === 2, 'two trim handles exist');
      check(
        this.name,
        geometry.handles[0]?.value === 0 && geometry.handles[1]?.value === FEATURE_DURATION_MS,
        `the range starts as the whole clip (${geometry.handles[0]?.value} → ${geometry.handles[1]?.value} of ${FEATURE_DURATION_MS}ms)`,
      );
      check(
        this.name,
        geometry.handles.every((h) => !h.disabled),
        'both handles are enabled',
      );
      check(
        this.name,
        geometry.handles.every((h) => h.bar.width >= 3.5 && h.bar.width <= 4.5),
        `both grab bars are painted 4px wide (${geometry.handles.map((h) => h.bar.width).join(', ')})`,
      );
      check(
        this.name,
        geometry.handles.every((h) => h.barColor !== 'rgba(0, 0, 0, 0)'),
        `both grab bars have a colour, not transparent (${geometry.handles.map((h) => h.barColor).join(', ')})`,
      );
      check(
        this.name,
        geometry.handles.every((h) => h.bar.left < geometry.track.right && h.bar.right > geometry.track.left),
        'both grab bars overlap the track rather than sitting outside it',
      );
      // 1.5px of tolerance: the handles are positioned against the track's *padding* box,
      // so a bar at 0% overhangs the track's own 1px border. Anything larger is a bar that
      // is genuinely half outside the track.
      for (const handle of geometry.handles) {
        const overhang = Math.max(geometry.track.left - handle.bar.left, handle.bar.right - geometry.track.right);
        check(
          this.name,
          overhang <= 1.5,
          `the ${handle.label} grab bar is inside the track (overhang ${overhang.toFixed(1)}px; ` +
            `bar ${handle.bar.left.toFixed(1)}..${handle.bar.right.toFixed(1)} of track ${geometry.track.left.toFixed(1)}..${geometry.track.right.toFixed(1)})`,
        );
      }
      check(
        this.name,
        geometry.playhead !== null && geometry.playhead.height > geometry.track.height * 0.8,
        `the playhead is painted at full track height (${geometry.playhead?.height.toFixed(1)}px of ${geometry.track.height.toFixed(1)}px)`,
      );
      const startGap = geometry.playhead && geometry.handles[0]
        ? Math.abs(geometry.handles[0].box.left + geometry.handles[0].box.width / 2 - (geometry.playhead.left + geometry.playhead.width / 2))
        : null;
      report(this.name, `playhead centre is ${startGap.toFixed(1)}px from the trim-start handle at position 0`);
      check(
        this.name,
        insideViewport(geometry.track, geometry.viewport),
        `the track is fully inside the window (top ${geometry.track.top.toFixed(0)}, bottom ${geometry.track.bottom.toFixed(0)})`,
      );

      const media = await page.locator('video').evaluate((el) => ({
        videoWidth: el.videoWidth,
        videoHeight: el.videoHeight,
        readyState: el.readyState,
        controls: el.controls,
        rect: el.getBoundingClientRect().toJSON(),
      }));
      if (mediaAvailable) {
        check(this.name, media.videoWidth > 0, `the player decoded real media (${media.videoWidth}x${media.videoHeight}, ${FEATURE_MEDIA})`);
      } else {
        report(this.name, `no generated media: the player is at readyState ${media.readyState}`);
      }
      check(this.name, media.rect.width > 200 && media.rect.height > 100, `the player has a real box (${media.rect.width.toFixed(0)}x${media.rect.height.toFixed(0)})`);
      check(this.name, media.controls, 'the player keeps its native controls');

      const layout = await layoutAudit(page);
      context.layout = layout;
      check(this.name, layout.document.scrollWidth <= layout.viewport.width, `the window does not overflow horizontally (${layout.document.scrollWidth} of ${layout.viewport.width})`);
      if (layout.viewScroll.scrollsVertically) {
        report(
          this.name,
          `the clip page scrolls: ${layout.viewScroll.scrollHeight}px of content in ${layout.viewScroll.clientHeight}px ` +
            `(trim section bottom ${layout.trimSection?.bottom.toFixed(0)}, trim button bottom ${layout.trimButton?.bottom.toFixed(0)})`,
        );
      }
      check(
        this.name,
        insideViewport(layout.player, layout.viewport) || layout.player.top >= 0,
        `the player starts inside the window (top ${layout.player.top.toFixed(0)})`,
      );
    },
  },
  {
    name: '04-detail-seeked',
    title: 'clip detail: scrubbed, so the playhead is off the start of the track',
    group: DETAIL_GROUP,
    async verify(page, context) {
      await waitForMedia(page);
      const track = (await timelineGeometry(page)).track;
      const fraction = 0.4;
      await page.mouse.click(xAt(track, fraction), track.top + track.height / 2);
      await waitForFrame(page);

      const geometry = await timelineGeometry(page);
      const readout = (await page.locator('.playhead-readout').textContent()) ?? '';
      const expected = Math.round(fraction * FEATURE_DURATION_MS);
      check(this.name, /^\d:\d\d\.\d{3}$/.test(readout.trim()), `the playhead readout is a timestamp (${readout.trim()})`);
      const readoutMs = msFromStamp(readout);
      check(
        this.name,
        Math.abs(readoutMs - expected) <= 200,
        `clicking ${(fraction * 100).toFixed(0)}% of the track seeks to about ${expected}ms (readout says ${readoutMs}ms)`,
      );
      const currentTime = await page.locator('video').evaluate((el) => el.currentTime * 1000);
      if (mediaAvailable) {
        const ready = await page.locator('video').evaluate((el) => el.readyState);
        check(this.name, ready >= 2, `the player has the seeked frame (readyState ${ready})`);
        check(
          this.name,
          Math.abs(currentTime - readoutMs) <= 120,
          `the video element followed the playhead to ${currentTime.toFixed(0)}ms`,
        );
      } else {
        report(this.name, 'no generated media to seek');
      }
      const playheadCentre = geometry.playhead.left + geometry.playhead.width / 2;
      check(
        this.name,
        Math.abs(playheadCentre - xAt(geometry.track, fraction)) <= 3,
        `the playhead is painted where it was clicked (${playheadCentre.toFixed(1)} vs ${xAt(geometry.track, fraction).toFixed(1)})`,
      );
      const start = geometry.handles[0];
      const gap = Math.abs(start.box.left + start.box.width / 2 - playheadCentre);
      check(this.name, gap > 12, `the playhead is distinguishable from the trim-start handle (${gap.toFixed(1)}px apart)`);
      check(this.name, geometry.playhead.width <= 3, `the playhead stays a thin line (${geometry.playhead.width.toFixed(1)}px)`);
      check(
        this.name,
        geometry.playhead.color !== geometry.selection.color,
        `the playhead and the selection band are different colours (${geometry.playhead.color} vs ${geometry.selection.color})`,
      );
      context.seekedSeconds = currentTime / 1000;
    },
  },
  {
    name: '05-trim-partial',
    title: 'trim in progress: both handles dragged in, partial range in the readout',
    group: DETAIL_GROUP,
    async verify(page, context) {
      const before = await timelineGeometry(page);
      const start = before.handles[0];
      const end = before.handles[1];
      const midY = before.track.top + before.track.height / 2;

      // Real mouse input: pointerdown on the handle, move, up — the same events a hand
      // produces. The handles capture the pointer on the track, so the drag is delivered
      // even once it leaves the handle.
      await page.mouse.move(start.box.left + start.box.width / 2, midY);
      await page.mouse.down();
      await page.mouse.move(xAt(before.track, 0.25), midY, { steps: 12 });
      await page.mouse.up();

      await page.mouse.move(end.box.left + end.box.width / 2, midY);
      await page.mouse.down();
      await page.mouse.move(xAt(before.track, 0.72), midY, { steps: 12 });
      await page.mouse.up();
      await page.waitForTimeout(50); // let Svelte flush the range to the DOM

      const after = await timelineGeometry(page);
      const tolerance = (FEATURE_DURATION_MS * 3) / before.track.width + 20;
      const expectedStart = Math.round(0.25 * FEATURE_DURATION_MS);
      const expectedEnd = Math.round(0.72 * FEATURE_DURATION_MS);

      check(
        this.name,
        Math.abs(after.handles[0].value - expectedStart) <= tolerance,
        `dragging the start handle to 25% of the track set start=${after.handles[0].value}ms (expected ~${expectedStart}ms, ±${tolerance.toFixed(0)})`,
      );
      check(
        this.name,
        Math.abs(after.handles[1].value - expectedEnd) <= tolerance,
        `dragging the end handle to 72% set end=${after.handles[1].value}ms (expected ~${expectedEnd}ms, ±${tolerance.toFixed(0)})`,
      );
      check(
        this.name,
        after.handles[0].value > 0 && after.handles[1].value < FEATURE_DURATION_MS,
        'both handles are off the ends',
      );

      const band = after.selection;
      const bandLeftFraction = (band.left - after.track.left) / after.track.width;
      const bandWidthFraction = band.width / after.track.width;
      check(
        this.name,
        Math.abs(bandLeftFraction - 0.25) < 0.03 && Math.abs(bandWidthFraction - 0.47) < 0.04,
        `the selection band covers the dragged range (left ${(bandLeftFraction * 100).toFixed(1)}%, width ${(bandWidthFraction * 100).toFixed(1)}%)`,
      );

      const legend = (await page.locator('.legend .mono').first().textContent()) ?? '';
      const panel = (await page.locator('.panel.trim .selection .mono').textContent()) ?? '';
      const selected = (await page.locator('.panel.trim .selection .muted').textContent()) ?? '';
      check(this.name, /^\d:\d\d\.\d{3} → \d:\d\d\.\d{3}$/.test(legend.trim()), `the timeline legend shows the partial range (${legend.trim()})`);
      check(this.name, legend.trim() === panel.trim(), `the trim panel agrees with the legend (${panel.trim()})`);
      check(this.name, !legend.trim().startsWith('0:00.000 → 0:12.960'), 'the readout is a partial range, not the whole clip');
      check(this.name, /0:0[6-7]\.\d/.test(selected), `the panel states the length of the selection (${selected.trim()})`);
      check(
        this.name,
        await page.locator('.panel.trim button.primary').isEnabled(),
        'the trim button is enabled for a valid range',
      );
      check(
        this.name,
        (await page.locator('.panel.trim .invalid').count()) === 0,
        'no validation complaint is shown',
      );
      context.range = { startMs: after.handles[0].value, endMs: after.handles[1].value };
    },
  },
  {
    name: '06-trimmed-notice',
    title: 'after a trim: the notice, the new clip and its own detail view',
    group: DETAIL_GROUP,
    async verify(page, context) {
      const button = page.locator('.panel.trim button.primary');
      await button.click();
      const notice = page.locator('.banner.notice');
      await notice.waitFor({ state: 'visible', timeout: 10_000 });
      const text = ((await notice.textContent()) ?? '').replace(/\s+/g, ' ').trim();
      report(this.name, `notice: ${JSON.stringify(text.slice(0, 150))}`);
      check(this.name, text.includes('Trimmed to'), 'the notice reports the trim');
      check(
        this.name,
        text.includes('.trim-'),
        'the notice names the file that was written',
      );
      check(
        this.name,
        /asked for .*, the file is /.test(text),
        'the notice reports both the asked-for and the written length when they differ',
      );
      const heading = (await page.getByRole('heading', { level: 1 }).textContent()) ?? '';
      check(this.name, heading.includes('.trim-'), `the clip page switched to the new clip (${heading})`);
      const chips = await page.locator('.facts .chip').allTextContents();
      check(this.name, chips.every((c) => !/^row \d+$/.test(c.trim())), 'no chip names the index row: that was a debugging leftover, not clip metadata');
      const trimmedRange = await timelineGeometry(page);
      check(
        this.name,
        trimmedRange.handles[0]?.value === 0,
        'the new clip opens with the full range selected',
      );
      // The list gained the new clip; then back on its page, which is what the shot shows.
      await backToLibrary(page);
      const names = await page.locator('.name').allTextContents();
      check(this.name, names.length === 7, `the new clip was added to the list (${names.length} rows)`);
      check(this.name, names[0]?.includes('.trim-') === true, `the new clip is the newest row (${names[0]})`);
      await openFirstClip(page);
    },
  },
  {
    name: '07-storage-favourites-over-cap',
    title: 'storage panel: the favourites alone exceed the cap',
    group: 'overcap',
    fixture: FAVOURITES_OVER_CAP_FIXTURE,
    extraShots: [{ name: '07-storage-panel-closeup', selector: '[data-region=storage] .storage' }],
    async verify(page) {
      await waitForThumbnails(page, 8);
      check(
        this.name,
        (await page.locator('[data-region=clip-grid] ul li').count()) === 8,
        'all eight clips are indexed',
      );
      await showView(page, 'Storage');
      const verdict = page.locator('.panel .verdict.bad');
      check(this.name, (await verdict.count()) === 1, 'exactly one bad verdict is rendered');
      const text = ((await verdict.first().textContent()) ?? '').replace(/\s+/g, ' ').trim();
      report(this.name, `verdict: ${JSON.stringify(text.slice(0, 190))}`);
      check(this.name, text.includes('This cap cannot be met.'), 'the verdict says the cap cannot be met');
      check(this.name, /exceed it by 1\.\d GiB/.test(text), 'the verdict quantifies the overshoot');
      check(this.name, text.includes('un-protect'), 'the verdict tells the user what to do');
      const border = await verdict.first().evaluate((el) => getComputedStyle(el).borderLeftColor);
      report(this.name, `verdict accent colour: ${border}`);

      const bar = await page.locator('.panel .bar').evaluate((el) => {
        const used = el.querySelector('.used');
        const favourites = el.querySelector('.favourites');
        return {
          track: el.getBoundingClientRect().width,
          used: used ? used.getBoundingClientRect().width : null,
          favourites: favourites ? favourites.getBoundingClientRect().width : null,
          usedColor: used ? getComputedStyle(used).backgroundColor : null,
          favouriteColor: favourites ? getComputedStyle(favourites).backgroundColor : null,
        };
      });
      report(
        this.name,
        `cap bar: ${bar.favourites?.toFixed(0)}px favourites + ${bar.used?.toFixed(0)}px other in ${bar.track.toFixed(0)}px ` +
          `(${bar.favouriteColor} / ${bar.usedColor})`,
      );
      check(this.name, bar.favourites > 0, 'the favourites portion of the bar is drawn');
      check(
        this.name,
        (await page.locator('.stats-grid, dl > div').count()) >= 5,
        'the panel lists its figures',
      );

      const layout = await layoutAudit(page);
      check(
        this.name,
        insideViewport(layout.storage, layout.viewport),
        `the whole storage panel is inside the window (top ${layout.storage.top.toFixed(0)}, bottom ${layout.storage.bottom.toFixed(0)})`,
      );
      check(
        this.name,
        !layout.storageScroll.scrollsVertically,
        `the storage panel's own content fits its box (${layout.storageScroll.scrollHeight}px in ${layout.storageScroll.clientHeight}px)`,
      );
    },
  },
  {
    name: '08-ffmpeg-missing',
    title: 'ffmpeg absent: placeholder thumbnails, error banner and startup warning',
    group: 'noffmpeg',
    fixture: NO_FFMPEG_FIXTURE,
    async verify(page) {
      // The startup warning is one of the three things this state photographs, and it is
      // rendered in the storage panel, so it is read from the Storage view.
      const banner = page.locator('.banner.error');
      await banner.waitFor({ state: 'visible', timeout: 10_000 });
      const text = ((await banner.textContent()) ?? '').replace(/\s+/g, ' ').trim();
      check(this.name, text.includes('ffmpeg was not found'), `the error banner carries the command error (${JSON.stringify(text.slice(0, 80))})`);
      const placeholders = await page.locator('.thumb.placeholder').count();
      check(this.name, placeholders === 6, `every row falls back to the placeholder (${placeholders} of 6)`);
      await showView(page, 'Storage');
      const warnings = page.locator('.verdict.bad');
      check(this.name, (await warnings.count()) === 1, 'the startup warning is rendered in the storage panel');
      check(
        this.name,
        ((await warnings.first().textContent()) ?? '').includes('ffmpeg was not found'),
        'the warning text is the one the Rust side sent',
      );
      const okVerdict = ((await page.locator('.panel .verdict.ok').first().textContent()) ?? '')
        .replace(/\s+/g, ' ')
        .trim();
      check(this.name, okVerdict.includes('The cap can be met'), 'the cap verdict is still the good one');
      check(this.name, okVerdict.includes('delete 2 clips'), `the planned deletion count is stated (${JSON.stringify(okVerdict)})`);
    },
  },
  {
    name: '09-recording',
    title: 'recording: the REC badge, the live counters and Save clip',
    group: 'recording',
    fixture: RECORDING_FIXTURE,
    async verify(page) {
      const panel = page.locator('[data-region=recorder]');
      check(this.name, await panel.isVisible(), 'the recorder bar is on screen');
      check(
        this.name,
        ((await panel.locator('.badge').textContent()) ?? '').trim() === 'REC',
        'the badge says REC while the engine runs',
      );
      check(this.name, (await panel.locator('.dot').count()) === 1, 'and carries the marker dot');
      check(
        this.name,
        await panel.getByRole('button', { name: 'Stop recording' }).isVisible(),
        'the primary action is Stop while recording',
      );
      check(
        this.name,
        (await panel.getByRole('button', { name: 'Start recording' }).count()) === 0,
        'and Start is not offered on top of a running recording',
      );
      check(
        this.name,
        await panel.getByRole('button', { name: 'Save clip' }).isEnabled(),
        'Save clip is enabled — the trigger is reachable',
      );

      // The readout, field by field, against the status the engine published.
      const readout = ((await panel.textContent()) ?? '').replace(/\s+/g, ' ');
      for (const expected of [
        '0:12.0 of media',
        '12 (40 MiB)',
        '480',
        '29.8 / 30 fps',
        '1258ms behind real time',
      ]) {
        check(this.name, readout.includes(expected), `the readout shows ${expected}`);
      }
      check(
        this.name,
        readout.includes('320 frames skipped without a GPU readback'),
        'the skipped frames are accounted for rather than silent',
      );
      check(
        this.name,
        !readout.includes('dropped by the encoder'),
        'and no encoder drop is claimed: there is none',
      );

      await waitForThumbnails(page, 6);
      const layout = await layoutAudit(page);
      check(
        this.name,
        insideViewport(layout.recorder, layout.viewport),
        `the recorder panel fits the window (bottom ${layout.recorder.bottom.toFixed(0)} of ${layout.viewport.height})`,
      );
      check(
        this.name,
        layout.view.top >= layout.recorder.bottom - 0.5,
        `and the active view starts below it (view top ${layout.view.top.toFixed(0)}, bar bottom ${layout.recorder.bottom.toFixed(0)})`,
      );

      // The half of the shell that is not a window, in the one place a user can read it:
      // the chord (and the fact that it works with this window hidden) and what closing the
      // window does — the sentence that stops "my app disappeared" being a mystery.
      const panelText = ((await panel.textContent()) ?? '').replace(/\s+/g, ' ');
      check(
        this.name,
        panelText.includes('Press Ctrl+F8 to take a clip'),
        `the panel names the chord to press (${JSON.stringify((await panel.locator('.hotkey').textContent())?.trim().slice(0, 70))})`,
      );
      check(
        this.name,
        panelText.includes('with this window hidden'),
        'and says it works with the window hidden',
      );
      check(
        this.name,
        panelText.includes('hides replayed in the tray'),
        'and says what closing the window does',
      );
      check(
        this.name,
        panelText.includes('Config: C:\\Users\\player\\AppData\\Local\\replayed\\config.toml'),
        'and names the config file this process read',
      );

      // The engine counters are a popover: open it so the contrast audit reads real, painted text.
      await panel.getByRole('button', { name: 'Engine counters' }).click();
      check(this.name, await panel.locator('.engine-pop').isVisible(), 'the engine counters open from the bar');
      await auditContrast(page, this.name, {
        '.recorder .badge.recording': 'REC badge',
        '.recorder .hotkey': 'hotkey line',
        '.recorder dt': 'recorder label',
        '.recorder dd': 'recorder value',
        '.recorder .footnote': 'recorder footnote',
        '.recorder .shell-notes .config': 'config path',
      });
    },
  },
  {
    name: '10-clip-saved',
    title: 'recording: Save clip writes a clip, reports it and lists it',
    group: 'recording',
    fixture: RECORDING_FIXTURE,
    async verify(page) {
      // A real pointer event on the button the panel renders; the mock answers with the
      // clip `clip_now` would return, and the list is refreshed exactly as it is after a
      // real trigger.
      await page.locator('[data-region=recorder]').getByRole('button', { name: 'Save clip' }).click();

      const notice = page.locator('.banner.notice');
      await notice.waitFor({ state: 'visible', timeout: 10_000 });
      const text = ((await notice.textContent()) ?? '').replace(/\s+/g, ' ').trim();
      check(
        this.name,
        text.includes('Saved clip-2026-09-23_19-31-02_saved_from_the_window.mp4'),
        `the notice names the file that was written (${JSON.stringify(text.slice(0, 90))})`,
      );
      check(this.name, text.includes('(0:12.0, 26 KiB, h264_nvenc)'), 'and its length, size and codec');
      check(this.name, text.includes('as clip #43'), 'and the row it was indexed as');

      const names = await page.locator('.name').allTextContents();
      check(this.name, names.length === 7, `the list gained the clip (${names.length} rows)`);
      check(
        this.name,
        names[0] === 'clip-2026-09-23_19-31-02_saved_from_the_window.mp4',
        'the clip that was just saved is the newest row',
      );
      check(
        this.name,
        await page.locator('.name').first().isVisible(),
        'and the row is on screen, not below the fold',
      );
      const errorBanner = await page.locator('.banner.error').count();
      check(this.name, errorBanner === 0, 'saving a clip reports no error');
    },
  },
  {
    name: '11-hotkey-not-installed',
    title: 'no hotkey: the chord that failed is on screen, and the app still works',
    group: 'nohotkey',
    fixture: NO_HOTKEY_FIXTURE,
    extraShots: [{ name: '11-hotkey-not-installed-closeup', selector: '.recorder .engine-pop' }],
    async verify(page) {
      // The whole reason this panel exists in the shape it does. A hotkey that silently does
      // nothing is the worst outcome this feature has, so the failure is an alert above the
      // buttons — not a log line, and not a footnote under the numbers.
      const panel = page.locator('[data-region=recorder]');
      check(
        this.name,
        await panel.locator('.hotkey-alert').isVisible(),
        'the bar itself flags the dead hotkey, without being opened',
      );
      await panel.getByRole('button', { name: 'Engine counters' }).click();
      const alert = panel.locator('.hotkey.bad');
      check(this.name, await alert.isVisible(), 'the hotkey failure is spelled out in the recorder popover');

      const text = ((await alert.textContent()) ?? '').replace(/\s+/g, ' ');
      check(this.name, text.includes('Ctrl+F8'), `it names the chord that failed (${JSON.stringify(text.slice(0, 80))})`);
      check(this.name, text.includes('NOT installed'), 'and says that nothing is listening');
      check(this.name, text.includes('already registered'), 'and carries the reason the registration failed');
      check(
        this.name,
        text.includes('another replayed'),
        'including the second-instance case: the CLI and the app cannot both hold one chord',
      );

      const panelText = ((await panel.textContent()) ?? '').replace(/\s+/g, ' ');
      check(
        this.name,
        panelText.includes('No config.toml at'),
        'and the config line says the example values are in force, naming the path',
      );
      check(
        this.name,
        panelText.includes('config.example.toml are'),
        'so the defaults are not presented as configured values',
      );

      // Without a hotkey this is still a recording application — the alert is why it is not
      // a refusal to start.
      check(
        this.name,
        await panel.getByRole('button', { name: 'Start recording' }).isEnabled(),
        'Start recording is still offered: the window works without the hotkey',
      );
      check(
        this.name,
        (await panel.locator('.badge').textContent())?.trim() === 'not recording',
        'and the recorder is honestly idle',
      );

      await waitForThumbnails(page, 6);
      await auditContrast(page, this.name, {
        '.recorder .hotkey': 'hotkey alert',
        '.recorder .hotkey-alert': 'hotkey flag in the bar',
        '.recorder .shell-notes .config': 'config path',
      });
    },
  },
  {
    name: '12-session-review',
    title: 'session review: a finished session with markers, and one still recording',
    group: 'session',
    fixture: SESSION_FIXTURE,
    async verify(page) {
      await waitForThumbnails(page, 2);
      await showView(page, 'Sessions');
      check(
        this.name,
        await page.getByText('No sessions are indexed yet').isHidden(),
        'the sessions list is populated, not empty',
      );
      const running = await page.getByRole('button', { name: /Manual recording/ }).textContent();
      check(this.name, (running ?? '').includes('recording'), 'the running session says it is recording');
      check(this.name, (running ?? '').includes('length unknown'), 'and that its length is unknown, not zero');

      await page.getByRole('button', { name: /Dota 2/ }).click();
      // The row actions fade in over 150ms; wait for them settled rather than reading
      // mid-transition (an opacity read during the fade is a fraction, not the verdict).
      await page.waitForFunction(
        () => {
          const actions = document.querySelector('[data-region=session-list] ul li.selected .actions');
          return actions !== null && getComputedStyle(actions).opacity === '1';
        },
        null,
        { timeout: 5_000 },
      );
      check(
        this.name,
        await page.locator('[data-region=session-list] ul li.selected .actions').first().isVisible(),
        'the selected row reveals its actions once the fade settles',
      );
      check(
        this.name,
        await page.getByRole('heading', { name: 'Dota 2' }).isVisible(),
        'selecting the session shows its detail, not a clip',
      );
      check(this.name, (await page.locator('.banner.error').count()) === 0, 'opening a session reports no error');
      check(
        this.name,
        (await page.locator('.markers tbody tr').count()) === 3,
        'all three markers are tabled in media time',
      );
      check(
        this.name,
        ((await page.locator('.markers').textContent()) ?? '').includes('#42'),
        'the bookmark names the clip it produced',
      );
      check(
        this.name,
        await page.getByRole('button', { name: 'Extract clip' }).isEnabled(),
        'a finished session offers the extract',
      );

      const layout = await layoutAudit(page);
      check(
        this.name,
        layout.sessionPage !== null && layout.sessionPage.right <= layout.viewport.width + 0.5,
        `the session page fits beside the list (right edge ${layout.sessionPage?.right.toFixed(0)} of ${layout.viewport.width})`,
      );
      check(
        this.name,
        layout.document.scrollWidth <= layout.viewport.width,
        `nothing overflows horizontally (${layout.document.scrollWidth} of ${layout.viewport.width})`,
      );
      await auditContrast(page, this.name);
    },
  },
  {
    name: '13-long-lists',
    title: 'long library: thirty clips and ten sessions stay inside their views',
    group: 'long',
    fixture: LONG_FIXTURE,
    async verify(page) {
      await waitForThumbnails(page, 30);
      check(this.name, (await page.locator('.name').count()) === 30, 'all thirty clip rows render');
      const layout = await layoutAudit(page);
      check(
        this.name,
        layout.document.scrollWidth <= layout.viewport.width,
        `nothing overflows horizontally (${layout.document.scrollWidth} of ${layout.viewport.width})`,
      );
      if (layout.clipList.scrollsVertically) {
        report(this.name, `the clip list scrolls (${layout.clipList.scrollHeight}px of rows in ${layout.clipList.clientHeight}px)`);
      } else {
        report(this.name, 'the clip list fits without scrolling');
      }
      check(
        this.name,
        layout.view.bottom <= layout.viewport.height + 0.5,
        `the library ends inside the window (bottom ${layout.view.bottom.toFixed(0)} of ${layout.viewport.height})`,
      );
      await showView(page, 'Sessions');
      const sessions = await layoutAudit(page);
      check(
        this.name,
        (await page.locator('[data-region=session-list] ul li').count()) === 10,
        'all ten session rows render',
      );
      check(
        this.name,
        sessions.sessionList !== null && !sessions.sessionList.scrollsHorizontally,
        'session rows do not scroll sideways',
      );
      check(
        this.name,
        sessions.view.bottom <= sessions.viewport.height + 0.5,
        `the sessions view ends inside the window (bottom ${sessions.view.bottom.toFixed(0)} of ${sessions.viewport.height})`,
      );
      await showView(page, 'Library');
    },
  },
  {
    name: '14-settings',
    title: 'settings: values, the auto-record toggle, and a rejected framerate',
    group: 'settings',
    fixture: SETTINGS_FIXTURE,
    extraShots: [{ name: '14-settings-encoder-closeup', selector: '#settings-encoder' }],
    async verify(page) {
      await waitForThumbnails(page, 6);
      check(
        this.name,
        ((await page.locator('.recorder .auto').textContent()) ?? '').includes('Automatic recording is off.'),
        'the recorder says nothing is armed while auto-record is off',
      );

      await showView(page, 'Settings');
      const panel = page.locator('.settings');
      check(this.name, await panel.getByRole('heading', { name: 'Library' }).isVisible(), 'the library section opens');
      check(
        this.name,
        (await panel.locator('input[type="number"]').first().inputValue()) === '30',
        'the framerate shows the configured value',
      );
      check(
        this.name,
        ((await panel.textContent()) ?? '').includes('League of Legends'),
        'the watched titles are listed',
      );

      // Narrow windows: the device pickers must wrap, never push a scrollbar into view.
      for (const width of [940, 700]) {
        await page.setViewportSize({ width, height: 600 });
        await page.waitForTimeout(150);
        const over = await page.evaluate(() =>
          [...document.querySelectorAll('.view, .settings, .settings select')].filter(
            (el) => el.scrollWidth > el.clientWidth + 1,
          ).length,
        );
        check(this.name, over === 0, `nothing scrolls sideways at ${width}px (${over} overflowing)`);
      }
      await page.setViewportSize({ width: 1280, height: 800 });
      await page.waitForTimeout(150);

      await panel.getByLabel(/Record watched games automatically/).check();
      await panel.getByRole('button', { name: 'Apply automatic' }).click();
      const notice = page.locator('.banner.notice');
      await notice.waitFor({ state: 'visible', timeout: 10_000 });
      check(
        this.name,
        ((await notice.textContent()) ?? '').includes('Saved games.auto_record.'),
        'the notice names the key that was saved',
      );
      // The recorder line follows on the status poll, not on the apply call.
      await page.waitForFunction(
        () =>
          (document.querySelector('.recorder .auto')?.textContent ?? '').includes(
            'Watching for a game',
          ),
        null,
        { timeout: 5_000 },
      );
      check(
        this.name,
        true,
        'the recorder line follows the toggle once it settles',
      );

      await panel.locator('input[type="number"]').first().fill('999');
      check(
        this.name,
        await panel.getByText('Framerate must be 1–240.').isVisible(),
        'an out-of-range framerate is refused inline',
      );
      check(
        this.name,
        await panel.getByRole('button', { name: 'Apply capture' }).isDisabled(),
        'and cannot be applied',
      );

      // The native picker has no Tauri runtime here: Browse must fail silent, not loud.
      await panel.getByRole('button', { name: 'Browse' }).click();
      check(
        this.name,
        (await page.locator('.banner.error').count()) === 0,
        'browsing without a runtime reports no error',
      );
      check(
        this.name,
        (await panel.locator('input[type="text"]').first().inputValue()) === '',
        'and leaves the draft alone',
      );

      // The encoder section: the encoder that was chosen is named, and the pickers offer
      // the chosen encoder's own choices rather than the union of every vendor's.
      const encoder = panel.locator('#settings-encoder');
      check(this.name, await encoder.isVisible(), 'the encoder section is on the page');
      const encoderText = ((await encoder.textContent()) ?? '').replace(/\s+/g, ' ');
      check(
        this.name,
        encoderText.includes('Selected: NVIDIA (NVENC), H.264'),
        'and names the encoder that will actually run',
      );
      const modes = await encoder
        .getByLabel('Rate control')
        .locator('option')
        .evaluateAll((options) => options.map((option) => option.value));
      check(
        this.name,
        modes.join(',') === 'cbr,vbr,quality',
        `the rate control offers only the chosen encoder's modes (${modes.join(', ')})`,
      );
      const presets = await encoder
        .getByLabel('Preset')
        .locator('option')
        .evaluateAll((options) => options.map((option) => option.value));
      check(
        this.name,
        presets.join(',') === ',p1,p2,p3,p4,p5,p6,p7',
        `and the presets are the encoder's own (${presets.join(', ')})`,
      );
      check(
        this.name,
        encoderText.includes('applied on the next recording start'),
        'and the panel says when the change takes effect',
      );

      await auditContrast(page, this.name, {
        '.settings .subnav button': 'settings sub-nav',
        '.settings input[type="text"]': 'settings field',
      });
    },
  },
  {
    name: '15-short-window',
    title: 'short window (940x600, the minimum): every view scrolls itself, nothing is unreachable',
    group: 'short',
    fixture: LONG_FIXTURE,
    async verify(page) {
      await page.setViewportSize({ width: 940, height: 600 });
      await page.waitForTimeout(150);
      await waitForThumbnails(page, 30);
      const layout = await layoutAudit(page);
      check(
        this.name,
        layout.document.scrollWidth <= layout.viewport.width,
        `nothing overflows horizontally (${layout.document.scrollWidth} of ${layout.viewport.width})`,
      );
      check(
        this.name,
        (await page.locator('.name').count()) === 30,
        'the clip rows still render in a short window',
      );
      check(
        this.name,
        insideViewport(layout.recorder, layout.viewport),
        `the recorder bar stays on screen (bottom ${layout.recorder.bottom.toFixed(0)} of ${layout.viewport.height})`,
      );
      check(
        this.name,
        insideViewport(layout.rail, layout.viewport),
        'and so does the rail, with every destination on it',
      );

      // Every destination is reachable: its content is on screen, or its own view scrolls to
      // it. (The window never scrolls as a whole: that is the old sidebar's last resort, gone.)
      for (const [label, selector] of [
        ['Storage', '[data-region=storage] .storage'],
        ['Health', '[data-region=health] .health'],
        ['Settings', '[data-region=settings] .settings'],
      ]) {
        await showView(page, label);
        const target = page.locator(selector);
        await target.scrollIntoViewIfNeeded();
        check(this.name, await target.isVisible(), `${label} is reachable in a 600px window`);
        const after = await layoutAudit(page);
        check(
          this.name,
          after.document.scrollWidth <= after.viewport.width && after.view.bottom <= after.viewport.height + 0.5,
          `${label} fits the window width and its view ends inside it`,
        );
      }
      await showView(page, 'Sessions');
      check(
        this.name,
        await page.locator('[data-region=session-list] ul li').first().isVisible(),
        'Sessions lists its rows with the detail pane beside them',
      );

      // The diagnostics button: one click, and the panel says what was copied — and that
      // nothing was sent anywhere.
      await showView(page, 'Health');
      const health = page.locator('[data-region=health] .health');
      await health.getByRole('button', { name: 'Copy diagnostics' }).click();
      const copied = page.locator('.copied');
      await copied.waitFor({ state: 'visible', timeout: 10_000 });
      const copiedText = ((await copied.textContent()) ?? '').replace(/\s+/g, ' ');
      check(
        this.name,
        copiedText.includes('0.2.0') && copiedText.includes('windows 11 x86_64'),
        'the copied report names the build and the machine',
      );
      check(
        this.name,
        ((await health.textContent()) ?? '').replace(/\s+/g, ' ').includes('Nothing is sent anywhere'),
        'and the panel says that nothing left the machine',
      );

      await showView(page, 'Library');
    },
  },
  {
    name: '16-storage-missing-files',
    title: 'storage panel: rows whose files are gone are named, not counted',
    group: 'missingfiles',
    fixture: MISSING_FILES_FIXTURE,
    extraShots: [{ name: '16-storage-missing-closeup', selector: '[data-region=storage] .storage' }],
    async verify(page) {
      await waitForThumbnails(page, 2);
      // The other half of the contradiction: the clips list must show the same set the
      // storage panel counts, or the two views are disagreeing again.
      check(
        this.name,
        (await page.locator('[data-region=clip-grid] ul li').count()) === 2,
        'the clips list shows the two clips the panel will count',
      );
      await showView(page, 'Storage');
      const panel = page.locator('[data-region=storage] .storage');
      const text = ((await panel.locator('dl').textContent()) ?? '').replace(/\s+/g, ' ').trim();
      report(this.name, `storage figures: ${JSON.stringify(text)}`);
      check(this.name, /Clips on disk\s*2/.test(text), 'the count is the clips that are on disk');
      check(this.name, /Missing files\s*9/.test(text), 'and the rows left out are named');
      check(this.name, text.includes('whose file is gone'), 'with what became of them');
    },
  },
];

/**
 * Wait for the clip list's thumbnails to finish decoding, so a screenshot is never taken
 * with half a strip of pictures in it. A missing ffmpeg means they never arrive, which is
 * reported rather than waited on for ever.
 */
async function waitForThumbnails(page, count) {
  if (!mediaAvailable) return;
  await page
    .waitForFunction(
      (expected) => {
        const images = [...document.images];
        return images.length >= expected && images.every((image) => image.complete);
      },
      count,
      { timeout: 15_000 },
    )
    .catch(() => {});
}

/** Wait for the `<video>` to have decoded metadata, so `videoWidth` is not read too early. */
async function waitForMedia(page) {
  if (!mediaAvailable) return;
  await page
    .waitForFunction(() => (document.querySelector('video')?.readyState ?? 0) >= 1, null, {
      timeout: 15_000,
    })
    .catch(() => {});
}

/** Wait for the seek to have produced a frame; the check that it did happens in `verify`. */
async function waitForFrame(page) {
  if (!mediaAvailable) return;
  await page
    .waitForFunction(() => (document.querySelector('video')?.readyState ?? 0) >= 2, null, {
      timeout: 15_000,
    })
    .catch(() => {});
}

/** `0:05.184` → 5184. */
function msFromStamp(stamp) {
  const match = /(\d+):(\d\d)\.(\d{3})/.exec(stamp.trim());
  if (!match) return Number.NaN;
  return Number(match[1]) * 60_000 + Number(match[2]) * 1_000 + Number(match[3]);
}

// ---------------------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------------------

async function main() {
  log('replayed desktop — headless render');
  log(`  shots go to ${shotsDir}`);

  if (installBrowserOnly) {
    log('  --install-browser-only: downloading the headless Chromium shell');
    const install = spawnSync(
      process.execPath,
      [join(appDir, 'node_modules', 'playwright', 'cli.js'), 'install', 'chromium', '--only-shell'],
      { stdio: 'inherit', env: process.env },
    );
    process.exit(install.status ?? 1);
  }

  if (!existsSync(distDir)) {
    log('  dist/ is missing — building');
  } else {
    log('  building the frontend (vite build)');
  }
  const build = spawnSync('npm', ['run', 'build'], { cwd: appDir, stdio: 'inherit' });
  if (build.status !== 0) {
    log('  ! vite build failed');
    process.exit(build.status ?? 1);
  }

  mkdirSync(shotsDir, { recursive: true });
  mediaAvailable = buildMedia();

  log('  starting the static server for dist/ and the generated media');
  const { server, port } = await startServer();
  const base = `http://127.0.0.1:${port}`;

  const { chromium } = await import('playwright');
  let browser;
  try {
    browser = await chromium.launch({ timeout: 60_000 });
  } catch (error) {
    if (!/Executable doesn't exist/.test(String(error))) throw error;
    log('  the headless Chromium shell is not downloaded yet — installing it into target/');
    const install = spawnSync(
      process.execPath,
      [join(appDir, 'node_modules', 'playwright', 'cli.js'), 'install', 'chromium', '--only-shell'],
      { stdio: 'inherit', env: process.env },
    );
    if (install.status !== 0) {
      log('  ! the browser install failed; retry with `npm run shots -- --install-browser-only`');
      process.exit(install.status ?? 1);
    }
    browser = await chromium.launch({ timeout: 60_000 });
  }
  log(`  chromium: ${browser.version()}`);

  const groups = new Map();
  const shots = [];

  try {
    for (const state of states) {
      log('');
      log(`${state.name} — ${state.title}`);
      try {
        await runState(state);
      } catch (error) {
        // One broken state must not cost the screenshots of the others: photograph what is
        // there, record the failure, and carry on.
        check(state.name, false, `the state threw: ${String(error?.message ?? error).split('\n')[0]}`);
        const page = groups.get(state.group)?.page;
        if (page) {
          const file = join(shotsDir, `${state.name}.png`);
          try {
            await page.screenshot({ path: file });
            shots.push(file);
            log(`    → ${state.name}.png (taken after the failure, for diagnosis)`);
          } catch {
            log('    (the page is unusable; no screenshot)');
          }
        }
      }
    }

    async function runState(state) {
      const fresh = !groups.has(state.group);
      let freshPage = null;
      if (fresh) {
        const context = await browser.newContext({
          viewport: VIEWPORT,
          deviceScaleFactor: 1,
          colorScheme: 'dark',
          reducedMotion: 'reduce',
        });
        if (state.fixture) {
          await context.addInitScript(installMockSource, state.fixture);
        }
        const page = await context.newPage();
        const consoleErrors = [];
        page.on('console', (message) => {
          if (message.type() === 'error') {
            const url = message.location()?.url ?? '';
            consoleErrors.push(`${message.text()}${url ? ` (${url})` : ''}`);
          }
        });
        page.on('pageerror', (error) => consoleErrors.push(`pageerror: ${error.message}`));
        page.on('response', (response) => {
          if (response.status() >= 400) {
            consoleErrors.push(`${response.status()} ${response.url()}`);
          }
        });
        groups.set(state.group, { context, page, consoleErrors, shared: {} });
        freshPage = page;
      }

      const { page, consoleErrors, shared } = groups.get(state.group);
      if (freshPage) {
        // A group is one page load: the detail states have to run as one session, because
        // state 5 drags the handles and state 6 trims the range it made. Reloading between
        // them would reset the range to the whole clip and quietly test something else.
        await page.goto(`${base}${state.url ?? '/?test-clip-source=1'}`, { waitUntil: 'load' });
        // The window's first paint is whatever the components do on mount; wait for the
        // list's own text so a screenshot cannot be taken mid-render.
        await page.locator('[data-region=rail]').waitFor({ state: 'visible' });
        await page.waitForFunction(() => {
          const images = [...document.images];
          return images.length === 0 || images.every((image) => image.complete);
        });
        await page.waitForTimeout(150);
      }
      if (!mediaAvailable && state.group !== 'nomock') {
        report(state.name, 'ffmpeg was missing: the player is black and thumbnails are placeholders');
      }

      await state.verify.call(state, page, shared);

      const file = join(shotsDir, `${state.name}.png`);
      await page.screenshot({ path: file });
      shots.push(file);
      log(`    → ${state.name}.png`);

      for (const extra of state.extraShots ?? []) {
        const extraFile = join(shotsDir, `${extra.name}.png`);
        await page.locator(extra.selector).screenshot({ path: extraFile });
        shots.push(extraFile);
        log(`    → ${extra.name}.png`);
      }

      const { explained, surprises } = partitionErrors(consoleErrors);
      for (const { error, why } of explained) {
        report(state.name, `expected: ${error} — ${why}`);
      }
      if (surprises.length > 0) {
        check(state.name, false, `the page logged ${surprises.length} error(s): ${surprises[0].slice(0, 140)}`);
      } else {
        check(state.name, true, 'no console, page or HTTP errors beyond the ones the mock explains');
      }

      if (state.group === DETAIL_GROUP && state.name === '05-trim-partial') {
        await auditContrast(page, state.name);
      }
      if (state.group === 'overcap') {
        await auditContrast(page, state.name);
      }
    }
  } finally {
    await browser.close();
    server.close();
  }

  // -------------------------------------------------------------------------------------
  // did any of it actually render?
  // -------------------------------------------------------------------------------------

  log('');
  log('blank-detection (decoded pixels, not file sizes)');
  log(
    '  ' +
      'file'.padEnd(34) +
      'bytes'.padStart(9) +
      'dims'.padStart(11) +
      'unique'.padStart(8) +
      'modal'.padStart(7) +
      'lum'.padStart(7) +
      '  verdict',
  );

  for (const file of shots) {
    const name = file.slice(shotsDir.length + 1);
    let stats;
    try {
      stats = pngStats(file);
    } catch (error) {
      failures.push(`${name}: ${error.message}`);
      log(`  ${name.padEnd(34)}unreadable: ${error.message}`);
      continue;
    }
    const problems = [];
    if (stats.bytes < MIN_BYTES) problems.push(`only ${stats.bytes} bytes`);
    if (stats.uniqueColors < MIN_UNIQUE_COLORS) problems.push(`only ${stats.uniqueColors} distinct colours (a uniform pane has 1)`);
    if (stats.modalShare > MAX_MODAL_SHARE) problems.push(`${(stats.modalShare * 100).toFixed(1)}% of pixels are one colour`);
    if (stats.meanLuminance > MAX_MEAN_LUMINANCE) problems.push(`mean luminance ${stats.meanLuminance.toFixed(3)} is not a dark window`);
    if (stats.transparentPixels > 0) problems.push(`${stats.transparentPixels} transparent pixels`);

    const verdict = problems.length === 0 ? 'OK' : `BLANK/SUSPECT: ${problems.join('; ')}`;
    if (problems.length > 0) failures.push(`${name}: ${problems.join('; ')}`);
    log(
      '  ' +
        name.padEnd(34) +
        `${(stats.bytes / 1024).toFixed(0)} KiB`.padStart(9) +
        `${stats.width}x${stats.height}`.padStart(11) +
        String(stats.uniqueColors).padStart(8) +
        `${(stats.modalShare * 100).toFixed(1)}%`.padStart(7) +
        stats.meanLuminance.toFixed(3).padStart(7) +
        `  ${verdict}`,
    );
    log(`    ${' '.repeat(34)}dominant colour ${stats.modalColor}`);
  }

  log('');
  log(`${shots.length} screenshots, ${checks} checks, ${failures.length} failure(s)`);
  for (const failure of failures) log(`  ✗ ${failure}`);
  log('');
  log('what these screenshots do NOT prove: the Tauri shell (no window was opened), the');
  log('asset protocol (media came from this script\'s static server), or any Rust behaviour');
  log('(every command returned a fixture). See the header of scripts/shots.mjs.');

  process.exit(failures.length === 0 ? 0 : 1);
}

/** Print the contrast table and fail on anything that is not readable. */
/**
 * The text every state has to keep readable, and which panel each piece lives in.
 *
 * The selectors are scoped to the panel they describe rather than to `.panel` alone: the
 * sidebar's first panel is the recorder (added with the recording UI), and
 * `document.querySelector` would otherwise have measured the recorder's footnote where the
 * label says "storage warning" — an audit that silently checks the wrong element.
 */
const CONTRAST_LABELS = {
  '[data-region=library] .view-head h2': 'view heading',
  '[data-region=library] .view-head .muted': 'clip count',
  '.name': 'clip name',
  '.sub.muted': 'clip metadata',
  '.chip': 'fact chip',
  '.storage .verdict.bad': 'storage warning',
  '.storage .verdict.ok': 'storage verdict',
  '.storage dt': 'panel label',
  '.storage dd': 'panel value',
  '.storage .footnote': 'panel footnote',
  '.timeline .legend .muted': 'selection length',
  '.timeline .playhead-readout': 'playhead readout',
  '.timeline .hint': 'timeline hint',
  '.panel.trim .selection .mono': 'trim range',
  '.panel.trim .lossless': 'lossless note',
};

/** Print the contrast table and fail on anything that is not readable. */
async function auditContrast(page, state, extra = {}) {
  const table = await contrastAudit(page, { ...CONTRAST_LABELS, ...extra });
  log('    contrast audit (WCAG 2.1 AA: 4.5 normal text, 3.0 large)');
  for (const row of table) {
    if (row.missing) {
      report(state, `${row.label}: not present in this state`);
      continue;
    }
    const ok = row.ratio >= row.threshold;
    check(
      state,
      ok,
      `${row.label}: ${row.ratio}:1 (${row.foreground} on ${row.background}, ${row.fontSize}px) — needs ${row.threshold}`,
    );
  }
}

main().catch((error) => {
  log(`  ! ${error?.stack ?? error}`);
  process.exit(1);
});
