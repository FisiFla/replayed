# Production readiness and Outplayed parity plan

Status: in progress. Written 2026-10-24 against `main` at `a8acaf9`. **Phases A and B are
implemented**, step 8's probe fix is in, and Phase D's non-code items (stray file, CI compiler,
non-goals, changelog) are done — ledgers §38–§41 record what the tests prove and what they do
not. Step 11 (combine and the export presets) is implemented — ledger §42. Phase C's Windows
sweep, the release itself and step 10 (screenshot) are open; **step 12 is implemented** —
hotkey rebinding (§45), autostart toggle, first-run checklist and the open-data-folder button
(§43, §44). What has and has not been verified is recorded in
[`docs/verification-status.md`](../verification-status.md); this plan does not upgrade any claim
there.

## Goal

Take `replayed` from pre-alpha to a releasable Outplayed alternative: fix the stale-index defect, prove the app on real Windows hardware, ship an (initially unsigned) self-updating release, and close the Outplayed parity gaps compatible with the five principles. Code signing is explicitly last.

## Auto full-session already exists

Yes — and it needs no key moments and no game integration. It is already built, just split across two toggles and poorly discoverable:

- `[games] auto_record = true` (Settings → *Automatic recording*): a watcher polls process names every 2.5 s; when a watched game starts, recording starts; when it stops, recording finalises and retention runs (`crates/recorder/src/lib.rs` `spawn_supervisor`). Works for **any** title named in `[[games.watch]]` — defaults are LoL, CS2, Dota 2, R6 Siege, Valorant, Overwatch 2 — by executable name only. Nothing is injected or read.
- `[recorder] save_session = true` (Settings → *Also save the full session*): the whole recording is written to `session-<timestamp>.mp4` from the same single encode while the replay buffer still serves hotkey clips. Crash loses ~1 s.
- The CLI can also run true full-session mode (`[recorder] mode = "session"` / `--mode session`). The desktop window has no mode switch by design (`commands/settings.rs:102`); buffer + `save_session` is its equivalent.

So for Fortnite, Apex, Valorant, etc. you get: launch game → full match recorded automatically → quit game → finished session file, plus hotkey clips and bookmark markers along the way. What you do **not** get for those games is kill/win auto-clips or match-boundary markers. Known limits: recording includes menus/lobbies (starts at process detection, ~2 polls late), the watch list is only editable in `config.toml`, and the watcher is unverified on Windows. Phase B below turns this into a first-class, per-game feature instead of two buried toggles.

## Success Criteria

- Deleting a clip file in Explorer or in the Clips tab leaves no phantom anywhere: library, storage figures, retention planning, sessions list, startup counts all agree.
- Any watched game auto-records a full session with two obvious toggles and an in-app watch-list editor; verified on Windows with a non-integrated game (e.g. R6).
- An unsigned release is published and an installed copy updates itself end to end; tray, hotkey, hide-on-close, autostart, and both integrations are *Verified on Windows hardware* in the ledger.
- Issue #79 closed; 120 fps zero-copy either measured working or no longer offered. Cert/signing is the only remaining release item, deliberately last.

## Indexing bug: root cause (investigated)

Folder-delete keeps rows **by design** (`clips_on_disk`, `commands/clips.rs:45` hides missing files but never prunes; no prune path exists anywhere). Dead rows then (1) accumulate forever, (2) inflate recorder retention planning (`crates/recorder/src/index.rs:251`) and can evict live clips, (3) sessions have no existence filter at all (`commands/sessions.rs:39`). Clips-tab delete is correct in the backend (row-first, `commands/clips.rs:461`); any ghost there is most likely a failed unlink (locked file) — to be verified, not assumed. Fix: explicit user-confirmed reconciliation, never a read side effect.

## Approach

Order: correctness → parity you asked for → Windows proof → unsigned release → remaining parity → signing last. Outplayed auto-highlights cover 11 games via a game-events provider; replayed covers 3 via local APIs. The other 8 get **auto full-session + hotkey + bookmarks** as the honest parity tier. Cloud upload, accounts, injection/memory reading, and effects editing stay non-goals by principle.

## Steps

### Phase A — Fix the stale index (implemented; ledger §38)

1. Failing tests first: folder-deleted clip must not inflate retention; folder-deleted session must not list; prune removes row + tags + events + thumbnails.
2. Retention/cleanup plan over live files only; `Store` prune API + `prune_missing` command; Storage page "N entries point at missing files — Remove from library" (confirm; refuse when `clips_dir` unreachable).
3. Same existence filter for sessions; startup log reports live vs missing. Verify Clips-tab locked-file case; fix only if reproduced.

### Phase B — Auto full-session as a first-class feature (implemented; ledger §39)

4. Settings: one *Automatic recording* section explaining the two tiers in plain words ("Games with highlights: CS2, Dota 2, League. Every other watched game: full session automatically, clips by hotkey"), with `auto_record` + `save_session` together and a warning if auto-record is on but session saving is off.
5. Watch-list editor in the panel (add/remove `[[games.watch]]` entries by exe name, writing `config.toml`); optional per-game `save_session` override only if the global toggle proves too coarse.
6. Session naming/metadata: auto sessions carry the game name (already known to the supervisor) so the Sessions view reads "Valorant — full session" instead of a bare timestamp.

### Phase C — Windows verification sweep (ledger §9; step 7 is manual and open, step 8's probe fix is in — ledger §40)

7. By hand on the Windows box: auto full-session with a non-integrated game (R6 under BattlEye conditions already used); one LoL bot game + one CS2 round; hidden-window `Ctrl+F8`; CLI+app hotkey collision; every tray item; `start_with_system` registry write/removal.
8. Fix #79 (per-frame texture in MFT probe; run its Windows-only test on Windows). Diagnose MFT at 4K120; measure zero-copy 120 fps or cap the picker. `cargo xtask verify` clean; watch 3 clips with audio; start 72-hour soak. Document the ≈1.8–2× bitrate overshoot honestly in USER-GUIDE.

### Phase D — Unsigned release + remaining parity (docs and hygiene done — ledger §41; the release itself and steps 10–12 are open)

9. Publish unsigned `v0.2.0`; install; publish `v0.2.1`; prove signed-minisign self-update end to end (ledger §27). Delete stray `created2.json`; pin exact Rust toolchain. Release notes keep the SmartScreen warning front and centre.
10. Screenshot hotkey (single WGC frame → PNG in library).
11. Lossless combine (multi-select → concatenated clip; refuses mixed codecs/sizes). Local export presets as labelled re-encodes: Discord-size compress, vertical 9:16, GIF.
12. Settings panel: hotkey rebinding with collision detection, autostart toggle. First-run checklist, Health "Copy diagnostics", open-data-folder button.
13. README/USER-GUIDE non-goals: 8 provider-only games' *highlights* (full sessions still work), upload/accounts, effects editor, webcam overlay, software-encoder fallback.

### Phase E — Signing (last)

14. Authenticode cert → signed installer in `release.yml` → remove SmartScreen workaround copy once reputation allows. Nothing else waits on this.

### Phase F — Maintainability (after 1.0)

15. Split the five 3k+ line modules one crate per PR, no behaviour change.

## Validation Plan

- Phase A/B: new failing-then-passing tests; workspace + desktop `cargo test`; manual Explorer-delete → prune → all figures drop; auto-session settings round-trip through `config.toml`.
- Every phase: `cargo fmt --check`, clippy `-D warnings` (both hosts + Windows target), Windows cross-check `--tests`, frontend `npm run check && npm test && npm run build`.
- Phase C/D (highest risk, manual): real-game sessions on the box, `xtask verify` 0-failed report, `v0.2.0 → v0.2.1` self-update on a clean user profile. Ledger rows move only on observed evidence.

## Risks / Open Questions

- Prune must never fire when a network clips directory is merely offline — reachability guard is load-bearing.
- Unsigned releases will hit SmartScreen until Phase E; accepted explicitly.
- `~/work/ev` merge conflicts may hold unique work; excluded until resolved/abandoned. Assumption: `~/work/clean` (synced to `a8acaf9`) is the canonical clone.

## Sources

- https://dev.overwolf.com/ow-native/guides/general-tech/auto-highlights-supported-games
- https://www.overwolf.com/app/overwolf-outplayed
