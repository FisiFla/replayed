/**
 * What the window says after `update_settings` answers.
 *
 * Pure, for the usual reason: the notice wording is a display rule the headless suite
 * pins rather than a screenshot. The restart sentence arrives whole from the backend
 * (`restart_reason` names the keys); this only decides the saved-vs-nothing part.
 */

import { basename } from './clips';
import type {
  AudioDevice,
  SettingsDto,
  SettingsUpdate,
  UpdateSettingsOutcome,
  WatchGame,
} from './types';

export function describeSettingsUpdate(outcome: UpdateSettingsOutcome): string {
  const saved =
    outcome.applied.length === 0
      ? 'No changes.'
      : `Saved ${outcome.applied.join(', ')}.`;
  const restart = outcome.restart_reason === null ? '' : ` ${outcome.restart_reason}`;
  // The registry is a second truth: the setting can be saved while Windows was not changed, and
  // the notice is the only place that says so.
  const autostart = outcome.autostart === null ? '' : ` ${outcome.autostart.detail}`;
  // The hotkey is the other second truth: the file is saved, the process still holds the old chord.
  const hotkey = outcome.hotkey === null ? '' : ` ${outcome.hotkey}`;
  return `${saved}${restart}${autostart}${hotkey}`.trim();
}

/** The quality presets, mirroring the backend table (`QUALITY_PRESETS` in config.rs). */
export const QUALITY_OPTIONS = [
  { value: 'low', label: 'Low (8 Mbit/s)' },
  { value: 'balanced', label: 'Balanced (20 Mbit/s)' },
  { value: 'high', label: 'High (40 Mbit/s)' },
] as const;

/** A monitor index is a whole number from 0; the backend refuses one with no monitor behind it. */
export function monitorIndex(text: string): number | null {
  return /^\d+$/.test(text.trim()) ? Number(text.trim()) : null;
}

/**
 * The capture update for the drafts: only what differs from what is saved, so applying an
 * untouched panel writes nothing and a `custom` bitrate is never overwritten by a preset the
 * user did not pick.
 */
export function captureUpdate(
  saved: SettingsDto,
  draft: {
    quality: string;
    monitor: number;
    window: string;
    micDevice: string;
    outputDevice: string;
  },
): SettingsUpdate {
  const update: SettingsUpdate = {};
  if (draft.quality !== saved.quality && draft.quality !== 'custom') update.quality = draft.quality;
  if (draft.monitor !== saved.capture_monitor) update.capture_monitor = draft.monitor;
  if (draft.window.trim() !== saved.capture_window) update.capture_window = draft.window.trim();
  if (draft.micDevice !== saved.mic_device) update.mic_device = draft.micDevice;
  if (draft.outputDevice !== saved.output_device) update.output_device = draft.outputDevice;
  return update;
}

/**
 * The options for a device picker: the Windows default first, then each active device. A saved
 * device that is no longer listed (unplugged) stays selectable as itself, so the panel shows what
 * the file says and an untouched Apply does not rewrite it to the default.
 */
export function deviceOptions(
  devices: AudioDevice[],
  selected: string,
): { value: string; label: string }[] {
  const options = [{ value: '', label: 'Windows default' }];
  for (const device of devices) {
    options.push({
      value: device.id,
      label: device.is_default ? `${device.name} (current default)` : device.name,
    });
  }
  if (selected !== '' && !devices.some((device) => device.id === selected)) {
    options.push({ value: selected, label: 'Saved device (not connected)' });
  }
  return options;
}

/**
 * The name a new watch entry gets when the user gives none: the executable without its `.exe`.
 *
 * `RainbowSix.exe` becomes "RainbowSix" — a label, not a claim about the game. The panel offers an
 * editable name field beside it, because the label is what the user reads in the tray.
 */
export function defaultWatchName(exe: string): string {
  const file = basename(exe.trim());
  return file.replace(/\.exe$/i, '');
}

/**
 * What is wrong with a watch list, or `null` when the recorder would accept it.
 *
 * These are the engine's own rules (`validate_watch_list`) plus the one a panel can add without
 * lying: an entry whose executable does not end in `.exe` can never match a process image, so
 * accepting it would watch nothing while looking like it watched something. An empty list is
 * refused here rather than written, because the backend expresses "no entries" by removing the
 * key — which means the default titles — and a Remove button that silently brought six games back
 * would be a bug the user cannot see.
 */
export function watchListProblem(games: WatchGame[]): string | null {
  if (games.length === 0) {
    return 'The list cannot be empty: with no entries the default titles are watched.';
  }
  const seen: string[] = [];
  for (const game of games) {
    const exe = basename(game.exe.trim());
    if (exe === '') return `${game.name.trim() || 'An entry'} has no executable name to watch for.`;
    if (!/\.exe$/i.test(exe)) {
      return `${exe} does not end in .exe, so no running game can match it.`;
    }
    const key = exe.toLowerCase();
    if (seen.includes(key)) return `${exe} is watched twice.`;
    seen.push(key);
  }
  return null;
}

/** Whether two lists are the same entries in the same order, as the panel would write them. */
export function sameWatchList(a: WatchGame[], b: WatchGame[]): boolean {
  return (
    a.length === b.length &&
    a.every(
      (game, i) =>
        game.name === b[i]!.name && game.exe === b[i]!.exe && game.signal === b[i]!.signal,
    )
  );
}

/**
 * The "Automatic recording" update: only what differs from what is saved.
 *
 * The two toggles and the watch list are one feature — auto-record watches this list — so they are
 * applied together and the section says so. Sending only the differences keeps a re-apply from
 * rewriting keys the user did not touch and from reporting them as saved.
 */
export function automaticUpdate(
  saved: SettingsDto,
  draft: { autoRecord: boolean; saveSession: boolean; watch: WatchGame[] },
): SettingsUpdate {
  const update: SettingsUpdate = {};
  if (draft.autoRecord !== saved.auto_record) update.auto_record = draft.autoRecord;
  if (draft.saveSession !== saved.save_session) update.save_session = draft.saveSession;
  if (!sameWatchList(draft.watch, saved.watch_games)) update.watch_games = draft.watch;
  return update;
}

/**
 * The sentence that matters when automatic recording is on but nothing is being kept.
 *
 * Auto-record on its own only starts the replay buffer: a game with no highlights integration is
 * then recorded into hotkey clips, and the match itself is never saved. The plan's parity tier is
 * "full session + clips", so the panel says which half is off instead of leaving it to be found
 * out after a match.
 */
export function automaticWarning(draft: { autoRecord: boolean; saveSession: boolean }): string | null {
  return draft.autoRecord && !draft.saveSession
    ? 'Automatic recording is on but “Also save the full session” is off: games without ' +
        'highlights are recorded into clips only, and the match itself is not kept.'
    : null;
}

/**
 * What is wrong with a chord the user typed, or `null` when the backend's parser would take it.
 *
 * A courtesy check, not the authority: `hotkeys.clip` is validated by
 * `replayed_events::hotkey::Hotkey::parse` on the way in, and this mirrors its vocabulary so a
 * typo is answered before a round trip. The shape is modifiers in any order (Ctrl, Alt, Shift),
 * then exactly one key: `F1`–`F24` or a single letter or digit.
 */
export function chordProblem(chord: string): string | null {
  const text = chord.trim();
  if (text === '') return 'A chord cannot be empty.';
  const parts = text.split('+').map((part) => part.trim().toLowerCase());
  const key = parts.pop() ?? '';
  for (const modifier of parts) {
    if (!['ctrl', 'alt', 'shift'].includes(modifier)) {
      return `"${modifier}" is not a modifier: use Ctrl, Alt or Shift.`;
    }
  }
  if (key === '' || (parts.length === 0 && ['ctrl', 'alt', 'shift'].includes(key))) {
    // A bare modifier (`Ctrl`, `Ctrl+`) is a chord with no key: it would register nothing.
    return 'A chord needs a keypress too, like Ctrl+F8.';
  }
  if (/^f([1-9]|1[0-9]|2[0-4])$/.test(key)) return null;
  if (/^[a-z0-9]$/.test(key)) return null;
  return `"${key}" is not a key: use one letter or digit, or F1 to F24.`;
}

/**
 * The clipped chord, in the spelling the backend stores: modifiers title-cased in the order the
 * parser prints them, then the key upper-cased.
 *
 * The panel shows what it will write, and the writer normalises anyway; doing it here too keeps
 * the field from looking unchanged after an apply that did change it.
 */
export function normaliseChord(chord: string): string {
  const parts = chord
    .trim()
    .split('+')
    .map((part) => part.trim())
    .filter((part) => part !== '');
  const key = parts.pop() ?? '';
  const modifiers = new Set(parts.map((part) => part.toLowerCase()));
  const ordered = ['ctrl', 'alt', 'shift']
    .filter((modifier) => modifiers.has(modifier))
    .map((modifier) => modifier.charAt(0).toUpperCase() + modifier.slice(1));
  return [...ordered, key.toUpperCase()].join('+');
}
