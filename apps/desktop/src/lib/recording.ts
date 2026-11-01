/**
 * The recording readout: how a live `RecordingStatus` becomes text.
 *
 * Pure functions over a value — no DOM, no state — for the same reason `time.ts` is: the
 * window cannot be opened by this project's test suite, so the display rules are pinned by
 * tests rather than eyeballed.
 *
 * The numbers come from `replayed-recorder`, the engine the CLI also drives, so what is
 * described here is a real capture (or the honest absence of one). Nothing in this file
 * invents a value the engine does not publish: a rate that has not been measured yet says
 * so, rather than printing `0.0fps` next to a running recorder.
 */

import { basename } from './clips';
import { formatBytes, formatDuration } from './time';
import type { AppStatus, HotkeyStatus, RecordedClip, RecordingStatus } from './types';

/** How the panel describes the recorder: three states, one readout. */
export type RecordingState = 'idle' | 'recording' | 'failed';

/**
 * Which of the three states the engine is in.
 *
 * `failed` is checked first: a recorder that stopped for a failure still has counters, and
 * showing them under a "not recording" label would hide the reason it stopped.
 */
export function recordingState(status: RecordingStatus | null): RecordingState {
  if (status === null) return 'idle';
  if (status.error !== null) return 'failed';
  return status.running ? 'recording' : 'idle';
}

/**
 * The line that says why a recording that did not fail is not running, or that a running one is
 * receiving no frames; `null` when there is nothing to say.
 *
 * Neither is an error. A closed game window ends the recording cleanly and keeps everything
 * recorded, and a minimised window or an unchanging screen presents no frames for as long as it
 * stays that way. The stall is shown only while running, from 5 s, so an ordinary pause between
 * frames never reads as a frozen recording.
 */
export function describeRecorderNotice(status: RecordingStatus | null): string | null {
  if (status === null || status.error !== null) return null;
  if (!status.running && status.ended !== null) return `The recording ended: ${status.ended}`;
  if (status.running && status.stalled_for_ms !== null && status.stalled_for_ms >= 5_000) {
    return `No new frames for ${Math.floor(status.stalled_for_ms / 1000)} s (minimised or unchanging window).`;
  }
  return null;
}

/** The badge text for a state. */
export function recordingLabel(status: RecordingStatus | null): string {
  switch (recordingState(status)) {
    case 'recording':
      return 'REC';
    case 'failed':
      return 'stopped';
    default:
      return 'not recording';
  }
}

/**
 * The achieved rate against the rate the pipeline is running at — `29.8 / 30 fps`.
 *
 * The denominator is `effective_fps`, not `configured_fps`: the engine measures what this
 * machine can sustain at the captured resolution and paces to `min(configured, measured)`
 * (`encode.adapt_fps`, spec §10.1). That is a *pacing* decision — capture work not spent on
 * frames the encoder would drop — not the rate the media timeline is recorded at: the
 * timeline is the frames' arrival timestamps and tracks the wall clock at any rate. When the
 * effective rate is below what the config asked for, the readout says so — otherwise a
 * machine that cannot hold the configured rate would look like a permanent shortfall rather
 * than the decision the engine actually made.
 *
 * `replayed-recorder` measures the achieved rate over a one-second window and reports 0.0
 * until the first window closes, deliberately (a rate over 200ms is noise). So a running
 * recorder whose rate has not been measured yet says "measuring…", and an idle one shows a
 * dash: `0.0 / 30 fps` on a healthy recorder would read as a failure that is not there.
 */
export function formatRate(status: RecordingStatus | null): string {
  if (status === null || !status.running) return '—';
  if (!(status.fps > 0)) return 'measuring…';
  const rate = `${status.fps.toFixed(1)} / ${status.effective_fps} fps`;
  return status.effective_fps < status.configured_fps
    ? `${rate} (${status.configured_fps} configured)`
    : rate;
}

/**
 * How far the media timeline has drifted from the wall clock, in words.
 *
 * Positive means the media timeline is *behind* real time — the measured 0.81x case on 4K
 * hardware, where a clip's configured `pre_seconds` of media covers more real seconds than
 * configured. It is reported rather than hidden because it is the reason the trigger is
 * taken from media time (see `replayed-recorder`'s trigger path).
 */
export function formatDrift(status: RecordingStatus | null): string {
  if (status === null || !status.running) return '—';
  const drift = Math.round(status.drift_ms);
  const direction = drift >= 0 ? 'behind' : 'ahead of';
  return `${Math.abs(drift)}ms ${direction} real time`;
}

/**
 * What the frame accounting says, or `null` when there is nothing to report.
 *
 * `dropped` is the encoder's own count of frames it discarded because its queue was full:
 * the machine cannot encode the rate the pipeline declared (`effective_fps`), which is a
 * timeline problem and not only a quality one. `skipped` is the pacer dropping surplus
 * frames *without* reading them back, which is the optimisation working — it is normal and
 * is only mentioned when it happens.
 */
export function describeFrames(status: RecordingStatus | null): string | null {
  if (status === null) return null;
  const parts: string[] = [];
  if (status.dropped > 0) {
    parts.push(
      `${status.dropped} frame${status.dropped === 1 ? '' : 's'} dropped by the encoder — ` +
        `this machine is not keeping up with the ${status.effective_fps}fps it declared`,
    );
  }
  if (status.skipped > 0) {
    parts.push(
      `${status.skipped} frame${status.skipped === 1 ? '' : 's'} skipped without a GPU readback`,
    );
  }
  if (status.dropped_audio > 0) {
    parts.push(`${status.dropped_audio} audio blocks dropped`);
  }
  return parts.length === 0 ? null : `${parts.join('; ')}.`;
}

/**
 * The notice for a clip that was just taken.
 *
 * A clip that could not be indexed is reported as exactly that, with its path: the file is
 * on disk and it is what the user asked for, but it will not be in the list, and the
 * storage policy cannot see it. Saying "saved" about it would be a lie a user would only
 * discover later.
 */
export function describeRecordedClip(clip: RecordedClip): string {
  const name = basename(clip.path);
  const length = formatDuration(clip.duration_ms);
  const size = formatBytes(clip.size_bytes);
  if (clip.id === null) {
    return (
      `Wrote ${name} (${length}, ${size}, ${clip.codec}) but it could not be indexed, ` +
      'so it will not appear in the list and the storage policy cannot manage it.'
    );
  }
  return `Saved ${name} (${length}, ${size}, ${clip.codec}) as clip #${clip.id}.`;
}

/**
 * How the panel describes the clip hotkey, and whether that is a problem.
 *
 * `problem` is the difference between a line of orientation and an alert: an *installed*
 * hotkey is a sentence telling the user what to press; one that is not installed is a
 * failure they have to act on — the chord is held by another application or by another
 * replayed, or this build has no global hotkey at all. The reason is the shell's own
 * sentence (it names the chord and the likely owner), not something invented here.
 */
export function describeHotkey(
  hotkey: HotkeyStatus | null,
): { text: string; problem: boolean } | null {
  if (hotkey === null) return null;
  if (hotkey.installed) {
    return {
      text: `Press ${hotkey.chord} to take a clip — with this window hidden, or in the tray.`,
      problem: false,
    };
  }
  const reason = hotkey.error ?? 'the reason was not reported.';
  return { text: `The clip hotkey ${hotkey.chord} is NOT installed. ${reason}`, problem: true };
}

/**
 * The automatic-recording line: whether the recorder is armed for games, and what it
 * caught. `null` until the first status arrives — the panel shows nothing rather than a
 * verdict about a recorder it has not heard from.
 */
export function describeAutoRecord(status: RecordingStatus | null): string | null {
  if (status === null) return null;
  if (!status.watching_games) return 'Automatic recording is off.';
  if (status.matched_game !== null) return `${status.matched_game} detected — recording.`;
  return 'Watching for a game — recording starts on match.';
}

/**
 * One line naming the configuration file this process read.
 *
 * The file is where the window's settings are persisted, so the panel says which one
 * it found — and says plainly when there was none, rather than presenting the
 * example's defaults as if they had been configured.
 */
export function describeConfig(status: AppStatus | null): string | null {
  if (status === null) return null;
  if (status.config_exists) return `Config: ${status.config_path}`;
  return (
    `No config.toml at ${status.config_path} yet — the values in config.example.toml are ` +
    'in force. Create that file to change the hotkey; "Open config file" in the tray menu ' +
    'opens its folder.'
  );
}
