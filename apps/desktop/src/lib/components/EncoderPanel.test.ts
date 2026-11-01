// @vitest-environment jsdom
/**
 * The encoder section: which encoder it names, and which knobs it offers.
 *
 * The panel is modelled on OBS Studio's Output → Encoder tab, and the lesson under test is
 * the one that separates a panel that is honest from a panel that is merely complete: the
 * choices are the *selected encoder's* choices. A rate-control mode the encoder does not
 * have must not be offered as if it did, and a saved value it does not have must be named
 * rather than dropped — otherwise the backend refuses a setting the panel itself offered.
 */

import axe from 'axe-core';
import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import EncoderPanel, {
  DEFAULT_ENCODER_SETTINGS,
  bitrateLabel,
  estimateBytesPerMinute,
  parseOutputSize,
  presetChoices,
  rateControlChoices,
} from './EncoderPanel.svelte';
import type { CaptureDisplays, ClipSource } from '../ipc';
import type { EncoderCapabilities, EncoderSettings, EncoderStatus } from '../ipc';

const SAVED: EncoderSettings = {
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

const CAPABILITIES: EncoderCapabilities = {
  rate_control_modes: ['cbr', 'vbr'],
  presets: ['p1', 'p2'],
  supports_scaling: true,
  max_bitrate_kbps: 100_000,
};

const STATUS: EncoderStatus = {
  requested_vendor: 'auto',
  selected_vendor: 'nvenc',
  selected_codec: 'h264',
  capabilities: CAPABILITIES,
  message: 'NVENC was chosen: it is the fastest encoder this machine has.',
};

/** The machine has one 120 Hz screen, and the capture is pointed at it. */
const DISPLAYS_120: CaptureDisplays = {
  displays: [{ index: 0, width: 3840, height: 2160, refresh_hz: 120 }],
  selected: 0,
};

/** The machine has one 60 Hz screen — a rate the shipped picker does carry. */
const DISPLAYS_60: CaptureDisplays = {
  displays: [{ index: 0, width: 1920, height: 1080, refresh_hz: 60 }],
  selected: 0,
};

/** A 144 Hz screen: a rate the four shipped choices do not carry. */
const DISPLAYS_144: CaptureDisplays = {
  displays: [{ index: 0, width: 2560, height: 1440, refresh_hz: 144 }],
  selected: 0,
};

/** A screen the OS would not name a rate for. */
const DISPLAYS_UNKNOWN_RATE: CaptureDisplays = {
  displays: [{ index: 0, width: 1920, height: 1080, refresh_hz: null }],
  selected: 0,
};

/** A source that answers the three encoder commands and nothing else. */
function fakeSource(overrides: Partial<ClipSource> = {}): ClipSource {
  const known: Record<string, unknown> = {
    encoderSettings: () => Promise.resolve(SAVED),
    encoderStatus: () => Promise.resolve(STATUS),
    updateEncoderSettings: (settings: EncoderSettings) => Promise.resolve(settings),
    ...overrides,
  };
  return new Proxy(known, {
    get: (t, k: string) => t[k] ?? (() => Promise.resolve(undefined)),
  }) as unknown as ClipSource;
}

describe('the encoder section', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  async function settle() {
    for (let i = 0; i < 10; i++) {
      await tick();
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  async function mountPanel(source: ClipSource): Promise<HTMLElement> {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(EncoderPanel, { target, props: { source } });
    await settle();
    return target;
  }

  /** The control a field label names. */
  function field(target: HTMLElement, label: string): HTMLElement | null {
    return (
      [...target.querySelectorAll<HTMLElement>('label.field')].find(
        (l) => l.querySelector('span')?.textContent?.trim() === label,
      ) ?? null
    );
  }

  function select(target: HTMLElement, label: string): HTMLSelectElement | null {
    return field(target, label)?.querySelector('select') ?? null;
  }

  function input(target: HTMLElement, label: string): HTMLInputElement | null {
    return field(target, label)?.querySelector('input') ?? null;
  }

  /**
   * The panel's text, with the markup's own line breaks folded away.
   *
   * The sentences are wrapped in the source, so the raw `textContent` of a paragraph is
   * several lines with the indentation in between; a test that reads them has to fold the
   * whitespace the way a reader's eye does.
   */
  function text(target: HTMLElement): string {
    return (target.textContent ?? '').replace(/\s+/g, ' ');
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => b.textContent?.trim() === label,
      ) ?? null
    );
  }

  it('has no axe violations, with the controls on screen', async () => {
    const target = await mountPanel(fakeSource());
    // Not vacuous: the pickers are really there, so axe had something to check.
    expect(target.querySelectorAll('select').length).toBeGreaterThan(3);
    const result = await axe.run(target, {
      // jsdom does no layout or painting, so contrast cannot be computed.
      rules: { 'color-contrast': { enabled: false } },
    });
    expect(result.violations.map((violation) => violation.id)).toEqual([]);
  });

  it('names the encoder that was chosen, not just the one that was asked for', async () => {
    const target = await mountPanel(fakeSource());
    const said = text(target);
    // "Auto" is what the settings ask for; the answer is the vendor that will actually run.
    expect(said).toContain('Requested: Auto');
    expect(said).toContain('Selected: NVIDIA (NVENC), H.264');
    // The backend's own explanation is shown rather than a sentence of our own.
    expect(said).toContain('NVENC was chosen: it is the fastest encoder this machine has.');
  });

  it('says whose options are on screen when another vendor is picked', async () => {
    const target = await mountPanel(fakeSource());
    // "Auto" is a request, not a vendor: the options belong to the encoder that was
    // chosen, and there is nothing stale about that.
    expect(text(target)).not.toContain('The options below are the ones');

    const vendor = select(target, 'Encoder');
    vendor!.value = 'qsv';
    vendor!.dispatchEvent(new Event('change', { bubbles: true }));
    await tick();

    expect(text(target)).toContain(
      'The options below are the ones NVIDIA (NVENC) has. Save to see what Intel (Quick Sync) has.',
    );
  });

  it('offers only the rate-control modes the selected encoder has', async () => {
    const target = await mountPanel(fakeSource());
    const options = [...(select(target, 'Rate control')?.options ?? [])];
    expect(options.map((option) => option.value)).toEqual(['cbr', 'vbr']);
    // The modes this encoder does not have are not on offer at all.
    expect(options.map((option) => option.value)).not.toContain('crf');
  });

  it('names a preset the selected encoder does not have, instead of dropping it', async () => {
    const source = fakeSource({
      encoderSettings: () =>
        Promise.resolve({ ...SAVED, preset: 'legacy-preset' }),
    });
    const target = await mountPanel(source);
    const options = [...(select(target, 'Preset')?.options ?? [])];
    const saved = options.find((option) => option.value === 'legacy-preset');
    expect(saved, 'the saved preset is still on screen').toBeDefined();
    expect(saved?.disabled, 'and cannot be chosen again').toBe(true);
    expect(saved?.textContent).toContain('not supported by the selected encoder');
    // The encoder's own presets are still there.
    expect(options.map((option) => option.value)).toEqual(['', 'p1', 'p2', 'legacy-preset']);
  });

  it('says the selected path cannot scale instead of offering a control that does nothing', async () => {
    const source = fakeSource({
      encoderStatus: () =>
        Promise.resolve({
          ...STATUS,
          capabilities: { ...CAPABILITIES, supports_scaling: false },
        }),
    });
    const target = await mountPanel(source);
    expect(select(target, 'Resolution')?.disabled, 'the resolution picker is held').toBe(true);
    expect(text(target)).toContain('The selected path cannot scale');
  });

  it('saves the draft and shows the effective values the backend answers with', async () => {
    // A holder rather than a plain variable: the assignment happens inside a callback, and
    // the type checker cannot see through it.
    const sent: { value: EncoderSettings | null } = { value: null };
    const source = fakeSource({
      updateEncoderSettings: (settings: EncoderSettings) => {
        sent.value = settings;
        // The backend answers with what it actually wrote, which may not be what was sent.
        return Promise.resolve({ ...settings, bitrate_kbps: 25000 });
      },
    });
    const target = await mountPanel(source);

    const bitrate = input(target, 'Video bitrate (kbit/s)');
    expect(bitrate?.value).toBe('20000');
    bitrate!.value = '24000';
    bitrate!.dispatchEvent(new Event('input', { bubbles: true }));
    await tick();

    button(target, 'Apply encoder')?.click();
    await settle();

    expect(sent.value, 'the draft was sent').not.toBeNull();
    expect(sent.value?.bitrate_kbps).toBe(24000);
    // The effective value is what the panel shows afterwards.
    expect(input(target, 'Video bitrate (kbit/s)')?.value).toBe('25000');
    expect(text(target)).toContain('They apply on the next recording start');
  });

  /** Type into a field, as the user would. */
  async function type(field: HTMLInputElement, value: string) {
    field.value = value;
    field.dispatchEvent(new Event('input', { bubbles: true }));
    await tick();
  }

  it('writes only what was edited here, over a fresh read of the file', async () => {
    // The Capture group saved 120 fps after this panel read the file at 60. Raising the
    // bitrate here used to send the whole stale draft and put 60 back, then say "saved".
    let reads = 0;
    const sent: { value: EncoderSettings | null } = { value: null };
    const source = fakeSource({
      encoderSettings: () => {
        reads += 1;
        return Promise.resolve(reads === 1 ? SAVED : { ...SAVED, fps: 120, output_size: '1920x1080' });
      },
      updateEncoderSettings: (settings: EncoderSettings) => {
        sent.value = settings;
        return Promise.resolve(settings);
      },
    });
    const target = await mountPanel(source);
    expect(select(target, 'Frame rate (ceiling)')?.value).toBe('60');

    await type(input(target, 'Video bitrate (kbit/s)')!, '24000');
    button(target, 'Apply encoder')?.click();
    await settle();

    expect(sent.value?.bitrate_kbps, 'the edit was sent').toBe(24000);
    expect(sent.value?.fps, 'the frame rate Capture saved was kept').toBe(120);
    expect(sent.value?.output_size, 'so was the size').toBe('1920x1080');
  });

  it('still sends a field the user did edit here, even when another one changed under it', async () => {
    let reads = 0;
    const sent: { value: EncoderSettings | null } = { value: null };
    const source = fakeSource({
      encoderSettings: () => {
        reads += 1;
        return Promise.resolve(reads === 1 ? SAVED : { ...SAVED, fps: 120 });
      },
      updateEncoderSettings: (settings: EncoderSettings) => {
        sent.value = settings;
        return Promise.resolve(settings);
      },
    });
    const target = await mountPanel(source);

    const fps = select(target, 'Frame rate (ceiling)')!;
    fps.value = '30';
    fps.dispatchEvent(new Event('change', { bubbles: true }));
    await tick();
    button(target, 'Apply encoder')?.click();
    await settle();

    expect(sent.value?.fps).toBe(30);
  });

  it('writes nothing when the file cannot be read again first', async () => {
    // Laying a stale draft over a table that could not be read is the overwrite itself.
    let reads = 0;
    const update = vi.fn((settings: EncoderSettings) => Promise.resolve(settings));
    const source = fakeSource({
      encoderSettings: () => {
        reads += 1;
        return reads === 1 ? Promise.resolve(SAVED) : Promise.reject(new Error('the file is locked'));
      },
      updateEncoderSettings: update,
    });
    const target = await mountPanel(source);

    await type(input(target, 'Video bitrate (kbit/s)')!, '24000');
    button(target, 'Apply encoder')?.click();
    await settle();

    expect(update).not.toHaveBeenCalled();
    expect(target.querySelector('[role="alert"]')?.textContent).toContain('the file is locked');
  });

  it('tells its owner when the settings were saved', async () => {
    const onSaved = vi.fn();
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(EncoderPanel, { target, props: { source: fakeSource(), onSaved } });
    await settle();

    await type(input(target, 'Video bitrate (kbit/s)')!, '24000');
    button(target, 'Apply encoder')?.click();
    await settle();

    expect(onSaved).toHaveBeenCalledTimes(1);
  });

  it('refuses a bitrate the backend would not take, before it is sent', async () => {
    let sent = 0;
    const source = fakeSource({
      updateEncoderSettings: (settings: EncoderSettings) => {
        sent += 1;
        return Promise.resolve(settings);
      },
    });
    const target = await mountPanel(source);

    const bitrate = input(target, 'Video bitrate (kbit/s)');
    bitrate!.value = '999999';
    bitrate!.dispatchEvent(new Event('input', { bubbles: true }));
    await tick();

    expect(text(target)).toContain('The bitrate must be between 1 and 100000 kbit/s');
    expect(button(target, 'Apply encoder')?.disabled, 'and cannot be applied').toBe(true);

    button(target, 'Apply encoder')?.click();
    await settle();
    expect(sent, 'nothing was sent').toBe(0);
  });

  it('shows the backend’s refusal instead of swallowing it', async () => {
    const source = fakeSource({
      updateEncoderSettings: () =>
        Promise.reject({ code: 'invalid_input', message: 'this encoder has no CRF mode' }),
    });
    const target = await mountPanel(source);
    button(target, 'Apply encoder')?.click();
    await settle();

    expect(text(target)).toContain('this encoder has no CRF mode');
    expect(target.querySelector('[role="alert"]')?.textContent).toContain('no CRF mode');
  });

  it('puts the shipped defaults back', async () => {
    const sent: { value: EncoderSettings | null } = { value: null };
    const source = fakeSource({
      encoderSettings: () =>
        Promise.resolve({
          ...SAVED,
          vendor: 'qsv',
          codec: 'hevc',
          bitrate_kbps: 40000,
          fps: 120,
          rate_control: 'vbr',
        }),
      updateEncoderSettings: (settings: EncoderSettings) => {
        sent.value = settings;
        return Promise.resolve(settings);
      },
    });
    const target = await mountPanel(source);
    expect(input(target, 'Video bitrate (kbit/s)')?.value).toBe('40000');

    button(target, 'Restore defaults')?.click();
    await settle();

    expect(sent.value).toEqual(DEFAULT_ENCODER_SETTINGS);
    expect(input(target, 'Video bitrate (kbit/s)')?.value).toBe('20000');
    expect(select(target, 'Encoder')?.value).toBe('auto');
    expect(select(target, 'Codec')?.value).toBe('h264');
  });

  it('sends the three-state zero-copy setting as the backend expects it', async () => {
    const sent: { value: EncoderSettings | null } = { value: null };
    const source = fakeSource({
      updateEncoderSettings: (settings: EncoderSettings) => {
        sent.value = settings;
        return Promise.resolve(settings);
      },
    });
    const target = await mountPanel(source);

    // The default is "use it when the hardware has it", which is neither true nor false.
    expect(select(target, 'Zero-copy capture')?.value).toBe('auto');
    button(target, 'Apply encoder')?.click();
    await settle();
    expect(sent.value?.zero_copy).toBeNull();

    const zeroCopy = select(target, 'Zero-copy capture');
    zeroCopy!.value = 'off';
    zeroCopy!.dispatchEvent(new Event('change', { bubbles: true }));
    await tick();
    button(target, 'Apply encoder')?.click();
    await settle();
    expect(sent.value?.zero_copy).toBe(false);
  });

  it('names the display it is recording and the rate that display runs at', async () => {
    const source = fakeSource({ captureDisplays: () => Promise.resolve(DISPLAYS_120) });
    const target = await mountPanel(source);
    // The framerate is chosen against this number, so the number has to be on screen.
    expect(text(target)).toContain('Recording display 0 — 3840×2160 at 120 Hz');
  });

  it('says the rate was not reported instead of inventing one', async () => {
    const source = fakeSource({
      captureDisplays: () => Promise.resolve(DISPLAYS_UNKNOWN_RATE),
    });
    const target = await mountPanel(source);
    expect(text(target)).toContain('refresh rate not reported');
    // No rate is made up: the panel names no hertz at all.
    expect(text(target)).not.toMatch(/\d+ Hz/);
  });

  it('offers the display’s own rate when it is not one of the four', async () => {
    const source = fakeSource({ captureDisplays: () => Promise.resolve(DISPLAYS_144) });
    const target = await mountPanel(source);
    const options = [...(select(target, 'Frame rate (ceiling)')?.options ?? [])];
    // A 144 Hz screen can be recorded at 144 fps, so the picker has to offer it.
    expect(options.map((option) => option.value)).toEqual(['24', '30', '60', '120', '144']);
    // And the rate is offered, not chosen: the saved rate still stands.
    expect(select(target, 'Frame rate (ceiling)')?.value).toBe('60');
  });

  it('leaves the panel as it was when the machine reports no displays', async () => {
    const captureDisplays = vi.fn(() => Promise.resolve({ displays: [], selected: 0 }));
    const source = fakeSource({ captureDisplays });
    const target = await mountPanel(source);
    // The panel asked what the machine has, and the answer was "none" — which is what a
    // machine that is not Windows says.
    expect(captureDisplays, 'the panel read the display list').toHaveBeenCalled();
    expect(text(target)).not.toContain('Recording display');
    expect(text(target)).not.toContain('refresh rate');
    // The picker is exactly what it was: the four shipped rates, and still working.
    const fps = select(target, 'Frame rate (ceiling)');
    expect([...(fps?.options ?? [])].map((option) => option.value)).toEqual([
      '24',
      '30',
      '60',
      '120',
    ]);
    fps!.value = '120';
    fps!.dispatchEvent(new Event('change', { bubbles: true }));
    await tick();
    expect(select(target, 'Frame rate (ceiling)')?.value).toBe('120');
  });

  it('keeps the panel working when the display list cannot be read', async () => {
    const captureDisplays = vi.fn(() => Promise.reject(new Error('capture_displays failed')));
    const source = fakeSource({ captureDisplays });
    const target = await mountPanel(source);
    expect(captureDisplays, 'the panel asked anyway').toHaveBeenCalled();
    // Nothing is said about a display, and nothing is claimed to have gone wrong: the
    // display list is context for the picker, not a setting the user can change.
    expect(text(target)).not.toContain('Recording display');
    expect(target.querySelector('[role="alert"]'), 'and no error banner').toBeNull();
    expect(select(target, 'Frame rate (ceiling)')?.options.length).toBe(4);
  });

  it('says when the chosen rate is above the display’s own rate', async () => {
    const source = fakeSource({ captureDisplays: () => Promise.resolve(DISPLAYS_60) });
    const target = await mountPanel(source);
    // The saved rate matches the display's, so there is nothing to say yet.
    expect(text(target)).not.toContain('records frames it never shows');

    const fps = select(target, 'Frame rate (ceiling)');
    fps!.value = '120';
    fps!.dispatchEvent(new Event('change', { bubbles: true }));
    await tick();

    expect(text(target)).toContain(
      'The display runs at 60 Hz, so a rate above 60 fps records frames it never shows.',
    );
  });
});

describe('the encoder section’s arithmetic', () => {
  it('counts video and audio into what a minute of footage costs', () => {
    // 20 Mbit/s of video and 192 kbit/s of audio is 150 MB a minute, near enough.
    expect(estimateBytesPerMinute(20000, 192)).toBe(151_440_000);
    expect(estimateBytesPerMinute(20000, 0)).toBe(150_000_000);
  });

  it('says a bitrate in the unit a person reads it in', () => {
    expect(bitrateLabel(20000)).toBe('20.0 Mbit/s');
    expect(bitrateLabel(8000)).toBe('8.0 Mbit/s');
  });

  it('accepts a custom size only in the shape the backend wants', () => {
    expect(parseOutputSize('1920x1080')).toBe('1920x1080');
    expect(parseOutputSize(' 2560X1440 ')).toBe('2560x1440');
    expect(parseOutputSize('1920')).toBeNull();
    expect(parseOutputSize('wide')).toBeNull();
    expect(parseOutputSize('0x0')).toBeNull();
  });

  it('offers the encoder’s own modes and nothing else', () => {
    const choices = rateControlChoices(CAPABILITIES, 'cbr');
    expect(choices.map((choice) => choice.value)).toEqual(['cbr', 'vbr']);
    expect(choices.every((choice) => !choice.unsupported)).toBe(true);
  });

  it('names a mode the encoder does not have as unsupported', () => {
    const choices = rateControlChoices(CAPABILITIES, 'crf');
    const crf = choices.find((choice) => choice.value === 'crf');
    expect(crf?.unsupported).toBe(true);
    expect(crf?.label).toContain('not supported by the selected encoder');
  });

  it('always offers the encoder’s own default as the preset', () => {
    const choices = presetChoices(CAPABILITIES, 'p1');
    expect(choices.map((choice) => choice.value)).toEqual(['', 'p1', 'p2']);
    expect(choices[0]?.label).toBe('Encoder default');
  });
});
