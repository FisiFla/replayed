/**
 * Turning a session row into what the review view shows, and deciding what can be done to it.
 *
 * Kept out of the components for the same reason `clips.ts` is: this is where the decisions
 * that a screenshot cannot check live — whether a session's length is known at all, and
 * whether a clip can be extracted from it yet.
 *
 * The one thing here that is easy to get wrong, and that `extractionBlocker` exists to get
 * right: a session reports `duration_ms: 0` when its length is **unknown**, not when it is
 * zero. That happens while it is still recording and for a session recovered from a crash.
 * Drawing a zero-length axis, or offering to extract from it, would both present an unknown
 * as a fact.
 */

import { formatBytes, formatDateTime, formatDuration } from './time';
import type { DeleteSessionOutcome, SessionDto } from './types';

/** A session row with the strings the view needs already derived. */
export interface SessionView {
  id: number;
  /** The game, or `null` for a recording nobody detected. */
  game: string | null;
  /** What the row is called on screen. Never empty. */
  title: string;
  /** `'buffer'` or `'session'`. */
  mode: string;
  /** Wall clock, ms. */
  startedAtMs: number;
  /** How long the recorded **media** is, ms; `0` means unknown (see the module note). */
  durationMs: number;
  /** Whether the media length is known. */
  hasLength: boolean;
  sizeBytes: number;
  favourite: boolean;
  /** Whether it is still recording — `ended_at_ms === null` on the wire. */
  running: boolean;
  /** Whether a concatenated file exists to extract from. */
  hasFile: boolean;
  scratchDir: string;
}

/** What a session is called when no game was detected. */
export const UNNAMED_SESSION = 'Manual recording';

/**
 * What a session is called on screen: the game, and which kind of recording it is.
 *
 * The mode is the difference a user cannot otherwise see. A buffer run (`mode: "buffer"`) is the
 * ring of a whole recording, kept because the games in it produced clips; a full session
 * (`mode: "session"`) is the match itself, concatenated into one file. Both used to read as the
 * game name alone, which is why an auto-recorded Valorant match looked like a bare timestamp
 * beside a manual one.
 *
 * The suffix is only added when the mode is known — a row written by a newer build reports a mode
 * this one does not switch on, and it is shown as the game alone rather than guessed at.
 */
export function sessionTitle(session: Pick<SessionView, 'game' | 'mode'>): string {
  const game = session.game?.trim();
  const name = game !== undefined && game.length > 0 ? game : UNNAMED_SESSION;
  return session.mode === 'session' ? `${name} — full session` : name;
}

/**
 * The session's length, or an explicit statement that it is not known.
 *
 * Deliberately not `formatDuration(0)` → `"0:00"`: a session that is still recording has no
 * probed length yet, and showing "0:00" for a two-hour recording in progress reads as data
 * loss.
 */
export function sessionLengthLabel(session: Pick<SessionView, 'durationMs'>): string {
  return session.durationMs > 0 ? formatDuration(session.durationMs) : 'length unknown';
}

export function toSessionView(session: SessionDto): SessionView {
  return {
    id: session.id,
    game: session.game,
    title: sessionTitle(session),
    mode: session.mode,
    startedAtMs: session.started_at_ms,
    durationMs: session.duration_ms,
    hasLength: session.duration_ms > 0,
    sizeBytes: session.size_bytes,
    favourite: session.favourite,
    running: session.ended_at_ms === null,
    hasFile: session.final_path !== null,
    scratchDir: session.scratch_dir,
  };
}

export function toSessionViews(sessions: SessionDto[]): SessionView[] {
  return sessions.map(toSessionView);
}

/**
 * Why this session cannot be extracted from, or `null` if it can.
 *
 * The same two refusals the Rust side enforces (`commands::extract_clip`), stated here so the
 * button can explain itself instead of failing on click: a session that is still recording has
 * no finished file, and a buffer-mode session never has one. The messages are the UI's own —
 * the Rust ones are the authority and are what a user sees if this drifts — but the *order* of
 * the checks matches, so the reason shown is the reason the command would give.
 */
export function extractionBlocker(
  session: Pick<SessionView, 'running' | 'mode' | 'hasFile'>,
): string | null {
  if (session.running) {
    return 'This session is still recording, so there is no finished file to cut from. Stop it first.';
  }
  if (!session.hasFile) {
    return (
      'This session has no concatenated file: it was recorded in buffer mode, which keeps a ' +
      'rolling ring rather than one session file. Use the clip hotkey while it is running.'
    );
  }
  return null;
}

/** Whether a clip can be cut out of this session. */
export function canExtract(session: Parameters<typeof extractionBlocker>[0]): boolean {
  return extractionBlocker(session) === null;
}

/** One line about a session: when, how long, how big, and whether it is still running. */
export function describeSession(session: SessionView): string {
  const parts = [formatDateTime(session.startedAtMs), sessionLengthLabel(session)];
  if (session.sizeBytes > 0) parts.push(formatBytes(session.sizeBytes));
  if (session.running) parts.push('still recording');
  else if (!session.hasLength) parts.push('length unknown');
  return parts.join(' · ');
}

/** What deleting a session did, in the words the banner shows. */
export function describeSessionDelete(outcome: DeleteSessionOutcome, title: string): string {
  if (!outcome.row_deleted) {
    return `Session ${title} was already gone — nothing was deleted.`;
  }
  const parts = [`Deleted session ${title}`];
  if (outcome.final_file_removed) parts.push('its recorded file');
  if (outcome.scratch_removed) parts.push('its segments');
  const freed = outcome.bytes_reclaimed > 0 ? `, freeing ${formatBytes(outcome.bytes_reclaimed)}` : '';
  if (outcome.orphaned.length > 0) {
    return (
      `${parts.join(' and ')}${freed}, but ${outcome.orphaned.length} path(s) could not be ` +
      `removed and are still on disk: ${outcome.orphaned.join(', ')}`
    );
  }
  return `${parts.join(' and ')}${freed}.`;
}

/** What extracting a clip produced, in the words the banner shows. */
export function describeExtraction(sessionTitleText: string, written: { id: number; duration_ms: number }): string {
  return (
    `Cut a ${formatDuration(written.duration_ms)} clip (clip #${written.id}) out of ` +
    `${sessionTitleText}. It is in the library now.`
  );
}
