// @vitest-environment jsdom
/**
 * The session's cut, and the rule that decides whether it can be made.
 *
 * The rule itself is `validateRange`, shared with the clip page; what is tested here is
 * that the session view asks it the same question, so a selection the command would refuse
 * is not offered from either place.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import SessionDetail from './SessionDetail.svelte';
import { toSessionView } from '../sessions';
import type { SessionView } from '../sessions';
import type { SessionDto } from '../types';

const session: SessionDto = {
  id: 1, game: 'Valorant', mode: 'session', started_at_ms: 1_700_000_000_000,
  ended_at_ms: 1_700_000_600_000, final_path: '/s/1.mp4', size_bytes: 9_000_000,
  favourite: false, scratch_dir: '/s/scratch', duration_ms: 600_000,
};

describe('the session detail', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  async function mountDetail(
    view: SessionView,
    overrides: { onDelete?: () => void; busy?: boolean } = {},
  ): Promise<HTMLElement> {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(SessionDetail, {
      target,
      props: {
        session: view,
        events: [],
        matches: [],
        busy: overrides.busy ?? false,
        onExtract: () => {},
        onExtractMatch: () => {},
        onFavourite: () => {},
        onDelete: overrides.onDelete ?? (() => {}),
      },
    });
    // The selection is seeded in an effect, so the component has to be given a tick
    // before the button knows what it is offering.
    await tick();
    return target;
  }

  /** The button that cuts the selected range out of the session. */
  function extractButton(target: HTMLElement): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find((b) =>
        b.textContent?.includes('Extract clip'),
      ) ?? null
    );
  }

  it('offers the cut when the session is long enough to cut', async () => {
    const target = await mountDetail(toSessionView(session));
    expect(extractButton(target)?.disabled).toBe(false);
  });

  it('refuses a selection the command would refuse, as the clip page does', async () => {
    // The clip page asks `validateRange` before it offers the cut; the session page used
    // to ask only whether the selection was empty, and would send a one-millisecond cut.
    const target = await mountDetail(toSessionView({ ...session, duration_ms: 40 }));
    expect(extractButton(target)?.disabled).toBe(true);
    expect(target.textContent).toContain('shortest selection');
  });

  /** The button whose text is exactly `label`. */
  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => (b.textContent ?? '').replace(/\s+/g, ' ').trim() === label,
      ) ?? null
    );
  }

  it('does not delete the whole session on the first click', async () => {
    // One unconfirmed click used to remove the row, the scratch segments and the concatenated
    // file, next to a Favourite button that is harmless to hit by mistake.
    const onDelete = vi.fn();
    const target = await mountDetail(toSessionView(session), { onDelete });

    button(target, 'Delete session')!.click();
    await tick();

    expect(onDelete).not.toHaveBeenCalled();
    expect(button(target, 'Confirm delete'), 'the click armed the button').not.toBeNull();
  });

  it('deletes once, on the confirmation', async () => {
    const onDelete = vi.fn();
    const target = await mountDetail(toSessionView(session), { onDelete });

    button(target, 'Delete session')!.click();
    await tick();
    button(target, 'Confirm delete')!.click();
    await tick();

    expect(onDelete).toHaveBeenCalledTimes(1);
    // It is disarmed again: the next delete asks again.
    expect(button(target, 'Delete session')).not.toBeNull();
    expect(button(target, 'Confirm delete')).toBeNull();
  });

  it('lets the user back out of a delete', async () => {
    const onDelete = vi.fn();
    const target = await mountDetail(toSessionView(session), { onDelete });

    button(target, 'Delete session')!.click();
    await tick();
    button(target, 'Cancel')!.click();
    await tick();

    expect(onDelete).not.toHaveBeenCalled();
    expect(button(target, 'Delete session')).not.toBeNull();
  });

  it('holds the delete button while a command is in flight', async () => {
    const target = await mountDetail(toSessionView(session), { busy: true });
    expect(button(target, 'Delete session')?.disabled).toBe(true);
  });
});
