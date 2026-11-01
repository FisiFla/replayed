<script module lang="ts">
  /**
   * The encoder section's pure parts: the shipped defaults, the words for the encoders and
   * their modes, and the arithmetic behind the size estimate.
   *
   * They live here, in the module context of the component that shows them, because the
   * window cannot be opened in the suite: what these say is pinned by tests instead of by
   * eyeballing a window (the same reason `lib/health.ts` and `lib/settings.ts` exist).
   */
  import type {
    CaptureDisplay,
    CaptureDisplays,
    EncoderCapabilities,
    EncoderSettings,
  } from '../ipc';

  /**
   * The settings this application ships with, as `config.example.toml` states them.
   *
   * "Restore defaults" puts these back. They are the *shipped* defaults, not the ones this
   * machine happens to have: a user who asks for the defaults should get the same recording
   * everyone else gets.
   */
  export const DEFAULT_ENCODER_SETTINGS: EncoderSettings = {
    vendor: 'auto',
    codec: 'h264',
    bitrate_kbps: 20000,
    fps: 60,
    output_size: '',
    adapt_fps: true,
    zero_copy: null,
    rate_control: 'cbr',
    preset: '',
    audio_bitrate_kbps: 192,
  };

  /** The vendors a user can ask for, in the order the picker lists them. */
  export const VENDOR_CHOICES = [
    { value: 'auto', label: 'Auto — probe this machine' },
    { value: 'nvenc', label: 'NVIDIA (NVENC)' },
    { value: 'qsv', label: 'Intel (Quick Sync)' },
    { value: 'amf', label: 'AMD (AMF)' },
  ];

  /** The codecs the hardware encoders all have. */
  export const CODEC_CHOICES = [
    { value: 'h264', label: 'H.264' },
    { value: 'hevc', label: 'HEVC (H.265)' },
  ];

  /** The output sizes the picker offers; the native size is the empty one. */
  export const SIZE_CHOICES = [
    { value: '1920x1080', label: '1920 × 1080' },
    { value: '2560x1440', label: '2560 × 1440' },
    { value: '3840x2160', label: '3840 × 2160' },
  ];

  /** The frame-rate ceilings the picker offers. */
  export const FPS_CHOICES = [24, 30, 60, 120];

  /** One entry of a picker, and whether the chosen encoder has it. */
  export interface Choice {
    value: string;
    label: string;
    /** True when the selected encoder does not have this one. */
    unsupported: boolean;
  }

  /** The name of a vendor, as the panel says it. */
  export function vendorLabel(vendor: string): string {
    switch (vendor) {
      case 'auto':
        return 'Auto';
      case 'nvenc':
        return 'NVIDIA (NVENC)';
      case 'qsv':
        return 'Intel (Quick Sync)';
      case 'amf':
        return 'AMD (AMF)';
      default:
        return vendor;
    }
  }

  /** The name of a codec, as the panel says it. */
  export function codecLabel(codec: string): string {
    switch (codec) {
      case 'h264':
        return 'H.264';
      case 'hevc':
        return 'HEVC (H.265)';
      default:
        return codec;
    }
  }

  /** What a rate-control mode does, in the words the picker shows. */
  export function rateControlLabel(mode: string): string {
    switch (mode) {
      case 'cbr':
        return 'CBR — constant bitrate';
      case 'vbr':
        return 'VBR — variable bitrate';
      case 'crf':
        return 'CRF — constant quality';
      case 'quality':
        return 'Quality — the encoder decides';
      default:
        return mode;
    }
  }

  /**
   * The rate-control modes to offer: the selected encoder's own, and nothing else.
   *
   * A mode the encoder does not have is named as unsupported rather than dropped: the saved
   * value has to stay visible, or the panel would be offering a choice the backend refuses
   * without saying why.
   */
  export function rateControlChoices(
    capabilities: EncoderCapabilities | null,
    current: string,
  ): Choice[] {
    const modes = capabilities?.rate_control_modes ?? [];
    const choices: Choice[] = modes.map((mode) => ({
      value: mode,
      label: rateControlLabel(mode),
      unsupported: false,
    }));
    if (current !== '' && !choices.some((choice) => choice.value === current)) {
      choices.push({
        value: current,
        label: `${rateControlLabel(current)} — not supported by the selected encoder`,
        unsupported: true,
      });
    }
    return choices;
  }

  /**
   * The presets to offer: the encoder's own, plus the one that means "let the encoder
   * choose", plus the saved value when the encoder has no such preset.
   */
  export function presetChoices(
    capabilities: EncoderCapabilities | null,
    current: string,
  ): Choice[] {
    const presets = capabilities?.presets ?? [];
    const choices: Choice[] = [{ value: '', label: 'Encoder default', unsupported: false }];
    for (const preset of presets) {
      choices.push({ value: preset, label: preset, unsupported: false });
    }
    if (current !== '' && !choices.some((choice) => choice.value === current)) {
      choices.push({
        value: current,
        label: `${current} — not supported by the selected encoder`,
        unsupported: true,
      });
    }
    return choices;
  }

  /**
   * The display the capture is pointed at, or `null` when that is not known.
   *
   * `selected` is the index `[capture] monitor` carries, and the list is in that same
   * order, so the answer is the display with that index. A null selection means the
   * settings could not be read, and an index the list does not have means the settings
   * name a display this machine does not have: neither of those names a display, and
   * guessing one would be worse than saying nothing.
   */
  export function selectedDisplay(displays: CaptureDisplays | null): CaptureDisplay | null {
    if (displays === null) return null;
    const selected = displays.selected;
    if (selected === null) return null;
    return displays.displays.find((display) => display.index === selected) ?? null;
  }

  /**
   * What the panel says about the display the capture is pointed at, or `null`.
   *
   * Nothing is said when there is nothing to say: off Windows there are no displays to
   * enumerate, and a read that failed is not a fact about the display. What is said names
   * the display by the index `[capture] monitor` uses — the number the settings carry —
   * and gives the refresh rate only when the OS reported one. A rate that was not
   * reported is unknown, not 60 Hz, and a made-up rate would send the user to the wrong
   * setting.
   */
  export function captureDisplayNote(displays: CaptureDisplays | null): string | null {
    if (displays === null || displays.displays.length === 0) return null;
    const display = selectedDisplay(displays);
    if (display === null) {
      // The machine has displays, but which one the capture is pointed at is unknown.
      return 'The display being recorded is not known, so its refresh rate is not shown.';
    }
    const size =
      display.width > 0 && display.height > 0
        ? `${display.width}×${display.height}`
        : 'size not reported';
    if (display.refresh_hz === null) {
      return `Recording display ${display.index} — ${size}, refresh rate not reported`;
    }
    return `Recording display ${display.index} — ${size} at ${display.refresh_hz} Hz`;
  }

  /**
   * The frame rates the picker offers.
   *
   * The four the panel ships with, plus two more when they are worth having: the rate the
   * settings were saved with, so a saved value is never dropped from its own picker, and
   * the capture display's own refresh rate, so a 144 Hz display can be recorded at 144 fps
   * — a picker that does not offer it cannot be told to.
   */
  export function fpsChoices(savedFps: number | null, displayRefreshHz: number | null): number[] {
    const extra: number[] = [];
    if (savedFps !== null && !FPS_CHOICES.includes(savedFps)) extra.push(savedFps);
    if (
      displayRefreshHz !== null &&
      Number.isInteger(displayRefreshHz) &&
      displayRefreshHz > 0 &&
      !FPS_CHOICES.includes(displayRefreshHz) &&
      !extra.includes(displayRefreshHz)
    ) {
      extra.push(displayRefreshHz);
    }
    return [...FPS_CHOICES, ...extra].sort((a, b) => a - b);
  }

  /**
   * A custom output size, as the backend wants it: `WIDTHxHEIGHT`, lower case.
   *
   * `null` when the text is not a size. The backend refuses a size it cannot record with, so
   * the panel refuses it here first and says what the shape is.
   */
  export function parseOutputSize(text: string): string | null {
    const trimmed = text.trim().toLowerCase();
    if (!/^\d{2,5}x\d{2,5}$/.test(trimmed)) return null;
    const parts = trimmed.split('x');
    const width = Number(parts[0]);
    const height = Number(parts[1]);
    if (!Number.isFinite(width) || !Number.isFinite(height)) return null;
    if (width <= 0 || height <= 0) return null;
    return `${width}x${height}`;
  }

  /** A bitrate in the unit a person reads it in. */
  export function bitrateLabel(bitrateKbps: number): string {
    if (!Number.isFinite(bitrateKbps)) return '—';
    return `${(bitrateKbps / 1000).toFixed(1)} Mbit/s`;
  }

  /**
   * What a minute of footage costs at these bitrates.
   *
   * Video and audio together, in bytes: the container carries both, so a size estimate that
   * counted only the video would be quietly wrong.
   */
  export function estimateBytesPerMinute(bitrateKbps: number, audioKbps: number): number {
    const video = Number.isFinite(bitrateKbps) ? Math.max(0, bitrateKbps) : 0;
    const audio = Number.isFinite(audioKbps) ? Math.max(0, audioKbps) : 0;
    return Math.round(((video + audio) * 1000 * 60) / 8);
  }
</script>

<script lang="ts">
  /**
   * The encoder section: the hardware encoder the recorder will use, and the knobs that
   * encoder actually has.
   *
   * Modelled on OBS Studio's Output → Encoder tab, and on the one lesson from it that
   * matters here: the knobs are the *selected encoder's* knobs. A panel that offers every
   * vendor's rate-control modes and presets is a panel that silently ignores half of what
   * the user picked, so the choices here come from `encoder_status().capabilities` — what the
   * backend says the chosen encoder can do — and a mode or preset this encoder does not have
   * is named as unsupported instead of offered and dropped.
   *
   * The panel also says which encoder was *chosen*, not just which one was asked for: "Auto"
   * is a request, not an answer, and a user who cannot see what they got cannot tell
   * whether the recording will work.
   *
   * The encoder is spawned per recording, so a saved setting takes effect on the next
   * recording start. The panel says that rather than pretending the change is live.
   */
  import { onMount, untrack } from 'svelte';
  import { activeSource, errorMessage } from '../ipc';
  import type { ClipSource, EncoderStatus } from '../ipc';
  import { formatBytes } from '../time';

  interface Props {
    /**
     * The command seam: the real Tauri IPC, or whatever a test or the screenshot harness
     * injects. See `activeSource`.
     */
    source?: ClipSource;
    /**
     * Called after the settings were written, so the settings panel can read them again.
     *
     * Several of these keys (`fps`, `output_size`, the bitrate behind the quality preset) are
     * also edited from the Capture group, which keeps its own copy: without this it would go on
     * comparing its drafts against values this panel has since changed.
     */
    onSaved?: () => void;
    /**
     * The two keys the Capture group shares with this panel, as that group last read them.
     *
     * When Capture saves a new frame rate or size, this panel's picture of the file is stale; a
     * change here moves the baseline (and the draft, if the user had not edited it) so the panel
     * shows what the file says and only a real edit here is written back.
     */
    shared?: { fps: number; output_size: string } | null;
  }

  let { source = activeSource(), onSaved = () => {}, shared = null }: Props = $props();

  /** The saved settings, as the backend read them back. */
  let saved = $state<EncoderSettings | null>(null);
  /** What the backend would use, and what it can do. */
  let status = $state<EncoderStatus | null>(null);
  /** The displays this machine has, and the one the capture is pointed at. */
  let displays = $state<CaptureDisplays | null>(null);
  /** A refusal from the backend, in words the user can act on. */
  let error = $state<string | null>(null);
  /** A confirmation that something was saved. */
  let notice = $state<string | null>(null);
  /** A command is in flight; the controls are held while it is. */
  let busy = $state(false);

  // The drafts behind the controls, reseeded whenever a fresh read lands.
  let vendor = $state(DEFAULT_ENCODER_SETTINGS.vendor);
  let codec = $state(DEFAULT_ENCODER_SETTINGS.codec);
  let bitrateText = $state(String(DEFAULT_ENCODER_SETTINGS.bitrate_kbps));
  let fps = $state(DEFAULT_ENCODER_SETTINGS.fps);
  /** `''` is the native size; `'custom'` is the text box next to it. */
  let sizeChoice = $state('');
  let customSize = $state('');
  let adaptFps = $state(DEFAULT_ENCODER_SETTINGS.adapt_fps);
  let zeroCopy = $state<boolean | null>(DEFAULT_ENCODER_SETTINGS.zero_copy);
  let rateControl = $state(DEFAULT_ENCODER_SETTINGS.rate_control);
  let preset = $state(DEFAULT_ENCODER_SETTINGS.preset);
  let audioBitrateText = $state(String(DEFAULT_ENCODER_SETTINGS.audio_bitrate_kbps));

  /** Point the size picker at a saved output size: a listed one, or the custom box. */
  function seedSize(size: string) {
    if (size === '') {
      sizeChoice = '';
      customSize = '';
    } else if (SIZE_CHOICES.some((choice) => choice.value === size)) {
      sizeChoice = size;
      customSize = '';
    } else {
      sizeChoice = 'custom';
      customSize = size;
    }
  }

  /**
   * Reseed every draft from a fresh read.
   *
   * Called where a read lands, not from an effect on `saved`: `saved` is also moved by the
   * shared-key sync below, and that must not throw away what the user has typed in the other
   * fields.
   */
  function seed(from: EncoderSettings) {
    vendor = from.vendor;
    codec = from.codec;
    bitrateText = String(from.bitrate_kbps);
    fps = from.fps;
    seedSize(from.output_size);
    adaptFps = from.adapt_fps;
    zeroCopy = from.zero_copy;
    rateControl = from.rate_control;
    preset = from.preset;
    audioBitrateText = String(from.audio_bitrate_kbps);
  }

  // The Capture group saved a frame rate or size: take it as the new baseline, and show it
  // unless the user has edited that field here, in which case the edit stays and is still sent.
  $effect(() => {
    const incoming = shared;
    if (incoming === null) return;
    untrack(() => {
      if (saved === null) return;
      const next = { ...saved };
      if (incoming.fps !== saved.fps) {
        if (fps === saved.fps) fps = incoming.fps;
        next.fps = incoming.fps;
      }
      if (incoming.output_size !== saved.output_size) {
        if (outputSize === saved.output_size) seedSize(incoming.output_size);
        next.output_size = incoming.output_size;
      }
      saved = next;
    });
  });

  const capabilities = $derived(status?.capabilities ?? null);
  const rateControlOptions = $derived(rateControlChoices(capabilities, rateControl));
  const presetOptions = $derived(presetChoices(capabilities, preset));
  /** The chosen encoder cannot scale when the backend says so. */
  const canScale = $derived(capabilities === null ? true : capabilities.supports_scaling);
  /** The rate control can only be chosen from modes the encoder has. */
  const rateControlKnown = $derived(
    capabilities !== null && capabilities.rate_control_modes.length > 0,
  );
  const presetKnown = $derived(capabilities !== null);

  const bitrate = $derived(parseNumber(bitrateText));
  const audioBitrate = $derived(parseNumber(audioBitrateText));
  /** The highest rate the backend will write, as it reports it. */
  const maxBitrate = $derived(capabilities?.max_bitrate_kbps ?? null);
  const bitrateValid = $derived(
    bitrate !== null && bitrate > 0 && (maxBitrate === null || bitrate <= maxBitrate),
  );
  const audioBitrateValid = $derived(audioBitrate !== null && audioBitrate > 0);
  const outputSize = $derived(sizeChoice === 'custom' ? customSize.trim() : sizeChoice);
  const customSizeValid = $derived(
    sizeChoice !== 'custom' || parseOutputSize(customSize) !== null,
  );
  /** The display the capture is pointed at, or `null` when that is not known. */
  const captureDisplay = $derived(selectedDisplay(displays));
  /** What the panel says about that display, or `null` when it has nothing to say. */
  const displayNote = $derived(captureDisplayNote(displays));
  const fpsOptions = $derived(
    fpsChoices(saved?.fps ?? null, captureDisplay?.refresh_hz ?? null),
  );
  /**
   * The note about a rate the display cannot show, or `null`.
   *
   * A capture rate above the display's refresh rate records frames the display never
   * shows. That is a fact, not a scolding — the user may want them anyway — so the panel
   * says it once, in its own voice, and leaves the choice alone.
   */
  const aboveDisplayNote = $derived(
    captureDisplay !== null &&
      captureDisplay.refresh_hz !== null &&
      fps > captureDisplay.refresh_hz
      ? `The display runs at ${captureDisplay.refresh_hz} Hz, so a rate above ${captureDisplay.refresh_hz} fps records frames it never shows.`
      : null,
  );
  const estimate = $derived(estimateBytesPerMinute(bitrate ?? 0, audioBitrate ?? 0));
  const zeroCopyValue = $derived(zeroCopy === null ? 'auto' : zeroCopy ? 'on' : 'off');
  /** The encoder the backend says it would use, in words. */
  const selectedLabel = $derived(
    status === null || status.selected_vendor === null
      ? 'no encoder'
      : status.selected_codec === null
        ? vendorLabel(status.selected_vendor)
        : `${vendorLabel(status.selected_vendor)}, ${codecLabel(status.selected_codec)}`,
  );
  /**
   * The note about capabilities that belong to another vendor, or `null`.
   *
   * "Auto" is not a vendor whose capabilities can be listed — it is the request to probe —
   * so only a *named* vendor that is not the selected one makes the options below stale.
   */
  const staleVendorNote = $derived(
    status !== null &&
      status.selected_vendor !== null &&
      vendor !== 'auto' &&
      vendor !== status.selected_vendor
      ? `The options below are the ones ${vendorLabel(status.selected_vendor)} has. Save to see what ${vendorLabel(vendor)} has.`
      : null,
  );

  /** Whether the drafts can be sent: the numbers have to be numbers. */
  const valid = $derived(
    bitrateValid && audioBitrateValid && customSizeValid,
  );

  function parseNumber(text: string): number | null {
    const trimmed = text.trim();
    if (!/^\d+$/.test(trimmed)) return null;
    return Number(trimmed);
  }

  function isEncoderSettings(value: unknown): value is EncoderSettings {
    return typeof value === 'object' && value !== null && 'vendor' in value;
  }

  function isEncoderStatus(value: unknown): value is EncoderStatus {
    return typeof value === 'object' && value !== null && 'capabilities' in value;
  }

  function isCaptureDisplays(value: unknown): value is CaptureDisplays {
    if (typeof value !== 'object' || value === null) return false;
    if (!('displays' in value) || !('selected' in value)) return false;
    return Array.isArray(value.displays);
  }

  /** Read the settings and the status, each on its own, so one failure hides neither. */
  async function load(): Promise<void> {
    busy = true;
    error = null;
    const [settings, encoderStatus, capture] = await Promise.allSettled([
      source.encoderSettings(),
      source.encoderStatus(),
      source.captureDisplays(),
    ]);
    if (settings.status === 'fulfilled') {
      saved = isEncoderSettings(settings.value) ? settings.value : null;
      if (saved !== null) seed(saved);
    }
    if (encoderStatus.status === 'fulfilled') {
      status = isEncoderStatus(encoderStatus.value) ? encoderStatus.value : null;
    }
    if (capture.status === 'fulfilled' && isCaptureDisplays(capture.value)) {
      displays = capture.value;
    }
    // The display list is context for the framerate picker, not a setting: a read that
    // failed leaves the picker exactly as it was, rather than raising a banner the user
    // cannot act on. It is never reported as a failure of the settings.
    const failed = [settings, encoderStatus].find((result) => result.status === 'rejected');
    if (failed !== undefined && failed.status === 'rejected') {
      error = errorMessage(failed.reason);
    }
    busy = false;
  }

  /** The settings the drafts describe, as the backend wants them. */
  function draft(): EncoderSettings {
    return {
      vendor,
      codec,
      bitrate_kbps: bitrate ?? 0,
      fps,
      output_size: outputSize,
      adapt_fps: adaptFps,
      zero_copy: zeroCopy,
      rate_control: rateControl,
      preset,
      audio_bitrate_kbps: audioBitrate ?? 0,
    };
  }

  /** Re-read what the backend would use, after a save changed the answer. */
  async function refreshStatus(): Promise<void> {
    try {
      const next = await source.encoderStatus();
      status = isEncoderStatus(next) ? next : null;
    } catch (err) {
      // The save itself may have gone through; say what failed rather than claiming the
      // whole thing failed, and let the next read settle it.
      error = errorMessage(err);
    }
  }

  /**
   * The drafts laid over a fresh read of the file, so that only what was edited here is written.
   *
   * `update_encoder_settings` takes the whole `[encode]` table, and this panel read it once, on
   * mount. The Capture group writes `fps` and `output_size` into the same table, so sending the
   * whole draft would put back the values this panel read minutes ago over whatever Capture saved
   * since — and say "Encoder settings saved" while doing it. So the table is read again just
   * before the write, and only the fields whose draft differs from what this panel last saw are
   * laid over it: a key the user did not touch here keeps whatever the file says now.
   *
   * `everything` is for "Restore defaults", which means every field, touched or not.
   */
  async function wanted(everything: boolean): Promise<EncoderSettings> {
    const mine = draft();
    if (everything || saved === null) return mine;
    const fresh = await source.encoderSettings();
    if (!isEncoderSettings(fresh)) {
      // Writing the drafts over a table that could not be read is exactly the overwrite this
      // is here to prevent, so nothing is written.
      throw new Error('The current encoder settings could not be read, so nothing was saved.');
    }
    const edited: Partial<EncoderSettings> = {};
    for (const key of Object.keys(mine) as (keyof EncoderSettings)[]) {
      if (mine[key] !== saved[key]) Object.assign(edited, { [key]: mine[key] });
    }
    return { ...fresh, ...edited };
  }

  /** Save the drafts, and show the effective values the backend answers with. */
  async function apply(everything = false): Promise<void> {
    if (!valid) return;
    busy = true;
    error = null;
    try {
      const effective = await source.updateEncoderSettings(await wanted(everything));
      if (isEncoderSettings(effective)) {
        saved = effective;
        seed(effective);
      }
      await refreshStatus();
      notice = 'Encoder settings saved. They apply on the next recording start.';
      onSaved();
    } catch (err) {
      // The backend's refusal is the answer here: a bitrate it will not take, a codec the
      // chosen encoder does not have. It is shown, not swallowed.
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  /** Put the shipped defaults back, and save them. */
  async function restoreDefaults(): Promise<void> {
    const defaults = DEFAULT_ENCODER_SETTINGS;
    vendor = defaults.vendor;
    codec = defaults.codec;
    bitrateText = String(defaults.bitrate_kbps);
    fps = defaults.fps;
    sizeChoice = '';
    customSize = '';
    adaptFps = defaults.adapt_fps;
    zeroCopy = defaults.zero_copy;
    rateControl = defaults.rate_control;
    preset = defaults.preset;
    audioBitrateText = String(defaults.audio_bitrate_kbps);
    notice = null;
    await apply(true);
  }

  onMount(() => {
    void load();
  });
</script>

<section class="group" id="settings-encoder">
  <h3>Encoder</h3>

  {#if saved === null}
    <p class="muted footnote">
      The encoder settings could not be read. The recorder will use whatever `config.toml`
      says until they can be.
    </p>
    {#if error !== null}
      <p class="invalid" role="alert">{error}</p>
    {/if}
    <button type="button" onclick={() => void load()} disabled={busy}>Try again</button>
  {:else}
    {#if status !== null}
      <p class="chosen">
        Requested: <strong>{vendorLabel(status.requested_vendor)}</strong>. Selected:
        <strong>{selectedLabel}</strong>.
      </p>
      {#if status.message !== ''}
        <p class="muted footnote">{status.message}</p>
      {/if}
      {#if staleVendorNote !== null}
        <p class="muted footnote">{staleVendorNote}</p>
      {/if}
    {/if}

    <div class="row">
      <label class="field">
        <span>Encoder</span>
        <select
          value={vendor}
          onchange={(event) => (vendor = event.currentTarget.value)}
          disabled={busy}
        >
          {#each VENDOR_CHOICES as option (option.value)}
            <option value={option.value}>{option.label}</option>
          {/each}
        </select>
      </label>
      <label class="field">
        <span>Codec</span>
        <select
          value={codec}
          onchange={(event) => (codec = event.currentTarget.value)}
          disabled={busy}
        >
          {#each CODEC_CHOICES as option (option.value)}
            <option value={option.value}>{option.label}</option>
          {/each}
        </select>
      </label>
    </div>

    <div class="row">
      <label class="field">
        <span>Resolution</span>
        <select
          value={sizeChoice}
          onchange={(event) => (sizeChoice = event.currentTarget.value)}
          disabled={busy || !canScale}
        >
          <option value="">Native (capture size)</option>
          {#each SIZE_CHOICES as option (option.value)}
            <option value={option.value}>{option.label}</option>
          {/each}
          <option value="custom">Custom…</option>
        </select>
      </label>
      <label class="field">
        <span>Frame rate (ceiling)</span>
        <select
          value={String(fps)}
          onchange={(event) => (fps = Number(event.currentTarget.value))}
          disabled={busy}
        >
          {#each fpsOptions as option (option)}
            <option value={String(option)}>{option} fps</option>
          {/each}
        </select>
      </label>
    </div>
    {#if !canScale}
      <p class="invalid">
        The selected path cannot scale: the recording is written at the capture size.
      </p>
    {/if}
    {#if displayNote !== null}
      <p class="muted footnote">{displayNote}</p>
    {/if}
    {#if aboveDisplayNote !== null}
      <p class="muted footnote">{aboveDisplayNote}</p>
    {/if}
    <p class="muted footnote">
      Resolution and frame rate are the same two settings as the ones under Capture, so the
      last one you change is the one the recorder uses.
    </p>
    {#if sizeChoice === 'custom'}
      <label class="field">
        <span>Custom size (width × height)</span>
        <input
          type="text"
          value={customSize}
          oninput={(event) => (customSize = event.currentTarget.value)}
          placeholder="1920x1080"
          disabled={busy}
          spellcheck={false}
        />
      </label>
      {#if !customSizeValid}
        <p class="invalid">A size is written as WIDTHxHEIGHT, e.g. 1920x1080.</p>
      {/if}
    {/if}
    <label class="check">
      <input
        type="checkbox"
        checked={adaptFps}
        onchange={(event) => (adaptFps = event.currentTarget.checked)}
        disabled={busy}
      />
      <span>Adapt to what this machine sustains</span>
    </label>
    <p class="muted footnote">
      The frame rate is a ceiling, not a promise. With the toggle on, the recorder measures
      what it can hold at the capture resolution and records at the lower of that and the
      ceiling.
    </p>

    <div class="row">
      <label class="field">
        <span>Rate control</span>
        <select
          value={rateControl}
          onchange={(event) => (rateControl = event.currentTarget.value)}
          disabled={busy || !rateControlKnown}
        >
          {#each rateControlOptions as option (option.value)}
            <option value={option.value} disabled={option.unsupported}>{option.label}</option>
          {/each}
        </select>
      </label>
      <label class="field">
        <span>Preset</span>
        <select
          value={preset}
          onchange={(event) => (preset = event.currentTarget.value)}
          disabled={busy || !presetKnown}
        >
          {#each presetOptions as option (option.value)}
            <option value={option.value} disabled={option.unsupported}>{option.label}</option>
          {/each}
        </select>
      </label>
    </div>
    {#if !rateControlKnown}
      <p class="invalid">
        The encoder's capabilities are not known, so the rate control cannot be chosen here.
      </p>
    {/if}

    <div class="row">
      <label class="field">
        <span>Video bitrate (kbit/s)</span>
        <input
          type="number"
          min="100"
          max={capabilities?.max_bitrate_kbps ?? undefined}
          value={bitrateText}
          oninput={(event) => (bitrateText = event.currentTarget.value)}
          disabled={busy}
        />
      </label>
      <label class="field">
        <span>Audio bitrate (kbit/s)</span>
        <input
          type="number"
          min="16"
          value={audioBitrateText}
          oninput={(event) => (audioBitrateText = event.currentTarget.value)}
          disabled={busy}
        />
      </label>
    </div>
    <p class="estimate">
      {#if bitrate !== null}
        {bitrateLabel(bitrate)} video · about {formatBytes(estimate)} per minute of footage
      {:else}
        A bitrate is a number of kbit/s.
      {/if}
    </p>
    {#if !bitrateValid}
      <p class="invalid">
        {#if maxBitrate === null}
          The bitrate must be a number of kbit/s above zero.
        {:else}
          The bitrate must be between 1 and {maxBitrate} kbit/s.
        {/if}
      </p>
    {/if}
    {#if !audioBitrateValid}
      <p class="invalid">The audio bitrate must be a number of kbit/s above zero.</p>
    {/if}

    <label class="field">
      <span>Zero-copy capture</span>
      <select
        value={zeroCopyValue}
        onchange={(event) =>
          (zeroCopy =
            event.currentTarget.value === 'auto' ? null : event.currentTarget.value === 'on')}
        disabled={busy}
      >
        <option value="auto">Use it when the hardware has it</option>
        <option value="on">Always use it (refuses to start without it)</option>
        <option value="off">Never use it</option>
      </select>
    </label>
    <p class="muted footnote">
      Zero-copy hands the captured frames to the hardware encoder as GPU textures, so their
      pixels never reach this process. It needs a hardware encoder and Windows, and it cannot
      scale the recording.
    </p>

    {#if error !== null}
      <p class="invalid" role="alert">{error}</p>
    {/if}
    {#if notice !== null}
      <p class="saved" role="status">{notice}</p>
    {/if}

    <div class="actions">
      <button type="button" class="primary" onclick={() => void apply()} disabled={busy || !valid}>
        Apply encoder
      </button>
      <button type="button" onclick={() => void restoreDefaults()} disabled={busy}>
        Restore defaults
      </button>
    </div>
    <p class="muted footnote">
      Saved settings are applied on the next recording start — the encoder is spawned per
      recording, so a recording that is already running keeps the encoder it started with.
    </p>
  {/if}
</section>

<style>
  .group {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    padding: var(--s-4);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-lg);
    scroll-margin-top: var(--s-2);
  }

  h3 {
    margin: 0;
    font: var(--t-heading);
    color: var(--text);
  }

  .chosen {
    font: var(--t-body);
    color: var(--text);
  }

  .row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 240px), 1fr));
    gap: 8px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    font: var(--t-small);
  }

  .field span {
    color: var(--text-muted);
  }

  input[type='text'],
  input[type='number'],
  select {
    font: inherit;
    color: inherit;
    background: var(--surface-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    padding: 6px 10px;
    min-width: 0;
    max-width: 100%;
    text-overflow: ellipsis;
  }

  select {
    width: 100%;
  }

  input:disabled,
  select:disabled {
    opacity: 0.45;
  }

  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    font: var(--t-small);
    cursor: pointer;
  }

  .check input {
    width: 15px;
    height: 15px;
    margin: 0;
    accent-color: var(--accent);
    cursor: pointer;
  }

  .invalid {
    color: var(--warn);
    font: var(--t-small);
  }

  .saved {
    color: var(--ok);
    font: var(--t-small);
  }

  .footnote {
    font: var(--t-small);
  }

  .estimate {
    font: var(--t-small);
    color: var(--text-muted);
  }

  .actions {
    display: flex;
    gap: var(--s-2);
    flex-wrap: wrap;
  }
</style>
