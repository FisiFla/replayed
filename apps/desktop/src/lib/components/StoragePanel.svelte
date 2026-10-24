<script lang="ts">
  /**
   * The storage panel: what is on disk, what the cap is, and what the policy makes of it
   * (spec §8.1).
   *
   * The cleanup pass itself belongs to the recorder (`replayed-cli`), which runs it at
   * startup and on a timer while it is capturing; this window reports the verdict that
   * `plan_cleanup` reaches. The one write it can perform is the *reconciliation*: index
   * rows whose files are gone are not listed and not counted, so the panel says how many
   * there are and offers to remove them — after a confirmation, because deleting rows that
   * a temporarily-offline directory made look dead would be unrecoverable in the window.
   *
   * It owns the whole Storage view, so nothing is folded away: the figures, the usage meter,
   * the verdict and the reconciliation are all on screen at once.
   */
  import { formatBytes, percentOf } from '../time';
  import type { StorageStats } from '../types';

  interface Props {
    stats: StorageStats;
    /** A command is in flight; the reconciliation controls are held while it is. */
    busy: boolean;
    /** The user confirmed: remove the index rows whose files are gone. */
    onPruneMissing: () => void;
    /** Show the data folder (clips, sessions, index, log, config) in the file manager. */
    onOpenDataDir: () => void;
  }

  let { stats, busy, onPruneMissing, onOpenDataDir }: Props = $props();

  /** The reconciliation is a two-step action, like a clip's delete. */
  let confirming = $state(false);

  const usedPercent = $derived(percentOf(stats.total_bytes, stats.cap_bytes));
  const favouritePercent = $derived(percentOf(stats.favourite_bytes, stats.cap_bytes));
  /** Both libraries' phantoms: one entry point, because one user decision. */
  const missingTotal = $derived(stats.missing_count + stats.missing_sessions);
  const missingLabel = $derived(
    [
      stats.missing_count > 0
        ? `${stats.missing_count} ${stats.missing_count === 1 ? 'clip' : 'clips'}`
        : null,
      stats.missing_sessions > 0
        ? `${stats.missing_sessions} ${stats.missing_sessions === 1 ? 'session' : 'sessions'}`
        : null,
    ]
      .filter((part): part is string => part !== null)
      .join(' and '),
  );
</script>

<section class="panel storage">
  <header class="head">
    <h2>Storage</h2>
    <p class="figures">
      <span class="mono">{formatBytes(stats.total_bytes)} / {formatBytes(stats.cap_bytes)}</span>
      <span class="state" class:over={!stats.cap_met}>
        {stats.cap_met ? 'within the cap' : 'over the cap'}
      </span>
    </p>
  </header>

  <div class="stack">
    <div class="bar" role="img" aria-label="{usedPercent.toFixed(0)}% of the cap is used">
      <span class="favourites" style="width: {favouritePercent}%"></span>
      <span class="used" style="width: {Math.max(0, usedPercent - favouritePercent)}%"></span>
    </div>

    <dl>
      <div>
        <dt>Clips on disk</dt>
        <dd>{stats.clip_count} <span class="muted">({stats.favourite_count} favourite)</span></dd>
      </div>
      {#if missingTotal > 0}
        <!-- Counted nowhere above, so the panel says so here: both lists filter rows whose
             file is gone, and a storage number that did not would contradict them. -->
        <div class="wide">
          <dt>Missing files</dt>
          <dd>
            {missingLabel} whose file is gone
            <span class="muted">
              — not counted or measured above; the rows stay in the index in case the file
              comes back
            </span>
          </dd>
        </div>
      {/if}
      <div>
        <dt>In use</dt>
        <dd>{formatBytes(stats.total_bytes)} <span class="muted">of {formatBytes(stats.cap_bytes)}</span></dd>
      </div>
      <div>
        <dt>Favourite bytes</dt>
        <dd>{formatBytes(stats.favourite_bytes)} <span class="muted">(exempt from every rule)</span></dd>
      </div>
      <div>
        <dt>Age limit</dt>
        <dd>{stats.max_age_days} days</dd>
      </div>
      <div class="wide">
        <dt>Clips directory</dt>
        <dd class="path-block">
          <span class="mono path">{stats.clips_dir}</span>
          <button type="button" disabled={busy} onclick={onOpenDataDir}>Open the data folder</button>
        </dd>
      </div>
    </dl>

    {#if stats.cap_met}
      <p class="verdict ok">
        <strong>The cap can be met.</strong>
        {#if stats.planned_deletions > 0}
          The recorder's next cleanup pass would delete {stats.planned_deletions}
          {stats.planned_deletions === 1 ? 'clip' : 'clips'}, leaving
          {formatBytes(stats.bytes_after)}.
        {:else}
          Nothing is over the cap or past the age limit.
        {/if}
      </p>
    {:else}
      <p class="verdict bad">
        <strong>This cap cannot be met.</strong>
        The favourite clips alone exceed it by {formatBytes(stats.over_cap_by_bytes)}, and no
        cleanup pass may delete a favourite to satisfy a cap it cannot meet — so nothing
        else is deleted in the attempt either. Raise the cap, or un-protect the clips you
        no longer need.
      </p>
    {/if}

    {#each stats.warnings as warning}
      <p class="verdict bad">{warning}</p>
    {/each}

    {#if missingTotal > 0}
      <div class="reconcile">
        <p>
          <strong>{missingTotal}</strong>
          {missingTotal === 1 ? 'index entry points' : 'index entries point'} at {missingTotal === 1 ? 'a file' : 'files'}
          that {missingTotal === 1 ? 'is' : 'are'} gone. Removing {missingTotal === 1 ? 'it' : 'them'} from the library
          leaves the disk untouched — there is nothing there to delete.
        </p>
        {#if confirming}
          <div class="actions">
            <button
              class="danger"
              type="button"
              disabled={busy}
              onclick={() => {
                confirming = false;
                onPruneMissing();
              }}
            >
              Remove {missingTotal} {missingTotal === 1 ? 'entry' : 'entries'}
            </button>
            <button type="button" onclick={() => (confirming = false)}>Cancel</button>
          </div>
          <p class="muted">
            The index entries and their cached thumbnails go. If a directory is offline rather
            than deleted, the removal is refused and nothing changes.
          </p>
        {:else}
          <div class="actions">
            <button
              class="danger"
              type="button"
              disabled={busy}
              onclick={() => (confirming = true)}
            >
              Remove from the library…
            </button>
          </div>
        {/if}
      </div>
    {/if}

    <p class="muted footnote">
      The recorder cleans up while it is capturing; this window deletes only what you ask it
      to.
    </p>
  </div>
</section>

<style>
  .storage {
    flex: none;
    gap: var(--s-4);
    padding: var(--s-5);
    border-top: 0;
  }

  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--s-3);
    flex-wrap: wrap;
  }

  .head h2 {
    font: var(--t-display);
    color: var(--text);
  }

  .figures {
    display: flex;
    align-items: baseline;
    gap: var(--s-3);
    font: var(--t-body);
    color: var(--text);
    font-feature-settings: 'tnum';
  }

  .figures .state {
    color: var(--text-muted);
  }

  .figures .state.over {
    color: var(--warn);
  }

  .stack {
    display: flex;
    flex-direction: column;
    gap: var(--s-4);
  }

  /* The usage meter: favourites first (they are exempt), then the rest of what is used. */
  .bar {
    display: flex;
    height: 10px;
    border-radius: var(--r-full);
    background: var(--surface-2);
    border: 1px solid var(--border);
    overflow: hidden;
  }

  .used {
    background: var(--accent-strong);
  }

  .favourites {
    background: var(--star);
  }

  /* Each figure is a stat card; the long ones (missing files, the directory) span the row. */
  dl {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: var(--s-3);
    margin: 0;
  }

  dl > div {
    display: flex;
    flex-direction: column;
    gap: var(--s-1);
    min-width: 0;
    padding: var(--s-3) var(--s-4);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
  }

  dt {
    color: var(--text-muted);
    font: var(--t-small);
  }

  dd {
    margin: 0;
    font: var(--t-heading);
    font-feature-settings: 'tnum';
  }

  dd .muted {
    font: var(--t-small);
  }

  .path {
    overflow-wrap: anywhere;
    font: 400 13px/1.5 var(--font-mono);
  }

  /* The directory's figure holds a button: a `<dd>` may contain flow content, a `<dl>` may not
     contain a button of its own. */
  .path-block {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--s-2);
    font: var(--t-heading);
    font-feature-settings: 'tnum';
  }

  /* An absolute path is one unbreakable token; full width keeps it on as few lines as possible. */
  dl > .wide {
    grid-column: 1 / -1;
  }

  .verdict {
    font: var(--t-small);
    padding: var(--s-3) var(--s-4);
    border-radius: var(--r-md);
    border: 1px solid var(--border);
    background: var(--surface-2);
  }

  .verdict.ok {
    border-color: var(--accent-strong);
    background: rgba(139, 123, 255, 0.06);
  }

  .verdict.bad {
    border-color: var(--warn);
    background: rgba(251, 191, 36, 0.07);
  }

  /* The reconciliation: a warning-toned block with its own two-step confirmation. */
  .reconcile {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    padding: var(--s-3) var(--s-4);
    border: 1px solid var(--warn);
    border-radius: var(--r-md);
    background: rgba(251, 191, 36, 0.07);
    font: var(--t-small);
  }

  .reconcile p {
    margin: 0;
  }

  .actions {
    display: flex;
    gap: var(--s-2);
  }

  .footnote {
    font: var(--t-small);
  }
</style>
