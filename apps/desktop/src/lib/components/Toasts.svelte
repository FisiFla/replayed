<script module lang="ts">
  /** A notice is confirmation and can go quickly; an error is given time to be read. */
  export const NOTICE_MS = 6000;
  export const ERROR_MS = 12000;
</script>

<script lang="ts">
  /**
   * The error and notice banners, as a stack in the bottom-right corner. The classes and
   * roles are the old banners' (`banner error` is a `role="alert"`, `banner notice` a
   * `role="status"`), so anything that found them before still does.
   */
  import Icon from './Icon.svelte';

  interface Props {
    error: string | null;
    notice: string | null;
    onDismissError: () => void;
    onDismissNotice: () => void;
  }

  let { error, notice, onDismissError, onDismissNotice }: Props = $props();

  /**
   * Whether the pointer is over a toast, or focus is inside it.
   *
   * A message that vanishes while it is being read, or while its Dismiss button is being
   * reached for by keyboard, is out of the user's control (WCAG 2.2.1): the export notice is
   * the only place that says where the exported file went. So the timer runs only while the
   * toast is left alone, and starts again from the full time when it is.
   */
  let noticeHeld = $state(false);
  let errorHeld = $state(false);

  // Each message gets its own timer, restarted when the text changes or the hold ends.
  $effect(() => {
    if (notice === null) {
      // The toast went away under the pointer, so no `mouseleave` will come.
      noticeHeld = false;
      return;
    }
    if (noticeHeld) return;
    const timer = setTimeout(onDismissNotice, NOTICE_MS);
    return () => clearTimeout(timer);
  });
  $effect(() => {
    if (error === null) {
      errorHeld = false;
      return;
    }
    if (errorHeld) return;
    const timer = setTimeout(onDismissError, ERROR_MS);
    return () => clearTimeout(timer);
  });

  /** Focus moving between the toast's own controls is still focus inside it. */
  function leftToast(event: FocusEvent): boolean {
    const next = event.relatedTarget;
    return !(next instanceof Node && event.currentTarget instanceof Node && event.currentTarget.contains(next));
  }
</script>

<div class="toasts" data-region="toasts">
  {#if error !== null}
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="banner error"
      role="alert"
      onmouseenter={() => (errorHeld = true)}
      onmouseleave={() => (errorHeld = false)}
      onfocusin={() => (errorHeld = true)}
      onfocusout={(event) => {
        if (leftToast(event)) errorHeld = false;
      }}
    >
      <Icon name="circle-alert" size={16} />
      <span class="text"><strong>Error.</strong> {error}</span>
      <button type="button" onclick={onDismissError}>Dismiss</button>
    </div>
  {/if}

  {#if notice !== null}
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="banner notice"
      role="status"
      onmouseenter={() => (noticeHeld = true)}
      onmouseleave={() => (noticeHeld = false)}
      onfocusin={() => (noticeHeld = true)}
      onfocusout={(event) => {
        if (leftToast(event)) noticeHeld = false;
      }}
    >
      <Icon name="info" size={16} />
      <span class="text">{notice}</span>
      <button type="button" onclick={onDismissNotice}>Dismiss</button>
    </div>
  {/if}
</div>

<style>
  .toasts {
    position: fixed;
    right: var(--s-4);
    bottom: var(--s-4);
    z-index: 50;
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    width: min(420px, calc(100vw - 64px - 2 * var(--s-4)));
    pointer-events: none;
  }

  .banner {
    display: flex;
    align-items: flex-start;
    gap: var(--s-2);
    margin: 0;
    padding: var(--s-2) var(--s-3);
    background: var(--surface-3);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-pop);
    pointer-events: auto;
  }

  .text {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .banner :global(svg) {
    flex: none;
    margin-top: 3px;
  }

  .banner button {
    flex: none;
    padding: 2px 8px;
    font-size: 11px;
  }
</style>
