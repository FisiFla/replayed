/**
 * What the session view decides, tested without a DOM.
 *
 * The properties worth pinning are the ones that are wrong in a way a screenshot will not
 * show: that an *unknown* media length is never presented as a zero-length recording, and
 * that the two refusals the Rust side enforces are predicted here rather than discovered by
 * clicking a button and reading an error.
 */

import { describe, expect, it } from 'vitest';

import {
  UNNAMED_SESSION,
  canExtract,
  describeSession,
  describeSessionDelete,
  extractionBlocker,
  sessionLengthLabel,
  sessionTitle,
  toSessionView,
} from './sessions';
import type { DeleteSessionOutcome, SessionDto } from './types';

function dto(overrides: Partial<SessionDto> = {}): SessionDto {
  return {
    id: 1,
    game: 'Dota 2',
    mode: 'session',
    started_at_ms: 1_700_000_000_000,
    ended_at_ms: 1_700_000_060_000,
    final_path: '/sessions/1.mp4',
    size_bytes: 4_096,
    favourite: false,
    scratch_dir: '/scratch/1',
    duration_ms: 60_000,
    ...overrides,
  };
}

describe('naming a session', () => {
  it('uses the detected game', () => {
    expect(sessionTitle({ game: 'Dota 2', mode: 'buffer' })).toBe('Dota 2');
  });

  it('says when the session is the whole match', () => {
    // The parity tier's payoff: an auto-recorded match reads as a match, not as the game name
    // it shares with the buffer run that produced the clips.
    expect(sessionTitle({ game: 'Valorant', mode: 'session' })).toBe('Valorant — full session');
    expect(sessionTitle({ game: null, mode: 'session' })).toBe(
      `${UNNAMED_SESSION} — full session`,
    );
  });

  it('does not guess at a mode this build does not know', () => {
    // A row written by a newer build: shown as the game alone rather than mislabelled.
    expect(sessionTitle({ game: 'Dota 2', mode: 'something_new' })).toBe('Dota 2');
  });

  it('names a session nobody detected rather than leaving it blank', () => {
    expect(sessionTitle({ game: null, mode: 'buffer' })).toBe(UNNAMED_SESSION);
    // A game string of spaces is not a name: a row must never render as an empty row.
    expect(sessionTitle({ game: '   ', mode: 'buffer' })).toBe(UNNAMED_SESSION);
  });
});

describe('a session whose media length is unknown', () => {
  it('says so instead of showing zero', () => {
    // `durationMs: 0` means UNKNOWN — it is what a session still recording carries, and what
    // a session recovered from a crash carries for ever. `formatDuration(0)` would print
    // "0:00", which for a two-hour recording in progress reads as data loss.
    expect(sessionLengthLabel({ durationMs: 0 })).toBe('length unknown');
    expect(sessionLengthLabel({ durationMs: 0 })).not.toContain('0:00');
  });

  it('formats a real length', () => {
    expect(sessionLengthLabel({ durationMs: 90_000 })).toBe('1:30.0');
  });

  it('is reported as unknown, not as empty, by the view', () => {
    const view = toSessionView(dto({ duration_ms: 0 }));
    expect(view.hasLength).toBe(false);
    expect(describeSession(view)).toContain('length unknown');
  });
});

describe('what can be done to a session', () => {
  it('allows an extraction from a finished full session', () => {
    const view = toSessionView(dto());
    expect(canExtract(view)).toBe(true);
    expect(extractionBlocker(view)).toBeNull();
  });

  it('refuses a session that is still recording, and says why', () => {
    const view = toSessionView(dto({ ended_at_ms: null }));
    expect(view.running).toBe(true);
    expect(canExtract(view)).toBe(false);
    expect(extractionBlocker(view)).toContain('still recording');
  });

  it('refuses a buffer session, which never has a concatenated file', () => {
    const view = toSessionView(dto({ mode: 'buffer', final_path: null }));
    expect(canExtract(view)).toBe(false);
    expect(extractionBlocker(view)).toContain('buffer mode');
  });

  it('reports the recording reason before the buffer one, as the Rust side does', () => {
    // A running buffer session is both; the command checks "still recording" first, so the
    // button must show the same reason the click would have produced.
    const view = toSessionView(dto({ mode: 'buffer', final_path: null, ended_at_ms: null }));
    expect(extractionBlocker(view)).toContain('still recording');
  });
});

describe('describing a deletion', () => {
  const outcome = (over: Partial<DeleteSessionOutcome> = {}): DeleteSessionOutcome => ({
    id: 1,
    row_deleted: true,
    scratch_removed: true,
    final_file_removed: true,
    bytes_reclaimed: 2_048,
    orphaned: [],
    ...over,
  });

  it('names what went and what was freed', () => {
    const text = describeSessionDelete(outcome(), 'Dota 2');
    expect(text).toContain('Dota 2');
    expect(text).toContain('recorded file');
    expect(text).toContain('segments');
    expect(text).toContain('2.0 KiB');
  });

  it('reports a session that was already gone as a no-op, not a failure', () => {
    // The same contract `delete_clip` has: a UI racing a retention pass can refresh truthfully.
    expect(describeSessionDelete(outcome({ row_deleted: false }), 'Dota 2')).toContain(
      'already gone',
    );
  });

  it('names the paths that outlived the row rather than swallowing them', () => {
    const text = describeSessionDelete(outcome({ orphaned: ['/scratch/1'] }), 'Dota 2');
    expect(text).toContain('/scratch/1');
    expect(text).toContain('could not be removed');
  });

  it('does not claim a file was removed when there was none', () => {
    const text = describeSessionDelete(
      outcome({ final_file_removed: false, scratch_removed: false, bytes_reclaimed: 0 }),
      'Dota 2',
    );
    expect(text).not.toContain('recorded file');
    expect(text).not.toContain('freeing');
  });
});
