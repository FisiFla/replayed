// @vitest-environment jsdom
/**
 * The storage panel's reconciliation.
 *
 * The panel is otherwise a readout, and this is the one thing it can *do*: remove index rows whose
 * files are gone. That is a destructive-looking action on data the window did not create, so what is
 * under test is the shape of the offer, not the arithmetic:
 *
 * * nothing to reconcile means no offer at all (a button that says "remove 0 entries" is noise);
 * * it takes two clicks — the confirm step names how many entries go before anything is called;
 * * the callback is called once, on the confirm, and never merely on opening the offer.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import StoragePanel from './StoragePanel.svelte';
import type { StorageStats } from '../types';

function stats(overrides: Partial<StorageStats> = {}): StorageStats {
  return {
    clip_count: 1,
    total_bytes: 5_000_000,
    missing_count: 0,
    missing_sessions: 0,
    favourite_count: 0,
    favourite_bytes: 0,
    cap_bytes: 10_000_000_000,
    max_age_days: 30,
    cap_met: true,
    over_cap_by_bytes: 0,
    planned_deletions: 0,
    bytes_after: 5_000_000,
    clips_dir: 'C:\\Users\\player\\Videos\\replayed',
    warnings: [],
    ...overrides,
  };
}

async function settle() {
  for (let i = 0; i < 10; i++) {
    await tick();
    await new Promise((r) => setTimeout(r, 0));
  }
}

describe('the storage panel', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  function render(value: StorageStats, onPruneMissing = () => {}) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(StoragePanel, { target, props: { stats: value, busy: false, onPruneMissing } });
    return target;
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find((b) =>
        (b.textContent ?? '').includes(label),
      ) ?? null
    );
  }

  it('offers nothing when every indexed entry has its file', async () => {
    const target = render(stats());
    await settle();
    expect(target.textContent).not.toContain('Remove from the library');
    expect(button(target, 'Remove')).toBeNull();
  });

  it('names both libraries in one count when there is something to reconcile', async () => {
    const target = render(stats({ missing_count: 2, missing_sessions: 1 }));
    await settle();
    expect(target.textContent).toContain('2 clips and 1 session');
    expect(button(target, 'Remove from the library')).not.toBeNull();
  });

  it('calls the command only after the second click', async () => {
    const onPruneMissing = vi.fn();
    const target = render(stats({ missing_count: 3 }), onPruneMissing);
    await settle();

    button(target, 'Remove from the library')!.click();
    await settle();
    expect(onPruneMissing).not.toHaveBeenCalled();

    // The confirm step says how many entries it removes, which is the number the user saw.
    const confirm = button(target, 'Remove 3 entries');
    expect(confirm).not.toBeNull();
    confirm!.click();
    await settle();
    expect(onPruneMissing).toHaveBeenCalledTimes(1);
  });

  it('can be backed out of, and calls nothing', async () => {
    const onPruneMissing = vi.fn();
    const target = render(stats({ missing_sessions: 1 }), onPruneMissing);
    await settle();

    button(target, 'Remove from the library')!.click();
    await settle();
    button(target, 'Cancel')!.click();
    await settle();

    expect(onPruneMissing).not.toHaveBeenCalled();
    expect(button(target, 'Remove from the library')).not.toBeNull();
  });

  it('holds the controls while a command is in flight', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(StoragePanel, {
      target,
      props: { stats: stats({ missing_count: 1 }), busy: true, onPruneMissing: () => {} },
    });
    await settle();

    const offer = button(target, 'Remove from the library') as HTMLButtonElement;
    expect(offer.disabled).toBe(true);
  });
});
