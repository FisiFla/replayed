# Security policy

## Reporting a vulnerability

Please report privately through GitHub: **Security → Report a vulnerability** on this
repository (private security advisories). Do not open a public issue for a vulnerability.
Expect an acknowledgement within a week.

There is **no monitored security email address** in this repository and no other monitored
contact, so the private advisory above is the channel: it is the only one a report is read
on, and it is the one that keeps a report off the public issue tracker. A report made
anywhere else is not monitored.

---

## Scope and threat model

replayed is a local application: no accounts, no telemetry, no crash reporting, no cloud
upload. It listens only on loopback and makes no outbound connections other than to
loopback — **with one exception, the update check, described below**.

- the **game-state-integration (GSI) listener** — a token-gated HTTP listener on
  `127.0.0.1` (`crates/events/src/gsi.rs`; `bind` refuses any non-loopback address and the
  only constructor production uses, `bind_loopback`, takes a port alone, so a listener on
  `0.0.0.0` cannot be expressed);
- the **League Live Client Data API** — polled at `127.0.0.1:2999`, GET only, over TLS with
  Riot's root pinned and the platform roots disabled
  (`crates/events/src/lol/client.rs`; the one relaxation of the host-name check lives in
  that same file, and nothing may disable certificate-chain verification). The same
  service answers the **game-presence check**: a `GET /liveclientdata/gamestats` (a couple
  of hundred bytes) through the same pinned client, and the body has to name a game (a map
  or a running clock) for League to count as open. A `200` with `null`, `{}` or a body that
  is not JSON is not "a game"
  (`crates/events/src/process.rs::LiveClientPresence`, `serves_a_game`);
- the **audio transport to ffmpeg** — a link the encoder listens on and the ffmpeg child
  dials, and the encoder uses a connection only if the operating system says its peer **is
  the child it spawned**. Loopback is not enough: another program on the machine that dials
  freshly opened loopback ports would otherwise win the race, become the audio input and be
  fed the game's and the microphone's PCM. On Windows the link is loopback TCP and the owner
  of the client end of the accepted connection is read from the system TCP table
  (`GetExtendedTcpTable`, the table `netstat -o` prints) and compared with the child's process
  id; that is a read of a system-wide table, not an open of any process. Elsewhere it is a Unix
  socket in a directory created exclusively with mode `0700`, and the kernel reports the
  connecting process (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS). A peer that is not the
  child is dropped and the wait carries on to its deadline; a peer whose owner cannot be
  determined is refused the same way. **There is no fallback to an unauthenticated link**
  (`crates/encoder/src/audio_link.rs`).

The tree's outbound sockets (the files that call `TcpStream::connect`) are exactly five, and
that list is pinned by a test:
`crates/events/tests/no_egress.rs::only_the_loopback_modules_talk_to_a_socket_at_all`. The
audio link only *accepts* a connection, so it is not on that list; it is on the capability
check's exemption list below, with its reason. The
updater's request is made inside the updater plugin, whose code is not this tree's; what
this tree grants it, and how narrow that is, is described below. The
same file carries the rest of the egress guard, and it is worth knowing how:

- the capability check is a **denylist of names** with an explicit **per-file exemption
  list**, each exemption carrying its reason
  (`no_capability_to_reach_off_this_machine_is_used_outside_its_exemptions`), rather than
  an allowlist of constructors — an allowlist of constructors is only as good as the list;
- there is a separate **dependency check**: no HTTP client may be a direct dependency of any
  manifest, and no resolved dependency graph may hold one either, with a single exemption —
  the desktop shell's, because the updater plugin is in it
  (`no_http_client_is_a_dependency`);
- every `TcpListener::bind` in the tree is pinned to loopback, and the address is *read off
  the source* rather than guessed from a file-level mention
  (`every_listener_is_bound_to_loopback`).

In scope:

- a local attacker or malicious game config reaching the GSI listener,
- a malicious or corrupt clip / `replayed.db` causing file access outside the library
  (`delete_clip` refuses to unlink outside the clips and app-data directories —
  `apps/desktop/src-tauri/src/commands/clips.rs::is_inside_library` and
  `apps/desktop/src-tauri/src/paths.rs::ensure_within`),
- the webview (strict CSP, no filesystem/shell/http capability granted),
- the installer writing into a game's own folder, and the containment rule around it,
- the log file, and what a second instance or the webview can put in it,
- **the updater**: a tampered update manifest, a swapped update artifact, and what an
  attacker who can serve the manifest gets to choose,
- supply chain: pinned, checksummed ffmpeg sidecars; `cargo-deny`, `npm audit` and
  gitleaks in CI.

Out of scope: an attacker who already runs code as the same user.

---

## The updater: the one thing that reaches another machine

The application has exactly one outbound request, and the user starts it: **Settings →
Updates → Check for updates**. There is no check at startup, no polling, no background
task, no telemetry and no phone-home of any kind. If the button is never pressed, nothing
is ever requested.

**What is requested, and from where.** The endpoint is fixed in
`apps/desktop/src-tauri/tauri.conf.json` (`plugins.updater.endpoints`) and is this
project's own GitHub release page:

```
https://github.com/FisiFla/replayed/releases/latest/download/latest.json
```

The frontend cannot change it, and the webview cannot open a connection of its own: the
content security policy's `connect-src` is `ipc: http://ipc.localhost`, and the request is
made by the Rust side of the updater plugin, not by the window. The webview's only
permissions for it are `updater:allow-check` and `updater:allow-download-and-install`
(`apps/desktop/src-tauri/capabilities/default.json`).

**The trust anchor is the pinned minisign public key.** `plugins.updater.pubkey` in
`tauri.conf.json` holds the public half of the release key pair. It is compiled into the
binary, so it cannot be changed at runtime by anything on the machine or by the manifest.
Every artifact the updater downloads is checked against it **before it is installed**:

- the artifact's bytes are verified against the signature the manifest names, using that
  pinned key. A **tampered `latest.json`** — one that names a different artifact, or a
  signature that is not the one for those bytes — fails the check and **nothing is
  installed**;
- a **swapped artifact** — a genuine file of a different release, or a modified installer —
  fails the same check, because the signature covers the bytes;
- `plugins.updater.requireSignedVersion` is on, so the signature must also carry the
  version it was signed for. The manifest is not itself signed, so without this an
  attacker who can serve it could pair a *new* version number with an *older* release's
  URL and signature and force a downgrade onto a validly signed build. With it, the
  signed version and the announced version have to agree.

**The private key never ships.** It lives only in the release pipeline's secrets
(`TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) and in the
maintainer's own storage; it is not in this repository, not in the installer, and not in
the application's data directory. The Tauri CLI signs with whatever key it is given and only
warns when that is not the pinned one, so the release pipeline checks the signature itself
(`scripts/verify-updater-signature.py`, before the draft exists and again on the bytes GitHub
stored): a release signed with the wrong key fails at the pipeline rather than on a user's
machine.

**Installing an update ends a running recording, and asks first.** The installer replaces the
executable and the application exits, which would cut a recording off where it stands. So the
install first asks the shell to prepare for it (`prepare_for_update`). The shell, not the page's
last poll, decides whether a recording is running: if one is, it refuses unless the call says the
user agreed to stop it, and the Updates panel then asks. Nothing is stopped or downloaded until
the user answers; agreeing stops and finalises the recording (the same stop as the Stop button)
before the installer starts, and declining leaves the recording running and the update
uninstalled. The updater ends the process directly once the installer has run, which skips the
exit path that normally stops the recorder and finalises the segment in flight; this check is the
only guard on that path. If the stop fails, the install is not attempted
(`apps/desktop/src-tauri/src/commands/recorder.rs::prepare_for_update`,
`apps/desktop/src/lib/updater.ts::prepareToInstall`). This is a consent step in the user's own
window, not a defence against the webview.

**What this does not protect against.** The updater inherits the platform's trust: the
request is made with the platform's certificate roots, so whoever controls the DNS or the
network path to `github.com` can serve *a* manifest — but not one whose artifact carries
a signature from the pinned key. And the release pipeline itself is a trust anchor: whoever
can publish a release under this repository's identity can ship an artifact the updater
will accept. That is the same trust the GitHub Releases page already carries, and the
pipeline is the only place the signing key is used.

**What has not been verified.** The signature check path is unit-tested
(`apps/desktop/src-tauri/tests/updater.rs`), but **no release has ever been signed with
the release key and offered to an installed copy**: the end-to-end path is unverified, and
`docs/verification-status.md` §27 records that.


---

## The game-state listener: one token per game

The listener's token model is **per game**, and that is the point of it.

| | Counter-Strike 2 | Dota 2 |
|---|---|---|
| path on the listener | `/gsi/cs2` | `/gsi/dota2` |
| token file in the app data directory | `gsi-token-cs2.txt` | `gsi-token-dota2.txt` |
| Valve app id the payload must carry | `730` | `570` |

(`crates/events/src/gsi/integration.rs` — `game_path`, `CS2_PATH`, `DOTA2_PATH`,
`CS2_APP_ID`, `DOTA2_APP_ID`; the token files are named in
`crates/recorder/src/event_driver.rs::per_game_token`.)

The order of the checks is the security property, and it is in
`crates/events/src/gsi.rs::serve_one`:

1. the **route** is resolved first. A path no game serves is a `404` and the body is never
   read;
2. the **token** is looked for in the raw body, *before the body is parsed*, and compared in
   constant time over the whole value (`subtle::ConstantTimeEq`; `token_equals`, which
   fails closed on an empty expected token). A request whose token is not the one that path
   serves is `403`;
3. the **app id** is checked. A payload whose `provider.appid` is not that game's is `403`,
   even on the path the token was lifted for.

So a token lifted from one game's installed `.cfg` cannot forge the other game's events:
the path, the token and the app id have to agree. The per-game token files are the
application's own; the tokens are never logged and never printed — the `Debug` of a
binding, a configuration and a live binding all redact them
(`GsiGameBinding`, `GsiConfig`, `replayed_recorder::GsiBinding`).

**The old single token is not carried over.** An install that predates per-game tokens has
one shared `gsi-token.txt`. It is **not** copied into the per-game files: each game gets a
fresh token the first time, the old file is left where it is and is never read, and a
warning in the log says the integration has to be installed again
(`crates/recorder/src/event_driver.rs::per_game_token`). Carrying the old token forward would
have kept a token that is already sitting in a game folder, readable by the `Users` group,
valid on both games' paths. When the desktop app starts a recording it finds a
`gamestate_integration_replayed.cfg` in a detected Steam library game folder and rewrites it
to carry the new token and the live port; the CLI, and a game outside a detected library, need
the file installed or copied again, and the game restarted
(`apps/desktop/src-tauri/src/health.rs::refresh_installed_gsi`). The rewrite has not been
exercised on Windows.

**How a request is taken in.** The listener is reachable by every process on the machine and by
every web page in the user's browser, so the order in which it spends its effort is part of
the design (`crates/events/src/gsi.rs::serve`, "Starving the game"):

- no reader thread waits on a peer. The accept loop collects each connection's bytes without
  blocking and hands a reader only a request that is already whole in memory. A connection that
  sends nothing, or a head and then stalls, costs one of 16 pending slots and is answered and
  closed after 2 seconds; when the slots are full the new connection replaces the most idle one;
- a request that carries the token for its path goes in the **game's lane**: its own bound, served
  first, and not charged to the rate limit. Every other request waits in the anonymous lane and is
  charged to 100 requests per second, past which it is answered `429` unread. A peer without the
  token can use up its own budget and its own lane; it cannot reach the game's. Lane choice
  decides only where a request waits: the token is still checked in full, in constant time,
  before the body is parsed;
- a request a **web page** made is refused with `403` as soon as its head is in, token or not.
  Every browser fetch carries `Sec-Fetch-*` headers, which a script can neither set nor remove
  and which the game never sends (`crates/events/src/gsi/http.rs`, `Framing::Browser`). A
  browser too old to send them falls under the lane rules above.

What none of this defends against is a local process with enough CPU to starve the whole PC.

**The listener is bound before any configuration file is written.** The order is the point:
a `.cfg` written before the bind names a port nobody may be listening on — a taken port, or
an ephemeral one the operating system picked — and the game posts its token into the void,
possibly into a squatter's socket. The order is now: read the tokens, **bind**, then write.
Every file — in the application's data directory *and* in the game's own folder — names the
port the listener actually bound. A failed bind leaves no `.cfg` behind at all
(`crates/recorder/src/event_driver.rs::start_gsi`).

**The installer takes the live binding** instead of re-reading the token file, and **refuses
when no listener is running**: with nothing listening there is nothing for the game to post
to, so a file naming a port nobody serves would be a game whose every POST is answered
`403` (`apps/desktop/src-tauri/src/health.rs::install_gsi_cfg` and `GsiListener`; the
command wiring is `apps/desktop/src-tauri/src/lib.rs::install_gsi_cfg`). The write is
**atomic** — a uniquely-named temporary file in the same directory, written, flushed to
disk, then renamed over the destination — so the game reads either the old file or the new
one, never a half-written mixture, and a crash cannot leave a truncated `.cfg` behind
(`health.rs::write_cfg_atomically`). An installation whose live binding has moved is
rewritten when the listener comes up (`health.rs::refresh_installed_gsi`).

**The containment rule.** The destination must be exactly a known library's
`steamapps/common/<game folder>`, and the containment root is **the library** — not the game
folder and not the Steam root. A symlink planted inside the game folder that escapes the
library is refused, and the write goes through
`paths::ensure_new_file_within` before a byte is written
(`health.rs::trusted_install`).

A game in a **secondary** Steam library — one the user's own `libraryfolders.vdf` lists —
**is** installed. That file is the user's own Steam configuration: it lives in the user's
Steam directory, so no other unprivileged account can edit it, and the application writes
with that user's own permissions either way. Refusing a secondary library would break a game
on a second drive for no security gain. Where to *look* for libraries is Steam's registered
install path (`HKCU\Software\Valve\Steam`, value `SteamPath`, read through `reg.exe` from the
user's own hive; nothing is written) plus the default locations; those roots only say where to
look, and the containment rule above still applies to whatever they lead to
(`apps/desktop/src-tauri/src/health.rs::steam_roots`).

### The residual risks, stated plainly

These are **not** solved, and this document does not pretend they are.

- **The token is written in cleartext into the game's own `cfg` folder.** The game has to be
  able to read it, so it cannot be hidden from the game; on Windows that directory is
  readable by the `Users` group. Any local account that can read the game installation can
  therefore read the token and post forged events for the game the listener is pinned to,
  for as long as the installation keeps the token. What the work above buys is **scope and
  freshness** — a token lifted from Counter-Strike is useless on Dota 2 — not secrecy.
  Closing the remaining gap needs an ACL on the installed file, or not writing the token
  into the game folder at all. (`crates/events/src/gsi/integration.rs` says the same, in
  the module documentation.)
- **A fixed port is still squattable.** `[events] gsi_ephemeral_port` (default `false`)
  is the mitigation: the operating system picks the port, so a process that squats on the
  configured one never receives the game's POSTs, and the `.cfg` names the port the
  listener actually bound. The default is unchanged at `45671`, so the risk stands for a
  user who does not turn it on.
- **`events.gsi_port = 0` switches the listener off entirely**, in both modes. That key
  chooses *whether* there is a listener; `gsi_ephemeral_port` chooses *which* port it binds
  (`crates/recorder/src/event_driver.rs::EventsSection` and `start_event_sources`). Leaving
  the key out is **not** the same as `0`: an omitted `gsi_port` is `45671`, so the listener
  is on. A non-zero value must be between 1024 and 65535.
- The token in the application's own data directory is created owner-only (`0600`) where the
  platform has file modes. On Windows it is in the per-user application data directory with
  **no ACL hardening**.

---

## The webview

The window is a WebView2 (or WebKit) instance running the application's own bundle. It is
treated as untrusted input, not as the application.

- **Strict CSP** (`apps/desktop/src-tauri/tauri.conf.json`, `app.security.csp`):
  `default-src 'self'`, no `object-src`, no `base-uri`, no `form-action`, no
  `frame-ancestors`.
- **No filesystem, shell, http, dialog or notification capability**
  (`apps/desktop/src-tauri/capabilities/default.json`): `core:default` and the updater's two
  commands (`updater:allow-check`, `updater:allow-download-and-install`) — and nothing else.
  The folder picker and the "clip saved" toast are driven from Rust, which the capability
  system does not gate, so the page cannot call either itself. The updater
  permissions are **not** an http capability: they let the *Rust side* make the one
  request described above, to the one URL pinned in `tauri.conf.json`. The webview still
  has no way to open a connection of its own — the CSP's `connect-src` is
  `ipc: http://ipc.localhost` — and it cannot name a different endpoint.
- **The asset protocol is the window's only read channel**, so its scope is the boundary.
  The static scope is the default layout
  (`$LOCALDATA/replayed/clips/**`, `$LOCALDATA/replayed/thumbnails/**`) and the runtime
  scope is set to the resolved clips directory and thumbnails
  (`apps/desktop/src-tauri/src/lib.rs`). The webview **cannot widen it to an arbitrary
  directory**: a clips directory outside the application data directory has to be approved,
  and the approval is a directory the **user picked in a native folder picker the backend
  runs** (`choose_clips_dir` / `pick_clips_folder` / `approve_clips_root` /
  `clips_dir_allowed`). A path the webview names is not one it may name for itself.
- **The webview cannot forge log lines.** What arrives from the webview is untrusted text, so
  it is flattened to one line and capped: control characters, a newline among them, become
  spaces, and every frontend line is tagged `[webview]` so a forged backend line is
  distinguishable from a real one (`apps/desktop/src-tauri/src/lib.rs::log_from_frontend`
  and `flatten_for_log`).

### The test seam

`apps/desktop/src/main.ts` has one seam: the `?test-clip-source` query parameter together
with `globalThis.__replayedClipSource`, which replaces the **entire** IPC surface with a
fake. It exists for the screenshot harness (`scripts/shots.mjs`, headless Chromium).

It belongs in this threat model, plainly: **both** halves are required, and a shipped
window is loaded with no query string, so reaching it needs prior script execution in the
window. That is a capability an attacker who already has script execution in the webview
would have anyway — but it is a real one, and it is stated here rather than left to be
discovered. Nothing in the application reads either half.

---

## The log

The log is the application's own record of its warnings and errors: file paths and error
text, never the GSI token and never footage.

- **It is written under an exclusive lock**, held on a file of its own, `logs\replayed.lock`,
  not on the log: the log is renamed when it rotates, and a lock on a renamed file locks
  nothing. Two shells would otherwise interleave their lines into one file, and the rotation
  would rename a file the other process holds open — which fails on Windows. The process that
  cannot take the lock writes a file of its own (`replayed-<pid>.log`) rather than sharing
  (`apps/desktop/src-tauri/src/log_file.rs::open_locked`,
  `apps/desktop/src-tauri/src/lib.rs::init_tracing`).
- **"Copy diagnostics" is a manual, local copy.** It puts the configuration and the tail of the
  log on the clipboard when the user presses it; nothing is sent anywhere. Credentials are
  replaced with `<redacted>` (credential-looking keys, the GSI tokens wherever they appear, an
  `Authorization: Bearer` header), and the Windows profile prefix in paths is shortened to
  `%LOCALAPPDATA%`, `%APPDATA%` or `%USERPROFILE%`, which drops the account name. **That is
  all it does**: game names, window titles and paths outside the profile are still in the
  text, so read it before pasting it into a public issue
  (`apps/desktop/src-tauri/src/commands/diagnostics.rs`).
- **The webview cannot forge multi-line entries** — see above.

---

## The local data

Almost everything the application keeps is under one per-user directory:
`%LOCALAPPDATA%\replayed` on Windows
(`apps/desktop/src-tauri/src/config.rs::app_data_dir`). The clips and the sessions are the
exceptions — the user may point either elsewhere.

| | |
|---|---|
| `config.toml` | the settings, hand-editable |
| `replayed.db` | the clip index (SQLite) — **not** `localplay.db`; the rebrand renamed it |
| `clips\` | the recorded clips, unless another folder was chosen |
| `thumbnails\` | a cache; losing it costs one ffmpeg run per clip |
| `sessions\` | full-session recordings, unless `sessions_dir` says otherwise |
| `logs\replayed.log` | the log, rotated at 5 MB with 3 kept |
| `gsi-token-<game>.txt` | the per-game GSI tokens |
| `gsi\<game>\gamestate_integration_replayed.cfg` | the file to copy into the game's own folder |

**The data directory moves with the rebrand.** The project is now `replayed`; the directory
was `%LOCALAPPDATA%\localplay` and is now `%LOCALAPPDATA%\replayed`. The installer creates the
new directory before the application ever runs, so what decides the move is the data in it: if
the new directory already holds a `replayed.db` or a `config.toml`, this build has been used and
the old directory is left alone — merging two libraries is not something to do to somebody's
clips unasked. Otherwise each entry of the old directory is **moved across one at a time**
(renamed, never copied, never overwriting a name that is taken), and a directory present on both
sides is merged entry by entry. A failure loses nothing: what did not move is still in the old
directory. The desktop shell leaves a marker while entries remain, so an interrupted move is
finished at the next start, and it writes a report next to the data when anything was skipped or
left behind. The paths *inside* the index are then repaired: a row is rewritten only if its path
is under the old directory, nothing exists at it, and the same file exists under the new
one (`apps/desktop/src-tauri/src/config.rs::migrate_legacy_data_dir`,
`plan_legacy_path_repair`; the index is backed up first and the rewrite is one transaction that
changes a row only if it still holds the value that was read). The CLI has its own, simpler
move (`apps/replayed-cli/src/lib.rs::migrate_legacy_data_dir`).

The index is user-writable and `config.toml` is hand-edited, so a path read from either can
name anything. Every path the shell unlinks, removes or cuts from goes through
`apps/desktop/src-tauri/src/paths.rs` first: the path's real parent (symlinks, junctions and
`..` resolved) must sit under one of the directories replayed manages, and the path must be
a proper child of it.

**The automatic cleanup has its own, narrower rule.** Retention and cleanup run at start-up and
every few minutes with nobody watching, on paths read out of that same editable database. They
delete only through a `LibraryRoots` (`crates/store/src/roots.rs`): a clip's file must be inside
the clips directory, a session's concatenated file inside the sessions directory, and a session's
scratch directory must be a direct child of the sessions directory named `session-<digits>` (what
the recorder creates), never the clips directory or an ancestor of it. "Inside" is decided on
canonical paths, so a symlink or junction planted in the library cannot lead a removal out of
it. A row that fails the check costs the user the row, not a file, and is reported as refused.
The price is that rows written under a previous clips or sessions directory are no longer cleaned
up automatically after the setting changes. A shared `[buffer] scratch_dir` is never a session's
directory, so it is never removed as one.

One thing worth stating in a security document because it is **data at rest**: the derived
game payload is stored verbatim — in-game player names, kill/death/assist totals and, for
League, the match scoreboard with every player's name, champion, team and an `is_local`
flag. The GSI token and the footage are never in the index or the log.

---

## Supply chain

- **The bundled ffmpeg is pinned and checksummed.** `xtask/sidecars.toml` names a *versioned*
  BtbN release asset — the `win64-lgpl` variant, whose configure line carries
  `--disable-libx264 --disable-libx265` and no `--enable-gpl`, and whose licence banner
  reads "LGPL version 3 or later" — together with its SHA-256. The archive is verified
  before anything is extracted, and only the allowlisted paths inside it are. (The pin was
  repinned from Gyan's *essentials* build, which upstream documents as **GPLv3** and which
  carries libx264 and libx265, while the docs claimed LGPL.)
- **`cargo-deny`, `npm audit` and gitleaks run in CI** (`.github/workflows/ci.yml`). The npm audit
  covers dev dependencies as well as production ones and blocks at `--audit-level=moderate`.
- **The two crates that would synthesise input are vendored and patched** (see
  [Anti-cheat](#anti-cheat)).
- **Every CI action is pinned to a full commit SHA**, and clippy, the formatter, the
  Windows-target lint and the verification harness are gates rather than advisories.
- **The secret scan covers the whole history.** The per-push gitleaks step narrows itself
  to the commits the push brought, so a credential committed long ago was never reported —
  and this repository carried a committed TLS test key from 2026-09-23 to 2026-09-26
  without a scan ever mentioning it. A weekly `secret-scan` workflow now sweeps every
  commit, and the two documented exceptions (fixed hex strings in the GSI tests, and that
  removed test key) live in `.gitleaks.toml` with their reasons written down, rather than
  in a `.gitleaksignore` fingerprint that goes stale when a line moves.

---

## Anti-cheat

The project must be safe to run next to kernel anti-cheat. The policy is
[`docs/anti-cheat.md`]; these enforce it:

- **`scripts/check-forbidden-apis.sh`** greps the tree for the *names* of the APIs the
  policy forbids — process injection, hooking, reading or writing another process, and
  input synthesis. The list covers more names than it did (`OpenProcess`,
  `LoadLibraryEx`, the `Nt`/`Zw` memory calls, `DeviceIoControl`, `PostMessage`,
  `GetProcAddress` and more), and it now scans the **`scripts/`** directory as well as the
  Rust tree and the **vendored crates** under `apps/desktop/src-tauri/vendor/`, which are
  deliberately not excluded. It also bans a `.set_focus(` call in our own code. Window
  enumeration is the one group that cannot be forbidden outright, so it is asserted to be in
  the capture backend and nowhere else.
- **`scripts/check-forbidden-apis.sh --binary <exe>`** (which runs
  `scripts/check-pe-imports.py`) is the half that sees dependencies: it reads the import table
  and the delay-load table of the *built* executable, and scans it for the names of functions
  that would be looked up at run time, against an **empty** allowlist
  (`scripts/pe-imports-allowlist.txt`). A file it cannot read as a PE image, or one with no
  imports, fails rather than passes. CI runs it on the Windows build, and the release workflow
  runs it again on the binary that goes into the installer.
- **Two dependencies are vendored for this reason.** `tao` and `muda` each contained a
  `SendInput` call (tao's `set_focus` pressed a synthetic Alt key when the system refused to
  raise the window; muda's Copy/Paste menu items work by sending Ctrl+C/V), and the binary guard
  found the import in `replayed-desktop.exe`. They live in `apps/desktop/src-tauri/vendor/`
  with the call removed, wired in with `[patch.crates-io]`. The shell never calls `set_focus`;
  it raises its window with `SetForegroundWindow` and `FlashWindowEx`
  (`apps/desktop/src-tauri/src/window_focus.rs`), and a window the system will not raise stays
  where it is. How to re-vendor is in `apps/desktop/src-tauri/vendor/README.md`.
- **The CLI installs a console-control handler** for Ctrl+C, Ctrl+Break and closing the
  console, so a recording is finalised instead of abandoned. It is
  `SetConsoleCtrlHandler` for the process's own console: it sets a flag, touches no other
  process, installs no hook and synthesises no input
  (`apps/replayed-cli/src/shutdown.rs`).
- **`crates/events/tests/anti_cheat.rs`** pins the `windows` feature groups the events
  crate enables and the Windows APIs it imports — a list of what *is* allowed, so a new
  import from anywhere else fails whatever it is called. The groups that would allow
  opening a process, reading or writing another process's memory, or holding tokens are
  asserted to be absent.

---

## Release signing

**The build is not code-signed.** No certificate exists.

- On Windows, SmartScreen shows **"unknown publisher"** and hides the installer behind
  *More info → Run anyway*. That is the documented behaviour of an unsigned build, not a
  defect in this one, but a user has to know it.
- The certificate path is documented in [`docs/packaging.md`](docs/packaging.md) §5.9: the
  exact config keys (`bundle.windows.certificateThumbprint`,
  `bundle.macOS.signingIdentity`), the CI variables, and how to check that signing worked
  (`signtool verify /pa /v <installer>`). §5.1 states the consequence, §5.2 the
  notarisation gap.

A user who has not read that will meet "unknown publisher" on first run and may reasonably
decide the download is not what it claims to be. Say so here as well.

---

## What is verified, and what is not

[`docs/verification-status.md`](docs/verification-status.md) is the project's honest ledger.
It distinguishes *verified on Windows hardware* from *verified on the development host* from
*type-checked only* from *not performed*, and this document does not claim more than it
supports.

Two facts matter for the claims above:

- **The 2026-10-19 review sweep was verified on the development host (macOS) only.** The
  new GSI routing, the token scoping, the atomic install and the containment rule are
  covered by the unit and integration tests, and the Windows target compiles and lints
  clean — but the Windows *runtime* is unverified for that work, and no part of it has been
  exercised against a real CS2 or Dota 2 client (ledger §25, §26).
- **The game-state integrations have never run against a real game.** The payloads are
  canned fixtures and the servers are mocks in the test process. The token echo, the
  per-game routing and the app-id scoping are all proven against mocks.

So: the *design* of the listener is as described above and is enforced by tests; the
*behaviour against a real game* is unproven.
