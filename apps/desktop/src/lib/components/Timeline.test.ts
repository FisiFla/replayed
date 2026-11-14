// @vitest-environment jsdom
/**
 * The scrubber's gestures, against a real DOM.
 *
 * The arithmetic behind the timeline is tested without a DOM in `trim.test.ts` and
 * `markers.test.ts`. What is tested here is the part those cannot reach: that a press on
 * a marker reaches the component at all, that a click on one seeks to the marker's lead-in
 * rather than to the pixel under the cursor, and that the handles answer the keys the
 * ARIA slider pattern asks for.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import Timeline from './Timeline.svelte';
import { MARKER_LEAD_MS } from '../markers';
import type { SessionEvent, TrimRange } from '../types';

/**
 * jsdom implements neither pointer capture nor the events that use it, and the scrubber's
 * gesture handling calls all three. Without these the component throws on the first press.
 */
const elementProto = Element.prototype as unknown as Record<string, unknown>;
elementProto['setPointerCapture'] ??= () => {};
elementProto['releasePointerCapture'] ??= () => {};
elementProto['hasPointerCapture'] ??= () => false;

/** The track's box. jsdom does no layout, so the geometry is stated here. */
const TRACK_WIDTH = 600;
const TRACK_HEIGHT = 48;

function marker(offsetMs: number): SessionEvent {
  return { id: offsetMs, kind: 'kill', offset_ms: offsetMs, payload: null, clip_id: null };
}

describe('the timeline', () => {
  let app: ReturnType<typeof mount> | undefined;

  beforeEach(() => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: TRACK_WIDTH,
      bottom: TRACK_HEIGHT,
      width: TRACK_WIDTH,
      height: TRACK_HEIGHT,
      toJSON: () => ({}),
    });
  });

  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    vi.restoreAllMocks();
    document.body.innerHTML = '';
  });

  /** Mount a scrubber over a one-minute clip and collect what it reports. */
  async function mountTimeline(options: { markers?: SessionEvent[]; range?: TrimRange } = {}) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    const seeks: number[] = [];
    const ranges: TrimRange[] = [];
    app = mount(Timeline, {
      target,
      props: {
        durationMs: 60_000,
        playheadMs: 0,
        range: options.range ?? { startMs: 0, endMs: 60_000 },
        markers: options.markers ?? [],
        onSeek: (ms: number) => {
          seeks.push(ms);
        },
        onRangeChange: (next: TrimRange) => {
          ranges.push(next);
        },
      },
    });
    // The track's own reference is bound in an effect, so the component has to be given a
    // tick before a press can be measured against it.
    await tick();
    const query = <T extends Element>(selector: string): T => {
      const found = target.querySelector<T>(selector);
      expect(found, selector).not.toBeNull();
      return found as T;
    };
    return { target, seeks, ranges, query };
  }

  /** A pointer event at a position on the track. */
  function pointer(target: Element, type: string, x: number, y = 12): void {
    target.dispatchEvent(
      new PointerEvent(type, {
        bubbles: true,
        cancelable: true,
        clientX: x,
        clientY: y,
        pointerId: 1,
      }),
    );
  }

  function key(target: Element, name: string, shift = false): void {
    target.dispatchEvent(
      new KeyboardEvent('keydown', { key: name, shiftKey: shift, bubbles: true, cancelable: true }),
    );
  }

  it('seeks to the lead-in before a marker when the marker is clicked', async () => {
    // The whole point of the feature: the moment that caused the marker is not the first
    // frame, and the click is not a scrub to the pixel under the cursor.
    const { seeks, query } = await mountTimeline({ markers: [marker(30_000)] });
    const button = query<HTMLButtonElement>('button.marker');
    pointer(button, 'pointerdown', TRACK_WIDTH / 2);
    pointer(button, 'pointerup', TRACK_WIDTH / 2);
    expect(seeks).toEqual([30_000 - MARKER_LEAD_MS]);
  });

  it('seeks to the lead-in when a marker is activated from the keyboard', async () => {
    // Enter and Space on a focused button arrive as a click with no pointer gesture behind
    // it. The marker is announced as a control, so it has to do what a click on it does.
    const { seeks, query } = await mountTimeline({ markers: [marker(30_000)] });
    query<HTMLButtonElement>('button.marker').click();
    expect(seeks).toEqual([30_000 - MARKER_LEAD_MS]);
  });

  it('does not seek twice for a mouse click, which the track already handled', async () => {
    const { seeks, query } = await mountTimeline({ markers: [marker(30_000)] });
    const button = query<HTMLButtonElement>('button.marker');
    pointer(button, 'pointerdown', TRACK_WIDTH / 2);
    pointer(button, 'pointerup', TRACK_WIDTH / 2);
    // The browser's click that follows a mouse press has a click count of 1.
    button.dispatchEvent(new MouseEvent('click', { bubbles: true, detail: 1 }));
    expect(seeks).toEqual([30_000 - MARKER_LEAD_MS]);
  });

  it('scrubs to the pointer when the track itself is pressed', async () => {
    const { seeks, query } = await mountTimeline();
    const track = query<HTMLElement>('.track');
    pointer(track, 'pointerdown', TRACK_WIDTH / 4);
    pointer(track, 'pointerup', TRACK_WIDTH / 4);
    expect(seeks).toEqual([15_000]);
  });

  it('scrubs the track when a drag starts on a marker', async () => {
    // A marker is a press target inside the track, so a drag that begins on one scrubs
    // like any other; the marker's lead-in is the target of a click, not of a drag.
    const { seeks, query } = await mountTimeline({ markers: [marker(30_000)] });
    const button = query<HTMLButtonElement>('button.marker');
    pointer(button, 'pointerdown', TRACK_WIDTH / 2);
    pointer(button, 'pointermove', (TRACK_WIDTH * 2) / 3);
    pointer(button, 'pointerup', (TRACK_WIDTH * 2) / 3);
    expect(seeks).toEqual([40_000]);
  });

  it('puts a handle at either end of the clip with Home and End', async () => {
    const { ranges, query } = await mountTimeline({ range: { startMs: 1_000, endMs: 30_000 } });
    key(query<HTMLButtonElement>('button.handle.start'), 'Home');
    expect(ranges).toEqual([{ startMs: 0, endMs: 30_000 }]);
    key(query<HTMLButtonElement>('button.handle.end'), 'End');
    expect(ranges[1]).toEqual({ startMs: 1_000, endMs: 60_000 });
  });

  it('moves a handle a page at a time with PageUp and PageDown', async () => {
    const { ranges, query } = await mountTimeline({ range: { startMs: 1_000, endMs: 30_000 } });
    const end = query<HTMLButtonElement>('button.handle.end');
    key(end, 'PageUp');
    expect(ranges).toEqual([{ startMs: 1_000, endMs: 20_000 }]);
    key(end, 'PageDown');
    expect(ranges[1]).toEqual({ startMs: 1_000, endMs: 40_000 });
  });

  it('still moves a handle with the arrow keys, and further with Shift', async () => {
    const { ranges, query } = await mountTimeline({ range: { startMs: 1_000, endMs: 30_000 } });
    const start = query<HTMLButtonElement>('button.handle.start');
    key(start, 'ArrowRight');
    expect(ranges).toEqual([{ startMs: 1_100, endMs: 30_000 }]);
    key(start, 'ArrowRight', true);
    expect(ranges[1]).toEqual({ startMs: 2_000, endMs: 30_000 });
  });

  it('ignores a key the handle does not answer', async () => {
    const { ranges, query } = await mountTimeline();
    key(query<HTMLButtonElement>('button.handle.start'), 'a');
    expect(ranges).toEqual([]);
  });
});
