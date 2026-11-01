// @vitest-environment jsdom
/**
 * The encoder section when the Capture group saves a key they share.
 *
 * `fps` and `output_size` are edited from both. The panel read them once on mount, so after
 * Capture saves a new rate it must show the file's value rather than its own old one, and must
 * not treat that new value as something the user changed here. The props are held in `$state`
 * (hence `.svelte.test.ts`) so that the settings panel's side of that can be replayed.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import EncoderPanel from './EncoderPanel.svelte';
import type { ClipSource, EncoderSettings, EncoderStatus } from '../ipc';

const SAVED: EncoderSettings = {
  vendor: 'auto', codec: 'h264', bitrate_kbps: 20000, fps: 60, output_size: '', adapt_fps: true,
  zero_copy: null, rate_control: 'cbr', preset: '', audio_bitrate_kbps: 192,
};
const STATUS: EncoderStatus = {
  requested_vendor: 'auto', selected_vendor: 'nvenc', selected_codec: 'h264',
  capabilities: {
    rate_control_modes: ['cbr', 'vbr'], presets: ['p1'], supports_scaling: true,
    max_bitrate_kbps: 100_000,
  },
  message: '',
};

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

describe('the encoder section beside the Capture group', () => {
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

  function mountPanel(
    props: { shared: { fps: number; output_size: string } },
    source: ClipSource,
  ): HTMLElement {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(EncoderPanel, {
      target,
      props: {
        source,
        get shared() {
          return props.shared;
        },
      },
    });
    return target;
  }

  function control(target: HTMLElement, label: string): HTMLSelectElement | HTMLInputElement {
    const field = [...target.querySelectorAll<HTMLElement>('label.field')].find(
      (l) => l.querySelector('span')?.textContent?.trim() === label,
    );
    return field!.querySelector('select, input') as HTMLSelectElement | HTMLInputElement;
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => b.textContent?.trim() === label,
      ) ?? null
    );
  }

  it('shows the frame rate Capture saved instead of the one it read on mount', async () => {
    const props = $state({ shared: { fps: 60, output_size: '' } });
    const target = mountPanel(props, fakeSource());
    await settle();
    expect(control(target, 'Frame rate (ceiling)').value).toBe('60');

    props.shared = { fps: 120, output_size: '' };
    await settle();

    expect(control(target, 'Frame rate (ceiling)').value).toBe('120');
  });

  it('does not take the Capture group’s save as an edit made here', async () => {
    const sent: { value: EncoderSettings | null } = { value: null };
    // The file now says 120, as Capture saved it; the panel's own read is the old 60.
    let reads = 0;
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
    const props = $state({ shared: { fps: 60, output_size: '' } });
    const target = mountPanel(props, source);
    await settle();
    props.shared = { fps: 120, output_size: '' };
    await settle();

    const bitrate = control(target, 'Video bitrate (kbit/s)');
    bitrate.value = '24000';
    bitrate.dispatchEvent(new Event('input', { bubbles: true }));
    await tick();
    button(target, 'Apply encoder')?.click();
    await settle();

    expect(sent.value?.fps).toBe(120);
    expect(sent.value?.bitrate_kbps).toBe(24000);
  });

  it('keeps what the user typed in other fields when Capture saves', async () => {
    const props = $state({ shared: { fps: 60, output_size: '' } });
    const target = mountPanel(props, fakeSource());
    await settle();
    const bitrate = control(target, 'Video bitrate (kbit/s)');
    bitrate.value = '24000';
    bitrate.dispatchEvent(new Event('input', { bubbles: true }));
    await tick();

    props.shared = { fps: 120, output_size: '' };
    await settle();

    expect(control(target, 'Video bitrate (kbit/s)').value).toBe('24000');
  });

  it('keeps a frame rate the user chose here over the one Capture saved', async () => {
    const props = $state({ shared: { fps: 60, output_size: '' } });
    const target = mountPanel(props, fakeSource());
    await settle();
    const fps = control(target, 'Frame rate (ceiling)');
    fps.value = '30';
    fps.dispatchEvent(new Event('change', { bubbles: true }));
    await tick();

    props.shared = { fps: 120, output_size: '' };
    await settle();

    expect(control(target, 'Frame rate (ceiling)').value).toBe('30');
  });
});
