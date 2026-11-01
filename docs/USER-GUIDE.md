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
| **Disk** | Room for your clips. The default caps are 50 GiB for clips and 200 GiB for sessions, **and clips and sessions older than 7 days are deleted** unless you favourite them; see [Clips are deleted after 7 days](#clips-are-deleted-after-7-days-by-default). |
| **WebView2** | Bundled in the installer — the install touches no network. |

**On the hardware encoder.** `encode.vendor = "auto"` probes the GPU and tries NVENC, then
QuickSync, then AMF, in that order. Each candidate must actually encode a frame before it is
accepted, because ffmpeg lists encoders it was *built* with, including ones whose vendor
runtime is not installed — that is a real failure mode, and it is why the probe encodes rather
than asks.

**On a machine with two GPUs** (an NVIDIA card and an Intel iGPU, say), the zero-copy path uses
an encoder on the GPU the captured display is on, and only there: a frame is never copied from
one GPU to the other. Asking for `vendor = "qsv"` while the game runs on the NVIDIA card therefore
records through ffmpeg's Quick Sync encoder on the ordinary path, or refuses to start with
`encode.zero_copy = true`, saying which GPU the capture is on. Intel's Quick Sync encoders also
cannot be created from a Windows service or an SSH login (session 0); the desktop app never runs
there, but `replayed-cli` started over SSH does.

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

**Your microphone, only if you turn it on.** It is off by default. When it is on, the recording
carries it as a **separate track** beside the game audio, and a saved clip has three audio
tracks: **Game + Mic** (the two mixed), then **Game Audio** and **Microphone** as they were
recorded. The mix comes first because most players play only a file's first audio track. The
app's own player is one of them, and so are most of the places a clip is shared to. The two
recorded tracks are still in the clip for a player or an editor that lets you pick a track
(VLC: *Audio → Audio Track*). The mix is the only part of a clip that is encoded again; the
video and the recorded tracks are copied. A full session keeps just the two recorded tracks, and
its export mixes them. If the microphone goes away while a recording
is running (a headset unplugged), the recording carries on: the gap is filled with silence so the
voice stays in step with the video, and the microphone is picked up again when the device comes
back. A microphone that cannot be opened when the recording *starts* refuses the start instead.

**Game events.** Only while a recording is running, and only for the games listed below.

### What it does not do

- **It does not clip the games that have no integration.** Valorant, Rainbow Six Siege,
  Overwatch 2, Fortnite and Apex Legends have no local event API, and this app will not use a
  third-party game-events provider to invent one. What they get instead is the honest tier:
  the whole session recorded automatically and clips on the hotkey, with a marker for each one.
  The three games that do clip themselves are listed under
  [The games that are supported](#the-games-that-are-supported).
- It does not upload, share or sync anything: no account, no cloud, no public profile. A clip
  leaves the machine when you copy it somewhere.
- It has no effects editor — no transitions, filters, speed ramps or keyframes. A trim is a
  lossless cut, and an export that re-encodes is not one.
- It does not add a webcam overlay.
- It does not fall back to a software encoder in a release build: if the hardware encoder
  cannot keep up, the app says so rather than silently switching to a slower path.
- It does not record other applications' audio (by default — see above).
- It does not send anything anywhere on its own. Its running traffic is to your own machine: a
  read-only poll of `127.0.0.1:2999` for League, and a listener bound to `127.0.0.1` for Valve's
  Game State Integration. The listener rejects any request that does not carry its generated
  token. The one request to another machine is the **Check for updates** button
  ([Updating](#updating)), and it happens only when you press it.
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
├── scratch\                        the shared scratch folder (nothing is written here while buffering)
├── thumbnails\                     the thumbnail cache (safe to delete)
├── logs\replayed.log                the app's own log
├── logs\replayed.lock               the log's lock file (empty; do not delete while running)
├── recorder.lock                    held by whichever process is recording (empty)
├── replayed.db                      the SQLite index
├── config.toml                      your settings
├── gsi-token-cs2.txt                the CS2 integration token
├── gsi-token-dota2.txt              the Dota 2 integration token
└── gsi\cs2\gamestate_integration_replayed.cfg
    gsi\dota2\gamestate_integration_replayed.cfg
```

The clips directory can be moved: *Settings → Library → Clips directory*, or
`[storage] clips_dir` in `config.toml`. The rest of the folder cannot be moved.

- **A move takes effect for the recorder when it restarts.** Changing the directory while a
  recording is running does not redirect that recording; the panel says so. Stop and start the
  recording to write into the new folder.
- **The window only reads a folder you picked in the folder picker** (or that the config file
  already named when replayed started). Once the window has stopped reading a folder, it cannot
  be pointed back at it until replayed restarts.
- **Automatic cleanup only deletes inside the clips and sessions folders.** If you move one,
  index rows that were written under the old folder are no longer cleaned up for you.
- **`recorder.lock`** is how the app and the CLI avoid recording into one data folder at the
  same time. The operating system drops the lock when the process ends, however it ends, so a
  crash does not leave it stuck. A second recorder that finds it held refuses to start and says
  so. Do not delete it to get around that message: find the other replayed first.
- **`scratch\`** is the shared scratch folder (`[buffer] scratch_dir` moves it). The replay
  buffer is held in memory and writes no segment files, so buffering puts nothing here. A full
  session writes its segments into a `session-<timestamp>` folder of its own under `sessions\`,
  not into `scratch\`. Automatic cleanup never deletes the scratch folder or what is in it, so
  it is safe to point it at a folder that holds other files, but nothing tidies it for you.

**Opening it from the application.** The **Storage** page has an *Open the data folder* button
next to the clips directory: it opens the folder that holds everything above in Explorer. There
is also:

- the **Health** page shows the log file's path, as text;
- a clip's page has a **Show in folder** button that reveals that clip in Explorer;
- the tray's **Open config file** reveals `config.toml` in Explorer — or opens the folder it
  belongs in, when there is no file yet.

---

## Your first minute

**The window tells you this itself on the first run.** An empty library shows a three-step
checklist — start recording, press the clip hotkey (the chord this process actually installed),
find the clip in the grid — so nothing here has to be remembered from a manual.

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

### Clips are deleted after 7 days by default

**Out of the box, a clip that is not a favourite is deleted when it is more than 7 days old.**
The same goes for full-session recordings. Two rules apply, and each clips-or-sessions library
has its own numbers:

| Rule | Clips | Sessions |
|---|---|---|
| Older than | `[storage] max_age_days = 7` | `[storage.sessions] max_age_days = 7` |
| Total size over | `max_total_bytes` = 50 GiB | `max_total_bytes` = 200 GiB |

**Favourite a clip to keep it.** A favourite is exempt from both rules (it still counts toward the
total). A session that is still recording is never deleted. Clips and sessions are cleaned
independently, so a large session library cannot push your clips out. Raise or lower the numbers
in `config.toml`; a zero is refused rather than read as "keep nothing". Cleanup only ever
deletes inside the clips and sessions folders: a session is only deleted if it lives under the
sessions folder (`[storage.sessions] sessions_dir`, which when empty means
`%LOCALAPPDATA%\replayed\sessions`).

### When the captured window changes size or goes away

- **The window is resized, or the display changes mode.** What happens depends on the kind of
  recording:
  - *Replay buffer:* a new buffer is started at the new size, and the footage it held before
    is gone, so the pre-roll starts again from nothing. A clip that was already waiting for its
    post-roll is cut first. The output follows the new size unless `encode.output_size` pins one.
  - *Full session:* the output size is held at what the recording started with, and the new
    picture is scaled to it. If the aspect ratio changed, the picture is stretched. That looks
    wrong, but the file plays.
  - *Zero-copy sessions, and a replay buffer that also saves its session:* these cannot follow
    a size change, so the recording ends cleanly with a message that says why, and the session
    is kept as far as it went.
- **The captured window is closed (or the game exits).** The recording ends cleanly and what was
  recorded is kept. It is not treated as a failure. The window shows *"The recording ended: …"*
  with the reason.
- **No new frames for a while.** A minimised window, or one whose picture is not changing, gives
  the recorder nothing to encode. After 5 seconds the window shows *"No new frames for N s"* and
  clears it when frames resume.
- **The graphics device is lost for good** (a driver reset the app could not recover from) is a
  failure, not a clean end, and the message names the reason.

These resize and close paths are covered by tests with a fake capture; none has been watched on a
real Windows game yet.

### Combining clips, and exporting one

**Combine** two or more clips into one, losslessly. Tick the box on each card in the Library, then
press *Combine N clips*; the result appears in the library as a new clip and the originals are
untouched. It is the clips' own packets in order — nothing is re-encoded — which is why every clip
in a combine must share one stream layout (same codec, frame size and audio-track count). Mixed
clips are refused, naming both, rather than concatenated into a file with a truncated track: a
stream copy cannot change the frame.

**Export** a clip from its page. Three presets, each a labelled re-encode:

| Preset | What it makes |
|---|---|
| Discord (720p, small) | H.264 + AAC fitted inside 1280×720, at a quality-based rate |
| Vertical 9:16 | 1080×1920, cropped from the centre — the sides are lost |
| GIF (480px, 15fps) | an animated GIF, no audio, 256 colours from the clip's own frames |

An export decodes and encodes **on purpose**: that is the only way to change the frame, and it is
the one media action here that encodes the video again. A video export has one audio track, and
with a microphone that track is the game audio and the voice mixed. The file is written beside the clip and is
**not** added to the library — it is made to be uploaded, and indexing it would put a derived copy
of the footage under the storage cap. The original is never modified.

### The hotkey

`Ctrl+F8` is the default. Rebind it in **Settings → Clip hotkey**: type a chord (`Ctrl+F8`,
`Alt+Shift+K`, `F9`), press *Apply hotkey*, and it is written to `[hotkeys] clip` in
`config.toml`.

A chord is Ctrl, Alt and Shift in any combination plus one key: `F1` to `F24`, one letter or one
digit. **A letter or a digit must carry Ctrl or Alt.** The chord is claimed system-wide, so a
bare `K` would stop you typing K in every other program, and `Shift+K` still types a capital K.
Both are refused with that explanation. A function key is fine on its own or with Shift.
Holding the chord down counts as one press, and two presses within 300 ms count as one, so a
held or bouncing key does not save a burst of clips.

Two things the panel is honest about:

- **A saved chord takes effect when replayed restarts.** `RegisterHotKey` is system-wide and the
  chord is registered once, at startup, and owned until the process exits; the panel says which
  chord is in force now beside the one you typed.
- **It checks the chord is free before writing it.** Another application — or another replayed,
  because the CLI and the app cannot both hold one chord — may already own it, and that is
  refused with a message rather than saved into a file that could not install it. The check is
  "free right now": anything can take the chord afterwards, which is why the startup's own
  registration failure is still reported.

The recorder bar always shows the chord in force and whether it is really installed.

### The command-line recorder

`replayed-cli` is the headless recorder: a console program with no window, tray or settings
page, and it is never started with Windows. It reads the same `config.toml` and refuses to start
on a bad one. It holds `recorder.lock` while it records, so it will not start while the app is
recording and the app will not record while it runs.

**Quit it with Ctrl+C in its console.** One press stops the recorder: a clip that is already
being written is finished and indexed, a session recording is joined into its `session-<timestamp>.mp4`,
and the program exits with status 0. A second Ctrl+C ends it at once without waiting. Closing
the console window does the same as the first press, but Windows allows only a few seconds
(4.5 s here), so a long session may not finish. This has not been run in a real console yet: it
is covered by unit tests and listed as a manual step in
[`docs/runbooks/phase-1-verification.md`](runbooks/phase-1-verification.md).

### Screenshots

**The Screenshot button** in the recorder bar (and the optional `[hotkeys] screenshot` chord)
saves one frame of the configured capture target as a lossless PNG in the library. It works while
nothing is recording — that is most of the time it is useful — and it needs no game running.

A screenshot is a real library entry: tag it, favourite it (which protects it from the storage
policy), delete it. Its card shows the picture with a `PNG` chip, and its page has no player, no
trim and no export, because there is nothing to play or cut. It cannot be combined.

### The optional chords

Three more chords exist and are **off unless you set them** in the same file:

```toml
[hotkeys]
clip = "Ctrl+F8"
toggle_recording = "Ctrl+F9"   # start / stop the recording
bookmark = "Ctrl+F10"          # a marker on the session timeline, no clip
screenshot = "Ctrl+F7"         # one frame of the display → a PNG in the library
```

### Starting with Windows

**Settings → Startup → Start with Windows** gives replayed an entry under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, so the tray icon and the hotkey are there
after a reboot. It is **off by default**, and the equivalent key is
`[app] start_with_system = true` in `config.toml`.

The entry starts replayed with `--background`: a launch by the login leaves the window hidden
and only the tray icon and the hotkey are there, so a 1280×880 window does not appear on top of
whatever you log in to. Starting replayed by hand, or from the Start Menu, shows the window as
usual. (The window may flash briefly on a background launch; that has not been checked on a
real desktop.)

The file is still the switch: applying the checkbox asks Windows for the entry immediately and
says what happened, and the next launch re-syncs the registry from the file either way. The entry
is per-user (no elevation) and only this application's value is touched, so a Run entry you added
by hand survives. Applying on a non-Windows build saves the setting and registers nothing, and
the notice says so.

### Where the settings live

Two places, and they are not the same thing:

- the **Settings page** in the window — *Library*, *Startup*, *Capture*, *Encoder*,
  *Automatic recording*, *Game integrations* and *Updates*. It edits the clips directory, the
  start-with-Windows entry, the frame rate, the resolution, the quality preset, the monitor and
  window to capture, the microphone and the desktop-audio device, the "also save the full session"
  switch, the automatic-recording switch and the watched-games list.
- **`config.toml`** — everything the panel does not edit: the hotkey chords, the GSI port and the
  event rules.

The desktop app and the CLI read the same file but not the same keys. **The desktop app ignores
`[audio] enabled` and `[recorder] mode`**: it always records the game audio and always records
the replay buffer (`[recorder] save_session` is the one `[recorder]` key it reads). Those two
are for the CLI, which takes `--mode session` as well. `config.example.toml` marks each key that
only one of them reads.

**Zero is refused where it would do damage.** A zero for `segment_time`, `post_seconds`, `fps`,
`[storage] max_total_bytes` or `max_age_days` (clips or sessions), a `ram_cap_bytes` below 8 MiB,
or a `gsi_port` below 1024 other than 0 is refused, with the key named, instead of being
clamped or guessed at. A zero storage cap would otherwise delete every clip that is not a
favourite. In the GSI settings, `gsi_port = 0` turns the listener off; **leaving the key out does
not**, because an omitted `gsi_port` means the default, 45671, with the listener on.

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

**A process-name match means the game is open, not that a match is under way.** For every title
except League the app can only see that the executable is running, so automatic recording starts
when you launch the game and keeps going through the menus and lobby. Only League's Live Client
API tells a match from the menu.

### What each integration actually reports

**Counter-Strike 2 and Dota 2 (Game State Integration).** A kill, a death, an assist, the round
starting and ending, the game starting and ending, and — in Counter-Strike — the bomb being
planted or defused. A kill, a death, an assist, a bomb event or the end of the game takes a
clip through the same trigger the hotkey uses. A round boundary is recorded as a timeline
marker and takes no footage — otherwise a whole match would write a clip per round.

**League of Legends (Live Client Data API).** This source is on by default
(`[events] lol_poll_enabled = true`) and only runs while a recording is running. Your kills, deaths and assists; dragons, barons,
heralds, turrets and inhibitors; first blood, an ace and a multi-kill (recorded as a generic
objective); and the start and end of the game. A kill, a death, an assist, an objective or the
end of the game takes a clip. The start of the game is a marker.

**League is read-only, and that is the point.** The app issues a `GET` to
`https://127.0.0.1:2999/liveclientdata/allgamedata` — the game's own local API — and reads the
answer. To decide whether a match is running at all it also reads
`/liveclientdata/gamestats`, and only a reply that describes a game counts. It never writes to the League client (the LCU), never POSTs or PUTs, and never opens
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
says so in the log, so a stale port does not silently stop the events. The same refresh happens
when a recording starts and the file is found in a Steam library, so a token that was regenerated
reaches the game's copy without pressing *Install* again.

**Upgrading from an earlier build:** a build that kept one shared `gsi-token.txt` is not read by
this one. Every game now has its own token (`gsi-token-cs2.txt`, `gsi-token-dota2.txt`), and the
installed `gamestate_integration_replayed.cfg` still carries the old one, so its events are
refused until the file is replaced. The desktop app replaces a file it finds in your Steam
libraries when a recording starts; for the CLI, or for a game that is outside a library it
detected, press *Install* or copy the file from `gsi\<game>\` by hand. Then restart the game.
This path has not been run on Windows against a real game.

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

A chord that breaks the rules is refused rather than half-installed: a **letter or digit needs Ctrl
or Alt** (`K` and `Shift+K` are refused), while `F8` or `Shift+F8` are fine. The desktop app then
reports that no hotkey was installed, and `replayed-cli` will not start; the message names the
chord.

### Recording will not start: "another replayed process is already recording"

Only one recorder may use a data folder at a time, and `recorder.lock` is how that is enforced:
the desktop app and `replayed-cli` each take it while they record. The message means exactly
that, so look for the other replayed (the tray icon, or a console window running the CLI) and
stop it. The lock is released by the operating system when its holder exits, however it exits,
so there is no need to delete the file, and deleting it is not a way round the message.

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
  30-second pre-roll covers that gap;
- for every title except League, a match of the executable name only says the game is *open*, so
  a recording that starts while you sit in the menu is the expected behaviour, not a fault.

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
  older than `storage.max_age_days` (**7 days by default**; see
  [above](#clips-are-deleted-after-7-days-by-default)) or when the clips directory goes over
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

The Health page's **Copy diagnostics** button puts a report on your clipboard and sends nothing
anywhere; you decide where to paste it. It holds the version and platform, your effective
configuration, the problems the app knows about, and the last 256 KiB of the log. What it
removes, and what it does not:

- **Removed:** credential keys in the configuration, the Game State Integration tokens
  wherever they appear (configuration or log), and `Authorization: Bearer` headers.
- **Shortened:** your Windows account name. Paths that start with your profile folder become
  `%LOCALAPPDATA%`, `%APPDATA%` or `%USERPROFILE%`.
- **Not removed:** game names, window titles, paths outside your profile folder, and the
  game-event payloads in the log, including player names and the League scoreboard. The report
  says so in its own heading. Read it, and edit it, before you paste it into a public issue.

The panel shows what was copied, so the text on your clipboard and the text on screen are the
same. If you would rather attach the log itself, it is here:

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

## Updating

**Settings → Updates → Check for updates.** That button is the whole feature. It makes one
request, to `https://github.com/FisiFla/replayed/releases/latest/download/latest.json`, and
nothing else in replayed ever contacts another machine. GitHub's "latest" is the newest
published release that is not marked a pre-release, so a release candidate is not offered.

If there is something newer, the panel shows its version, date and notes, with **Download and
install** and **Not now**. The download is checked against the minisign public key built into
the app before anything is installed; a file that does not match is refused and nothing is run.
The signature check is not a statement about the download link: the file is trusted because it
was signed with the project's key, not because it came over HTTPS. When the install finishes
the panel says to restart replayed.

**Installing an update closes replayed, and ends a recording that is running.** So the install
asks first. If a recording is running, the panel shows *Stop recording and install?*: the session
is saved first, exactly as the Stop button does it, but the replay buffer is not kept.
**Cancel** puts the offer back and changes nothing; nothing is stopped or downloaded until you
say yes. After an update that stopped a recording, start it again from the recorder bar.

This has not been exercised against a published release: none has been published. The unit and
component tests cover the panel and the refusal, and the ledger records the rest
([`docs/verification-status.md`](verification-status.md) §27).

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

There is **no telemetry, no crash reporting, no analytics, no account and no cloud**. While it
runs the app talks only to your own machine: a read-only poll of `127.0.0.1:2999` for League,
and a token-gated listener on `127.0.0.1` for Valve's Game State Integration. The one request to
another machine is the **Check for updates** button, which asks this project's GitHub release
page whether a newer version exists. Nothing checks on its own: there is no check at startup and
no polling. See [Updating](#updating).

What is written to disk, all under `%LOCALAPPDATA%\replayed`:

| File | What it is |
|---|---|
| `replayed.db` | the SQLite index: clip and session paths, sizes, durations, favourite flags, the marker rows and the event payloads |
| `logs\replayed.log` | the app's own warnings and errors, with paths and error text — and the event payloads, which include player names and the League scoreboard |
| `clips\`, `sessions\`, `thumbnails\` (and `scratch\`, if anything is in it) | the footage itself and the thumbnail cache |
| `gsi-token-cs2.txt`, `gsi-token-dota2.txt` | the generated integration tokens. They are never logged |
| `config.toml` | your settings |

The README's [Privacy](../README.md#privacy) section has the detail, including what the event
payloads carry and why the microphone is opt-in.

To follow the game's own audio, the app reads the process list to find that game's process id —
the same enumeration Task Manager performs. It opens no process, requests no access right and
reads no memory; it copies the image names out of a snapshot and compares them to your watch
list.
