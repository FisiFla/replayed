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
  watching_games: true, matched_game: null, recorder_mode: 'buffer', mic_enabled: false,
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
