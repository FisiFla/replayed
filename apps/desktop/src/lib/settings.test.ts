/**
 * The settings notice: what the panel reports after an apply.
 *
 * Same reasoning as the other `*.test.ts` files in this directory: the window cannot
 * be opened here, so the words it shows are pinned here.
 */

import { describe, expect, it } from 'vitest';
import {
  automaticUpdate,
  automaticWarning,
  captureUpdate,
  defaultWatchName,
  describeSettingsUpdate,
  deviceOptions,
  monitorIndex,
  sameWatchList,
  watchListProblem,
} from './settings';
import type { SettingsDto, UpdateSettingsOutcome } from './types';

function settings(overrides: Partial<SettingsDto> = {}): SettingsDto {
  return {
    clips_dir: '',
    clips_dir_resolved: 'C:\\Users\\player\\replayed\\clips',
    fps: 60,
    output_size: '',
    mic_enabled: false,
    auto_record: false,
    start_with_system: false,
    bitrate_kbps: 20000,
    quality: 'balanced',
    capture_monitor: 0,
    capture_window: '',
    mic_device: '',
    output_device: '',
    save_session: false,
    watch_games: [{ name: 'League of Legends', exe: 'League of Legends.exe', signal: 'live_client_api' }],
    defaulted: [],
    config_exists: true,
    config_path: 'C:\\Users\\player\\replayed\\config.toml',
    ...overrides,
  };
}

function outcome(overrides: Partial<UpdateSettingsOutcome> = {}): UpdateSettingsOutcome {
  return {
    settings: settings(),
    applied: [],
    restart_required: false,
    restart_reason: null,
    autostart: null,
    ...overrides,
  };
}

describe('the settings notice', () => {
  it('says nothing changed when nothing did', () => {
    expect(describeSettingsUpdate(outcome())).toBe('No changes.');
  });

  it('names the keys that were saved', () => {
    expect(
      describeSettingsUpdate(outcome({ applied: ['encode.fps', 'mic.enabled'] })),
    ).toBe('Saved encode.fps, mic.enabled.');
  });

  it('carries the restart sentence the backend wrote', () => {
    expect(
      describeSettingsUpdate(
        outcome({
          applied: ['encode.fps'],
          restart_required: true,
          restart_reason: 'a recording is running: encode.fps takes effect now',
        }),
      ),
    ).toBe('Saved encode.fps. a recording is running: encode.fps takes effect now');
  });
});

describe('the capture drafts', () => {
  const saved = settings();
  const same = { quality: 'balanced', monitor: 0, window: '', micDevice: '', outputDevice: '' };

  it('writes nothing for an untouched panel', () => {
    expect(captureUpdate(saved, same)).toEqual({});
  });

  it('sends only the keys that changed, trimming the window title', () => {
    expect(captureUpdate(saved, { ...same, quality: 'high', monitor: 1, window: '  League  ' })).toEqual({
      quality: 'high',
      capture_monitor: 1,
      capture_window: 'League',
    });
  });

  it('never overwrites a custom bitrate with the placeholder option', () => {
    const custom = settings({ quality: 'custom', bitrate_kbps: 12000 });
    expect(captureUpdate(custom, { ...same, quality: 'custom' })).toEqual({});
  });

  it('clears the window with an empty title', () => {
    const windowed = settings({ capture_window: 'Game' });
    expect(captureUpdate(windowed, { ...same, window: '' })).toEqual({ capture_window: '' });
  });

  it('sends a changed device and clears it back to the Windows default', () => {
    expect(captureUpdate(saved, { ...same, micDevice: '{mic-1}', outputDevice: '{out-1}' })).toEqual({
      mic_device: '{mic-1}',
      output_device: '{out-1}',
    });
    const chosen = settings({ mic_device: '{mic-1}' });
    expect(captureUpdate(chosen, { ...same, micDevice: '' })).toEqual({ mic_device: '' });
  });

  it('keeps a saved device that is not connected selectable as itself', () => {
    const device = { id: '{a}', name: 'Blue Yeti', is_default: true };
    expect(deviceOptions([device], '')).toEqual([
      { value: '', label: 'Windows default' },
      { value: '{a}', label: 'Blue Yeti (current default)' },
    ]);
    expect(deviceOptions([], '{gone}')).toEqual([
      { value: '', label: 'Windows default' },
      { value: '{gone}', label: 'Saved device (not connected)' },
    ]);
  });

  it('parses a monitor index only as a whole non-negative number', () => {
    expect(monitorIndex(' 2 ')).toBe(2);
    expect(monitorIndex('-1')).toBeNull();
    expect(monitorIndex('1.5')).toBeNull();
    expect(monitorIndex('')).toBeNull();
  });
});

/** A watch entry, with the panel's own defaults. */
function watch(name: string, exe: string, signal = 'process') {
  return { name, exe, signal };
}

describe('the settings notice', () => {
  it('says what the registry did, after what the file says', () => {
    const message = describeSettingsUpdate(
      outcome({
        applied: ['app.start_with_system'],
        autostart: {
          applied: false,
          detail: 'this build is not Windows and has nothing to register',
        },
      }),
    );
    expect(message).toContain('Saved app.start_with_system.');
    expect(message).toContain('nothing to register');
  });

  it('adds nothing when no autostart change was made', () => {
    expect(describeSettingsUpdate(outcome({ applied: ['encode.fps'] }))).toBe('Saved encode.fps.');
  });
});

describe('the watch list', () => {
  it('names a new entry from the executable when the user gives no name', () => {
    expect(defaultWatchName('RainbowSix.exe')).toBe('RainbowSix');
    expect(defaultWatchName('  C:\\Games\\VALORANT-Win64-Shipping.exe ')).toBe(
      'VALORANT-Win64-Shipping',
    );
    // A file name with no extension is its own name; nothing is invented.
    expect(defaultWatchName('something')).toBe('something');
  });

  it('accepts a list the recorder would accept', () => {
    expect(watchListProblem([watch('R6', 'RainbowSix.exe'), watch('Valorant', 'x.exe')])).toBeNull();
  });

  it('refuses an empty list, because empty means the defaults come back', () => {
    const problem = watchListProblem([]);
    expect(problem).toContain('cannot be empty');
    expect(problem).toContain('default');
  });

  it('refuses an entry whose executable cannot match a running game', () => {
    expect(watchListProblem([watch('R6', '   ')])).toContain('no executable name');
    expect(watchListProblem([watch('R6', 'RainbowSix')])).toContain('does not end in .exe');
  });

  it('refuses the same executable twice, case-insensitively', () => {
    const problem = watchListProblem([watch('Dota 2', 'dota2.exe'), watch('Dota 2 again', 'DOTA2.EXE')]);
    expect(problem).toContain('watched twice');
  });

  it('compares two lists entry for entry, in order', () => {
    const a = [watch('R6', 'RainbowSix.exe')];
    expect(sameWatchList(a, [watch('R6', 'RainbowSix.exe')])).toBe(true);
    expect(sameWatchList(a, [watch('R6', 'RainbowSix.exe', 'live_client_api')])).toBe(false);
    expect(sameWatchList(a, [watch('R6', 'RainbowSix.exe'), watch('X', 'x.exe')])).toBe(false);
  });
});

describe('applying the automatic-recording section', () => {
  const draft = {
    autoRecord: false,
    saveSession: false,
    watch: [watch('League of Legends', 'League of Legends.exe', 'live_client_api')],
  };

  it('sends nothing when nothing changed', () => {
    expect(automaticUpdate(settings(), draft)).toEqual({});
  });

  it('sends only the toggle that changed, not the whole section', () => {
    expect(automaticUpdate(settings(), { ...draft, autoRecord: true })).toEqual({
      auto_record: true,
    });
    expect(automaticUpdate(settings(), { ...draft, saveSession: true })).toEqual({
      save_session: true,
    });
  });

  it('sends the whole list when an entry changed, because the backend replaces it', () => {
    const next = [...draft.watch, watch('R6', 'RainbowSix.exe')];
    expect(automaticUpdate(settings(), { ...draft, watch: next })).toEqual({
      watch_games: next,
    });
  });
});

describe('the automatic-recording warning', () => {
  it('says what is lost when auto-record is on and nothing is saved', () => {
    const warning = automaticWarning({ autoRecord: true, saveSession: false });
    expect(warning).toContain('clips only');
    expect(warning).toContain('match itself is not kept');
  });

  it('is silent when both are on, or when automatic recording is off', () => {
    expect(automaticWarning({ autoRecord: true, saveSession: true })).toBeNull();
    expect(automaticWarning({ autoRecord: false, saveSession: false })).toBeNull();
  });
});
