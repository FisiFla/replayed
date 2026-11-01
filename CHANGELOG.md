# Changelog

All notable changes to replayed. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). `0.2.0` is the first versioned
entry in this file; the dated headings below it are when the work landed, not versions. For
what has and has not been *verified* on real hardware, read
[`docs/verification-status.md`](docs/verification-status.md), not this file.

## [Unreleased]

This is the production-readiness hardening pass after 0.2.1: a code review of the whole tree, and the
fixes it produced. Almost all of it has been verified on the development host and on the Windows test
machine's unit and integration suites. **None of it has been exercised against a real game, and a
signed update has never been offered to an installed copy**; the ledger
(`docs/verification-status.md`) says what each run did and did not prove.

### Security
- **The audio link authenticates its peer process.** ffmpeg reads the game's and the microphone's PCM
  from a local socket, and the first thing to connect used to win. Another program dialling freshly
  opened loopback ports (one was seen doing exactly that) could become the audio input and be fed the
  recording. The encoder now uses a connection only if the operating system says its peer is the
  ffmpeg child it spawned: on Windows by reading the TCP table (`GetExtendedTcpTable`, no process is
  opened), elsewhere through a Unix socket in a `0700` directory and the kernel's peer credentials. A
  stranger is dropped and the wait goes on; a peer whose owner cannot be determined is refused. There
  is no unauthenticated fallback.
- **Retention and cleanup delete only inside the library.** The unattended passes read paths from an
  editable database and used to check only that they were absolute. They now go through a
  `LibraryRoots`: a clip's file must be inside the clips directory, a session's file inside the
  sessions directory, and a session's scratch directory a `session-<n>` child of the sessions
  directory. Canonical paths are compared, so a planted symlink cannot lead out. A shared
  `[buffer] scratch_dir` is never removed as if it were a session's. A row that fails the check costs
  the row, not a file.
- **A zero retention limit is rejected instead of deleting everything.** `max_age_days = 0` or
  `max_total_bytes = 0` used to mark every non-favourite clip and session for deletion at the next
  cleanup. Both are now a configuration error, as are the other zero limits the engine cannot honour
  (`segment_time`, `post_seconds`, `fps`).
- **The GSI listener cannot be starved by a stalled peer or a web page.** Connections are collected
  without blocking, the game's own requests (recognised by their token) wait in a lane of their own
  that the anonymous rate limit does not charge, and a request a browser made (it carries `Sec-Fetch-*` headers) is refused with `403`.
  See `SECURITY.md`.
- **The legacy shared GSI token is no longer copied to both games.** Each game gets a fresh token, so a
  token read from one game's folder cannot forge the other's events. The installed
  `gamestate_integration_replayed.cfg` has to be refreshed: the desktop app does it for installs it
  finds when a recording starts, otherwise install it again (see Upgrading below).
- **No synthetic input in the shipped binary.** The binary guard found `SendInput` in the desktop
  executable's import table through two dependencies. `tao` and `muda` are now vendored and patched
  (`apps/desktop/src-tauri/vendor/`), the shell raises its window through `window_focus.rs` instead of
  `set_focus`, and the guard scans the vendored sources and the built binary against an empty
  allowlist.
- **The webview's permissions are narrower.** `capabilities/default.json` now grants only
  `core:default` and the updater's check and install; the dialog and notification permissions are
  gone, because the folder picker and the "clip saved" toast are driven from Rust.
- **The `--work-dir` of `cargo xtask verify` is only emptied if the harness made it** (a marker file),
  so a mistyped path can no longer be recursively deleted. Sidecar downloads refuse a redirect to
  anything but HTTPS and are size- and time-bounded.

### Added
- **Installing an update asks before it stops a recording.** The install used to end the process under
  a running recording. Now the shell refuses while one is running, the Updates panel asks, and
  agreeing stops and finalises the recording first. Declining installs nothing.
- **Quitting flushes a recording.** Quit from the tray, or any other exit that goes through the
  application's exit events, stops the recorder and finalises the in-flight segment instead of cutting
  it off. The updater's own exit skips this path, which is why the install asks first. The flush has
  not been checked on a real desktop (the ledger lists it as a smoke test).
- **Graceful shutdown for the CLI.** Ctrl+C, Ctrl+Break, closing the console and SIGTERM finish a clip
  in flight, write a full session out and exit `0`. A second Ctrl+C quits at once.
- **Start with Windows starts hidden.** The autostart entry passes `--background`, so a login start
  puts replayed in the tray; a normal launch, or a second launch handing over to a running copy,
  shows the window.
- **One recorder per data directory** (`recorder.lock`, an advisory file lock the operating system
  releases when the process exits, so a crash never leaves it stuck). A CLI started while the desktop
  app is recording is refused instead of "recovering" the desktop's live session and deleting the
  directory ffmpeg is writing into.
- **A microphone that disappears no longer ends the recording.** The outage is padded with silence
  (the pad is capped at 60 seconds), so the voice track stays aligned with the video, and the
  microphone is reopened when the device returns. Only the first open is fatal: a recording that asks
  for a microphone and never had one is still refused at start.
- **A recording follows its capture target when it is resized.** A replay buffer starts a fresh buffer
  at the new size and loses its pre-roll (clips already waiting are cut first). A full session holds its
  output size and scales the new picture to it, so a change of aspect ratio stretches. A zero-copy session,
  and a replay buffer that also saves its session, cannot follow: they end cleanly with a message and keep
  what was recorded.
- **A closed game window ends the recording cleanly, not as a failure.** The window shows "The recording
  ended: ..." with the reason, and "No new frames for N s" after 5 s without a frame (a minimised or
  unchanging window). A graphics device that is lost for good is still a failure, with its reason.
  None of this has been watched on a real game; it is covered by tests with a fake capture.
- **Hotkeys are registered with `MOD_NOREPEAT` and debounced** (300 ms, at most 8 presses queued), so
  holding the chord saves one clip and a stalled consumer cannot fire a burst afterwards.
- **The CS2 integration subscribes to `player_id`**, and the desktop app rewrites an installed `.cfg`
  that lacks it, so a spectated player's scoreboard can be told apart from the user's. CS2 warmup
  kills no longer produce clips.
- **A data-directory migration report.** The move from `localplay` is per entry, resumable and merges
  directories present on both sides; anything left behind is logged and written next to the data. The
  paths inside the index that still name `localplay` are repaired in one transaction after a backup.
- **Library schema v7** adds the `pending_deletes` table: a file whose row was removed but whose unlink
  failed is retried by the next pass instead of escaping the index and the size cap for good.

### Changed
- **A/V start alignment.** Audio and video now share one origin, the first video frame: audio from
  before it is cut away and audio that starts later is preceded by silence for the gap, instead of the
  audio running ahead of or behind the picture for the whole file.
- **Frame scheduling drains a stall's lag.** After a capture stall the frames are paced out from the
  capture clock again instead of staying behind by the length of the stall.
- **The encoder probe caches only successes.** A probe that failed (a busy GPU, a driver hiccup) used to
  be remembered and read as "no hardware encoder"; it is now asked again.
- **Hardware encoder failures surface.** A Media Foundation encoder that failed mid-run used to die
  silently; the error now propagates out of the encoder to its caller.
- **NV12 requests fail closed.** An encoder opened on NV12 used to be fed BGRA frames at the first
  input. It now refuses an input format the texture does not match, so a recording cannot come out as
  garbage or empty.
- **Variable-bitrate peak is 150% of the mean** (it was pinned to the mean, which made VBR behave as
  constant bitrate).
- **A `%` in `scratch_dir` (or in the account name) works.** ffmpeg expanded `%` sequences in the whole
  segment path; every `%` in the directory is now escaped.
- **Recovery drops a truncated tail.** A session whose last segment was cut off by a crash is
  concatenated from the segments that are readable, instead of failing for good.
- **Retention runs off the capture thread**, a clip's window is anchored at the event rather than at the
  moment the clip was cut, and a post-roll no longer blocks Stop.
- **Hotkey rules.** A letter or digit needs Ctrl or Alt (Shift alone is not enough); a function key can
  be bound bare. The letter `F` can now be bound as a key. A bare letter in `[hotkeys]` is no longer
  registered: the CLI refuses to start, and the desktop app reports that no hotkey was installed.
- **A process-name match means the game is open**, not that a match is in progress; the guide and the
  README now say so. League's presence is read from `/liveclientdata/gamestats` and the body has to
  name a game, so a `200` with an empty document is not one.
- **The CLI applies the same engine validation as the desktop.** It used to carry its own copy of the
  checks; both now call the recorder library's, so a value one refuses the other refuses, with the same
  message.
- **Retention deletes a session only under the configured sessions directory**, and an empty
  `sessions_dir` means `<app data>/sessions`. Before, with the clips directory moved, session cleanup
  looked for sessions in a folder beside the clips directory and refused every eviction, so the sessions
  cap was never enforced.
- **The desktop ignores `[audio] enabled` and `[recorder] mode`.** They are read by the CLI only; the
  settings panel and the documentation now say so.
- **Changing the clips directory while a recording is running needs a restart** for the recorder
  (it keeps writing where it started); the panel says so. A clips directory the window's file scope
  has already been withdrawn from cannot be moved back to until replayed restarts, because the scope
  can only be narrowed for the life of the process, and Settings refuses the move with the reason
  instead of leaving clips that do not play.
- **Copy diagnostics shortens the Windows profile name out of paths** (`%LOCALAPPDATA%`,
  `%USERPROFILE%`), writes UTF-8 to the clipboard so a non-ASCII profile or game title survives, and
  says in its own heading that it is not safe to publish unread: game names and window titles are
  still in it.
- **The thumbnail cache is keyed by clip id and creation time**, so a recycled id no longer shows the
  previous clip's frame; thumbnails nothing owns are swept once at start-up.
- **Failed trims and combines remove the file they claimed**, and two identical commands in the same
  millisecond get different names.
- **CPU criterion.** `cargo xtask verify` judges CPU against a flat 5% of one core on a quiet screen
  (an offered rate of 15 fps or less).
- **`npm audit` runs at `--audit-level=moderate`** and covers dev dependencies.
- **"Show in folder" selects the file in Explorer.** The path was quoted for `/select,` in a way
  `CreateProcess` rewrote, so Explorer opened a default folder instead; the argument is now passed as
  it is, from every place that reveals a file.
- **The Steam install is found from its registered path**, `HKCU\Software\Valve\Steam` (`SteamPath`,
  a read of the user's own hive through `reg.exe`), as well as the default locations, so a Steam on
  another drive is found without a manual copy of the integration file. The containment rule for
  where the file may be written is unchanged.
- **The CLI claims its hotkey before it starts the recorder.** It used to start first, and crash
  recovery in a second copy could "recover" the live session of the copy that held the chord.

### Release
- **`release.yml` is four jobs: gate, build, sign and publish**, each holding only the authority it
  needs. The build job has no secret, so the code that runs every build script holds neither the
  updater key nor a write token; the sign job is the only one with the key and runs in the `release`
  environment; the publish job creates a draft, checks what GitHub stored, and only then publishes.
  A tag containing `-` is published as a pre-release and can never become `releases/latest`. The
  gate refuses a commit whose `CI` run has not passed, a tag that is not on the default branch, and
  a tag that disagrees with any of the six version sources
  (`scripts/check-release-version.py`). Every cargo command is `--locked`.
- **The assets are `replayed_<version>_x64-setup.exe`, its `.sig`, `SHA256SUMS` and `latest.json`.**
  Tauri 2 signs the installer itself, so there is no `.nsis.zip`. See `docs/packaging.md`.

### Fixed
- **The microphone could be recorded and never heard.** It was a separate second audio track,
  and the app's player (like most players a clip is shared to) plays only the first, so the
  voice was in the file and nobody heard it. And an export kept one audio track, the game
  audio, so the Discord and Vertical presets dropped the voice altogether. A saved clip with a
  microphone now leads with a **Game + Mic** mix, followed by the game audio and the microphone
  as recorded; only the mix is encoded again. An export carries the voice, mixing the two
  tracks itself when the source has no mix (a full session, or a clip saved before this).
- **The microphone's start line named the default device even when another was configured.**
  It now says whether the device was the default communications microphone or the one in
  `[mic] device`; the endpoint id beside it was already right.
- **A library moved from the old `localplay` directory no longer lists every clip as missing.** The
  index stores absolute paths and they still named the old directory; they are repaired (see above).
- **Video-only clips can be trimmed**: the trim mapped an audio stream unconditionally and failed on a
  clip with none.
- **A spectated teammate's kills are not counted as the user's** (CS2 shows the observed player's
  scoreboard once the user is dead). Unverified against a real payload.
- **A clip favourited while the cleanup plan was being made is no longer deleted.**
- **The Capture settings and the Encoder panel no longer overwrite each other's frame rate and output
  size with stale drafts.**
- **The log's lock is held on `replayed.lock`**, not on the log that rotation renames.
- **A chatty ffmpeg no longer stalls a timed-out media command.** Its output pipes are drained while
  waiting, so a trim or probe fails with ffmpeg's own error instead of "timed out".
- **Closing a hardware encoder no longer crashes the process.** NVIDIA's Media Foundation encoder
  still has work queued when it is released, and says it has shut down anyway; the work then ran in
  freed memory (an access violation, about one test run in three). A closed encoder's last
  references are now held for 100 ms before they are released, which adds that much to a stop or a
  resize.
- **Screen capture no longer crashes when its last capture session closes.** COM unloaded the
  capture components while their threads were still exiting; the process now keeps its COM apartment
  for its whole life.
- **The hardware encoder is taken from the GPU the capture runs on**, and from the requested vendor's
  encoders only. On a machine with two GPUs it used to be whichever encoder sorted first, which could
  be another GPU's (one that cannot see the captured frame) or another vendor's than the one asked for.
  `encode.zero_copy = true` with no matching encoder on the capture's GPU is refused, and the message
  names that GPU.
- **Intel Quick Sync encodes through the zero-copy path.** Its encoder asks for three frames at once
  and only the first request was kept, so the second frame waited for a request that had already
  come; and it answers `E_UNEXPECTED` to the read right after a format change, which ended the
  recording at its second frame.
- **A hotkey listener that is closed gives its chords back.** Its thread waited for messages for the
  life of the process, so a chord stayed registered after its listener was dropped, and claiming
  the same chord again in that process was refused as "already registered". Dropping the listener
  now stops the thread, which unregisters the chords before it ends.

### Upgrading
- **Reinstall the CS2 and Dota 2 integration if the game does not react.** The old shared token is not
  reused. Recording from the desktop app rewrites an existing
  `gamestate_integration_replayed.cfg` it finds in a detected Steam library; with the CLI, or a game
  outside one, copy the file again from `gsi\<game>\` in the data directory, then restart the game.
- **A bare letter or digit hotkey, or a zero retention limit, is now refused.** The CLI will not start
  on either; the desktop app reports that no hotkey was installed, and does not start a recording with
  a zero limit. The error names the key.

## [0.2.1] - 2026-10-24

The production-readiness and Outplayed-parity work from the 2026-10-24 plan: the stale index and
its reconciliation, automatic full-session as a first-class feature, combine and the export
presets, the screenshot hotkey, and the settings the panel was missing. It was verified on the
Windows test machine — the workspace suite, the desktop crate, `cargo xtask verify` (29 checks, 0
failed), a real 4K WGC frame written as a PNG, autostart and hide-on-close — and the ledger
(`docs/verification-status.md` §38–§51) records exactly what each run proved and what it did not.
A real game with an integration has **not** been exercised against this build.

### Added
- **A screenshot hotkey** (`[hotkeys] screenshot`, and a **Screenshot** button in the recorder
  bar): one frame of the configured capture target, saved as a lossless PNG in the library. It
  needs no recording to be running. Schema v6 gives `clips` a `kind`, so a screenshot is a real
  library entry — taggable, favouritable, counted by the storage policy — with no duration, no
  player, no trim and no combine.
- **Combine clips, losslessly** — tick two or more cards in the Library and press *Combine*: the
  result is their packets in order (`-c copy`), indexed as a new clip, originals untouched. Mixed
  codecs, frame sizes or audio-track counts are refused by name rather than concatenated into a
  file with a truncated track.
- **Export presets** — *Discord* (720p, small), *Vertical 9:16* (1080×1920, centre-cropped) and
  *GIF* (480px, 15fps, no audio). These are the one media path that re-encodes on purpose; the file
  lands beside the clip and is deliberately not added to the library.
- **The clip hotkey can be rebound** in Settings → Clip hotkey. The chord is written to
  `[hotkeys] clip`, checked free first (another application, or another replayed, may own it), and
  takes effect when replayed restarts — the panel says which chord is in force meanwhile. The
  parser no longer cuts a multi-character key down to its first letter (`Ctrl+Hovercraft` used to
  become Ctrl+H).
- ***Start with Windows*** is a checkbox in Settings → Startup. The file stays the switch; applying
  asks Windows immediately and reports what the registry said, including on a build that has no
  such mechanism.
- **A first-run checklist** in an empty library: start recording, press the chord this process
  actually installed, find the clip. No more remembering the manual.
- ***Open the data folder*** on the Storage page: one button for the folder that holds the clips,
  sessions, index, log and `config.toml`, instead of copying a path out of a page. The guide said
  there was no such button, and now there is one.
- **The watch list is editable in the window** (*Settings → Automatic recording → Watched
  games*): add a game by executable name (a pasted path is reduced to its file name), give it a
  displayed name, choose process-name or Live Client Data API detection, and remove one with ×.
  The list is written to `[[games.watch]]` through `toml_edit`, so every other key and comment
  in `config.toml` survives; an emptied list is refused because the engine reads no list as the
  six default titles. Each entry is validated by the engine's own rule (a blank executable, or
  the same executable twice, is refused) plus one the panel adds: an executable that does not
  end in `.exe` can never match a running game.
- **A session says which kind of recording it is**: "Valorant — full session" beside the replay
  buffer's "Valorant". The game name was already on the row; the mode is what a user could not
  see.
- **The display's refresh rate, and the framerate the user picks** (`capture_displays`): the
  encoder panel now says which display is being recorded and the rate that display runs at
  ("Recording display 0 — 3840×2160 at 120 Hz"), and offers that rate in the framerate list
  next to the saved one. The rate is **offered, never chosen** — the saved rate stands unless
  you change it — and the panel says so plainly when the chosen rate is above the display's
  ("a rate above 60 fps records frames it never shows"). A rate the operating system does not
  really report is reported as unknown rather than believed.
- **`replayed-cli displays`** — the displays the capture can target, each with the index
  `[capture] monitor` takes, its size and its refresh rate. The index printed is the capture's
  own: a list assembled anywhere else can order the displays differently, and then its
  "index 1" names a different screen than the capture's "index 1". That is not hypothetical —
  it is how a measurement came to capture the wrong display and report a frame rate for it.

### Changed
- **Automatic recording is one section, and it says what each tier does.** The two switches
  (`[games] auto_record` and `[recorder] save_session`) were three groups apart with nothing
  connecting them, so turning on "record watched games automatically" and getting only hotkey
  clips was an easy mistake. The panel now holds both, states that League of Legends, CS2 and
  Dota 2 clip themselves while every other watched game is kept as a full session plus hotkey
  clips, and warns when automatic recording is on with session saving off.
- **The storage policy plans retention over files that are on disk, not rows that claim to be.**
  A row whose file was deleted in Explorer still counted its recorded bytes, so a full-looking
  library could evict a live clip to pay for empty space. Plan order is oldest-first, so the
  live clip evicted was the *older* one and the phantom stayed.
- **The capture backend waits for a frame instead of knocking for one**: the frame pool's
  `FrameArrived` event is registered, and the consumer waits on a condition variable the
  handler raises. The backend had been spending **79% of the recording's wall clock** asleep
  for a millisecond at a time and asking the frame pool for a frame that had not arrived.
- **The throughput probe measures the path the recording will actually take.** It used to
  measure a raw-pixel pipe — about 97% of the pipeline's work, the very cost the zero-copy
  path removes — and the number it produced was used to cap the recording, so a machine that
  does 4K fine was told "encode.fps = 60 is not achievable at 3840x2160". The zero-copy
  configuration is now measured with the same hardware encoder the recording uses. On an
  RTX 3090 at 4K that reads **154.3 fps**.
- **A failed probe no longer stops the recording.** The measurement is a benchmark of a
  separate encoder instance; the recording's own encoder is opened moments later. A probe
  that cannot run now costs the recording the measured rate, and says so, instead of costing
  it the recording.
- **The status line accounts for all of the wall clock.** Two shares were added — the
  frame-pool drain and the pacer's sleep — so the line adds up to 99.8% instead of leaving a
  share of every second unattributed.

### Fixed
- **A clip or session deleted outside the app left a phantom, and the storage panel offered no
  way to remove it.** The library and the storage figures already hid such a row, but it stayed
  in the index for ever: the sessions list had no existence filter at all, retention planned
  over it, and nothing pruned it. Now the sessions list hides it, retention ignores it, the
  storage panel counts both libraries' phantoms and offers a two-step *Remove from the library*
  that deletes the rows (with their tags and events) and nothing on disk. It refuses when the
  clips directory cannot be read, and leaves a row alone when the directory its file lived in is
  unreachable — an offline share looks exactly like a library whose files were all deleted.
  A session that is still recording is never touched.
- **The throughput probe feeds a texture per frame, and reports what the encoder said.** It
  re-submitted one zero-filled texture and told the encoder a one-frame-per-second timeline
  (a whole second between frames while the sample duration said `1/fps`), two differences from
  the recording that #79 is about; and on failure it now reports whether the transform is
  accepting input, whether a sample is ready, and how many times it renegotiated.
- **The warning that blamed you for the encoder's rate** ("the encoder cannot sustain the
  {}fps it was told: only {:.1} frames per second **are reaching it**") reported the rate the
  encoder *did* sustain as evidence it could not sustain it, and advised lowering
  `encode.fps` as if the request were the problem. It now describes what is happening: the
  encoder is taking that many frames per second of what the pipeline is offering it, so its
  queue is dropping the rest.
- **The pacer no longer loses a frame a second at 120 fps.** At that rate the frame interval
  is 8,333,333.33 ns, and a clock in whole microseconds drifts a third of a microsecond per
  frame — a whole frame late by the hundred-and-nineteenth. The schedule is in nanoseconds
  now, and there is a test that admits 120 frames in a simulated second.
- **A recording could hang and produce nothing at all.** ffmpeg opens its inputs in the order
  they are declared, and the raw-H.264 video input blocked: the demuxer cannot probe the
  stream until it has seen a keyframe carrying the sequence header. So ffmpeg sat in the video
  input's probe, the **audio input was never opened**, and the recorder's audio writer waited
  ten seconds for a connection that could not happen. It was intermittent in the worst
  possible way — a display that changes often delivers keyframes quickly, and a quiet one
  does not, so a recording of a still screen failed and a recording of a moving one did not.
  The audio inputs are now declared first, and the encoder's sequence header is sent with the
  first frame, so the demuxer has something to probe.
- **A hardware encoder that renegotiates its stream no longer ends the recording.** Media
  Foundation reports a stream change by its return code, not by an event. The code read that
  as a failure and stopped the encode loop, so the rest of the recording would have had no
  video at all. A stream change is now renegotiated — a new output type the transform itself
  offers, the input type set again, the bitrate re-pinned — and it is reported as a failure
  only if the renegotiation itself fails.
- **The audio backend says what it is doing, and a start that produces nothing is refused.**
  Every line the audio backend wrote was a success line, so a machine whose audio service did
  not answer said nothing at all. The open is now bounded (five seconds) and its first block
  is waited for (three), the source is announced *before* it is opened, and the recording
  reports how many audio blocks the audio side handed the encoder and how many the encoder's
  queue dropped.
- **The audio device's own numbers are bounded.** The buffer length the endpoint reports was
  handed to the audio engine unchecked; a device reporting an impossible one is now clamped
  to something usable, or refused with a reason.

## [0.2.0] - 2026-10-20

The first versioned entry, and the first release of the renamed project. It carries the
2026-10-19 review sweep — thirty-six issues, opened as #27-#62 — and everything that
followed it up to this date.

**What is verified and what is not.** The sweep was built and tested on the **development
host (macOS)**; the Windows target compiles and lints clean, but the Windows *runtime* is
unverified for it, and the game-state work has never been exercised against a real CS2 or
Dota 2 client. The ledger says so item by item in §25 and §26; nothing here claims more
than it supports.

### Added
- **Game audio by process loopback** (`83eaa0c`): the game track now records only the
  watched game's own audio — WASAPI process loopback on that game's process id — so voice
  chat, music and notifications stay out of the clip. **Privacy-relevant**: the default is
  narrower than the old "everything the speakers play", and it falls back to the whole
  output only while no watched game is running. `[capture] game_audio = "system"` restores
  the old behaviour.
- **Per-match segments and a match timeline** (`dd77671`): a session is split into per-match
  segments, the sessions view plots the matches in it on a timeline, and a clip can be cut
  out of one match rather than the whole session. The League end-of-game scoreboard is
  captured with it.
- **Game events as MP4 chapters** (`0f39d22`): the events inside a clip's span are written
  into the file as chapters, so they are visible in the clip itself and not only in the
  library.
- **Schema v5 library** (`de2051c`): a clip carries a title, a game and tags, the library
  view searches and filters on them, and a clip can be copied, revealed or dragged out.
- **Replay buffer plus full session off one encode**: `[recorder] save_session = true` (Settings →
  "Also save the full session") writes the whole recording to a session file while the buffer keeps
  serving clips, with no second encoder. The file is written as fragments arrive, so a crash loses
  about a second and the next start recovers it; on stop it is remuxed into the usual
  `session-<timestamp>.mp4`. A disk that cannot keep up ends the session file early with a warning and
  never the buffer. Covered by tests against the software encoder; unverified on hardware.
- **Audio device selection**: Settings now picks the microphone (`[mic] device`) and the output
  device recorded as desktop audio (`[capture] output_device`) from the active Windows endpoints;
  empty keeps the Windows default. A chosen device that is unplugged at start refuses the
  recording instead of recording another one. The "Microphone track" checkbox is now "Record
  microphone". Windows-only paths type-check but are unverified on hardware.
- **Desktop UI overhaul**: a navigation rail (Library, Sessions, Storage, Health, Settings), a
  clip-card grid, per-view pages, Inter / JetBrains Mono, Lucide icons and a role-named dark
  token set. Polish: view cross-fades, Esc / Alt+Left to leave a clip page, toasts that
  dismiss themselves (6 s notices, 12 s errors), one empty-state style, `prefers-reduced-motion`
  honoured (and set in the screenshot run). README screenshots are refreshed from `npm run shots`.
  **Unverified:** everything was rendered in headless Chromium against fixtures; WebView2, real
  DPI scaling (150%) and Inter's variable-font rendering on Windows have not been seen.
- **Health page** in the desktop window: recorder state, the encoder the last recording ran,
  free disk space where clips are stored, how each game-event source started, and the last
  warnings and errors from the log.
- **Game integration installer** in Settings: finds Counter-Strike 2 and Dota 2 in the Steam
  libraries and writes `gamestate_integration_replayed.cfg` into the game's `cfg` folder when
  you press Install. The webview only names the game; the destination is checked against the
  library the game is in (symlinks and `..` refused).
- **Privacy section** in the README: what is recorded, that it stays local, and that there is
  no telemetry.
- This changelog.
- Opt-in `toggle_recording` and bookmark hotkey chords, with a Bookmark marker kind (2026-10-17).
- Toast and sound when a clip is saved, from any trigger (2026-10-17).
- `[events] mute_clips`: keep chosen highlight kinds as timeline markers instead of clips
  (2026-10-16).
- The game-event sources (League, CS2 / Dota 2) now run inside the desktop app, with burst
  coalescing (2026-10-16).
- Backup of the clip database before a migration; `SECURITY.md`, an anti-cheat policy with an
  API guard, and a release checklist (2026-10-16).
- **`[events] gsi_ephemeral_port`** (default `false`): the operating system picks the port
  the game-state listener binds, so the port cannot be squatted on before the application
  starts. The generated `.cfg` names the port the listener actually bound either way.
  `events.gsi_port = 0` still switches the listener off entirely, in both modes.

### Changed
- **The project is `replayed`.** Until this release the same application answered to three
  names: `better-outplayed` in the installer, the window and the tray; `localplay` in the
  binary, the data directory, the database and the game's own configuration file; and
  `localplay-*` on the seven crates. Everything is `replayed` now: the crates, the CLI
  package, the binary, the database (`replayed.db`), the game's
  `gamestate_integration_replayed.cfg`, the asset scope and the Tauri identifier. The one
  name left alone is the bare word "Outplayed" in the README, which names the commercial
  product this one is compared against.
- **The data directory moves with it, and the old one is migrated in place.** The directory
  was `%LOCALAPPDATA%\localplay` and is now `%LOCALAPPDATA%\replayed`. Both binaries rename
  the old directory over when it exists and the new one does not, and leave the old one alone
  if both are there — merging two directories is not something to do to somebody's library
  unasked — so a user keeps their clips, their index and their settings. The move is a
  rename, not a copy, so a failure leaves the old directory exactly as it was.
- `RecorderStatus` reports the encoder in use; the event driver reports how each source
  started (`SourceReport`).
- `docs/verification-status.md` and the README no longer say the desktop GUI or
  `cargo xtask verify` have never run on Windows (they ran 2026-09-26 and 2026-10-15), and the
  ledger tally was recounted from the item tables (53 distinct claims).
- **The bundled ffmpeg is now an LGPL build.** The pin was Gyan's *essentials* archive, which
  upstream's own page says is **GPLv3** and which carries **libx264 and libx265** — while the
  README and the pin file said it was LGPL and that "LGPL is sufficient". The pin now points
  at a versioned BtbN release asset whose configure line carries `--disable-libx264
  --disable-libx265` and no `--enable-gpl`, with its SHA-256 recorded in
  `xtask/sidecars.toml`. A versioned asset also stops the pin breaking on every upstream
  rebuild. `apps/desktop/THIRD_PARTY_NOTICES.md` now names the bundled ffmpeg, the Tauri
  runtime, the npm packages and the Rust crates, and ships with the installer.
- **CI now lints the code that actually ships.** The clippy step's `-A clippy::unwrap_used`
  was overriding the workspace's own `[lints]` table, so the `unwrap_used` guarantee its
  comment claimed CI enforced was enforced nowhere. The two blanket `-A` flags are gone, the
  test bodies are exempted structurally instead, and there is a **Windows-target** clippy step
  — 52 `cfg(windows)` blocks across 16 files had never been linted at all. `cargo fmt --check`
  and a `cargo xtask verify` run now happen in CI too, and every action is pinned to a full
  commit SHA.
- The Windows job found something the macOS cross-check could not: `frame_gaps`, a helper in
  the recorder's test module, is called only by the `#[cfg(unix)]` splice test, so on Windows
  nothing calls it and `dead_code` fires under `-D warnings`. It is gated like its only
  caller. The workflow comment now also says **why** `replayed-recorder`, `replayed-store` and
  `replayed-cli` are missing from the macOS cross-check — rusqlite's bundled SQLite is
  compiled from C source and building it for the MSVC target needs the MSVC C toolchain, which
  a macOS host does not have. They are not left unlinted: the `windows` job covers them, and
  that is how the finding surfaced.
- **`cargo xtask verify` gained `--strict`**, which makes a *not performed* check a non-zero
  exit, and its verdict line now reads `PASS with N check(s) not performed` instead of a bare
  `PASS`. The harness's two ring rows were rewritten to match the engine: the cap row is
  `ram_cap` (against `buffer.ram_cap_bytes`, the bound the engine really enforces) and the
  eviction row reads the ring's own `evicted=` counter instead of looking for segment files
  that buffer mode never writes.
- The desktop shell no longer exits on a malformed `config.toml`: it starts with the example's
  values and says why, in the window. The global hotkey is now installed **after** the
  single-instance check, so a double launch cannot leave the survivor with a dead hotkey. The
  six blocking commands (`trim_clip`, `thumbnail`, `copy_clip_to_clipboard`, `delete_session`,
  `list_audio_devices`, `update_settings`) run off the UI thread, so the window no longer
  freezes for every trim, thumbnail and clipboard copy. The clips directory can only be
  changed to a folder the **user** picked, through a native folder picker the backend runs —
  the webview can no longer widen the asset protocol's read scope to any path it names.

### Fixed
- **The game state integration can no longer be squatted, go stale, or be forged with another game's token.** Each game now has its own token and its own path, and the listener refuses a payload whose provider app id is not that game's, so a token lifted from one game's installed `.cfg` cannot forge the other game's events. The listener is bound before any `.cfg` is written, and every `.cfg` -- in the app's data directory and in the game's own folder -- names the port it actually bound rather than the configured one. The desktop shell no longer re-reads the token file on its own: the installed `.cfg` is written from the live binding, it is written atomically, it is refreshed when the listener's port moves, and an install is refused outright when no listener is running. `[events] gsi_ephemeral_port` opts into an unguessable port for anyone who does not need a fixed one. The old single token file is migrated into the per-game files, so an existing install keeps working. *(Superseded in the Unreleased section above: the legacy token is no longer carried over, and the installed `.cfg` has to be refreshed.)*

- **The installer writes where the library is, not where the game folder is.** The destination
  must be exactly a known library's `steamapps/common/<game folder>`, and the containment root
  is the **library**, so a symlink planted inside the game folder that escapes the library is
  refused. A game in a secondary library — one the user's own `libraryfolders.vdf` lists — is
  installed: that file is the user's own Steam configuration, no other unprivileged account
  can edit it, and the application writes with that user's own permissions either way.
  Refusing it would break a game on a second drive for no security gain.
- **The screenshot harness drives the whole clip-source contract, and the build now checks
  that it does.** The harness's fake `ClipSource` had fallen behind the interface: the
  method that opens the native folder picker was missing, so the settings screenshot
  failed with `chooseClipsDir is not a function`, and seven further methods were missing
  besides. The harness was not a CI gate, so the README's "0 failures" went stale instead
  of turning red. Every method the interface declares is now implemented, a new test
  (`clip-source-contract.test.ts`) fails the build when the two fall apart again, the
  screenshot run is a CI step, and the settings panel's Browse button treats a picker
  that is missing or throws as "no directory" rather than an uncaught error.
- **The secret scan now covers the whole history, not just the commits a push brought.** The
  per-push gitleaks step narrows its scan to the range the push touched, so a credential
  committed long ago was never reported — and this repository carried a committed TLS test
  key from 2026-09-23 to 2026-09-26 without a scan ever mentioning it. A weekly
  `secret-scan` workflow now sweeps every commit, and the two documented exceptions (fixed
  hex strings in the Game State Integration tests, and that removed test key) live in
  `.gitleaks.toml` with the reason written down, instead of in a `.gitleaksignore`
  fingerprint that goes stale when a line moves.
- **The webview can no longer forge log lines.** The frontend's own log command flattened its
  input to one line and capped it, and every frontend line is tagged `[webview]`, so a newline
  in the text can no longer end the log line and have the rest of it read as a backend entry.
- **The log is written under an exclusive lock, and the lock works on Windows.** Two shells
  would otherwise interleave their lines into one file, and the rotation would rename a file
  the other process holds open — which fails on Windows. `LockFileEx` refuses a handle that
  was not opened with the `GENERIC_READ` or `GENERIC_WRITE` access right, and `append(true)`
  on its own opens the file with `FILE_APPEND_DATA` and nothing else, so the lock failed with
  `ERROR_ACCESS_DENIED` ("Access is denied") rather than with the "somebody else holds it"
  the caller asks about: the second shell was handed a permission error, and the log it was
  supposed to fall back to was never reached. Both handles that get locked now carry
  `read(true)`, and the test makes the second holder a real second process, because a
  byte-range lock is scoped to a process and the case that matters is two shells.
- Dependabot bumps applied together with the code changes they needed (2026-10-16).
- **A failed recording start no longer bricks games mode.** The recorder took its single
  SQLite connection out of the struct before the fallible half of the start handshake and only
  put it back on the success path, so one transient failure — a probe timeout, a locked
  database, a WGC session held by another app — left auto-record dead for the life of the
  process, reporting the misleading message *"this recorder's store is already in a
  recording"*.
- **A clip no longer punches a hole in the recording it was cut from.** The ffmpeg splice ran
  on the pump thread, so the capture pump stopped for the length of the remux and the encoder
  dropped the frames it missed. The splice now runs on its own thread and the pump keeps
  going.
- **Event markers and MP4 chapters are stamped at the moment the event happened**, not when
  the command was handled — a burst-coalesced kill was landing up to 18 seconds late — and
  chapter offsets are derived from the clip's real first frame rather than the requested
  window start.
- **The post-roll wait no longer loses clips.** The budget ignored the session ledger's
  one-segment trust lag, so with `segment_time >= 6` every clip timed out and was lost.
- **The index no longer loses a clip to a locked database.** SQLite had no busy timeout
  anywhere, and two connections write to the same file, so a hotkey press racing a UI write
  could leave a clip on disk with no row — invisible in the library and unmanaged by the
  storage policy.
- The ffmpeg child is now killed **and reaped** when an encoder is dropped, instead of
  leaving an unreaped process and three detached writer threads.
- Durations derived from configuration and file names are now saturating, so an absurd
  `pre_seconds` cannot wrap a timestamp.
- The GSI listener now checks the token **before** parsing the body, and serves connections
  from a small pool instead of one at a time — a browser page could previously make it parse
  2 MB of JSON per request on a single thread and hold the real game's events off the socket.
- The zero-egress guard is a capability denylist with an explicit per-file exemption list
  rather than a three-literal allowlist, so `reqwest::get`, `ureq::get` and
  `Command::new("curl")` no longer pass it unnoticed. A dependency check was added beside it:
  no HTTP client may be a direct dependency of any manifest.
- The anti-cheat guard covers the APIs the policy names but the script did not
  (`OpenProcess`, `LoadLibraryEx`, the `Nt`/`Zw` memory calls, `DeviceIoControl`,
  `PostMessage`, `GetProcAddress`, and more) and now scans `scripts/` as well.
- The Timeline's *"click a marker to seek to just before it"* actually seeks now; the press
  bubbled up to the track and clobbered the marker, so the feature was dead at runtime. The
  trim handles answer `Home`/`End`/`PageUp`/`PageDown`, the two extraction buttons agree,
  and an overlapping library refresh can no longer resurrect a stale list.
- `escape_ffmetadata` escapes `\r` as well: a carriage return in a chapter title could inject
  a `title=` line into the next chapter.
- The desktop's autostart entry is removed even when the registry query fails, and a `reg`
  failure is no longer read as "the value is not set".
- The README, the ledger, the runbooks, the packaging notes and `apps/desktop/README.md` were
  corrected against the code: the status block, the TLS story, the ring buffer, the DXGI
  fallback, the encoder path, the audio source, the CI description, 25 commit hashes that did
  not exist in this history, and the "there is no Windows runner" claim. The test counts
  follow the tests: the desktop README and the ledger's §25 said 249 frontend cases, which
  stopped being true when the screenshot harness's contract guard added two.
- **`SECURITY.md` was brought back in line with the code.** It still described the single
  token, the single path and the configured port, and it did not mention the app-id scoping,
  the `gsi_ephemeral_port` key, the installer's live binding, the cleartext token in the
  game's own folder, the release signing status or the test seam in `main.ts`. It says now
  what the code does, and it names the residual risks as risks.

## 2026-10-15 - encoder and capture rate honesty
- The configured video and audio bitrates are now actually sent to the encoder; the rate is set
  through `ICodecAPI`, not only described (superseded on 2026-10-17: the MFT still overshoots;
  see ledger §19 and §24).
- `[audio] enabled = false` gives a true video-only recording.
- The ring reports where its footage starts instead of assuming zero.

## 2026-09-26 - diagnosable desktop app
- The app writes a rotating log file and a panic hook; frontend errors go to the same log.
- Subprocesses no longer open console windows.
- `list_clips` omits clips whose file is gone and `thumbnail` refuses them: a stale index had
  caused an ffmpeg storm.
- Settings panel (library folder with a Browse button, framerate, resolution, microphone,
  auto-record).
- Windows installer configured and measured; signing path prepared for when a certificate exists.

## 2026-09-25 - zero-copy capture
- Hybrid GPU video path removes the CPU readback (CPU per 4K capture from ~50% of a core to
  ~3%); the hardware encoder runs on its own thread.
- Project renamed to `better-outplayed`; renamed to `replayed` again on 2026-10-20.

## 2026-09-24 - first Windows runs
- Whole workspace built and tested on Windows; first real capture, clip and hardware encode.
- In-memory replay buffer; game presence through process names and Riot's Live Client API.

## 2026-09-23 - initial build
- Phases 0-4: workspace, lossless media tools, ring buffer and clip splicing, capture backends,
  hardware-encoder selection with smoke test, SQLite index and storage policy, CLI, Tauri
  desktop shell with tray and global hotkey, LoL and CS2 / Dota 2 event integrations, sidecar
  fetching with checksums, CI.
