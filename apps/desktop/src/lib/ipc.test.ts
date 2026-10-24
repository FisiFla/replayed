/**
 * The IPC layer, driven against a mocked Tauri runtime.
 *
 * The Tauri `invoke`/`convertFileSrc` calls are the one boundary in the frontend that
 * neither TypeScript nor Rust can check for the other side. A command renamed in Rust, or
 * an argument sent as `startMs` where the command declares `start_ms`, fails at runtime —
 * in a window nobody is watching. So this file asserts the boundary itself:
 *
 * 1. the exact command names and argument keys that go over the wire, and
 * 2. that `ipc.ts` and `src-tauri/src/lib.rs` agree — the names match the
 *    `generate_handler!` list, and the argument keys match the Rust parameters, which are
 *    read straight out of the Rust source.
 */

import { readFileSync } from 'node:fs';
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async () => ({})),
  convertFileSrc: vi.fn((path: string) => `asset://localhost/${encodeURIComponent(path)}`),
}));

import { convertFileSrc, invoke } from '@tauri-apps/api/core';
import { errorCode, errorMessage, isCommandError, tauriIpc } from './ipc';
import type { EncoderSettings } from './ipc';

const mockedInvoke = vi.mocked(invoke);
const mockedConvert = vi.mocked(convertFileSrc);

const LIB_RS = readFileSync(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');

/** The encoder settings the wire assertions below travel with. */
const ENCODER_SETTINGS: EncoderSettings = {
  vendor: 'nvenc',
  codec: 'hevc',
  bitrate_kbps: 20000,
  fps: 60,
  output_size: '1920x1080',
  adapt_fps: true,
  zero_copy: null,
  rate_control: 'cbr',
  preset: 'p4',
  audio_bitrate_kbps: 192,
};

/** The command names registered in `generate_handler![...]`. */
function registeredCommands(): string[] {
  const block = /generate_handler!\[([\s\S]*?)\]/.exec(LIB_RS)?.[1] ?? '';
  return block
    .split(',')
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
}

/** The parameter names of a `#[tauri::command]` function, without the ones the
 * runtime injects: `State` (the application state) and `AppHandle` (for the asset
 * scope) are not sent over `invoke`, so the frontend must not send them either. */
function rustParameters(command: string): string[] {
  const body = new RegExp(`fn ${command}\\(([\\s\\S]*?)\\)\\s*->`).exec(LIB_RS)?.[1] ?? '';
  // `:[^:]` — a path like `commands::SettingsUpdate` must not read as a parameter
  // named `commands`.
  return [...body.matchAll(/(\w+)\s*:(?!:)/g)]
    .map((match) => match[1] ?? '')
    .filter((name) => name !== 'state' && name !== 'app' && name !== 'window');
}

/** Run one IPC call and return the first argument `invoke` was handed. */
async function wireName(call: () => Promise<unknown>): Promise<string> {
  mockedInvoke.mockClear();
  await call();
  const [name] = mockedInvoke.mock.calls[0] ?? [];
  return String(name);
}

/** Run one IPC call and return the second argument `invoke` was handed. */
async function wireArgs(call: () => Promise<unknown>): Promise<Record<string, unknown>> {
  mockedInvoke.mockClear();
  await call();
  const [, args] = mockedInvoke.mock.calls[0] ?? [];
  return (args ?? {}) as Record<string, unknown>;
}

beforeEach(() => {
  mockedInvoke.mockClear();
  mockedConvert.mockClear();
});

describe('the command names', () => {
  it('calls each command by the name the Rust side registers', async () => {
    expect(await wireName(() => tauriIpc.listClips())).toBe('list_clips');
    expect(await wireName(() => tauriIpc.storageStats())).toBe('storage_stats');
    expect(await wireName(() => tauriIpc.setFavourite(1, true))).toBe('set_favourite');
    expect(await wireName(() => tauriIpc.setClipTitle(1, 'x'))).toBe('set_clip_title');
    expect(await wireName(() => tauriIpc.setClipTags(1, ['x']))).toBe('set_clip_tags');
    expect(await wireName(() => tauriIpc.copyClipToClipboard(1))).toBe('copy_clip_to_clipboard');
    expect(await wireName(() => tauriIpc.revealClip(1))).toBe('reveal_clip');
    expect(await wireName(() => tauriIpc.dragClipOut(1))).toBe('drag_clip_out');
    expect(await wireName(() => tauriIpc.trimClip(1, 0, 100))).toBe('trim_clip');
    expect(await wireName(() => tauriIpc.thumbnail(1, 0))).toBe('thumbnail');
    expect(await wireName(() => tauriIpc.deleteClip(1))).toBe('delete_clip');
    expect(await wireName(() => tauriIpc.pruneMissing())).toBe('prune_missing');
    expect(await wireName(() => tauriIpc.combineClips([1, 2]))).toBe('combine_clips');
    expect(await wireName(() => tauriIpc.exportClip(1, 'gif'))).toBe('export_clip');
    expect(await wireName(() => tauriIpc.openDataDir())).toBe('open_data_dir');
    expect(await wireName(() => tauriIpc.screenshotNow())).toBe('screenshot_now');
    expect(await wireName(() => tauriIpc.listSessions())).toBe('list_sessions');
    expect(await wireName(() => tauriIpc.sessionDetail(1))).toBe('session_detail');
    expect(await wireName(() => tauriIpc.sessionEvents(1))).toBe('session_events');
    expect(await wireName(() => tauriIpc.sessionMatches(1))).toBe('session_matches');
    expect(await wireName(() => tauriIpc.extractMatch(1, 0))).toBe('extract_match');
    expect(await wireName(() => tauriIpc.setSessionFavourite(1, true))).toBe(
      'set_session_favourite',
    );
    expect(await wireName(() => tauriIpc.deleteSession(1))).toBe('delete_session');
    expect(await wireName(() => tauriIpc.extractClip(1, 0, 1_000))).toBe('extract_clip');
    expect(await wireName(() => tauriIpc.startRecording())).toBe('start_recording');
    expect(await wireName(() => tauriIpc.stopRecording())).toBe('stop_recording');
    expect(await wireName(() => tauriIpc.recordingStatus())).toBe('recording_status');
    expect(await wireName(() => tauriIpc.clipNow())).toBe('clip_now');
    expect(await wireName(() => tauriIpc.appStatus())).toBe('app_status');
    expect(await wireName(() => tauriIpc.getSettings())).toBe('get_settings');
    expect(await wireName(() => tauriIpc.updateSettings({ fps: 30 }))).toBe('update_settings');
    expect(await wireName(() => tauriIpc.health())).toBe('health');
    expect(await wireName(() => tauriIpc.gsiTargets())).toBe('gsi_targets');
    expect(await wireName(() => tauriIpc.listAudioDevices())).toBe('list_audio_devices');
    expect(await wireName(() => tauriIpc.installGsiCfg('dota2'))).toBe('install_gsi_cfg');
    expect(await wireName(() => tauriIpc.encoderSettings())).toBe('encoder_settings');
    expect(await wireName(() => tauriIpc.encoderStatus())).toBe('encoder_status');
    expect(await wireName(() => tauriIpc.captureDisplays())).toBe('capture_displays');
    expect(await wireName(() => tauriIpc.copyDiagnostics())).toBe('copy_diagnostics');
    expect(await wireName(() => tauriIpc.updateEncoderSettings(ENCODER_SETTINGS))).toBe(
      'update_encoder_settings',
    );
  });

  it('uses every command the Rust side registers, and invents none', async () => {
    // The commands the methods above actually reached, compared against the handler list
    // in src-tauri/src/lib.rs. A command added on one side and not the other fails here
    // rather than as "Command not found" in a window.
    const reached = [
      await wireName(() => tauriIpc.listClips()),
      await wireName(() => tauriIpc.storageStats()),
      await wireName(() => tauriIpc.setFavourite(1, true)),
      await wireName(() => tauriIpc.setClipTitle(1, 'x')),
      await wireName(() => tauriIpc.setClipTags(1, ['x'])),
      await wireName(() => tauriIpc.copyClipToClipboard(1)),
      await wireName(() => tauriIpc.revealClip(1)),
      await wireName(() => tauriIpc.dragClipOut(1)),
      await wireName(() => tauriIpc.trimClip(1, 0, 100)),
      await wireName(() => tauriIpc.thumbnail(1, 0)),
      await wireName(() => tauriIpc.deleteClip(1)),
      await wireName(() => tauriIpc.pruneMissing()),
      await wireName(() => tauriIpc.combineClips([1, 2])),
      await wireName(() => tauriIpc.exportClip(1, 'gif')),
      await wireName(() => tauriIpc.openDataDir()),
      await wireName(() => tauriIpc.screenshotNow()),
      await wireName(() => tauriIpc.listSessions()),
      await wireName(() => tauriIpc.sessionDetail(1)),
      await wireName(() => tauriIpc.sessionEvents(1)),
      await wireName(() => tauriIpc.sessionMatches(1)),
      await wireName(() => tauriIpc.extractMatch(1, 0)),
      await wireName(() => tauriIpc.setSessionFavourite(1, true)),
      await wireName(() => tauriIpc.deleteSession(1)),
      await wireName(() => tauriIpc.extractClip(1, 0, 1_000)),
      await wireName(() => tauriIpc.startRecording()),
      await wireName(() => tauriIpc.stopRecording()),
      await wireName(() => tauriIpc.recordingStatus()),
      await wireName(() => tauriIpc.clipNow()),
      await wireName(() => tauriIpc.appStatus()),
      await wireName(() => tauriIpc.getSettings()),
      await wireName(() => tauriIpc.updateSettings({ fps: 30 })),
      await wireName(() => tauriIpc.chooseClipsDir()),
      await wireName(() => tauriIpc.health()),
      await wireName(() => tauriIpc.gsiTargets()),
      await wireName(() => tauriIpc.listAudioDevices()),
      await wireName(() => tauriIpc.installGsiCfg('cs2')),
      await wireName(() => tauriIpc.logFromFrontend('info', 'a message', 'a detail')),
      await wireName(() => tauriIpc.encoderSettings()),
      await wireName(() => tauriIpc.updateEncoderSettings(ENCODER_SETTINGS)),
      await wireName(() => tauriIpc.encoderStatus()),
      await wireName(() => tauriIpc.captureDisplays()),
      await wireName(() => tauriIpc.copyDiagnostics()),
    ];

    expect([...reached].sort()).toEqual([...registeredCommands()].sort());
  });
});

describe('the argument keys', () => {
  it('sends only the game to install_gsi_cfg, never a path', async () => {
    expect(await wireArgs(() => tauriIpc.installGsiCfg('cs2'))).toEqual({ game: 'cs2' });
  });

  it('sends snake_case, because the commands declare rename_all = "snake_case"', async () => {
    // Tauri's default is camelCase for command arguments; these commands opt out of it, so
    // `start_ms` — not `startMs` — is what the Rust side expects. This is exactly the kind
    // of mismatch that no compiler catches.
    expect(await wireArgs(() => tauriIpc.trimClip(3, 1_000, 2_000))).toEqual({
      id: 3,
      start_ms: 1_000,
      end_ms: 2_000,
    });
    expect(await wireArgs(() => tauriIpc.thumbnail(3, 1_000))).toEqual({
      id: 3,
      at_ms: 1_000,
    });
  });

  it('sends the whole encoder settings table under one key', async () => {
    // `update_encoder_settings` takes one argument — the settings object — and the object
    // keeps its snake_case keys: the backend reads it into a Rust struct whose fields are
    // named the same way, so `bitrate_kbps` must not travel as `bitrateKbps`.
    const args = await wireArgs(() => tauriIpc.updateEncoderSettings(ENCODER_SETTINGS));
    expect(Object.keys(args)).toEqual(['settings']);
    expect(args.settings).toEqual(ENCODER_SETTINGS);
  });

  it('sends exactly the parameters each Rust command declares', async () => {
    // The recording commands take none: every one of them is answered from the shell's own
    // state, so their arguments are the empty set and the Rust side must declare no
    // parameters either.
    const sent = {
      set_favourite: await wireArgs(() => tauriIpc.setFavourite(3, true)),
      set_clip_title: await wireArgs(() => tauriIpc.setClipTitle(3, 'x')),
      set_clip_tags: await wireArgs(() => tauriIpc.setClipTags(3, ['x'])),
      copy_clip_to_clipboard: await wireArgs(() => tauriIpc.copyClipToClipboard(3)),
      reveal_clip: await wireArgs(() => tauriIpc.revealClip(3)),
      drag_clip_out: await wireArgs(() => tauriIpc.dragClipOut(3)),
      trim_clip: await wireArgs(() => tauriIpc.trimClip(3, 1_000, 2_000)),
      thumbnail: await wireArgs(() => tauriIpc.thumbnail(3, 1_000)),
      delete_clip: await wireArgs(() => tauriIpc.deleteClip(3)),
      list_sessions: await wireArgs(() => tauriIpc.listSessions()),
      session_detail: await wireArgs(() => tauriIpc.sessionDetail(3)),
      session_events: await wireArgs(() => tauriIpc.sessionEvents(3)),
      session_matches: await wireArgs(() => tauriIpc.sessionMatches(3)),
      extract_match: await wireArgs(() => tauriIpc.extractMatch(3, 0)),
      set_session_favourite: await wireArgs(() => tauriIpc.setSessionFavourite(3, true)),
      delete_session: await wireArgs(() => tauriIpc.deleteSession(3)),
      extract_clip: await wireArgs(() => tauriIpc.extractClip(3, 1_000, 2_000)),
      start_recording: await wireArgs(() => tauriIpc.startRecording()),
      stop_recording: await wireArgs(() => tauriIpc.stopRecording()),
      recording_status: await wireArgs(() => tauriIpc.recordingStatus()),
      clip_now: await wireArgs(() => tauriIpc.clipNow()),
      get_settings: await wireArgs(() => tauriIpc.getSettings()),
      update_settings: await wireArgs(() => tauriIpc.updateSettings({ fps: 30 })),
      update_encoder_settings: await wireArgs(() =>
        tauriIpc.updateEncoderSettings(ENCODER_SETTINGS),
      ),
      capture_displays: await wireArgs(() => tauriIpc.captureDisplays()),
    };

    for (const [command, args] of Object.entries(sent)) {
      const declared = rustParameters(command);
      expect(Object.keys(args).sort(), `arguments for ${command}`).toEqual(declared.sort());
    }
  });

  it('sends no arguments to the commands that take none', async () => {
    expect(mockedInvoke).toHaveBeenCalledTimes(0);
    await tauriIpc.listClips();
    expect(mockedInvoke).toHaveBeenCalledWith('list_clips');
    await tauriIpc.storageStats();
    expect(mockedInvoke).toHaveBeenCalledWith('storage_stats');
    // The recording commands take no arguments either: the shell already knows its
    // application data directory and its config file.
    await tauriIpc.startRecording();
    expect(mockedInvoke).toHaveBeenCalledWith('start_recording');
    await tauriIpc.stopRecording();
    expect(mockedInvoke).toHaveBeenCalledWith('stop_recording');
    await tauriIpc.recordingStatus();
    expect(mockedInvoke).toHaveBeenCalledWith('recording_status');
    await tauriIpc.clipNow();
    expect(mockedInvoke).toHaveBeenCalledWith('clip_now');
    // `app_status` takes nothing either: the shell knows its own config path and hotkey.
    await tauriIpc.appStatus();
    expect(mockedInvoke).toHaveBeenCalledWith('app_status');
    // The encoder's three reads are the same: the settings, the status and the diagnostics
    // report are all answered from the backend's own state.
    await tauriIpc.encoderSettings();
    expect(mockedInvoke).toHaveBeenCalledWith('encoder_settings');
    await tauriIpc.encoderStatus();
    expect(mockedInvoke).toHaveBeenCalledWith('encoder_status');
    await tauriIpc.copyDiagnostics();
    expect(mockedInvoke).toHaveBeenCalledWith('copy_diagnostics');
    // The display list is read from the machine itself, so it takes no arguments either.
    await tauriIpc.captureDisplays();
    expect(mockedInvoke).toHaveBeenCalledWith('capture_displays');
  });
});

describe('a command that fails', () => {
  it('is written to the application log, named, before the rejection reaches the caller', async () => {
    // The banner is for the person in front of the window; the log is for the maintainer who
    // is not. Every command failure used to stop at the component's own `catch`, so
    // `get_settings` failing blanked the settings panel and left the log completely silent —
    // and that is the one failure where the log is the only witness there is.
    mockedInvoke.mockRejectedValueOnce({ code: 'invalid_input', message: 'encode.fps is 0' });

    await expect(tauriIpc.getSettings()).rejects.toEqual({
      code: 'invalid_input',
      message: 'encode.fps is 0',
    });

    expect(mockedInvoke).toHaveBeenLastCalledWith('log_from_frontend', {
      level: 'error',
      message: 'get_settings failed',
      detail: 'encode.fps is 0',
    });
  });

  it('still rejects, so the caller can put it in front of the user as well', async () => {
    mockedInvoke.mockRejectedValueOnce('Command not found');

    await expect(tauriIpc.listClips()).rejects.toBe('Command not found');
  });

  it('does not try to report a failure of the log itself through the log', async () => {
    // `log_from_frontend` is the last resort: if it fails there is nowhere left to say so,
    // and reporting *that* through itself would be a rejection loop.
    mockedInvoke.mockClear();
    mockedInvoke.mockRejectedValueOnce('Command not found');

    await expect(tauriIpc.logFromFrontend('error', 'a message', 'a detail')).rejects.toBe(
      'Command not found',
    );

    expect(mockedInvoke).toHaveBeenCalledTimes(1);
  });
});

describe('assetUrl', () => {
  it('turns a path into an asset-protocol URL rather than a file URL', () => {
    const path = 'C:\\Users\\player\\AppData\\Local\\replayed\\clips\\clip-1.mp4';
    expect(tauriIpc.assetUrl(path)).toBe(mockedConvert.mock.results[0]?.value);
    expect(mockedConvert).toHaveBeenCalledWith(path);
    expect(tauriIpc.assetUrl(path)).not.toContain('file://');
  });
});

describe('error handling', () => {
  it('recognises a structured command error', () => {
    const err = { code: 'invalid_range', message: 'the trim range is empty' };
    expect(isCommandError(err)).toBe(true);
    expect(errorCode(err)).toBe('invalid_range');
    expect(errorMessage(err)).toBe('the trim range is empty');
  });

  it('unwraps the message from every shape a rejection arrives in', () => {
    expect(errorMessage('Command not found')).toBe('Command not found');
    expect(errorMessage(new Error('boom'))).toBe('boom');
    expect(errorMessage(null)).toContain('unknown reason');
  });

  it('does not mistake some other object for one of ours', () => {
    expect(isCommandError({ message: 'no code here' })).toBe(false);
    expect(isCommandError('a string')).toBe(false);
    expect(isCommandError(null)).toBe(false);
    expect(errorCode({ message: 'no code here' })).toBeNull();
  });

  it('falls back to something readable for an object it does not know', () => {
    expect(errorMessage({ unexpected: true })).toBe('{"unexpected":true}');
  });
});
