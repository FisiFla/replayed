# Packaging replayed

**Status: packagable, not shippable.** Two things were true before this document existed, and
neither is true now: `bundle.active` was `false`, so `tauri build` produced no artifact anyone
could install; and the ffmpeg sidecars went into the repository's `binaries/` directory, which
an installed application never looks at, so a shipped build would have fallen through to `PATH`
and refused to start on a machine without a system ffmpeg — defeating the entire sidecar
pipeline.

What is still **not** true is more important than what is, and §5 lists it in full: the
artifacts are **unsigned**, the **icon is a placeholder**, there is **no auto-update**, and
the Windows installer has been **installed and uninstalled on the test machine but never on
a user's machine** (§3.5). Read §5 before repeating anything in §1 as a
release claim.

This document is written the same way as
[`verification-status.md`](verification-status.md): every claim is tagged with how it is known
— *built here*, *read from the Tauri source*, or *not verified*.

---

## 1. What is built, and what was actually built

| Target | Config | Artifact | Built in this repository? |
|---|---|---|---|
| Windows | `bundle.targets: ["nsis"]` (`src-tauri/tauri.windows.conf.json`) | `better-outplayed_0.1.0_x64-setup.exe` | **Yes** — built, installed, run, uninstalled and reinstalled on the Windows test machine on 2026-09-26 (§3.5) |
| macOS | `bundle.targets: ["app"]` (`src-tauri/tauri.macos.conf.json`) | `replayed.app` | **Yes**, on this development host (§4) |
| macOS | (not enabled) `.dmg` | `replayed_0.2.0_aarch64.dmg` | **No** — `hdiutil` is refused in this environment (§4.3) |
| Linux | — | — | No target: the capture path is Windows-only (spec §2) |

Common metadata, from `apps/desktop/src-tauri/tauri.conf.json`, all of it consistent with the
rest of the repository rather than invented here:

| Field | Value | Where it comes from |
|---|---|---|
| `productName` | `replayed` | the product's own name, and what the installer shows; §3.3 records when it changed and why the install directory follows it |
| `mainBinaryName` | `replayed` | without it the installed executable would be `replayed-desktop`, the Cargo package name |
| `version` | `0.2.0` | `[workspace.package] version` in the root `Cargo.toml`, and `package.json` |
| `identifier` | `io.github.fisifla.replayed` | reverse-DNS of the project's owner (`github.com/FisiFla`, the maintainer's address in the git history). Fixed now on purpose: it is the Windows uninstall key and the macOS bundle id |
| `publisher` | `FisiFla` | the copyright holder in `LICENSE-MIT` and the GitHub account that owns the repository |
| `copyright` | `Copyright (c) 2026 FisiFla` | verbatim from `LICENSE-MIT` |
| `category` | `Video` | the app records and cuts video; maps to `public.app-category.video` on macOS |
| `homepage` | `https://github.com/FisiFla/replayed` | `repository` in the root `Cargo.toml` |
| `shortDescription` | *Local, zero-cloud game clipping and replay buffer for Windows* | the README's tagline |
| `longDescription` | the README's first paragraph, plus the Windows/encoder requirement | README + spec §2 |
| `licenseFile` | `../LICENSE.txt` | a short file that states the **dual** licence (MIT **OR** Apache-2.0) and points at both texts and at the third-party notices, so the installer shows a licence page that is not wrong. It was unset for a long time because naming a single licence file would have misstated the dual licence; the file states the dual licence instead |
| `resources` (bundle) | the sidecars, plus `LICENSE.txt` and `THIRD_PARTY_NOTICES.md` | the notices have to travel with the binaries they describe — the bundled ffmpeg is LGPL v3-or-later and its source offer lives in the notices file |

---

## 2. How the bundled ffmpeg reaches an installed app

This is the part that decides whether a shipped build works at all, so it is documented
end to end.

### 2.1 The sidecars come from `binaries/`, through Tauri's resource mapping
`cargo xtask sidecars fetch` downloads the pinned ffmpeg archive, verifies its SHA-256 against
`xtask/sidecars.toml`, and extracts `ffmpeg`/`ffprobe` into the repository's `binaries/`
(gitignored — see §2.4 for why a `.gitkeep` inside it *is* tracked).

`apps/desktop/src-tauri/tauri.conf.json` then hands that directory to the bundler:

```json
"resources": { "../../../binaries/": "binaries/" }
```

The relative path is resolved from `src-tauri/`, so `../../../binaries/` is the repository's
sidecar directory, and the destination is the bundle's `<resource dir>/binaries/`. This is the
map form of `bundle.resources` (Tauri v2: *"To fine control where the files will get copied
to, use a map instead"*), and the directory form means the destination keeps the file names
exactly as fetched: a `.exe` on Windows, a bare `ffmpeg` elsewhere.

### 2.2 Where that lands, per platform

`<resource dir>` is whatever Tauri's `resource_dir()` resolves to, and it is **not the same
directory** on both platforms (Tauri's own documentation of that method, and the installer
templates):

| Platform | Resource directory | Sidecars therefore at | Installer mechanism |
|---|---|---|---|
| Windows (`nsis`, `msi`) | the directory holding the executable | `%LOCALAPPDATA%\replayed\binaries\ffmpeg.exe` (per-user) or `C:\Program Files\replayed\binaries\ffmpeg.exe` (all-users) | the NSIS template does `SetOutPath $INSTDIR` and then `File /a "/oname=<destination>"` for every resource |
| macOS (`.app`) | `<Foo>.app/Contents/Resources` | `replayed.app/Contents/Resources/binaries/ffmpeg` | `bundle_project` copies resources into `Contents/Resources` and the main binary into `Contents/MacOS` |

### 2.3 How the app finds them (`crates/media/src/binaries.rs`)

`FfmpegBinaries::discover` searches, in this order, and takes the first directory holding both
binaries:

1. **an explicit directory**, if the caller named one (nothing in the application does today);
2. **next to the executable** — `<exe dir>/binaries/`. On Windows this is the installation
   directory, i.e. exactly where the installer's resource mapping puts them;
3. **inside the installed bundle's resources** — `<exe dir>/../Resources/binaries/`, which is
   the macOS `.app` layout. This is the candidate that did not exist before, and without it a
   packaged macOS app never looks inside itself;
4. **a checkout's `binaries/`**, found by walking up from the executable to the first directory
   with a `Cargo.toml` — the *development-only* fallback that a `cargo run` executable needs,
   since it lives in `target/<profile>/`;
5. **`PATH`**, last, because a system ffmpeg is the one source replayed cannot vouch for (the
   LGPL-only, hardware-encoder-only assumptions of spec §2 do not hold for an arbitrary build).

Everything an installer can produce outranks everything a developer's machine can produce, and
the failure message names every location it searched, in order.

### 2.4 Why `binaries/.gitkeep` is tracked

`tauri-build` resolves every `bundle.resources` entry at compile time and **fails the build**
when one does not exist. A mapping into a gitignored directory would therefore make the desktop
crate uncompilable on any host that has not fetched the sidecars — including CI, which never
fetches them (the pinned archive is Windows-only). A zero-byte, tracked `binaries/.gitkeep`
keeps the directory present everywhere; the `.gitignore` rules that make that possible are
commented in place, because git cannot re-include a file inside an excluded *directory*.

### 2.5 Why bundling refuses to run without the sidecars

Tauri copies whatever is in the source directory — including nothing. A missing pair would
produce a perfectly valid installer whose application cannot clip, discovered by a user rather
than by us. So `build.beforeBundleCommand` runs `npm run sidecars:check`
(`apps/desktop/scripts/require-sidecars.mjs`), which fails the bundling step — after the app is
compiled, before any installer is written — if `binaries/` does not hold the pair under the
names the host platform needs, and prints the sizes and destination of what it is about to
embed. It is not run by `tauri dev`.

What that check does **not** do: prove the files are a real, working, correctly-licensed
ffmpeg. Only `cargo xtask sidecars fetch` proves that, by verifying the archive's pinned hash.
§4.2 is the case where this matters. It also does not stop a *second* platform's sidecars being
bundled: the mapping copies the whole directory, so a checkout holding both the fetched Windows
pair and a macOS one embeds both (§4.1 shows the 201 MiB difference that makes). The check
prints what else it found rather than failing on it, because a developer working across both
platforms from one checkout is a normal thing to be doing.

---

## 3. Building the Windows installer (the shipping target)

Run on a Windows 10 1903+/Windows 11 x64 host with the Rust MSVC toolchain, Node.js 20+, and a
hardware encoder for anything that records (none is needed to build):

```powershell
# 1. The sidecars: pinned and checksum-verified, extracted into binaries\ (gitignored).
cargo xtask sidecars fetch --target x86_64-pc-windows-msvc

# 2. Frontend dependencies, exactly as CI installs them.
cd apps\desktop
npm ci

# 3. Compile and bundle. `-t/--target` is optional on a native x64 host.
.\node_modules\.bin\tauri build --target x86_64-pc-windows-msvc
```

> **Call the CLI directly in PowerShell, and this is not a style preference.** `npm run tauri --
> build --bundles nsis` *loses the flags on Windows*: PowerShell treats `--` as end-of-parameters
> and strips it, so npm never sees the separator, swallows `--bundles` itself, and the CLI ends up
> running `tauri build nsis` — which forwards the stray `nsis` to cargo, which fails with
> `error: unexpected argument 'nsis' found`. Measured on 2026-09-26 while building the first
> installer: the bundle failed at exactly this, having already succeeded at fetching node, the
> sidecars and the npm dependencies. The `npm run tauri` form is fine with no flags after it, and
> the `--` form is correct in bash, which is why CI uses it.

Expected artifacts (the paths and names follow Tauri's conventions; **neither could be
produced from macOS**, because the installer cannot be built here — §3.5 records the one
that was built on the Windows test machine, and it is named as above):

```text
apps\desktop\src-tauri\target\x86_64-pc-windows-msvc\release\bundle\nsis\replayed_0.2.0_x64-setup.exe
apps\desktop\src-tauri\target\x86_64-pc-windows-msvc\release\bundle\msi\replayed_0.2.0_x64_en-US.msi   # only with --bundles msi
```

**This is a compile-and-bundle step and nothing else. It does not launch the application.**

For an MSI as well, build with `.\node_modules\.bin\tauri build --bundles nsis,msi` (or add `"msi"` to
`tauri.windows.conf.json`). MSI is opt-in because WiX v3 can only run on Windows, the build needs
the Windows VBSCRIPT optional feature, and the result is a per-machine install that prompts for
elevation — none of which suits a consumer clipping utility. NSIS gives a single `.exe`
installer with a per-user install and no UAC prompt.

### 3.1 What the NSIS installer does

Read from Tauri v2's NSIS template (`crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi`),
not from a run:

- installs the executable and every resource to `$INSTDIR`, per-user by default:
  `%LOCALAPPDATA%\replayed` (all-users, `C:\Program Files\replayed`, is
  available in the UI
  and needs elevation). `tauri.windows.conf.json` pins `installMode: currentUser` explicitly so
  this is a decision rather than an inherited default;
- creates a Start Menu shortcut (and an optional desktop shortcut offered by the finish page),
  and an Add/Remove Programs entry (`DisplayName`, `Publisher`, `InstallLocation`, `DisplayIcon`)
  plus `uninstall.exe`;
- **checks and installs the WebView2 runtime if it is missing**, which by default means
  downloading Microsoft's bootstrapper — see §5.4;
- refuses to install while replayed is running, and reuses the existing installation record
  when it is run over an existing one (a non-NSIS install of the same product is detected and
  the user is asked to remove it first).

### 3.2 What it does not do

- It does not install ffmpeg system-wide, and does not touch `PATH`. The bundled copy is the
  app's private one — that is the point of the sidecar pipeline;
- it registers no file associations and no URL protocols (none are configured, and the asset
  protocol is an internal Tauri scheme);
- it installs no service, scheduled task or run-at-startup entry (Tauri's elevated update task
  is not enabled — there is no updater to drive it);
- it does not check for a GPU or a hardware encoder. replayed fails loudly at *runtime* when
  the machine cannot encode (spec §3.2), which is deliberate;
- it has no licence page (§1).

### 3.3 Two Windows-specific things a release has to decide

Both *were* read out of the NSIS template; §3.5 records what they actually did when the installer was first built and run on 2026-09-26.

1. ~~**The install directory is also the application's data directory.**~~ *Decided and set
   2026-09-26:* the installer gets its own directory. `productName` is now `replayed`
   while `mainBinaryName` stays `replayed`, and Tauri derives the per-user install path from
   the former and the installed binary's name from the latter — so an install lands in
   `%LOCALAPPDATA%\replayed` with the exe still called `replayed.exe`, while the
   app's data stays at `%LOCALAPPDATA%\replayed` exactly where it was and **no migration is
   needed**. The alternative — moving the data into a subdirectory — would have been a
   recorder/store change plus a migration for the existing install, for the same result.
2. **The uninstaller's "delete app data" checkbox does not delete replayed's data.** It
   removes `%APPDATA%\<identifier>` and `%LOCALAPPDATA%\<identifier>` — Tauri's directories,
   which this app does not use. The checkbox is therefore misleading in the *safe* direction:
   clips and the index survive an uninstall. A release should either point it at
   `%LOCALAPPDATA%\replayed` through an NSIS hook, or drop the checkbox and say plainly that
   recordings are never deleted by the uninstaller.

### 3.4 Checking an installer without installing it

On the Windows test machine, the installer's contents can be listed without running either it or the
app (`7z l better-outplayed_0.1.0_x64-setup.exe`, 7-Zip understands NSIS archives) — useful as a
first-pass check that `binaries\ffmpeg.exe` is inside. `npm run sidecars:check` already prints
what was embedded at build time, and that *is* verified here (§4.2).


### 3.5 What building and installing actually did

Run on the Windows test machine on 2026-09-26 — the first time any of §3.1–3.3 was observed rather than read
out of a template. The working directory was a scratch directory, and the machine
is a real, persistent one, so the install was real and left installed at the end.

* **The build.** The machine had no node at all. It was fetched as a *portable zip* rather than
  installed — `node-v22.12.0-win-x64` extracted into the working directory, `PATH` and npm's cache
  set per run — so nothing was added to the system. Then `cargo xtask sidecars fetch --target
  x86_64-pc-windows-msvc` verified and extracted both sidecars, `npm ci` installed the frontend,
  and the bundle produced **`better-outplayed_0.1.0_x64-setup.exe`, 262.6 MB**. That size is the
  ~201 MiB of sidecars plus the embedded WebView2 that §5.4's `offlineInstaller` decision buys, and
  it is the first Windows installer this project has ever produced.
* **The installer.** Silent, per-user, **no UAC prompt**, 21 seconds. It landed in
  `%LOCALAPPDATA%\replayed` — the separate directory §3.3 decided on — holding
  `replayed.exe` (13989 KB), `uninstall.exe` (77 KB) and `binaries\{ffmpeg,ffprobe}.exe`
  (100.5 MB and 100.3 MB). So `productName` really does move the install directory while
  `mainBinaryName` keeps the executable's name, and the §2 resource mapping really does put the
  sidecars where discovery looks for them.
* **The application.** Launched through `schtasks /it`, because it is a tray application and needs
  a real session: 37.5 MB working set, still running at 30 seconds. Killed afterwards.
* **The uninstall.** Silent. The installation directory was gone afterwards, and the Start Menu
  shortcut (`replayed.lnk`) with it.
* **The user's data.** Untouched by all of it. `clips\`, `thumbnails\`, `scratch\`, `config.toml`
  and a 28 KB `replayed.db` all survived the uninstall, and the one recording in `clips\` was
  still there. §3.2 called the "delete app data" checkbox misleading *in the safe direction*; it is,
  and now it is measured rather than reasoned.
* **Reinstall** after the silent uninstall reproduced all of the above with the data still intact.

What this still does not cover: an **upgrade** over a *different* version (only the same build was
reinstalled), and the macOS `.app`, which remains unlaunched (§5.8).

---

## 4. Building on macOS (verification only)

macOS is the development host, **not** a shipping target: spec §2 rules out non-Windows capture,
and no macOS ffmpeg is pinned. The `.app` exists so the packaging path can be built and
inspected on the machine the work is done on.

```bash
cargo xtask sidecars fetch --target x86_64-pc-windows-msvc   # optional here
cd apps/desktop
npm ci
npm run tauri -- build            # -> src-tauri/target/release/bundle/macos/replayed.app
```

### 4.1 What was built here

The command above, on this host (exit status 0), with the host pair staged by hand (§4.2).
The transcript below is **verbatim**: it is what the build printed on 2026-09-23, when the
product was still called `localplay`. It is kept as a transcript, not rewritten, because a
build log that has been edited is not evidence.

```console
$ cd apps/desktop && CARGO_HOME="$PWD/../../target/cargo-home" npm run tauri -- build
       Built application at: .../apps/desktop/src-tauri/target/release/localplay
     Running beforeBundleCommand `npm run sidecars:check`

> localplay-desktop@0.1.0 sidecars:check
> node scripts/require-sidecars.mjs

bundling sidecars from .../localplay/binaries: ffmpeg (0.0 MiB), ffprobe (0.0 MiB)
    Bundling localplay.app (.../bundle/macos/localplay.app)
    Finished 1 bundle at:
        .../apps/desktop/src-tauri/target/release/bundle/macos/localplay.app
```

Artifact: **`apps/desktop/src-tauri/target/release/bundle/macos/localplay.app`**, ad-hoc signed
only (§4.4). The bundle is named after `productName`, and that was still `localplay` when
this build ran: the product was renamed to `better-outplayed` on 2026-09-26 (§3.3) and to
`replayed` on 2026-10-20, so a build from the current tree produces `replayed.app`. Its size is decided entirely by what sits in `binaries/` when it is built:

| `binaries/` held | `.app` size | Of which sidecars |
|---|---|---|
| the host pair and `.gitkeep` | 13,064 KiB (12.8 MiB) | 1,004 bytes of placeholders |
| the host pair *and* the fetched Windows pair | **214 MB** | 105,423,872 + 105,221,120 bytes of `.exe` files that macOS never runs |

The second row is what a checkout that has fetched the Windows sidecars produces — including
this one while the verification above was run — and the build log says so: the check prints
`note: binaries/ also holds ffmpeg.exe, ffprobe.exe -- bundled as well`, because the mapping
copies the **whole directory**. The `.exe` files are inert on macOS, but they are 201 MiB of
ballast in an installer, so a real Windows build must not be staged with a macOS pair either.
The listing below is from the first, smaller build:

```console
$ find localplay.app -type f -exec ls -l {} \; | awk '{print $5, $9}'
1135    localplay.app/Contents/Info.plist
13244960 localplay.app/Contents/MacOS/localplay
0       localplay.app/Contents/Resources/binaries/.gitkeep
242     localplay.app/Contents/Resources/binaries/ffmpeg
243     localplay.app/Contents/Resources/binaries/ffprobe
117998  localplay.app/Contents/Resources/icon.icns
```

`Contents/Resources/binaries/{ffmpeg,ffprobe}` is exactly where `discover` looks once the
executable is at `Contents/MacOS/`, and the executable bit survived the copy — relevant if a
real macOS ffmpeg is ever bundled. (The zero-byte `.gitkeep` is copied along with them: it
exists so that `tauri-build` can resolve the directory in a fresh clone, §2.4, and discovery
ignores it.) `Info.plist` carries the metadata from §1
(`CFBundleIdentifier=io.github.fisifla.replayed`, `CFBundleShortVersionString=0.2.0`,
`LSApplicationCategoryType=public.app-category.video`, `NSHumanReadableCopyright=Copyright (c)
2026 FisiFla`).

### 4.2 What those sidecars are — read this before quoting the listing above

They are **placeholders**: two shell scripts (a few hundred bytes) that print
`placeholder ffmpeg: not a real ffmpeg` and exit non-zero. There is no pinned macOS ffmpeg to
put there (`xtask/sidecars.toml` pins a Windows archive, and a Homebrew ffmpeg is GPL, which
replayed does not ship against), and `npm run sidecars:check` says so in the build log —
`ffmpeg (0.0 MiB)`. So the listing above proves **placement**, and nothing about shipping a
working binary. The Windows build embeds the real, checksum-verified pair, because that is the
platform the pipeline exists for.

They were staged by hand, into the same `<repo>/binaries/` the fetch command writes:

```sh
for stem in ffmpeg ffprobe; do
  printf '#!/bin/sh\necho "placeholder %s: not a real ffmpeg" >&2\nexit 1\n' "$stem" > "binaries/$stem"
  chmod +x "binaries/$stem"
done
```

Without them the build stops before writing anything, which is the point of the check:

```console
$ npm run sidecars:check
error: the installer would be built without ffmpeg and ffprobe.
  looked in: .../replayed/binaries
  needs:     ffmpeg, ffprobe  (host platform: darwin)
  ...
$ echo $?
1
```

Delete them again before running `cargo test`: `binaries/` is the development fallback that
`FfmpegBinaries::discover` consults *before* `PATH`, so a placeholder left in place is handed to
the CLI's integration tests, which fail with
`using encoder 'libx264': placeholder ffmpeg: not a real ffmpeg`. That is the fallback working
as designed — loudly — and it is how this note came to be written.

The lookup itself is tested against real bytes: `crates/media`'s
`installed_app_finds_the_sidecars_in_its_own_bundle_resources` builds the same tree and asserts
discovery returns the resource copy, and — with a bundle that actually exists —
`REPLAYED_APP_BUNDLE=…/replayed.app cargo test -p replayed-media --lib
a_real_app_bundle_beats_path` resolves the *real* `.app`'s `Contents/MacOS` through the
production code path and asserts it lands on `Contents/Resources/binaries/ffmpeg`, ahead of the
real ffmpeg on this host's `PATH`.

### 4.3 The `.dmg` could not be built here

`--bundles dmg` was attempted and **failed**, for an environment reason rather than a project
one: Tauri's bundler runs its `bundle_dmg.sh`, which shells out to `hdiutil`, and this session
is not permitted to create disk images.

```console
$ ./bundle_dmg.sh --volname localplay ... localplay_0.1.0_aarch64.dmg localplay.app
Creating disk image...
could not access /Volumes/localplay - Operation not permitted
hdiutil: create failed - Operation not permitted
failed to bundle project: error running bundle_dmg.sh
```

`tauri.macos.conf.json` therefore lists `["app"]`, so the default macOS build succeeds. On a
host where `hdiutil` is permitted, `npm run tauri -- build --bundles app,dmg` should produce
`target/release/bundle/dmg/replayed_0.1.0_aarch64.dmg`; that is **unverified**.
(The attempt above was made on 2026-09-23, before the product was renamed; the volume name
and the artifact name both follow `productName`.)

### 4.4 The macOS bundle is not distributable

```console
$ codesign -dv --verbose=4 replayed.app
Identifier=replayed_desktop-f15df2dfe1af8f63
CodeDirectory ... flags=0x20002(adhoc,linker-signed)
Signature=adhoc
Info.plist=not bound

$ spctl -a -vv replayed.app
replayed.app: code has no resources but signature indicates they must be present
```

The Mach-O binary carries the ad-hoc signature the linker applies, but the bundle was never
signed as a bundle, so the resources added after linking are not sealed and Gatekeeper's
assessment fails. No signing identity or notarisation is configured, and Tauri skipped the
signing step. **The app was not launched to find out what Gatekeeper does with it** — see §7.

---

## 5. What is not done, and blocks a real release

| # | Gap | Consequence | Where it has to be fixed |
|---|---|---|---|
| 5.1 | **No code signing.** Neither the Windows installer nor the macOS app is signed with a code-signing certificate | Windows SmartScreen shows "unknown publisher" and hides the app behind *More info → Run anyway*; macOS Gatekeeper refuses the `.app` outright (§4.4) | a Windows Authenticode certificate for NSIS/MSI, an Apple Developer ID + notarisation for macOS (`bundle.macOS.signingIdentity`, `APPLE_CERTIFICATE`/`APPLE_ID` for notarisation)  §5.9 has the exact keys and variables, verified against the schema. **This is separate from the updater's own signature** (5.3), which is a minisign key the release pipeline holds: the updater verifies what it downloads, but a *manual* download of the installer is still unsigned until this row is closed |
| 5.2 | **No notarisation**, macOS-only, and the same certificate work | an unsigned `.app` cannot be distributed at all, only run locally | the macOS signing flow in Tauri's documentation; needs a paid Apple Developer account  §5.9 has the credential names |
| 5.3 | ~~**No auto-update mechanism.**~~ **Done 2026-10-20, with the caveat below.** Tauri v2's official updater plugin is installed and configured: `bundle.createUpdaterArtifacts` is on, the endpoint is pinned to this project's own `releases/latest/download/latest.json`, `requireSignedVersion` is on, and the artifact is checked against a minisign public key compiled into the build before anything is installed. The update check is **user-initiated only** — there is no startup check, no polling and no background task, so the updater is the one thing in the application that reaches another machine, and only when a person presses the button | a release is now a download rather than a manual hunt, and the artifact is verified against a key the pipeline's secrets hold and the application never carries. **What is not true yet: no update has ever been performed end to end** — no `latest.json` has ever been published, the updater has never run on Windows, and the plugin's own `check()` makes a real HTTPS GET with no seam to mock, so that path is verified by inspection only | the release workflow now stages the signed `.nsis.zip`, its `.sig` and the `latest.json`; what is left is to publish a release and exercise the update against it |
| 5.4 | ~~**The installer may need the network.**~~ *Decided and set 2026-09-26:* `bundle.windows.webviewInstallMode` is now `offlineInstaller`, so the installer embeds the WebView2 runtime and the install touches no network at all. Resolved, and deliberately at the cost of size — the installer carries ~127 MB more on top of the ~201 MiB of ffmpeg sidecars, which principle 1 is worth | — | — |
| 5.5 | **The icon is a placeholder** (§6) | it is a teal play triangle on near-black, generated at scaffold time; shipping it would look unfinished | real artwork, then `npm run tauri -- icon` |
| 5.6 | **No uninstaller verification on Windows** — *done 2026-09-26, see §3.5.* Installed, run, uninstalled and reinstalled silently on the Windows test machine | the install lands where §3.3 said, both sidecars are inside it, the app launches and stays up, the uninstall removes the installation and the shortcut, and **the user's clips and index survive throughout** — the misleading checkbox errs in the safe direction, as §3.2 inferred | still open: an upgrade across *different* versions, and MSI |
| 5.7 | ~~**No CI packaging job.**~~ **Done 2026-10-19.** A `package` job on `macos-latest` fetches the sidecars, builds the frontend, runs `tauri build --bundles app` and asserts the bundle actually carries the pinned ffmpeg sidecar; a `package-windows` job builds and inspects the NSIS installer. **Both now run on pushes to `main`, not only on pull requests.** A separate `.github/workflows/release.yml` turns a pushed `v*` tag into a published GitHub Release carrying the installer and a `SHA256SUMS` file, and the built installer is uploaded as a workflow artifact as well, so a failed publish still leaves it retrievable | bundling is exercised on every push, the bundle configuration — the identifier, `webviewInstallMode`, the resource mapping — is read by something rather than only by hand, and there is now a path from a tag to a download. The release body says plainly that the build is **not code-signed** (5.1) and gives the SmartScreen steps | — |
| 5.8 | **The `.app` has never been launched** — *the installer half is done 2026-09-26*: built on Windows and run in a real session 1, where it stayed up for 30 seconds (§3.5) | the macOS half of this row remains a statement about the artifact as *files* | a login session on macOS, which is out of scope for a headless task (§7) |

Not on this list, because it is done: the sidecars *are* bundled, and discovery *does* look
where they land (§2, §4.1–4.2). The packaging jobs run on every push, and a pushed `v*` tag
publishes a GitHub Release with the installer and its checksum (5.7).

---

## 5.9 The signing path, ready for a certificate

**Nothing below is active.** No certificate exists, so the build is unsigned and §5.1/5.2 stay open.
This is the shape to reach for on the day one does — the keys are taken from the schema `tauri build`
itself validates against (`node_modules/@tauri-apps/cli/config.schema.json`) and the variables from
Tauri's own signing guide, not from memory, because a wrong key here fails silently rather than
loudly.

| | macOS | Windows |
|---|---|---|
| what is needed | a **Developer ID Application** certificate, and a paid Apple Developer account for notarisation | an **Authenticode** certificate |
| the config key | `bundle.macOS.signingIdentity` — the name of the certificate's keychain entry | `bundle.windows.certificateThumbprint` — the certificate, in the Windows store |
| already set here | `bundle.macOS.hardenedRuntime = true` | `bundle.windows.digestAlgorithm = "sha256"`, `timestampUrl` |
| CI variables | `APPLE_CERTIFICATE` (base64 `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`; to notarise, `APPLE_ID` + `APPLE_PASSWORD` (an *app-specific* password) + `APPLE_TEAM_ID`, or the more CI-stable `APPLE_API_ISSUER` / `APPLE_API_KEY` / `APPLE_API_KEY_PATH` | the certificate in the store, selected by thumbprint |
| how to check it worked | `security find-identity -v -p codesigning` to find the identity; then `codesign -dv --verbose=4 <app>` and `spctl -a -vvv <app>`, which must answer *accepted, source=Notarized Developer ID* | `signtool verify /pa /v <installer>`, and SmartScreen stops hiding it behind *unknown publisher* |

**Nothing about the build command changes.** Tauri signs and, given the Apple credentials, notarises
automatically once those variables are in the environment — and since `@tauri-apps/cli@2.0.0-rc.4` it
also infers the identity from `APPLE_CERTIFICATE`, so a CI job needs no `signingIdentity` at all.

**A free half-measure worth knowing about.** `signingIdentity: "-"` signs *ad-hoc*: no Apple account,
no cost, and it prevents the *"app is damaged and can't be opened"* error that a wholly unsigned
`.app` produces on some macOS versions. It is **not** notarisation, so the "unidentified developer"
prompt remains. Worth enabling only if that prompt turns out to be worse than nothing — since it is a
one-line change either way, it is not made here.

The reference for all of it is [macOS Code
Signing](https://v2.tauri.app/distribute/sign/macos/).

---

## 6. The icon

`apps/desktop/src-tauri/icons/` now holds a full desktop set generated from the placeholder
that shipped with the Tauri scaffold:

```bash
cd apps/desktop
npm run tauri -- icon src-tauri/icons/app-icon.png
```

- `app-icon.png` is the **source artwork**: 512×512, RGBA, a teal play triangle inside a teal
  ring on the dark `#0b0c10` background the window already uses. It is a placeholder.
- The command generated `32x32.png`, `64x64.png`, `128x128.png`, `128x128@2x.png`, `icon.icns`
  (macOS), `icon.ico` (Windows, six layers), `icon.png`, and the `Square*Logo`/`StoreLogo` set
  for Microsoft Store packaging. All of them are committed, so a checkout builds without
  running the generator.
- `bundle.icon` lists the five PNG/ICNS/ICO files the two shipping platforms actually need.
  A test in `apps/desktop/src-tauri/src/lib.rs` fails if any listed file is missing, because a
  wrong path there breaks the bundle build.
- The mobile outputs the generator also writes (`android/`, `ios/`) were **not** committed:
  replayed has no mobile target.

**Real artwork is a product decision, not a task.** Whatever replaces `app-icon.png` should be a
square, transparent-cornered PNG of at least 1024×1024 with a dark-friendly silhouette, and a
designer's own choice of mark — the current one is a placeholder nothing else in the product
depends on.

---

## 7. What the build and the tests do not touch

replayed's development host is also a gaming machine, running kernel-level anti-cheat, and its
owner may be mid-game. So, stated explicitly:

- **Building an installer is a compile-and-bundle step.** It runs `cargo build`, the Vite
  frontend build, `npm run sidecars:check`, and (on Windows) NSIS or WiX. It does **not** launch
  the application — `npm run tauri -- build` never does; that is `tauri dev`'s job.
- **A `cargo build` binary is not the shipping UI.** The desktop crate's `tauri.conf.json` carries
  both `frontendDist` (`../dist`) and `devUrl` (`http://localhost:5173`), and a binary produced by
  `cargo build` or `cargo build --release` *in this workspace* loads the dev URL — so its window
  shows `ERR_CONNECTION_REFUSED` unless the Vite server is running. Verified the hard way on the
  Windows box (2026-10-24): both profiles rendered the error page, and UI Automation read
  `ERR_CONNECTION_REFUSED` out of the window. To look at the real UI, either run
  `npm run tauri -- dev` (or `npm run dev` beside the binary) or build through the CLI
  (`npm run tauri -- build`), which is what the installer is made from. `cargo build` alone
  compiles the crate and proves nothing about what the window will show.
- **Installing and running are separate, later, human decisions.** Nothing here requires them,
  and §5.6/5.8 record them as not done.
- **No part of the packaging path captures the screen, enumerates windows, or synthesises
  input.** The only occurrences of the names to grep for in shipping code are two comment
  lines that disclaim their use:

  ```console
  $ grep -rn "keybd_event\|SendInput\|SendKeys" apps crates xtask
  xtask/src/verify.rs:22://! nothing enumerates windows.** There is no `keybd_event`, no `SendInput`, no
  xtask/src/verify.rs:23://! `SendKeys`, no `mouse_event`, and no enumeration call anywhere in this crate. The
  ```

  The same disclaimer is in the Phase 1 runbook, and it holds for the build: `tauri build`
  compiles, signs nothing, copies files, and writes an installer.
- The `.dmg` attempt in §4.3 is the only system tool this work invoked outside the build; the
  environment refused it, nothing was mounted, and nothing was written outside the ignored
  `target/` directories (and `binaries/`).

---

## 8. What this document does not prove

- That an **installed Windows application finds and runs its ffmpeg**. The installer was not
  built (it cannot be, from macOS), installed or run. What is proven is the mechanism on both
  sides: the bundler puts the resources in the directory Tauri's own `resource_dir()`
  documents — the executable's directory on Windows, `Contents/Resources` inside a macOS
  `.app` — and discovery reads that directory, ahead of the checkout fallback and `PATH`, with
  tests that build the layout and one that resolves a real `.app` (§4.2).
- That the **macOS bundle is usable**: its sidecars are placeholders, it is unsigned, and it was
  never launched.
- That the **installer's runtime behaviour** matches §3.1–3.3: those paragraphs are read from
  the NSIS template.
- That the app is **shippable**. It is packagable, unsigned, with a placeholder icon.
