/**
 * The window's polls: how they are kept from stacking up, and from running for nobody.
 *
 * Two readouts are polled from `App.svelte` — the recorder's counters every half second and the
 * health page every five — and both go through the Tauri IPC to a Rust command that takes a
 * lock or reads the disk. A bare `setInterval` does not care whether the last call has answered:
 * if a command stalls (a lock held for seconds, a log tail on a slow disk), the timer queues
 * another call behind it every tick, and each of those is one more blocked IPC thread. The
 * stalled command that should have been a hiccup becomes a pile, and the window freezes. So a
 * poll that is still in flight is not started again, whatever the timer says.
 *
 * The window also keeps polling while it is hidden to the tray, which is where it spends most of
 * a gaming day: about two IPC calls a second, and every five a read of the log's tail, for a view
 * nobody can see. A hidden window stops its timers and takes one catch-up reading when it is
 * shown again.
 *
 * Pure of Svelte and of the Tauri runtime, like the other `lib/*.ts` files, so that the timers
 * can be driven with fake ones.
 */

/**
 * Wrap an async job so that a call made while the previous one is still running does nothing.
 *
 * The skipped call resolves at once rather than joining the one in flight: a poll that is late
 * has no use for a second copy of the answer, and the next tick asks again anyway. A job that
 * throws is not left "in flight" — the guard is released either way, so one failure cannot stop
 * the poll for good.
 */
export function singleFlight(job: () => Promise<void>): () => Promise<void> {
  let running = false;
  return async () => {
    if (running) return;
    running = true;
    try {
      await job();
    } finally {
      running = false;
    }
  };
}

/** One polled readout: what to run, and how often. */
export interface PollJob {
  run: () => Promise<void>;
  everyMs: number;
}

/** The part of `document` the poller needs, so a test can hand it something smaller. */
export interface VisibilitySource {
  readonly hidden: boolean;
  addEventListener(type: 'visibilitychange', listener: () => void): void;
  removeEventListener(type: 'visibilitychange', listener: () => void): void;
}

/** The part of `window` the poller needs for its focus fallback. */
export interface FocusSource {
  addEventListener(type: 'focus', listener: () => void): void;
  removeEventListener(type: 'focus', listener: () => void): void;
}

/**
 * Run the jobs now and then on their own intervals, each one at most once at a time, and only
 * while the window is visible. Returns the function that stops it all.
 *
 * Each job runs once straight away (unless the window starts hidden), through the same guard as
 * its timer, so the first reading and the first tick cannot overlap either. Hiding the window
 * clears every timer; showing it takes one reading of each job and starts the timers again.
 *
 * **`visibilitychange` is not trusted to come back.** It fires only when the webview itself is
 * told it is hidden or shown. Hiding to the tray hides the top-level window (`Window::hide`,
 * `ShowWindow(SW_HIDE)`), and wry sets WebView2's `IsVisible` only through the webview's own
 * `set_visible`, so on Windows the document may be marked hidden by one path and never marked
 * visible by the other; a poll that waited for `visibilitychange` alone would then stay stopped
 * under a window the user is looking at. Focus is the second signal: a window that receives
 * focus is visible whatever `document.hidden` still says, so it starts the timers (a no-op
 * when they already run). Blur is deliberately *not* a stop signal: a visible, unfocused
 * window on a second monitor is the common case while playing, and it should keep updating.
 */
export function startPolling(
  jobs: PollJob[],
  visibility: VisibilitySource = document,
  focus: FocusSource | undefined = typeof window === 'undefined' ? undefined : window,
): () => void {
  const guarded = jobs.map((job) => ({ ...job, run: singleFlight(job.run) }));
  let timers: ReturnType<typeof setInterval>[] = [];

  const start = () => {
    if (timers.length > 0) return;
    for (const job of guarded) void job.run();
    timers = guarded.map((job) => setInterval(() => void job.run(), job.everyMs));
  };
  const stop = () => {
    for (const timer of timers) clearInterval(timer);
    timers = [];
  };
  const onVisibilityChange = () => (visibility.hidden ? stop() : start());

  visibility.addEventListener('visibilitychange', onVisibilityChange);
  focus?.addEventListener('focus', start);
  if (!visibility.hidden) start();

  return () => {
    visibility.removeEventListener('visibilitychange', onVisibilityChange);
    focus?.removeEventListener('focus', start);
    stop();
  };
}
