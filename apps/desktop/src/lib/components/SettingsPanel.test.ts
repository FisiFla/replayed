// @vitest-environment jsdom
/**
 * The automatic-recording section: one feature, two tiers, and the watch list that feeds it.
 *
 * What is under test is the part a screenshot cannot check: that the panel says which half of the
 * feature is off, that adding and removing a game produces exactly the list the backend would
 * write, and that a list the recorder would refuse cannot be applied — including the empty list,
 * which the backend reads as "the default titles", so a Remove button that emptied it would
 * silently bring six games back.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import SettingsPanel from './SettingsPanel.svelte';
import type { SettingsDto, SettingsUpdate, WatchGame } from '../types';

const LEAGUE: WatchGame = {
  name: 'League of Legends',
  exe: 'League of Legends.exe',
  signal: 'live_client_api',
};
const CS2: WatchGame = { name: 'Counter-Strike 2', exe: 'cs2.exe', signal: 'process' };

function settings(overrides: Partial<SettingsDto> = {}): SettingsDto {
  return {
    clips_dir: '',
    clips_dir_resolved: 'C:\\Users\\player\\replayed\\clips',
    fps: 60,
    output_size: '',
    mic_enabled: false,
    mic_device: '',
    output_device: '',
    save_session: false,
    auto_record: false,
    bitrate_kbps: 20_000,
    quality: 'balanced',
    capture_monitor: 0,
    capture_window: '',
    watch_games: [LEAGUE, CS2],
    defaulted: [],
    config_exists: true,
    config_path: 'C:\\Users\\player\\replayed\\config.toml',
    ...overrides,
  };
}

describe('the automatic-recording section', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  async function settle() {
    for (let i = 0; i < 10; i++) {
      await tick();
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  async function mountPanel(value: SettingsDto, onApply = vi.fn()) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(SettingsPanel, {
      target,
      props: {
        settings: value,
        recording: null,
        busy: false,
        onApply,
        onChooseClipsDir: () => Promise.resolve(''),
      },
    });
    await settle();
    return { target, onApply };
  }

  function text(target: HTMLElement): string {
    return (target.textContent ?? '').replace(/\s+/g, ' ');
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => (b.textContent ?? '').replace(/\s+/g, ' ').trim() === label,
      ) ?? null
    );
  }

  /** The input of the field whose label starts with `label`. */
  function field(target: HTMLElement, label: string): HTMLInputElement | null {
    const wrapper = [...target.querySelectorAll<HTMLElement>('label.field')].find((l) =>
      (l.querySelector('span')?.textContent ?? '').trim().startsWith(label),
    );
    return wrapper?.querySelector('input') ?? null;
  }

  function listed(target: HTMLElement): string[] {
    return [...target.querySelectorAll('ul.watch li .watch-name')].map(
      (li) => li.textContent?.trim() ?? '',
    );
  }

  function applyAutomatic(target: HTMLElement): HTMLButtonElement | null {
    return button(target, 'Apply automatic recording');
  }

  it('lists every watched game with its executable', async () => {
    const { target } = await mountPanel(settings());
    expect(listed(target)).toEqual(['League of Legends', 'Counter-Strike 2']);
    expect(text(target)).toContain('Live Client API');
  });

  it('says which half of the feature is off when auto-record has no session save', async () => {
    const { target } = await mountPanel(settings({ auto_record: true, save_session: false }));
    const warning = target.querySelector('.warn-note')?.textContent ?? '';
    expect(warning.replace(/\s+/g, ' ')).toContain('clips only');
  });

  it('does not warn when the full session is kept as well', async () => {
    const { target } = await mountPanel(settings({ auto_record: true, save_session: true }));
    expect(target.querySelector('.warn-note')).toBeNull();
  });

  it('adds a game with the executable reduced to its file name, then applies the list', async () => {
    const { target, onApply } = await mountPanel(settings());
    field(target, 'Executable')!.value = 'C:\\Games\\RainbowSix.exe';
    field(target, 'Executable')!.dispatchEvent(new Event('input', { bubbles: true }));
    // A flush before the click: the Add button is disabled while the field is empty, and the
    // disabled attribute is only cleared once Svelte has re-rendered.
    await settle();
    button(target, 'Add game')!.click();
    await settle();

    // The name defaulted to the executable's stem, and the pasted directory is gone.
    expect(listed(target)).toContain('RainbowSix');
    applyAutomatic(target)!.click();
    await settle();

    const update: SettingsUpdate = onApply.mock.calls[0]![0];
    expect(update.watch_games?.map((game) => game.exe)).toEqual([
      'League of Legends.exe',
      'cs2.exe',
      'RainbowSix.exe',
    ]);
    // Toggles were not touched, so they are not in the update.
    expect(update.auto_record).toBeUndefined();
    expect(update.save_session).toBeUndefined();
  });

  it('removes an entry without touching the file until applied', async () => {
    const { target, onApply } = await mountPanel(settings());
    target.querySelector<HTMLButtonElement>('button[aria-label="Stop watching Counter-Strike 2"]')!
      .click();
    await settle();

    expect(listed(target)).toEqual(['League of Legends']);
    expect(onApply).not.toHaveBeenCalled();
    applyAutomatic(target)!.click();
    await settle();
    const update: SettingsUpdate = onApply.mock.calls[0]![0];
    expect(update.watch_games?.map((game) => game.exe)).toEqual(['League of Legends.exe']);
  });

  it('refuses a list the recorder would refuse, and says why', async () => {
    const { target } = await mountPanel(settings());
    field(target, 'Executable')!.value = 'CS2.EXE';
    field(target, 'Executable')!.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    button(target, 'Add game')!.click();
    await settle();

    expect(text(target)).toContain('watched twice');
    expect(applyAutomatic(target)!.disabled).toBe(true);
  });

  it('refuses to empty the list, because that brings the default titles back', async () => {
    const { target } = await mountPanel(settings());
    target.querySelector<HTMLButtonElement>('button[aria-label="Stop watching Counter-Strike 2"]')!
      .click();
    await settle();
    target.querySelector<HTMLButtonElement>('button[aria-label="Stop watching League of Legends"]')!
      .click();
    await settle();

    expect(listed(target)).toEqual([]);
    expect(text(target)).toContain('cannot be empty');
    expect(applyAutomatic(target)!.disabled).toBe(true);
  });

  it('sends the toggles alone when only a toggle changed', async () => {
    const { target, onApply } = await mountPanel(settings());
    const toggles = [...target.querySelectorAll<HTMLInputElement>('input[type=checkbox]')];
    // The automatic section's two checkboxes are the last two of the panel.
    const autoRecord = toggles[toggles.length - 2]!;
    autoRecord.checked = true;
    autoRecord.dispatchEvent(new Event('change', { bubbles: true }));
    await settle();
    applyAutomatic(target)!.click();
    await settle();

    expect(onApply.mock.calls[0]![0]).toEqual({ auto_record: true });
  });
});
