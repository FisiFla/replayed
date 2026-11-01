# replayed __VERSION__ (__TAG__)

__PRERELEASE_NOTICE__The Windows installer, built from commit `__COMMIT__` and published as the tag
`__TAG__`.

**`__INSTALLER__`** — the NSIS installer. Per-user install, no UAC prompt, into
`%LOCALAPPDATA%\replayed`.

**SHA-256**

```
__SHA256__  __INSTALLER__
```

The same value is attached as `SHA256SUMS`. The `.sig` file and `latest.json` beside it are what the
in-app updater reads; you do not need them to install by hand. Check the download before you run it:

```powershell
Get-FileHash -Algorithm SHA256 .\__INSTALLER__
```

or, with both files in one directory:

```sh
sha256sum -c SHA256SUMS
```

## What this build is

- **Local only, no telemetry.** replayed records on this machine and writes clips
  to your disk. There is no account, no sign-in, no telemetry, no crash reporting,
  no analytics and no cloud upload. The network traffic it creates is to your own
  machine, over loopback, with **one exception**: the update check you ask for in
  Settings → Updates. That asks this project's own GitHub release page once, and
  what it downloads is checked against the signing key this build was made with
  before anything is installed.
- **It carries its own ffmpeg.** The installer embeds the pinned `ffmpeg` and
  `ffprobe` pair, so no system ffmpeg is needed and nothing is installed
  system-wide. They are **LGPL v3 or later** (BtbN's `win64-lgpl` build, without
  the GPL-only encoders), and the licence and the LGPL source offer are installed
  beside them.
- **Supported systems: Windows 10 1903+ or Windows 11, x86_64**, with a hardware
  encoder (NVIDIA NVENC, Intel QuickSync or AMD AMF). The installer embeds the
  WebView2 runtime, so installing it needs no network access.
- **No process injection, hooks or synthesised input.** The executable inside the
  installer was checked before this release was published: its import table names no
  function that injects into, hooks, reads or writes another process, or synthesises
  input (`docs/anti-cheat.md`).

## Windows SmartScreen will say "unknown publisher" — this build is not code-signed

**replayed is not code-signed yet.** No code-signing certificate has been bought,
so Windows SmartScreen cannot vouch for the publisher and hides the installer
behind a warning. The warning is about the missing signature, not about the file
being malicious — but it is also why checking the SHA-256 above is worth doing
before you run it.

To install:

1. Download `__INSTALLER__` and, if you want to, check its SHA-256 against the
   value above.
2. Run it. Windows shows "Windows protected your PC".
3. Click **More info**.
4. Click **Run anyway**.

Every build will show that warning until the project has a code-signing
certificate; the open question is recorded in `docs/packaging.md` §5.1 in the
repository.
