/**
 * The updater: the one thing in replayed that reaches another machine.
 *
 * Everything else in this application talks to this machine or to nothing at all. This
 * module is the exception, and the exception is deliberate and narrow:
 *
 * - the **user** asks. There is no check at startup, no polling, no background task and
 *   no telemetry. The only way in is the "Check for updates" button in the settings
 *   panel, and the only request that leaves the machine is the one that button makes.
 * - the request goes to **one fixed URL**: the GitHub Releases "latest" asset,
 *   `plugins.updater.endpoints` in `tauri.conf.json`. The frontend cannot change it.
 * - the **request is made by the Rust side**, not by this window. The plugin's own
 *   client fetches the release manifest, and the webview's content security policy
 *   (`connect-src ipc: http://ipc.localhost` in `tauri.conf.json`) says so: the window
 *   cannot open a connection of its own to anything.
 * - what comes back is **verified** before it is installed. The plugin checks the
 *   artifact's minisign signature against the public key pinned in `tauri.conf.json`,
 *   and refuses an artifact that does not match it. A tampered manifest or a swapped
 *   file fails that check and nothing is installed.
 *
 * The module is a thin wrapper rather than a call site, so that the panel does not have
 * to know about the plugin's `Update` resource, its download events, or the fact that
 * a resource has to be released when the user does not want the update after all.
 */

import { check, type DownloadEvent, type Update } from '@tauri-apps/plugin-updater';
import { errorCode, prepareForUpdate } from './ipc';

/** What the update check found, in the words the panel shows. */
export interface UpdateOffer {
  /** The version running now. */
  currentVersion: string;
  /** The version the release announced. */
  version: string;
  /** The release notes, or an empty string when the release carried none. */
  notes: string;
  /** The publish date, as the release announced it (RFC 3339), or `null`. */
  date: string | null;
  /**
   * Download the update and install it.
   *
   * `onProgress` is called with the bytes downloaded so far and the total, when the
   * server said what the total is.
   *
   * On Windows the installer takes over and the application exits, so this promise
   * does not resolve there: the panel says what is happening before it calls. It
   * therefore must not be called until `prepareToInstall` has answered `'ready'`.
   */
  install(onProgress?: (downloaded: number, total: number | null) => void): Promise<void>;
  /** Let the update go: the Rust side releases the resource it was holding. */
  discard(): Promise<void>;
}

/**
 * Ask the one endpoint whether there is a newer release.
 *
 * `null` means there is not — the application is up to date. A rejection means the
 * check itself failed (no network, the endpoint is down, the manifest is not one this
 * build understands); the caller shows the reason.
 */
export async function checkForUpdate(): Promise<UpdateOffer | null> {
  const update = await check();
  if (!update) return null;
  return offerOf(update);
}

/** The panel's view of one update. */
function offerOf(update: Update): UpdateOffer {
  return {
    currentVersion: update.currentVersion,
    version: update.version,
    notes: update.body ?? '',
    date: update.date ?? null,
    install: (onProgress) => installUpdate(update, onProgress),
    discard: () => update.close(),
  };
}

/** Download an update and install it, reporting the progress as it goes. */
async function installUpdate(
  update: Update,
  onProgress?: (downloaded: number, total: number | null) => void,
): Promise<void> {
  let downloaded = 0;
  let total: number | null = null;
  await update.downloadAndInstall((event: DownloadEvent) => {
    if (event.event === 'Started') {
      total = event.data.contentLength ?? null;
    } else if (event.event === 'Progress') {
      downloaded += event.data.chunkLength;
      onProgress?.(downloaded, total);
    }
  });
}

/** What the shell said about installing now. */
export type InstallGate = 'ready' | 'recording_active';

/**
 * Get the recording out of the way before an install, asking the shell rather than the poll.
 *
 * Installing ends the process: on Windows the installer takes over and the application exits
 * under it, and nothing on the way out stops the recorder first. A recording that is running
 * then loses what only exists in memory (the replay buffer, a clip's post-roll that has not
 * been spliced) and its session is left unfinished for the next start to recover with no length
 * and no file. The ffmpeg child lives in the install directory the installer is about to
 * overwrite, too, so it can also fail on a file in use.
 *
 * The shell's `prepare_for_update` is the authority on whether a recording is running, not the
 * window's last poll, which is half a second old at best and absent at worst. Call this with
 * `false` first. `'ready'` means nothing was recording and the install may go on.
 * `'recording_active'` means one is, nothing was touched, and the user has to be asked; if they
 * agree, call again with `true`, which stops and finalises the recording as the Stop button does
 * and answers `'ready'`.
 *
 * Every other outcome throws, and the caller must not install: a stop that failed, a shell that
 * could not be asked, an answer that says a recording was left running. The refusal is only
 * translated when consent had *not* been given; with it given, `recording_active` is a
 * recording that would not stop, and treating it as "ask again" would loop.
 */
export async function prepareToInstall(stopRecording: boolean): Promise<InstallGate> {
  let outcome;
  try {
    outcome = await prepareForUpdate(stopRecording);
  } catch (err) {
    if (!stopRecording && errorCode(err) === 'recording_active') return 'recording_active';
    throw err;
  }
  if (outcome.was_recording && !outcome.stopped) {
    throw new Error('A recording is running and the shell did not stop it, so the update was held back.');
  }
  return 'ready';
}

/**
 * The release date, as a day a person reads.
 *
 * The manifest carries an RFC 3339 timestamp; the panel shows the calendar day of it.
 * A string that is not a timestamp is shown as it is, rather than as `Invalid Date`.
 */
export function formatUpdateDate(date: string | null): string {
  if (date === null) return '';
  const day = /^(\d{4}-\d{2}-\d{2})/.exec(date);
  return day?.[1] ?? date;
}
