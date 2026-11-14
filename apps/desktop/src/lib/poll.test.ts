/**
 * The polls: that a stalled one cannot pile up behind itself, and that a hidden window is not
 * polled at all.
 *
 * Driven with fake timers and a stand-in for `document`, so what is asserted is the number of
 * calls that reached the job, which is the thing a frozen window is made of.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { singleFlight, startPolling } from './poll';
import type { FocusSource, VisibilitySource } from './poll';

/** A `document` that can be hidden and shown on demand. */
function fakeVisibility(hidden = false) {
  let listener: (() => void) | null = null;
  const source = {
    hidden,
    addEventListener: (_type: 'visibilitychange', fn: () => void) => {
      listener = fn;
    },
    removeEventListener: () => {
      listener = null;
    },
  };
  return {
    source: source as VisibilitySource,
    set(next: boolean) {
      source.hidden = next;
      listener?.();
    },
    listening: () => listener !== null,
  };
}

/** A window whose focus can be given on demand, as the webview gives it when it is shown. */
function fakeFocus() {
  let listener: (() => void) | null = null;
  const source: FocusSource = {
    addEventListener: (_type, fn) => {
      listener = fn;
    },
    removeEventListener: () => {
      listener = null;
    },
  };
  return { source, focus: () => listener?.(), listening: () => listener !== null };
}

describe('singleFlight', () => {
  it('does not start a second call while the first has not answered', async () => {
    let release: () => void = () => {};
    const job = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    );
    const guarded = singleFlight(job);

    const first = guarded();
    await guarded();
    await guarded();
    expect(job).toHaveBeenCalledTimes(1);

    release();
    await first;
    void guarded();
    expect(job).toHaveBeenCalledTimes(2);
  });

  it('is released by a job that throws, so one failure does not stop the poll for good', async () => {
    const job = vi.fn(async () => {
      throw new Error('the lock was poisoned');
    });
    const guarded = singleFlight(job);

    await expect(guarded()).rejects.toThrow('poisoned');
    await expect(guarded()).rejects.toThrow('poisoned');
    expect(job).toHaveBeenCalledTimes(2);
  });
});

describe('startPolling', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('reads once straight away and then on the interval', async () => {
    const run = vi.fn(async () => {});
    const stop = startPolling([{ run, everyMs: 500 }], fakeVisibility().source);
    expect(run).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(1500);
    expect(run).toHaveBeenCalledTimes(4);
    stop();
  });

  it('does not stack calls behind a read that never answers', async () => {
    // The health command that blocks on a lock: every tick of the old `setInterval` queued
    // another call behind it.
    const run = vi.fn(() => new Promise<void>(() => {}));
    const stop = startPolling([{ run, everyMs: 5000 }], fakeVisibility().source);

    await vi.advanceTimersByTimeAsync(60_000);
    expect(run).toHaveBeenCalledTimes(1);
    stop();
  });

  it('polls again once the stalled read has answered', async () => {
    let release: () => void = () => {};
    let calls = 0;
    const run = vi.fn(() => {
      calls += 1;
      return calls === 1
        ? new Promise<void>((resolve) => {
            release = resolve;
          })
        : Promise.resolve();
    });
    const stop = startPolling([{ run, everyMs: 1000 }], fakeVisibility().source);

    await vi.advanceTimersByTimeAsync(3000);
    expect(run).toHaveBeenCalledTimes(1);

    release();
    await vi.advanceTimersByTimeAsync(1000);
    expect(run).toHaveBeenCalledTimes(2);
    stop();
  });

  it('keeps each job to its own guard: a stalled one does not hold up the other', async () => {
    const stalled = vi.fn(() => new Promise<void>(() => {}));
    const quick = vi.fn(async () => {});
    const stop = startPolling(
      [
        { run: stalled, everyMs: 500 },
        { run: quick, everyMs: 500 },
      ],
      fakeVisibility().source,
    );

    await vi.advanceTimersByTimeAsync(2000);
    expect(stalled).toHaveBeenCalledTimes(1);
    expect(quick).toHaveBeenCalledTimes(5);
    stop();
  });

  it('stops polling while the window is hidden and reads once when it is shown', async () => {
    const run = vi.fn(async () => {});
    const visibility = fakeVisibility();
    const stop = startPolling([{ run, everyMs: 500 }], visibility.source);
    expect(run).toHaveBeenCalledTimes(1);

    visibility.set(true);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(run, 'nothing was read while the window was hidden').toHaveBeenCalledTimes(1);

    visibility.set(false);
    expect(run, 'one catch-up reading on show').toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(500);
    expect(run).toHaveBeenCalledTimes(3);
    stop();
  });

  it('does not start timers twice when a show arrives without a hide', async () => {
    const run = vi.fn(async () => {});
    const visibility = fakeVisibility();
    const stop = startPolling([{ run, everyMs: 500 }], visibility.source);

    visibility.set(false);
    visibility.set(false);
    await vi.advanceTimersByTimeAsync(500);
    expect(run).toHaveBeenCalledTimes(2);
    stop();
  });

  it('waits for the window to be shown when it starts hidden', async () => {
    const run = vi.fn(async () => {});
    const visibility = fakeVisibility(true);
    const stop = startPolling([{ run, everyMs: 500 }], visibility.source);

    await vi.advanceTimersByTimeAsync(5000);
    expect(run).not.toHaveBeenCalled();

    visibility.set(false);
    expect(run).toHaveBeenCalledTimes(1);
    stop();
  });

  it('stops everything, and listens no more, when it is stopped', async () => {
    const run = vi.fn(async () => {});
    const visibility = fakeVisibility();
    const stop = startPolling([{ run, everyMs: 500 }], visibility.source);

    stop();
    expect(visibility.listening()).toBe(false);
    await vi.advanceTimersByTimeAsync(5000);
    expect(run).toHaveBeenCalledTimes(1);
  });
});

describe('startPolling when the webview does not report visibility reliably', () => {
  // `visibilitychange` only fires if the webview itself is told it is hidden. Tauri's
  // `Window::hide()` hides the top-level window and leaves the WebView2 controller "visible"
  // (wry's `SetIsVisible` is only reached through the webview's own `set_visible`), so on
  // Windows a tray hide may never mark the document hidden, and a show may never mark it
  // visible again. The poll must not depend on that second half: a window the user has just
  // focused is visible whatever `document.hidden` still says.
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('starts again on focus when a hide was seen and the show never was', async () => {
    const run = vi.fn(async () => {});
    const visibility = fakeVisibility();
    const focus = fakeFocus();
    const stop = startPolling([{ run, everyMs: 500 }], visibility.source, focus.source);

    visibility.set(true);
    await vi.advanceTimersByTimeAsync(5000);
    expect(run).toHaveBeenCalledTimes(1);

    // The window comes back, but `visibilitychange` stays silent and `hidden` is still true.
    focus.focus();
    expect(run, 'one catch-up reading on focus').toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(500);
    expect(run).toHaveBeenCalledTimes(3);
    stop();
  });

  it('does not start a second set of timers when focus arrives while already polling', async () => {
    const run = vi.fn(async () => {});
    const focus = fakeFocus();
    const stop = startPolling([{ run, everyMs: 500 }], fakeVisibility().source, focus.source);

    focus.focus();
    focus.focus();
    await vi.advanceTimersByTimeAsync(500);
    expect(run).toHaveBeenCalledTimes(2);
    stop();
  });

  it('does not stop polling on blur: a visible window on a second monitor still updates', async () => {
    // Only a focus listener is offered; there is no blur handler to stop anything with, and
    // the fake window records the only event type it was asked about.
    const run = vi.fn(async () => {});
    const types: string[] = [];
    const focus: FocusSource = {
      addEventListener: (type) => void types.push(type),
      removeEventListener: () => {},
    };
    const stop = startPolling([{ run, everyMs: 500 }], fakeVisibility().source, focus);
    expect(types).toEqual(['focus']);
    stop();
  });

  it('listens no more to focus once stopped', () => {
    const focus = fakeFocus();
    const stop = startPolling([], fakeVisibility().source, focus.source);
    expect(focus.listening()).toBe(true);
    stop();
    expect(focus.listening()).toBe(false);
  });
});
