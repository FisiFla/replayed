# Release checklist

Tick every box on the release candidate; attach the filled copy to the GitHub Release.

## Automated (must be green on the tagged commit)
- [ ] CI: macOS tests, Windows tests, supply-chain (cargo-deny, npm audit, gitleaks, API guard)
- [ ] Installer builds; contains `binaries\ffmpeg.exe` / `ffprobe.exe` (LGPL build, no `--enable-gpl`)
- [ ] `release.yml` ran end to end on a **prerelease tag** first (`vX.Y.Z-rc.N`): gate, build, sign, publish; the `release` environment has required reviewers and holds the two signing secrets (`docs/packaging.md` §5.10)
- [ ] The built executable passes the binary guard (`scripts/check-forbidden-apis.sh --binary <exe>`; the build job runs it)

## Windows hardware (human, interactive session — see `docs/anti-cheat.md` for the Vanguard operating rules)
- [ ] `cargo xtask verify` report attached; first row `capture_started` is real 3840x2160 (or native) capture
- [ ] Install from the NSIS artifact; first run; tray icon; hide-on-close
- [ ] Clip hotkey pressed by a person -> clip appears, plays, scrubs, trims, deletes
- [ ] Settings: folder picker, fps, mic toggle, start-with-system (check `HKCU\\...\\Run`, reboot)
- [ ] CSP: no console violations in the review window; thumbnails and video load
- [ ] Upgrade over the previous release keeps clips, settings, Run key
- [ ] Update from a published prerelease through *Settings -> Updates*, with a recording running: the panel asks, declining changes nothing, agreeing finalises the session before the installer starts
- [ ] Quit from the tray while recording: the last segment is finalised and the clip or session plays (the exit flush has not been checked on a real desktop)
- [ ] Log in with *Start with Windows* on: replayed starts in the tray, window hidden (`--background`), hotkey active
- [ ] `replayed-cli`: Ctrl+C in a real console finishes the clip in flight and exits 0 (`docs/runbooks/phase-1-verification.md`, manual step)
- [ ] Resize and close the captured window while recording: the buffer restarts at the new size, a closed window ends the recording with "The recording ended: ...", and a minimised one shows "No new frames for N s" (covered by tests with a fake capture only)
- [ ] Uninstall behaves as the installer text says
- [ ] 72 h soak: flat RSS, bounded `logs\\` directory, no console windows
- [ ] CS2 / Dota 2 GSI and League Live Client events against a real game (read-only APIs only)
- [ ] Anti-cheat run-alongside check recorded in docs/anti-cheat.md

## Housekeeping
- [ ] CHANGELOG has a `## [X.Y.Z]` section, and the version agrees in all six places (`python3 -I scripts/check-release-version.py --tag vX.Y.Z`)
- [ ] docs match reality (README status, verification ledger)
