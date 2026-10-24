// @vitest-environment jsdom
/**
 * The library grid's keyboard navigation.
 *
 * The grid is `auto-fill`, so how many columns it has depends on the window's width — and
 * the count is measured rather than read on every key press, because reading it forces a
 * synchronous layout. What is tested here is that the count is right, and that it is
 * measured again when the window changes.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import ClipList from './ClipList.svelte';
import { toClipView } from '../clips';
import type { ClipView } from '../clips';
import type { ClipDto } from '../types';

function clip(id: number): ClipView {
  const dto: ClipDto = {
    id, path: `/c/${id}.mp4`, started_at_ms: 0, duration_ms: 12_345, size_bytes: 812 * 1024,
    codec: 'h264', favourite: false, created_at_ms: 1_756_000_000_000, title: null, game: null,
    tags: [],
  };
  return toClipView(dto, (path) => `asset://${path}`);
}

describe('the clip grid', () => {
  let app: ReturnType<typeof mount> | undefined;
  /** The grid's laid-out columns, as the browser would report them. */
  let layout: string;

  beforeEach(() => {
    layout = '200px 200px 200px';
    vi.spyOn(window, 'getComputedStyle').mockImplementation(
      () =>
        ({
          gridTemplateColumns: layout,
        }) as unknown as CSSStyleDeclaration,
    );
  });

  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    vi.restoreAllMocks();
    document.body.innerHTML = '';
  });

  async function mountGrid(
    count: number,
    extras: { combineIds?: number[]; onToggleCombine?: (id: number) => void } = {},
  ): Promise<HTMLElement> {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(ClipList, {
      target,
      props: {
        clips: Array.from({ length: count }, (_, i) => clip(i + 1)),
        thumbnails: {},
        selectedId: null,
        onSelect: () => {},
        onFavourite: () => {},
        onDelete: () => {},
        onCopy: () => {},
        onReveal: () => {},
        onDragOut: () => {},
        ...extras,
      },
    });
    // The grid is measured in an effect, so the component has to be given a tick before
    // the count is known.
    await tick();
    return target;
  }

  function rows(target: HTMLElement): HTMLButtonElement[] {
    return [...target.querySelectorAll<HTMLButtonElement>('button.row')];
  }

  it('moves down a whole row of cards at a time', async () => {
    const target = await mountGrid(6);
    const cards = rows(target);
    cards[0]?.focus();
    cards[0]?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    // Three columns, so a row down is three cards along.
    expect(document.activeElement).toBe(cards[3]);
  });

  it('measures the grid again when the window is resized', async () => {
    const target = await mountGrid(6);
    const cards = rows(target);
    layout = '200px 200px';
    window.dispatchEvent(new Event('resize'));
    cards[0]?.focus();
    cards[0]?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    // The window narrowed, so the grid has two columns now.
    expect(document.activeElement).toBe(cards[2]);
  });

  it('does not measure the grid again for every arrow key', async () => {
    const target = await mountGrid(6);
    const getStyle = vi.mocked(window.getComputedStyle);
    getStyle.mockClear();
    const cards = rows(target);
    cards[0]?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    // Measuring the grid forces a synchronous layout, which is not something a key press
    // should do: the count is cached, and read again only when the window changes.
    expect(getStyle).not.toHaveBeenCalled();
  });

  it('walks a first run through the three steps, with the chord this process installed', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(ClipList, {
      target,
      props: {
        clips: [],
        thumbnails: {},
        selectedId: null,
        onSelect: () => {},
        onFavourite: () => {},
        onDelete: () => {},
        onCopy: () => {},
        onReveal: () => {},
        onDragOut: () => {},
        hotkey: 'Ctrl+F9',
      },
    });
    await tick();

    const checklist = target.querySelector('[data-region="first-run-checklist"]');
    expect(checklist).not.toBeNull();
    expect(checklist!.querySelectorAll('ol li')).toHaveLength(3);
    // The chord is the live one, not the documented default: a checklist that named a chord the
    // process did not install would send the user to a key nothing answers.
    expect(checklist!.querySelector('kbd')?.textContent).toBe('Ctrl+F9');
    expect(target.textContent).not.toContain('not active');
  });

  it('says the hotkey is not active when nothing installed one', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(ClipList, {
      target,
      props: {
        clips: [],
        thumbnails: {},
        selectedId: null,
        onSelect: () => {},
        onFavourite: () => {},
        onDelete: () => {},
        onCopy: () => {},
        onReveal: () => {},
        onDragOut: () => {},
        hotkey: null,
      },
    });
    await tick();

    const checklist = target.querySelector('[data-region="first-run-checklist"]');
    expect(checklist!.querySelector('kbd')?.textContent).toBe('Ctrl+F8');
    expect(checklist!.textContent).toContain('not active');
  });

  it('ticks the clips the window selected for combining, and only those', async () => {
    // The tick has to be a real checkbox whose state comes from the window, not one kept inside
    // this component: the combine button lives in the header and reads the same list.
    const target = await mountGrid(3, { combineIds: [2] });
    const ticks = [...target.querySelectorAll<HTMLInputElement>('input.pick')];
    expect(ticks).toHaveLength(3);
    expect(ticks.map((tick) => tick.checked)).toEqual([false, true, false]);
    expect(ticks[1]?.getAttribute('aria-label')).toContain('Select');
  });

  it('reports a tick by id, so the window owns the selection', async () => {
    const onToggleCombine = vi.fn();
    const target = await mountGrid(3, { onToggleCombine });
    const ticks = [...target.querySelectorAll<HTMLInputElement>('input.pick')];
    ticks[0]!.click();
    expect(onToggleCombine).toHaveBeenCalledWith(1);
  });
});
