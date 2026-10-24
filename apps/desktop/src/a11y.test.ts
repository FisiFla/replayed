// @vitest-environment jsdom
import axe from 'axe-core';
import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import App from './App.svelte';
import type { ClipSource } from './lib/ipc';
import type { EncoderSettings, EncoderStatus } from './lib/ipc';
import type {
  ClipDto,
  MatchDto,
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

const clip: ClipDto = {
  id: 1, path: '/c/a.mp4', started_at_ms: 0, duration_ms: 30_000, size_bytes: 5_000_000,
  codec: 'h264', favourite: false, title: null, game: 'Valorant', tags: [], created_at_ms: 1_700_000_000_000,
};
const session: SessionDto = {
  id: 1, game: 'Valorant', mode: 'session', started_at_ms: 1_700_000_000_000,
  ended_at_ms: 1_700_000_600_000, final_path: '/s/1.mp4', size_bytes: 9_000_000,
  favourite: false, scratch_dir: '/s/scratch', duration_ms: 600_000,
};
const sessionEvent: SessionEvent = {
  id: 1, kind: 'kill', offset_ms: 120_000, payload: null, clip_id: null,
};
const sessionMatch: MatchDto = {
  index: 0, start_ms: 0, end_ms: 600_000, complete: true, start_inferred: false,
  game_mode: 'Ranked', map: 'Ascent', result: 'Win',
  scoreboard: [
    { name: 'Player', champion: 'Jett', team: 'Blue', kills: 21, deaths: 12, assists: 5, cs: 240, ward_score: 3, is_local: true },
    { name: 'Other', champion: 'Sova', team: 'Red', kills: 14, deaths: 18, assists: 9, cs: 180, ward_score: 1, is_local: false },
  ],
  events: [sessionEvent],
};
const stats: StorageStats = {
  clip_count: 1, total_bytes: 5_000_000, missing_count: 0, missing_sessions: 0, favourite_count: 0, favourite_bytes: 0,
  cap_bytes: 10_000_000_000, max_age_days: 30, cap_met: true, over_cap_by_bytes: 0,
  planned_deletions: 0, bytes_after: 5_000_000, clips_dir: '/c', warnings: [],
};
const settings: SettingsDto = {
  clips_dir: '/c', clips_dir_resolved: '/c', fps: 60, output_size: '1080p', mic_enabled: false,
  mic_device: '', output_device: '', save_session: false,
  auto_record: true, start_with_system: false, watch_games: [{ name: 'Valorant', exe: 'VALORANT-Win64-Shipping.exe', signal: 'process' }], defaulted: [], config_exists: true,
  config_path: '/cfg/config.toml', bitrate_kbps: 8000, quality: 'balanced', capture_monitor: 0,
  capture_window: '',
};
const recording: RecordingStatus = {
  running: false, frames: 0, segments: 0, bytes: 0, span_ms: 0, dropped: 0, dropped_audio: 0,
  skipped: 0, fps: 0, configured_fps: 60, effective_fps: 0, drift_ms: 0, clips: 0, error: null,
  watching_games: true, matched_game: null, recorder_mode: 'buffer', mic_enabled: false,
};
const encoderSettings: EncoderSettings = {
  vendor: 'auto', codec: 'h264', bitrate_kbps: 20000, fps: 60, output_size: '', adapt_fps: true,
  zero_copy: null, rate_control: 'cbr', preset: '', audio_bitrate_kbps: 192,
};
const encoderStatus: EncoderStatus = {
  requested_vendor: 'auto', selected_vendor: 'nvenc', selected_codec: 'h264',
  capabilities: {
    rate_control_modes: ['cbr', 'vbr', 'quality'],
    presets: ['p1', 'p2', 'p3', 'p4', 'p5', 'p6', 'p7'],
    supports_scaling: true,
    max_bitrate_kbps: 100000,
  },
  message: 'the last recording used h264_nvenc: the startup smoke test chose nvenc.',
};

/** Explicit answers for what the window reads on open; any other command resolves to nothing. */
function fakeSource(): ClipSource {
  const known: Record<string, unknown> = {
    listClips: () => Promise.resolve([clip]),
    storageStats: () => Promise.resolve(stats),
    listSessions: () => Promise.resolve([session]),
    sessionEvents: () => Promise.resolve([sessionEvent]),
    sessionMatches: () => Promise.resolve([sessionMatch]),
    sessionDetail: () => Promise.resolve(session),
    getSettings: () => Promise.resolve(settings),
    recordingStatus: () => Promise.resolve(recording),
    appStatus: () =>
      Promise.resolve({
        hotkey: { chord: 'Ctrl+Shift+C', installed: true, error: null },
        config_path: '/cfg/config.toml', config_exists: true, close_hint: 'Closing hides to the tray.',
      }),
    thumbnail: () => Promise.resolve({ clip_id: 1, path: '/t.jpg', at_ms: 0, cached: true }),
    health: () =>
      Promise.resolve({
        recorder_state: 'idle', recorder_error: null, encoder: null, ffmpeg_available: true,
        disk: null, event_sources: [], recent_errors: [], log_path: '/cfg/app.log',
      }),
    encoderSettings: () => Promise.resolve(encoderSettings),
    encoderStatus: () => Promise.resolve(encoderStatus),
    copyDiagnostics: () =>
      Promise.resolve({
        version: '0.2.0', platform: 'windows 11 x86_64', generated_at: '2026-10-20 09:00:00',
        config: '[encode]\nvendor = "auto"', problems: [], log_tail: 'INFO all good',
      }),
    assetUrl: (p: string) => `asset://${p}`,
  };
  return new Proxy(known, {
    get: (t, k: string) => t[k] ?? (() => Promise.resolve(undefined)),
  }) as unknown as ClipSource;
}

/**
 * Arm the same test-only seam `main.ts` arms, so the panels *below* the window find this
 * fake too.
 *
 * `App` takes its source as a prop, but the encoder section and the diagnostics button are
 * their own components and resolve their own source (see `activeSource`), so without the
 * global they would read the mocked Tauri runtime — which answers nothing — and the encoder
 * controls would never be on the page for axe to check.
 */
window.history.replaceState({}, '', '/?test-clip-source=1');
(globalThis as { __replayedClipSource?: ClipSource }).__replayedClipSource = fakeSource();

async function settle() {
  for (let i = 0; i < 10; i++) {
    await tick();
    await new Promise((r) => setTimeout(r, 0));
  }
}

async function violations(root: HTMLElement) {
  const result = await axe.run(root, {
    // jsdom does no layout or painting, so contrast cannot be computed.
    rules: { 'color-contrast': { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`);
}

describe('accessibility (axe)', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  it('the main window has no axe violations', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    // Guard against a vacuous pass: the data really rendered.
    expect(target.textContent).toContain('Valorant');
    expect(await violations(target)).toEqual([]);
  });

  /** Mount the window, open one destination from the rail, and return its root. */
  async function openView(label: string) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    const item = [...target.querySelectorAll<HTMLButtonElement>('nav button')].find((b) =>
      b.textContent?.includes(label),
    );
    expect(item, `rail item ${label}`).toBeDefined();
    item!.click();
    await settle();
    expect(item!.getAttribute('aria-current')).toBe('page');
    return target;
  }

  it.each([
    ['Sessions', 'session-list'],
    ['Storage', 'storage'],
    ['Health', 'health'],
    ['Settings', 'settings'],
  ])('the %s view has no axe violations', async (label, region) => {
    const target = await openView(label);
    // Not vacuous: the region for this view really is on screen and has content.
    const el = target.querySelector(`[data-region="${region}"]`);
    expect(el?.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(await violations(target)).toEqual([]);
  });

  it('the encoder controls are really on the page, so the run above is not vacuous', async () => {
    const target = await openView('Settings');
    // The encoder section reads its own settings, so it renders its controls only when the
    // source answered. Without this, an axe run over an empty section would pass quietly.
    const encoder = target.querySelector('#settings-encoder');
    expect(encoder, 'the encoder section').not.toBeNull();
    expect(encoder?.querySelectorAll('select').length ?? 0).toBeGreaterThan(3);
    expect(encoder?.querySelectorAll('input[type="number"]').length ?? 0).toBe(2);
    expect(encoder?.querySelector('label')?.textContent?.trim()).toBeTruthy();
  });

  it('a clip page has no axe violations', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    target.querySelector<HTMLButtonElement>('[data-region="clip-grid"] button.row')?.click();
    await settle();
    expect(target.querySelector('[data-region="clip-page"]')).not.toBeNull();
    expect(await violations(target)).toEqual([]);
  });

  it('a session page has no axe violations', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    const item = [...target.querySelectorAll<HTMLButtonElement>('nav button')].find((b) =>
      b.textContent?.includes('Sessions'),
    );
    expect(item, 'rail item Sessions').toBeDefined();
    item!.click();
    await settle();
    // Open a session, so the detail pane really holds its markers, its match timeline and
    // its scoreboard — the parts of this window no other case looks at.
    const row = target.querySelector<HTMLButtonElement>('[data-region="session-list"] button.row');
    expect(row, 'a session in the list').not.toBeNull();
    row!.click();
    await settle();
    const page = target.querySelector('[data-region="session-page"]');
    // Not vacuous: the pane is really on screen and has the session's content in it.
    expect(page?.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(page?.querySelector('table.markers')).not.toBeNull();
    expect(page?.querySelector('table.score')).not.toBeNull();
    expect(await violations(target)).toEqual([]);
  });

  it('the harness does catch a violation', async () => {
    const target = document.createElement('div');
    target.innerHTML = '<img src="x.png"><button></button>';
    document.body.appendChild(target);
    const found = await violations(target);
    expect(found.some((v) => v.startsWith('image-alt'))).toBe(true);
    expect(found.some((v) => v.startsWith('button-name'))).toBe(true);
  });
});
