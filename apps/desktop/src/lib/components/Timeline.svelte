<script lang="ts">
  /**
   * The timeline scrubber.
   *
   * Four things live on the track: the playhead (driven by the `<video>` element in
   * `ClipDetail`), two trim handles, and — when the clip came from a session — that session's
   * event markers. The arithmetic behind all of it is in `../trim.ts` and `../markers.ts`,
   * both tested without a DOM; this component only turns pointer positions into milliseconds
   * and draws the result.
   *
   * Markers used to be forbidden here, on the grounds that the `events` table existed but
   * nothing wrote to it: a timeline drawn from an always-empty table would look like a feature
   * and tell the user nothing. That is no longer true in either half. The recorder links every
   * event it records to the session that was running and a hotkey clip writes a `bookmark`,
   * and a session's offsets are measured on the media clock the store computes from its
   * `media_epoch_ms`. So markers are drawn — and `markers` defaults to `[]`, so a clip with no
   * session behind it draws exactly what it drew before.
   */
  import {
    KEYBOARD_COARSE_STEP_MS,
    KEYBOARD_PAGE_MS,
    KEYBOARD_STEP_MS,
    dragHandle,
    fractionToMs,
    handleKeyRange,
    msToPercent,
    rangeLengthLabel,
    rangeLabel,
    rangePercents,
  } from '../trim';
  import { isClick, placeMarkers, seekTargetMs } from '../markers';
  import { formatPrecise } from '../time';
  import type { SessionEvent, TrimRange } from '../types';

  interface Props {
    durationMs: number;
    playheadMs: number;
    range: TrimRange;
    /** Move playback to `ms`. */
    onSeek: (ms: number) => void;
    onRangeChange: (range: TrimRange) => void;
    /**
     * The markers of the session this clip came from, in the store's media-time order.
     * Empty for a clip with no session behind it — which is the default, because `events` rows
     * belong to a session and a clip is not a session.
     */
    markers?: SessionEvent[];
  }

  let { durationMs, playheadMs, range, onSeek, onRangeChange, markers = [] }: Props = $props();

  let track = $state<HTMLDivElement | null>(null);
  /** What the pointer is currently moving, if anything. */
  let dragging = $state<'start' | 'end' | 'playhead' | null>(null);
  /** Where the current gesture started, so a marker click can be told from a scrub. */
  let gestureStart = $state<{ x: number; y: number } | null>(null);
  /** The offset of the marker the current gesture began on, if it began on one. */
  let pressedMarkerMs = $state<number | null>(null);

  const percents = $derived(rangePercents(range, durationMs));
  const playheadPercent = $derived(msToPercent(playheadMs, durationMs));
  const placed = $derived(placeMarkers(markers, durationMs));
  const disabled = $derived(durationMs <= 0);

  /** The millisecond offset under the pointer. */
  function msAt(event: PointerEvent): number {
    const element = track;
    if (!element) return 0;
    const rect = element.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    return fractionToMs((event.clientX - rect.left) / rect.width, durationMs);
  }

  function apply(kind: 'start' | 'end' | 'playhead', event: PointerEvent) {
    const ms = msAt(event);
    if (kind === 'playhead') {
      onSeek(ms);
      return;
    }
    onRangeChange(dragHandle(range, kind, ms, durationMs));
  }

  /**
   * Take the pointer, so that a gesture which leaves the element it started on keeps
   * being delivered to the track.
   */
  function capture(event: PointerEvent) {
    event.preventDefault();
    // Capture on the track, not on the handle: a drag that leaves the handle — which every
    // drag past the opposite handle does — must keep being delivered here.
    track?.setPointerCapture(event.pointerId);
  }

  function begin(kind: 'start' | 'end' | 'playhead', event: PointerEvent) {
    if (disabled) return;
    dragging = kind;
    capture(event);
    apply(kind, event);
  }

  /**
   * The offset of the marker a press landed on, if it landed on one.
   *
   * A marker does not stop the press — a drag that starts on one scrubs like any other —
   * so the track is the one that sees the press, and it reads the marker back out of the
   * button it landed on. One handler for the whole gesture, and no way for the track and
   * the marker to disagree about which of them was pressed.
   */
  function markerOffsetAt(event: PointerEvent): number | null {
    const target = event.target;
    if (!(target instanceof Element)) return null;
    const offset = target.closest<HTMLElement>('button.marker')?.dataset['offset'];
    if (offset === undefined) return null;
    const ms = Number(offset);
    return Number.isFinite(ms) ? ms : null;
  }

  /**
   * Begin a gesture on the track itself: a scrub, or — if it started on a marker — a press
   * that may turn out to be a click. Which one it was is decided on release, by distance.
   */
  function beginPlayhead(event: PointerEvent) {
    if (disabled) return;
    const markerMs = markerOffsetAt(event);
    gestureStart = { x: event.clientX, y: event.clientY };
    pressedMarkerMs = markerMs;
    if (markerMs !== null) {
      // A press on a marker does not scrub to the pointer. The marker's own lead-in is
      // where the user asked to be, and scrubbing to the pointer first would move the
      // playhead twice — out to the marker and back to the lead-in. A drag that starts on
      // a marker still scrubs, from the first pointer move onwards.
      dragging = 'playhead';
      capture(event);
      return;
    }
    begin('playhead', event);
  }

  function onPointerMove(event: PointerEvent) {
    if (dragging !== null) apply(dragging, event);
  }

  function onPointerUp(event: PointerEvent) {
    if (dragging === null) return;
    const start = gestureStart;
    const markerMs = pressedMarkerMs;
    dragging = null;
    gestureStart = null;
    pressedMarkerMs = null;
    if (track?.hasPointerCapture(event.pointerId)) {
      track.releasePointerCapture(event.pointerId);
    }
    // A press that did not travel is a click: on a marker it seeks to that marker's lead-in,
    // not to the pixel under the cursor. Without this the click would have scrubbed to the
    // pointer and left the marker — the thing the user aimed at — unused.
    if (markerMs !== null && start !== null && isClick(start, { x: event.clientX, y: event.clientY })) {
      onSeek(seekTargetMs(markerMs));
    }
  }

  function onHandleKey(handle: 'start' | 'end', event: KeyboardEvent) {
    const next = handleKeyRange(range, handle, event.key, durationMs, event.shiftKey);
    if (next === null) return;
    // The arrow keys scroll the pane otherwise, and Home and End jump the view.
    event.preventDefault();
    onRangeChange(next);
  }
</script>

<div class="timeline">
  <div class="legend">
    <span class="mono">{rangeLabel(range)}</span>
    <span class="muted">selection {rangeLengthLabel(range)}</span>
    <span class="playhead-readout mono">{formatPrecise(playheadMs)}</span>
  </div>

  <div
    class="track"
    bind:this={track}
    onpointerdown={(event) => beginPlayhead(event)}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    role="none"
  >
    <div class="selection" style="left: {percents.left}%; width: {percents.width}%"></div>

    {#each placed as marker (marker.event.id)}
      <!--
        The marker carries its own offset and no pointer handler of its own: the track owns the
        gesture, so a drag that starts on a marker scrubs like any other and the track
        reads the marker back out of this button when the press lands on it.
      -->
      <button
        class="marker"
        style="left: {marker.percent}%; --marker: {marker.color}"
        type="button"
        data-offset={marker.event.offset_ms}
        title="{marker.label} at {formatPrecise(marker.event.offset_ms)}"
        aria-label="Marker: {marker.label} at {formatPrecise(marker.event.offset_ms)}"
        disabled={disabled}
        onclick={(event) => {
          // A pointer click is the track's gesture (above), which also seeks to the marker on
          // release. Enter and Space on a focused marker produce a click with no pointer
          // behind it (`detail` is 0), which nothing else handles: without this the marker is
          // announced and tabbable and does nothing.
          if (event.detail === 0) onSeek(seekTargetMs(marker.event.offset_ms));
        }}
      ></button>
    {/each}

    <button
      class="handle start"
      style="left: {percents.left}%"
      type="button"
      role="slider"
      aria-label="Trim start"
      aria-valuemin={0}
      aria-valuemax={durationMs}
      aria-valuenow={range.startMs}
      aria-valuetext={formatPrecise(range.startMs)}
      disabled={disabled}
      onpointerdown={(event) => {
        event.stopPropagation();
        begin('start', event);
      }}
      onkeydown={(event) => onHandleKey('start', event)}
    ></button>

    <button
      class="handle end"
      style="left: {percents.left + percents.width}%"
      type="button"
      role="slider"
      aria-label="Trim end"
      aria-valuemin={0}
      aria-valuemax={durationMs}
      aria-valuenow={range.endMs}
      aria-valuetext={formatPrecise(range.endMs)}
      disabled={disabled}
      onpointerdown={(event) => {
        event.stopPropagation();
        begin('end', event);
      }}
      onkeydown={(event) => onHandleKey('end', event)}
    ></button>

    <div class="playhead" style="left: {playheadPercent}%"></div>
  </div>

  <p class="hint muted">
    Drag the track to scrub. Drag the two handles to set the trim range; focus one and use
    the arrow keys for a {KEYBOARD_STEP_MS}ms step, or Shift for {KEYBOARD_COARSE_STEP_MS}ms.
    Home and End put a handle at either end of the clip, PageUp and PageDown move it
    {KEYBOARD_PAGE_MS}ms.
    {#if placed.length > 0}
      Click a marker to seek to just before it ({placed.length} on this session's timeline).
    {/if}
  </p>
</div>

<style>
  .timeline {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    padding: var(--s-4) var(--s-4) var(--s-3);
    background: var(--surface-1);
  }

  .legend {
    display: flex;
    align-items: baseline;
    gap: var(--s-3);
    font: var(--t-small);
  }

  .playhead-readout {
    margin-left: auto;
    color: var(--accent);
  }

  .track {
    position: relative;
    height: 48px;
    margin: 0 7px;
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    background: var(--surface-2);
    cursor: crosshair;
    touch-action: none;
  }

  .selection {
    position: absolute;
    top: 0;
    bottom: 0;
    background: var(--accent-soft);
    border-top: 2px solid var(--accent);
    border-bottom: 2px solid var(--accent);
    pointer-events: none;
  }

  .playhead {
    position: absolute;
    top: -3px;
    bottom: -3px;
    width: 2px;
    margin-left: -1px;
    background: var(--text);
    box-shadow: 0 0 6px rgb(255 255 255 / 0.35);
    pointer-events: none;
  }

  .playhead::before {
    content: '';
    position: absolute;
    top: -2px;
    left: -3px;
    width: 8px;
    height: 8px;
    border-radius: var(--r-full);
    background: var(--text);
  }

  /*
   * A marker is a press target inside the track, so it is painted narrow and hit wide: the
   * button spans the full track height and 9px across, while the visible tick is 3px. Two
   * markers a second apart in a long session would otherwise be a pixel apart and impossible
   * to click at all.
   */
  .marker {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 9px;
    margin-left: -4.5px;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: pointer;
  }

  .marker::before {
    content: '';
    position: absolute;
    left: 3px;
    top: 3px;
    width: 3px;
    height: 13px;
    border-radius: 1px;
    background: var(--marker, var(--text-muted));
  }

  .marker:hover::before,
  .marker:focus-visible::before {
    box-shadow: 0 0 0 3px rgb(163 170 187 / 0.35);
  }

  .marker:focus-visible {
    outline: none;
  }

  .handle {
    position: absolute;
    top: -5px;
    bottom: -5px;
    width: 14px;
    margin-left: -7px;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: ew-resize;
  }

  /* The grab bar itself, so the hit area can be wider than what is painted. */
  .handle::before {
    content: '';
    position: absolute;
    left: 5px;
    top: 0;
    bottom: 0;
    width: 4px;
    border-radius: 2px;
    background: var(--accent);
    transition: box-shadow var(--d-fast) var(--ease);
  }

  .handle:hover::before,
  .handle:focus-visible::before {
    box-shadow: 0 0 0 3px var(--accent-soft), 0 0 0 4px var(--accent);
  }

  .handle:focus-visible {
    outline: none;
  }

  .hint {
    font: var(--t-small);
    color: var(--text-muted);
  }
</style>
