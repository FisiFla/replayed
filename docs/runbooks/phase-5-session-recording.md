# Phase 5 — session recording, the microphone and the game watcher

How to drive the new surface, and what has actually been observed. Written the way
`phase-1-verification.md` is: every claim is tagged with how it is known — *run here*,
*read from the code*, or *not verified*.

Everything below was run on the development host (macOS, ffmpeg on `PATH`, no GPU encoder)
with the synthetic sources, which is the only place this pipeline can be exercised without
Windows. **The microphone and the WASAPI capture path have, however, run on Windows**: the
2026-09-24 session produced a clip with two audio streams and `converting=true` on the
Windows test machine (ledger §10.2), and the audio-queue fix that followed it was verified
there (ledger §10.4). Everything else below was still only ever run on the development host.

---

## 1. The configuration surface

```toml
[recorder]
mode = "buffer"          # or "session" — the CLI overrides it with --mode

[storage.sessions]       # additive: a pre-Phase-5 [storage] table keeps working
sessions_dir = ""        # empty = <app data dir>/sessions
max_total_bytes = 214748364800
max_age_days = 7

[mic]
enabled = false          # off by default; the opt-in is this line

[games]
auto_record = false      # off by default: nothing is watched, no thread exists
poll_ms = 2500
# [[games.watch]] name/exe/signal entries replace the default list
```

`[recorder] mode`, `[mic]` and `[games]` travel into the engine in
`replayed_recorder::RecorderOptions`, not as fields on `RecorderConfig`: the desktop shell
is a separate workspace that builds a `RecorderConfig` field by field, so a new required
field there would break a crate this change does not own. `Recorder::start` is
`start_with_options(cfg, RecorderOptions::default())` — buffer mode, no microphone, nothing
watched — so every existing caller keeps the behaviour it had.

## 2. The CLI recipe (run here, both modes, microphone off and on)

```sh
export CARGO_HOME="$PWD/target/cargo-home"
run() {  # $1 = app dir name, $2 = config file, rest = extra flags
  dir="target/phase5-e2e/$1"; rm -rf "$dir"; mkdir -p "$dir/replayed"
  cp "$2" "$dir/replayed/config.toml"
  LOCALAPPDATA="$PWD/$dir" RUST_LOG=info cargo run -q -p replayed-cli --features test-encoders -- \
    buffer --dev-software-encoder --dev-stub-sources --self-test-clip-after 4 "$@"
}
run buffer  target/phase5-e2e/config-buffer.toml
run session target/phase5-e2e/config-session.toml --mode session
run session-mic target/phase5-e2e/config-session-mic.toml --mode session   # [mic] enabled = true
```

`--dev-stub-sources` is new and feature-gated exactly like `--dev-software-encoder`: it is
the only way the microphone path can run off Windows, because there is no non-Windows
microphone backend and a stub selected implicitly would be worse than an error. It prints
`NOTHING REAL IS CAPTURED` before it starts.

Observed (excerpts of real runs; the first line's closing note read "the scratch cap does not apply" in the build that was run, and the code now says "the RAM cap does not apply"):

```
INFO replayed_recorder: session #1 opened (session): segments in .../sessions/session-1790211072 (the RAM cap does not apply)
INFO replayed_recorder: hotkey pressed: media=4000ms wall=7030ms (drift 3030ms); waiting for post-roll
INFO replayed_recorder::index: indexed clip #1 (media t=2000ms, 3187ms, 1023170 bytes, libx264): .../clips/clip-1790211080.mp4
INFO replayed_recorder: session #1 finalised: .../sessions/session-1790211072.mp4 (8051ms, 2122859 bytes) concatenated from 9 segment(s); 2132516 bytes of temporary segments removed
```

and, with `[mic] enabled = true`:

```
INFO replayed_recorder: capture geometry 1280x720 at 10fps (frame counter starts at 0), plus a microphone track on port Some(59590)
INFO replayed_recorder::session: session .../clips/clip-1790211174.mp4 carries 2 audio tracks (game audio + microphone)
INFO replayed_recorder::session: session .../sessions/session-1790211166.mp4 carries 2 audio tracks (game audio + microphone)
```

`ffprobe` on those files: `video audio audio` for both the clip and the session file
(1 video + 2 audio). The cleanup pass, with 1 KB caps, on the next start:

```
INFO replayed_recorder::index: storage policy: deleted 1 clip(s) and 1 session(s), reclaimed 3330354 bytes;
     the clips directory now holds 0 bytes against its 1000 byte cap, and the session store 0 bytes against its 1000 byte cap
```

## 3. What a full session does, and where it can fail

* Segments go into `sessions/session-<start>/`; no size cap is applied to them (structurally:
  the session's ledger never evicts, and the bounded ring that used to be the one with a cap is
  memory-backed now — see §12 of the verification ledger).
* At stop the segments are concatenated with `-c copy` into `sessions/session-<start>.mp4`
  and the temporary segments are removed. The concatenation is `replayed_media::edit`'s —
  `check_concat_layout` → `write_concat_list` → `concat_lossless_sized`, the same three steps
  `ClipSplicer::splice` takes, so a clip and a session cannot diverge. It was once a private
  copy here, for two measured reasons that no longer exist: the splicer's ffmpeg invocation
  had **no `-map`** (so the concat demuxer's default selection kept one audio stream and the
  microphone track was dropped from every microphone-enabled *clip*), and
  `edit::concat_lossless` bounded the copy with a fixed 60 s timeout, which a multi-GB session
  on a slow volume exceeds — and would then fail again on every retry. Both now live in
  `replayed-media`, where one implementation serves both callers: `-map 0` keeps every
  stream, a **pre-flight stream-layout check** refuses a segment that disagrees with the first
  rather than letting `-map 0` silently truncate a track, and the budget is derived from the
  bytes being copied. The list format, the Windows `\\?\` path normalisation
  (`concat_list_path`, which moved there with it), `-c copy -movflags +faststart` and the
  ffprobe check are all reused.
* **Not enough space**: the concatenation needs transiently about twice the session's bytes.
  There is no pre-flight check; ffmpeg fails, the partial output is removed, **every segment
  is kept**, the `sessions` row is left running (`ended_at IS NULL` — a running session is
  never evicted, so retention cannot delete footage that has no session file), and the error
  is returned by `stop()`. The next start's recovery pass finishes the job once there is
  room. *Run here* for the failure path (`cargo test -p replayed-recorder` — a session whose
  segments are unreadable); *not verified* against a real full disk.
* **A crash mid-session** leaves a running row and a directory of segments. The next start
  concatenates them into the file the session would have written, closes the row with the
  real size, and removes the segments. The segment being written when the process died is
  usually unreadable; recovery drops that truncated tail (it walks back from the newest segment
  to the first one ffprobe can open) and joins the rest, rather than failing for good on it. A
  session that is recovered is owned by the process that finds it: recovery runs only while this
  process holds `recorder.lock`, so a second front-end cannot "recover" a session that is still
  being recorded; a session that cannot be finalised is reported and
  retried at every start; a directory no row names is reported and never deleted.

## 4. Honest limitations

1. **A buffer-mode session row owns no files.** The replay buffer lives in RAM and the scratch
   directory is shared and configurable, so the row names no directory (retention can never
   delete the configured `[buffer] scratch_dir` through it) and its `size_bytes` stays 0, because
   it is a claim about disk and the ring occupies none; the bytes the ring holds go to the log.
   Full-session rows are exact: they have a directory of their own under `sessions/`.
2. **The 4K sustained-rate issue is closed** ([issue #1](https://github.com/FisiFla/replayed/issues/1),
   closed): the zero-copy path sustains 59.6 of 60 fps with no dropped frames at 3.1–4.0% of
   one core (ledger §13, §15). A full session
   records at whatever rate the machine sustains; the media timeline is the frames' arrival
   timestamps, so a `pre_seconds` window is still that many real seconds, but at 4K the
   picture can hold frames that were dropped. It does not make full-session recording
   *unsafe* — it makes the file's smoothness a property of the machine, and the status line's
   `dropped=` is where that shows.

### Three limitations this runbook used to carry, now fixed

Recorded rather than deleted, because a reader who remembers them should find out here that
they are gone — and because the first two were fixed by one change, and the reason matters.

* **Clips taken with the microphone on, in buffer mode, carried only the game audio.** The
  ring's trigger splices clips with `ClipSplicer::splice`, whose concat had no `-map`, so the
  second audio track was dropped; the engine warned about it at start. `-map 0` is now in
  `replayed_media::edit::concat_lossless`, which the splicer calls, so a buffer-mode clip
  keeps both tracks — confirmed on real Windows hardware, where the clip came back with three
  streams (`video, audio, audio`). The warning is gone because the thing it warned about is.
* **A session's audio tracks were unnamed.** `-map 0` kept the tracks, and the encoder *did*
  tag them (`-metadata:s:a:0 title=Game Audio`, which an MP4 stores in the track's `name`
  atom), but **a `-c copy` through the concat demuxer does not carry per-stream metadata** —
  measured with and without `-map_metadata 0`. So every clip and every session file came out
  with anonymous audio tracks: a player showed two tracks both called "Audio". Found by
  probing a real Windows clip (`docs/verification-status.md` §10.2); the concat now re-applies
  the names from `edit::audio_titles`, which is also where the encoder reads them from, so the
  two cannot drift.
* **The session timeline was not populated by the engine.** `sessions.started_at` is the wall
  clock (the retention rules age by it) while `events.at` is the media clock, so `offset_ms`
  was derived as the difference of two unrelated clocks and `events.session_id` stayed NULL.
  Schema **v3** added `sessions.media_epoch_ms` — the media position the session began at —
  and the timeline is now `at - media_epoch_ms`, one clock; the recorder links every event it
  records to its session, and a manual clip records a `bookmark` event. Schema **v4** then
  added `sessions.duration_ms`, the media length a review timeline is drawn against, which is
  what the session view in the desktop shell needed and could not derive.
5. **Windows-only paths are unrun.** `microphone_backend()` (WASAPI communications endpoint)
   has never executed; the game watcher's process enumeration has never executed. The game
   watcher's *driving* half (start/stop recording on presence changes) is run here.
6. **`replayed-recorder` and `replayed-cli` are not cross-checkable for
   `x86_64-pc-windows-msvc`** — they depend on `replayed-store` → bundled rusqlite C, which
   cannot be built for MSVC from macOS. The recorder's new code has no `#[cfg(windows)]` of
   its own (the platform rule lives in `replayed-capture`), so what this costs is small.
