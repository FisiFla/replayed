# Anti-cheat policy

replayed must be safe to run next to kernel anti-cheat (Vanguard, EAC, BattlEye, VAC).
It therefore uses **only** the capture paths OBS-style recorders use:

- Windows Graphics Capture (video) and WASAPI loopback / microphone (audio),
- `RegisterHotKey` for the clip hotkey,
- read-only, official game APIs: CS2 / Dota 2 Game State Integration (the game POSTs to us) and
  the League Live Client Data API (`https://127.0.0.1:2999`, GET only).

It never injects, hooks, reads or writes another process's memory, synthesises input, or writes
to a game client. `scripts/check-forbidden-apis.sh` enforces the API half of this in CI.

**The one request it makes to another machine.** *Settings → Updates → Check for updates*
asks this project's own GitHub release page whether there is a newer replayed. That is an
ordinary HTTPS GET, and then a download of the installer from the same page; it touches no
game, no game client and no process, it says nothing about the machine it runs on, and it
happens only when the user presses the button. Nothing else in the application reaches the
network. What it downloads is checked against the signing key this build was made with
before it is installed.

## What enforces it

- **Source guard.** `scripts/check-forbidden-apis.sh` greps our Rust, our scripts and the vendored
  crates for the names of the forbidden APIs (injection, hooks, reading or writing another process,
  input synthesis) and for Windows feature groups a crate has no business with. It also bans a
  `.set_focus(` call in our own code (see below). Window enumeration is allowed in the capture
  backend and nowhere else.
- **Binary guard.** `scripts/check-forbidden-apis.sh --binary <exe>` runs `scripts/check-pe-imports.py`
  over the *built* executable: its import table and delay-load table, and a scan for the names of
  functions that would be looked up at run time. The allowlist (`scripts/pe-imports-allowlist.txt`)
  is empty by design, so any match fails. This is what sees a dependency that calls a forbidden API
  without our source ever naming it. CI runs it on the Windows build and the release workflow runs
  it on the binary that goes into the installer.
- **No synthetic input from the window toolkit.** The binary guard found `SendInput` in the shell's
  import table through two dependencies: `tao`'s `Window::set_focus` (it presses a synthetic Alt key
  when the system will not raise the window) and `muda`'s predefined Copy/Paste menu items (they
  send Ctrl+C/V). Both crates are vendored and patched in `apps/desktop/src-tauri/vendor/`
  (procedure in its `README.md`). Do not call `set_focus`; raise the window with
  `apps/desktop/src-tauri/src/window_focus.rs`, which uses `SetForegroundWindow` and then
  `FlashWindowEx`. A window the system will not raise stays where it is.
- **Game APIs stay read-only.** The League presence check is one `GET /liveclientdata/gamestats`
  through the same loopback client, pinned to Riot's root certificate, as the event poller; the
  GSI listener only receives. Neither writes to a game. The listener refuses a request a browser
  made (it carries `Sec-Fetch-*` headers, which the game never sends) and keeps the game's own
  requests in a lane that a stranger's traffic cannot fill; the design is in `SECURITY.md`.
- **The CLI's console handler** (`apps/replayed-cli/src/shutdown.rs`) is `SetConsoleCtrlHandler` for
  the process's own console, so Ctrl+C or closing the window finalises the recording. It sets a flag.
  It touches no other process, installs no hook and synthesises no input.
- **The audio link authenticates the ffmpeg child by reading the system TCP table**
  (`GetExtendedTcpTable`), which opens no process and creates no handle to the game or anything
  else (`crates/encoder/src/audio_link.rs`).

Tested anti-cheats: none systematically yet — tracked in the release checklist. This project is not
affiliated with any game or anti-cheat vendor; use at your own risk.

## Operating rules for testing on a machine that runs anti-cheat

These apply to anyone (human or automated agent) verifying this project on a Windows machine
where a kernel anti-cheat such as Riot Vanguard is installed:

- **Never synthesise input.** No `SendInput`, `keybd_event`, SendKeys or AutoHotkey; ask the
  person at the machine to press keys physically. The CLI's `--self-test-clip-after` trigger
  exists so verification does not need a keypress.
- **Never write to a game client.** The League Live Client Data API is GET-only; never POST or
  PUT to the League client (LCU) API.
- **No injection, hooks or game-memory access** of any kind.
- **No screen capture while a protected game is running.** Check that the game and its client
  processes (for League of Legends: `League of Legends`, `LeagueClient`, `LeagueClientUx`) are
  not running before any run that touches the display.
- `cargo build`, `cargo test` and `cargo xtask probe` are CPU-only and safe to run at any time.
- From a remote shell (no desktop session), run GUI or capture steps through a one-shot
  scheduled task in the logged-on user's session, and clean the task up afterwards.
- Read the application log rather than asking for a screenshot.
- Clean up when finished: scheduled tasks, helper scripts and scratch build directories.
