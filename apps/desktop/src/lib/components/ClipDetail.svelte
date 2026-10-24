<script lang="ts">
  /**
   * One clip, with its player and its trim controls.
   *
   * Playback is an HTML5 `<video>` pointed at an `asset:` URL (spec §9) — the Tauri asset
   * protocol, scoped to the clips directory, serving range requests so the element's own
   * scrubbing works. There is no streaming code in this application and there must not be.
   */
  import Icon from './Icon.svelte';
  import Timeline from './Timeline.svelte';
  import { EXPORT_PRESETS, parseTagInput } from '../clips';
  import type { ClipView } from '../clips';
  import { formatDateTime } from '../time';
  import { fullRange, rangeLabel, rangeLengthLabel, validateRange } from '../trim';
  import type { TrimRange } from '../types';

  interface Props {
    clip: ClipView;
    /** A command is in flight; both actions are held while it is. */
    busy: boolean;
    onTrim: (range: TrimRange) => void;
    onFavourite: (favourite: boolean) => void;
    onRename: (title: string) => void;
    onTags: (tags: string[]) => void;
    onCopy: () => void;
    onReveal: () => void;
    /** Re-encode this clip with an export preset (`discord`, `vertical`, `gif`). */
    onExport: (preset: string) => void;
  }

  let {
    clip,
    busy,
    onTrim,
    onFavourite,
    onRename,
    onTags,
    onCopy,
    onReveal,
    onExport,
  }: Props = $props();

  // svelte-ignore state_referenced_locally — keyed on the clip id by App (see `range`).
  let titleDraft = $state(clip.title ?? '');
  let tagDraft = $state('');

  const titleChanged = $derived(titleDraft.trim() !== (clip.title ?? ''));

  function saveTitle() {
    if (titleChanged) onRename(titleDraft);
  }

  function addTags() {
    const added = parseTagInput(tagDraft);
    if (added.length === 0) return;
    tagDraft = '';
    onTags([...new Set([...clip.tags, ...added])]);
  }

  let video = $state<HTMLVideoElement | null>(null);
  let playheadMs = $state(0);
  // svelte-ignore state_referenced_locally — deliberate: this component is keyed on the
  // clip id by App, so a different clip remounts it rather than re-rendering it with a new
  // prop. Reading the prop once here is therefore the whole clip, which is what this
  // initial value is meant to be.
  let range = $state<TrimRange>(fullRange(clip.durationMs));
  let playing = $state(false);

  const validation = $derived(validateRange(range, clip.durationMs));

  /**
   * The playhead follows the element, never a timer of this window's own: while the video
   * plays it is read once per animation frame, and `ontimeupdate` covers the frames in
   * between plus every seek and pause. A timer would drift away from the decoder — and the
   * number this produces is the number that gets sent to `trim_clip`, so drift would mean
   * cutting somewhere other than where the user pointed.
   */
  $effect(() => {
    if (!playing) return;
    let frame = 0;
    const tick = () => {
      playheadMs = (video?.currentTime ?? 0) * 1000;
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  });

  function syncPlayhead() {
    playheadMs = (video?.currentTime ?? 0) * 1000;
  }

  function seek(ms: number) {
    playheadMs = ms;
    if (video) video.currentTime = ms / 1000;
  }

  function trim() {
    if (validation.ok) onTrim(validation.range);
  }
</script>

<div class="clip">
  <header class="head">
    <div class="titles">
      <h1>{clip.displayName}</h1>
      <p class="mono path" title={clip.path}>{clip.path}</p>
    </div>
    <div class="toolbar">
      <button
        class="star"
        type="button"
        aria-pressed={clip.favourite}
        title={clip.favourite ? 'Remove from favourites' : 'Protect this clip from cleanup'}
        onclick={() => onFavourite(!clip.favourite)}
        disabled={busy}
      >
        <Icon name="star" size={16} />
        Favourite
      </button>
      <button type="button" onclick={onCopy}><Icon name="copy" size={16} />Copy file</button>
      <button type="button" onclick={onReveal}>
        <Icon name="folder-open" size={16} />Show in folder
      </button>
    </div>
  </header>

  <div class="layout">
    <div class="main">
      <!--
        svelte-ignore a11y_media_has_caption — there is no caption source to point a <track> at.
        This element plays the user's own gameplay recording, and nothing in this application
        transcribes audio; an empty caption track would satisfy the linter and tell a screen
        reader nothing.
      -->
      {#if clip.isScreenshot}
        <!-- A still image: no player, no timeline, no trim and no export presets. The file is the
             picture, shown at its own aspect ratio. -->
        <img class="player still" src={clip.assetUrl} alt={`Screenshot: ${clip.displayName}`} />
        <section class="panel trim">
          <div class="trim-head">
            <h2>Screenshot</h2>
            <div class="selection">
              <span class="muted">one frame, saved as a lossless PNG</span>
            </div>
          </div>
          <p class="lossless">
            <strong>A still image.</strong> There is nothing to trim or export: the file is the
            frame exactly as it was on screen, and it can be copied, revealed, tagged and
            favourited like any clip.
          </p>
        </section>
      {:else}
      <video
        class="player"
        bind:this={video}
        src={clip.assetUrl}
        controls
        preload="metadata"
        ontimeupdate={syncPlayhead}
        onseeked={syncPlayhead}
        onplay={() => (playing = true)}
        onpause={() => (playing = false)}
        onended={() => (playing = false)}
      ></video>

      <Timeline
        durationMs={clip.durationMs}
        {playheadMs}
        {range}
        onSeek={seek}
        onRangeChange={(next) => (range = next)}
      />

      <section class="panel trim">
        <div class="trim-head">
          <h2>Trim</h2>
          <div class="selection">
            <span class="mono">{rangeLabel(range)}</span>
            <span class="muted">→ {rangeLengthLabel(range)} selected</span>
          </div>
        </div>

        <div class="actions">
          <button class="primary" type="button" disabled={!validation.ok || busy} onclick={trim}>
            <Icon name="scissors" size={16} />
            {busy ? 'Working…' : 'Trim to a new clip'}
          </button>
          <button
            type="button"
            class="ghost"
            disabled={busy}
            onclick={() => (range = fullRange(clip.durationMs))}
          >
            Select all
          </button>
        </div>

        {#if !validation.ok}
          <p class="invalid">{validation.reason}</p>
        {/if}

        <p class="lossless">
          <strong>Lossless.</strong> The selection is cut with an ffmpeg stream copy
          (<code>-c copy</code>), so nothing is re-encoded — but the cut can only land where the
          stream allows, and the file can differ by a fraction of a second from the selection.
          It is written next to the original as
          <code>{clip.name.replace(/\.[^.]+$/, '')}.trim-START-END.&lt;ext&gt;</code>; the
          original is never modified.
        </p>
      </section>

      <section class="panel trim">
        <div class="trim-head">
          <h2>Export</h2>
          <div class="selection">
            <span class="muted">a re-encode, written beside the clip</span>
          </div>
        </div>

        <div class="actions">
          {#each EXPORT_PRESETS as preset (preset.value)}
            <button type="button" disabled={busy} onclick={() => onExport(preset.value)}>
              {preset.label}
            </button>
          {/each}
        </div>

        <p class="lossless">
          <strong>Re-encoded, on purpose.</strong> Unlike a trim, an export has to decode and
          encode: that is how the frame gets smaller, vertical or animated. The file is written
          next to the clip and is <em>not</em> added to the library — it is made to be uploaded,
          not to be another copy under the storage cap. The original is never modified.
        </p>
      </section>
      {/if}
    </div>

    <aside class="side">
      <section class="card box">
        <h2>Details</h2>
        <div class="facts">
          {#if clip.game}<span class="chip">{clip.game}</span>{/if}
          <span class="chip">{clip.durationLabel}</span>
          <span class="chip">{clip.sizeLabel}</span>
          <span class="chip">{clip.codec}</span>
          <span class="chip">{formatDateTime(clip.createdAtMs)}</span>
        </div>
      </section>

      <section class="card box">
        <h2>Title</h2>
        <form
          class="row"
          onsubmit={(e) => {
            e.preventDefault();
            saveTitle();
          }}
        >
          <input
            type="text"
            aria-label="Clip title"
            placeholder={clip.name}
            maxlength="120"
            bind:value={titleDraft}
            disabled={busy}
          />
          <button type="submit" disabled={busy || !titleChanged}>Rename</button>
        </form>
        <p class="muted note">The title is a label in this library; the file keeps its name.</p>
      </section>

      <section class="card box">
        <h2>Tags</h2>
        {#if clip.tags.length > 0}
          <div class="row">
            {#each clip.tags as tag (tag)}
              <span class="tag">
                #{tag}
                <button
                  type="button"
                  class="x ghost"
                  aria-label={`Remove tag ${tag}`}
                  disabled={busy}
                  onclick={() => onTags(clip.tags.filter((t) => t !== tag))}
                  ><Icon name="x" size={12} /></button
                >
              </span>
            {/each}
          </div>
        {/if}
        <form
          class="row"
          onsubmit={(e) => {
            e.preventDefault();
            addTags();
          }}
        >
          <input
            type="text"
            aria-label="Add tags"
            placeholder="Add tags, comma separated"
            maxlength="200"
            bind:value={tagDraft}
            disabled={busy}
          />
          <button type="submit" disabled={busy || parseTagInput(tagDraft).length === 0}>Add</button>
        </form>
      </section>
    </aside>
  </div>
</div>

<style>
  .clip {
    display: flex;
    flex-direction: column;
    gap: var(--s-4);
    padding: var(--s-2) var(--s-5) var(--s-5);
  }

  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: var(--s-3) var(--s-4);
  }

  .titles {
    min-width: 0;
    flex: 1 1 320px;
  }

  h1 {
    font: var(--t-title);
    overflow-wrap: anywhere;
  }

  .path {
    margin-top: var(--s-1);
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .toolbar {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s-2);
  }

  .toolbar button {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  .star[aria-pressed='true'] {
    color: var(--star);
    border-color: var(--star);
  }

  .star[aria-pressed='true'] :global(svg) {
    fill: currentColor;
  }

  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 320px;
    gap: var(--s-5);
    align-items: start;
  }

  /* Below 1100px the details column drops under the player instead of squeezing it. */
  @media (max-width: 1100px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    background: var(--surface-1);
    border: 1px solid var(--border);
    border-radius: var(--r-lg);
    overflow: hidden;
  }

  .player {
    display: block;
    width: 100%;
    /*
     * The box is width-driven and capped in height, so the height that *paints* is normally
     * the media's own. `aspect-ratio` is what keeps it there when there is no media yet:
     * an unloaded `<video>` has an intrinsic size of 300x150, and in this column it is a
     * flex item — so without this the pane's flexbox shrinks the player to its 150px
     * intrinsic height until metadata arrives, and a clip whose file cannot be read leaves
     * it that way for good. `object-fit: contain` letterboxes inside the capped box.
     */
    aspect-ratio: 16 / 9;
    max-height: 44vh;
    object-fit: contain;
    background: #000;
  }

  .side {
    display: flex;
    flex-direction: column;
    gap: var(--s-3);
    min-width: 0;
  }

  .box {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    padding: var(--s-3) var(--s-4);
  }

  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
  }

  .row input {
    flex: 1 1 140px;
    min-width: 0;
    padding: 5px 8px;
  }

  .note {
    font: var(--t-small);
  }

  .tag {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    font: var(--t-small);
    background: var(--accent-soft);
    border-radius: var(--r-full);
    padding: 1px 4px 1px 10px;
  }

  .x {
    display: inline-grid;
    place-items: center;
    border: 0;
    padding: 3px;
    border-radius: var(--r-full);
  }

  .facts {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  /* A screenshot at its own aspect ratio, capped so a 4K capture still fits the column. */
  .player.still {
    width: 100%;
    height: auto;
    max-height: 60vh;
    object-fit: contain;
    background: #000;
  }

  .trim {
    border-top: 1px solid var(--border);
    padding: var(--s-3) var(--s-4) var(--s-4);
  }

  .trim-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: var(--s-2);
  }

  .selection {
    display: flex;
    align-items: baseline;
    gap: 10px;
    font: var(--t-small);
  }

  .actions {
    display: flex;
    gap: var(--s-2);
  }

  .actions button {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  .invalid {
    color: var(--warn);
  }

  .lossless {
    font: var(--t-small);
    color: var(--text);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-left: 3px solid var(--accent-strong);
    border-radius: var(--r-sm);
    padding: var(--s-2) var(--s-3);
  }
</style>
