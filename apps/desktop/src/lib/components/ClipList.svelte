<script lang="ts">
  /**
   * The clip library: a grid of thumbnail cards, newest first. A card opens the clip,
   * and carries a favourite toggle and a menu (copy, show in folder, confirmed delete).
   *
   * The list does not sort. `list_clips` returns the clips newest first, ordered by the
   * wall-clock instant the row was written, and that decision belongs to the Rust side
   * (see `list_clips`' own note on why `clips.started_at` cannot be used for it). Sorting
   * again here would be a second opinion that could silently disagree.
   */
  import { shouldStartDrag } from '../clips';
  import type { ClipView } from '../clips';
  import { gridIndex } from '../nav';
  import type { NavKey } from '../nav';
  import Icon from './Icon.svelte';

  interface Props {
    clips: ClipView[];
    /** Clip id → `asset:` URL of its cached thumbnail. Missing entries show a placeholder. */
    thumbnails: Record<number, string>;
    selectedId: number | null;
    onSelect: (id: number) => void;
    onFavourite: (id: number, favourite: boolean) => void;
    onDelete: (id: number) => void;
    onCopy: (id: number) => void;
    onReveal: (id: number) => void;
    /** The pointer dragged a row: hand the clip's file to a native drag. */
    onDragOut: (id: number) => void;
    /** A filter is hiding clips, so an empty list is not "no clips". */
    filtering?: boolean;
    /** Clip ids ticked for combining, in the order they were ticked. */
    combineIds?: number[];
    /** Tick or untick one clip for combining. */
    onToggleCombine?: (id: number) => void;
    /** The clip hotkey in force, for the empty library's first-run checklist. */
    hotkey?: string | null;
  }

  let {
    clips,
    thumbnails,
    selectedId,
    onSelect,
    onFavourite,
    onDelete,
    onCopy,
    onReveal,
    onDragOut,
    filtering = false,
    combineIds = [],
    onToggleCombine = () => {},
    hotkey = null,
  }: Props = $props();

  /** Where the primary button went down on a row, until it is released or becomes a drag. */
  let press: { id: number; x: number; y: number } | null = $state(null);

  function pointerMove(event: PointerEvent) {
    if (press === null) return;
    if (shouldStartDrag(press, { x: event.clientX, y: event.clientY }, event.buttons)) {
      const id = press.id;
      press = null;
      onDragOut(id);
    }
  }

  /** The clip whose menu is open, and whether its delete is armed. */
  let menuFor = $state<number | null>(null);
  let confirming = $state(false);
  let list = $state<HTMLUListElement>();

  /**
   * How many columns the grid is laid out in.
   *
   * Read off the laid-out grid rather than counted from the CSS, because the grid is
   * `auto-fill`: the number depends on the window's width. It is measured when the grid
   * is laid out and again whenever the window is resized, because reading it forces a
   * synchronous layout — which is not something an arrow key should do.
   */
  let columns = $state(1);

  $effect(() => {
    const element = list;
    if (!element) return;
    const measure = () => {
      columns = Math.max(1, getComputedStyle(element).gridTemplateColumns.split(' ').length);
    };
    measure();
    window.addEventListener('resize', measure);
    return () => window.removeEventListener('resize', measure);
  });

  function closeMenu(refocus = false) {
    const id = menuFor;
    menuFor = null;
    confirming = false;
    if (refocus && id !== null) list?.querySelector<HTMLElement>(`[data-menu-for="${id}"]`)?.focus();
  }

  function toggleMenu(id: number) {
    if (menuFor === id) return closeMenu();
    menuFor = id;
    confirming = false;
  }

  /** Focus the first item of a menu as it opens, so the keyboard path continues inside it. */
  function focusFirst(node: HTMLElement) {
    node.querySelector<HTMLElement>('button')?.focus();
  }

  function menuKey(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.stopPropagation();
      closeMenu(true);
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const items = [...(event.currentTarget as HTMLElement).querySelectorAll<HTMLElement>('button')];
      const at = items.indexOf(document.activeElement as HTMLElement);
      const next = event.key === 'ArrowDown' ? at + 1 : at - 1;
      items[(next + items.length) % items.length]?.focus();
    }
  }

  function outside(event: PointerEvent) {
    if (menuFor !== null && !(event.target as Element).closest('.menu-wrap')) closeMenu();
  }

  /** Arrow keys move between cards; the column count is the one the grid was laid out with. */
  function cardKey(event: KeyboardEvent, index: number) {
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return;
    const target = gridIndex(index, event.key as NavKey, clips.length, columns);
    if (target < 0) return;
    event.preventDefault();
    list?.querySelectorAll<HTMLElement>('button.row')[target]?.focus();
  }
</script>

<svelte:window onpointerdown={outside} />

{#if clips.length === 0 && filtering}
  <div class="empty-state">
    <Icon name="search" size={32} />
    <p>No clips match the current search and filters.</p>
  </div>
{:else if clips.length === 0}
  <!-- The first run, and any empty library after it: the three steps, in the order they happen.
       The chord is the one this process really installed, not the documented default. -->
  <div class="empty-state first-run" data-region="first-run-checklist">
    <Icon name="clapperboard" size={40} />
    <div>
      <h3>Nothing recorded yet</h3>
      <ol>
        <li>
          <strong>Press Start recording</strong> in the bar above. The buffer fills in memory and
          writes nothing until something is kept.
        </li>
        <li>
          <strong>Play.</strong> Press <kbd>{hotkey ?? 'Ctrl+F8'}</kbd> — or
          <strong>Save clip</strong> — and the last 30 seconds plus the next 5 are written.
        </li>
        <li>
          <strong>Find it here.</strong> The clip is indexed and appears in this grid; the Storage
          page names the folder it is in.
        </li>
      </ol>
      {#if hotkey === null}
        <p class="muted">The clip hotkey is not active in this window; the buttons above work.</p>
      {/if}
    </div>
  </div>
{:else}
  <ul bind:this={list}>
    {#each clips as clip, index (clip.id)}
      {@const thumbnail = thumbnails[clip.id]}
      <li class:selected={clip.id === selectedId} class:menu-open={menuFor === clip.id}>
        <button
          class="row"
          type="button"
          aria-current={clip.id === selectedId}
          onclick={() => onSelect(clip.id)}
          onkeydown={(e) => cardKey(e, index)}
          onpointerdown={(e) => {
            if (e.button === 0) press = { id: clip.id, x: e.clientX, y: e.clientY };
          }}
          onpointermove={pointerMove}
          onpointerup={() => (press = null)}
          onpointerleave={pointerMove}
          title="Drag to copy this clip's file into another app"
        >
          <span class="frame">
            {#if thumbnail}
              <img class="thumb" src={thumbnail} alt="" loading="lazy" decoding="async" draggable="false" />
            {:else}
              <span class="thumb placeholder" aria-hidden="true"><Icon name="video" size={28} /></span>
            {/if}
            <span class="duration">{clip.durationLabel}</span>
          </span>
          <span class="meta">
            <span class="name">{clip.displayName}</span>
            <span class="sub muted">
              {#if clip.game}{clip.game} · {/if}{clip.sizeLabel} · {clip.createdAtLabel}
            </span>
            {#if clip.tags.length > 0}
              <span class="tags">
                {#each clip.tags as tag (tag)}<span class="tag">#{tag}</span>{/each}
              </span>
            {/if}
          </span>
        </button>

        <div class="actions">
          <input
            class="pick"
            type="checkbox"
            checked={combineIds.includes(clip.id)}
            aria-label={`Select ${clip.displayName} to combine`}
            title="Select to combine with another clip"
            onchange={() => onToggleCombine(clip.id)}
          />
          <button
            class="icon star"
            type="button"
            aria-pressed={clip.favourite}
            aria-label={clip.favourite ? 'Remove from favourites' : 'Add to favourites'}
            title="Favourites are exempt from the storage policy's cap and age rules"
            onclick={() => onFavourite(clip.id, !clip.favourite)}
          >
            <Icon name="star" size={16} />
          </button>

          <div class="menu-wrap">
            <button
              class="icon more"
              type="button"
              aria-label="More actions"
              aria-haspopup="true"
              aria-expanded={menuFor === clip.id}
              data-menu-for={clip.id}
              onclick={() => toggleMenu(clip.id)}
            >
              <Icon name="ellipsis" size={16} />
            </button>
            {#if menuFor === clip.id}
              <div class="menu" role="menu" tabindex="-1" aria-label="Clip actions" use:focusFirst onkeydown={menuKey}>
                {#if confirming}
                  <button
                    role="menuitem"
                    class="danger"
                    type="button"
                    title="The index row is deleted first, then the file"
                    onclick={() => {
                      onDelete(clip.id);
                      closeMenu();
                    }}
                  >
                    <Icon name="trash-2" size={15} /> Delete clip
                  </button>
                  <button role="menuitem" type="button" onclick={() => (confirming = false)}>Cancel</button>
                {:else}
                  <button
                    role="menuitem"
                    type="button"
                    title="Copy the clip file to the clipboard"
                    aria-label="Copy clip file"
                    onclick={() => {
                      onCopy(clip.id);
                      closeMenu(true);
                    }}
                  >
                    <Icon name="copy" size={15} /> Copy file
                  </button>
                  <button
                    role="menuitem"
                    type="button"
                    title="Show the clip file in the file manager"
                    aria-label="Show in folder"
                    onclick={() => {
                      onReveal(clip.id);
                      closeMenu(true);
                    }}
                  >
                    <Icon name="folder-open" size={15} /> Show in folder
                  </button>
                  <button
                    role="menuitem"
                    class="danger"
                    type="button"
                    title="Delete this clip"
                    onclick={() => (confirming = true)}
                  >
                    <Icon name="trash-2" size={15} /> Delete
                  </button>
                {/if}
              </div>
            {/if}
          </div>
        </div>
      </li>
    {/each}
  </ul>
{/if}

<style>
  /* The first-run checklist: left-aligned prose and a numbered list, unlike the one-line
     "nothing matches" state, because its content is instructions rather than a statement. */
  .first-run {
    align-items: flex-start;
    text-align: left;
  }

  .first-run > div {
    max-width: 56ch;
  }

  .first-run h3 {
    margin: 0 0 var(--s-2);
    font: var(--t-heading);
    color: var(--text);
  }

  .first-run ol {
    margin: 0;
    padding-left: var(--s-5);
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    font: var(--t-small);
    color: var(--text-muted);
  }

  .first-run strong {
    color: var(--text);
  }

  .first-run kbd {
    font: 400 12px/1.4 var(--font-mono);
    padding: 1px 5px;
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    background: var(--surface-2);
    color: var(--text);
  }

  ul {
    /* The grid scrolls inside the view: it takes what is left and no more. */
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    margin: 0;
    padding: var(--s-2) var(--s-5) var(--s-5);
    list-style: none;
    display: grid;
    /* 3 columns at the 940px minimum window, 5 at the 1280px default. */
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    grid-auto-rows: max-content;
    gap: var(--s-5) var(--s-4);
  }

  li {
    position: relative;
    min-width: 0;
    border-radius: var(--r-md);
  }

  li.menu-open {
    z-index: 2;
  }

  .row {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    width: 100%;
    padding: 0;
    background: transparent;
    border: 0;
    border-radius: var(--r-md);
    text-align: left;
  }

  .row:hover:not(:disabled),
  .row:active:not(:disabled) {
    background: transparent;
    border-color: transparent;
  }

  .row:focus-visible {
    box-shadow: 0 0 0 4px var(--accent-soft);
  }

  .frame {
    position: relative;
    display: block;
    aspect-ratio: 16 / 9;
    overflow: hidden;
    border-radius: var(--r-md);
    border: 1px solid var(--border);
    background: var(--surface-2);
    transition: border-color var(--d-fast) var(--ease);
  }

  li:hover .frame {
    border-color: var(--border-strong);
  }

  li.selected .frame {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }

  .thumb {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
    background: #000;
  }

  .thumb.placeholder {
    display: grid;
    place-items: center;
    background: var(--surface-2);
    color: var(--text-faint);
  }

  .duration {
    position: absolute;
    right: var(--s-2);
    bottom: var(--s-2);
    padding: 1px 6px;
    border-radius: var(--r-sm);
    background: rgb(0 0 0 / 0.78);
    color: #fff;
    font: var(--t-micro);
    font-feature-settings: 'tnum';
  }

  .meta {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    padding: 0 2px;
  }

  .name {
    font: var(--t-body);
    font-weight: 550;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sub {
    font: var(--t-small);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .tags {
    display: flex;
    gap: 4px;
    overflow: hidden;
    margin-top: 2px;
  }

  .tag {
    font: var(--t-micro);
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: var(--r-full);
    padding: 0 6px;
    white-space: nowrap;
  }

  /*
   * The favourite and the menu float over the thumbnail. They show for the card under the
   * pointer, with focus inside it, and always for a favourited clip's star; hiding is by
   * opacity so nothing leaves the tab order or the accessibility tree, and touch devices
   * (no hover) always see them.
   */
  .actions {
    position: absolute;
    top: var(--s-2);
    left: var(--s-2);
    right: var(--s-2);
    display: flex;
    justify-content: space-between;
    pointer-events: none;
  }

  /* The combine tick: same treatment as the icon buttons (fades in with the card, readable
     against a thumbnail), but a checkbox rather than a glyph. */
  .actions .pick {
    pointer-events: auto;
    width: 18px;
    height: 18px;
    margin: 6px;
    accent-color: var(--accent);
    opacity: 0;
    transition: opacity var(--d-fast) var(--ease);
  }

  li:hover .pick,
  .pick:checked,
  .pick:focus-visible {
    opacity: 1;
  }

  .actions button {
    pointer-events: auto;
    width: 30px;
    height: 30px;
    background: rgb(12 13 18 / 0.8);
    border-color: transparent;
    backdrop-filter: blur(6px);
    opacity: 0;
    transition:
      opacity var(--d-fast) var(--ease),
      background var(--d-fast) var(--ease);
  }

  li:hover .actions button,
  li:focus-within .actions button,
  li.menu-open .actions button,
  .actions .star[aria-pressed='true'] {
    opacity: 1;
  }

  .actions button:hover:not(:disabled) {
    background: rgb(12 13 18 / 0.95);
    border-color: var(--border-strong);
  }

  .star {
    color: var(--text);
  }

  .star[aria-pressed='true'] {
    color: var(--star);
  }

  .star[aria-pressed='true'] :global(svg) {
    fill: currentColor;
  }

  @media (hover: none) {
    .actions button {
      opacity: 1;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .frame,
    .actions button {
      transition: none;
    }
  }

  .menu-wrap {
    position: relative;
    pointer-events: auto;
  }

  .menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 160px;
    padding: var(--s-1);
    background: var(--surface-3);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-pop);
  }

  .menu button {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    width: auto;
    height: auto;
    padding: 6px 10px;
    text-align: left;
    background: transparent;
    border-color: transparent;
    backdrop-filter: none;
    opacity: 1;
    font: var(--t-small);
  }

  .menu button:hover:not(:disabled) {
    background: var(--surface-2);
  }

</style>
