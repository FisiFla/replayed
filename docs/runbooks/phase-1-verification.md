# Phase 1 verification runbook

**Status: partly executed.** This document tells a human how to decide, honestly, whether
Phase 1 works. Some Windows runs have happened — the capture→NVENC encode→clip-splice
path ran on a 4K machine and the WGC/WASAPI backends did execute (see the ledger
`docs/verification-status.md`), so it is no longer true that nothing has run on Windows —
but **large parts of it still have not**, and the eight criteria below have **not** been
re-run end-to-end on the current build. A green `cargo test` on macOS exercises the
synthetic stubs and proves none of the eight criteria — see [Known gaps](#known-gaps).

**`cargo xtask verify` automates most of this document** and writes one Markdown report
with every check, its threshold, its measured value and the raw output behind it. Start
there ([Run the harness first](#run-the-harness-first-cargo-xtask-verify)); the per
criterion sections below are what it cannot do, plus how to reproduce any of its rows by
hand.

You need:

- the repository checked out on a **Windows** machine,
- a Rust toolchain (`cargo`) on `PATH`,
- a GPU that offers a hardware H.264/HEVC encoder (NVIDIA **NVENC**, Intel **Quick
  Sync**, or AMD **AMF**), and
- `ffmpeg` and `ffprobe` reachable. The simplest setup is both on `PATH`. Otherwise
  put them in a `binaries\` directory next to the built executable —
  `target\release\binaries\` for a `cargo build` checkout — which is the first place the
  app looks, ahead of an installed copy's own resources, a checkout root's `binaries\`,
  and `PATH`, in that order ([`docs/packaging.md`](../packaging.md) §2.3).

You do **not** need to have read the design spec. Where a result depends on your
machine (resolution, fps, CPU %, RSS), this runbook says what to measure and the
threshold to compare against; it never hands you a number to expect.

The eight criteria are the Phase 1 success criteria. Run them in order. Every one gives
(1) the exact command, (2) the exact observation that means **PASS**, and (3) what
**FAILURE** looks like — so "it did not work" is distinguishable from "it worked and I
misread it".

Conventions used below:

- Log lines are timestamped and printed to **stdout** by default.
- Lines shown as `key=value` are structured tracing fields (e.g.
  `width=1920 height=1080`); the values are the numbers you record.
- `%LOCALAPPDATA%\replayed\` is the app data directory: `config.toml`, `scratch\`, and
  `clips\` live there unless the config overrides `scratch_dir` / `clips_dir`.

---

## Run the harness first (`cargo xtask verify`)

There is now a command that does most of this runbook for you and writes down what it saw:

```powershell
cargo xtask verify
```

It builds the release CLI, probes the encoders, starts the buffer with a **bounded,
documented configuration** (printed in the report), takes one clip through the CLI's own
verification trigger (`buffer --self-test-clip-after` — see criterion 3), probes that clip
with ffprobe (duration, streams, codec, A/V drift, audio level), reads the process's own
CPU time and working set against criterion 6's thresholds, checks the ring's RAM cap and that
eviction happened, confirms the media-vs-wall-clock ratio from the status line's `span=`
(not from file mtimes), exercises criterion 7 by forcing a vendor this machine cannot run,
and writes everything — every check, its expected value, its measured value, and the raw
command output — to `target\verify\report.md`. It exits non-zero if any check failed.

Useful flags: `--seconds N` (how much media to buffer before the clip), `--report PATH`,
`--clean-work` (delete the sandbox afterwards).

**The harness is not a replacement for this document.** It cannot check the GUI window, a
real `Ctrl+F8` keypress, anything needing a real game, or the live WGC-vs-WASAPI clock
divergence; it marks everything it could not perform as *not performed*, with the reason,
and it says so in its own report. The criteria below remain the source of truth for a human
who wants to look with their own eyes — and the runbook text still applies, because the
self-test trigger emits the **same log lines** a keypress does.

**If you are driving the Windows test machine from a remote shell**, a capture session needs a desktop, which a remote-shell session does not have. Use the wrapper, which runs the harness in the logged-on user's session
through a one-shot scheduled task and cleans the task up afterwards:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\verify-in-interactive-session.ps1
```

---

## Before you start

### 0.1 Build a release binary

```powershell
cargo build --release --bin replayed-cli
```

Release matters for criterion 6 — a debug build's CPU and memory are not representative
of steady state. Use `--release` for every run below.

### 0.2 Find out which hardware encoders this machine has

```powershell
cargo xtask probe
```

If your checkout does not have the `xtask` cargo alias, the equivalent is:

```powershell
cargo run -p xtask -- probe
```

This runs `ffmpeg -encoders` **and a 1-frame smoke test** for each candidate id —
`h264_nvenc`, `hevc_nvenc`, `h264_qsv`, `hevc_qsv`, `h264_amf`, `hevc_amf` — and prints one
line per encoder in one of three states:

```
h264_nvenc   advertised, WORKS
h264_amf     advertised, FAILS: [h264_amf @ …] DLL amfrt64.dll failed to open | …
hevc_qsv     not advertised
```

- **`advertised, WORKS`** — ffmpeg lists the encoder *and* this machine encoded a 320x240
  frame with it. This is the only state that is a usable vendor value.
- **`advertised, FAILS: …`** — ffmpeg lists the encoder but it cannot open a session on
  this machine (no vendor runtime, wrong driver). The text after `FAILS:` is ffmpeg's own
  reason. **Do not put this vendor in `encode.vendor`.**
- **`not advertised`** — the encoder is not in this ffmpeg build at all.
- Note every vendor whose encoder is `advertised, WORKS`. That is the value you put in
  `encode.vendor` in step 0.3. On an AMD-only machine expect the `*_amf` lines
  `advertised, WORKS` and the NVENC/QSV lines `not advertised`.
- `probe` locates ffmpeg the way the CLI does — the sidecar directory first, then `PATH`.
- `advertised` alone is necessary but **not** sufficient: ffmpeg lists every encoder it
  was *built* with, including ones whose vendor runtime is not installed. A Windows box
  with no AMD hardware still lists `h264_amf`. That is why the smoke test runs: it is the
  encoder actually encoding a frame, and nothing else, that proves the vendor works here.
- The smoke test is synthetic: it encodes one generated 320x240 frame to a discarded
  output and does **not** capture the screen or inject input. (320x240, not something
  smaller, because the vendors have driver-level minimum frame sizes — NVENC refuses
  anything under 145x145 — and a probe below one would report a working encoder as broken.)

### 0.3 Configure

If `%LOCALAPPDATA%\replayed\config.toml` does not exist, the CLI runs with the built-in
example defaults (`pre_seconds = 30`, `post_seconds = 5`, `fps = 60`,
`segment_time = 1`, `ram_cap_bytes = 268435456`, `vendor = "auto"`). To change
anything, copy `config.example.toml` there and edit it. At minimum:

- `encode.vendor` — a vendor step 0.2 reported **`advertised, WORKS`**, or `"auto"`.
- `encode.fps`, `buffer.pre_seconds`, `buffer.post_seconds`, `buffer.segment_time`,
  `buffer.ram_cap_bytes` — as you want them for the runs below.

`replayed` never reads configuration from environment variables.

### 0.4 Turn on the debug log

Criterion 2 is read from a line emitted only at `debug` level:

```powershell
$env:RUST_LOG = "debug"            # PowerShell
# or, in cmd.exe:  set RUST_LOG=debug
```

For the ledger line alone (much less noise) use `$env:RUST_LOG = "replayed_recorder=debug"`.
The status line is emitted by the recording engine (`crates/recorder`), which both the CLI
and the desktop shell drive, so its tracing target is that crate's — the CLI's own lines
(`clip index …`, `buffering …`) keep `replayed_cli`.

To keep a transcript, run everything through a file. For example:

```powershell
cargo run --release -p replayed-cli -- buffer 2>&1 | Tee-Object -FilePath run.log
```

---

## Criterion 1 — Captures the primary monitor via WGC at the configured fps

**Command**

```powershell
$env:RUST_LOG = "debug"
cargo run --release -p replayed-cli -- buffer
```

Leave it running. This is also the process you use for criteria 2 and 3–6.

**Expected observation (PASS)**

At startup, on stdout:

- `capturing the primary monitor display="<name>"` (the WGC capture item's display
  name).
- `WGC capture started on the primary monitor width=<W> height=<H>` — `<W>x<H>` must
  equal your primary monitor's native resolution (Settings → System → Display).
- `rawvideo -s=<W>x<H> (capture native), encode output <W>x<H>` — the raw-video pipe
  size must equal that same `<W>x<H>`.
- `encoding with <encoder_name>` — e.g. `encoding with h264_nvenc`.
- `encode rate: <measured>fps sustainable at <W>x<H> with <encoder_name> (…)` — the startup
  throughput probe (`encode.adapt_fps`, spec §10.1). Two shapes:
  - *not reduced*: `…; encode.fps = <fps> is within that, so capture runs at the configured
    <fps>fps`. Nothing else changes.
  - *reduced* (`WARN`): `encode.fps = <fps> is not achievable at <W>x<H> on this machine:
    <measured>fps sustainable … Capturing at <effective>fps instead …`. The pipeline paces
    to `<effective>` and the encoder child is told the same number, so no readback is spent
    on frames the encoder will drop — but criterion 1 *as written* (the configured fps) is
    not met at this resolution, and the ledger must record it as such. The levers are
    `encode.output_size` and `encode.fps`; `encode.adapt_fps = false` declares the
    configured rate anyway and accepts the dropped frames. **Neither shape says anything
    about the media timeline**: media time is the frames' arrival timestamps and tracks the
    wall clock either way (ledger §1).
- `buffering <pre>s pre / <post>s post at <fps>fps; press Ctrl+F8 to clip` — `<fps>` is
  the rate the pipeline is running at: your configured `encode.fps` unless the probe above
  reduced it.

Nothing is written to the disk while the buffer runs. `seg-000000.mp4`,
`seg-000001.mp4`, … do **not** appear in `%LOCALAPPDATA%\replayed\scratch\` — the ring
holds its footage in RAM and writes nothing until a clip is saved. (A full *session*
recording does write segments, into `sessions\`.)

With `RUST_LOG=debug`, every ~200 ms you get a line:

```
frames=<n> segments=<n> bytes=<b> span=<ms>ms dropped=<n> dropped_audio=<n> \
skipped=<n> fps=<achieved>/<effective> configured=<configured>
```

`frames=` is a literal counter of the video frames actually submitted to the encoder
since startup — not a proxy. At startup the CLI also prints a one-line geometry summary,
`capture geometry <W>x<H> at <fps>fps (frame counter starts at 0)`, so the resolution
being captured is visible up front.

`fps=<achieved>/<effective>` is the achieved rate — the frames that actually reached the
encoder per second, measured over the last second of the run — against the rate the
pipeline is running at (a fresh run reads `0.0/<effective>` until its first second has been
measured). It is the rate the capture path is *delivering* to the encoder, and with
`dropped=` it says how many captured moments are being held in the picture instead of shown.
It does **not** decide whether media time tracks real time — that is the frames' arrival
timestamps, and it holds at any achieved rate (ledger §1). `configured=` is what `encode.fps`
asked for, and differs from `<effective>` exactly when the startup probe reduced the rate.

`dropped=` (video frames) and `dropped_audio=` (10 ms audio blocks) are the encoder's own
count of payloads it had to discard because its queue was full during that run: a live
capture cannot be slowed down, so ffmpeg falling behind costs frames rather than memory.
Zero is the expected value on a machine that keeps up; **a non-zero and rising
`dropped=` means the encoder is the bottleneck** — the recording has holes in it, and the
run should not be reported as clean. The queue is bounded (4 video frames, 300 audio
blocks), so these counters can never be traded for unbounded RAM.

`skipped=` is the other half of `frames=`: the frames the capture backend offered that the
pacer threw away **without reading their pixels back** (`CaptureBackend::discard_pending`).
A display that delivers more frames per second than the rate the pipeline is running at is
the normal case — measured on the reference machine at 53-75fps delivered against a configured
30 — so `skipped=` is expected to be a substantial fraction of `frames=` there, close to
`(source rate / fps - 1) × frames=`. On a high-refresh display, **`skipped= 0` means every
surplus frame was read back before being dropped, which is the CPU cost that was measured
at 50.8% of a core**; that is a failure of this criterion even though the recording itself
is fine. `frames=` + `skipped=` is the source's own delivery rate.

If the encoder's queue is dropping frames *after* the pipeline was paced to the rate it
measured — a load that changed since startup — the CLI raises a `WARN` (at most one per
10 s): `the encoder cannot sustain the <effective>fps it was told: only <achieved> frames per
second are reaching it …`. It names both rates and states the consequence: **media time will
not track real time, so a `pre_seconds` clip will correspond to more real seconds than
configured.** A run that prints this warning must not be reported as passing criterion 1 or
2: the machine cannot encode that many frames per second at that resolution, and the remedy
is a lower `encode.fps` or a smaller `encode.output_size`.

Segment numbering **continues across runs**. A run started on a scratch directory that
already holds `seg-000000.mp4`…`seg-000018.mp4` writes `seg-000019.mp4` onward (the CLI
logs `segment numbering continues at <n>`) instead of restarting at `seg-000000.mp4` and
overwriting files the adopted ledger still names. The previous run's segments are still
adopted, though, so a clip taken shortly after startup can include footage from before the
restart — delete or rename the scratch directory when you want a run that cannot see any
earlier material.

Over a run of `T` seconds:

- `frames=` should grow by roughly `T × <fps>` (as fast as the capture-and-encode path
  sustains; if it lags the encoder is the bottleneck, not the counter).
- `span=` should grow by roughly `1000` ms per second of wall clock.
- `segments=` should advance by one every `segment_time` seconds.

That is the rate check: `frames` counts the frames, `span` tracks the capture timeline,
and the `WGC … width=<W> height=<H>` and `rawvideo -s=<W>x<H>` lines confirm the
resolution and the pipe size.

**FAILURE looks like**

- Startup aborts with `Error: …` before any capture line — e.g. WGC could not start, or
  programmatic-capture access was denied. No `WGC capture started` line.
- `WGC capture started on the primary monitor` appears but `width=`/`height=` is **not**
  your monitor's resolution (e.g. `1280x720` would mean the synthetic stub ran, i.e. the
  real backend was not selected on this build).
- After 30 s, `frames=` is still `0`, `span=` is still `0` and `segments=` is still `0` —
  frames are not flowing. (No `seg-*.mp4` files appear either, but in buffer mode they
  never do; that is not the signal.)
- `frames=` grows materially slower than `T × <fps>` (e.g. well under half of `<fps>`),
  or `span=` grows materially slower than 1000 ms per wall-clock second — capture is not
  keeping up at the configured fps (or the pipeline is stalling).
- `skipped=` stays at `0` on a display whose refresh rate is far above `encode.fps`: every
  surplus frame is being read back before it is dropped, which is the measured CPU burn
  (50.8% of a core at 4K) with nothing to show for it.
- `fps=` settles below the configured rate, or the `the encoder cannot sustain the
  configured <fps>fps` warning appears: the machine cannot encode that rate at this
  resolution. Media time then runs slower than the wall clock, so a configured `pre_seconds`
  corresponds to more real seconds than asked for — lower `encode.fps` or `encode.output_size`.

**Do not misread:** if `segments=` has stopped climbing but `span` still grows, that is
normal ring behaviour once the RAM budget is reached (oldest footage is evicted) — not
a criterion-1 failure.

---

## Criterion 2 — The ring's RAM budget is not exceeded

**Command**

Run the same `buffer` process (with `RUST_LOG=debug`) for a **5-minute soak**, and read
the `bytes=` field from the debug ledger line.

**The cap is a RAM budget, not a disk cap.** `buffer.scratch_cap_bytes` was removed when
the ring moved into RAM; the key that bounds it now is `buffer.ram_cap_bytes` (256 MiB by
default). In buffer mode the ring holds its footage **in RAM** and writes **nothing to the
disk at all** — there is no `scratch\` directory of `seg-*.mp4` files to measure. (A full
*session* recording does write segments, into `sessions\`.)

To make eviction actually happen inside 5 minutes, set `buffer.ram_cap_bytes` lower
than the soak will produce but comfortably higher than `(pre_seconds + post_seconds)`
worth of footage (otherwise the pre-roll window is truncated). Estimate one second of
video as `bitrate_kbps / 8` kilobytes plus `audio.bitrate_kbps / 8` kilobytes of audio,
then multiply by 300 s. For example, at `bitrate_kbps = 20000` that is roughly 2.5 MB/s,
so 5 minutes is roughly 750 MB — set the budget below that. Make sure the budget is also
larger than a **single** segment, because the ring always retains at least one segment
and the CLI aborts if it cannot satisfy the cap (see below).

**Expected observation (PASS)**

- The debug line's `bytes=` rises, then plateaus once the ring is full, and **never
  exceeds** the configured `ram_cap_bytes` for the whole soak.
- The process keeps running — it never aborts.
- No `scratch\` directory of `seg-*.mp4` files appears at all: buffer mode writes
  nothing to the disk while it buffers.

**Measure the LEDGER, not a directory.** `bytes=` is the number of bytes the ring holds
in RAM. There is nothing on disk to compare it against: `du` / `dir /s` over the data
directory will find the clip you asked for and nothing else.

**FAILURE looks like**

- The process aborts with
  `Error: the in-memory ring is over its RAM budget: <held> bytes held against <cap>`.
  That is the code's own assertion on the ring, so seeing it is unambiguous.
- The `bytes=` field ever exceeds the budget while the process keeps running — enforcement
  is broken.
- A `scratch\` directory of `seg-*.mp4` files appears during a buffer run — that is
  session behaviour leaking into buffer mode.

**Do not misread:** if you set the budget smaller than one segment, the ring can never
satisfy it and the run aborts by design; that is a misconfiguration, not a criterion-2
failure.

---

## Criterion 3 — a trigger writes `clip-*.mp4` within 2 s of post-roll completion

**Command (recommended: the self-test trigger)**

```powershell
cargo run --release -p replayed-cli -- buffer --self-test-clip-after 45
```

`--self-test-clip-after <SECONDS>` is a **verification aid, not a feature**. Once the ring
holds `SECONDS` of media — and never before it holds a full pre-roll plus post-roll — it
calls the *same* media-time trigger the hotkey calls (`Recorder::clip_now`), exactly once,
logs the same lines, and stops. It sends, simulates and injects **no** keyboard or mouse
input, and it does not enumerate windows. `replayed-cli --help` says so too.

**Why not just press the key from a script.** It used to be done that way, with
`keybd_event`. This machine runs kernel-level anti-cheat, and synthetic input injection is
precisely the behaviour that anti-cheat is built to flag; a verification run that gets the
machine banned is not verification. The hotkey is still the shipping trigger — it is
installed in this mode too, and nothing about it changed — but the *scripted* path to the
clip no longer goes anywhere near the input stack.

**Command (the manual alternative: press the key yourself)**

With the buffer running, start a stopwatch, press **Ctrl+F8** at a known instant, and
watch the log. The trigger lines are identical to the self-test's, so everything under
"Expected observation" below applies to both.

**Expected observation (PASS)**

Immediately after the trigger:

```
hotkey pressed: media=<m>ms wall=<w>ms (drift <d>ms); waiting for post-roll
```

(`media=` is the trigger in media time, `wall=` in wall-clock time, and `drift` is the
difference between them; the post-roll wait is bounded from the wall-clock trigger.)

Then, once the post-roll has elapsed (about `post_seconds` later):

```
wrote <path> (<duration_ms>ms, <size_bytes> bytes, encoder=<name>)
```

- `<path>` ends with `clip-<unix_seconds>.mp4` and the file exists in
  `%LOCALAPPDATA%\replayed\clips\`.
- The **wall-clock delta from the trigger to the `wrote` line** is **less than
  `post_seconds + 2 s`**. The trigger→write path includes waiting out the rest of the
  post-roll, up to 50 ms of polling granularity, and the splice; the 2 s budget is on top
  of `post_seconds`.

To measure precisely, subtract the timestamp the log formatter prints at the start of the
`wrote` line from the timestamp of the `hotkey pressed:` line (with the self-test trigger
those two timestamps are the whole measurement; with a real press, use your stopwatch).
`cargo xtask verify` does exactly this subtraction and reports it as
`clip_trigger_to_write`.

**FAILURE looks like**

- The `wrote` line never appears after the trigger — e.g.
  `Error: timed out waiting for post-roll (span=<ms> need=<ms>)` (the post-roll never
  completed).
- `wrote` appears but the delta is **≥ `post_seconds + 2 s`**.
- No new `clip-*.mp4` appears on disk, or the file is not named `clip-*.mp4`.

**Do not misread:** the budget is `post_seconds + 2 s`, not `2 s`. With `post_seconds = 5`
a delta of 6.8 s is a **PASS**. Count the delta from the trigger line to the `wrote` line
only. And a self-test run is not a weaker check than a keypress: the trigger it calls is
the same call, on the same media-time path, with the same reason (`Manual`) — what a real
keypress adds is proof that the global hotkey itself fires, and that is worth doing once
by hand (press it and watch for the same `hotkey pressed:` line).

### Manual step: quit the CLI with Ctrl+C (not automated, not yet run on Windows)

`cargo xtask verify` drives the CLI with its own self-test trigger and ends it through the
engine, so it never presses Ctrl+C. The console handler (`apps/replayed-cli/src/shutdown.rs`) is
therefore covered by unit tests and a test that raises the shutdown flag directly, and **not** by a
run in which a person quits a real console. Do this once on a Windows machine, in a real console
(`cmd.exe` or PowerShell, not a remote shell with no console):

1. Start `replayed-cli buffer` (add `[recorder] mode = "session"`, or `--mode session`, for the
   second half of this step) and let it run past one full pre-roll.
2. Press **Ctrl+F8**, then press **Ctrl+C** while the post-roll is still running.
3. **PASS:** the log says `shutting down: stopping the recorder (press Ctrl+C again to quit at
   once)`; the clip you triggered is still written and indexed (the clip in flight is finished, not
   abandoned); with `--mode session` the `session-<timestamp>.mp4` file appears under `sessions\`
   and the segment folder is gone; and `echo %ERRORLEVEL%` (or `$LASTEXITCODE`) prints `0`.
4. Start it again and press **Ctrl+C twice**. **PASS:** the second press ends the process at
   once, without waiting for the recorder.
5. Start it again and **close the console window** (the close button). **PASS:** the same files
   as in step 3 appear. The system gives the handler a few seconds (the code waits at most 4.5 s),
   so a very long session may be cut off here; that is the same outcome as before the handler
   existed and not a failure of this step, but note it.

Record what you saw in the ledger (`docs/verification-status.md`, the line marked for it).

---

## Criterion 4 — Clip duration matches `pre_seconds + post_seconds` (±0.5 s)

**Command**

```powershell
ffprobe -v error -show_format -of json "%LOCALAPPDATA%\replayed\clips\clip-<ts>.mp4"
```

Read `format.duration` (seconds).

**Expected observation (PASS)**

`format.duration` ≈ `pre_seconds + post_seconds`, within **±0.5 s**. With the defaults
that is ≈ 35.0 s.

Clips are whole segments concatenated with a stream copy (no re-encode, no leading trim),
and cuts snap to segment/keyframe boundaries (`segment_time`). So the duration is
quantised to `segment_time`; the ±0.5 s tolerance covers that step plus container
rounding. With `segment_time = 1` the achievable steps are 1 s. If you raise
`segment_time`, expect coarser quantisation — either lower it back to 1 s for this check,
or widen your expectation accordingly.

**FAILURE looks like**

- `format.duration` ≈ `pre_seconds` only, or ≈ `post_seconds` only — one side of the
  window is missing.
- `format.duration` ≈ 0, or ffprobe errors, or there is no `format.duration`.
- With `segment_time = 1`, the duration is off by more than 0.5 s (e.g. 33 s or 37 s when
  expecting 35).

**Do not misread:** `format.duration` is a decimal string of seconds; `34.8` is within
tolerance of `35.0`. Do not confuse `format.duration` with a per-`stream` `duration` field.

---

## Criterion 5 — The clip is a stream copy, not a re-encode

**Command**

```powershell
ffprobe -v error -show_streams -show_format -of json "%LOCALAPPDATA%\replayed\clips\clip-<ts>.mp4"
```

Compare against the encoder the CLI logged: the `encoder=<name>` field on the `wrote …`
line, and the startup `encoding with <name>` line.

**Expected observation (PASS)**

- The clip's **video** stream `codec_name` matches the codec *family* of the logged
  encoder:

  | Logged encoder | Expected `codec_name` |
  |---|---|
  | `h264_nvenc`, `h264_qsv`, `h264_amf` | `h264` |
  | `hevc_nvenc`, `hevc_qsv`, `hevc_amf` | `hevc` |

  ffprobe reports the codec, not the encoder implementation, so it never prints the
  `_nvenc`/`_qsv`/`_amf` suffix — matching on the family is correct.
- **Extraction completed near-instantly.** The gap between the post-roll finishing
  (keypress time + `post_seconds`) and the `wrote` line is a small fraction of a second —
  materially shorter than the clip's own duration. A re-encode of a 35 s clip would take
  on the order of tens of seconds (CPU re-encoding runs far slower than real time), which
  is the whole point of stream copy.

Why this proves it: the clip is built by concatenating whole segments with `-c copy` — no
decode and no encode runs on it. The two independent pieces of evidence are (a) the clip's
codec family equals the hardware encoder that produced the segments, and (b) the
extraction is far faster than the clip's duration. A re-encode would show the time scaling
with duration instead.

**FAILURE looks like**

- Codec mismatch — e.g. the clip is `hevc` while the log says `encoder=h264_nvenc`; or the
  clip's codec is `h264` but the log shows a software encoder name (which would itself mean
  the "no CPU fallback" rule was violated).
- Extraction takes a significant fraction of the clip's duration (e.g. more than a few
  seconds for a 35 s clip) — something decoded and re-encoded.
- The `wrote … encoder=` value does not match the startup `encoding with …` value.

**Do not misread:** ffprobe will never literally print `h264_nvenc`; `codec_name: h264` is
the expected, correct match. The `_nvenc`/`_qsv`/`_amf` suffix appears only in the CLI log.

---

## Criterion 6 — Steady-state CPU under 5% of one core and RSS under 400 MB

**Command** (PowerShell, while the buffer is running and has settled)

```powershell
$x1 = Get-Process replayed-cli
$cpu1 = $x1.TotalProcessorTime.TotalSeconds
$t1 = Get-Date
Start-Sleep -Seconds 60
$x2 = Get-Process replayed-cli
$cpu2 = $x2.TotalProcessorTime.TotalSeconds

"cores used : {0}" -f [math]::Round(($cpu2-$cpu1)/((Get-Date)-$t1).TotalSeconds, 3)
"RSS (MB)   : {0}" -f [math]::Round($x2.WorkingSet64/1MB, 1)
```

**Expected observation (PASS)**

- `cores used` **< 0.05** — i.e. under 5% of one core.
- `RSS (MB)` **< 400**.

Measured at steady state: after the ring is full and startup has passed (sample after the
first 30–60 s), not during startup.

**How the numbers map to the thresholds**

- "5% of one core" = `0.05` of a single logical core. `ΔCPU/Δt` is cores used, so
  `< 0.05` passes.
- RSS = `WorkingSet64` (bytes); the threshold is 400 MB = `419430400` bytes.
- Task Manager's **CPU** column is normalised across all cores — multiply its reading by
  your logical-processor count to get "% of one core". `Get-Counter`'s
  `\Process(replayed-cli)\% Processor Time` reports `100` = one core, so the threshold
  there is `< 5`. Task Manager's **Memory (active private working set)** column is the
  figure to compare against 400 MB.

**FAILURE looks like**

- `cores used` sustained at or above `0.05`, or `RSS (MB)` sustained at or above 400.
- CPU spiking continuously (not just at startup) — suggests CPU re-encoding or a
  busy-wait.

**Do not misread:** a **debug** build's CPU/RSS are not representative and may fail while
release passes — always sample the `--release` binary. Startup spikes (ffmpeg spawn,
encoder init, first probes) are not the steady-state figure; sample later.

**The 5% is a quiet-screen figure.** The cost is per frame the display presents, and Windows
Graphics Capture offers a frame whenever the screen changes, so the same build reads several times
higher while something animates. `cargo xtask verify` therefore judges the flat 5% only when the
display offered 15 frames/s or fewer, and reports a busier screen as *not performed*, with the
offered rate, rather than as a failure or a pass (ledger §48, §56). Close animating windows before
reading this number by hand, too. The RSS figure of the harness is taken at its 16 MiB ring; a
default `ram_cap_bytes` of 256 MiB holds more.

---

## Criterion 7 — Non-zero exit with an actionable message when no hardware encoder exists

**Command**

Force `vendor` to an encoder this machine does **not** have, then run. Two cases belong to
this criterion, and both must be checked:

1. a vendor step 0.2 reported `not advertised` (e.g. `vendor = "nvenc"` on an AMD-only machine);
2. a vendor step 0.2 reported **`advertised, FAILS: …`** — ffmpeg lists the encoder, but
   this machine cannot run it. This is the case that used to slip through, because the
   encoder list was the only check.

```powershell
# in %LOCALAPPDATA%\replayed\config.toml set:
#   vendor = "nvenc"
cargo run --release -p replayed-cli -- buffer
```

**Expected observation (PASS)**

- The process starts, cannot use the requested encoder, prints an error, and exits
  **non-zero**. Confirm the exit code: `$LASTEXITCODE` (PowerShell) or `%ERRORLEVEL%`
  (cmd) is not `0`.
- The error **names the encoder that was tried and why it failed**, e.g. for a vendor in
  the `not advertised` state:

  ```
  Error: no usable hardware encoder. Tried in order — h264_nvenc: not advertised by this
  ffmpeg build. An encoder appearing in `ffmpeg -encoders` does not mean this machine can
  run it: that list names every encoder ffmpeg was built with, including ones whose
  vendor runtime is not installed. Install the GPU vendor runtime (NVIDIA driver / Intel
  graphics driver / AMD Adrenalin), or set encode.vendor to a vendor this machine has.
  replayed will not fall back to CPU encoding because it would cost game performance.
  ```

  and for the advertised-but-unusable case, where the encoder's own failure is included:

  ```
  Error: no usable hardware encoder. Tried in order — h264_amf: advertised, but it could
  not encode a single frame: [h264_amf @ 0x…] DLL amfrt64.dll failed to open | Error
  while opening encoder for output stream #0:0 — … . Install the GPU vendor runtime …
  replayed will not fall back to CPU encoding because it would cost game performance.
  ```

- With `vendor = "auto"` the message lists **every** vendor tried, in order
  (`h264_nvenc`, `h264_qsv`, `h264_amf` for H.264), each with its own reason.
- Nothing starts: no `WGC capture started …` line, no `seg-*.mp4`, no `buffering …` line,
  no clip.
- The failure happens at **startup**, before any capture. A `video writer thread has
  stopped` message means the encoder was accepted and then died mid-flight: for this
  criterion that is a **failure**, not a pass, even though the exit code is non-zero.
  (Such a message now carries ffmpeg's exit status and stderr after the semicolon, e.g.
  `video writer thread has stopped; ffmpeg exited with exit status: 1 using encoder
  'h264_amf': DLL amfrt64.dll failed to open` — useful, but this criterion is met by
  refusing to start at all.)

**FAILURE (each of these is a defect, not a pass)**

- The process **exits 0**, keeps running, or produces a clip. That is a **silent CPU
  fallback**, which violates the project's non-negotiable principle against CPU encoding
  — a defect.
- It exits non-zero but the message does not name the encoder (e.g. a generic "ffmpeg
  failed", or `video writer thread has stopped` with no encoder named).
- It exits non-zero but the message does not say *why* — no ffmpeg reason, and no
  "not advertised" for an encoder the build does not carry.
- It names an encoder but not the failure reason, for a vendor that was
  `advertised, FAILS` in step 0.2. The distinguishing check is: does the message repeat
  ffmpeg's own words (e.g. the missing DLL)?
- It silently proceeds with a *different* hardware vendor than the one requested, without
  saying so.

**Do not misread:** the message names the **ffmpeg encoder id** the codec resolves to
(`h264_nvenc` for `codec = "h264"`, or `hevc_nvenc` for HEVC), not the vendor word
"NVIDIA". Look for `h264_nvenc` / `hevc_nvenc` (and the `*_qsv` / `*_amf` equivalents).
Also note that only the *first* candidate to work is used (`auto`), so a machine with a
working Intel iGPU and a broken NVIDIA runtime legitimately picks `h264_qsv` and says so
in the log line `encoding with h264_qsv`.

---

## Criterion 8 — The clip contains a synchronised audio stream

**Command**

```powershell
ffprobe -v error -show_streams -show_format -of json "%LOCALAPPDATA%\replayed\clips\clip-<ts>.mp4"
```

Then play the clip — e.g. `ffplay "%LOCALAPPDATA%\replayed\clips\clip-<ts>.mp4"`, or any
player — and listen.

**Expected observation (PASS)**

- `ffprobe` lists **exactly one** stream with `codec_type: "video"` and **exactly one**
  with `codec_type: "audio"`.
- Video `codec_name` is `h264` (or `hevc`, per config); audio `codec_name` is `aac`.
- On playback the audio is **audible** and **lip-sync is correct** — play a sound that is
  tied to a visible on-screen event and check the sound lines up with the picture.

**Drift:** the design says A/V drift is logged per clip, and it is. Every splice emits an
`info`-level line, so it is visible at the default `RUST_LOG=info`:

```
clip clip-<ts>: video <v>ms audio <a>ms drift <delta>ms
```

`<v>` and `<a>` are the two streams' durations in the produced clip. `<delta>` is
`(video start + video duration) − (audio start + audio duration)` — how much later the
video timeline ends than the audio's. **A negative `<delta>` means the audio outlasts the
video.**

What is healthy: on the real 60 fps pipeline both streams are cut on the same segment
boundaries, so the offset is small — **tens of milliseconds, comfortably under ~100 ms** —
and it does **not grow** with clip length. What is unhealthy: a **multi-hundred-ms or
second-scale** offset, above all one that **grows** as `pre_seconds + post_seconds`
increases — that is the two live sources genuinely desyncing, not muxing quantisation.

**Honest limitation:** this drift is measured **within the produced clip**. It does **not**
measure the QPC-clock divergence between the two *live* capture sources (the WGC video
clock vs the WASAPI audio clock); that would require instrumenting the capture path
itself, which this build does not do. The line proves the muxed clip's two streams line up;
it is not a check of the live source clocks. (See [Known gaps](#known-gaps).)

**FAILURE looks like**

- Zero audio streams, more than one audio stream, or more than one video stream.
- An audio stream is present but **silent** — WASAPI loopback captured nothing.
- Lip-sync is visibly/tangibly off (audio leads or trails the video).
- The drift line reports a **multi-hundred-ms or second-scale** `<delta>`, or `<delta>`
  grows with clip length across successive clips — a real desync.
- `ffprobe` errors on the clip.

**Do not misread:** a normal clip has exactly two streams (one video, one audio). The
`clip-<ts>.concat.txt` intermediate and the scratch `seg-*.mp4` files are inputs to the
splice, not what you probe — probe the finished `clip-*.mp4`.

### Checking an endpoint that is not natively 48 kHz

The audio backend opens the loopback stream asking the audio **engine** to convert to the
pipeline's 48 kHz stereo s16 (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`), so a default playback device whose native mix
format is 44.1 kHz or 96 kHz — the usual default for USB DACs and wireless gaming
headsets — no longer refuses to start. This build has **no Rust-side resampling**; the
engine-side conversion **has** been observed, on the Windows test machine on 2026-09-24
(ledger §10.2: the backend logged `converting=true` and the clip had audio in it), so this
section is the regression check for it rather than the first sighting. Set the machine's
default playback device to 44.1 kHz (or 96 kHz) in Windows Sound
settings first, or there is nothing to check.

1. Play something audible (a game, a video, music), then take a clip as in criterion 3.
2. In the log, find the line printed when audio capture starts:

   ```
   WASAPI loopback capture started on the default render endpoint native_sample_rate=44100 native_channels=2 native_sample_format=F32 requested_sample_rate=48000 requested_channels=2 requested_sample_format="s16" converting=true
   ```

3. Probe the clip's audio stream:

   ```powershell
   ffprobe -v error -select_streams a:0 -show_entries stream=codec_name,sample_rate,channels -of default=nw=1 "%LOCALAPPDATA%\replayed\clips\clip-<ts>.mp4"
   ```

**Expected observation (PASS)** — all three:

- The log line names the endpoint's real, non-48 kHz rate, `requested_sample_rate=48000`
  and `converting=true`. That pair is the evidence the *engine* did the conversion.
- `ffprobe` reports `sample_rate=48000`, `channels=2` (`codec_name=aac`).
- The clip's audio is **audible** and lip-sync is correct, as in the steps above.

**FAILURE looks like**

- The app refuses to start with an `Initialize` error naming the requested 48000 Hz stereo
  s16 format: on this machine the engine would not convert, so the format could not be
  produced. Report it — with the `native_sample_rate` / `native_channels` /
  `native_sample_format` values — and **do not** set the device to 48 kHz and call the
  check passed; that is the cliff this change exists to remove.
- `ffprobe` reports an audio stream that is not 48000 Hz stereo: the requested format was
  not what came back, and the clip's audio timeline is mislabelled.
- The clip's audio is silent.

---

## Known gaps

These are unverified or broken areas of the current build. They are stated plainly and
not softened; do not read a pass elsewhere as coverage of them.

- **The Windows backends have run, and on a much more recent build than before.** An
  early build of `crates/capture/src/wgc.rs` and `crates/capture/src/wasapi.rs` did run on
  Windows (a 4K capture session — frames were captured and audio came through WASAPI; see
  `docs/verification-status.md` §1). Since then a **2026-09-24 session** (§10 of the same
  file) ran the whole workspace's test suite on the Windows test machine and, in a real desktop session,
  captured a clip with the shipping build. That run is the current evidence for the readback
  skip's *pacer* half (`dd921b3`) and for the WASAPI autoconversion (`5a3dce8`); what it did
  **not** cover is the WGC `discard_pending` half — the CLI test that covers the readback path
  drives the **stub** capture, not WGC — and the silence detector (`4ce2c72`) has still never
  *fired*. The GUI window **has** been opened — the installer was built, installed and run
  in a real session 1 on 2026-09-26 (ledger §7's correction, `docs/packaging.md` §3.5) —
  but the tray, the keypress and hide-on-close are still unobserved, and every criterion
  below still needs a run
  on the **current** build. This runbook is where that gets done.
- **Engine-side sample-rate conversion has now been observed.** The WASAPI backend
  initialises the loopback stream with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
  AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` and asks for 48 kHz stereo s16, so the audio
  engine converts an endpoint that is not natively that instead of refusing it. **Observed on
  the Windows test machine on 2026-09-24**: `WASAPI loopback capture started on the default render endpoint
  native_sample_rate=48000 native_channels=2 native_sample_format=F32
  requested_sample_format=s16 converting=true`, and the clip that came out had audio in it.
  That is the *conversion*; the *silence detector* on the same path is still unrun. If the
  engine declines instead, the app refuses to start: loudly, naming the requested format,
  and **without** a silent fallback to the endpoint's own rate (that would mislabel the
  clip's audio timeline). The remaining half of the check is criterion 8, "Checking an
  endpoint that is not natively 48 kHz", on a machine whose default playback device is set to
  44.1 kHz or 96 kHz.
- **Live-source clock divergence is still unmeasured.** A/V drift *within each produced
  clip* is now logged (see criterion 8), but that only checks that the two muxed streams
  line up. The divergence between the two *live* capture clocks — the WGC video clock and
  the WASAPI audio clock, both nominally QPC-based — is **not** instrumented anywhere, so
  a slow live-source desync that the segment muxing happens to reshape would not be
  caught here.
- **The capture frame pool is never recreated on a display mode change.**
  `Direct3D11CaptureFramePool::Recreate` is not called; a mid-capture resolution or
  refresh-rate change is not handled. The backend copies whatever size the incoming
  texture happens to be, but the pool itself is never resized.
- **Encoder probing is a 1-frame smoke test, not a full capture.** Vendor selection now
  lists encoders with `ffmpeg -encoders` *and* makes each advertised candidate encode one
  generated 320x240 frame (`cargo xtask probe` reports all three states: `advertised, WORKS`,
  `advertised, FAILS: …`, `not advertised`). That proves the encoder **opens** on this
  machine. It does **not** prove the *live capture path* drives it at 4K. The
  `advertised, FAILS` case **has** been observed on real hardware — on the Windows test
  machine, where ffmpeg listed `h264_amf` and died with `DLL amfrt64.dll failed to open`
  (ledger §1) — but not on the *live capture path*; whoever has the Windows test machine
  confirms it with `cargo xtask probe` there. What is verified on the
  development host is the selection logic (against injected results) and the smoke test's
  own plumbing (a working encoder passes, an encoder that cannot open reports ffmpeg's
  reason, a hanging child is bounded by the timeout).
- **The harness is not a substitute for a human.** `cargo xtask verify` runs the criteria
  it can automate and marks the rest *not performed* with a reason. It has **run on
  Windows** twice — on 2026-10-15 and again on 2026-10-16, the second time after the
  harness itself was corrected (ledger §21 and §23) — but a fresh run is still checking
  the harness as much as the app. The first thing to check on the Windows test machine is that the interactive session it
  runs in really gives the capture backend a desktop: if the report's `capture_started` row
  says the WGC line is missing, or the geometry says `1280x720`, the session has no
  desktop (or the stub ran) and nothing else in that report means anything. One row was
  still *not performed* after both runs: `silence_detector`, which needs a machine whose
  default playback device is not 48 kHz. The other, `eviction`, was unperformable only
  because the row was written against the old disk ring — it has since been rewritten to
  read the ring's `evicted=` counter out of the status line (ledger §25). See the harness's
  own "What this harness does not cover" section for the full list.
- **Non-Windows runs prove none of this.** Off Windows the pipeline uses `StubCapture`,
  which captures a synthetic image and nothing real. A green macOS/Linux test run is not
  evidence for any of the eight criteria.

---

## How to record the result

`cargo xtask verify` writes the whole record for you: `target\verify\report.md` carries
one row per check with its criterion, the expected value, the measured value, the verdict,
and every raw command output underneath (the probe, the run's log, the ffprobe, the
volume numbers). Attach that file. It is generated by the code in `xtask/src/verify.rs`,
so every number in it is reproducible with the command line it prints.

What it does **not** cover, and what therefore still has to be written by hand (or left
open with a reason):

- the **GUI window** and its recording wiring — no window is opened by any automated run;
- a **real `Ctrl+F8` press** — the harness takes its clip through the same trigger the key
  reaches, but it never drives the global hotkey itself;
- **lip-sync by ear** — the report carries the A/V drift number, not a human's judgement;
- anything needing a **real game** (the Phase 4 integrations);
- the **live-source clock divergence** (not instrumented in this build);
- whatever the machine's state made impossible that day — an endpoint that is natively
  48 kHz never exercises the engine-side conversion, and a box whose every encoder works
  has no unusable vendor to point criterion 7 at. The report marks those *not performed*
  with the reason, so they are visible rather than silently absent.

If you are recording by hand instead, write the measurements to
`docs/runbooks/phase-1-results-<date>.md` (or a comment on the tracking issue) — one line
per criterion, with the measured value and PASS/FAIL. Record, specifically:

- **Environment:** ffmpeg version (`ffmpeg -version`) and the GPU + driver version, because
  encoder behaviour is driver-dependent.
- **Criterion 1:** the WGC display name and the captured `W×H`; the configured `fps`; and
  whether `frames=` grew at roughly `fps × seconds` and `span=` tracked real time over the
  run (yes/no).
- **Criterion 2:** the configured `ram_cap_bytes`; the largest `bytes=` seen; and that no
  `seg-*.mp4` files were written while the buffer ran.
- **Criterion 3:** `post_seconds`; the measured trigger→`wrote` delta (with the self-test
  trigger this is the two log timestamps; with a real press, your stopwatch); pass/fail
  against `post_seconds + 2 s`.
- **Criterion 4:** the expected `pre_seconds + post_seconds`; the `format.duration` seen;
  the absolute difference.
- **Criterion 5:** the logged encoder name; the clip's video `codec_name`; the approximate
  extraction wall time and the clip duration.
- **Criterion 6:** `cores used`; RSS in MB; and the machine's logical-processor count (to
  interpret Task Manager).
- **Criterion 7:** the forced `vendor`; the state step 0.2 reported for it
  (`advertised, WORKS` / `advertised, FAILS: …` / `not advertised`); the process exit code;
  and the exact error text.
- **Criterion 8:** the stream counts (video/audio); the codec names; whether lip-sync was
  correct on playback; and the `<delta>` from each clip's `clip … drift` log line (note
  whether it stayed sub-100 ms or grew).

Do **not** mark a criterion PASS unless you personally observed it on Windows. Where a run
was skipped or blocked, write that down.
