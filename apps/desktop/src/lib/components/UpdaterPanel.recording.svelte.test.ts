// @vitest-environment jsdom
/**
 * The updates section when the recorder starts or stops while an update is on offer.
 *
 * The recording status is polled by the window and handed down as a prop. It is only a hint:
 * the shell's `prepare_for_update` is asked at the click and is the one that decides, so the
 * install button is never held on a reading that may be stale, and a recording that started
 * since the last poll still gets the user asked rather than cut off.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/plugin-updater', () => ({
  check: vi.fn(),
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async () => ({ was_recording: false, stopped: false })),
  convertFileSrc: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { check } from '@tauri-apps/plugin-updater';
import type { Update } from '@tauri-apps/plugin-updater';
import UpdaterPanel from './UpdaterPanel.svelte';
import type { RecordingStatus } from '../types';

function status(running: boolean): RecordingStatus {
  return {
    running, frames: 0, segments: 0, bytes: 0, span_ms: 0, dropped: 0, dropped_audio: 0,
    skipped: 0, fps: 0, configured_fps: 60, effective_fps: 0, drift_ms: 0, clips: 0, error: null,
    watching_games: false, matched_game: null, recorder_mode: 'buffer', mic_enabled: false, ended: null, stalled_for_ms: null,
  };
}

function fakeUpdate(): Update {
  return {
    rid: 1, currentVersion: '0.2.0', version: '0.3.0', date: '2026-10-20T10:00:00Z',
    body: 'A new release.', rawJson: {}, available: true,
    download: vi.fn(), install: vi.fn(),
    downloadAndInstall: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
  } as unknown as Update;
}

describe('the updates section as the recorder changes state', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
    vi.mocked(check).mockReset();
    vi.mocked(invoke).mockReset();
    vi.mocked(invoke).mockResolvedValue({ was_recording: false, stopped: false });
  });

  async function settle() {
    for (let i = 0; i < 10; i++) {
      await tick();
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  /** The panel's text, with the markup's own line breaks folded away. */
  function text(target: HTMLElement): string {
    return (target.textContent ?? '').replace(/\s+/g, ' ');
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement {
    return [...target.querySelectorAll<HTMLButtonElement>('button')].find(
      (b) => b.textContent?.trim() === label,
    )!;
  }

  async function offered(props: { recording: RecordingStatus }) {
    const update = fakeUpdate();
    vi.mocked(check).mockResolvedValue(update);
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(UpdaterPanel, {
      target,
      props: {
        get recording() {
          return props.recording;
        },
      },
    });
    await settle();
    button(target, 'Check for updates').click();
    await settle();
    return { target, update };
  }

  it('keeps the install offered as the recorder starts and stops, and leaves the decision to the shell', async () => {
    // The poll only drives the hint under the buttons. Whether a recording is in the way is the
    // shell's answer at the click, so the button is never held on the window's last reading.
    const props = $state({ recording: status(false) });
    const { target, update } = await offered(props);
    expect(button(target, 'Download and install').disabled).toBe(false);
    expect(text(target)).not.toContain('A recording is running');

    props.recording = status(true);
    await settle();
    expect(button(target, 'Download and install').disabled).toBe(false);
    expect(text(target)).toContain('A recording is running');

    props.recording = status(false);
    await settle();
    expect(text(target)).not.toContain('A recording is running');
    expect(update.downloadAndInstall).not.toHaveBeenCalled();
  });

  it('asks at the click even if the poll had not caught up with the recorder', async () => {
    const props = $state({ recording: status(false) });
    const { target, update } = await offered(props);

    // The recorder started between the last poll and the click: the window thinks it is idle,
    // the shell knows better and refuses, and the user is asked instead of the install running.
    vi.mocked(invoke).mockRejectedValue({ code: 'recording_active', message: 'a recording is running' });
    button(target, 'Download and install').click();
    await settle();

    expect(update.downloadAndInstall).not.toHaveBeenCalled();
    expect(text(target)).toContain('Stop the recording and install?');
  });
});
