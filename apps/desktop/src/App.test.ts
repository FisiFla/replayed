// @vitest-environment jsdom
/**
 * The window itself: the library it shows, and what happens when two refreshes of that
 * library overlap.
 *
 * The rest of the frontend is tested as pure functions over a fake source; what is tested
 * here is the wiring — that the window keeps the newest listing when an older one answers
 * after it.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import App from './App.svelte';
import type { ClipSource } from './lib/ipc';
import type {
  ClipDto,
  RecordedClip,
  RecordingStatus,
  SessionDto,
  SessionEvent,
  SettingsDto,
  StorageStats,
} from './lib/types';

// The real module reaches for the Tauri runtime, which does not exist under jsdom.
vi.mock('@tauri-apps/api/core', () => ({
  convertFileSrc: (p: string) => `asset://${p}`,
  invoke: () => Promise.resolve(undefined),
  // The updater plugin is imported by the settings panel, and it extends `Resource` and
  // builds a `Channel` at module level. The mock has to carry both, or the import throws
  // before a single test in this file runs.
  Resource: class {
    rid = 0;
    async close() {}
  },
  Channel: class {
    onmessage: ((message: unknown) => void) | null = null;
  },
}));

const clipA: ClipDto = {
  id: 1, kind: 'clip', path: '/c/a.mp4', started_at_ms: 0, duration_ms: 30_000, size_bytes: 5_000_000,
  codec: 'h264', favourite: false, title: null, game: 'Valorant', tags: [],
  created_at_ms: 1_700_000_000_000,
};
const clipB: ClipDto = { ...clipA, id: 2, path: '/c/b.mp4' };
const stats: StorageStats = {
  clip_count: 1, total_bytes: 5_000_000, missing_count: 0, missing_sessions: 0, favourite_count: 0, favourite_bytes: 0,
  cap_bytes: 10_000_000_000, max_age_days: 30, cap_met: true, over_cap_by_bytes: 0,
  planned_deletions: 0, bytes_after: 5_000_000, clips_dir: '/c', warnings: [],
};
const settings: SettingsDto = {
  clips_dir: '/c', clips_dir_resolved: '/c', fps: 60, output_size: '1080p', mic_enabled: false,
  mic_device: '', output_device: '', save_session: false,
  auto_record: true, clip_hotkey: 'Ctrl+F8', start_with_system: false, watch_games: [{ name: 'Valorant', exe: 'VALORANT-Win64-Shipping.exe', signal: 'process' }], defaulted: [], config_exists: true,
  config_path: '/cfg/config.toml', bitrate_kbps: 8000, quality: 'balanced', capture_monitor: 0,
  capture_window: '',
};
/** Running, so that the recorder bar offers to take a clip. */
const recording: RecordingStatus = {
  running: true, frames: 0, segments: 0, bytes: 0, span_ms: 0, dropped: 0, dropped_audio: 0,
  skipped: 0, fps: 0, configured_fps: 60, effective_fps: 0, drift_ms: 0, clips: 0, error: null,
  watching_games: true, matched_game: null, recorder_mode: 'buffer', mic_enabled: false, ended: null, stalled_for_ms: null,
};
const recorded: RecordedClip = {
  id: 2, path: '/c/b.mp4', duration_ms: 30_000, size_bytes: 5_000_000, codec: 'h264',
  started_at_ms: 0,
};

/** Explicit answers for what the window reads on open; any other command resolves to nothing. */
function fakeSource(overrides: Partial<ClipSource> = {}): ClipSource {
  const known: Record<string, unknown> = {
    listClips: () => Promise.resolve([clipA]),
    storageStats: () => Promise.resolve(stats),
    listSessions: () => Promise.resolve([]),
    sessionEvents: () => Promise.resolve([]),
    sessionMatches: () => Promise.resolve([]),
    recordingStatus: () => Promise.resolve(recording),
    clipNow: () => Promise.resolve(recorded),
    getSettings: () => Promise.resolve(settings),
    thumbnail: () => Promise.resolve({ clip_id: 1, path: '/t.jpg', at_ms: 0, cached: true }),
    appStatus: () =>
      Promise.resolve({
        hotkey: { chord: 'Ctrl+Shift+C', installed: true, error: null },
        config_path: '/cfg/config.toml', config_exists: true, close_hint: 'Closing hides to the tray.',
      }),
    health: () =>
      Promise.resolve({
        recorder_state: 'recording', recorder_error: null, encoder: null, ffmpeg_available: true,
        disk: null, event_sources: [], recent_errors: [], log_path: '/cfg/app.log',
      }),
    gsiTargets: () => Promise.resolve([]),
    listAudioDevices: () => Promise.resolve({ inputs: [], outputs: [] }),
    assetUrl: (p: string) => `asset://${p}`,
  };
  const target: Record<string, unknown> = { ...known, ...overrides };
  return new Proxy(target, {
    get: (t, k: string) => t[k] ?? (() => Promise.resolve(undefined)),
  }) as unknown as ClipSource;
}

async function settle() {
  for (let i = 0; i < 10; i++) {
    await tick();
    await new Promise((r) => setTimeout(r, 0));
  }
}

describe('the window', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  /** The clip rows the library is showing. */
  function rows(target: HTMLElement): Element[] {
    return [...target.querySelectorAll('[data-region="clip-grid"] button.row')];
  }

  it('keeps the newest library when two refreshes of it overlap', async () => {
    // The first listing is slow and the second is quick, so the older one answers last.
    let releaseFirst: (clips: ClipDto[]) => void = () => {};
    const first = new Promise<ClipDto[]>((resolve) => {
      releaseFirst = resolve;
    });
    let calls = 0;
    const source = fakeSource({
      listClips: () => {
        calls += 1;
        return calls === 1 ? first : Promise.resolve([clipA, clipB]);
      },
    });

    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source } });
    await settle();
    expect(rows(target)).toHaveLength(0);

    // A clip taken from the hotkey refreshes the library while the first listing is still
    // in flight, and its listing is the one that knows about the new clip.
    const save = [...target.querySelectorAll<HTMLButtonElement>('[data-region="recorder"] button')].find(
      (b) => b.textContent?.includes('Save clip'),
    );
    expect(save, 'the Save clip button').toBeDefined();
    save!.click();
    await settle();
    expect(rows(target)).toHaveLength(2);

    // The slow listing lands now, and it is the older of the two: it must not overwrite
    // what the newer one found, or the clip just saved vanishes from the library.
    releaseFirst([clipA]);
    await settle();
    expect(rows(target)).toHaveLength(2);
  });
});

/** A finished full session, as the store lists it. */
function sessionRow(id: number, overrides: Partial<SessionDto> = {}): SessionDto {
  return {
    id, game: `Game ${id}`, mode: 'session', started_at_ms: 1_700_000_000_000 + id,
    ended_at_ms: 1_700_000_600_000, final_path: `/s/${id}.mp4`, size_bytes: 9_000_000,
    favourite: false, scratch_dir: `/s/${id}`, duration_ms: 600_000, ...overrides,
  };
}

function marker(id: number): SessionEvent {
  return { id, kind: 'kill', offset_ms: id * 1000, payload: null, clip_id: null };
}

function mountApp(source: ClipSource): HTMLElement {
  const target = document.createElement('div');
  document.body.appendChild(target);
  appHandle.current = mount(App, { target, props: { source } });
  return target;
}

/** The mounted window, held outside the describe blocks so each can unmount it. */
const appHandle: { current: ReturnType<typeof mount> | undefined } = { current: undefined };

function navButton(target: HTMLElement, label: string): HTMLButtonElement {
  return [...target.querySelectorAll<HTMLButtonElement>('[data-region="rail"] button')].find(
    (b) => b.textContent?.includes(label),
  )!;
}

function recorderButton(target: HTMLElement, label: string): HTMLButtonElement {
  return [...target.querySelectorAll<HTMLButtonElement>('[data-region="recorder"] button')].find(
    (b) => b.textContent?.includes(label),
  )!;
}

describe('the window and the recorder', () => {
  afterEach(() => {
    if (appHandle.current) void unmount(appHandle.current);
    appHandle.current = undefined;
    document.body.innerHTML = '';
    vi.useRealTimers();
  });

  it('reads the sessions and the storage again after Stop, so the session is no longer "recording"', async () => {
    // A stop finalises the session row (its end, its length, its file) and the library's
    // totals. The window used to keep the list it read before: the session still said
    // "recording", extraction said "Stop it first" and the storage panel's numbers were old.
    const stopped: RecordingStatus = { ...recording, running: false };
    const listSessions = vi.fn(() => Promise.resolve([sessionRow(1)]));
    const storageStats = vi.fn(() => Promise.resolve(stats));
    const source = fakeSource({
      listSessions,
      storageStats,
      stopRecording: () => Promise.resolve(stopped),
    });
    const target = mountApp(source);
    await settle();
    const before = { sessions: listSessions.mock.calls.length, stats: storageStats.mock.calls.length };

    recorderButton(target, 'Stop recording').click();
    await settle();

    expect(listSessions.mock.calls.length).toBeGreaterThan(before.sessions);
    expect(storageStats.mock.calls.length).toBeGreaterThan(before.stats);
  });

  it('reads the sessions again after Start, which inserted the session row', async () => {
    const idle: RecordingStatus = { ...recording, running: false };
    const listSessions = vi.fn(() => Promise.resolve([]));
    const source = fakeSource({
      recordingStatus: () => Promise.resolve(idle),
      listSessions,
      startRecording: () => Promise.resolve(recording),
    });
    const target = mountApp(source);
    await settle();
    const before = listSessions.mock.calls.length;

    recorderButton(target, 'Start recording').click();
    await settle();

    expect(listSessions.mock.calls.length).toBeGreaterThan(before);
  });

  it('reads the library again when the recorder stops by itself, as a watched game exiting does', async () => {
    // Only the interval is faked: `settle` waits on real zero-delay timers.
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    let polls = 0;
    const listSessions = vi.fn(() => Promise.resolve([sessionRow(1)]));
    const source = fakeSource({
      // Recording on the first answer, and gone on every one after: nothing in the window
      // pressed Stop.
      recordingStatus: () => Promise.resolve({ ...recording, running: ++polls === 1 }),
      listSessions,
    });
    mountApp(source);
    await settle();
    const before = listSessions.mock.calls.length;

    await vi.advanceTimersByTimeAsync(500);
    await settle();

    expect(listSessions.mock.calls.length).toBeGreaterThan(before);
  });

  it('does not stack health reads behind one that never answers', async () => {
    // The backend's health command can stall; a `setInterval` that does not look queued another
    // IPC call behind it every five seconds, and the window froze.
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    const health = vi.fn(() => new Promise<never>(() => {}));
    mountApp(fakeSource({ health }));
    await settle();
    expect(health).toHaveBeenCalledTimes(1);

    for (let i = 0; i < 6; i++) await vi.advanceTimersByTimeAsync(5000);

    expect(health).toHaveBeenCalledTimes(1);
  });

  it('does not stack recorder reads behind one that never answers', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    const recordingStatus = vi.fn(() => new Promise<never>(() => {}));
    mountApp(fakeSource({ recordingStatus }));
    await settle();

    await vi.advanceTimersByTimeAsync(10_000);

    expect(recordingStatus).toHaveBeenCalledTimes(1);
  });
});

describe('the sessions view', () => {
  afterEach(() => {
    if (appHandle.current) void unmount(appHandle.current);
    appHandle.current = undefined;
    document.body.innerHTML = '';
  });

  it('shows the markers of the session that is open when another one’s answer arrives late', async () => {
    // Session 1 is clicked, then session 2 before 1's markers have come back. When they do, they
    // belong to a session that is no longer on screen: drawing them put session 1's markers on
    // session 2's timeline.
    let releaseOne: (events: SessionEvent[]) => void = () => {};
    const one = new Promise<SessionEvent[]>((resolve) => {
      releaseOne = resolve;
    });
    const source = fakeSource({
      listSessions: () => Promise.resolve([sessionRow(2), sessionRow(1)]),
      sessionEvents: (id: number) => (id === 1 ? one : Promise.resolve([marker(20)])),
    });
    const target = mountApp(source);
    await settle();
    navButton(target, 'Sessions').click();
    await settle();

    const rows = () => [...target.querySelectorAll<HTMLButtonElement>('[data-region="session-list"] button.row')];
    rows().find((r) => r.textContent?.includes('Game 1'))!.click();
    await settle();
    rows().find((r) => r.textContent?.includes('Game 2'))!.click();
    await settle();

    releaseOne([marker(1), marker(2), marker(3)]);
    await settle();

    const markers = [...target.querySelectorAll('[data-region="session-page"] dl.facts div')].find(
      (d) => d.querySelector('dt')?.textContent === 'Markers',
    );
    expect(markers?.querySelector('dd')?.textContent).toBe('1');
  });
});

describe('the library thumbnails', () => {
  afterEach(() => {
    if (appHandle.current) void unmount(appHandle.current);
    appHandle.current = undefined;
    document.body.innerHTML = '';
  });

  const thumb = { clip_id: 1, path: '/t.jpg', at_ms: 0, cached: true };

  it('says once that clips have no thumbnail, instead of a banner per clip', async () => {
    // Each failing clip used to replace the error banner with its own message, one after
    // another, and any recorder failure that was on screen with them.
    const clipC: ClipDto = { ...clipA, id: 3, path: '/c/c.mp4' };
    const source = fakeSource({
      listClips: () => Promise.resolve([clipA, clipB, clipC]),
      thumbnail: (id: number) =>
        id === 3
          ? Promise.resolve({ ...thumb, clip_id: 3 })
          : Promise.reject({ code: 'internal', message: `clip ${id} is unreadable` }),
    });
    const target = mountApp(source);
    await settle();

    expect(target.querySelector('.banner.error')).toBeNull();
    expect(target.querySelector('.banner.notice')?.textContent).toContain('2 clips have no thumbnail');
  });

  it('does not ask again, on the next refresh, for a thumbnail that could not be made', async () => {
    const thumbnail = vi.fn(() => Promise.reject({ code: 'internal', message: 'unreadable' }));
    const target = mountApp(fakeSource({ thumbnail }));
    await settle();
    expect(thumbnail).toHaveBeenCalledTimes(1);

    // A clip taken from the hotkey refreshes the library and asks for thumbnails again.
    recorderButton(target, 'Save clip').click();
    await settle();

    expect(thumbnail).toHaveBeenCalledTimes(1);
  });

  it('keeps ffmpeg missing as the one banner it always was, and stops asking', async () => {
    const thumbnail = vi.fn(() =>
      Promise.reject({ code: 'ffmpeg_unavailable', message: 'ffmpeg was not found' }),
    );
    const target = mountApp(
      fakeSource({ listClips: () => Promise.resolve([clipA, clipB]), thumbnail }),
    );
    await settle();

    expect(thumbnail).toHaveBeenCalledTimes(1);
    expect(target.querySelector('.banner.error')?.textContent).toContain('ffmpeg was not found');
  });

  it('runs one thumbnail pass at a time, so a clip is not decoded twice', async () => {
    // The mount's pass is still waiting on the first thumbnail when a hotkey clip refreshes
    // the library and asks again. Two passes both missed the cache for the same clip and
    // started two ffmpeg processes for it.
    let release: () => void = () => {};
    const slow = new Promise<typeof thumb>((resolve) => {
      release = () => resolve(thumb);
    });
    const thumbnail = vi.fn((id: number) => (id === 1 ? slow : Promise.resolve({ ...thumb, clip_id: id })));
    const target = mountApp(fakeSource({ thumbnail }));
    await settle();
    expect(thumbnail).toHaveBeenCalledTimes(1);

    recorderButton(target, 'Save clip').click();
    await settle();
    expect(thumbnail.mock.calls.filter(([id]) => id === 1)).toHaveLength(1);

    release();
    await settle();
    expect(thumbnail.mock.calls.filter(([id]) => id === 1)).toHaveLength(1);
  });
});
