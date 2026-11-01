// @vitest-environment jsdom
/**
 * The updates section of the settings panel.
 *
 * The panel is the whole of the feature: one button, one request, one answer. So the
 * tests here are mostly about *when* the request happens — it must not happen because the
 * window opened, and it must not happen twice because the panel was rendered again.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/plugin-updater', () => ({
  check: vi.fn(),
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
  convertFileSrc: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { check } from '@tauri-apps/plugin-updater';
import type { Update } from '@tauri-apps/plugin-updater';
import UpdaterPanel from './UpdaterPanel.svelte';
import type { RecordingStatus } from '../types';

const mockedCheck = vi.mocked(check);
const mockedInvoke = vi.mocked(invoke);

/** The shell's `prepare_for_update`, as each situation answers it. */
const NOT_RECORDING = { was_recording: false, stopped: false };
const REFUSED = { code: 'recording_active', message: 'a recording is running' };

/** The plugin's `Update` resource, as the plugin would hand it over. */
function fakeUpdate(overrides: Record<string, unknown> = {}): Update {
  return {
    rid: 1,
    currentVersion: '0.2.0',
    version: '0.3.0',
    date: '2026-10-20T10:00:00Z',
    body: 'A new release.',
    rawJson: {},
    available: true,
    download: vi.fn(),
    install: vi.fn(),
    downloadAndInstall: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
    ...overrides,
  } as unknown as Update;
}

/** The engine's counters: idle, or running when a test says so. */
function status(running = false): RecordingStatus {
  return {
    running, frames: 0, segments: 0, bytes: 0, span_ms: 0, dropped: 0, dropped_audio: 0,
    skipped: 0, fps: 0, configured_fps: 60, effective_fps: 0, drift_ms: 0, clips: 0, error: null,
    watching_games: false, matched_game: null, recorder_mode: 'buffer', mic_enabled: false, ended: null, stalled_for_ms: null,
  };
}

describe('the updates section', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
    mockedCheck.mockReset();
    mockedInvoke.mockReset();
    // Nothing is recording unless a test says so.
    mockedInvoke.mockResolvedValue(NOT_RECORDING);
  });

  async function settle() {
    for (let i = 0; i < 10; i++) {
      await tick();
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  async function mountPanel(recording: RecordingStatus | null = status()): Promise<HTMLElement> {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(UpdaterPanel, { target, props: { recording } });
    await settle();
    return target;
  }

  /** The panel's text, with the markup's own line breaks folded away. */
  function text(target: HTMLElement): string {
    return (target.textContent ?? '').replace(/\s+/g, ' ');
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => b.textContent?.trim() === label,
      ) ?? null
    );
  }

  it('asks nothing when the panel opens', async () => {
    await mountPanel();
    // The panel is on screen with its button; nothing was requested.
    expect(mockedCheck).not.toHaveBeenCalled();
  });

  it('asks once, when the button is pressed', async () => {
    mockedCheck.mockResolvedValue(null);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(mockedCheck).toHaveBeenCalledTimes(1);
  });

  it('says so when there is nothing newer', async () => {
    mockedCheck.mockResolvedValue(null);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    // The panel says there is nothing newer, as a status a screen reader announces, rather
    // than going back to looking as it did before the click.
    expect(target.querySelector('[role="status"]')?.textContent).toContain(
      'You are on the latest version.',
    );
    expect(text(target)).not.toContain('is available');
    expect(button(target, 'Check for updates')).not.toBeNull();
  });

  it('does not say it is up to date when the check failed', async () => {
    mockedCheck.mockRejectedValue(new Error('the endpoint did not answer'));
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(text(target)).not.toContain('latest version');
  });

  it('shows the release it found, with its notes and its date', async () => {
    mockedCheck.mockResolvedValue(fakeUpdate());
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(text(target)).toContain('0.3.0');
    expect(text(target)).toContain('you are running 0.2.0');
    expect(text(target)).toContain('Published 2026-10-20');
    expect(text(target)).toContain('A new release.');
    expect(button(target, 'Download and install')).not.toBeNull();
  });

  it('installs only when the user asks for it', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    // The update was found and nothing was downloaded: that is a second button's job.
    expect(update.downloadAndInstall).not.toHaveBeenCalled();

    button(target, 'Download and install')?.click();
    await settle();

    expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
  });

  it('says that the application has to be restarted to finish', async () => {
    mockedCheck.mockResolvedValue(fakeUpdate());
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();
    button(target, 'Download and install')?.click();
    await settle();

    expect(text(target)).toContain('Restart replayed to finish');
  });

  it('shows the failure instead of swallowing it', async () => {
    mockedCheck.mockRejectedValue(new Error('the endpoint did not answer'));
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(target.querySelector('[role="alert"]')?.textContent).toContain(
      'the endpoint did not answer',
    );
  });

  it('lets the update go, and releases what the backend was holding', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();
    button(target, 'Not now')?.click();
    await settle();

    expect(update.close).toHaveBeenCalledTimes(1);
    expect(button(target, 'Download and install')).toBeNull();
  });

  it('asks the shell before it downloads, without leave to stop the recording', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel();
    button(target, 'Check for updates')?.click();
    await settle();

    button(target, 'Download and install')?.click();
    await settle();

    expect(mockedInvoke).toHaveBeenCalledWith('prepare_for_update', { stop_recording: false });
    expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
    // In that order: the shell is asked first, so a refusal can stop the download.
    const asked = mockedInvoke.mock.invocationCallOrder[0] ?? Infinity;
    const downloaded = vi.mocked(update.downloadAndInstall).mock.invocationCallOrder[0] ?? 0;
    expect(asked).toBeLessThan(downloaded);
  });

  it('offers the install while a recording is running, and says it will ask first', async () => {
    // The shell, not the poll, decides whether a recording is in the way; the button is no
    // longer held, because the way to proceed is to ask the user.
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel(status(true));

    button(target, 'Check for updates')?.click();
    await settle();

    expect(button(target, 'Download and install')?.disabled).toBe(false);
    expect(text(target)).toContain('A recording is running');
    expect(text(target)).toContain('it will ask you');
    expect(update.downloadAndInstall).not.toHaveBeenCalled();
  });

  describe('when the shell says a recording is running', () => {
    async function refused() {
      const update = fakeUpdate();
      mockedCheck.mockResolvedValue(update);
      mockedInvoke.mockRejectedValue(REFUSED);
      const target = await mountPanel(status(true));
      button(target, 'Check for updates')?.click();
      await settle();
      button(target, 'Download and install')?.click();
      await settle();
      return { target, update };
    }

    it('asks the user, and downloads nothing while the question is open', async () => {
      const { target, update } = await refused();

      expect(text(target)).toContain('Stop the recording and install?');
      expect(button(target, 'Stop recording and install')).not.toBeNull();
      expect(button(target, 'Cancel')).not.toBeNull();
      expect(update.downloadAndInstall).not.toHaveBeenCalled();
      // The refusal is the question, not a failure to show in red.
      expect(target.querySelector('[role="alert"]')).toBeNull();
      // And it did not stop anything by itself.
      expect(mockedInvoke).not.toHaveBeenCalledWith('prepare_for_update', { stop_recording: true });
    });

    it('stops the recording and then installs, when the user agrees', async () => {
      const { target, update } = await refused();
      mockedInvoke.mockReset();
      mockedInvoke.mockResolvedValue({ was_recording: true, stopped: true });

      button(target, 'Stop recording and install')?.click();
      await settle();

      expect(mockedInvoke).toHaveBeenCalledWith('prepare_for_update', { stop_recording: true });
      expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
      expect(text(target)).toContain('Restart replayed to finish');
    });

    it('aborts cleanly when the user declines: no stop, no install, the offer stays', async () => {
      const { target, update } = await refused();
      mockedInvoke.mockClear();

      button(target, 'Cancel')?.click();
      await settle();

      expect(mockedInvoke).not.toHaveBeenCalled();
      expect(update.downloadAndInstall).not.toHaveBeenCalled();
      expect(button(target, 'Stop recording and install')).toBeNull();
      // Back where it was, and not wedged: the user can still install later, or let it go.
      expect(button(target, 'Download and install')?.disabled).toBe(false);
      expect(button(target, 'Not now')).not.toBeNull();
      expect(target.querySelector('[role="alert"]')).toBeNull();
    });

    it('does not install if stopping the recording failed, and says why', async () => {
      const { target, update } = await refused();
      mockedInvoke.mockReset();
      mockedInvoke.mockRejectedValue({ code: 'recording', message: 'the session could not be closed' });

      button(target, 'Stop recording and install')?.click();
      await settle();

      expect(update.downloadAndInstall).not.toHaveBeenCalled();
      expect(target.querySelector('[role="alert"]')?.textContent).toContain(
        'the session could not be closed',
      );
      expect(button(target, 'Download and install')).not.toBeNull();
    });

    it('says the recording was stopped if the download then fails', async () => {
      // The user agreed to lose the recording for an install that did not happen. Say so: the
      // recorder bar already shows it stopped, and an error that only blames the download would
      // leave them wondering where their recording went.
      const update = fakeUpdate({
        downloadAndInstall: vi.fn(async () => {
          throw new Error('the connection was reset');
        }),
      });
      mockedCheck.mockResolvedValue(update);
      mockedInvoke.mockRejectedValue(REFUSED);
      const target = await mountPanel(status(true));
      button(target, 'Check for updates')?.click();
      await settle();
      button(target, 'Download and install')?.click();
      await settle();
      mockedInvoke.mockReset();
      mockedInvoke.mockResolvedValue({ was_recording: true, stopped: true });

      button(target, 'Stop recording and install')?.click();
      await settle();

      const alert = target.querySelector('[role="alert"]')?.textContent ?? '';
      expect(alert).toContain('the connection was reset');
      expect(alert).toContain('recording was stopped');
    });
  });

  it('does not install when the shell cannot be asked: no answer is not an all-clear', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    mockedInvoke.mockRejectedValue({ code: 'recording', message: 'the recorder is busy' });
    const target = await mountPanel(null);

    button(target, 'Check for updates')?.click();
    await settle();
    button(target, 'Download and install')?.click();
    await settle();

    expect(update.downloadAndInstall).not.toHaveBeenCalled();
    expect(target.querySelector('[role="alert"]')?.textContent).toContain('the recorder is busy');
  });

  it('still lets the update be looked at and dismissed while recording', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel(status(true));

    button(target, 'Check for updates')?.click();
    await settle();

    expect(text(target)).toContain('0.3.0');
    expect(button(target, 'Not now')?.disabled).toBe(false);
  });
});
