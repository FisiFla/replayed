/**
 * The clip list's view model: the store's rows turned into what the list and the player
 * need. Pure, so the mapping — including the two places it can quietly go wrong, the
 * basename of a Windows path and the choice of thumbnail frame — is tested.
 */

import { formatBytes, formatDateTime, formatDuration } from './time';
import { isClick } from './markers';
import type { ClipDto, DeleteOutcome, ExportOutcome, PruneOutcome, TrimRange } from './types';

/** One row of the clip list. */
export interface ClipView {
  id: number;
  /**
   * Whether this row is a still image. A screenshot has no player, no trim, no duration and no
   * combine; the file itself is its own picture.
   */
  isScreenshot: boolean;
  /** The file's name, which is what the list shows: the directory is noise there. */
  name: string;
  /** The full path, for the detail view and for error messages. */
  path: string;
  /** `asset:` URL for the `<video>` element, built by [`ClipSource.assetUrl`]. */
  assetUrl: string;
  durationMs: number;
  durationLabel: string;
  sizeBytes: number;
  sizeLabel: string;
  codec: string;
  favourite: boolean;
  createdAtMs: number;
  createdAtLabel: string;
  /** The user's title, or `null`. */
  title: string | null;
  /** What the list and the header show: the title when there is one, else the file name. */
  displayName: string;
  game: string | null;
  tags: string[];
}

/**
 * The last path component of `path`.
 *
 * Both separators are handled: the application runs on Windows and indexes paths written
 * there, while the development host is macOS, and a list of clips whose names all read as
 * the full path because the split only knew about `/` is exactly the sort of thing a
 * window-less test suite has to catch instead of an eye.
 */
export function basename(path: string): string {
  const parts = path.split(/[\\/]/);
  const last = parts[parts.length - 1];
  return last !== undefined && last !== '' ? last : path;
}

/**
 * The frame to thumbnail a clip at.
 *
 * One second in, or the middle of anything shorter than that. Deliberately not the last
 * frame: a stream's final frame starts up to one frame interval before its nominal
 * duration ends, so asking for a frame a few milliseconds from the end can legitimately
 * read nothing at all (see the note on `thumbnail` in `src-tauri/src/commands.rs`). One
 * second in is past the first keyframe of every clip this application writes and nowhere
 * near the end.
 */
export function thumbnailAtMs(durationMs: number): number {
  if (!Number.isFinite(durationMs) || durationMs <= 0) return 0;
  const oneSecond = 1_000;
  return durationMs > oneSecond ? oneSecond : Math.floor(durationMs / 2);
}

/** Map one row. `assetUrl` is injected so this stays free of the Tauri API. */
export function toClipView(clip: ClipDto, assetUrl: (path: string) => string): ClipView {
  const isScreenshot = clip.kind === 'screenshot';
  return {
    id: clip.id,
    isScreenshot,
    name: basename(clip.path),
    path: clip.path,
    assetUrl: assetUrl(clip.path),
    durationMs: clip.duration_ms,
    // "0:00" under a screenshot would read as a broken clip; there is no duration to show.
    durationLabel: isScreenshot ? 'still' : formatDuration(clip.duration_ms),
    sizeBytes: clip.size_bytes,
    sizeLabel: formatBytes(clip.size_bytes),
    codec: clip.codec,
    favourite: clip.favourite,
    createdAtMs: clip.created_at_ms,
    createdAtLabel: formatDateTime(clip.created_at_ms),
    title: clip.title,
    displayName: clip.title ?? basename(clip.path),
    game: clip.game,
    tags: clip.tags,
  };
}

/**
 * Map a whole list, preserving its order.
 *
 * Order is not re-sorted here. `list_clips` already returns clips newest first, ordered by
 * the wall-clock instant the row was written — and it is the Rust side that owns that
 * decision, because it is the side that knows `clips.started_at` is not comparable across
 * captures. Sorting again here would be a second, silently different opinion.
 */
export function toClipViews(clips: ClipDto[], assetUrl: (path: string) => string): ClipView[] {
  return clips.map((clip) => toClipView(clip, assetUrl));
}

/** Which clip a list refresh should leave selected. */
export function resolveSelection(
  views: ClipView[],
  selectedId: number | null,
): number | null {
  if (views.length === 0) return null;
  if (selectedId !== null && views.some((view) => view.id === selectedId)) return selectedId;
  // The selected clip is gone (deleted, or the index was reloaded): fall to the newest.
  return views[0]?.id ?? null;
}

/**
 * The thumbnails of the clips that are still listed.
 *
 * A clip removed by the storage policy — or by a retention pass over the sessions — takes
 * its cached picture with it, but the map of them is held by the window and nothing else
 * ever empties it. Pruned against the clips that still exist, it cannot grow without bound
 * over a window that is left open all day.
 *
 * The map is returned unchanged when nothing was dropped, so a refresh that finds the same
 * library does not churn the rows that show these pictures.
 */
export function pruneThumbnails(
  thumbnails: Record<number, string>,
  clipIds: readonly number[],
): Record<number, string> {
  const live = new Set(clipIds);
  const kept: Record<number, string> = {};
  let dropped = 0;
  for (const [id, url] of Object.entries(thumbnails)) {
    const key = Number(id);
    if (live.has(key)) {
      kept[key] = url;
    } else {
      dropped += 1;
    }
  }
  return dropped === 0 ? thumbnails : kept;
}

/**
 * What to tell the user after `delete_clip`, in terms of what actually happened.
 *
 * The four outcomes are the four states the Rust command can report, and three of them are
 * not "it went fine": a row whose file could not be unlinked leaves a file the index no
 * longer names (spec §8.2 — recoverable, but only if it is said out loud rather than
 * reported as a clean delete), and an id with no row deletes nothing at all.
 */
export function describeDelete(outcome: DeleteOutcome, name: string): string {
  if (!outcome.row_deleted) {
    return `${name} was no longer in the index, so nothing was deleted.`;
  }
  if (outcome.orphaned_path !== null) {
    return (
      `${name} was removed from the index, but its file could not be deleted and is ` +
      `left on disk at ${outcome.orphaned_path}. It is no longer managed by the storage policy.`
    );
  }
  if (outcome.already_missing) {
    return `${name} was removed from the index; its file was already gone.`;
  }
  return `${name} deleted, freeing ${formatBytes(outcome.bytes_reclaimed)}.`;
}

/**
 * What a reconciliation of the index with the disk did, in one sentence.
 *
 * Two counts, because the libraries are separate: a session row has no clip to be counted in.
 * "Nothing to remove" is not a failure — a second press is allowed, and so is a press after the
 * files came back (a network directory that was briefly offline).
 */
export function describePrune(outcome: PruneOutcome): string {
  const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;
  const parts: string[] = [];
  if (outcome.clips_removed > 0) parts.push(plural(outcome.clips_removed, 'clip'));
  if (outcome.sessions_removed > 0) parts.push(plural(outcome.sessions_removed, 'session'));

  if (parts.length === 0) {
    if (outcome.failed > 0) {
      return (
        `Nothing could be removed: ${plural(outcome.failed, 'index entry')} could not be ` +
        'deleted. The library is unchanged; the log names the rows.'
      );
    }
    return 'Nothing to remove: every entry in the index has its file.';
  }

  let line =
    `Removed ${parts.join(' and ')} from the index; ` +
    `${parts.length === 1 ? 'its file was' : 'their files were'} already gone. ` +
    'Nothing was deleted from disk.';
  if (outcome.sessions_refused > 0) {
    line +=
      ` ${plural(outcome.sessions_refused, 'session')} is still recording and was left alone.`;
  }
  if (outcome.failed > 0) {
    line += ` ${plural(outcome.failed, 'index entry')} could not be removed; the log names which.`;
  }
  return line;
}

/**
 * The export presets this build offers, as the panel lists them and as `export_clip` names them.
 *
 * Kept in step with `replayed_media::export::ExportPreset` by hand, like the DTO interfaces: the
 * wire names are the enum's `as_str` values, and `export_clip` refuses anything else.
 */
export const EXPORT_PRESETS = [
  { value: 'discord', label: 'Discord (720p, small)' },
  { value: 'vertical', label: 'Vertical 9:16 (1080×1920)' },
  { value: 'gif', label: 'GIF (480px, 15fps)' },
] as const;

/**
 * What combining clips did, in one sentence.
 *
 * The count is in the sentence because the button said it: "combine 3 clips" that then reported
 * only a file name would leave the user counting cards.
 */
export function describeCombine(count: number, written: ClipDto): string {
  return (
    `Combined ${count} clips into ${basename(written.path)} — it is in the library now, ` +
    `and the originals are untouched.`
  );
}

/**
 * What an export produced, and where it went.
 *
 * The path's file name and the size, not just "exported": the file is deliberately **not** in
 * the library, so this notice is the only place the user learns where to find it.
 */
export function describeExport(outcome: ExportOutcome): string {
  const preset =
    EXPORT_PRESETS.find((option) => option.value === outcome.preset)?.label ?? outcome.preset;
  return (
    `Exported as ${preset}: ${basename(outcome.path)} (${formatBytes(outcome.size_bytes)}). ` +
    'It is beside the clip, not in the library.'
  );
}

/**
 * The two durations a trim report has to show: what was asked for, and what was written.
 *
 * They are not always the same. A stream copy cuts where the stream allows, so `trim_clip`
 * reports the probed length of the file rather than echoing the request — and the only
 * honest thing to do with two different numbers is show both.
 */
export function describeTrim(requested: TrimRange, written: ClipDto): string {
  const asked = Math.max(0, requested.endMs - requested.startMs);
  const got = written.duration_ms;
  const name = basename(written.path);

  // Within a couple of frames at ordinary frame rates, the two are the same number as far
  // as anyone reading this is concerned; beyond that the difference is worth stating.
  if (Math.abs(asked - got) <= TRIM_MATCH_TOLERANCE_MS) {
    return `Trimmed to ${name} (${formatDuration(got)}), losslessly remuxed and indexed as clip #${written.id}.`;
  }
  return (
    `Trimmed to ${name}: asked for ${formatDuration(asked)}, the file is ` +
    `${formatDuration(got)} — a stream copy cuts where the stream allows. Indexed as ` +
    `clip #${written.id}.`
  );
}

/** How close two durations have to be before the trim report calls them equal. */
const TRIM_MATCH_TOLERANCE_MS = 50;

/** The library's filters: free-text search, one game, one tag. */
export interface ClipFilter {
  query: string;
  /** Exact game name, or `null` for all. */
  game: string | null;
  /** Normalized tag, or `null` for all. */
  tag: string | null;
}

export const NO_FILTER: ClipFilter = { query: '', game: null, tag: null };

/** Whether any filter is narrowing the list. */
export function isFiltering(filter: ClipFilter): boolean {
  return filter.query.trim() !== '' || filter.game !== null || filter.tag !== null;
}

/**
 * The clips that pass `filter`, in their given order.
 *
 * The query is split on whitespace and every word must appear (case-insensitively) somewhere
 * in the title, file name, game or tags — "ace valorant" finds a Valorant clip tagged ace.
 * The game and tag filters are exact. Done here rather than in SQL because the library is
 * already fully in memory for the list and the sort order is the Rust side's.
 */
export function filterClips(views: ClipView[], filter: ClipFilter): ClipView[] {
  const words = filter.query.toLowerCase().split(/\s+/).filter((w) => w !== '');
  return views.filter((view) => {
    if (filter.game !== null && view.game !== filter.game) return false;
    if (filter.tag !== null && !view.tags.includes(filter.tag)) return false;
    if (words.length === 0) return true;
    const haystack = [view.displayName, view.name, view.game ?? '', ...view.tags]
      .join('\n')
      .toLowerCase();
    return words.every((word) => haystack.includes(word));
  });
}

/** Distinct games with their clip counts, alphabetical (case-insensitive). */
export function gameOptions(views: ClipView[]): { value: string; count: number }[] {
  return countBy(views.flatMap((v) => (v.game !== null ? [v.game] : [])));
}

/** Distinct tags with their clip counts, alphabetical. */
export function tagOptions(views: ClipView[]): { value: string; count: number }[] {
  return countBy(views.flatMap((v) => v.tags));
}

function countBy(values: string[]): { value: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const value of values) counts.set(value, (counts.get(value) ?? 0) + 1);
  return [...counts]
    .map(([value, count]) => ({ value, count }))
    .sort((a, b) => a.value.localeCompare(b.value, undefined, { sensitivity: 'base' }));
}

/**
 * Tags typed into the editor: split on commas and newlines, trimmed, inner whitespace
 * collapsed, lower-cased and de-duplicated — the same normalization the store applies, so
 * what the user sees is what is saved. Blank entries are dropped.
 */
export function parseTagInput(text: string): string[] {
  const seen = new Set<string>();
  for (const part of text.split(/[,\n]/)) {
    const tag = part.trim().split(/\s+/).join(' ').toLowerCase();
    if (tag !== '') seen.add(tag);
  }
  return [...seen];
}

/**
 * Whether a pointer that went down on a library row and is now at `now` should hand the
 * gesture to a native drag-out: the primary button is still held and it has moved beyond
 * the click slop (so an ordinary click still selects).
 */
export function shouldStartDrag(
  down: { x: number; y: number },
  now: { x: number; y: number },
  buttons: number,
): boolean {
  return (buttons & 1) === 1 && !isClick(down, now);
}
