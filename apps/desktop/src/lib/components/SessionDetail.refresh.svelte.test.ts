// @vitest-environment jsdom
/**
 * What a library refresh does to a session that is open.
 *
 * `App.svelte` rebuilds every `SessionView` on each refresh, so the detail page is handed a
 * new object whenever the clip hotkey, a favourite toggle or an extraction refreshes the
 * library. That must not undo the user's trim handles. This file is `.svelte.test.ts` so that
 * it can hold the props in `$state` and replace the session the way the window does.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import SessionDetail from './SessionDetail.svelte';
import { toSessionView } from '../sessions';
import type { SessionView } from '../sessions';
import type { SessionDto } from '../types';

const dto: SessionDto = {
  id: 1, game: 'Valorant', mode: 'session', started_at_ms: 1_700_000_000_000,
  ended_at_ms: 1_700_000_600_000, final_path: '/s/1.mp4', size_bytes: 9_000_000,
  favourite: false, scratch_dir: '/s/scratch', duration_ms: 600_000,
};

describe('the session detail across library refreshes', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  function mountDetail(props: { session: SessionView }) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(SessionDetail, {
      target,
      props: {
        get session() {
          return props.session;
        },
        events: [],
        matches: [],
        busy: false,
        onExtract: () => {},
        onExtractMatch: () => {},
        onFavourite: () => {},
        onDelete: () => {},
      },
    });
    return target;
  }

  function handle(target: HTMLElement, label: 'Trim start' | 'Trim end'): HTMLElement {
    return target.querySelector<HTMLElement>(`[aria-label="${label}"]`)!;
  }

  it('keeps the trim range when the refresh brings back a session of the same length', async () => {
    const props = $state({ session: toSessionView(dto) });
    const target = mountDetail(props);
    await tick();
    expect(handle(target, 'Trim start').getAttribute('aria-valuenow')).toBe('0');

    // The user moves the start handle in from the left.
    handle(target, 'Trim start').dispatchEvent(
      new KeyboardEvent('keydown', { key: 'PageDown', bubbles: true }),
    );
    await tick();
    const moved = handle(target, 'Trim start').getAttribute('aria-valuenow');
    expect(moved, 'the handle moved').not.toBe('0');

    // A refresh: a different object, the same length — and a changed favourite, as a toggle does.
    props.session = toSessionView({ ...dto, favourite: true });
    await tick();
    await tick();

    expect(handle(target, 'Trim start').getAttribute('aria-valuenow')).toBe(moved);
  });

  it('still resets the range when the session’s length really changes', async () => {
    const props = $state({ session: toSessionView({ ...dto, duration_ms: 600_000 }) });
    const target = mountDetail(props);
    await tick();
    handle(target, 'Trim start').dispatchEvent(
      new KeyboardEvent('keydown', { key: 'PageDown', bubbles: true }),
    );
    await tick();
    expect(handle(target, 'Trim start').getAttribute('aria-valuenow')).not.toBe('0');

    // A recording that finished: the length went from one value to another.
    props.session = toSessionView({ ...dto, duration_ms: 300_000 });
    await tick();
    await tick();

    expect(handle(target, 'Trim start').getAttribute('aria-valuenow')).toBe('0');
    expect(handle(target, 'Trim end').getAttribute('aria-valuenow')).toBe('300000');
  });
});
