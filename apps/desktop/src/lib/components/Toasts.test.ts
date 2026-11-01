// @vitest-environment jsdom
/**
 * The banners' timers: a toast is dismissed on a clock, but never while it is being read.
 *
 * The export notice is the only place that names the exported file, and an error is given 12
 * seconds to be read; both used to expire under the pointer and under keyboard focus alike.
 * Fake timers drive the clock, so what is asserted is whether the dismiss callback ran.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import Toasts, { ERROR_MS, NOTICE_MS } from './Toasts.svelte';

describe('the toasts', () => {
  let app: ReturnType<typeof mount> | undefined;

  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
    vi.useRealTimers();
  });

  async function mountToasts(props: { error?: string | null; notice?: string | null }) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    const dismissed = { error: vi.fn(), notice: vi.fn() };
    app = mount(Toasts, {
      target,
      props: {
        error: props.error ?? null,
        notice: props.notice ?? null,
        onDismissError: dismissed.error,
        onDismissNotice: dismissed.notice,
      },
    });
    await tick();
    return { target, dismissed };
  }

  it('dismisses a notice after its time when nobody is looking at it', async () => {
    const { dismissed } = await mountToasts({ notice: 'Exported clip.mp4 (12 MB).' });
    await vi.advanceTimersByTimeAsync(NOTICE_MS + 10);
    expect(dismissed.notice).toHaveBeenCalledTimes(1);
  });

  it('does not dismiss a notice while the pointer is over it', async () => {
    const { target, dismissed } = await mountToasts({ notice: 'Exported clip.mp4 (12 MB).' });
    const banner = target.querySelector('.banner.notice')!;

    banner.dispatchEvent(new MouseEvent('mouseenter'));
    await vi.advanceTimersByTimeAsync(NOTICE_MS * 5);
    expect(dismissed.notice).not.toHaveBeenCalled();

    // On leaving it starts again from the full time, so it is not cut short by the hold.
    banner.dispatchEvent(new MouseEvent('mouseleave'));
    await vi.advanceTimersByTimeAsync(NOTICE_MS - 100);
    expect(dismissed.notice).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(200);
    expect(dismissed.notice).toHaveBeenCalledTimes(1);
  });

  it('does not dismiss an error while focus is inside it', async () => {
    const { target, dismissed } = await mountToasts({ error: 'The recorder stopped.' });
    const banner = target.querySelector('.banner.error')!;

    banner.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(ERROR_MS * 3);
    expect(dismissed.error).not.toHaveBeenCalled();

    banner.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: null }));
    await vi.advanceTimersByTimeAsync(ERROR_MS + 10);
    expect(dismissed.error).toHaveBeenCalledTimes(1);
  });

  it('keeps holding while focus moves between the toast’s own controls', async () => {
    const { target, dismissed } = await mountToasts({ error: 'The recorder stopped.' });
    const banner = target.querySelector('.banner.error')!;
    const dismiss = banner.querySelector('button')!;

    banner.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    banner.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: dismiss }));
    await vi.advanceTimersByTimeAsync(ERROR_MS * 3);
    expect(dismissed.error).not.toHaveBeenCalled();
  });

  it('holds only the toast that is being looked at', async () => {
    const { target, dismissed } = await mountToasts({ error: 'Boom.', notice: 'Saved.' });
    target.querySelector('.banner.error')!.dispatchEvent(new MouseEvent('mouseenter'));

    await vi.advanceTimersByTimeAsync(NOTICE_MS + 10);
    expect(dismissed.notice, 'the other one still expires').toHaveBeenCalledTimes(1);
    expect(dismissed.error).not.toHaveBeenCalled();
  });
});
