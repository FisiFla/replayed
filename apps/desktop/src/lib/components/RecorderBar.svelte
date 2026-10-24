<script lang="ts">
  /**
   * The recorder bar, pinned above every view: start, stop, take a clip, and the live readout — plus the two facts
   * about this shell's *background* half that a user cannot otherwise discover: which chord
   * takes a clip (and whether it is really installed), and what closing the window does.
   *
   * This window OWNS nothing of the recording: the engine is `replayed-recorder` — the
   * same crate the CLI drives — and every number below comes from its status. What this
   * component decides is only what a person sees: three states (not recording, REC,
   * stopped with a reason), the counters while it runs, and two buttons.
   *
   * It deliberately has no controls for things that do not exist yet: no hotkey *binding*
   * (the chord is `[hotkeys] clip` in `config.toml`, and the panel names the file), no
   * encoder picker, no per-game event toggle. Those are settings (spec §10) or Phase 4, and
   * a disabled-looking control that cannot work is worse than no control.
   */
  import {
    describeAutoRecord,
    describeConfig,
    describeFrames,
    describeHotkey,
    formatDrift,
    formatRate,
    recordingLabel,
    recordingState,
  } from '../recording';
  import { formatBytes, formatDuration } from '../time';
  import type { AppStatus, RecordingStatus } from '../types';
  import Icon from './Icon.svelte';

  interface Props {
    /** `null` until the first status has arrived. */
    status: RecordingStatus | null;
    /** The background half — the hotkey, and where the config came from. `null` until read. */
    app: AppStatus | null;
    /** A command is in flight; the buttons are held while it is. */
    busy?: boolean;
    onStart: () => void;
    onStop: () => void;
    onClip: () => void;
    /** Take one frame of the capture as a PNG in the library. Works without a recording. */
    onScreenshot?: () => void;
    /** The failed pill is a link to the Health page when the shell provides one. */
    onOpenHealth?: () => void;
  }

  let {
    status,
    app = null,
    busy = false,
    onStart,
    onStop,
    onClip,
    onScreenshot = () => {},
    onOpenHealth,
  }: Props = $props();

  /** The engine counters popover. The content stays in the DOM while closed (hidden), so the
   *  bar's text is the same whether or not it is open. */
  let engineOpen = $state(false);

  const mode = $derived(recordingState(status));
  const frames = $derived(describeFrames(status));
  const hotkey = $derived(describeHotkey(app?.hotkey ?? null));
  const config = $derived(describeConfig(app));
  const autoRecord = $derived(describeAutoRecord(status));
</script>

<section class="recorder" data-region="recorder">
  <div class="bar">
    {#if mode === 'failed' && onOpenHealth}
      <button type="button" class="badge pill failed" onclick={onOpenHealth} title="Open Health">
        {recordingLabel(status)}
      </button>
    {:else}
      <span class="badge pill {mode}" role="status">
        {#if mode === 'recording'}<span class="dot" aria-hidden="true"></span>{/if}
        {recordingLabel(status)}
      </span>
    {/if}

    <div class="middle">
      {#if mode === 'recording' && status !== null}
        <p class="live num">
          {formatDuration(status.span_ms)} of media · {formatRate(status)} ·
          {status.clips}
          {status.clips === 1 ? 'clip' : 'clips'} saved
        </p>
      {:else if autoRecord !== null}
        <p class="muted auto">{autoRecord}</p>
      {/if}
      {#if hotkey !== null && hotkey.problem}
        <p class="hotkey-alert"><Icon name="triangle-alert" size={14} /> Clip hotkey is not active</p>
      {/if}
    </div>

    <div class="controls">
      {#if mode === 'recording'}
        <button class="primary" type="button" onclick={onStop} disabled={busy}>Stop recording</button>
      {:else}
        <button class="primary" type="button" onclick={onStart} disabled={busy}>Start recording</button>
      {/if}
      <button type="button" onclick={onClip} disabled={busy || mode !== 'recording'}>Save clip</button>
      <!-- Deliberately not gated on a recording: a screenshot is one frame of the same capture
           target, and needing to start a recording first would be a restriction with no cause. -->
      <button type="button" onclick={onScreenshot} disabled={busy}>Screenshot</button>
      <button
        type="button"
        class="icon ghost engine-toggle"
        aria-expanded={engineOpen}
        aria-controls="engine-pop"
        aria-label="Engine counters"
        title="Engine counters"
        onclick={() => (engineOpen = !engineOpen)}
      >
        <Icon name="chevron-down" size={16} />
      </button>
    </div>
  </div>

  <div class="engine engine-pop" id="engine-pop" hidden={!engineOpen}>
    {#if hotkey !== null}
      <p class="hotkey" class:bad={hotkey.problem}>{hotkey.text}</p>
    {/if}

    {#if status !== null && status.error !== null}
      <p class="verdict bad">
        <strong>The recorder stopped.</strong>
        {status.error}
      </p>
    {/if}

    {#if status !== null}
      <dl>
        <div>
          <dt>Buffered</dt>
          <dd>{formatDuration(status.span_ms)} <span class="muted">of media</span></dd>
        </div>
        <div>
          <dt>Segments</dt>
          <dd>{status.segments} <span class="muted">({formatBytes(status.bytes)})</span></dd>
        </div>
        <div>
          <dt>Frames</dt>
          <dd>{status.frames}</dd>
        </div>
        <div>
          <dt>Rate</dt>
          <dd>{formatRate(status)}</dd>
        </div>
        <div>
          <dt>Clips saved</dt>
          <dd>{status.clips}</dd>
        </div>
        <div>
          <dt>Media drift</dt>
          <dd>{formatDrift(status)}</dd>
        </div>
      </dl>

      {#if frames !== null}
        <p class="muted footnote">{frames}</p>
      {/if}
    {:else}
      <p class="muted footnote">
        No status has arrived yet — the readout above says which read failed, if one did.
      </p>
    {/if}

    {#if mode !== 'recording'}
      <p class="muted footnote">
        Nothing is being captured. Recording needs a GPU encoder (NVENC, Quick Sync or
        AMF); the CLI records through this same engine.
      </p>
    {/if}

    {#if app !== null}
      <p class="muted footnote shell-notes">
        {app.close_hint}{#if config !== null}<span class="config">{' '}{config}</span>{/if}
      </p>
    {/if}
  </div>
</section>

<style>
  .recorder {
    position: relative;
    flex: none;
    background: var(--surface-1);
    border-bottom: 1px solid var(--border);
  }

  .bar {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    height: 52px;
    padding: 0 var(--s-4);
  }

  .middle {
    display: flex;
    flex: 1;
    align-items: center;
    gap: var(--s-3);
    min-width: 0;
    font-size: 12px;
  }

  .middle p {
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .hotkey-alert {
    display: inline-flex;
    flex: none;
    align-items: center;
    gap: 4px;
    color: var(--warn);
  }

  .badge {
    display: inline-flex;
    flex: none;
    align-items: center;
    gap: 6px;
    padding: 2px 10px;
    border: 1px solid var(--border-strong);
    border-radius: var(--r-full);
    background: transparent;
    font: var(--t-small);
    color: var(--text-muted);
  }

  .badge.recording {
    color: var(--rec);
    border-color: var(--rec);
  }

  .badge.failed {
    color: var(--danger);
    border-color: var(--danger);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--rec);
    animation: rec-pulse 1.6s ease-out infinite;
  }

  @keyframes rec-pulse {
    0% {
      box-shadow: 0 0 0 0 rgb(255 77 94 / 0.55);
    }
    70% {
      box-shadow: 0 0 0 6px rgb(255 77 94 / 0);
    }
    100% {
      box-shadow: 0 0 0 0 rgb(255 77 94 / 0);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .dot {
      animation: none;
    }
  }

  .controls {
    display: flex;
    flex: none;
    align-items: center;
    gap: var(--s-2);
  }

  /* The engine counters: a popover under the bar, not a section of a scrolling column. */
  .engine-pop {
    position: absolute;
    top: calc(100% + 6px);
    right: var(--s-4);
    z-index: 20;
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    width: min(380px, calc(100vw - 96px));
    max-height: calc(100vh - 80px);
    overflow-y: auto;
    padding: var(--s-3) var(--s-4);
    background: var(--surface-3);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-pop);
    font-size: 12px;
  }

  .engine-pop[hidden] {
    display: none;
  }

  dl {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px 16px;
    margin: 0;
    font-size: 12px;
  }

  dl > div {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  dt {
    color: var(--text-muted);
    font-size: 11px;
  }

  dd {
    margin: 0;
  }

  .verdict {
    font-size: 12px;
    padding: 8px 10px;
    border-radius: var(--r-sm);
    border: 1px solid var(--border);
    background: var(--surface-2);
  }

  .verdict.bad {
    border-color: var(--warn);
    background: rgb(251 191 36 / 0.07);
  }

  /*
   * The hotkey line: the sentence that tells a user which key to press, or — when nothing is
   * listening — the failure, which has to look like one.
   */
  .hotkey {
    margin: 0;
    padding: 4px 8px;
    border: 1px solid var(--border);
    background: var(--surface-2);
    border-radius: var(--r-sm);
    font-size: 12px;
  }

  .hotkey.bad {
    border-color: var(--warn);
    background: rgb(251 191 36 / 0.07);
  }

  .footnote {
    margin: 0;
    font-size: 11px;
  }

  .shell-notes .config {
    font-family: var(--font-mono);
    overflow-wrap: anywhere;
  }
</style>
