# Using replayed

replayed is a local replay buffer and clipping tool for Windows games. It keeps the last 30
seconds of your gameplay in memory, so pressing a hotkey writes a clip of what just happened —
including the seconds *before* you pressed it — and it can also cut a clip by itself when your
game reports an event.

Nothing is uploaded. There is no account, no telemetry and no cloud. The app is a tray
application: it sits in the notification area, records whether or not its window is open, and
takes a clip when you press one key.

This guide is for people using the application. For how it is built, read the
[top-level README](../README.md); for what has actually been verified on real hardware, read
[`docs/verification-status.md`](verification-status.md) — the two are not the same thing, and
this guide does not claim more than that file records.

> **What has been seen running, and what has not.**
>
> The capture path — Windows Graphics Capture, the audio loopback, the hardware encoder, the
> ring buffer and the clip splice — has run on real Windows hardware and produced correct
> clips. The desktop window has been opened on a Windows test machine once, on 2026-09-26.
>
> **The tray icon, the `Ctrl+F8` keypress and hide-on-close have never been observed on any
> machine**, and **the game integrations have never been run against a real game** — the
> payloads are fixtures and the servers are mocks. [`docs/verification-status.md`](verification-status.md)
> records this item by item; read it before you trust anything above it.

---

## Requirements

| Requirement | What you need |
|---|---|
| **Operating system** | Windows 10 1903+ or Windows 11, 64-bit (x64). The capture path is Windows-only: there is no macOS or Linux screen-capture backend. |
| **Graphics** | A GPU with a hardware encoder: NVIDIA (NVENC), Intel (QuickSync) or AMD (AMF). |
| **Disk** | Room for your clips. The default caps are 50 GiB for clips and 200 GiB for sessions; the recorder enforces them by deleting the oldest non-favourited files. |
| **WebView2** | Bundled in the installer — the install touches no network. |

**On the hardware encoder.** `encode.vendor = "auto"` probes the GPU and tries NVENC, then
QuickSync, then AMF, in that order. Each candidate must actually encode a frame before it is
accepted, because ffmpeg lists encoders it was *built* with, including ones whose vendor
runtime is not installed — that is a real failure mode, and it is why the probe encodes rather
than asks.

**There is no software fallback, and there is no way for you to turn one on.** If no hardware
encoder works, the recording refuses to start and the message names every encoder it tried and
why each one failed. That is deliberate: a CPU encode would quietly cost you frames in-game.
A software (libx264) path does exist in the tree, but it is compiled only into test and
development builds, behind a Cargo feature a release build does not carry, and no
`config.toml` key reaches it.

---

## Installing

### Getting the installer

The installer is a single `.exe`: the NSIS package `replayed_<version>_x64-setup.exe`. It is
published on the project's [GitHub Releases](https://github.com/FisiFla/replayed/releases) page.

**No release has been tagged yet**, so that page may be empty — that is the project's state,
not a mistake on your side. The [`CHANGELOG.md`](../CHANGELOG.md) says so in as many words. If
you want the installer now, you can build it from a source checkout; [`docs/packaging.md`](packaging.md)
§3 has the exact commands.

### Windows SmartScreen will warn you — read this before you run it

**The build is not code-signed.** There is no Authenticode certificate behind it, so Windows
SmartScreen shows the installer as coming from an **unknown publisher**, and hides it behind
*More info → Run anyway*.

This is expected, and it is the first thing a new user hits. It means "this file has no
signature on it", not "this file is malicious" — but it is also not proof of anything, and
that is the trade the project has made so far: signing is listed as an open gap in
[`docs/packaging.md`](packaging.md) §5.1.

To get through it: **More info → Run anyway**. On a managed or school machine SmartScreen may
be locked down and the installer will simply refuse to run; that is your administrator's
policy, not the installer's.

### What the installer does

- Installs per-user into `%LOCALAPPDATA%\replayed`. **No administrator prompt, no UAC.**
- Creates a Start Menu shortcut and an entry in *Add/Remove Programs*.
- Carries its own `ffmpeg` and `ffprobe`, so you do not need to install ffmpeg yourself, and
  embeds the WebView2 runtime, so the install needs no network.
- Refuses to install while replayed is running.

### The settings file

`%LOCALAPPDATA%\replayed\config.toml`. The file is optional — with no file, the values in
`config.example.toml` are in force and the Settings panel says so. The panel writes that file
when you change something, and the tray's **Open config file** item reveals it in Explorer.

---

## What it records, and what it does not

**Your screen.** The captured display, through Windows Graphics Capture — the same API an
OBS-style recorder uses. By default that is the primary monitor; *Settings → Capture* can pick
another monitor by index, or record one window instead by naming its title. One display or one
window at a time, never both.

**The game's own audio.** Not the whole system output. By default the game track is a
per-application capture — Windows WASAPI *process* loopback on the watched game's own process
id — so Discord, music and notifications stay out of the clip. Two things widen it:

- while no watched game is running, the whole default output is captured instead;
- `[capture] game_audio = "system"` always takes everything the speakers play.

The setting is `game_audio`, and `"app"` is the default.

**Your microphone, only if you turn it on.** It is off by default, and when it is on it is a
**separate track** in the file rather than a mix.

**Game events.** Only while a recording is running, and only for the games listed below.

### What it does not do

- It does not record other applications' audio (by default — see above).
- It does not send anything anywhere. The only network traffic is to your own machine: a
  read-only poll of `127.0.0.1:2999` for League, and a listener bound to `127.0.0.1` for Valve's
  Game State Integration. The listener rejects any request that does not carry its generated
  token.
- It does not send keystrokes or mouse input, and it never writes to a game client's API. The
  one file it writes into a game's own folder is the Game State Integration config, and only
  when you press the button that says so.
- It does not read other processes' memory, inject anything, or hook anything.

### One thing worth knowing before you share a log

The game-event payload is stored **verbatim**: in-game player names, kill/death/assist totals
and, for League, the match scoreboard. That is a record of the match, not a clip of it. It
lives in the database and in the log file — see [Reporting a problem](#how-do-i-report-a-problem).

---

## Where the files are

Everything is under one per-user folder:

```
%LOCALAPPDATA%\replayed\
├── clips\                          the clip files
├── sessions\                       full-session recordings
├── scratch\                        the working directory of a session recording
├── thumbnails\                     the thumbnail cache (safe to delete)
├── logs\replayed.log                the app's own log
├── replayed.db                      the SQLite index
├── config.toml                      your settings
├── gsi-token-cs2.txt                the CS2 integration token
├── gsi-token-dota2.txt              the Dota 2 integration token
└── gsi\cs2\gamestate_integration_replayed.cfg
    gsi\dota2\gamestate_integration_replayed.cfg
```

The clips directory can be moved: *Settings → Library → Clips directory*, or
`[storage] clips_dir` in `config.toml`. The rest of the folder cannot be moved.

**Opening it from the application.** There is no button that opens the data directory, and the
guide will not pretend otherwise. What there is:

- the **Storage** page shows the clips directory path, as text;
- the **Health** page shows the log file's path, as text;
- a clip's page has a **Show in folder** button that reveals that clip in Explorer;
- the tray's **Open config file** reveals `config.toml` in Explorer — or opens the folder it
  belongs in, when there is no file yet.

So: copy the path out of the Storage or Health page and paste it into Explorer.

---

## Your first minute

1. **Start the app.** It opens a window and puts an icon in the notification area. The window is
   a review pane; you open it when you want it.
2. **Press "Start recording"** — in the window's recorder bar at the top, or **Start recording**
   in the tray menu.
3. **Play.** The replay buffer keeps the last 30 seconds of footage in memory, so a clip is 30
   seconds before the trigger and 5 seconds after it — 35 seconds in total, by default.
   Buffering writes nothing to the disk.
4. **Press `Ctrl+F8`.** The clip is written to `clips\`, indexed, and appears in the Library.
5. **Close the window whenever you like.** Closing it hides it; it does not quit and it does not
   end the recording. Quit from the tray menu.

### The hotkey

`Ctrl+F8` is the default, set by `[hotkeys] clip` in `config.toml`. You cannot rebind it from
the Settings panel — the file is the only place it is set. The recorder bar always shows the
chord in force and whether it is really installed.

Two more chords exist and are **off unless you set them** in the same file:

```toml
[hotkeys]
clip = "Ctrl+F8"
toggle_recording = "Ctrl+F9"   # start / stop the recording
bookmark = "Ctrl+F10"          # a marker on the session timeline, no clip
```

### Starting with Windows

`[app] start_with_system = true` in `config.toml` gives replayed an entry under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, so the tray icon and the hotkey are there
after a reboot. It is **off by default**, and — like the hotkey chord — the settings panel does
not edit it: the file is the only switch, because an autostart entry that disagreed with the
file would be a second, invisible source of truth. The change is applied at startup, so it
needs a restart.

### Where the settings live

Two places, and they are not the same thing:

- the **Settings page** in the window — *Library*, *Capture*, *Automatic recording* and
  *Game integrations*. It edits the clips directory, the frame rate, the resolution, the quality
  preset, the monitor and window to capture, the microphone and the desktop-audio device, the
  "also save the full session" switch, and the automatic-recording switch.
- **`config.toml`** — everything the panel does not edit: the hotkey chords, the start-with-Windows
  switch, the GSI port and the event rules.

**The frame rate is yours to pick, and the panel tells you what the display can show.** The
encoder section names the display being recorded and the rate that display runs at — *"Recording
display 0 — 3840×2160 at 120 Hz"* — and offers that rate in the list next to your saved one. It
**does not choose it for you**: your saved rate stands until you change it. If you pick a rate
above the display's own, the panel says what that costs (*"a rate above 60 fps records frames it
never shows"*), because the frames are real and the display simply never shows them. A rate your
operating system does not really report is shown as *not reported* rather than guessed at.

Recording above 60 fps at 4K asks more of the machine than 4K at 60 does, and a frame that
cannot be encoded in time is dropped rather than held — the panel's live counters say so while
it happens.

**The quality preset is a target, not a cap: expect about 1.8–2× the bitrate it names.** Two
paired runs on the test machine, both asked for 20 Mbit/s, wrote 39.6 Mbit/s with H.264 (1.98×)
and 35.7 Mbit/s with HEVC (1.79×), and the measurement is in the ledger (§18). The encoder's own
rate-control interface was set to the target afterwards and the file did not move (§19), so this
is the hardware encoder aiming near a number rather than holding it. Budget disk from the
overshoot: the preset says "Balanced (20 Mbit/s)" but a 4K/60 recording is closer to 40 Mbit/s,
which is about 18 GB an hour rather than 9.

---

## The games that are supported

Two things are separate and people mix them up:

- **Automatic events** — the app asks the game what happened and cuts a clip when something
  worth keeping happened. Only three games have an integration.
- **Detection** — the app noticing that a game is running, so that *automatic recording* can
  start and stop on it, and so that the audio track can follow that game's own audio. This is
  by executable name, and it works for any title you name.

| Game | Automatic events | How it is noticed | What it needs |
|---|---|---|---|
| **Counter-Strike 2** | **Yes** — Valve's Game State Integration, a local HTTP listener the game POSTs to | the process name `cs2.exe` | the integration file installed into the game's `cfg` folder, then the game restarted |
| **Dota 2** | **Yes** — the same Game State Integration, with its own token and its own path | the process name `dota2.exe` | the same, for Dota 2 |
| **League of Legends** | **Yes** — read-only Live Client Data API, polled once a second while a recording runs | the Live Client API, which only answers during a match | a League match running; nothing to install |
| **Everything else** — including Valorant, Rainbow Six Siege and Overwatch 2 | **No.** The hotkey is the only trigger | the process name, if the title is on the watch list | nothing |

### What each integration actually reports

**Counter-Strike 2 and Dota 2 (Game State Integration).** A kill, a death, an assist, the round
starting and ending, the game starting and ending, and — in Counter-Strike — the bomb being
planted or defused. A kill, a death, an assist, a bomb event or the end of the game takes a
clip through the same trigger the hotkey uses. A round boundary is recorded as a timeline
marker and takes no footage — otherwise a whole match would write a clip per round.

**League of Legends (Live Client Data API).** Your kills, deaths and assists; dragons, barons,
heralds, turrets and inhibitors; first blood, an ace and a multi-kill (recorded as a generic
objective); and the start and end of the game. A kill, a death, an assist, an objective or the
end of the game takes a clip. The start of the game is a marker.

**League is read-only, and that is the point.** The app issues a `GET` to
`https://127.0.0.1:2999/liveclientdata/allgamedata` — the game's own local API — and reads the
answer. It never writes to the League client (the LCU), never POSTs or PUTs, and never opens
the game process. The project's [anti-cheat policy](anti-cheat.md) says the same thing and
names the rules it holds itself to. If you run a kernel-level anti-cheat, read that file
before you run the app.

### Installing the CS2 / Dota 2 integration

**Start a recording first.** The integration file names the port the listener is bound to and
the token the listener accepts, and those only exist while a recording is running — so the
*Install* button refuses, with the reason, when nothing is listening.

Then: *Settings → Game integrations → Install*. The app looks for Counter-Strike 2 and Dota 2
in your Steam libraries and writes one file — `gamestate_integration_replayed.cfg` — into that
game's own `cfg` folder, refusing any destination outside the game's install. Nothing else in
the game folder is touched. **Restart the game afterwards.**

If the port changes between runs, the app rewrites the file it installed with the new one and
says so in the log, so a stale port does not silently stop the events.

If the game is not found in your Steam libraries, the same file is also written into
`%LOCALAPPDATA%\replayed\gsi\<game>\` when a recording starts, so you can copy it in by hand.

Each game has its own token and its own path, so a token lifted from one game's installed file
cannot forge the other game's events.

### Adding a game to the watch list

**Settings → Automatic recording → Watched games.** Type the executable name (`RainbowSix.exe`),
optionally a name to show in the tray, and press *Add game*; *Apply automatic recording* writes
the list to `config.toml`. Removing a game is the × beside it. The list cannot be emptied: with
no entries the six default titles are watched, so the panel refuses to write an empty list
rather than bring them back without saying so.

The same list by hand, for a config file you are editing yourself:

```toml
[[games.watch]]
name = "Dota 2"
exe = "dota2.exe"
signal = "process"            # or "live_client_api"
```

`exe` is compared to the process's image name exactly, case-insensitively — never by
substring, so `notcs2.exe` and `cs2.exe.bak` do not match `cs2.exe`. `signal` is `process` for
every title except League, which Riot publishes a local game-state API for. Two entries with the
same executable are one game, and both the panel and a start refuse the list.

**Adding a title gives you detection and that game's own audio. It does not give the title any
game events.** A game with no integration is the hotkey and nothing else.

The default list is six titles: League of Legends, Counter-Strike 2, Dota 2, Rainbow Six
Siege, Valorant and Overwatch 2 — always the *game* process, never the launcher, so a launcher
left open in the background does not start a recording.

### Automatic recording is off by default

`games.auto_record = false` is the shipped default, and it is the whole safety posture of the
feature: with it off, no watcher thread exists at all and nothing is watched on your behalf.
Turn it on in *Settings → Automatic recording*, or with `games.auto_record = true` in
`config.toml`.

With it on, replayed starts a recording when a watched game starts and stops it when the game
stops. A change has to be seen by two consecutive polls before it is believed, so one
flickering process list cannot start or end a recording.

There are two tiers, and the panel says which one is active:

- **Games with highlights** (League of Legends, CS2, Dota 2) write a clip automatically for each
  kill, round and win. `[recorder] save_session` decides whether the match itself is also kept.
- **Every other watched game** — Fortnite, Apex, Rainbow Six — has no game events, so the match
  is kept by *Also save the full session*: one `session-<timestamp>.mp4` from the game starting
  to it stopping, plus clips on the hotkey. With that switch off you get the clips only, and the
  panel warns you in the section when automatic recording is on and nothing is being kept.

---

## Troubleshooting

### The hotkey does nothing

The app tells you when this happens rather than letting you guess:

- the recorder bar shows **"Clip hotkey is not active"**;
- the tray icon goes to its **attention** state, and the tooltip says the chord is not
  installed and that the tray menu still records and saves clips;
- the engine-counters popover in the recorder bar gives the reason in full;
- the log has it too.

The reason is almost always that the chord is already held by something else. `RegisterHotKey`
is system-wide, so another application — **or another replayed**, because the CLI and the
desktop app cannot both hold one chord — can own `Ctrl+F8`. The message says so by name.

To fix it: close the other application, or set a different chord under `[hotkeys] clip` in
`config.toml` and restart replayed.

You can still clip without the hotkey: the window's **Save clip** button and the tray's
**Save clip now** item do the same thing. Both need a recording running.

### My game is not detected

**Automatic recording is off by default**, so unless you turned it on nothing is watching for
any game at all. That is the first thing to check.

If it is on and the game is still not detected:

- the title must be on the watch list (the six defaults, or your own `[[games.watch]]` entry);
- the match must be the *game* process, not the launcher — `cs2.exe`, not `steam.exe`;
- for League, the Live Client API only answers **during a match**, so a game in the lobby or
  in champion select is not a match;
- a change needs two polls, so a start is noticed a few seconds late — the replay buffer's
  30-second pre-roll covers that gap.

The recorder bar says which of these you are in: *Automatic recording is off*, *Watching for a
game*, or *<game> detected — recording*.

### There are no clips, or the clip is not in the library

The library is a SQLite index (`replayed.db`); the clips are the files in the clips directory.
The window lists the rows whose file is still there.

**If a clip's index write failed, the file is on disk but has no row**, so it does not appear
in the library and the storage policy cannot manage it. The log says exactly that:

> could not index the clip that was just written (…): … The file is kept — it is on disk and it
> is what was asked for — but it will not appear in the clip list, and the storage policy cannot
> manage it.

If a clip is missing from the library, look for that line in the log and then look in the
clips directory. The Storage page counts those files as bytes the index does not name.

Other reasons a clip is not there:

- **The storage policy deleted it.** Clips that are not favourites are deleted when they are
  older than `storage.max_age_days` (7 days by default) or when the clips directory goes over
  `storage.max_total_bytes` (50 GiB by default). Favourites are exempt from both.
- **The clip was never written**, because the recording had not been running.
- **The clip is shorter than you expected.** The pre-roll is clamped to what the buffer
  actually held, so a clip taken in the first seconds of a recording is short. The log says
  so: "only Nms of pre-roll was buffered for a 30000ms request".
- **The file is gone but the row is not.** The library skips rows whose file is missing, and the
  Storage page reports how many.

### The app does not start

A **malformed `config.toml` does not stop the desktop app**. It starts *degraded*: the values
in `config.example.toml` are in force and the window opens. The warning appears on the
**Storage** page (the startup warnings are shown there) and in the log.

**But a recording will not start** while the file is malformed: the capture sections are read
when a recording starts, and a file that cannot be read fails that command, with the reason:

> the config file's [buffer], [encode] and [storage] sections could not be read, so a recording
> cannot be started: …

So the symptom is "the app opens, but Start recording fails and names the key". Fix the file —
or delete it, and the example's values are used again.

The reason the app does not simply refuse to open is that the settings panel is where a broken
file gets repaired, and a panel that refuses to open cannot repair anything.

If the file cannot be read **at all**, the hotkey is not installed either. The app says so —
"the config file could not be read, so no hotkey is installed and the start-with-Windows
setting was not applied" — and the recorder bar shows "Clip hotkey is not active".

The headless CLI is the opposite: it refuses to start on a bad config file, because it has no
window to say why in.

Two things that *do* end the process:

- **The index cannot be opened** (`replayed.db`). There is nothing to show and nothing to
  repair it from, so the app exits. The reason is in the log.
- **A second launch.** The app is single-instance: launching it again hands over to the running
  one rather than fighting it for the hotkey.

### Windows SmartScreen warned me

That is expected — see [Installing](#installing). *More info → Run anyway*.

### How do I report a problem?

**replayed has no telemetry, so nothing reaches the maintainer automatically.** If you want a
bug fixed you have to hand it over yourself. That is the trade the project made, and it is the
reason this section exists.

The **Health** page is the first place to look. It shows the recorder's state, the encoder the
last recording ran, free disk space where the clips are stored, how each game-event source
started, the last warnings and errors from the log, and the log file's path.

> **In the next release** the Health page will also have a **Copy diagnostics** button. It
> assembles a report — the version, the platform, your effective configuration with the
> secrets redacted, the problems the app knows about, and the tail of the log — puts it on
> your clipboard, and sends nothing anywhere. It is not in this build yet. When it is, you
> still decide where the report goes: it fills your clipboard and nothing else.

Until it is, the file to attach is the log:

```
%LOCALAPPDATA%\replayed\logs\replayed.log
```

It rotates at 5 MB and three older files are kept beside it. The log holds file paths and
error text; it never holds the GSI token and never holds footage. It **does** hold the
game-event payloads, which include in-game player names and, for League, the match scoreboard
— so read it before you paste it into a public issue.

Then open an issue on [GitHub](https://github.com/FisiFla/replayed/issues). Nothing leaves your
machine until you paste it somewhere.

### How do I report a security issue?

**Privately.** Do not open a public issue. See [`SECURITY.md`](../SECURITY.md) for how to
report it and what is in scope.

---

## Uninstalling

Windows' *Add/Remove Programs → replayed → Uninstall* removes:

- the installed application files — `replayed.exe`, `uninstall.exe` and the bundled
  `binaries\ffmpeg.exe` / `binaries\ffprobe.exe`;
- the Start Menu shortcut.

**Your clips, the index, your settings and your logs survive.** This was measured, not
assumed: on 2026-09-26 the installer was built, installed, run, uninstalled and reinstalled on
the Windows test machine, and `clips\`, `thumbnails\`, `scratch\`, `config.toml` and the
`replayed.db` were all still there afterwards, with the recording in `clips\` intact
([`docs/packaging.md`](packaging.md) §3.5).

The install directory and the data directory are the same folder
(`%LOCALAPPDATA%\replayed`), and the uninstaller removes only the files it installed.

There is a checkbox in the uninstaller for deleting application data. **It does not delete
replayed's data** — it removes Tauri's own directories, which this app does not use. It errs in
the safe direction, but do not rely on it.

**To remove everything**, delete `%LOCALAPPDATA%\replayed` by hand after uninstalling.

One thing the uninstall has not been tested against: an upgrade from a *different* version.
Only the same build has been reinstalled over itself.

---

## Privacy

One honest paragraph, and then the detail.

There is **no telemetry, no crash reporting, no analytics, no account and no cloud**. The app
never talks to anything but your own machine: a read-only poll of `127.0.0.1:2999` for League,
and a token-gated listener on `127.0.0.1` for Valve's Game State Integration. There is no
auto-updater either — a new version is something you download yourself.

What is written to disk, all under `%LOCALAPPDATA%\replayed`:

| File | What it is |
|---|---|
| `replayed.db` | the SQLite index: clip and session paths, sizes, durations, favourite flags, the marker rows and the event payloads |
| `logs\replayed.log` | the app's own warnings and errors, with paths and error text — and the event payloads, which include player names and the League scoreboard |
| `clips\`, `sessions\`, `scratch\`, `thumbnails\` | the footage itself and the thumbnail cache |
| `gsi-token-cs2.txt`, `gsi-token-dota2.txt` | the generated integration tokens. They are never logged |
| `config.toml` | your settings |

The README's [Privacy](../README.md#privacy) section has the detail, including what the event
payloads carry and why the microphone is opt-in.

To follow the game's own audio, the app reads the process list to find that game's process id —
the same enumeration Task Manager performs. It opens no process, requests no access right and
reads no memory; it copies the image names out of a snapshot and compares them to your watch
list.
