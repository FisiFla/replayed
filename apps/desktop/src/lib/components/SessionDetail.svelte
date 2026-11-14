<script lang="ts">
  /**
   * One session: what it is, its timeline, and the range to cut a clip out of.
   *
   * This is a **range picker**, not a player. A session file is one long recording and
   * `ClipDetail` is where a clip is played; what a session's timeline is for is choosing the
   * part worth keeping, which is why the markers are drawn here — they are the reason to
   * scrub a session at all, and they arrive measured on the same media clock the range is.
   *
   * The cursor is not playback: it is where the last marker click landed, so the timeline
   * shows what a click did. Nothing here pretends to play, and there is no `<video>` to
   * drive.
   */
  import Icon from './Icon.svelte';
  import MatchTimeline from './MatchTimeline.svelte';
  import Timeline from './Timeline.svelte';
  import {
    canExtract,
    describeSession,
    extractionBlocker,
    sessionLengthLabel,
    sessionTitle,
  } from '../sessions';
  import type { SessionView } from '../sessions';
  import { markerColor } from '../markers';
  import { formatBytes, formatPrecise } from '../time';
  import { validateRange } from '../trim';
  import type { MatchDto, SessionEvent, TrimRange } from '../types';

  interface Props {
    session: SessionView;
    /** The session's markers, in the store's media-time order. */
    events: SessionEvent[];
    /** The session split into matches (Full Match segments). */
    matches: MatchDto[];
    busy: boolean;
    onExtract: (range: TrimRange) => void;
    onExtractMatch: (index: number) => void;
    onFavourite: (favourite: boolean) => void;
    onDelete: () => void;
  }

  let { session, events, matches, busy, onExtract, onExtractMatch, onFavourite, onDelete }: Props = $props();

  /**
   * The range to cut out. The whole session until the user says otherwise.
   *
   * Seeded empty and set in the effect below rather than in this initialiser, which would
   * capture the session's length as it was on the first render and never notice it change —
   * and it does change: a session's length is only known once its segments have been
   * concatenated, so a view watching a recording finish sees it go from unknown to real.
   */
  let range = $state<TrimRange>({ startMs: 0, endMs: 0 });
  /** Where the last marker click put the cursor. Not playback — see the header note. */
  let cursorMs = $state(0);

  const blocker = $derived(extractionBlocker(session));
  const extractable = $derived(canExtract(session));
  /**
   * Whether the range can be cut, by the same rule the clip page uses.
   *
   * One rule for both places a cut is asked for: a selection the command would refuse is
   * not offered here either, and the reason is shown rather than discovered as an error.
   */
  const validation = $derived(validateRange(range, session.durationMs));

  /** Cut the selection, but only one the command will accept. */
  function extract() {
    if (validation.ok) onExtract(validation.range);
  }

  /**
   * The session's length, as a number of its own.
   *
   * The `session` prop is a new object after every library refresh (`toSessionViews` rebuilds
   * every row), so an effect that read `session.durationMs` itself would run again each time
   * even though the length had not moved — a clip hotkey, a favourite toggle or an extraction
   * all refresh the library, and each would snap the user's handles back to the whole session
   * and the cursor to 0 mid-edit. A `$derived` number only notifies when the number changes,
   * which is the one thing the reset below is about.
   */
  const length = $derived(session.durationMs);

  // A change of axis resets both, so neither can be left sitting past the end of a shorter
  // recording. Reads only `length`, so it runs when the session's length changes and at no
  // other time — not on a refresh that brought the same length back, and not by itself.
  $effect(() => {
    range = { startMs: 0, endMs: length };
    cursorMs = 0;
  });

  /**
   * Whether the delete button is armed. Deleting a session removes its row, its scratch
   * segments and the concatenated file, none of which comes back; the clips already cut from it
   * survive, the rest of the recording does not. The session list asks twice before it does
   * that, and so does this page: a second click on the same spot would otherwise be the whole
   * of the safeguard, next to a Favourite button that is harmless to hit.
   */
  let confirmingDelete = $state(false);

  function confirmDelete() {
    confirmingDelete = false;
    onDelete();
  }
</script>

<section class="detail">
  <header>
    <div class="heading">
      <h2>{sessionTitle(session)}</h2>
      <span class="muted">{describeSession(session)}</span>
    </div>
    <div class="toolbar">
      <button
        type="button"
        class="fav"
        aria-pressed={session.favourite}
        onclick={() => onFavourite(!session.favourite)}
      >
        <Icon name="star" size={14} />
        {session.favourite ? 'Un-favourite' : 'Favourite'}
      </button>
      {#if confirmingDelete}
        <button type="button" class="danger" disabled={busy} onclick={confirmDelete}>
          <Icon name="trash-2" size={14} />
          Confirm delete
        </button>
        <button type="button" onclick={() => (confirmingDelete = false)}>Cancel</button>
      {:else}
        <button
          type="button"
          class="danger"
          disabled={busy}
          onclick={() => (confirmingDelete = true)}
        >
          <Icon name="trash-2" size={14} />
          Delete session
        </button>
      {/if}
    </div>
  </header>

  <dl class="facts">
    <div><dt>Mode</dt><dd class="mono">{session.mode}</dd></div>
    <div><dt>Media length</dt><dd class="mono">{sessionLengthLabel(session)}</dd></div>
    <div><dt>On disk</dt><dd class="mono">{formatBytes(session.sizeBytes)}</dd></div>
    <div><dt>Markers</dt><dd class="mono">{events.length}</dd></div>
    <div class="wide">
      <dt>Session directory</dt>
      <dd class="mono path">{session.scratchDir}</dd>
    </div>
  </dl>

  <div class="card">
    {#if session.hasLength}
      <Timeline
        durationMs={session.durationMs}
        playheadMs={cursorMs}
        {range}
        markers={events}
        onSeek={(ms) => (cursorMs = ms)}
        onRangeChange={(next) => (range = next)}
      />
    {:else}
      <p class="empty">
        This session's media length is not known, so there is no timeline to draw. That is the
        case while it is still recording, and for a session recovered after a crash.
      </p>
    {/if}

    <div class="controls">
      <button
        type="button"
        class="primary"
        disabled={!extractable || busy || !validation.ok}
        onclick={extract}
      >
        <Icon name="scissors" size={14} />
        {busy ? 'Working…' : 'Extract clip'}
      </button>
      <span class="muted mono range">
        {formatPrecise(range.startMs)}–{formatPrecise(range.endMs)}
        ({formatPrecise(range.endMs - range.startMs)})
      </span>
    </div>

    {#if !validation.ok}
      <p class="invalid">{validation.reason}</p>
    {/if}

    {#if blocker !== null}
      <p class="blocked">{blocker}</p>
    {:else}
      <p class="hint muted">
        The cut is lossless (<code class="mono">-c copy</code>), so it snaps to the nearest
        keyframe before the start rather than re-encoding: the clip may begin slightly earlier
        than the range, and it is never re-encoded.
      </p>
    {/if}
  </div>

  <MatchTimeline
    {matches}
    durationMs={session.durationMs}
    extractable={extractable}
    {busy}
    onExtract={onExtractMatch}
  />

  {#if events.length > 0}
    <section class="card">
      <h3>Markers</h3>
      <table class="markers">
        <caption class="muted">Every marker on this session, in media time.</caption>
        <thead>
          <tr><th>Marker</th><th>At</th><th>Clip</th></tr>
        </thead>
        <tbody>
          {#each events as event (event.id)}
            <tr>
              <td>
                <span class="dot" style:background={markerColor(event.kind)} aria-hidden="true"></span>
                {event.kind}
              </td>
              <td class="mono">{formatPrecise(event.offset_ms)}</td>
              <td class="mono muted">{event.clip_id === null ? '—' : `#${event.clip_id}`}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </section>
  {/if}
</section>

<style>
  .detail {
    display: flex;
    flex-direction: column;
    gap: var(--s-4);
    padding: var(--s-4) var(--s-5) var(--s-5);
    overflow-y: auto;
  }

  header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: var(--s-3);
  }

  .heading {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  h2 {
    margin: 0;
    font: var(--t-title);
  }

  .heading .muted {
    font: var(--t-small);
  }

  h3 {
    margin: 0;
    font: var(--t-heading);
  }

  .toolbar {
    display: flex;
    gap: var(--s-2);
  }

  .toolbar button,
  .controls button {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  .fav[aria-pressed='true'] {
    color: var(--star);
    border-color: var(--star);
  }

  .fav[aria-pressed='true'] :global(svg) {
    fill: currentColor;
  }

  .card {
    display: flex;
    flex-direction: column;
    gap: var(--s-3);
    padding: var(--s-4);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-lg);
  }

  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: var(--s-2);
    margin: 0;
  }

  .facts div {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--s-2) var(--s-3);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
  }

  .facts dt {
    color: var(--text-muted);
    font: var(--t-micro);
  }

  .facts dd {
    margin: 0;
    font: var(--t-small);
  }

  /*
   * The directory spans the grid. An absolute path is one unbreakable token several times
   * a column wide, and `overflow-wrap: anywhere` breaks it wherever it has to — so the row
   * that carries the path is the one row that must not be squeezed into a column.
   */
  .facts .wide {
    grid-column: 1 / -1;
  }

  .path {
    overflow-wrap: anywhere;
  }

  .markers {
    width: 100%;
    border-collapse: collapse;
    font: var(--t-small);
  }

  .markers caption {
    text-align: left;
    padding-bottom: var(--s-2);
    font: var(--t-small);
  }

  .markers th {
    color: var(--text-muted);
    font: var(--t-micro);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .markers th,
  .markers td {
    text-align: left;
    padding: 6px var(--s-3) 6px 0;
    border-bottom: 1px solid var(--border);
  }

  .markers tbody tr:last-child td {
    border-bottom: 0;
  }

  .markers td:first-child {
    display: flex;
    align-items: center;
    gap: var(--s-2);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    flex-wrap: wrap;
  }

  .primary {
    font-weight: 600;
  }

  .range {
    font: var(--t-small);
  }

  .invalid {
    color: var(--warn);
  }

  .blocked {
    margin: 0;
    padding: var(--s-2) var(--s-3);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    background: var(--surface-3);
    font: var(--t-small);
  }

  .hint {
    margin: 0;
    font: var(--t-small);
  }

  .empty {
    margin: 0;
    color: var(--text-muted);
  }
</style>
