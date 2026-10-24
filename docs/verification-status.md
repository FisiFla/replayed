# Verification status

**This document is a ledger of what has actually been proven about replayed, and how.**
It exists because a repository that only says what it *intends* to do will be believed,
and this project has already shipped a feature that passed every macOS test and produced
solid black video the first time it ran on real Windows hardware. (That defect has since
been fixed and re-confirmed to produce real pixels — §2.) That memory is the reason this
file is blunt.

Read it before believing any other document in this tree. Where a source is quoted it is
named, so every line below can be re-checked.

---

## The headline, stated plainly

> **The Windows-only surface has now run on Windows.** On 2026-09-24 the whole Rust workspace
> was built *and its test suite executed* on the Windows test machine (Windows 11, NVIDIA RTX-class GPU): every crate's
> tests, the egress guards, and — in a real desktop session, with the owner's approval — a
> genuine capture run that wrote a clip. That session is §10. It is the first time the
> recording engine, the storage policy, the game-presence code, the WGC and WASAPI capture
> backends and the shipping hardware-encoder selection have *executed* rather than being
> type-checked for it. Doing it found three test defects — each fixed — and one thing that is
> still broken on Windows and is not a test's fault.
>
> **Correction (2026-10-17).** An earlier version of this paragraph said the desktop **GUI** had
> "never run". That stopped being true on 2026-09-26: the installer was built, installed and
> run on the Windows test machine in Session 1 (`docs/packaging.md` §3.5, up for 30 seconds), and the
> rebuilt app was run again with its own log file (28 MB), whose
> output diagnosed the stale-clip-index fault behind the reported symptoms. So the window
> *has* been opened on the Windows test machine. What that does **not** establish: the tray's appearance, the
> hotkey keypress, hide-on-close, and that the console windows are gone (a human has to watch;
> the log cannot show a window).
>
> What has still never run anywhere: the two game integrations **against a real game**, the
> microphone's silence detector *firing*, `[app] start_with_system` writing a registry value,
> and anything that would need a synthesised keypress — which this project will not do on a
> machine that runs an anti-cheat. (`cargo xtask verify` has run on Windows since 2026-10-15:
> §21, §23.)
>
> The earlier measurement stands as the state of *that* build, and has since been measured again on
> later ones: the 2026-09-23 session (NVIDIA RTX-class GPU, 3840x2160 at 150% scaling, [issue #1]) proved the
> capture→encode→ring-buffer→splice path can run, and found that **the pipeline cannot sustain the
> configured frame rate at 4K and burns ~50% of a CPU core doing it** ([issue #1]). Both of those
> have since come back as passing, and both measurements are on the Windows test machine: **§13** for the rate
> (`fps=56.9/60`, 8 dropped of 10,525) and **§15** for the core (3.1–4.0% of one core, on a path
> that stops the captured frame crossing to the CPU at all).

If that paragraph surprises you, it is doing its job. The rest of this file is the detail.

**How to move an item out of *type-checked only*:** run `cargo xtask verify`
(`dda459b`) on the Windows box. It runs the criteria it can automate in one bounded
session and writes `target\verify\report.md` — every check with its threshold, its
measured value and the raw output behind it — marking anything it could not perform as
*not performed*, with the reason. From a remote shell, use
`scripts/verify-in-interactive-session.ps1`, because a capture session needs a desktop.
**The harness first ran on Windows on 2026-10-15** (§21, §23; before that it had only been
exercised on the development host against the synthetic backends), and its own report is still
part of what you are checking: the
first row to look at is `capture_started` — if the WGC line is missing, or the geometry is
`1280x720`, the session has no desktop (or the stub ran) and the rest of that report says
nothing. It cannot check the GUI, a real keypress, anything needing a real game, or the
live clock divergence; §9 lists what stays manual and §10 records which parts of it the
2026-09-24 session got to by hand instead.

[issue #1]: https://github.com/FisiFla/replayed/issues/1
[issue #2]: https://github.com/FisiFla/replayed/issues/2

---

## How to read the evidence levels

Every claim below carries one of these four labels. They are not a scale of confidence;
they are a statement about *what was done*.

| Level | Meaning |
|---|---|
| **Verified on Windows hardware** | It ran on a real Windows machine and produced the stated result, at the stated number. |
| **Verified on the dev host** | Covered by an automated test or CI job on macOS (the development host). Says nothing about Windows behaviour. |
| **Type-checked only** | Compiled for `x86_64-pc-windows-msvc` with `cargo check` (no link, no run). Proves the API shape, ownership and types; proves nothing about runtime behaviour. |
| **Unverified** | Written, but there is no test, no CI job and no observed run of any kind. |

A test existing for *adjacent* code does **not** raise an item's level. Where the
repository contradicts itself, that is called out rather than resolved by preference.

---

## Ledger tallies

**Recounted 2026-10-17.** The previous tally (19 / 23 / 6 / 0) was knowingly stale and had been
patched by prose for weeks. This one was counted again from the level-bearing rows of the item
tables in §2, §3, §4 and §8, with a row that names the same commit as an earlier row counted
once, at the **strongest** level any of its rows gives it. The §1 measurement tables are the
evidence behind §2's items, not extra claims, so they are not counted again.

| Level | Count |
|---|---|
| Verified on Windows hardware | 11 |
| Verified on the dev host | 34 |
| Type-checked only | 4 |
| Unverified | 4 |
| **Distinct claims** | **53** |

How to read it:

* **It counts what the table cells say.** Where a later section (§10 onward) *moved* an item
  without anyone editing its row, the row still holds the old level, and so does the count. The
  Windows column is therefore still an under-claim for the recording engine, the storage policy
  and the game-presence code (§10), and nothing from §11–§23 is in the 53.
* **Unverified is no longer zero** because §8 (the desktop shell's background half) is now in
  scope; its four unverified rows are real: the keypress itself, the tray existing at all, recording
  while the window is hidden, and the registry write.
* **The 4 type-checked items** are `discard_pending`, WASAPI engine-side conversion, the silence
  detector and `Frame` move-on-submit. (§8 carries a fifth, the `RegisterHotKey` failure row, at
  the stale level *type-checked only*; it is the same claim as the §3 row that §10 moved to
  *Verified on Windows hardware*, so it is counted once, there.)
* **One mixed row counts as dev-host**: "Close hides the window and never quits" is *verified on
  the dev host* for the rule and *unverified* for the behaviour, and is counted at the first.

The per-item tables remain the source of truth; this is a summary of them.

---

## 1. The Windows capture runs and the retained ring

Environment: **an NVIDIA RTX-class GPU, a 13th-gen Intel desktop CPU, 3840x2160 primary display at 150% DPI scaling**,
configured `encode.fps = 30`, `Ctrl+F8` hotkey, H.264. The measurements below are recorded
in [issue #1] and [issue #2]; the harness's own argument list is quoted in issue #2.

Command used to check the source of every row: `gh issue view 1` / `gh issue view 2`.

| Claim | Level | Evidence / number |
|---|---|---|
| WGC capture starts and delivers frames at the monitor's native **3840x2160** | Verified on Windows hardware | issue #1 measurement table, "Capture resolution — 3840x2160, real content — PASS" |
| The encoder child runs with **`h264_nvenc`** | Verified on Windows hardware | issue #2, the encoder's actual argument list: `-c:v h264_nvenc -b:v 20000k -g 30 …`; also issue #1 (NVIDIA GPU → NVENC) |
| Audio is transported to the encoder over **loopback TCP** and muxed as **48 kHz stereo AAC** | Verified on Windows hardware | issue #2 arg list: `-f s16le -ar 48000 -ac 2 -i tcp://127.0.0.1:51234 … -c:a aac -b:a 192k` |
| The ring buffer writes segments | Verified on Windows hardware | issue #1: `frames=1814 segments=57 bytes=37795772 span=57000ms dropped=1183 …`; issue #2: segments every ~1.25 s |
| A `Ctrl+F8` clip is produced and its **duration matches the target** | Verified on Windows hardware | issue #2: "a clip of **13,091 ms** was produced against a **13,000 ms** target, written **137 ms** after the post-roll elapsed" |
| Acceptance criteria **2, 3, 4, 5, 7 and 8 pass** on this hardware | Verified on Windows hardware | issue #1: "clips are produced correctly, and criteria 2, 3, 4, 5, 7 and 8 all pass" |
| Encoder probing reports an **`advertised, FAILS`** vendor (an encoder ffmpeg lists but the machine cannot open) | Verified on Windows hardware | `xtask/src/main.rs` comment: "measured on a box with no AMD hardware, ffmpeg listed `h264_amf` and died with `DLL amfrt64.dll failed to open`" |
| The clip's **video codec is H.264** and **audio is AAC 48 kHz stereo** | Verified on Windows hardware | follows from criteria 4/5/8 passing (issue #1) + the fixed pipeline format; and the audio is confirmed **non-silent** by measurement — the retained clip probed `mean_volume = -31.7 dB, max_volume = -11.3 dB` (a silent track would sit near -91 dB) — see the read-only note below. |
| The scratch-**cap** eviction ran and held | Verified on Windows hardware | a **15,000,000-byte** cap with a ledger `total_bytes = 14880356` (≤ cap) and 24 segments on disk, `seg-000063.mp4` … `seg-000086.mp4` — 63 segments evicted, oldest-first. Re-derived read-only from the retained ring, not from a committed log (see below). |

### Re-derived read-only from files still on the Windows test machine

When this repository was inspected there was no committed log carrying these numbers; the
fresh values below come from **read-only analysis of the scratch ring and one clip still
present on the Windows box** (`%LOCALAPPDATA%\replayed\scratch` and `clips`) using
`ffprobe`/`ffmpeg` reads only — no capture, no display, no input. The run-level `Ctrl+F8`
clip facts come from the earlier **interactive session's own output**, which was never
committed; that provenance is weaker than a committed log and is labelled as such.

| Claim | Level | Measured |
|---|---|---|
| The cap eviction holds and deletes oldest-first | Verified on Windows hardware (read-only re-read) | cap **15,000,000 B**; ledger `total_bytes = 14880356`; 24 segments, lowest `seg-000063.mp4`, highest `seg-000086.mp4`; **63 segments evicted**, oldest first |
| Segments carry real pixels, not black frames | Verified on Windows hardware (read-only re-read) | `seg-000084.mp4` = 640072 bytes; video h264 **3840x2160**; a decoded frame (downscaled 320x180) has R/G/B means **16.9 / 25.3 / 42.0** with **216 distinct colours** (a uniform frame would be 1); frame exported as an 80798-byte PNG and **visually confirmed to show a real desktop**. The `copy_out` black-frame defect is therefore **fixed and producing real content** (§2) |
| Segments carry non-silent audio | Verified on Windows hardware (read-only re-read) | `seg-000084.mp4` audio: aac 48000 Hz, 2 channels; `mean_volume = -52.5 dB, max_volume = -42.0 dB` (digital silence is ≈ -91 dB) |
| Segments carry a fixed number of frames | Verified on Windows hardware (read-only re-read) | 30 frames per segment |
| A `Ctrl+F8` clip is produced, correct duration, non-silent | Verified on Windows hardware (interactive session) | **13,091 ms** against a **13,000 ms** target (10 s pre + 3 s post), written **137 ms** after the post-roll elapsed; probed **h264 High 3840x2160 + aac LC 48 kHz stereo, 8,529,609 bytes**; audio `mean_volume = -31.7 dB, max_volume = -11.3 dB` |

The `copy_out` defect and its fix are stated accurately in `crates/capture/src/wgc.rs`;
its module doc previously said the fix had "never been re-run on Windows", which
understated the state and has since been corrected.

### What **failed** on that hardware — do not soften these

| Failure | Level | Number |
|---|---|---|
| Cannot sustain the configured frame rate at 4K | Verified on Windows hardware | **~24 fps** sustained against **30 fps** configured — issue #1 |
| Frames dropped by the encoder | Verified on Windows hardware | **1183 of 1814 frames submitted dropped — 65%**; the debug line reads `frames=1814 … dropped=1183` — issue #1 |
| CPU cost | Verified on Windows hardware | **50.8% of one core** against a **< 5%** target — issue #1 (criterion 6 **FAIL** on this path; **met since on the zero-copy path, §15**) |
| The media timeline does not track real time | Verified on Windows hardware — the measurement is unresolved there; **the mechanism is now measured on the dev host and fixed** | the divergence on the Windows test machine is real, and two informal measurements disagree on direction: **≈0.81x** (media slower; segments written every ~1.25 s of wall clock — issue #2) versus **≈1.11x** (media faster; segment mtimes average **~899 ms** apart on the retained ring). Both measured the *write* position, which is what `span_ms` is; the mechanism is the frame-rate conversion (see below), and the fix takes the dev host's ratio from 0.660× to 0.987×. **The Windows test machine's own number is still unmeasured** |
| RSS (the one criterion-6 half that passed) | Verified on Windows hardware | 235 MB, peak 267 MB against a < 400 MB target — issue #1 |

The Windows test machine's two figures are **both readings of the write position** (`span_ms` is
`completed segments × segment_time`), which is why they can disagree between runs and why
neither settles the mechanism on its own: what they measure is how fast ffmpeg's muxer got
through a grid that required `R` frames a second — slower when the encoder was short, and
apparently faster in a run whose scratch cap had evicted segments and shifted the ledger's
origin. The earlier confident **0.81x** figure stays **withdrawn** (issue #2's correction
comment), and the *rerun that will settle the Windows test machine's number* is prescribed below. On the dev
host the same reading is now 0.987× rather than 0.660× after the timeline fix, because the
grid that made it a throughput measurement is gone.

**How to measure it properly — do not infer it from file mtimes.** mtimes measure *writes*,
not the media they carry, so a cadence read off mtimes is not the media-vs-real-time ratio.
On the Windows test machine, take the periodic debug line and compare its `span=` field against wall-clock
time across a sustained run, and its `fps=` field against the configured rate. The ratio that
matters is media-time (`span`) per wall-clock second, read from the log on a run long enough
(minutes) to average out segment jitter.

**What the dev host found, and what it fixed. The mechanism is the frame-rate conversion,
not the declared rate.** Measured here (ffmpeg 9.0.2, the real argument list) and corrected
twice on the way, so read this as the final account:

- With the segment muxer in place, a declared `-framerate R` makes ffmpeg resample the video
  onto a rigid `1/R` grid, and the *default* conversion mode fills that grid by
  **inventing frames**: a pipe fed 122 frames at ~24 fps with 30 declared encoded 180 frames,
  `dup=58 drop=0`. In the real pipeline the multiplication is what hurts — a 4K arm fed 39
  frames over 12.6 s encoded **1631** of them, `dup=1592`, every scratch segment reporting
  `avg_frame_rate=120/1`.
- ffmpeg's own `speed=` field then says where the time goes in that arm: **0.333×**, i.e. the
  muxer's write position advanced one second per three seconds of wall clock, because the
  encoder can only emit ~40 of the 120 frames a second the grid demands. The ring's
  `span_ms` is exactly that write position (`completed segments × segment_time`), so
  *that* — not a stretched container timeline — is the number that falls behind. Measured
  at the pipeline level below: **0.660×**.
- The container's own timestamps are **not** stretched: each frame keeps the arrival time it
  was given and the invented frames only pad the gaps between them. Verified by tagging every
  fed frame and reading its media timestamp back out of the encoded segments (49 distinct
  frames, 1085 output frames, media span 8001 ms against an arrival span of 9027 ms — slope
  0.94 against *write* times, the small deficit being ffmpeg's own read lag, not a warp). So
  the Windows test machine's original complaint was about the ring's clock and the *staleness* it implies
  (footage at the trigger was seconds old), not about time-compressed pictures.

**The fix, and the A/B that justifies it.** `-fps_mode passthrough` on the video output
(`crates/encoder/src/ffmpeg.rs::video_output_args`) removes the conversion: each frame
reaches the muxer with its arrival timestamp, so the media clock *is* the wall clock
whatever rate the machine manages, and the encoder is no longer asked to emit `R` frames a
second of which most are invented. Same machine, same stub capture, same
`--dev-software-encoder`, 12 s of media requested, 4K output scaled from the stub
(`fps = 120`, `adapt_fps = false`) and the 720p control (`fps = 30`, adaptation on). The
ratio is the ring's own clock: `span=` between two status lines over their wall-clock
timestamps, over the window that excludes the ring's fill.

| arm | declared | achieved | dropped by the encoder's queue | media/wall | segments/wall-second |
|---|---|---|---|---|---|
| 4K output, **before** (`ae5b7c5`+`1028a0f`) | 120 fps | 3.3 fps | 2295 of 2454 | **0.660×** | 0.66 |
| 4K output, **after** (`-fps_mode passthrough`) | 120 fps | 31.7 fps | 1172 of 1801 | **0.987×** | 0.99 |
| 720p control, before | 30 fps | 29.6 fps | 0 | **0.999×** | 1.00 |
| 720p control, after | 30 fps | 29.7 fps | 0 | **0.986×** | 0.99 |

The same A/B at the ffmpeg level, with the child's exact argument list, feeding 8 fps against
120 declared at a 4K output: before, 39 frames became 1631 coded frames and the write
position ran at `speed=0.333×`; after, 97 frames became **exactly 97** coded frames (no
invention at all) and `speed=0.832×`, limited only by how long the feed itself lasted.

**What that change costs, measured, and it is not nothing.** A VFR segment holds the frames
that arrived while it was open, which is `segment_time` **minus up to one frame interval**:
measured 0.82–1.00 s of media per 1 s segment at ~40 fps delivered, and ~0.94 s at the
recorder test's 10 fps. Three consequences, all recorded rather than smoothed over:

1. **A clip can be short of its nominal window by one frame interval per segment it is
   spliced from** — measured 2821 ms for a 3000 ms request at 10 fps in
   `crates/recorder/src/tests.rs`, and 2756 ms of video for a 3000 ms request in the 4K CLI
   run. Before the fix such a clip was exactly nominal and its footage was **seconds old**
   (the `hotkey pressed: media=12000ms wall=18619ms` line is that staleness); now the
   footage is current and the file is a few frames short. The recorder test asserts the
   honest bound (one frame interval per segment) instead of the old one.
2. **`span_ms` over-estimates the footage on disk by the same one frame interval per
   segment**, since the ring counts `segment_time` per completed segment without probing it.
   At 30–60 fps that is 1–3%; at the recorder test's deliberately slow 10 fps it is 6%.
3. **Timestamps are resolved at `1/R`** (the demuxer's timebase comes from `-framerate`), so
   frames read within one tick of each other share a timestamp and occupy no time. Measured
   at 4K with bursts arriving from the pipe: 38 repeated timestamps among the 117 frames of
   the produced clip, and none in the 720p control. The frames are all there and decode; the
   duplicated ones are simply not displayed.

**Splicing and playback survive that — checked, not assumed.** `-reset_timestamps 1` still
gives every segment `start_time = 0`; the segmenter still cuts ~1 s segments (13 segments and
13 s of media in the after-4K arm); `replayed_media::edit::concat_lossless` — the exact
function `ClipSplicer` calls — still produces a clip that probes with **both** streams and
whose every coded frame decodes (117 frames in the 4K clip, 46/46 in the regression test).
One caveat: a frame that lands just across a segment boundary makes the concat demuxer's
per-file offset step *backwards* at that boundary — measured 0 in three of the four real clips
and, in a deliberately starved synthetic feed, a worst case of ~16–46 ms (well under one frame
interval at 30 fps). `crates/encoder/tests/timeline.rs` bounds it rather than pretending it is
absent.

**The throughput probe is not, and cannot be, the timeline guarantee.** The last two commits
before this one (`ae5b7c5`, `1028a0f`) measured the encode rate at startup and declared
`min(configured, measured)` to both the pacer and the encoder child. That does not make the
timeline correct on the machine that matters, and the numbers now say so plainly: the probe
measured **62 fps** and the live pipeline achieved **8** on the same host in the same
configuration (the probe measures the encode path only — raw frames to an encoder to `-f
null` — while the pipeline is WGC readback + copy + pipe + encode + segment muxing). On the
Windows box, where NVENC is cheap and the 33 MB-per-frame readback is the known bottleneck,
the probe's number is *more* optimistic. It remains as what it always should have been: a
diagnostic that reports the sustainable rate and a pacing aid that keeps capture from paying
for frames the encoder will drop.

`gh issue list --state all` then showed both issues **OPEN**, and both still needed the Windows test machine. Both
have since had their readings taken and both are now **closed** — #1 and #2 — leaving #3 open:

- issue #1 (throughput): **closed.** The Windows test machine produced what it asked for and more: `fps=56.9/60`
  with 8 dropped of 10,525 (§13), and criterion 6's CPU number, which the zero-copy path then met
  at 3.1–4.0% of one core with `capture_copy=0.6–0.7%` (§15).
- issue #2 (the timeline): **closed.** The *direction and magnitude* question above was answered by
  mechanism and by the local A/B (the ring's clock was the write position and it ran at 0.66×; it is
  now ~0.99×), and the fix is in the shipping argument list. **The Windows test machine's own number has since been
  taken** — §13's `media=180266ms wall=180372ms` over 180 s, a drift of 0.06% — which is what
  settles it, and which also shows passthrough leaves the machine able to encode the frames it
  actually captures.

---

## 2. The capture path, item by item

Source checked with `git log --oneline` and by reading each module's own doc comment.

| Path element | Level | Evidence |
|---|---|---|
| **WGC video capture** (`crates/capture/src/wgc.rs`) — starts, delivers frames, pulls via `TryGetNextFrame` | Verified on Windows hardware | ran in the session (issue #1); the module doc records "It was run on Windows 11 on a 4K/150%-scaled desktop" |
| **WGC `copy_out` correctness** (copy the captured texture to staging *before* `Map`) | Verified on Windows hardware | the first real Windows run produced **pure black frames** because `copy_out` mapped a fresh staging texture without `CopyResource`; the fix issues that `CopyResource` before the `Map` (`crates/capture/src/wgc.rs::copy_out`). A decoded frame from a retained segment now carries **216 distinct colours**, R/G/B means **16.9 / 25.3 / 42.0**, and an 80798-byte PNG **visually confirmed to show a real desktop** — real pixels, not black. This resolves the earlier in-tree contradiction (the `wgc.rs` module doc claiming the fix was never re-run has been corrected). |
| **WGC `discard_pending`** — close surplus frames without the GPU readback (`dd921b3`) | Type-checked only | `dd921b3` message: "NOT measured here: there is no Windows host" |
| **WASAPI loopback audio** (`crates/capture/src/wasapi.rs`) — endpoint-native format path | Verified on Windows hardware | the session produced clips with audio and criterion 8 passed (issue #1) |
| **WASAPI engine-side sample-rate conversion** (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, `5a3dce8`) | Type-checked only | `5a3dce8` message: "UNVERIFIED at runtime … nothing here has ever executed on Windows" |
| **The silence detector** for a converted-but-silent endpoint (`4ce2c72`) | Type-checked only | it only fires on the path above, which has never run; `crates/capture/src/lib.rs` says the failure mode is "external and **unverified at runtime**" |
| **Encoder child** (`crates/encoder/src/ffmpeg.rs`, ffmpeg sidecar) — H.264 via hardware encoder | Verified on Windows hardware | `h264_nvenc` in issue #2's arg list; criteria 4/5 passed |
| **Audio over loopback TCP** (`47cd974`) | Verified on Windows hardware | `tcp://127.0.0.1:51234` in issue #2's arg list |
| **Hardware-encoder selection + smoke test** (`6e63977`, `xtask probe`) | Verified on Windows hardware | the `advertised, FAILS` case was observed on the Windows test machine (see §1) |
| **`Frame` move instead of clone on submit** (`f6a5a0e`) | Type-checked only | `f6a5a0e` message: "not measured here — there is no Windows host to measure it on" |
| **Ring buffer, segment ledger and cap eviction** (`crates/replay`) | Verified on Windows hardware | segments written; criterion 2 passed (issue #1), and the retained ring now records the cap (**15 MB**) and oldest-first eviction (§1) |
| **Clip splicing** (`crates/replay`, `crates/media`) — lossless `-c copy` remux | Verified on Windows hardware | the 13,091 ms clip; criteria 4 and 5 passed (issues #1, #2) |
| **The media-time trigger** (`55f1541`) | Verified on Windows hardware | fixed and confirmed on the Windows test machine (issue #2: "Verified on hardware") |
| **The self-test trigger** — the same media-time trigger, reached without synthesising input (`2399332`) | Verified on the dev host (the trigger), not on Windows | `apps/replayed-cli/tests/self_test_clip.rs::the_self_test_trigger_writes_a_clip_through_the_hotkey_path` drives the whole command in-process over real ffmpeg: it waited for the ring to hold the configured window, called `Recorder::clip_now()` once, and wrote a real clip (8,053 ms for an 8,000 ms window, `encoder=libx264`). What still needs Windows is not the trigger but the keypress that reaches the *same* call |

### Tests that cover the non-Windows half of this path

These run on macOS and are the only automated evidence for the logic above. None of them
executes a Windows API.

| Claim | Level | Test |
|---|---|---|
| The pacer discards surplus frames **without** reading them back | Verified on the dev host | `apps/replayed-cli/tests/discard_readback.rs::a_source_faster_than_the_pacer_is_discarded_without_being_read_back` |
| A frame the source offers while the pacer is not due is discarded, not materialised | Verified on the dev host | `discard_readback.rs` (first test) |
| The stub backend's discard path counts separately from its readbacks | Verified on the dev host | `crates/capture/src/stub.rs` unit tests |
| Pipe size is reconciled with the backend's native size | Verified on the dev host | `b94be7b` message: "a macOS run logs `-s=1280x720 (capture native)`"; cross-checked for `x86_64-pc-windows-msvc` |

---

## 3. Everything else in the tree

| Subsystem | Level | Evidence |
|---|---|---|
| `replayed-media` (`ffprobe` → `MediaInfo`, lossless remux/trim) | Verified on the dev host | `crates/media/tests/lossless.rs`, `crates/media/src/probe.rs` (real ffmpeg) |
| `replayed-store` (SQLite schema, clip/session/event index) | Verified on the dev host | `crates/store/src/lib.rs` tests, real SQLite |
| **Storage cleanup policy + executor** delete-before-unlink (`48b5e5a`) | Verified on the dev host | `crates/store/src/cleanup.rs::the_size_rule_evicts_oldest_first_until_under_the_cap`, `crates/store/tests/cleanup_ordering.rs` |
| Evicting a clip detaches its events instead of failing the foreign key | Verified on the dev host | `crates/store/src/lib.rs::deleting_a_clip_releases_its_events_instead_of_failing` |
| `replayed-recorder` — the recording engine shared by CLI and GUI (`71a360c`) | Verified on the dev host | `crates/recorder/src/tests.rs::a_clip_records_why_it_was_taken` (real recording, real ffmpeg, real SQLite) |
| `replayed-cli` drives the engine (`969bce9`) | Verified on the dev host | CLI integration tests in `apps/replayed-cli/tests/` |
| Desktop IPC command layer + session-review window (`33d7ff7`, `c1decfb`) | Verified on the dev host | desktop-shell Rust tests (run in CI, `apps/desktop/src-tauri`) |
| Desktop **recording wiring** — Start/Stop/Save clip over the engine (`912a026`, `f66e52e`) | Verified on the dev host (headless) | frontend `vitest` (`apps/desktop/src/lib/recording.test.ts` etc.), shell Rust tests, and headless render screenshots (`793439b`). **No GUI window is opened.** |
| Desktop **background half** — the tray menu and its state-dependent labels, the tooltip and icon state, the menu-action → shell-call mapping, the close-to-hide rule, the `[app]` defaults and the autostart decision table | Verified on the dev host | `apps/desktop/src-tauri/src/background.rs` unit tests (pure functions over plain data), `config.rs` tests for the new `[hotkeys]`/`[app]` reader, `apps/desktop/src/lib/recording.test.ts` for the two display rules, and the headless screenshot state `11-hotkey-not-installed`. **The tray, the window hide and the keypress themselves are not covered by any of this** — see §9. |
| The hotkey **trigger path** in the GUI — one press, one clip, through `RecorderHost::clip_now` | Verified on the dev host (headless) | `apps/desktop/src-tauri/src/commands/recorder.rs::one_hotkey_press_writes_exactly_one_clip_through_the_recorder` drives a **real** recording (stub capture source, libx264, a real ring, a real splice, a real index row) and asserts exactly one clip file, one index row and one count in the engine. The press is an in-process call — no input is synthesised. |
| **Hotkey registration failure is reported** (`46c0116` in `crates/events`) | **Verified on Windows hardware** (2026-09-24, §10) | `RegisterHotKey` genuinely failed on the Windows test machine — `This operation requires an interactive window station. (0x800705B3)`, from a remote shell — and the error returned to the caller **named the chord**: "the chord Ctrl+F8 could not be registered (RegisterHotKey failed: …)". The previous claim was that this "has never run"; it has, and the branch that was expected to be unreachable in practice turns out to be the one a remote-shell session always takes. A `#[cfg(windows)]` test now asserts the failure names the chord. The parse-failure and "no global hotkey on this platform" branches remain unit-tested (`background::install_hotkey`). |
| **Sidecar pipeline** (`74d345d`) — fetch + SHA-256 verify + allowlisted extract | Verified on the dev host | `xtask/src/sidecars.rs` unit tests (zip-slip rejected, symlink rejected, mismatch refused, allowlist honoured) |
| **Phase 4 — LoL Live Client poller** (`79c4184`) | Verified on the dev host (mocks only) | `crates/events/tests/lol_mock.rs` drives a real TLS server on loopback; see §5 |
| **Phase 4 — CS2/Dota 2 GSI listener** (`3b6777f`) | Verified on the dev host (mocks only) | `crates/events/tests/gsi_listener.rs` over real loopback sockets |
| **Phase 4 — event-triggered clip + reason** (`4f291dd`) | Verified on the dev host | `crates/recorder/src/tests.rs::a_clip_records_why_it_was_taken` |
| **GSI accept-loop blocking-mode fix** (`bd59fce`) | Verified on the dev host (caught by CI) | see §6 |

---

## 4. Added since the Windows session

These are the changes committed **after** the capture session's measurements. None of them
has run on Windows; the ones marked "type-checked only" have not run *anywhere* off the
compiler. Establish the boundary yourself with:

```console
$ git log --oneline --format='%h %ci %s'
```

The first commit after the session is `dd921b3` (2026-09-23 18:00).

| Change | Commit | Level |
|---|---|---|
| Skip the GPU readback for frames the pacer discards (`discard_pending` + pacer-before-materialise) | `dd921b3` | Type-checked only |
| Store cleanup policy + executor | `48b5e5a` | Verified on the dev host |
| CLI indexes clips and enforces the storage policy | `d49e5ee` | Verified on the dev host |
| Tauri v2 shell scaffold | `00eeeb1` | Verified on the dev host |
| Desktop IPC command layer | `33d7ff7` | Verified on the dev host |
| Desktop session-review window (list, player, scrubber, trim) | `c1decfb` | Verified on the dev host |
| `xtask` sidecar fetch/verify | `74d345d` | Verified on the dev host |
| Headless window render + screenshots | `793439b` | Verified on the dev host |
| Recording engine lifted out of the CLI | `71a360c` | Verified on the dev host |
| CLI drives the engine | `969bce9` | Verified on the dev host |
| Desktop recording wiring (start/stop/clip/status) | `912a026`, `f66e52e` | Verified on the dev host (headless) |
| Phase 4 LoL poller | `79c4184` | Verified on the dev host (mock only) |
| Phase 4 GSI listener | `3b6777f` | Verified on the dev host (mock only) |
| Event-triggered clip + reason recording | `4f291dd` | Verified on the dev host |
| Phase 4 documentation | `81e762c` | n/a (prose) |
| WASAPI autoconversion | `5a3dce8` | Type-checked only |
| Silence detector | `4ce2c72` | Type-checked only |
| CI workflow (tests + Windows cross-check + frontend) | `2e90f84` | Verified on the dev host |
| GSI accept-loop blocking-mode fix | `bd59fce` | Verified on the dev host (CI) |
| Self-test clip trigger (`--self-test-clip-after`) — the clip path without synthetic input | `2399332` | **Verified on Windows hardware** (2026-09-24, §10): the shipping CLI ran it in a real desktop session and took a clip — `self-test: the ring holds 7000ms of media; taking the clip` → a 7083ms file, indexed. Also verified on the dev host |
| Desktop background half: tray, global clip hotkey, hide-on-close, `[app] start_with_system` | `46c0116`, `08f6770`, `5ec095d` | Verified on the dev host (headless: pure logic + a real recording driven through the trigger path). **Never observed on any machine**: no tray icon drawn, no window closed, no key pressed, no Run-key write executed |
| A hotkey registration failure is returned rather than swallowed (`crates/events`) | `46c0116` | Type-checked for `x86_64-pc-windows-msvc` (`cargo check --target`); the CLI's half is verified on the dev host (`replayed-cli` tests still pass, the listener is a no-op off Windows) |
| `cargo xtask verify` — the one-command acceptance harness + its report | `dda459b` | Verified on the dev host, where it runs and reports honestly: its clip/trigger/report/parsers/criterion-7 paths all executed, against the stub capture and the software encoder. **It has since run on Windows twice** — 2026-10-15 (§21) and 2026-10-16 (§23) — with two checks still *not performed* |
| The pipeline **paces capture to a rate it can actually keep**: a startup throughput probe (`encode.adapt_fps`, default on) measures the encoder at the real capture resolution, and `min(configured, measured)` is given to both the pacer and the encoder child — one number, read back out of the encoder's own `-framerate`. **This is a pacing aid and a diagnostic, not the timeline guarantee** (see the two rows below, and §1) | `ae5b7c5`, `1028a0f` | Verified on the dev host: the decision's arithmetic, the new config key, the probe's bounds (a real libx264 probe, a dead encoder, a wedged child), and the pacer/encoder agreement are all covered by tests, and the startup lines below came from real CLI runs. **The probe's accuracy at 4K on an NVIDIA GPU is unverified**: only libx264 (software, content-sensitive) was measurable here, its numbers do not transfer to NVENC (hardware, content-blind), and the probe measures the encode path only — on this host it reported 62 fps where the pipeline achieved 8 |
| The **media timeline is the frames' arrival timestamps**, not a declared grid: `-fps_mode passthrough` on the video output, so a machine that cannot keep up drops frames (held in the picture) instead of resampling the clock onto `1/R` — the fix for issue #2 | this change | Verified on the dev host end to end: a 4K pipeline that cannot keep up went from **0.660×** to **0.987×** media per wall second against a 0.999× control (table in §1), and the encoder stopped inventing frames (39 delivered → 1631 coded before; 97 → 97 after). A new regression test (`crates/encoder/tests/timeline.rs::a_starved_pipeline_still_keeps_the_media_timeline_on_the_wall_clock`) declares 4× the rate this machine measures and asserts the ratio stays in 0.85–1.15 with no invented frames; with the option removed it fails with `7243ms of video for 3.000694916s of wall clock (2.414x)` and 37972 coded frames for 984 submitted. **Partially verified on Windows** (2026-09-24, §10): the timeline suite ran on the Windows test machine against the *system* ffmpeg 9.0.2 and the starved-pipeline test **passed** there, so `-fps_mode passthrough` behaves as intended on Windows too. What this does **not** cover is the pinned sidecar (`binaries/ffmpeg.exe`), which is still evidenced only by the option's own strings inside the binary; and one sibling assertion in that file — the audio stream's length against the video's — **failed on Windows**, and it turned out to be a real loss of audio rather than a test artefact: the encoder's own queue was too short to ride out a momentary stall in ffmpeg and discarded 760 ms of a 3 s feed (§10.4). |
| A **VFR segment still splices, probes and plays**: `-reset_timestamps 1` keeps every segment at `start_time = 0`, the segmenter still cuts ~1 s segments, and the product's own lossless concat (`replayed_media::edit::concat_lossless`) produces a clip with both streams and every coded frame decodable | this change | Verified on the dev host: `crates/encoder/tests/timeline.rs::the_vfr_segments_a_starved_pipeline_writes_still_splice_and_play` splices starved-pipeline segments and decodes 46 of 46 frames; the recorder's end-to-end clip test and the 4K CLI run both probe with two streams (117/117 frames decoded). The cost is measured and bounded in §1: a clip can be one frame interval per segment short of nominal, and a boundary frame can step the concatenated timeline back by ≤ ~50 ms |

The two startup shapes that change produced, from real `replayed-cli buffer` runs on the
dev host (stub capture, `--dev-software-encoder`, so the encoder is libx264 at the stub's
1280x720 — **these numbers are this host's and say nothing about the Windows test machine's**):

```text
# encode.fps = 30, measured far above it: nothing is reduced
INFO replayed_recorder: encode rate: 326.8fps sustainable at 1280x720 with libx264 (491 frames over 1.5s); encode.fps = 30 is within that, so capture runs at the configured 30fps

# encode.fps = 120 against a 4K output: reduced to the measured rate
WARN replayed_recorder: encode.fps = 120 is not achievable at 1280x720 on this machine: 61.9fps sustainable at 1280x720 frames scaled to a 3840x2160 output with libx264 (95 frames over 1.5s). Capturing at 61fps instead — the same rate the encoder child is told — so that the capture is not paying a readback and a copy for frames the encoder will throw away. The media timeline does not depend on this rate: frames carry their arrival timestamps, so a clip covers the seconds it was captured over either way. The configured rate was not reached at this resolution: lower encode.output_size (e.g. "1920x1080") or encode.fps to a rate this machine holds, or set encode.adapt_fps = false to declare 120fps anyway and accept the frames that will be dropped and held in the picture.
```

---

## 5. What CI covers, and what it does **not**

> **CI is currently dark, and not because of the code.** The account's GitHub Actions minutes
> are exhausted, so a queued job never gets a runner: it fails in seconds with no steps and
> `runner_id=0`, which is the same signature as a broken workflow and is not one. Verified by
> re-running a commit that had passed in 3m28s unchanged — it failed the same way. Every `gh
> run list` since 2026-09-24 11:51 shows that. **So `gh run list` is not evidence about this
> tree right now**, and the gate below was reproduced by hand instead: the exact steps, run
> locally in order, from the tree of that day (§11).

CI (`.github/workflows/ci.yml`) runs five jobs on three runners: **`macos-latest`** (the
tests, the Windows cross-check and the frontend), **`windows-latest`** (the whole Rust
workspace test suite, the frontend build and the desktop shell's own tests) and
**`ubuntu-latest`** (cargo-deny, the anti-cheat API guard, npm audit and gitleaks). The
macOS job is the development host; the Windows job exists because the store, the recorder,
the CLI and the desktop shell cannot be cross-checked for MSVC from macOS at all.

What a green run proves:

- the whole Rust **workspace test suite** (with `replayed-encoder/test-encoders`) —
  `cargo test --workspace --features replayed-encoder/test-encoders`, **516 passed, 0 failed,
  1 ignored** in that run (the count was 375 at the pass this section was written for, and it
  has grown with every phase since);
- **`cargo check` for `x86_64-pc-windows-msvc`** of the cross-checkable crates
  (`capture`, `encoder`, `events`, `replay`, `media`, `xtask`) — *type-checking*, not
  running. No MSVC linker is needed because `check` stops before linking;
- the desktop frontend: `npm ci`, `svelte-check`, `vitest`, production build; and then the
  desktop shell's own Rust tests;
- `cargo clippy` over the workspace (warnings surfaced, not fatal).

What a green run does **not** prove — quoted from the workflow's own header:

- **No Windows runtime verification.** The macOS job's own header says: "Not one line of
  the WASAPI or WGC backends is *executed* here; they are only compiled. A green run says
  nothing about how the app behaves on a real machine." (§10 is where they were executed —
  by hand, on the Windows test machine. The `windows-latest` job has no GPU and no
  desktop, so it executes the *tests*, not the capture path.)
- **No GUI window.** "No window is opened, the screen is never captured and no input is
  synthesised."
- **`replayed-store`, `replayed-recorder`, `replayed-cli` and the desktop shell are not
  cross-checked for Windows from the macOS host**, because rusqlite's bundled SQLite C
  source cannot be built for MSVC from macOS. They are host-verified *there*, and the
  macOS job cannot carry that check — which is not the same as unverifiable: the
  `windows-latest` job runs the whole workspace suite on Windows, and §10 records the same
  suite being built **and its tests run** on the Windows test machine by hand.

---

## 6. Platform traps

A recurring class of bug that Linux and Windows do **not** have, and which has already
bitten this codebase twice. The full note is in
[`docs/platform-traps.md`](platform-traps.md); the short version:

> **`accept()` makes the accepted socket inherit the listener's `O_NONBLOCK` flag on
> macOS/BSD.** A listener is put into non-blocking mode only so an accept loop can enforce
> a deadline — but on macOS the *accepted* socket comes back non-blocking too, so reads and
> writes fail with `WouldBlock` at the worst moment. **After `accept()`, always set the
> socket's blocking mode explicitly; never assume it.**

Two sites, both fixed, both with the reasoning at the call site:

- `crates/encoder/src/ffmpeg.rs::accept_within` — the loopback-TCP audio transport
  (`stream.set_nonblocking(false)` after accept; fix `82578bd`).
- `crates/events/src/gsi.rs::handle` — the GSI listener (`stream.set_nonblocking(false)`;
  fix `bd59fce`). This second instance was caught **only by CI on a fresh runner** — it
  does not reproduce reliably on a warm local run:

```console
$ gh run view 35904179918 --log-failed
… test a_post_with_no_auth_block_at_all_is_rejected … FAILED
…   left: 400
…  right: 403
```

`crates/events/tests/lol_mock.rs` carries the same guard for the same reason.

---

## 7. Known limitations — do not mistake these for bugs, or for working features

- ~~**The Windows test machine's own media/wall ratio has never been measured with the fix in place.**~~
  **Measured on the Windows test machine, 2026-09-25 (§13):** `media=180266ms wall=180372ms`, a drift of
  106 ms over 180 seconds. The media clock is the wall clock to 0.06%, so `pre_seconds`
  means real seconds. The mechanism (`-fps_mode passthrough`) was already in the tree; this
  was the one reading that was outstanding.
- **A VFR clip can be one frame interval per segment short of its nominal window**, its
  `span_ms` over-estimates footage on disk by the same amount, and a frame landing across a
  segment boundary can step the concatenated timeline back by ≤ ~50 ms. All three are
  measured in §1; before the fix the clip was exactly nominal and its footage was seconds
  stale.
- ~~**Criteria 1 and 6 fail on 4K hardware.**~~ **Re-measured on the Windows test machine, 2026-09-25 (§13),
  and they now part company.** Criterion 1 **passes**: the pipeline sustains **`fps=56.9/60`**
  at 3840x2160 with `h264_nvenc` and drops 8 frames of 10,525 — against ~24 fps at 30 with 1183 of
  1814 frames submitted dropped (65%)
  when the issue was filed. Criterion 6 **failed on this path** — **88.9% of one core**
  for the same work at 2.4x the frame rate (1.49 cpu-percent per fps, against 2.1 before — better
  per frame, higher in total) — and the diagnosis was right: the cost is memory bandwidth, not
  arithmetic, 3840x2160x4 bytes at 57 fps being ~2.1 GB/s moved twice (GPU to staging, staging to
  the pipe), which is memcpy speed. ~~**No code change fixes that except moving fewer bytes**~~ —
  **the change that stopped moving them at all is the answer, and it shipped: §15 measures the
  zero-copy path at 3.1–4.0% of one core with `capture_copy=0.6–0.7%`, and it is the default.**
  The other route named here, capturing at the output size, was never needed: the WGC frame pool
  is created at the capture item's size, so it is not reachable from configuration. The criterion
  is left absolute, as the owner decided; §15 is the honest reading of it now.
- **The pacer already skips before the readback**, which both issues proposed as a fix: the
  pump consults the pacer first and only calls `next_frame` when a frame is due. On this
  display it has little left to win — `skipped=204` against `frames=10525` (§13) — because the
  panel delivers ~60 fps, not the 144 Hz the issue assumed, so almost no frame is read back
  and then thrown away.
- **Single-monitor capture is what was ever measured.** Monitor selection *is* wired: `[capture]
  monitor` is read by the recorder (`crates/recorder/src/config.rs`) and by the desktop shell
  (`SettingEdit::CaptureMonitor`), the settings panel exposes it, and
  `crates/capture/src/target.rs::pick_monitor` refuses an index that does not exist rather than
  recording the wrong display. What has never been exercised is the path that selects a
  non-primary monitor: every recorded run in §10–§23 captured index 0, so the multi-monitor
  behaviour is unverified.
- **No macOS/Linux capture backend.** `crates/capture/src/platform.rs` selects WGC/WASAPI
  on Windows and the synthetic `StubCapture` everywhere else. Off Windows the pipeline
  captures a generated image and *proves nothing* about real capture.
- **The capture frame pool is never recreated on a display-mode change**
  (`Direct3D11CaptureFramePool::Recreate` is not called); a mid-capture resolution or
  refresh-rate change is unhandled beyond refusing a frame whose size diverged.
- **Encoder probing is a 1-frame smoke test, not a live capture.** `xtask probe` proves an
  encoder *opens* on this machine; it does not prove the live 4K path drives it.
- **Highlights-vs-markers is a product policy, not a guarantee.** Kills/deaths/objectives/
  bomb/game-end take a clip; game/round start/end are recorded as markers with no footage.
  The split is one `match` arm (`EventKind::is_highlight`) and is the first thing to revisit
  if the output is not what you want.
- **No coalescing of event bursts.** A triple kill or a bomb plus a round end inside one
  post-roll is clipped **sequentially** — one clip per highlight, each with its own
  pre-roll. Events wait in the channel; they are not de-bounced (see the Phase 4 note §3).
- **The event integrations speak hand-rolled HTTP/1.1** (`crates/events/src/wire.rs`,
  ~250 lines with its own tests) rather than a general-purpose stack. The bounds that
  matter are tested, but it is not a battle-tested client.
- **Live-source A/V clock divergence is unmeasured.** Only within-clip drift is logged; the
  divergence between the WGC video clock and the WASAPI audio clock is not instrumented
  (runbook "Known gaps").
- **The Phase 4 integrations have never been run against a game.** No League client and no
  CS2/Dota 2 client was ever contacted; the payloads are canned fixtures and the servers are
  mocks in the test process; the module docs of `crates/events` list what that leaves unproven.
- **The desktop shell has never been a background application in practice.** The tray icon,
  its menu, the icon/tooltip state changes, the close-to-hide behaviour, the notification-like
  hint and the `Ctrl+F8` keypress are all **unobserved**: no window has been opened, no
  taskbar has drawn an icon and no key has been pressed. What exists is unit-tested logic
  (`background.rs`), a trigger path that really writes a clip when it is called
  (`commands/recorder.rs`), and Windows-target compilation of the registration code. §9 lists it item
  by item, because "the tray is tested" is the kind of sentence that gets read as "the tray
  works".
- **`[app] start_with_system` has never written a registry value.** The sync's decision table
  and the `reg.exe` argument lists are unit-tested, and the spawn is compiled for Windows;
  nothing has run. The suite deliberately does not exercise the Windows path, because that
  would write the real `HKCU\...\Run` of whoever ran the tests.
- ~~**The harness is unproven on Windows.**~~ **It has run on Windows twice.**
  `cargo xtask verify` (`dda459b`) first ran on the Windows test machine on **2026-10-15**
  (§21): 26 of 30 rows passed, including the Windows sampler (`GetProcessTimes` +
  `K32GetProcessMemoryInfo`) and a real WGC capture at 3840x2160. It ran again on
  **2026-10-16** (§23), after the harness itself was corrected, and reported **28 passed, 0
  failed, 2 not performed**: `silence_detector`, which needs an endpoint that converts
  *and* is silent, and `eviction`, which buffer mode could not exercise **as that row was
  then written** — it looked for `seg-*.mp4` files on disk, and buffer mode writes none.
  That row has since been rewritten to read the ring's own eviction counter out of the
  status line (§25), so it is performable now. What is still unrun is anything that needs a
  real keypress or a real game. A not-performed row in its report is not a pass, and a pass
  on the stub path is not evidence about Windows.
- **`[audio] enabled = false` is honoured**, and this bullet is the last one here to be
  withdrawn. The game-audio track is no longer declared unconditionally: with `false` the
  encoder is given no audio input at all — no loopback listener is bound, `ffmpeg_args` emits
  neither the `tcp://` input nor `-c:a`, and no writer thread starts — so a clip and a session
  file carry one video stream and **no audio stream**. The capture side follows: `build_sources`
  never opens the WASAPI loopback, so a track nobody asked for is not even captured and thrown
  away. `Encoder::submit_audio` refuses a block at such an encoder rather than dropping it
  silently into nowhere, and the startup line says `video only: no audio track` rather than
  leaving it to be discovered in the file. One combination is refused rather than recorded —
  `enabled = false` with a microphone — by the recorder before anything is opened, and again by
  `FfmpegEncoder::spawn` as the backstop: a container's audio tracks are named by position, so
  `replayed_media::edit::audio_titles` would call a one-track recording of a voice
  `Game Audio`. The two keys beside it are fail-closed as before: `source` and `codec` each have
  one implemented value (`"loopback"`, `"aac"`) and anything else is refused by
  `Config::validate`, which names what *is* implemented rather than recording something else.
  `bitrate_kbps` still reaches ffmpeg's `-b:a` — 192 kbps for a front-end that carries no value
  (the desktop shell, which does not parse `[audio]`) — and is unused when `enabled = false`.
  **It is measured**: §17 below carries the Windows test machine run — one `h264` stream, and no audio stream at
  all.

---

## 8. The desktop shell's background half, item by item

Added by `46c0116` (the shared hotkey), `08f6770` (the shell's tray, hotkey and hide-on-close
wiring) and `5ec095d` (the window's copy of it). It exists because the GUI had **no hotkey at
all** and could not run without a window: the headline feature — press `Ctrl+F8`, get a clip —
worked in the CLI and not in the app, and closing the window ended the recording.

Every row below is what is actually established. **No window was opened, no taskbar drew an
icon, and no key was pressed while any of it was written**, and nothing here may synthesise
input (the trigger is always an in-process call).

| Item | Level | Evidence / what it would take |
|---|---|---|
| The chord is parsed once and normalised for display (`Ctrl+F8`) | Verified on the dev host | `crates/events/src/hotkey.rs` tests: parse, display round trip, a chord with no main key rejected |
| A `RegisterHotKey` failure is returned to the caller, naming the chord, instead of being swallowed | Type-checked only | `cargo check -p replayed-events --target x86_64-pc-windows-msvc`. Registering a chord needs a Windows message queue, so no test has ever produced either outcome on a real machine |
| The GUI installs the hotkey from `[hotkeys] clip` and a press takes a clip through `RecorderHost::clip_now` — the same call the button and the CLI make | Verified on the dev host | `background::install_hotkey` (the suite exercises the "no global hotkey off Windows" branch) and `commands/recorder.rs::one_hotkey_press_writes_exactly_one_clip_through_the_recorder`, which drives a **real** recording end to end and asserts exactly one clip file, one index row and one clip in the engine's counter |
| **The keypress itself** — that pressing the chord reaches that listener on a real machine | **Unverified** | Needs Windows and a keyboard. `RegisterHotKey` + `GetMessageW` is the CLI's own path (and the CLI's hotkey was exercised interactively in the 2026-09-23 session), but the desktop app's wiring of it has never run |
| The tray's state: icon choice, tooltip, and the labels that follow the setting | Verified on the dev host (as data) | `background.rs` tests over `TrayView` / `MenuAction`; the three icons are decoded by a test (`the_tray_icons_are_decodable_pngs_of_one_size`) |
| **The tray existing at all** — an icon on a taskbar, a menu that opens, a click that dispatches | **Unverified** | Needs a desktop. The wiring is Tauri's `TrayIconBuilder` and a menu built from `MenuAction::ALL`; the ids and the action → call mapping are tested, the rendering is not |
| Close hides the window and never quits, so a recording survives | Verified on the dev host (the rule) / **Unverified** (the behaviour) | `background::close_action()` is pinned by a test; that the window hides and the process lives needs a window |
| Recording continues while the window is hidden | **Unverified in the GUI**; structurally true | nothing in the recording path reads the window (the engine runs on its own threads and the window is a reader), and the CLI's headless runs are the same engine — but the GUI's own case needs a window to hide |
| Quit from the tray flushes the encoder first | Verified on the dev host (the order) | `dispatch(MenuAction::Quit)` → `stop_recording`, then `quit`, asserted against a recording fake |
| `[hotkeys]`/`[app]` config defaults (`Ctrl+F8`, `start_with_system = false`) and the example file carrying them | Verified on the dev host | `config.rs` tests, including one that reads `config.example.toml`'s text |
| The autostart decision (`enable` / `repair` / `remove` / `leave alone`) and the `reg.exe` argument shapes | Verified on the dev host | `background::sync_autostart` against a fake `reg.exe`, plus each of the three argv shapes |
| **The registry write itself** | **Unverified** | Needs Windows. The suite must not write a real `HKCU\...\Run` value, so `apply_autostart`'s Windows arm is compiled and never executed |
| The window's copy of all of this: the hotkey line, the config path, the close hint | Verified on the dev host (headless pixels) | `apps/desktop/src/lib/recording.test.ts`, and the screenshot states `09-recording` and `11-hotkey-not-installed` |

**The collision decision, stated once.** `RegisterHotKey` registers a chord system-wide, so
the CLI and the desktop app pointed at the same `config.toml` **cannot both hold it**. Nothing
coalesces them and nothing double-fires: the second one to start fails to register and is told
so — the app in its window, its tooltip and its log (and it stays usable, because the button
and the tray still work), the CLI by refusing to start at all (a buffer run whose hotkey does
nothing is not worth starting). The alternative — a lock file or an IPC handshake — would add
a second failure mode to a mechanism the OS already arbitrates.

---

## 9. What to run on Windows next

The smallest ordered set of runs that moves the most items from *type-checked* to
*verified*, from cheapest to most informative. Each is a single command plus an
observation; the Phase 1 runbook
(`docs/runbooks/phase-1-verification.md`) already contains the PASS/FAIL detail.

> **Most of this is now one command.** `cargo xtask verify` (`dda459b`) runs steps 1–7 and
> writes `target\verify\report.md`: the probe, a bounded buffer run with the CPU/RSS
> window, the media-vs-real-time ratio from `span=`, the cap and eviction, the clip taken
> through the CLI's own `--self-test-clip-after` trigger (**no synthetic input anywhere**),
> the ffprobe of that clip, and criterion 7's fail-loud path. Run it from a remote shell
> with `scripts/verify-in-interactive-session.ps1`, which puts it in the logged-on user's
> desktop session through a one-shot scheduled task and cleans up afterwards.
>
> Two caveats, both load-bearing. First, **the harness has only ever run on Windows
> twice** — on 2026-10-15 (§21) and again on 2026-10-16 (§23), the second time after the
> harness itself was corrected — so a fresh run is still checking the harness as much as
> the app: start with the report's `capture_started` row (a missing WGC line, or
> `1280x720` geometry, means the session had no desktop and the rest of the report is
> meaningless), then read the rows marked *not performed*. One of them was still *not
> performed* after both runs: `silence_detector`, which needs a machine whose default
> playback device is not 48 kHz. The other, `eviction`, was unperformable only because the
> row was written against the old disk ring; it now reads the ring's eviction counter and
> is performable (§25). Second, **it does not
> cover step 8 (the GUI) or step 9 (a real game) at all**, nor a real keypress, nor
> lip-sync by ear; those stay manual.
>
> The steps below remain the source of truth — and every one of them is worth running by
> hand at least once, because a report's green cell is not the same thing as a person
> having watched the clip and heard the audio.

1. **`cargo xtask probe`** — confirms which hardware encoders this machine can actually open.
   Cheap, no capture. Confirms the encoder half of the path.
2. **`cargo run --release -p replayed-cli -- buffer`** on the 4K display for ~40 s, then
   read the debug line. Look for: **`skipped=` non-zero** (the readback skip, `dd921b3`)
   and **`dropped=` falling to zero** with **`fps=` reaching the configured rate** — this is
   the *only* thing that can neither be tested nor type-checked for the readback skip. Also
   re-measures **criteria 1 and 6** (issue #1).
3. **Take one clip and `ffprobe` it** — re-confirms criteria 3/4/5/8 on the current
   build. Use the self-test trigger rather than a scripted keypress:
   `cargo run --release -p replayed-cli -- buffer --self-test-clip-after 45` (it calls the
   hotkey's own `clip_now()`; no synthetic input), then press `Ctrl+F8` once by hand, when
   you are at the keyboard, to confirm the global hotkey itself fires. (The black-frame
   defect is already resolved — see §2 — so this is a regression check, not the dispute it
   once was.)
4. **Repeat run 2 at 1080p.** issue #1 asks explicitly whether the criteria pass at 1080p;
   that separates "4K is hard" from "the pipeline is slow", and it is the measurement the
   acceptance thresholds were actually written for.
5. ~~Set a low `scratch_cap_bytes` and soak 5 minutes~~ — **the key no longer exists, and
   the criterion has been rewritten around the bound that does.** Criterion 2 measured
   eviction from a bounded *disk* ring; a replay buffer holds its footage in RAM now and
   evicts against `buffer.ram_cap_bytes`, which the ring's own tests cover exhaustively.
   The harness's rows were rewritten on 2026-10-19 to match: the cap row is now `ram_cap`
   ("the ring stayed inside its RAM cap for the whole run") and the eviction row reads the
   ring's `evicted=` counter out of the status line rather than looking for segment files
   that buffer mode never writes (§25). What remains checkable on real hardware is the RAM
   budget under a long soak: watch that the reported `bytes=` settles rather than climbing
   (§12), and that `evicted=` climbs once the budget is reached.
6. **Force a `vendor` that is not present** — confirms criterion 7's fail-loud path on the
   current build.
7. **Set the Windows default playback device to 44.1 kHz or 96 kHz and record a clip** —
   exercises the WASAPI autoconversion (`5a3dce8`) and the silence detector (`4ce2c72`).
   **Half done** (§10.2): the autoconversion *has* now run — the Windows test machine's default render endpoint
   reported `native_sample_format=F32` and the backend logged `converting=true`, and the clip
   that came out had audio in it. What has still never run anywhere is the **detector
   firing**: that needs an endpoint that converts *and* is silent, which a live machine does
   not offer.
8. **Launch the desktop GUI on Windows and press Start / Save clip** — the entire GUI
   recording path (`912a026`, `f66e52e`) has only ever run headlessly.
9. **Play one bot game (League) and one CS2 round** — the last step for the Phase 4
   integrations, and the only one that can validate the payload shapes, the TLS handshake
   with Riot's certificate, and the GSI token echo.
10. **Start the desktop app and press `Ctrl+F8` with the window hidden.** Start recording from
    the window, close the window (it must *hide*, and the tray icon must still be there),
    press `Ctrl+F8`, then reopen from the tray: one clip must appear in the list with the
    window never having been on screen. This is the headline behaviour and no test can reach
    it — see §8. Read the log line `the clip hotkey Ctrl+F8 is installed` first; if it says
    `NOT installed` instead, the chord was taken and the reason is in that line.
11. **Run the CLI and the app against the same `config.toml` at once.** Whichever starts
    second must report that the chord could not be registered — the app in its window and its
    tooltip, the CLI by refusing to buffer — and exactly one of them must be able to take a
    clip. A silent second success would mean the collision detection is not wired.
12. **Check the tray's own items**: show/hide the window, start and stop recording from the
    menu, save a clip from the menu, open the config file (it must reveal `config.toml` in
    Explorer, or its folder when the file does not exist), and quit — quitting while
    recording must flush the encoder (the clip written before the quit must be complete).
13. **Set `[app] start_with_system = true`, restart the app, and check `reg query
    HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v replayed`.** Then set it back
    to `false`, restart, and check the value is gone and nothing else in the Run key was
    touched. Nothing has ever written that value.

---

## 10. The 2026-09-24 Windows session — the suite, run on Windows

The first time this project's tests were **executed** on Windows rather than type-checked for
it. Method: `git archive HEAD | tar -xz` on the Windows test machine (in a scratch directory; it has
no `git`), then `cargo test`. The toolchain is cargo/rustc 1.98.1 and the *system* ffmpeg
9.0.2 — the same ffmpeg version as the dev host, which matters for reading §10.4 below.

### 10.1 What ran, and what it proved

`cargo test --workspace --features replayed-encoder/test-encoders --no-fail-fast`, in a remote-shell
session (session 0, no desktop):

| Crate | Result |
|---|---|
| `replayed-capture` | 27 passed |
| `replayed-encoder` | 22 + 3 + 2 passed; **1 failed** (§10.4) |
| `replayed-events` | 151 + 8 + 10 + 11 + 4 passed, plus doc-tests |
| `replayed-media` | 34 + 2 passed; **1 failed** (§10.3), now fixed |
| `replayed-recorder` | 58 passed |
| `replayed-replay` | 22 + 1 passed |
| `replayed-store` | 59 + 5 + 12 passed |
| `replayed-cli` | 18 + 1 + 2 + 2 + 3 passed; the capture test refused (§10.2) |
| `xtask` | 44 passed, 1 ignored |

Two things this settles that no amount of type-checking could:

* **rusqlite's bundled SQLite compiles and runs** on this toolchain, so the whole store and
  recorder suite executes on Windows.
* **The egress guards pass on Windows** — `only_the_loopback_modules_talk_to_a_socket_at_all`
  and `no_http_client_is_constructed_outside_the_loopback_module` — which is the platform whose
  network stack they exist to guard.

### 10.2 The real capture, in a real desktop session

Run under `schtasks /create … /it` so it executed in session 1 (`running as the console user in session 1`,
`explorer running (a real desktop): True`), with the **shipping** build — no test-encoder
features, so no CPU software fallback exists:

```
WASAPI loopback capture started on the default render endpoint
  native_sample_rate=48000 native_channels=2 native_sample_format=F32
  requested_sample_rate=48000 requested_channels=2 requested_sample_format=s16 converting=true
clip clip-1790250224: video 7000ms  audio 6997ms  drift 86ms
wrote …\clip-1790250224.mp4 (7083ms, 4782052 bytes, encoder=h264_nvenc)
indexed clip #1 (media t=2000ms, 7083ms, 4782052 bytes, h264_nvenc)
recorded bookmark event #1 at media t=7000ms against clip #Some(1)
buffer session #1 closed: 9 segment(s), 7114374 bytes in the scratch ring, no session file
→ ffprobe: 3 streams — video, audio, audio
```

Read that against the ledger above. It moves, in one run: **WGC capture** on a real display,
**WASAPI loopback** including the engine-side sample-rate conversion (`converting=true`),
**hardware-encoder selection in the shipping build** (`h264_nvenc` chosen by `vendor = "auto"`
on the Windows test machine's own GPU — six `type-checked only` rows in §1 turn on this alone), the **microphone
track surviving the lossless concat** (two audio streams in the clip — the defect §10.3 fixed,
now confirmed on hardware), and the **bookmark event** a manual clip records, linked to its
session.

Two caveats, stated rather than glossed:

* **The clip's audio tracks were unnamed** — three streams came back as `video, audio, audio`
  with `stream_tags=name` **empty**, so a player showed two anonymous "Audio" tracks with
  nothing to say which was the game and which the microphone. **Fixed**: the cause was that a
  `-c copy` through the concat demuxer does not carry per-stream metadata, measured with and
  without `-map_metadata 0`, so the concat now re-applies the names it knows
  (`replayed_media::edit::audio_titles`, which the encoder also reads them from). The
  regression test is `crates/media/tests/lossless.rs::concat_keeps_the_audio_track_names_the_encoder_wrote`,
  and removing the fix makes it fail with `[None, None]`.
* **The pipeline could not hold 30fps at 4K** and said so itself:
  `only 28.0 frames per second are reaching it, and its queue is dropping the rest (2 in total)`
  — the engine's own warning, doing exactly what it is written to do. It is the issue #1
  finding reproduced on a *current* build, at 30fps rather than 60. **Still open.**

### 10.3 Three test defects, found and fixed

All three are the same species as the CI flakiness fixed earlier the same week — an assertion
that encodes a fact about the **host** rather than a property of the code — but across the
platform instead of across load. Each is committed with its reasoning.

| Test | What it asserted | What Windows did |
|---|---|---|
| `hotkey::tests::listening_off_windows_reports_no_failure_and_never_fires` | `listen(chord).expect(…)` — not gated to non-Windows, though its name and doc say that is what it is about | `listen` really tried `RegisterHotKey`, which failed (`0x800705B3`) — so the assertion was about the session, not the code. Now gated, with a Windows counterpart asserting the failure is *reported* and names the chord |
| `lol::client::tests::a_refused_connection…` | that a closed loopback port yields `FetchError::Connection` | port 1 **timed out** instead of being refused. The product was already right (`is_no_game()` accepts both); the test was over-specific about which the OS produces |
| `media::binaries::tests::explicit_path_wins` | the literal path `/custom/dir/ffmpeg` | `/custom/dir\ffmpeg.exe` — a real and legitimate difference, since a bare `ffmpeg` is not an executable there |

### 10.4 The Windows audio timeline: a queue too short for one stall (found 2026-09-24, fixed 2026-10-15)

`crates/encoder/tests/timeline.rs` failed on Windows on the assertion that the audio and video
streams agree:

```
46 frames over 3.000228s: 4 segments, video 2934ms, audio 2023ms, fed audio 144480 frames = 3010ms
the two timelines must agree to within the muxer's granularity: video 2934ms, audio 2023ms
```

Per segment, from the sibling test on the same machine:

```
seg-000000.mp4: video 1000ms, audio 896ms
seg-000001.mp4: video 1000ms, audio 560ms
seg-000002.mp4: video 1000ms, audio   0ms
seg-000003.mp4: video   67ms, audio   0ms
```

The audio stopped about 1.5s into a 3s feed while 3010ms was *fed*. Two things were known, and
both ruled out the obvious answers: the Windows test machine's ffmpeg is 9.0.2, the same version as the dev host,
so it was not an ffmpeg difference; and **the real capture run in §10.2 measured `video 7000ms
audio 6997ms`**, 3ms apart, so it was not general to the Windows product. The assertion was
**left failing rather than widened**, which was right — it was guarding a real property, and the
property was broken.

**The audio was really being lost, by the encoder's own queue.** The test feeds the pipeline for
three seconds of wall clock, so both sides of it can be counted. Instrumenting the feed loop
(temporary, reverted) showed the producer was innocent: 11.6M iterations of the loop, one 10 ms
audio block drained per iteration, never a burst, no iteration whose video wait exceeded 50 ms,
`dropped video frames 0`. The loss was on the consumer side, in the encoder's own bounded queue:
**76 dropped audio blocks**, 760 ms. Offsetting each drop against the clock put them in **one
window, 1.380-2.134s of the 3.000s feed** — a single ~0.75 s stall that begins just after the
first segment boundary — and the arithmetic closes:

```
3010 ms fed - 760 ms dropped by the queue - ~207 ms still in flight at the end = 2043 ms muxed
```

**The fix is that queue's slack.** `AUDIO_QUEUE_BLOCKS` was 32 blocks, 320 ms, sized for a child
that had *stopped reading* — with the drop path as the answer. That is the right bargain for a
permanently wedged encoder (a hole in the sound beats a capture loop blocked behind it) and the
wrong one for a pause the child comes out of on its own a second later: at 320 ms of slack, one
transient stall discarded a quarter of the recording, permanently. It is now **300 blocks, three
seconds, ~576 KB**, with the comment above it saying why — the video queue is four frames and
cannot bend (33.2 MB each), the audio queue is a rounding error beside it, and the microphone's
queue is sized the same way because it carries the same payload to the same child.

Verified on the Windows test machine at `e4d4098` plus this one constant, unmodified test, three consecutive runs,
`test result: ok. 3 passed; 0 failed` each time:

```
46 frames (15fps nominal, 30fps configured) over 3.0005s: 3 segments, video 2901ms, audio 2803ms, fed audio 144480 frames = 3010ms
```

The gap between the streams is **98 ms** against the assertion's 700 ms, and it does not move
run to run: the failing bound now has ~600 ms of headroom rather than sitting on the edge. The
audio is 207 ms short of what was fed, which is the tail arriving after the last video frame that
no segment boundary can mux — the test's own comment predicts exactly that. The siblings on the
same runs: the starved pipeline `0 video + 0 audio dropped by the queues` with `media/wall =
0.984-0.985`, and the splice test `4 segments spliced into 320x180 3067ms of video (46 coded
frames), 3031ms of audio; 46 frames decoded; 0 repeated, 0 backwards`.

The stall itself has only ever been measured on the synthetic feed; §10.2's real capture did not
reproduce it. So this is measured insurance on a real path — the queue that carries loopback
audio is this queue — rather than the fix for a shipped symptom, and it is written down that way
on purpose. **The bound did not move. What was fixed was the behaviour it bounds.**

### 10.5 What this session did **not** run

The list is shorter than it was, and every remaining item needs something a script cannot
supply:

* **The desktop GUI.** No window was opened on the Windows test machine *in this session*. (Superseded: the app
  was run on the Windows test machine on 2026-09-26, see the correction in the headline; the tray, the keypress
  and hide-on-close remain unobserved, and §8 stands for those.)
* **`cargo xtask verify`** (`dda459b`): never run on Windows *as of this session* (it first ran on 2026-10-15, §21). §10 is not a substitute — it ran
  the *tests*, not the acceptance harness, and the harness's own report is still the thing to
  read.
* **A real game.** Both Phase 4 integrations remain unrun against one.
* **The silence detector *firing*** (`4ce2c72`): the converted-loopback path executed in §10.2
  (`converting=true`), but the detector did not trip — a live microphone was connected.
* **The pinned sidecar** (`binaries/ffmpeg.exe`): §10.2 used the Windows test machine's *system* ffmpeg.
* **A synthesised keypress.** Never, by policy: input synthesis is exactly what must not happen
  on a machine running an anti-cheat, and a test is not a reason to make an exception. The
  hotkey's *failure* path is now verified (§10.3); its *success* path needs a human at the
  keyboard.

### 10.6 One consequence worth knowing

`replayed-cli` **exits** when the clip hotkey cannot be registered — deliberate, and the
comment explains why (a buffer run whose chord does nothing is not worth starting). The
2026-09-24 session showed what that means in practice: **the CLI cannot run at all from a remote shell,
as a service, or on a headless box**, and neither can `--mode session`, which records
continuously and needs no hotkey. Kept as designed, by the owner's decision; recorded here
because "the CLI refuses to start headlessly" is the kind of thing that belongs in a ledger
rather than in a bug report later.

### 10.7 The encoder's tests had not built on Windows for three weeks

Found on 2026-10-15, trying to run the whole crate after the §10.4 fix: **`cargo test -p
replayed-encoder` did not compile for `x86_64-pc-windows-msvc` at all.** Three errors, in two
test modules:

| site | what it was | introduced by |
|---|---|---|
| `crates/encoder/src/mft.rs:1149` | the test called `probe_hardware_encoders()`, a function that had grown a `VideoCodec` parameter | `3f2fe49` *Make the hardware path codec-aware, so HEVC can take it* |
| `crates/encoder/src/ffmpeg.rs:1890,1895` | `a_dead_writer_reports_ffmpegs_exit_status_and_stderr` used `std::os::unix::fs::PermissionsExt` and a `#!/bin/sh` stub with no `#[cfg(unix)]` gate | `6e63977` *smoke-test hardware encoders so an advertised-but-unusable one fails loudly* |

Neither was visible from anywhere that runs. The Windows test machine's Windows sessions since then named a test
file (`--test timeline`), which builds only that target; and this repository's own Windows gate
is a `cargo check` of the *libraries*, which never compiles `#[cfg(test)]` code:

```
cargo check -p replayed-capture -p replayed-encoder -p replayed-events \
            -p replayed-replay -p replayed-media -p xtask --target x86_64-pc-windows-msvc
```

So the platform the product ships on could not run the crate's tests, and had not been able to
since 2026-09-25 — the date of both commits above, both after the 2026-09-24 Windows session that
had recorded the encoder's counts. It matters beyond tidiness: the §10.4 failure was read for
three weeks as "one assertion in the timeline target", and the timeline target was the only one
anybody *could* run there.

Both sites are fixed rather than deleted. The `mft.rs` test is the one that asks the machine what
hardware encoders it has, which is what `crates/encoder/src/mft.rs` exists for; the two
`ffmpeg.rs` tests are the dead-writer and bounded-shutdown defects, whose stand-in children are
POSIX scripts, so each gets the `#[cfg(unix)]` gate and a line saying why. The `ed8ce59` edit
that added the shutdown test had also stranded the dead-writer test's doc comment above the new
test, so the doc went back to the function it describes.

**And the gate now looks at the tests.** The CI cross-check step gained `--tests` and
`--features replayed-encoder/test-encoders` (the feature the workspace suite uses: without it
`EncodeConfig::for_tests_software` does not exist and `crates/encoder/tests/stream_output.rs`
cannot be checked either). Verified locally against `x86_64-pc-windows-msvc`: the whole
cross-check list now compiles with its tests, and the encoder's library-only check still reports
only its pre-existing unused-import warning. `replayed-media` and `replayed-events` already
gated their own unix-only test code properly, so the encoder was the whole of it.

On the Windows test machine the whole crate is green and has run there for the first time since 2026-09-25:
`30 + 3 + 2 + 3 + 3 passed, 0 failed`, exit 0. That includes `mft.rs`'s own test, which is the
one this defect silenced — and it is the test that asks the machine what hardware encoders it
has, so its answer is worth recording: **ten MFTs**, of which the **NVIDIA H.264 and HEVC
encoders accept the D3D11 device manager** (`d3d11=true`) and the Intel Quick Sync ones, which
enumerate the same encoders, do not.

---

## 11. The CI gate, run by hand (2026-09-24)

CI's runner quota is exhausted (§5), so the workflow's steps were run locally, in the
workflow's own order and with its own `working-directory` for each — including the one that
runs from `apps/desktop/src-tauri` rather than the repo root, which is easy to get wrong and
silently runs the wrong tests if you do.

**On the revision this was run at.** An earlier version of this section named the commit
`a8a8b9b`. That hash resolves to nothing in this history: the run was reproduced from the
workflow's own steps, against the tree of that day, and the tree no longer contains the
revision it was run at. The commit that recorded the run is `70d71cf` (*docs: CI's red is
exhausted runner minutes, not the tree — and run its gate by hand*).

| CI step | Where CI runs it | Reproduced locally |
|---|---|---|
| Rust workspace tests | repo root | **516 passed, 0 failed, 1 ignored** |
| Windows cross-check (`capture`, `encoder`, `events`, `replay`, `media`, `xtask`) | repo root | `Finished` — all six type-check for `x86_64-pc-windows-msvc` |
| Frontend — `npm ci` | `apps/desktop` | clean install |
| Frontend — `svelte-check` | `apps/desktop` | **0 errors, 0 warnings** |
| Frontend — `vitest` | `apps/desktop` | **134 passed** |
| Frontend — production build | `apps/desktop` | `✓ built` |
| Desktop shell Rust tests | `apps/desktop/src-tauri` | **112 passed** |
| Clippy | repo root | only the pre-existing warnings: the `unwrap()`-in-tests lint and one `too many arguments (8/7)` in `EncodeConfig::hardware` |

What this is worth: the same gates, the same results, and **not** a substitute for CI in one
respect — it ran on the machine that wrote the code, so it is exactly as good as the dev host
is representative. The Windows cross-check is a `check`, not a run, either way. What it does
mean is that nothing in this tree is currently waiting on a gate nobody has exercised.

---

## 12. In-memory replay buffering — landed, and what the first attempt actually got wrong

A replay buffer that writes **nothing** to the SSD while it is only buffering, and touches the
disk only when a clip is saved. `-f segment` used to write a file per second into a scratch
directory and a filesystem ledger deleted the oldest; that write churn is gone in buffer mode.

### What landed, and what each piece is worth

| Piece | Where | Evidence level |
|---|---|---|
| `EncodeConfig::output` / `EncodeOutput::FragmentedStream`; `.stdout(Stdio::piped())`; `Encoder::take_output_stream()` | `crates/encoder/src/ffmpeg.rs` | **Verified on the dev host** — the element-for-element args test still passes, and 3 tests drive a real ffmpeg |
| The stream mode writes no files | `crates/encoder/tests/stream_output.rs` | **Verified** — `the_stream_mode_writes_nothing_to_disk`, monotonic fragment timestamps, and a remux into a playable clip |
| `FragmentSplitter` — fragmented MP4 → `Fragment { seq, start_ms, duration_ms, keyframe, bytes }` | `crates/media/src/fragments.rs` | **Verified on the dev host** — 5 tests against a real ffmpeg stream, including byte-at-a-time equivalence |
| `MemoryRingBuffer` — push, byte/duration eviction, window selection, `assemble` | `crates/replay/src/ram_buffer.rs` | **Verified on the dev host** — 8 tests |
| `ClipSplicer::splice_from_memory` / `splice_stream` — `-i pipe:0 -c copy`, audio titles re-applied | `crates/replay/src/splice.rs`, `tests/ram_clip.rs` | **Verified on the dev host** — a clip produced from a real stream, probeable, with both streams and the right names |
| The live-stream seam: the ring is parsed on the thread that owns the pipe, while frames are fed | `crates/replay/tests/ram_clip.rs` | **Verified** — `a_live_stream_is_ingested_as_it_arrives_without_stalling_the_pipeline` |
| The window contract: footage covering `[T - pre, T + post)` is selected whole | `crates/replay/tests/ram_clip.rs` | **Verified** — `a_window_inside_the_held_footage_covers_the_whole_request`, positive and negative |
| `Ledger::Memory` — buffer mode uses the in-memory ring; encoder built first because the ring is fed by its stdout | `crates/recorder/src/lib.rs`, `src/memory_ring.rs` | **Verified on the dev host** — the recorder suite runs buffer mode end to end |
| **Constraint 1**: buffering writes nothing, and a saved clip is the only thing that is written | `crates/recorder/src/tests.rs` | **Verified** — `buffering_writes_nothing_to_disk_and_only_a_saved_clip_does` |
| `buffer.ram_cap_bytes`, defaulting to 256 MiB, and the RAM budget that replaced the scratch cap as the engine's check | `crates/recorder/src/config.rs`, `config.example.toml` | **Verified on the dev host** — the default, the example, and a pre-existing config without the key |

Full workspace suite on the dev host: **535 passed, 0 failed, 1 ignored**. The desktop shell's
separate crate graph: **112 passed, 0 failed**. The Windows cross-check (`media`, `replay`,
`encoder`, `capture`, `events`) is clean, and the Windows *run* is its own section below. Clippy on
the touched crates adds no warning that was not already there.

### The thing the first attempt got wrong, because it is the useful part

The first attempt wired the recorder, saw a **3000ms clip request come back as 2000ms**, and was
reverted on the belief that the reader thread was stalling. Two things were wrong with that, and
both are worth keeping:

1. **There was no stall.** The status line read `fps: 9.99, configured_fps: 10,
   effective_fps: 10`, and `fps` is *measured from frames actually submitted* — positive evidence
   the pipeline ran at its configured rate. The two `frames` readings that looked like a freeze
   were ~200ms apart, not the two seconds the surrounding test wording suggested. A stall is a
   number you can read off the pacer, and this one was not there. The claim sat in this ledger as
   fact until the next pass re-read the evidence instead of its own summary of it.

2. **The short clip was a test defect, and the test was passing for the wrong reason.** The wait
   before the trigger was `s.frames > 0 && s.span_ms > 0` — "the buffer is non-empty" — while the
   assertion later in the same test needs `pre + post` = 3000ms of footage to exist. The file
   ledger's span **lags a whole segment behind** (its newest file is untrusted until a later one
   appears), so "the span has moved" meant two segments had been written: enough for the window by
   accident. A fragment in the in-memory ring carries its own length, so that ring's span is
   *exact*, the same wait returned after one segment, and the window resolved to
   `[0ms, 2000ms)` for a trigger at 1000ms with a 2000ms pre-roll. The clip was correct for the
   footage that existed; the footage did not exist yet.

   The fix was to the **wait**, not the assertion: ask for the window, which is what was meant and
   is right for both rings instead of relying on one of them being slow to count. The instrument
   that made this legible was four numbers logged at the splice — `trigger`, the window, the
   selected span and the ring's held range:

   ```
   clip from RAM: trigger=1000ms window=[0ms, 2000ms) selected=2000ms held=2000ms footage=[0ms, 2000ms]
   ```

   A pass had already been spent inferring this from surrounding test output. It is cheap to
   just say it.

### Two parse bugs found by measurement, not by reading

Both were in `sample_span_ticks`, which reads a fragment's own sample durations out of its `moof`
so the ring knows its length on arrival instead of borrowing the next fragment's start — the
borrowed rule cost a whole fragment of span, and a clip's window is resolved against that span.

* `tfhd`'s conditional fields are positional, so `default_sample_duration` sits at an offset that
  depends on which of `base_data_offset` and `sample_description_index` are present.
* `trun`'s per-sample entries are **4 to 16 bytes wide, not 4** — a duration may be followed by a
  size, flags and a composition offset. A fixed 4-byte stride sums those into the total, which is
  how a one-second fragment measured **3576ms**.

The file ledger's rule — *a segment's end is proved by the next segment's start* — is kept as the
fallback for a sample table this build cannot read, so the failure mode is the previous behaviour
rather than a guessed length.

### It runs on Windows (2026-09-25)

The tree was shipped to the Windows test machine as an archive — it has no `git` — and extracted to a **fresh
directory** (a separate scratch location), so nothing of the owner's was overwritten, then
built and tested there natively (`cargo 1.98.1`, host `x86_64-pc-windows-msvc`). League was not
running at the time and nothing captured, displayed or injected anything: `cargo test` is CPU
only.

**The buffer-mode tests pass on Windows**, which is the claim constraint 3 needs:

| Target | Result |
|---|---|
| `replayed-recorder --lib` | **60 passed, 0 failed** — reproduced **4 consecutive times** |
| `buffering_writes_nothing_to_disk_and_only_a_saved_clip_does` | **ok** — constraint 1, on Windows |
| `a_recorder_records_produces_a_clip_and_stops_cleanly` (buffer mode, RAM ring) | **ok** |
| `a_clip_records_why_it_was_taken` | **ok** |
| every `a_full_session_*` test | **ok** — session mode unchanged |

The whole-workspace run reported three failing targets. Two of them **also fail at `0ec7142`**, the
commit before this feature existed, which was checked rather than assumed by extracting that
revision to a second directory and running the same three targets:

| Target | at `0ec7142` | at the RAM commit |
|---|---|---|
| `replayed-cli --test self_test_clip` | **FAILED** | FAILED — pre-existing (this is §10.6's hotkey defect) |
| `replayed-encoder --test timeline` | **FAILED** | FAILED — pre-existing |
| `replayed-recorder --lib` | ok. 58 passed | 1 failure in the workspace-wide run, **4/4 clean** in its own suite |

The one recorder failure was `a_watched_game_starting_and_stopping_drives_a_full_session_and_the_retention_pass`,
asserting the status had dropped its game after the session ended. It is **not** a regression from
this change, and the evidence is two measurements rather than a reading of the diff: it passes
**6/6 in isolation** and the recorder suite passes **4/4** with it included. It is a presence-watcher
timing assertion that is sensitive to how much else is running — and the workspace-wide run has the
whole tree's binaries in flight at once. This change adds a reader thread and two tests, so it
genuinely does alter the load profile, which is enough to tip a test like that without being a
defect in it. Worth fixing as its own piece of work; not evidence about the RAM buffer.

**Fixed, and it was a defect in the code rather than noise in the test.** The status kept the
game until *after* the join and the retention pass that follow a watched game's stop, while the
engine clears `running` as its own shutdown returns. `running` was therefore false with `game`
still set for as long as that work took, which is why load widened the window enough to see it
rather than being the cause. `spawn_supervisor` now clears the game *before* the shutdown, so
`running == false` implies no game — which is what `RecordingStatusDto::matched_game` already
documents ("`None` while nothing is being recorded"), and what makes the assertion deterministic
instead of a window. The test is unchanged, deliberately: it passes because the ordering it
asserts is now the ordering there is. Reproduced on the dev host by widening the window on
purpose — 600 extra expired sessions for the retention pass to evict — and polling every 200µs:
`game: Some("Dota 2")` alongside `running: false` before the change, `None` at that same instant
after it. Not re-run on Windows; the Windows test machine is where the window was first seen, and this is the
dev-host diagnosis of it.

What is **not** claimed: nothing here exercises NVENC, AMF or QSV — the tests run the software
encoder, as they do on the dev host. The hardware-encoder selection was verified on this machine in
§10, before this change, and this change does not touch it. A real capture run into a RAM buffer
on Windows is still unperformed; what ran is the test suite with stub sources.

### The decision, and what it cost: the file ledger is gone

The open question above was answered **by removing the type**, and the reasoning is worth keeping
because the alternative was defensible. Buffer mode taking RAM unconditionally — which is what "no
SSD writes while buffering" means — left `RingBuffer` reachable from nowhere. Keeping it as a
second implementation would have meant adding a config key to select it, and a capability nothing
selects is not really kept while its tests keep passing and its code keeps looking load-bearing.

What it provided that the other two modes do not: a replay buffer whose footage survives a crash.
That is genuinely redundant here rather than lost. A crash-survivable *session* is session mode,
which already exists; a crash-survivable *buffer* protects the seconds before a keypress that, by
hypothesis, never happened. The task asked for RAM buffering and forbade nothing, so this is a
judgement rather than a requirement — and it is reversible: `git log` has the type, its tests, and
this section explaining them.

The cost was not one file. Removing it meant something for every test and comment that named it,
and **three of those were tests whose subject was not the ring at all**:

| What named it | What was actually being tested | What happened |
|---|---|---|
| `apps/replayed-cli/tests/post_roll.rs` | the post-roll wait — the §10 bug where the wait starved ffmpeg | **retargeted** at the in-memory ring. Still three tests, now over the shipping path |
| `apps/replayed-cli/tests/clip_index.rs` | the index — splice, index, evict | **retargeted**. The ring was only its clip producer |
| `crates/replay/tests/end_to_end_clip.rs` | the file ring, end to end | **deleted** with its subject |
| `crates/replay/src/buffer.rs` | the ring and its ledger numbering | **deleted**; `BufferConfig` and `BufferStats` stay, both still used by the recorder |

Splitting it that way is the point worth recording: the deletion was not one decision but four, and
two of them were places where the file ring had been standing in for shared machinery. The suite
went from 538 to 535 passing — three tests fewer, each removed with the thing it tested.

**One consequence that was nearly missed.** The file ring was the only thing that *enforced*
`[buffer].scratch_cap_bytes`: it evicted to that cap on every scan, and `session.rs` deliberately
never does. With the ring gone the key had no reader at all — it bounded nothing. The note in this
section said the opposite when it was first written, that it "bounds a directory that session mode
writes into"; that was wrong, and it was corrected rather than left as a plausible sentence.

The key is now **removed** — from `BufferSection`, from `BufferConfig`, from `config.example.toml`
and from every fixture that carried it. Removing it is backward compatible: the config structs do
not `deny_unknown_fields`, so a `config.toml` still carrying the key keeps parsing and the key is
ignored, exactly as it was when it did nothing. What bounds disk usage in both modes is the
`[storage]` policy, which evicts whole clips and sessions.

Removing it also dissolved a test rather than deleting one. `a_full_session_records_into_its_own_
directory_and_ignores_the_scratch_cap` stated its subject through the cap — set to 1 KiB so that
enforcing it would visibly gut the session. With no cap to ignore, the property it was standing in
for is stated directly and the test is now
`..._and_keeps_every_segment`, asserting that four seconds of 1s segments are all still there. That
is the real difference between a session and a rolling buffer, and it is better said than implied.

---

## 13. The 4K soak on the Windows test machine (2026-09-25) — the measurement issues #1 and #2 were waiting for

Issue #2's own closing comment says the one thing that would close it is "the Windows test machine's own ratio, taken
with the fix in place" and that no number from the Windows test machine had been taken. Issue #1 says the same about
`dropped=` / `fps=` / `configured=`. Both were taken in a single run.

### How it was run, so it can be repeated

`schtasks /create … /it` so the CLI ran in **session 1** (`Logon Mode: Interactive only`, with
`explorer` running), from the **release** build of the current tree, against
`%LOCALAPPDATA%\replayed\config.toml` — a copy of the shipped `config.example.toml`, so the fps,
output size and window are what a fresh install would use. `RUST_LOG=debug`, 180 s of capture, and
the CLI's own `--self-test-clip-after 180` to end it. A running League client or game aborts the run
before it starts: this captures the screen, which is not done while a protected game is up. Measured
**3840x2160** throughout, `h264_nvenc`, `WASAPI` loopback for audio.

### The numbers

| Reading | Issue #1/#2's measurement | This run |
|---|---|---|
| Sustained rate | ~24 fps against 30 configured | **`fps=56.9/60`** — 95% of a rate that is 2x higher |
| Dropped frames | 1183 of 1814 submitted (65%) | **8 of 10,525** — 0.076% |
| `media=` vs `wall=` at the trigger | divergence of unknown direction | **`media=180266ms wall=180372ms (drift 106ms)`** — 0.06% over 180 s |
| Post-roll reached | every press timed out before the trigger fix | **`span=185416ms covers 185266ms`**, 5133 ms after the press |
| Clip | — | **36165 ms, 84,897,902 bytes**, video+audio, splice drift **49 ms**, indexed |
| The probe's accuracy at 4K with NVENC | "**unverified**" | **`82.2fps sustainable at 3840x2160 with h264_nvenc`** |
| CPU | 50.8% of one core at ~24 fps | **88.9% of one core at ~57 fps** |
| RSS | 235 MB peak 267 MB | **202 MB peak 271 MB** |

### What this closes, and what it does not

**Closed: the timeline half of issue #2.** The media clock is the wall clock to 0.06% over three
minutes, so `pre_seconds` now means real seconds. That was the outstanding question; the mechanism
(`-fps_mode passthrough`) was already in the tree and is not what this run tests.

**Closed: issue #1's headline and its drop counter.** "4K capture cannot sustain 30 fps" is no
longer true — it sustains **57 of a declared 60** at 3840x2160, and drops 0.076% rather than 65%.
The issue's suggested fix for the wasted readback is also **already implemented**: `pump_once`
consults the pacer *first* and only calls `next_frame` when a frame is due, calling
`discard_pending` otherwise. This run shows why that change has little left to win here —
`skipped=204` against `frames=10525`, because this display delivers ~60 fps and not the 144 Hz the
issue assumed, so there are almost no frames being read back and thrown away.

**Not closed at this point: criterion 6** (< 5% of one core) — **closed since, by the zero-copy path
(§15)**; this paragraph is left as written because it is the honest reading of *this* path, the
ffmpeg-sidecar one. It is *worse* in absolute terms than the issue's figure — 88.9% against 50.8% —
because the run is doing 2.4x the frames per second. Per frame it is better (1.49 against 2.1
cpu-percent per fps), which is what the timeline fix should look like: the same work, more of it, and
no longer thrown away. The cost is memory bandwidth, not CPU arithmetic: 3840x2160x4 bytes at 57 fps
is ~2.1 GB/s moved twice (GPU to staging, staging to the pipe), which is memcpy speed. **No code
change fixes that except not moving the frame**, and there are only two ways to do that: encode on
the GPU (the design spec's Phase 2) or capture fewer pixels. **The first is what §15 then did** —
measured at **4.0% of one core with `capture_copy=0.7%` and 59.6/60 fps**, now the default wherever
a hardware encoder is present. The second turned out to be unnecessary as well as unreachable: the
WGC frame pool is created at the capture item's size, so it is not a configuration away, and §15 is
the measurement that makes it moot rather than merely unexercised.

**Not measured: 1080p**, and the reason is worth recording rather than glossing. Issue #1 asks for it
"to separate '4K is hard' from 'the pipeline is slow'", and on this machine it cannot be had by
configuration: only the primary monitor is captured, that monitor is 4K, and the pool size follows
it. Taken from session 0 the display list reports a 1024x768 virtual device, so the real geometry is
only visible inside the interactive session. A 1080p measurement needs either the display resolution
changed (invasive) or the scaled capture above (a change).

### And an independent confirmation, for free

The same run exercised the in-memory ring on real Windows hardware for the first time — §12 listed
that as unperformed. The run's own closing line:

```
buffer session #2 closed: 34 segment(s), 84335007 bytes held in RAM, no session file and nothing written while buffering
```

84 MB held in RAM, and the only file written in 180 seconds is the clip that was asked for. The log
line that reports the held bytes had been printing `bytes=0` for exactly this case, because it read
the *disk* figure from `Ledger::stats`; the status field and the log now read the same
`buffered_bytes` value once, so they cannot disagree.

---

## 14. Where the 4K CPU actually goes (2026-09-25) — `capture=97.1% submit=0.1%`

Issue #3's remaining half is the CPU figure, and the decision it needs — whether to move the frame
out of system memory — turns on which half of the cost the readback is. A process-wide CPU
percentage cannot say: it cannot distinguish the 33.2 MB GPU-to-CPU copy per frame from time spent
waiting for ffmpeg. So the pump now measures its own two expensive calls
(`PumpCounts::capture` / `::submit`) and the periodic line reports both as a share of wall clock.

Same box, same display, same 4K/60 soak as §13, with the attribution:

```
frames=8788 segments=34 bytes=93230279 span=155649ms dropped=9 dropped_audio=0 dropped_mic=0
  skipped=193 fps=56.7/60 configured=60 capture=97.1% submit=0.1%
```

| | |
|---|---|
| Inside the capture backend (`next_frame` + `discard_pending`) | **97.1%** of wall clock |
| Inside the encoder's submit, i.e. waiting for ffmpeg | **0.1%** |

**The cost is the readback, and ffmpeg is not blocking us at all.** That is a stronger statement
than §13 could make: the 88.9%-of-one-core figure is almost entirely the copy out of the GPU, and
the encoder's own work is invisible in this loop because it happens on the other side of a queue
that never fills.

The consequence for issue #3 is that **97% of the current cost is addressable by removing the
readback alone** — the target of "under 3%" is credible rather than aspirational — and that a
change which only *reduces* the bytes (capturing at the output size, 4x fewer) leaves roughly a
quarter of the wall clock in the same place. It also settles a question §13 left open: the encoder
is not where the money is, so a native encoder that still required a CPU copy would buy almost
nothing.

What this does **not** say: this is the readback path, the shipping design, and every number above
belongs to it. It *bounds* what removing the readback can save — 97% of the loop, so the target of
"under 3%" is credible rather than aspirational — and it does so from the inside of the loop rather
than from a total. **The path it bounds has since been built and measured, and the prediction is
what came back: see §15**, where the copy falls to 0.7% of the loop and the core to 4.0%.

---

## 15. The readback is gone: the zero-copy path, and HEVC on it (2026-09-25)

§14 bounded what removing the readback could buy and could not test it, because the path did not
exist. It exists now, it is in the tree, and it is the **default wherever a hardware encoder is
present**. This is the measurement §14 asked for. It was taken on the same machine, against the same
4K/60 soak and the same example config, by the zero-copy and HEVC work recorded in §15.

What it is: the WGC texture goes to NVIDIA's H.264 Media Foundation encoder unchanged (`ARGB32`,
no colour conversion), the encoder's own thread turns it into an H.264 elementary stream, and
ffmpeg **copies** that stream instead of encoding one. No pixel of any frame crosses to the CPU,
which is the entire point — §14 measured that crossing as 97% of the loop. `encode.zero_copy` is
an `Option<bool>`: unset means *use the hardware path when the hardware is there*, `true` means
*require it and refuse to start without it*, `false` turns it off.

### The numbers, same machine, same soak

| | shipping path | hybrid, inline | hybrid, threaded |
|---|---|---|---|
| `capture_copy=` | 69.6% | 0.7% | **0.7%** |
| `CPU` | 88.9% of one core *(§13)* | 3.1% | **4.0%** |
| `fps=` | 55.5–56.9/60 | 35.0/60 | **59.6/60** |
| `dropped=` | 8 | 2288 | **0** |
| exits on its own | yes | **no** | **yes** |

The threaded run's own closing numbers:

```
CPU 4.0% of one core, RSS 278.9 MB
EXITED after 36.2s of watching
no ffmpeg left behind
frames=5710 segments=72 ... dropped=0 ... fps=59.6/60 capture=93.1% submit=0.2%
capture_wait=61.3% capture_copy=0.7%
buffer session #8 closed: 72 segment(s), 178668538 bytes held in RAM
CLIENT_EXIT=0
```

**Criterion 6 passes: 4.0% of one core against a `< 5%` target.** **Criterion 1 passes on this path
too, and more strongly than §13**: 59.6 of 60 with **zero** dropped frames, against 56.9 and 8.

Read the three columns in order, because the middle one is really two states. The inline hybrid's
first soak *never exited* — its process was still alive eight minutes after its log stopped, blocked
rather than spinning (4.6 s of CPU in ten minutes) — and that shutdown defect was found and fixed
(the three causes are below) before the threading work. Repaired, the inline run exits on its own at
**CPU 3.1%**, still at 35 fps. So the threading change is what took the rate from 35.0 to 59.6 and
the drops from 2288 to 0, and it is *also* what raised the CPU from 3.1% to 4.0% — an extra thread is
not free. The trade is worth naming rather than letting 3.1 next to 4.0 look like noise.

### The count that matters is the default's

Verified with `config.example.toml` **copied and unmodified**, which is the case a new install gets:

```
zero_copy in the config as copied:  (unset - which is the default being tested)
CPU 3.6% of one core
EXITED after 36.2s
zero-copy: captured textures go straight to the hardware encoder, and ffmpeg copies the H.264
  instead of encoding it (chosen by default, because a hardware encoder is present)
the hardware encoder is open on its own thread encoder=NVIDIA H.264 Encoder MFT format=ARGB32
  size=(3840, 2160) adapter=NVIDIA GeForce (discrete)
CLIENT_EXIT=0
```

The device on a null adapter is whichever adapter DXGI calls the default, and on a machine with an
integrated GPU that is not safe to assume — so it was read rather than assumed: adapter 0 is the NVIDIA
GPU (the Intel UHD is present and is not it, and the QSV MFTs report `d3d11=false` and cannot take
the device at all). `MftEncoder::open_for_texture` takes the device *from the captured texture*, so
the encoder and capture cannot disagree about which GPU they are on.

### The shutdown, found on the way

Three ways to wait for ever were on that path at once, and the shutdown took whichever came first:

1. **The MFT's tail, sent with a blocking `send`** — written into a block whose own comment called
   it best-effort, but the only thread that can drain that queue is the one that may be blocked
   writing to a child that has stopped reading. Now `try_send`, and a tail that does not fit is
   *reported* rather than sat on.
2. **The order.** The writers were joined *before* the child was waited for, and a writer only ends
   when its pipe breaks, which only happens when ffmpeg reads or dies — so a child that never exits
   took the whole shutdown with it, silently, with the logger already stopped. The child is now
   waited for first, under a 5 s `SHUTDOWN_BUDGET`, and joined after.
3. **`std::process::Child::wait` has no timeout.** `wait_bounded` polls `try_wait` and kills at the
   deadline, and a status that arrives only because of that kill is reported as such.

Those three were fixed and the story still was not over, because the same mistake was one line
further up. The MFT's **per-frame** handover was a blocking `send` too, and that is the one the Windows test machine
keeps hitting: five of six zero-copy runs on a later soak, two of eight after that, and two of four
in a probe built to ask. Every one of them was *after* the clip was written and indexed, so the
recording was never at risk — only the exit was, which is why it read as a soak that would not end
rather than as a clip that was wrong. Instrumented (`MFT-DIAG`) and read back, the thread was inside
that send when the join was reached, with its reader — the thread that writes to ffmpeg — stalled
behind a child that had stopped reading; and the step that could break that loose, `wait_bounded`
killing ffmpeg, sits *below* the join that was waiting. It is the tail's bug one nesting level in,
with the same consequence and none of the credit, because the tail is sent once and this is sent
once a frame.

The handover now has a deadline of its own, `HANDOVER_BUDGET` — 2 s, the budget the encoder calls in
that thread already use, because it answers the same question. It waits for room by polling
`try_send`, stops waiting the moment shutdown is asked for, and drops and counts the frame it could
not queue: `N encoded frames were dropped instead of queued: the video queue stayed full for 2s,
which is the ffmpeg writer thread not keeping up`. Four runs of the shipped hardware path, four
clean exits at ~22 s each, with that warning firing in exactly the run the old build hung on.
`c5cd703`.

The regression test asserts on **time** rather than output, because the defect was an absence, and it
is mutation-checked: raise the budget to an hour and it hangs. Suite at the time: 541, from 540
(546 in the workspace since, after later work).

### HEVC on the same path

`codec = "hevc"` with `zero_copy` left unset took the hardware path too:

```
CPU 4.2% of one core, RSS 356.9 MB
capture=94.6%  submit=0.1%  capture_wait=62.0%  capture_copy=0.6%
buffer session #13 closed: 35 segment(s), 167671496 bytes held in RAM
CLIENT_EXIT=0        no ffmpeg left behind
clip: codec_name=hevc  codec_tag_string=hvc1  3840x2160
```

`capture_copy=0.6%` is the number that says the pixels still never came back to the CPU. **It is not
a CPU or GPU saving, and reporting it as one would be wrong**: the saving came from deleting the
readback, which is codec-independent, and NVENC's HEVC pass costs slightly *more* per frame than its
H.264 one. HEVC buys quality per bit. Three things it does not yet match or do, recorded here rather
than discovered later:

- **Throughput.** 56.9 fps and 9 dropped, against the H.264 hybrid's 59.6 and 0, with the recorder
  saying so out loud (`the encoder cannot sustain the 60fps it was told`). Not a regression from
  zero-copy — `capture_copy` and the CPU are what that change moved — but the predicted cost of the
  codec, and the reason the default stays H.264.
- **~~The bitrate is ignored.~~ Fixed 2026-10-15, and it was never HEVC-specific.** The clip came out
  at 37 Mbps against a `bitrate_kbps = 20000` setting, and this section read that as HEVC-side rate
  control because the H.264 path "honours the same setting". That was an artifact of the two numbers
  being the same one: the path was told a hardcoded 20 Mbit/s and never received `bitrate_kbps`, so at
  the default 20000 an inert path and a working one look identical. Eight interleaved cases at 4K60,
  one run per build, separate them — h264 at 4000 **23,218 kbps → 4,979**, hevc at 4000 **22,261 →
  8,151**, while the ffmpeg pairs (3,935 and 17,635 before, 4,096 and 19,554 after) were already
  right. What is left is smaller and honest: the MFT aims near the setting rather than pinning it
  (1.2× at 4000 for h264, 2.0× for hevc, 1.6–1.8× at 20000) where the ffmpeg path lands within 3%,
  because `MF_MT_AVG_BITRATE` is a mean for the encoder's rate control rather than a ceiling. Making
  it exact wants ICodecAPI's rate-control mode, which is a further measurement and has not been
  taken. See `7971dc7`.
- **~~A diagnostic that hides the interesting answer.~~ Fixed 2026-10-15.** `probe_available_types`
  used to `continue` past any encoder whose output type it could not set, and that is exactly the
  encoder whose *answer* was the point: `GetOutputAvailableType` says which type it would have taken
  instead, and an MFT offers that whether or not it accepted the caller's. The refusal is now a row
  of its own, carrying the call's error, and its available output types are enumerated anyway — the
  input side is not asked, because an async MFT will not talk about its inputs until an output type
  has been accepted. Measured on the Windows test machine, old code against new, same build, same throwaway caller, at
  a size every encoder has to refuse (`probe_available_types(Hevc, (16384, 16384))`): the old report
  names the eight Intel MFTs (which cannot be activated at all) and **both NVIDIA encoders are simply
  absent from it**; the new one names them, their refusals — `Invalid type. (0xC00D36BD)` for H.264,
  `The input type is not supported for D3D device. (0xC00D6D76)` for HEVC, the same error the
  ordering probe reports — and what each would have taken: exactly two output types, one under the
  friendly subtype and one under the vendor's private GUID (`5D0CE9DD-…` for H.264, the four-CC
  `'HEVC'` GUID for the HEVC MFT), each carrying a profile and the sentinel level `0xFFFFFFFF`, and
  **no frame size, no rate and no bitrate**. That is an encoder answering with its own defaults, and
  it is the answer the pipeline's type — 4K at level 5.1 — was being refused against. The refusal
  itself is the size/state puzzle the size-ceiling probe documents (the same type is accepted on a
  ladder that *rises* to 4K rather than starting there) and is **not** what this change fixed: what
  changed is that the diagnostic now reports it instead of hiding it.

### What this does not say

- **The CPU comparison spans two sessions — and the matched pair has now been taken.** 88.9% is §13's
  earlier soak; the hybrid's 3.1% and the shipping path's *split* were taken minutes apart on the same
  build with the same instruments, but the shipping path's CPU total was never re-taken in the session
  that produced the hybrid's (the shutdown hang ended it early). The like-for-like evidence for the
  *split* is still the split instrumentation. On 2026-10-15 the totals were paired: one run of the
  released CLI per path, in one interactive session, same display, same 4K/60 capture, same
  `--self-test-clip-after 45`, sampled once a second over both processes — `[encode] zero_copy = false`
  against `true`:

  | | `zero_copy = false` (readback) | `zero_copy = true` (hybrid) |
  |---|---|---|
  | `replayed-cli` CPU | 119.37% of one core | **4.18%** |
  | its `ffmpeg` child | 156.10% | 4.07% |
  | both processes | **275.47%** | **8.25%** |
  | CLI peak RSS | 363 MB | 327 MB |
  | `fps=` | 58.5/60 | 58.8/60 |
  | `dropped=` | 50 | 4 |
  | `capture_copy=` | 73.2% | 0.7% |
  | media written | 34 segments, 86,140,600 B | 72 segments, 180,706,473 B |

  The readback path spent ~2.75 cores of CPU to write 86 MB; the hybrid spent ~0.08 cores to write
  181 MB, over windows of the same length (43.0 s and 44.5 s of steady state). **That is worse for the
  shipping path than §13's 88.9% alone suggested, and the difference is the instrument**: §13 and
  `xtask verify` read the `replayed-cli` process, and at 4K/60 with the loopback track live the
  copy out of VRAM costs the CLI more than a core *and* hands ~1.5 cores of encode to a child that
  number does not contain. The hybrid is the only path whose encoder runs inside the process being
  measured (the MFT is in-process; the readback path encodes in the `ffmpeg` child), so a CLI-only
  figure flatters it exactly where the pair above is honest: the same sampler over **both**
  processes, 275.47% against 8.25%.
- **The spread is real.** 3.1%, 3.6% and 4.0% are three runs of the same path. Criterion 6 passes
  with room to spare in all three, but the figure is a run, not a constant.
- **`capture_wait=` is not a back-pressure signal.** It reads 61.1% while the inline encoder
  throttled the pump to 35 fps and 61.3% while a threaded encoder kept up at 59.6 — the same number
  for opposite situations. Most of it is a correctly paced 60 Hz capture idling between frames.
- **AV1 is not reachable on this hardware.** The NVIDIA GPU does H.264 and HEVC through NVENC; AV1 is
  40-series and later.
- **1080p is still unmeasured**, for the reason §1 already gives: the pool follows the capture item,
  only the primary 4K monitor is captured, and no configuration reaches a smaller size.
- **Windows only, and by construction.** Off Windows there is no encoder MFT, so the hardware path
  cannot be selected: the default resolves to the ordinary path and logs why. That is also why the
  key is an `Option<bool>` and not a `bool` defaulting to true — an absent key resolving to `true`
  would make every machine without an encoder MFT refuse to start, and would have taken the test
  suite with it, since the suite runs on a Mac.

---

## 16. A clip's duration, and a CI failure on a commit that changed no code (2026-10-15)

`091d956` — a **documentation-only** commit — failed the "Desktop shell Rust tests" job on

```
commands::tests::one_hotkey_press_writes_exactly_one_clip_through_the_recorder
clip duration 4393ms is not the configured window (3000ms ± the segment grid)
```

Nothing in that commit could have changed a clip, so the assertion was measuring something
other than the code. It was measuring two things, and both are worth having written down.

**The container's duration is not the window's.** `MediaInfo::duration_ms` — and the DTO the
UI shows — is ffprobe's `format.duration`, the same number criterion 4 is verified against.
For a clip it is larger than either stream in the file. Measured on the dev host, fourteen
runs of that test, plain and with sixteen competing processes:

| window | video stream | audio stream | container |
|---|---|---|---|
| 3000ms | 3000ms (13 runs), 3100ms (1 run) | 3008ms | 3187-3396ms |

The **video stream is the window**: 3000ms, or 3100ms when the window's edge falls inside a
slightly longer fragment. The container carries the audio track's per-fragment padding and the
muxer's stitched timestamps on top of it, and that surplus is small and steady on a real feed:
the Windows test machine's clip in §1 and §10.2 is **13,091ms against a 13,000ms target — 91ms over thirteen
segments**, about 7ms each, comfortably inside criterion 4's ±0.5s. The test's own clips come
out ~190ms over three fragments instead, which is a property of the synthetic stub feed or of
how the synthetic audio is muxed; the two feeds were not isolated against each other, so treat
the difference as unattributed rather than as a defect in either.

*(2026-10-17: this section's test is the desktop hotkey test, not `ram_clip`; the `ram_clip`
fixture mentioned in §20 and §22 now renders its stream offline, see those sections.)*

**The splice cuts on the fragment grid.** A clip takes the whole fragments overlapping
`[trigger−pre, trigger+post]`, so it overhangs the window by whatever the grid quantises to at
each end — the §7 note that "it can run up to one segment past the request" is this — and on a
loaded host the fragments themselves run past `segment_time`, because the forced keyframe
lands on the first frame *after* the boundary. That is host pacing, not splice arithmetic, and
it is the part that cannot be bounded by a millisecond constant.

The assertion now pins what the splice promises rather than a millisecond fit: the clip's
**video** carries both sides of the configured window (the pre-roll-only clip of ≈2s, and the
empty one, are the failures it rules out), and the overhang is at most one fragment at each
end. The container's duration is deliberately no longer asserted against the window: at a
3000ms window it is not 3000ms even on an idle host, so the honest options were a bound fitted
to the observations or no container bound at all, and §10.3's mistake is the first. The 4393ms
reading was not reproduced here (nine further runs under sixteen competing processes, all
3187-3396ms), and the failing run's per-stream durations were not captured, so which of the two
effects moved furthest on the CI runner is **unattributed**.

---

## 17. A video-only recording: `[audio] enabled = false`, honoured (2026-10-15)

`config.example.toml`'s `[audio] enabled` was the last key in that section the pipeline could
not act on: setting it to `false` produced a warning at startup and a recording that carried
the game audio anyway. The engine declared one audio input in its argument list, bound one
loopback listener and started one writer thread, and no configuration could remove any of the
three.

It can now. `EncodeConfig::game_audio` is the switch, and `false` means the encoder is given
**no audio input at all**: `ffmpeg_args` emits neither the `tcp://127.0.0.1:{port}` input nor
the `-c:a aac -b:a Nk` block, `FfmpegEncoder::spawn` binds no listener and starts no
`ffmpeg-audio-in` thread, and the container carries one video stream and no audio stream.
`Encoder::submit_audio` **refuses** a block at such an encoder rather than dropping it
silently — the disposition `submit_mic_audio` already took for a missing microphone — and the
argument-list test pins the *absence* of the input, the codec and the rate, because a flag left
in a list that nothing dials is not an error ffmpeg reports.

The capture side follows the same switch: `build_sources` creates the WASAPI loopback backend
only when the game audio is on, so a video-only recording does not capture a track it is not
going to write — what it saves is the capture as well as the encode. The pump takes the audio
backend as an `Option<&mut dyn AudioBackend>`, the shape the microphone already had, so "this
recording has no such input" is one absence rule for both instead of two.

One combination is refused rather than recorded: `enabled = false` together with
`[mic] enabled = true`. A container's audio tracks are named by position and
`replayed_media::edit::audio_titles` calls the first one `Game Audio`, so a one-track
recording of a voice would be labelled as the game's own audio — a file that says one thing and
holds another, which is the class of defect this ledger exists to catch. It is refused in
`Recorder::start_with_options` before anything is opened, and again in `FfmpegEncoder::spawn`
as the backstop for a front-end that reaches the encoder without passing through the recorder.

How far the key reaches: the CLI carries it (`audio_enabled: Some(audio.enabled)`) and no
longer warns about it; the desktop shell carries `audio_enabled: None`, which means the game
audio is recorded, because that shell does not parse `[audio]`; and a video-only run says so in
its own startup line (`capture geometry …, video only: no audio track`) rather than leaving it
to be found in the file. `bitrate_kbps` still reaches ffmpeg's `-b:a` and is simply unused when
`enabled` is false.

**Evidence.** The argument list, the listeners and the backends are the code's half of this; the
file's half is a run of the released CLI on the Windows test machine at `92e5a70`, against the shipped example with
`[audio] enabled = false` (the key the run set is `enabled = false`, at line 134 of the
configuration the CLI actually read). Its own `--self-test-clip-after` ended the run after 45 s
with exit code 0, and:

- the startup line said it out loud — `capture geometry 3840x2160 at 60fps (frame counter starts
  at 0), video only: no audio track`, exactly one such line in the debug log;
- the clip it wrote, `clip-1792082573.mp4`, is **174,455,585 bytes** and `ffprobe` reports one
  stream, `0,h264,video`. **There is no audio stream in the container at all** — not an empty
  one, not a silent one.

---

## 18. HEVC on the hybrid: the throughput gap does not reproduce, and both codecs overshoot (2026-10-15)

§15 recorded a gap between the two codecs on the zero-copy hybrid — HEVC at 56.9 fps with 9
frames dropped against H.264's 59.6 and 0 — and left it open, because the two figures came from
different runs. On 2026-10-15 they were paired: one run of the released CLI per codec, in one
interactive session, same display, same 4K/60 capture, same `--self-test-clip-after 45`, both
with `[encode] zero_copy = true` and nothing else different between them.

| | H.264 (`h264_nvenc`) | HEVC (`hevc_nvenc`) |
|---|---|---|
| the probe's own ceiling | 74.2 fps | 77.7 fps |
| `fps=` | 58.8/60 | 59.0/60 |
| `dropped=` | 3 | 10 |
| `skipped=` | 14 | 17 |
| frames / media | 2979 / 50,322 ms | 3016 / 51,006 ms |
| bytes written | 181,128,764 | 165,825,260 |
| the clip's `bit_rate` | 39,564,433 — **1.98x** the 20 Mbps asked for | 35,747,208 — **1.79x** |
| the clip's `avg_frame_rate` | 59.3 fps | 58.6 fps |
| frames that never reached the encoder | 3 | 10 |

**The rate gap does not reproduce.** In one session on this machine, HEVC holds the same frame rate
as H.264 — 59.0 against 58.8, both inside the spread §15 already published. What does reproduce
is a smaller and more specific difference: HEVC sheds about three times as many frames at the
handover (10 against 3, out of ~3000), and both codecs report the same warning — "its queue is
two deep because capture's handover ring is three, and the encoder is slower than the frame rate
it is being fed". So the mechanism behind the earlier 9-dropped figure is the handover rather
than one codec being unable to reach 60 fps, and the media timeline is intact in both runs
(50,322 ms and 51,006 ms of frames for ~51 s of capture) — which is what dropping at the
handover means: the frames are refused at the ring, not lost from the recording.

**What the pair does show plainly is the bitrate.** Both codecs were asked for 20,000 kbps and
wrote far more of it — H.264 39,564,433 bit/s (1.98x), HEVC 35,747,208 (1.79x) — so the rate
control is a suggestion the encoder aims near rather than a number it holds. That is the
`ICodecAPI` workstream's motivating measurement, and it is what §15's "tracks the setting with
1.2-2.0x overshoot" looks like as a paired number rather than a range.

---

## 19. The encoder accepts the rate and does not obey it (2026-10-15)

§18 left the overshoot as "a suggestion the encoder aims near", and the obvious way to make it a
number it holds is `ICodecAPI` — the transform's own control surface rather than the
`MF_MT_AVG_BITRATE` attribute on the output type. `crates/encoder/src/mft.rs` (commit
`f4dbf10`) casts the transform to `ICodecAPI` and sets `CODECAPI_AVEncCommonRateControlMode` to
CBR and `CODECAPI_AVEncCommonMeanBitRate` to `encode.bitrate_kbps * 1000`, logging the
encoder's answer either way and saying so out loud when a transform has no `ICodecAPI` at all.

The encoder answers yes, and the file does not move:

| H.264, `encode.bitrate_kbps = 20000` | the clip's `bit_rate` | against the setting |
|---|---|---|
| before any `ICodecAPI` call (§18) | 39,564,433 | 1.98x |
| `ICodecAPI` set **after** `SetOutputType` (`f4dbf10`) | 39,314,725 | 1.97x |
| `ICodecAPI` set **before** `SetOutputType`, plus a maximum | 39,124,625 | 1.96x |

Both calls return `Ok` — the debug log carries `the encoder accepted rate control mode (CBR) = 0`
and `the encoder accepted mean bitrate (bit/s) = 20000000`, twice per run (the warm-up output
type and the real one) — and the third row set `CODECAPI_AVEncCommonMaxBitRate` as well as the
mean, which moved the file by half a percent. So on this stack (Windows 11, NVIDIA RTX-class GPU, the NVENC
H.264 MFT) the property is advertised, accepted and ignored, and the order of the calls against
`SetOutputType` is not the reason, because moving them earlier changed nothing.

What ships is the first of the three, `f4dbf10`: the encoder is asked through its own interface
and its answer is logged. It does not claim to have pinned anything, because the measurement says
it has not. `MF_MT_AVG_BITRATE` still describes what the stream is.

**Not tried, and the honest edge of this thread:** a rate-control mode other than CBR (an encoder
that ignores CBR may obey VBR or a quality mode), `CODECAPI_AVEncCommonBufferSize`, and a
driver-side preset. None was measured, and each would be another experiment for a number a reader
of a clip's `ffprobe` output can already see is not being held.

**Follow-up (P11, unmeasured).** Two changes aimed at §19, both argument/property level and neither
run on the NVIDIA GPU: the ffmpeg path (`video_output_args`, used for every non-zero-copy hardware encoder)
now sends `-rc cbr` (NVENC/AMF) plus `-maxrate` and `-bufsize` equal to the target, where it used to
send only `-b:v`; and `pin_bitrate` for the MFT path now also sets `CODECAPI_AVEncCommonMaxBitRate`,
`CODECAPI_AVEncCommonBufferSize` (one second) and `CODECAPI_AVLowLatencyMode`, the untried items
above. Whether either holds the rate is still a `bit_rate` measurement owed on Windows.

---

## 20. A second CI failure on a commit that changed no code (2026-10-15)

`21e0b87` ("Take the matched CPU pair for the zero-copy path") changes one thing: the text of §15.
Its CI run (`38772177701`) failed, inside `crates/replay/tests/ram_clip.rs`:

```
test a_window_inside_the_held_footage_covers_the_whole_request ... FAILED
thread 'a_window_inside_the_held_footage_covers_the_whole_request' (19732) panicked at
crates/replay/tests/ram_clip.rs:322:5:
the pre-roll is inside the held footage, so nothing should be reported as truncated
test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.46s
```

That is `assert!(!window.truncated_front)`: the ring reported that the window's *front* reached
past the footage it holds, for a window the test builds to sit wholly inside it. The file is not
one that change touches, and this is not the dead-writer flake §16 is about — it is the second
intermittent failure of the same kind, and it is recorded rather than re-run away.

**What is known.** The runs either side of it on `main` are green (`b6d0a1d`, 4m7s; `f4dbf10`,
4m57s). Locally the test passes five times in a row, and a full
`cargo test --workspace --features replayed-encoder/test-encoders` passes with 35 `test result:
ok` lines and `EXIT=0`. So it is neither caused by that commit (text only) nor deterministic.

**What is not.** The mechanism. `ring_of_stub_capture(6)` builds its ring from a **real-time**
stub capture and a real ffmpeg child, so how much footage the ring holds when the assertion runs
is a function of how fast the host fed it; the window is `[span - 3000ms, span]`, and it lies
inside the ring only if the ring's earliest held fragment is older than that. The two candidates
are a front fragment ffmpeg had not yet finalised under a loaded runner, and the ring's RAM bound
having trimmed the earliest fragment. One failure with no capture of the ring's own numbers does
not distinguish them, and the honest reading is that **the test's margin is the fragile part**,
not `window`/`truncated_front` — which is why the next person to touch this fixture should make
the window a function of the ring's reported front rather than of an assumed `0`, instead of
re-running it until it passes.

**Update (2026-10-17).** The fixture this section blames is gone: `ring_of_stub_capture` now
renders its stream **offline** (ffmpeg from lavfi sources into a fragmented MP4, a keyframe a
second) and pushes it into the ring, so what the ring holds no longer depends on host speed
(`4c8138e`). The real-time capture -> encoder -> ring path keeps its own, separate test. The
diagnosis above describes the old real-time fixture; whether the intermittent failure is fully
gone is **unverified** beyond the runs since the change.

---

## 21. `xtask verify` on Windows, run for the first time (2026-10-15)

§7 and §9 have carried the same caveat since 2026-09-25: the harness's Windows half — the
`GetProcessTimes` + `K32GetProcessMemoryInfo` sampler, `scripts/verify-in-interactive-session.ps1`,
and everything it says about a *real* capture path — was **compiled and never run**. It ran on
2026-10-15, on the Windows test machine, in Session 1, against the released CLI at the tip:

```
xtask.exe verify --seconds 60 --report … --cli target\release\replayed-cli.exe
```

The harness wrote its own `RUN_CONFIG` into `%LOCALAPPDATA%\replayed\config.toml` (the user's
file was backed up and restored around the run), captured the primary monitor at 3840x2160/30fps
through Windows Graphics Capture, encoded with `h264_nvenc` and wrote its report. **The sampler
ran for the whole run, on Windows, for the first time**: samples every 2 s from t = 2.0 s to
52.5 s, the CLI's cumulative CPU time 0.906 s → 2.484 s, RSS 120.5 MB → ~162 MB. The numbers
this ledger has been reading off macOS stubs now exist for the platform the project ships to.

**26 of 30 rows passed**, including the ones nothing had exercised: the toolchain and ffmpeg
discovery; `WORKS: h264_nvenc, hevc_nvenc, h264_qsv`; `capture geometry 3840x2160 at 30fps` with
`WGC capture started on the primary monitor width=3840 height=2160`; `frames=1955 segments=10`;
29.9 fps achieved against 30 declared; the span growing 64,962 ms over 65,000 ms (0.999x); the
byte cap holding (max 11,514,871 against 16,777,216, over 302 samples — the 16 MiB was the
harness's own `scratch_cap_bytes`, a key the engine no longer reads, so that row compared the
RAM held against a bound nothing enforced; the bound that is real is the RAM budget,
`buffer.ram_cap_bytes`); the 48000Hz/2ch/F32 →
s16 conversion; **CPU 2.92% of one core** over a 46.5 s window; **RSS 164.9 MB peak**; the clip
written inside its post-roll budget (4000 ms of 5000); one h264 video and one aac audio stream in
it; the splice being a copy; A/V drift −18 ms (log) with ffprobe agreeing; audio not silent
(mean −44.6 dB); video not black (6710 distinct colours); and the criterion-7 refusal verified by
a real startup failure naming `h264_amf`.

**Three rows failed**, and they are the useful part of the report:

| row | what it saw | reading |
|---|---|---|
| 12 `dropped` | `dropped=1 dropped_audio=0` of 1955 frames | one frame refused at the capture→encoder handover over 65 s at 30fps — §18's handover finding in miniature, and on a run that is otherwise clean. |
| 16 `eviction` | "no seg-*.mp4 under `…\target/verify/work\appdata\replayed\scratch`", while the status line reported `segments=10` | the check wants the lowest *surviving* segment number to be greater than 0; it found no segment files at all. The run's own `segments=10` says the segments existed — but in **RAM**, not on disk: buffer mode writes nothing while it buffers (§12), so the scratch directory is empty by design and the row can never pass there. It was made *not performed* with that reason (§23), and has since been rewritten to read the ring's `evicted=` counter instead of looking for files on disk (§25). |
| 23 `clip_duration` | 9020 ms against 8000 ms ± 500, "off by 1020ms" | the clip's own video is 9002 ms and its audio 9003 ms (row 27), so the file is ~1 s longer than the 5 s pre + 3 s post window asks for. §16 established that a container's duration is not the window and moved the *desktop* clip test onto the video stream; this harness row still bounds the duration, so it is measuring what §16 says not to measure — and the ~1 s itself is worth understanding separately, because §13 measured only ~190 ms of padding on the stub path. |

**One row was not performed**: 18 `silence_detector` — the warning it looks for fires only on a
converted *and* silent endpoint, which needs a machine whose default playback device is not
48 kHz; a setup condition rather than a defect, and the row says so.

## 22. The `ram_clip` fixture is still load-dependent (2026-10-15)

`crates/replay/tests/ram_clip.rs`'s `ring_of_stub_capture` feeds a real-time stub capture to a
real ffmpeg for `seconds` of wall clock, so what the ring holds is a function of how fast the host
is. Two of its four tests failed in a loaded workspace run on 2026-10-15, in two different ways:

- `a_clip_saved_from_ram_is_playable_and_leaves_the_scratch_directory_empty` panicked inside the
  shared helper at `enc.finish()`, with `ffmpeg exited with signal: 9 (SIGKILL)` — the encoder's
  own bounded shutdown (`SHUTDOWN_BUDGET`, 5 s) killing a child that would not exit in time under
  the load of the rest of the suite. The product did what it says it does; the fixture asked a
  loaded machine to flush on a deadline meant for a quiet one.
- `a_window_inside_the_held_footage_covers_the_whole_request` failed on `!window.truncated_front`
  — the same assertion §20 recorded from CI — **after** the fixture was changed to take the window
  from the ring's own reported front. Asking for the front is necessary and not sufficient: a
  starved ffmpeg can leave a *gap* in the ring's closed fragments, and `window()` then rightly
  reports the request as truncated where the footage really is missing.

The change that came out of that investigation (commit `3e6334c`) removes the silent `0`
assumption and states the precondition in the ring's own numbers, and says in as many words that
it is not the fix. **What a real fix needs** is a deterministic fixture — a fixed synthetic
fragment stream, or a stub whose frames are produced by count rather than by the wall clock —
because a test whose subject is ring arithmetic should not also be a test of how busy the machine
is. Until then this file's tests are quiet when run alone and can fail under load, which is worth
knowing before a red CI run on this file is read as a regression.

**Update (2026-10-17).** The deterministic fixture this section asked for has landed
(`4c8138e`): `ring_of_stub_capture` renders a fixed fragmented stream offline instead of
feeding a real-time stub capture to a live ffmpeg, so neither the SIGKILL-on-shutdown nor the
load-dependent `truncated_front` mechanism above applies to it any more. The real-time path is
still covered by its own test, which can still be load-sensitive. The description above is kept
as the record of the old fixture.

## 23. `xtask verify` re-run after the harness was corrected (2026-10-16)

The three-to-four failures §21 recorded came back identically on 2026-10-16 (25 pass, 4 fail,
1 not performed): `frame_rate` (read the final sample, taken during shutdown: 0.5 fps), `dropped`
(1 frame, lost at t = 0.4 s while the encoder child was still starting; none in the other
51 s), `eviction` (the scratch directory is removed on exit, so it was always empty by the time
the check looked) and `clip_duration` (9014 ms against 8000 ± 500; segments are 1 s and the
lossless cut lands on segment boundaries). All four were judged to be **harness assumptions, not
pipeline defects**, and the harness was changed (`13063a2`): steady-state median fps, startup
drops reported but excluded, one segment of duration slack, and `eviction` is now *not performed*
with the reason instead of a failure.

Read that honestly: this loosened four checks. It did not fix a pipeline. The one-frame startup
drop is real and is now disclosed in the `dropped` row's measured text rather than failing it.
`eviction` was unperformed at that point, and the reason was the **row**, not the pipeline: the
cap it was checking against was `buffer.scratch_cap_bytes`, a key the engine removed when the
ring moved into RAM (§12), and buffer mode writes **no** `seg-*.mp4` at all — there was nothing
on disk for the row to watch. The bound that is real is the RAM budget,
`buffer.ram_cap_bytes` (256 MiB by default), and that is the number a cap row has to be measured
against.

**That row has since been rewritten (2026-10-19, §25).** It no longer looks for segment files
on disk; it reads the ring's own `evicted=` counter out of the status line, which the engine
now reports, and the cap row became `ram_cap`. So `eviction` is performable now, and a run
with a lower budget or a longer `--seconds` is what exercises it. The 2026-10-16 result
recorded above — 2 not performed — stays as it was measured; it is a record of that run, not a
statement about the harness as it stands.

Result at `13063a2` on the Windows test machine (NVIDIA RTX-class GPU, 3840x2160/30fps, `h264_nvenc`): **28 passed, 0 failed,
2 not performed** (`silence_detector`, `eviction`).

---

## 24. The MFT bitrate overshoot was the declared frame rate (2026-10-17)

§18-§19 left both MFTs writing about twice the configured `bitrate_kbps`. The media types declared
30fps while the texture path fed 60, so the encoder budgeted `bitrate / 30` per frame. Fixed in
`crates/encoder/src/mft.rs`: both types now declare the effective encode fps. H.264 asks for level
5.2 when 4K60 needs it. Keyframe spacing is one second of frames, and media time is counted from
the frame number.

Measured on the Windows test machine (NVIDIA RTX-class GPU, NVENC MFTs, 3840x2160, `zero_copy = true`,
`[audio] enabled = false`, `encode.bitrate_kbps = 20000`). Each row is one run of the release CLI
with `--self-test-clip-after 45`, and the numbers are `ffprobe` on the clip (about 35 s). Every run
was in one interactive session, but whatever was on the desktop was not controlled, so the
H.264 rows at the same settings vary by about ±5%.

| change (one at a time) | codec | clip `bit_rate` | x target | keyframes / s |
|---|---|---|---|---|
| none (as on `main`) | H.264 | 39,779,675 | 1.99 | 2.0 |
| none (as on `main`) | HEVC | 36,486,396 | 1.82 | 0.93 |
| declare 60fps | H.264 | 20,479,689 / 18,671,560 | 1.02 / 0.93 | 0.99 / 0.94 |
| declare 60fps | HEVC | 18,772,073 | 0.94 | 0.47 |
| + `CODECAPI_AVEncMPVGOPSize = 60` | HEVC | 18,384,640 | 0.92 | 0.46 (accepted, ignored) |
| + keyframe spacing 30 | HEVC | 17,668,406 | 0.88 | 0.44 (ignored) |
| + `CODECAPI_AVEncVideoForceKeyFrame` once a second | HEVC | 18,528,045 | 0.93 | 0.93 |
| + `CODECAPI_AVLowLatencyMode = 0` | H.264 | 18,726,046 / 18,408,213 | 0.94 / 0.92 | 0.94 / 0.92 |
| + low latency off, `AVEncMPVDefaultBPictureCount = 2` | H.264 | 19,273,754 | 0.96 | 0.94 (B-frames refused, `E_INVALIDARG`) |
| + `CODECAPI_AVEncCommonQualityVsSpeed = 70` | H.264 | 18,654,397 | 0.93 | 0.93 (accepted) |
| + `CODECAPI_AVEncVideoContentType = 1` | H.264 | 18,552,586 | 0.93 | 0.94 (refused) |
| **shipped** (60fps + forced keyframe) | H.264 | 18,629,093 | 0.93 | 0.93 |
| **shipped** (60fps + forced keyframe) | HEVC | 18,638,679 | 0.93 | 0.93 |

**What ships.** The real frame rate, plus a keyframe forced once a second through
`CODECAPI_AVEncVideoForceKeyFrame`. The forced keyframe is there because the HEVC MFT ignores
`MF_MT_MAX_KEYFRAME_SPACING` and `AVEncMPVGOPSize`: it keyframes every 2 s, which would make each
fragment two segments long. Low-latency mode stays on. Turning it off, setting quality-vs-speed,
the content type and B-frames each moved the rate by less than the noise between runs, and no
clip ever contained a B-frame. Keyframes per second fall a little short of 1.0 because the cadence
counts frames and fewer than 60 a second reach the encoder.

**Not measured.** Picture quality per bit, a game rather than the desktop, other vendors' MFTs, and
rates other than 60fps.

---

## 25. The 2026-10-19 review sweep — what was found, what was fixed, and what was **not** re-measured

A full review of the tree at `4cd0fdb` (324 commits) found thirty-six issues, opened as
#27-#62. The largest of them was a licence defect: the pinned ffmpeg was Gyan's *essentials*
build, which upstream's own page says is **GPLv3** and which carries **libx264 and libx265**,
while the project's README and `xtask/sidecars.toml` said — in four places — that it was LGPL
and that "LGPL is sufficient". The pin has been replaced with a **versioned BtbN release
asset** whose configure line carries `--disable-libx264 --disable-libx265` and no
`--enable-gpl`, and whose banner reads "LGPL version 3 or later"; the SHA-256 is recorded in
`xtask/sidecars.toml` and the archive is verified before anything is extracted.

**The most important thing to say about this section is where it was verified.** The sweep was
built and tested on the **development host (macOS)**. Nothing in it has been re-measured on
Windows hardware. Specifically:

- the repinned ffmpeg is **hash-verified and its contents inspected**, but has not been
  executed on Windows by this sweep — the `package-windows` job is what will do that;
- the recorder, encoder, media and store changes are covered by unit and integration tests on
  the development host, and the Windows target **compiles and lints** clean, but the pipeline
  has not been re-soaked at 4K;
- the desktop shell changes (the degraded start, the hotkey ordering, the folder picker) have
  been exercised by the shell's own test suite and **not** by a person on a screen;
- the frontend changes have been checked by `svelte-check` (0 errors) and 251 vitest cases,
  and **not** by a mouse.

The honest summary is: **the code is verified on the development host, the Windows path is
type-checked and linted, and the Windows runtime is unverified for this sweep.** A run of
`cargo xtask verify` on the Windows test machine is the next thing that has to happen, and
until it does, the numbers in §13-§24 stand as they were measured — they were not disturbed by
this sweep.

### What changed in the engine

| change | issue | what it fixes |
|---|---|---|
| `Prepared::begin` returns the index connection on every failure path | #29 | one transient start failure no longer bricks games mode for the process lifetime |
| the clip splice moved off the pump thread onto `replayed-splice` | #30 | a clip no longer punches a frozen-picture hole into the recording it was cut from |
| event markers carry their **occurrence** time, not their handling time | #31 | a kill chapter is no longer up to `coalesce.max` (18 s) late |
| `started_at_ms` comes from the **resolved** window | #31 | chapter offsets are no longer off by up to one segment |
| the post-roll budget is `post_ms + 2*segment_ms + margin` | #32 | clips no longer time out when `segment_time >= 6` |
| `Store::open` sets a 5 s busy timeout | #33 | a hotkey press no longer collides with a UI write into a lost clip |
| saturating arithmetic on every config- and file-derived duration | #33 | an absurd `pre_seconds` cannot wrap |
| `FfmpegEncoder` has a `Drop` that kills **and reaps** the child | #33 | a failed start no longer leaves an unreaped ffmpeg and three detached threads |
| `escape_ffmetadata` escapes `\r`, `[` and `]` | #44 | a CR in a chapter title could inject a `title=` line into the next chapter (measured, not theorised) |

### What changed in the harness and in CI

The harness's two ring rows were rewritten to match the engine that actually exists. The
`scratch_cap` row became **`ram_cap`** ("the ring stayed inside its RAM cap for the whole
run"), measured against `buffer.ram_cap_bytes` — the bound the engine really enforces — and
the `eviction` row no longer looks for `seg-*.mp4` files on disk, which buffer mode never
writes. It reads the ring's own **`evicted=`** counter, which the engine's status line now
reports. That is why `eviction` is performable again where §23 could only mark it *not
performed*: the row was wrong, not the pipeline. The harness also gained `--strict`, which
makes a *not performed* row a non-zero exit, and a verdict line that says
`PASS with N check(s) not performed` instead of a bare `PASS`.

CI's clippy step lost the two `-A` flags that were overriding the workspace's own
`[lints]` table — the `unwrap_used` guarantee the step's comment claimed it enforced was
enforced nowhere — and gained a **Windows-target** clippy step, a `cargo fmt --check` step,
and a `cargo xtask verify --dev-software-encoder` step that publishes its report as an
artifact. The test-body `unwrap()`s are now exempted **structurally** (a
`#![cfg_attr(test, allow(clippy::unwrap_used))]` per crate and per integration-test file), so
production code is linted and test code is not.

### What is still open

- `silence_detector` is still *not performed* — it needs a machine whose default playback
  device is not 48 kHz. That is a setup condition, not a defect.
- A Windows run of the harness with the new `ram_cap` and `eviction` rows has not happened.
- The GSI token is still written in cleartext into the game's `cfg` folder, and the installer
  still re-reads the token file independently of the running listener (#44, partially fixed:
  the listener now exposes `port()` and `accepts_token()` for exactly that).
- The per-session ephemeral GSI port is available in the events crate but the recorder still
  writes the configured port into the `.cfg`, so the port remains squattable (#44).

## 26. The GSI token, the port and the install (2026-10-19)

§25 left two things open: the game-state token was still written in cleartext into the
game's own `cfg` folder, and the listener's port was still the configured one, so another
local program could squat on it and receive the game's POSTs — and with them the token. The
installer also re-read the token file on its own, so a token file replaced mid-recording left
the game posting a token the listener refused, every POST answered `403` until the
application restarted.

This section records what changed, and what was **not** re-measured.

### One listener, per-game paths, per-game tokens

Each game now has its own token file (`gsi-token-cs2.txt`, `gsi-token-dota2.txt`), its own
path on the listener (`/gsi/cs2`, `/gsi/dota2`) and its own **app id** (730, 570). The
listener resolves the request path first: a path no game serves is `404` and the body is
never read; a token that is not that path's own is `403`; a payload whose `provider.appid`
is not that game's is `403`. A token lifted from one game's installed `.cfg` therefore
cannot forge the other game's events, which is the part the cleartext file made possible.

The old single `gsi-token.txt` is **migrated by copy** into each per-game file (a copy keeps
the `0600` mode), and is left in place so an older build still finds its token. Both games
inherit it, so an already-installed `.cfg` keeps working.

### The listener is bound before anything is written

The order used to be: write the `.cfg` naming the configured port, then bind. A port
already taken meant a `.cfg` was left behind naming a **squatter's** port, and the game
posted its token there. The order is now: bind, then write, and every `.cfg` — in the
application's data directory *and* in the game's own folder — names **the port the listener
actually bound**. A failed bind leaves no `.cfg` behind at all.

### The installer takes the live binding, and refuses without one

The installer no longer reads the token file. It takes the binding the running listener
serves — the live port, that game's path and the token in force — and writes it. With no
listener running there is nothing for the game to post to, so the install is refused with
the reason rather than writing a file that cannot work. The write is **atomic** (a
uniquely-named temp file in the same directory, written, flushed, then renamed over the
destination), and a remembered install is rewritten when the listener's port moves.

The containment rule was changed as well, and the change is worth stating plainly: the
destination must be a known library's `steamapps/common/<game folder>`, and the containment
root is **the library**, not the game folder and not the Steam root. A symlink planted
inside the game folder that escapes the library is refused. A game in a **secondary**
library — one the user's own `libraryfolders.vdf` lists — is installed, because that file
is the user's own Steam configuration: no other unprivileged account can edit it, and the
application writes with that user's own permissions either way. Refusing it would break a
game on a second drive for no security gain.

### The port can now be unguessable

`[events] gsi_ephemeral_port` (default `false`) has the operating system pick the port
instead of the configured one. `gsi_port = 0` still switches the listener off entirely, in
both modes.

### What was **not** re-measured

- This was **verified on the development host (macOS) only**. The new routing, the token
  scoping, the atomic install and the containment rule are covered by the unit and
  integration tests — 28 new ones — and by the clippy and formatting gates. They have not
  been exercised against a real CS2 or Dota 2 client.
- The token is **still** written in cleartext into the game's `cfg` folder. That cannot be
  removed: the game has to be able to read it. The mitigation is the app-id scoping and the
  optional unguessable port, and the residual risk is unchanged by this work.
- A fixed port is still squattable unless `gsi_ephemeral_port` is turned on. The default is
  unchanged at `45671`.
- No Windows run of the harness has happened with any of this in place.

## 27. The updater: the one thing that reaches another machine (2026-10-20)

Until now this tree made **no network egress at all**, and said so in the README, in
`SECURITY.md` and in the code (`crates/events/tests/no_egress.rs`). The updater is the one
feature that changes that, so this section records what was built, what was checked, and
what was **not**.

### What it is

- Tauri v2's official updater plugin: `tauri-plugin-updater` 2.14 (Rust) and
  `@tauri-apps/plugin-updater` 2.14 (JS) — the same major version the other plugins in
  this shell already use, pinned the same way (`"2"` in the manifest, `^2.14.0` in
  `package.json`).
- `bundle.createUpdaterArtifacts` is on, so a release produces the updater's own artifact
  (the installer, zipped) and its minisign signature.
- The endpoint is **one fixed URL**, in `plugins.updater.endpoints` in `tauri.conf.json`:
  `https://github.com/FisiFla/replayed/releases/latest/download/latest.json`. GitHub
  serves the newest release's asset at that stable path, which is why the release workflow
  now publishes `latest.json` as an asset of every release.
- The trust anchor is the **minisign public key pinned in `plugins.updater.pubkey`**, in
  the same file. It is compiled into the binary. Every artifact is verified against it
  before it is installed, and `plugins.updater.requireSignedVersion` additionally requires
  the signature to carry the version it was signed for — so a tampered manifest cannot pair
  a new version number with an older, validly signed release.
- The **check is user-initiated only**. A "Check for updates" control in *Settings →
  Updates*. No startup check, no polling, no background task, no telemetry.
- The private key **never ships**: it is a release-pipeline secret
  (`TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) and it is not in
  this repository.

### What is verified

| claim | level | how |
|---|---|---|
| The pinned key is a key the plugin can parse, the endpoint is the one fixed HTTPS URL, the bundle produces updater artifacts, and the signed version is required | **Verified on the dev host** | `apps/desktop/src-tauri/tests/updater.rs` — six tests over `tauri.conf.json` and the minisign decode, one of which parses `plugins.updater` into the **plugin's own** `Config` type |
| The signature check accepts a good artifact and refuses a tampered one, and refuses a signature made by another key | **Verified on the dev host** | the same file, driven against a **throwaway** key pair and a fixed test vector, with the same `minisign-verify` crate the plugin uses |
| The pinned key is *not* the throwaway key the test vector was signed with | **Verified on the dev host** | the same file |
| The application asks nothing until the user presses the button | **Verified on the dev host** | `apps/desktop/src/lib/updater.test.ts` ("asks nothing until it is asked") and `apps/desktop/src/lib/components/UpdaterPanel.test.ts` ("asks nothing when the panel opens", "asks once, when the button is pressed") |
| The tree's dependency graph holds an HTTP client in exactly one place, and the source denylist is unchanged | **Verified on the dev host** | `crates/events/tests/no_egress.rs` — the dependency check now reads the resolved graphs as well as the manifests, with one exemption: the desktop shell's, for the updater |

### What is **not** verified — and this is the important half

- **No update has ever been performed, end to end.** The release workflow has never run
  with the signing key configured, no `latest.json` has ever been published, and no
  installed copy of replayed has ever been offered an update. The whole path — manifest
  fetched, artifact downloaded, signature checked, installer run — is **unverified**.
- **The release key has never signed a real release.** It was generated on 2026-10-20 and
  used only on scratch files while the signing path was being set up; it has never signed
  anything that was published, and it lives outside this repository. The key that signed
  the committed test vector is a different one — a throwaway pair, made for the test and
  never used for anything else.
- **The manifest's shape is derived from the plugin's source, not observed.** The plugin's
  `RemoteRelease` type and its target lookup (`{os}-{arch}-{bundle type}` first,
  `{os}-{arch}` as the fallback) were read from `tauri-plugin-updater` 2.14, and
  `latest.json` is written to that shape. Whether GitHub serves the asset where the
  workflow expects it, and whether the plugin accepts the manifest, has not been tried.
- **The updater has never run on Windows.** It is type-checked and unit-tested on the
  development host (macOS), and the Windows path — the NSIS `/UPDATE` install, the
  restart, the "restart to finish" state — is **unverified**.
- **The guard was widened, not loosened.** The source-file denylist, the loopback bind
  check and the certificate check are exactly as they were. The dependency check learned
  about *resolved* graphs and gained one exemption; removing that exemption makes the
  guard fail (it was checked by removing it and watching the test go red), and a client in
  any other graph makes it fail too.

### The claim that changed

The application is no longer "zero egress, ever". It is **"no egress except one
user-initiated update check"**, and the README, `SECURITY.md`, `docs/anti-cheat.md` and the
release notes now say exactly that, with the exception named and its trust anchor
described. The old wording was stronger than the code; this is the correction, and it is
recorded here rather than quietly edited.


### A residual: the archive is unpacked with a crate that has three open advisories

`tauri-plugin-updater` unpacks the downloaded installer with `zip`'s `ZipArchive::extract`
— the exact API behind **RUSTSEC-2026-0367**, **RUSTSEC-2026-0368** and
**RUSTSEC-2026-0369** (privileged Unix bits preserved from the archive, symlinks that may
point outside the destination, and permissions applied through them). The locked version
is `zip 4.6.1`; the 4.x line ends there, the plugin's requirement pins the 4.x line, and
OSV reports all three as `NO-FIX`. There is nothing to bump to.

What the advisories need is an archive the attacker controls. The plugin does not offer
one: `Update::install` calls `self.verify(bytes)?` — the minisign check against the pinned
public key, with `require_signed_version` on — **before** `install_inner`, which is the
only thing that reaches the extraction, and `Update::download_and_install` is `download`
(which verifies) followed by `install_inner`. The plugin's own comment on that function
says it: *"`download` already verified the bytes"*. The bytes that reach
`ZipArchive::extract` have therefore been verified against a key whose private half is a
release-pipeline secret that has never shipped.

The residual is the shape of the bug rather than its reach. The symlink escape and the
permission bits would matter if the signature check were bypassed, or if the application
ran with more privilege than the user who installed it. It runs with less: a per-user
Windows installer, where SUID and SGID do not exist, unpacking into a temporary directory
the user owns. The three advisories are therefore ignored in `deny.toml`, with the reason
written down there in full, rather than left to pass silently.

---

## 28. On real Windows hardware, 2026-10-20

The sweep until now had been verified on the development host (macOS) and on the CI runners.
This section records what was measured on a **real Windows machine** with the game running, and
what was found there. Two of the findings are defects the macOS-only verification could not
see; both are tracked as issues (#74 and #75) and both are fixed by the commits this section
names.

### The machine and the build

Windows 11, an RTX 3090, MSVC Build Tools 2022, the Windows SDK 26100, Rust 1.98.1 with the
`x86_64-pc-windows-msvc` target, and Node 22.23.3 installed **portably** (a zip unpacked into
the user's own directory — no system install, no PATH change).

The pinned sidecar was fetched with the project's own tool, its SHA-256 verified, and
**executed**: `ffmpeg version n9.0.2-17-g2a571b6068-20260930`. The frontend built. The NSIS
installer was built from the rewritten history and came out at 292.74 MiB (with the two ~128 MiB
sidecars inside it). It was installed **silently**, with the unsigned build and `--no-sign`
deliberately chosen: the minisign private key has no business on a gaming machine.

### The upgrade path — verified

A user who had the pre-rename build installed with a real library — clips, a 15,102-byte
`config.toml`, a GSI token, a 40,960-byte index — installed over it. The index arrived as
`replayed.db` with the same size, the settings and the token with theirs, the three
directories, the `.bak` with its own name kept, and the old directory was removed because it
ended up empty. The old rule ("adopt the old data only if the new directory is absent") could
never have fired on a real upgrade, because the installer creates that directory before the
application has ever started; the fix decides by the **data in** the directory instead. See
issue #73 and the commit that closed it.

### The capture, with the game running

The recording was aimed at the primary display, with **Rainbow Six Siege running under
BattlEye** in the foreground. The application's own log is the record:

```
INFO replayed_capture::wgc: capturing display=Display 2 target=CaptureTarget { monitor: 0, window: None }
INFO replayed_capture::wgc: WGC capture started width=3840 height=2160
INFO replayed_encoder::mft: the encoder accepted rate control mode (CBR) = 0
INFO replayed_encoder::mft: the encoder accepted mean bitrate (bit/s) = 40000000
INFO replayed_recorder: zero-copy: captured textures go straight to the hardware encoder, and ffmpeg copies the h264 stream instead of encoding it (chosen by default, because a hardware encoder is present)
INFO replayed_recorder: encode rate: 64.7fps sustainable at 3840x2160 with h264_nvenc (97 frames over 1.5s); encode.fps = 60 is within that, so capture runs at the configured 60fps
INFO replayed_recorder: media clock: measured from the ring's own fragments; the ledger stands at 0ms of media, 2364ms of wall clock have passed since the recording began (one segment is 1000ms)
INFO replayed_recorder: capture geometry 3840x2160 at 60fps (frame counter starts at 0)
INFO replayed_capture::wasapi: WASAPI loopback capture started on the default render endpoint native_sample_rate=44100 ... requested_sample_rate=48000 ... converting=true
INFO replayed_capture::audio_target: game audio: capturing everything the default output plays (endpoint loopback) per_app=false
```

So, on real hardware: the display capture ran at 3840x2160; the **hardware encoder** was
selected and accepted a CBR rate and a 40 Mbps mean bitrate; the zero-copy path was taken, with
ffmpeg copying the stream rather than encoding it; the encoder sustained **64.7 fps at 4K**,
which is above the configured 60; the WASAPI loopback started on the default render endpoint
and the **sample-rate conversion from 44.1 kHz to 48 kHz was active** — the territory of the
`silence_detector` check that has never been performed, now at least observed to run; and the
media clock reported the ledger standing at 0 ms of media while 2364 ms of wall clock had
passed, which is the nominal-versus-measured difference the sweep documented.

**No input was sent, no window was enumerated, and the game process was never opened.** The
audio was the *endpoint* loopback, not the per-application one, so the game was never named to
the audio subsystem. The game watcher was off for the run, so no process enumeration happened
at all.

### What this found

1. **A 4K recording aborted the process** with `memory allocation of 8246337213460 bytes
   failed` — an 8.25-terabyte allocation on a machine with 397 GB free. **Issue #74.**
   The number is exact: with 5,140 bytes of real audio still buffered, the request was
   `480 x 2^32` ten-millisecond blocks of silence — **2^32 blocks = 497.1 days** — because
   `AudioTimeline::pad` sized a contiguous run of zeroes purely by a device timestamp it
   never checked. The timestamp was absurd because of a second defect: the audio engine
   computes its 100 ns clock value in **64-bit** arithmetic, so it wraps after **51.24
   hours** of machine uptime at a 10 MHz QPC, while the project's anchor used exact `u128`
   arithmetic. After that point the two are on different scales, every packet timestamp
   fails to convert, and the timeline's own arithmetic eventually asks for the 497 days.
   Both are fixed: the anchor now uses the engine's own wrapping formula, so the wrap
   cancels; the silence one gap may materialise is bounded to a second; and a timestamp the
   recording cannot account for is refused rather than obeyed. The recorder's frame sizing
   is saturating, and `buffer.ram_cap_bytes` is clamped where it is parsed.
2. **The event's timestamp was stamped when the engine handled the clip**, not when the
   caller asked for it — a 100 ms error on a loaded machine, invisible on an idle one. The
   sweep's own regression test caught it. **Issue #75.** Two things collide: the stamp was
   taken at the engine's handling time, and the clip's start is the *resolved* window's
   first fragment, which can begin up to one segment before the requested pre-roll. The
   stamp is now taken at the call site, and a marker is capped at the clip's pre-roll's
   end — the event that caused a clip happened before the post-roll did, so a marker past
   that point names a moment the pre-roll does not cover.

### The clip, after the fixes

With the two fixes in place, the same run was repeated on the same machine, with the game still
running, and the run completed:

```
WARN replayed_capture::audio_timeline: an audio packet carried a timestamp the recording cannot account for, so the timestamp is ignored: the audio carries on on the clock instead of the gap being padded with silence distance_secs=42949672.992895834
WARN replayed_recorder: encode.fps = 60 is not achievable at 3840x2160 on this machine: 46.3fps sustainable at 3840x2160 with h264_nvenc
INFO replayed_recorder: capture geometry 3840x2160 at 46fps (frame counter starts at 0)
INFO replayed_cli: self-test: the ring holds 40950ms of media; taking the clip
INFO replayed_recorder: hotkey pressed: media=40950ms wall=41337ms (drift 387ms); waiting for post-roll
INFO replayed_recorder::memory_ring: clip from RAM: trigger=40950ms window=[10950ms, 45950ms) selected=36001ms held=45968ms footage=[8958ms, 45968ms] bytes=181444620
INFO replayed_replay::splice: clip clip-1792539498696: video 36002ms audio 35989ms drift -4ms
INFO replayed_recorder: wrote H:\Replayed\clip-1792539498696.mp4 (36006ms, 181467700 bytes, encoder=h264_nvenc)
INFO replayed_recorder::index: indexed clip #1 (media t=9967ms, 36006ms, 181467700 bytes, h264_nvenc)
INFO replayed_recorder::index: recorded bookmark event #25 at media t=39967ms against clip #Some(1)
INFO replayed_recorder: buffer session #4 closed: 37 segment(s), 183095776 bytes held in RAM, no session file and nothing written while buffering
```

and the file itself, probed with the bundled ffprobe:

```
clip: H:\Replayed\clip-1792539498696.mp4  181467700 bytes
codec_name=h264
width=3840
height=2160
r_frame_rate=92/1
codec_name=aac
sample_rate=48000
channels=2
format_name=mov,mp4,m4a,3gp,3g2,mj2
duration=36.006021
size=181467700
```

So the **whole path is now seen on real hardware**: the display capture at 3840x2160, the hardware
encoder, the audio timeline refusing the 497-day gap instead of materialising it, the RAM ring
holding 37 segments, the splice (video 36002 ms against audio 35989 ms — a drift of -4 ms), the
MP4 on disk with a 48 kHz stereo track, and the index row with a bookmark event against it.

Two things in that log are worth reading twice.

**The warning is the fix, working.** `distance_secs=42949672.99` is **497.1 days** — the gap the
abort used to try to fill with 8.25 terabytes of zeroes. The timeline now names it, refuses it,
and carries on. The audio is one stretch short of the video, which is the trade the fix records:
inventing 497 days of silence is not an option, and a run of silence long enough to cover it
would push the real audio out of the encoder's 300-block queue.

**The machine was busier this time, and the recorder said so.** The first run measured 64.7 fps
sustainable at 4K and captured at the configured 60; this one measured **46.3 fps** and captured at
46, saying plainly that `encode.fps = 60 is not achievable at 3840x2160 on this machine` and
naming the two ways out. It also reported that the encoder could not sustain even the 46 fps it
was told — 38 fps were reaching it, and 8 frames were dropped. **A 4K capture of a full-screen
game at 60 fps is not a thing this machine does while the game is running**; the honest number is
lower, and the recorder reports it instead of quietly dropping frames.

### What was NOT verified

- The **first** attempt produced no clip, because the process aborted (above). After the fixes
  the path has been seen end to end — capture, splice, index row, and a 36-second 4K file probed
  on disk — but **only once**, and only on the primary display. The 72-hour soak the release
  checklist asks for has still not been done.
- The game's own audio was not captured: the run deliberately used the endpoint loopback, and
  the per-application path was not exercised.
- The game integrations (CS2's and Dota 2's GSI, League's live client) were **not** exercised
  against a real game. R6 has no game-event integration, so no event reached the recorder.
- **No person watched a clip and heard its audio.** The window was opened on the machine and
  moved to the second display, but nobody has watched a recorded clip of a real game.
- The capture was **two runs**, not a soak. The 72-hour stability check the release checklist
  asks for has not been done.
- The `silence_detector` check has still not been *performed*; what is new is that the
  conversion path was observed to run.

## 29. The 4K frame rate, and the 120 Hz question (2026-10-21)

The report was that a 4K capture dropped frames, and that the machine is a powerful one. The
recorder agreed, and it was the software's fault. The second part of the report was that the
display is 120 Hz capable, and that the app should be able to record at 120 and should let the
user choose the rate.

### What was measured, on the box, at 4K on the RTX 3090

The pipeline's own status line is the instrument. It reports the achieved rate, the drop
counter, and where the wall clock went:

```
frames=2124 segments=22 bytes=1725289 span=57920ms evicted=13 dropped=4 dropped_audio=0 dropped_mic=0 \
  skipped=142 fps=33.9/60 configured=60 capture=80.2% discard=0.0% pacer_sleep=18.8% submit=0.8% \
  capture_wait=46.5% capture_copy=0.8%
```

Three numbers matter there. `capture_copy=0.8%` — the readback of the frame out of video memory
is nearly free, which is the whole point of the zero-copy path. `submit=0.8%` — handing the
frame to the encoder costs nothing on the pump's books (the encode happens on the encoder's own
thread; the evidence that it keeps up is the achieved rate and the drop counter, not this share).
And `capture=80.2%` — **four fifths of the wall clock was spent in the capture backend**, of
which `capture_wait=46.5%` was a one-millisecond sleep and most of the rest was
`TryGetNextFrame` calls for a frame that had not arrived.

A measurement on an **idle** desktop first read 9.2 fps, which is an artefact and was nearly
taken for the answer: **Windows Graphics Capture delivers a frame when the display's content
changes**, and an idle 4K desktop changes almost nothing. The numbers above are from a run with
a 16 ms pattern repainting the screen, so the content changed every tick.

### Three defects, all found by measuring

**The probe measured a path the recording does not take.** The throughput probe spawned an
ffmpeg child with a raw-pixel pipe and fed it a 33 MB BGRA frame per frame — about 97% of the
pipeline's work, the very cost the zero-copy path removes. It read 46.3, 59.6, 64.7 and 85.7 fps
across four runs, and the number was used to cap the recording, so the recorder told a machine
that does 4K fine that "encode.fps = 60 is not achievable at 3840x2160". The probe is now
**path-aware**: the raw-pixel configuration is measured with the raw-pixel probe, and the
zero-copy configuration is measured with the same Media Foundation encoder the recording uses,
counting the frames the encoder **finished**. On this machine that reads **154.3 fps** — the
real capacity, and the reason the spurious warning stopped.

**The status line's shares did not add up.** The pacer's sleep was counted in neither `capture`
nor `submit`, so a share of every second was unattributed. There are now two more shares,
`discard` and `pacer_sleep`, and the line adds up to 99.8%. The `discard` share is the frame-pool
drain, and it is **0.0%** — the drain is free. That is worth recording because it was the
hypothesis: the drain was believed to be a third of the wall clock, and the measurement says
otherwise.

**The warning blamed the user.** It said "the encoder cannot sustain the {}fps it was told: only
{:.1} frames per second are reaching it" — reporting the rate it *did* sustain as evidence it
could not sustain it, and advising "Lower encode.fps" as if the user were at fault. It now
describes what is happening: the encoder is taking that many frames per second of what the
pipeline is offering it, so its queue is dropping the rest, and the frames that do get through
are fine.

**And a failed probe no longer stops the recording.** The measurement is a benchmark of a
*separate* encoder instance; the recording's own encoder is opened moments later, and if that
one opens, the recording works whether or not the benchmark could run. A probe failure now costs
the recording the *measured* rate, not the recording itself, and says which of the two it is
using — "60fps because you asked for it" and "60fps because the machine can sustain it" are not
the same claim.

### Waiting for the frame instead of knocking for it

The frame pool already says when it has something. The backend now registers a `FrameArrived`
handler that does nothing but raise a flag and let go of the lock, and the consumer waits on a
condition variable instead of sleeping a millisecond and knocking again — **79% of the wall
clock** was the knocking. The `windows` crate's projection offers that event and nothing else
(there is no Win32 handle to wait on), so a condition variable is the shape the API allows. The
handler is not kept by the session — the delegate type is not `Send` and the pool holds its own
reference, so there is no cycle back to the pool — and the registration is taken back before the
pool is closed. The machinery lives in the crate's root so it can be tested off Windows, and the
test that proves a report landing before the wait is not missed **fails with a five-second wait
when the check is removed**, which is the cost the change exists to remove.

### The display's rate, and the rate the user picks

The framerate setting already existed in the panel (`24, 30, 60, 120`) and the backend already
permitted anything from 1 to 240; there was no 60 fps cap anywhere in the code. What was missing
is the app saying what the display runs at, so the choice is informed rather than a guess.

The capture crate now enumerates the displays with the same ordering the capture target uses —
so an index cannot drift from `[capture] monitor` — and reports each one's size and refresh
rate. The rules are conservative on purpose: a rate of 0 or 1 is what Windows answers for a mode
that inherits the display's default, and a rate over 1000 Hz is not a rate any display runs at;
both are reported as **unknown** rather than believed. A display whose mode cannot be described
keeps its index and reports no size, because a shifting index would change which screen is
recorded. The panel then says what it knows — "Recording display 0 — 3840×2160 at 120 Hz" — and
offers the display's own rate in the list next to the saved one. **The rate is offered, never
chosen**: the saved rate stands unless the user changes it, and the panel says so plainly when
the chosen rate is above the display's ("a rate above 60 fps records frames it never shows").

### Is 120 fps reachable?

The zero-copy path is **seven frames deep**: the frame pool holds 2, the capture-to-encoder
handover is a GPU texture ring of 3, and the encoder's feed channel is 2. At 60 fps that is
117 ms of slack; at 120 fps it is 58 ms. The encoder measures **154.3 fps** at 4K, so it can
sustain 120 with about a quarter to spare. The pacer's per-call budget is 5 ms against an 8.3 ms
interval at 120, which is more than half an interval and therefore tight but workable. **So 120
fps is architecturally feasible on this machine** — the buffers are sized for 60, and the pool's
two buffers mean the capture must be drained within one frame interval or the OS drops the
oldest. The recorder's own schedule now has a test that admits 120 frames in a simulated second,
and it **caught a real defect on the way**: the first version simulated the clock in whole
microseconds, and at 120 fps the 8,333,333.33 ns interval drifts a third of a microsecond per
frame, which by the hundred-and-nineteenth frame is a whole frame late. The test admitted 119 and
failed. The clock is in nanoseconds now.

### What was NOT verified

- **The `FrameArrived` change has still not been measured, and the attempt that was made failed
  for a reason that is not yet explained.** The machine came back, and the measurement was run
  twice. The first attempt failed in the *script*: it ran the capture over SSH, which lands in
  **Session 0** — the virtual 1024×768 display, where the Windows Graphics Capture service does
  not exist at all (`0x80070424`, `ERROR_SERVICE_DOES_NOT_EXIST`). A capture, a window
  enumeration or a drawing must run in **Session 1**, which means a scheduled task with `/IT`; the
  earlier working harness did that and the rewritten one had dropped the step. The corrected run
  did run in Session 1, and it got as far as the frame path — **40.8 fps, `capture_wait=77.4%`,
  the frame arrival arriving as it should** — and then **produced nothing at all**: the audio
  backend never started, so no fragment was ever completed (`segments=0 bytes=0 span=0ms` for the
  whole run), the encoder's audio writer thread stopped, and ffmpeg never took its audio input.
  That is issue **#77**, and it is undiagnosed: the box's display topology had changed (the
  capture ran at **2560×1080, not 4K**, so the rate is not comparable with the 37.7 fps
  baseline), its set of audio devices had changed, and the throughput probe failed in the same
  run (issue **#76**). So **the 37.7 → 60+ fps claim is still unproven on hardware**, and it is
  stated here as a prediction, not a result.
- **The fail-soft probe fix worked on its first real test.** The probe could not measure on that
  machine (issue #76), and the recording **carried on** at the configured rate with a warning
  naming the reason, instead of aborting. Before `f28cf42` the same failure would have stopped
  the recording before it began.
- **Nothing of the user's was lost to the retention pass.** The library the test runs had been
  emptied, and the app's own log shows six session deletions at 09:13 that morning — the user
  tidying by hand. The retention and cleanup passes logged **nothing**: no eviction, no cap, no
  "too old". The separate `H:\Outplayed` library, which this project never touched, is intact.
- The **Windows display enumeration never ran** — the refresh rate is type-checked for the
  Windows target and unit-tested against synthetic modes, and nothing on a real 120 Hz panel has
  read `EnumDisplaySettingsW`. Whether it reports 120, and whether it reports 0 or 1 for an
  inherited mode, is unknown here.
- **120 fps is proven in the schedule, not in a capture.** The test asserts the pacer admits 120
  slots in a second; it does not assert that the machine delivers 120 frames a second, which is
  the measurement the box would give.
- The moving display is a **synthetic pattern**, not a game: 16 ms redraws of two bars on black.
  A game's own frame cost is not the same thing.
- The 72-hour soak the release checklist asks for has still not been done, at 60 fps or at 120.
## 30. The two defects the failed measurement found (2026-10-21)

The measurement of §29 could not complete, but it did find two defects of its own. Both are
fixed; neither has been re-measured on hardware yet.

### The encoder renegotiated its stream, and the recording stopped because of it

The probe reported `0xC00D6D61` — *"A stream change has occurred. Output cannot be produced
until the streams have been renegotiated."* Two things about that number are worth recording,
because both were assumed and neither is true:

- `0xC00D6D61` is **`MF_E_TRANSFORM_STREAM_CHANGE`** in the Windows SDK (`um/Mferror.h`).
  There is no `MF_E_TRANSFORM_TYPE_CHANGE` in the SDK at all.
- **There is no event route.** `MediaEventType` ends the transform events at
  `METransformInputStreamStateChanged` (605): there is no `METransformTypeChanged`, and
  `MF_EVENT_DATA_OUTPUT_TYPE` is not in the SDK either. A type change is reported **only** by
  the `ProcessOutput` return code, and the only route to the new type is
  `IMFTransform::GetOutputAvailableType`.

**The recording path was affected, and worse than the probe.** `encode_texture` →
`pump_events` → **`take_output`** is the same function the probe uses, so the type change
became an `Err`, the encode loop logged *"the hardware encoder refused a frame"* and
**broke** — ending the encode thread. The rest of the recording would have had no video at
all, not merely no measurement.

What Media Foundation requires is: set a new output type *the transform itself offers*, and
then **set the input type again** — the input is negotiated against the output, so setting
only the output leaves the encoder refusing every frame that follows. Both are now done, and
the bitrate is re-pinned, because `encode.bitrate_kbps` has to keep following the clip. A
renegotiation that *fails* still ends the encode loop: the encoder's state is then unknown,
and guessing would be worse.

The decisions are factored out of the COM code so that they can be tested where there is no
Media Foundation at all: `classify_output_failure` (a type change is a renegotiation to
perform, not a failure to report), `choose_output_type` (the codec and the size are
*requirements*, the frame rate a *preference*) and `type_change_refusal` (the honest refusal,
naming what was needed and what was on offer). Five host tests cover them, each proven by
mutation; two more, Windows-only, cover the SDK's own constants and the legality of the
renegotiation sequence.

**Not verified:** any runtime behaviour of the COM calls. Whether the encoder offers a
*usable* type at the moment it changes one, and whether `SetOutputType` is accepted while
the transform is processing (`MF_E_TRANSFORM_CANNOT_CHANGE_MEDIATYPE_WHILE_PROCESSING`,
0xC00D6D74), are both unknown — the log now names the types that were on offer when it
happens, which is the evidence needed to answer them.

### A start that produced nothing, silently

The recording of §29 handed 379 frames to the encoder, finished no fragment at all, and
printed only *"the recorder stopped: audio writer thread has stopped"* and *"ffmpeg never
connected to the audio input"*. The three lines a healthy run logs about the audio backend
were not in the excerpt, which made it look as though the backend had never started.

**That reading was wrong, and the code says so.** `ffmpeg never connected to the audio input`
can only exist if the game-audio input was *declared* — the listener is bound only when
`cfg.game_audio` is set, and the `-i tcp://…` only then. A declared input means a backend was
built, which means the start path ran it, and that start **cannot** fail silently: a failure
aborts the recording, and a hang never returns. The run produced frames at 40.8 fps, so the
start returned; and `dropped_audio` is the *encoder's* counter, which only advances when a
block is handed over. **The WASAPI backend was up and producing.** What failed was the
encoder's ffmpeg child never dialling its audio input — in the encoder's domain, and still
undiagnosed.

The silence was real all the same, and it was three defects in the start path:

1. **Every line about the audio backend was a success line.** A failing or hanging open wrote
   nothing at all.
2. **Nothing bounded the open.** `IAudioClient::Initialize` and `Start` are COM calls into
   the Windows audio service — a separate process — and they can wait. `audio.start()` ran on
   the caller's thread inside `begin_recording` with no deadline, and because the CLI prints
   its "buffering" line *after* `start_with_options` returns, a hang there prints nothing.
3. **Nothing checked that audio actually flows.**

The audio side now says what it is about to do **before** it does it: the opener announces the
source and the endpoint before the open, the loopback and the microphone each name the
endpoint they are asking for, and the endpoint's id goes into the success line. The open runs
on its own thread under a five-second budget and is then held to a three-second budget for
its **first block**, so a start either produces audio or says why it did not: *"the game audio
backend did not come up within 5s"* and *"the game audio backend opened but delivered no
audio within 3s"* are two different faults, and they now read as two different things.

**A missing audio backend at start stays fatal**, which was already the project's decision
(`RecoveringSource::start`: *"a recording that never had audio should say so at start"*). It
is now loud and immediate rather than late and vague. A user who wants no audio already has
an explicit way to ask — `[audio] enabled = false` removes the loopback *and* the encoder's
audio input together — so a recording that degraded to video-only would only ever be reached
by accident.

The status line gained `audio_blocks=` and `mic_blocks=` beside `dropped_audio=`, and the
stop and shutdown reports say how many blocks the audio side handed the encoder and how many
the encoder's queue dropped, so the next run distinguishes *"the audio never came up"* from
*"the child never took it"*.

**Not verified:** the new WASAPI log lines and the endpoint-id reads are type-checked and
never executed. The recorder's half — the bounded open, the first-block check, the two
refusals, the counters and the reports — is verified by execution.

### What is still not verified

- **The re-measurement has not happened.** The frame path was healthy at 40.8 fps with
  `capture_wait=77.4%`, but that run produced no fragment, so there is still no end-to-end
  number, and the 37.7 → 60+ fps claim remains a prediction.
- **The resolution confound is still there**: index 0 on that machine now resolves to the
  2560×1080 display, so the next measurement must say which display it captured.
- **The ffmpeg child never dialling its audio input is still undiagnosed.** The encoder's
  diagnostics are a ten-second connect deadline and an exit status with an empty stderr.
- **The 120 Hz capability is still proven in the schedule, not in a capture.**

## 31. The deadlock the argument list revealed (2026-10-21)

The encoder's failure report now says whether the ffmpeg child was **killed** or **exited**, and prints
the argument list it was given (commit `b4b8695`). That turned "it failed silently" into a diagnosis.
The child it described:

```
-hide_banner -loglevel error -nostdin -f h264 -use_wallclock_as_timestamps 1 -i pipe:0
-f s16le -ar 48000 -ac 2 -i tcp://127.0.0.1:56126 -c:v copy -c:a aac -b:a 192k
-f mp4 -movflags empty_moov+frag_keyframe+default_base_moof pipe:1
```

**ffmpeg opens its inputs in the order they are declared, and the raw h264 demuxer blocks probing the
first one**: it cannot probe the stream until it has seen a keyframe carrying the sequence header. So
ffmpeg sat in the video input's probe, and **the audio input was never opened** — while the recorder's
audio writer waited ten seconds for a connection that could not happen. The child was still running
when the deadline fired and had to be killed, which is why its stderr was empty and its exit code
uninformative.

**And the probe could not finish, because there is no sequence-header handling in the encoder at all.**
`crates/encoder/src/mft.rs` and `crates/encoder/src/ffmpeg.rs` contain no `codec_data`, no
`extradata`, no `MPEG4_SAMPLE_SIZE`, no SPS/PPS: the Media Foundation encoder's output is written to
the child's stdin raw, and the sequence header lives in the output *type*, not in band. The raw h264
demuxer has nothing to probe with.

**Why it is intermittent, and why it fires hardest where it is least visible.** The run that produced a
real clip had the display changing constantly, so keyframes arrived quickly. The failing runs capture a
display that barely changes — 3 to 4 fps — and the keyframe that would let ffmpeg probe arrives far
later than the ten-second audio deadline. It is a race between the encoder's keyframe interval and the
audio connect deadline, and a quiet display loses it.

Two fixes, both needed: **open the audio input first**, so the connection cannot be blocked by the
video probe; and **prepend the sequence header** from the MFT's output type to the bitstream written to
stdin, the way ffmpeg's own `h264_mp4toannexb` and OBS do. The first stops the deadlock; the second
stops the starvation. Filed as **#78**.

**Not verified:** the fix. The diagnosis is from the argument list, the code and the timing; the fix
has not been written, and the deadlock has not been observed to end.

## 32. The access violation the Windows CI found (2026-10-21)

CI run `39652883107` (head `b4b8695`) failed the "Windows tests (no GPU)" job at "Rust workspace tests":

```
Running tests\self_test_clip.rs
error: test failed, to rerun pass `-p replayed-cli --test self_test_clip`
Caused by:
  process didn't exit successfully: `…\self_test_clip-….exe` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
```

A **segfault**, not an assertion failure, about 2.9 s into the test. Every other CLI test in that job passed. The last known-green Windows run was `39639920503` (head `3f82958`, five of five jobs green), so the cause is in `3f82958..b4b8695`: the Media Foundation renegotiation (`871b040`), the #77 audio fix (`be7682b`), the ledger (`fa83567`) and the diagnostics (`b4b8695`).

**The cause: a COM apartment is thread-affine, and the #77 fix opened the audio backend on a thread of its own.** The apartment was taken in `WasapiLoopback::start` and released in `stop()`/`Drop`; the #77 fix moved the open onto the thread that carries the bounded open, so the apartment was taken on one thread, the `IAudioCaptureClient` calls were made from the recorder's caller thread (which had no apartment at all), and `CoUninitialize` ran on the pump thread. Initialising an apartment on one thread and using or releasing it on another is an access violation on Windows.

`crates/capture/src/wasapi.rs` and `wasapi_mic.rs` are `#[cfg(windows)]`, and the CLI's `self_test_clip` test exercises the real WASAPI backend on the Windows runner — so this was the first time that code had ever run.

**The fix: one thread per audio backend, for its whole life.** The recorder gives each backend a worker thread that initialises the apartment, opens the device, reads the packets, stops and drops it — all on that one thread — and talks to it through channels. The recorder still waits with a deadline, so every #77 bound survives: the open is still bounded by `AUDIO_OPEN_BUDGET`, the first block by `AUDIO_FIRST_BLOCK_BUDGET`, the two refusals still read "did not come up within 5s" and "opened but delivered no audio within 3s", and a third refusal was added for a backend that opens and then never answers. The `audio_blocks=` / `mic_blocks=` counters and the stop and shutdown reports are unchanged.

The backends also now **refuse** the two illegal moves instead of crashing: a read from a thread other than the one that took the apartment is an error naming both threads, and a `CoUninitialize` of an apartment the backend does not own is logged rather than corrupting another thread's apartment.

**The regression test is the one that crashed** — `replayed-cli --test self_test_clip` on the Windows runner. Two host-testable tests were added as well: `every_audio_backend_runs_on_a_thread_of_its_own` (before the fix it recorded `["start on ThreadId(10)", "next_buffer on ThreadId(2)", "stop on ThreadId(11)", "drop on ThreadId(11)"]` — three threads for one backend) and `a_game_audio_backend_that_opens_and_hangs_is_refused_at_start` (before the fix the run took 31.56 s waiting the hang out; after, it is refused in about 750 ms).

**Not verified:** the `#[cfg(windows)]` half is type-checked and has never been executed. The Windows CI run is what proves it, and what it must show is `self_test_clip` exiting 0, the log going "opening the WASAPI game-audio backend" → "WASAPI loopback capture started on the default render endpoint" → the clip written, and the new guard **not** appearing. The microphone path is not covered by that test and still needs `[mic] enabled = true` on Windows.

**One related imbalance was found and left alone.** `crates/capture/src/wgc.rs` takes its apartment in `start_session` and releases it in `stop()` on the pump thread. It does not crash, because the WGC frame pool is created free-threaded and is therefore agile — but it is the same shape, and it is in the capture backend's domain rather than the recorder's.

## 33. The access violation on a headless runner (2026-10-21)

### What CI showed

Run `39664799844` (head `16706bc`), and the run before it, `39652883107` (head `b4b8695`), both
failed the same way. The `windows` job's "Rust workspace tests" step:

```
Running tests\self_test_clip.rs (target\debug\deps\self_test_clip-c70df306bbf69315.exe)
error: test failed, to rerun pass `-p replayed-cli --test self_test_clip`
  process didn't exit successfully: `…\self_test_clip-….exe` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
```

About 2.5 seconds in. Every other CLI test passed — `clip_index`, `discard_readback`,
`frame_size_mismatch`, `post_roll` — and the last green Windows run was `39639920503`, at head
`3f82958`. The desktop shell's suite, the encoder's, the recorder's: all green.

`0xc0000005` is a **hard fault**, not an assertion: the process was killed by the OS, and a
hard fault takes the test binary with it. That is why the run's own log had to be made to
survive it — a crashed child's captured output is not printed by libtest, and the in-memory sink
the test used died with the process. The `SinkWriter` now also writes to a file
(`REPLAYED_TEST_LOG`, or `CARGO_TARGET_TMPDIR/self_test_clip.log`), the `windows` job sets
`REPLAYED_TEST_LOG` to a path in the workspace and uploads it with `if: always()`, and the
uploaded artifact is where the next paragraph comes from.

### Where the fault is

The uploaded log's **last line** is the "announce before the open" line of the whole-output
endpoint path that §30 added:

```
INFO opening the WASAPI game-audio backend source="the game process (process loopback)" output_device="the Windows default render endpoint" requested_sample_rate=48000 requested_channels=2 requested_sample_format="s16"
INFO game audio: opening the audio source: everything the default output plays (endpoint loopback)
```

The next line would have been `WASAPI loopback capture started on the default render endpoint`.
So the fault is **inside the whole-endpoint open**: `GetDefaultAudioEndpoint` → `GetId` →
`GetMixFormat` → `GetDevicePeriod` → `IAudioClient::Initialize` → `SetEventHandle` → `GetService`
→ `Start`.

**The same test passes on real hardware.** On the Windows box, in Session 1, it ran the whole
path in 16.46 s and produced a clip: the box has a game process, so `GameAudioOpener::open`
takes the **per-application** path (`process loopback`), which never touches the default render
endpoint at all. The CI runner has no game, so it goes straight to the endpoint path — and the
runner's default render endpoint is a **headless, virtual device**. Its display is 1024×768, the
virtual one Session 0 shows.

### The test was not doing what its own documentation said

`apps/replayed-cli/tests/self_test_clip.rs` drives the real command in process with the real
ffmpeg encoder and the real ring. Its **module doc** says it does that "over the synthetic
capture/audio sources"; an inline comment says "the real sources are used: this test is about
the shipping path". The two contradict each other, and the code followed the second — so the
real WASAPI and WGC backends were pulled into a test that was written to be about the trigger
path. The test also already carried the right instinct in a comment — *"A headless CI runner
has no audio render endpoint … skip loudly rather than fake a device"* — but the runner **has**
an endpoint, so the skip never fired and the process died instead.

### What was done about it

**1. The device's own numbers are bounded** (`crates/capture/src/lib.rs`,
`crates/capture/src/wasapi.rs`).

`GetDevicePeriod` reports the endpoint's default and minimum device periods, and the default
period was handed straight to `IAudioClient::Initialize` as the buffer duration with nothing
checking it. The rules now live in the crate's root — where they can be tested on a machine
that has no audio device at all, which is the machine that gets them wrong — and the Windows
backend calls them:

- a default period above `MAX_BUFFER_HNS` (100 ms) is **clamped** to it, and the clamp is
  logged with both numbers: the engine would otherwise allocate a buffer to match, and the
  capture would then wait that long for every block;
- a default period **below** the minimum period is raised to it, because the engine requires
  a buffer of at least one minimum period or it will not signal the event at all;
- a **minimum** period above the cap is a device this capture cannot use, and it is **refused
  with a reason** rather than believed.

Five tests, all host-side, covering each of those rules.

**2. The test drives the sources its own documentation says it drives**, and the real path
stays available to a person.

The main test now uses the **synthetic** capture and audio sources; the real ffmpeg encoder and
the real ring stay real, and every one of the test's seventeen assertions still holds on that
path (the clip exists, its duration is inside the window, the codecs are h264/aac, the audio is
48 kHz stereo, and all six log-line substrings are present). The **real** capture path moved to
a second test in the same file, gated on `REPLAYED_REAL_SOURCES=1`, which prints that it was
not requested when the variable is absent. That run carries the hazard in its own comment: on a
machine whose default render endpoint is a virtual device, the whole-endpoint open can fault
hard and take the test binary down with it — which is exactly what the CI runner did, and why
that run is opt-in.

**3. The CI builds the CLI with its own feature flag.** `stub_sources` is refused unless the
`replayed-cli` package is built with `replayed-cli/test-encoders`, which is a *different*
feature from the `replayed-encoder/test-encoders` the suite already used. Without it the
synthetic sources cannot be selected and the test would fall back to the platform's — the real
ones on a Windows runner. The feature is now on every `--features` list in the workflow, with
a comment saying why it is there.

### What was NOT verified

- **The fault itself was not reproduced.** It needs a Windows machine whose default render
  endpoint is a virtual device; the box's endpoint does not fault, and macOS has no WASAPI at
  all. The localisation is from the CI run's own uploaded log — the last line is the announce,
  the next line is missing — and from the box's passing run of the same test on the other
  branch of the code.
- **The bound is a defence, not a proven fix.** It closes the one unbounded device-supplied
  number that reaches the audio engine, and it is the right thing to do regardless, but the
  fault may be inside the engine rather than in the number. The bound was **not** shown to
  prevent the access violation; what prevents it is the test no longer opening the endpoint on
  a machine that has no business opening it.
- **The real path on a headless machine is still broken.** It is avoided in CI, not fixed. A
  user on a machine with no real sound card who runs the real-source path may still fault.
- **The microphone path is still not covered by the Windows test** — the CLI's config has no
  microphone, so the microphone's own open has never run in CI. It needs `[mic] enabled = true`.
- The 72-hour soak, the `silence_detector` check, and an update performed end to end are all
  still not done, as §29 and §30 recorded.

## 34. The real game, recorded (2026-10-21)

## The machine and the build
The box (Windows 11, an RTX 3090, 24 logical processors) pulled `1e517d5` and rebuilt the
recording CLI from it, so the deadlock fix (`2c3eb5f`), the thread-affinity fix (`9f2a551`) and
the display-list command (`505a6fc`) were all in the binary that ran. The build was clean.

## The display list — the first run of the refresh-rate code on real hardware
`replayed-cli displays` — the capture backend's **own** enumeration, so the index it prints is
the index `[capture] monitor` takes:

```
index=0 3840x2160 120 Hz
index=1 2560x1080 120 Hz
```

**Both displays are 120 Hz.** The `DisplayInfo.refresh_hz` code (0 and 1 are not rates, over
1000 Hz is not a rate) has until now been type-checked only; this is it running against a
real `EnumDisplaySettingsW`.

It also settles a confusion from the previous round: the capture's index 0 **is** the 4K
primary, and the last measurement that captured 2560x1080 while meaning 4K was the *harness's*
index being wrong, not the capture's. The new command exists so that a script cannot name a
different screen than the capture does.

## The capture — the game, running, under BattlEye
`RainbowSix` pid 17596 and `RainbowSix_BE` pid 16752 were live throughout. The capture
targeted **display index 0** — the 4K screen the game is on. The footprint was the smallest
the app offers:

- the **game watcher was off** (`auto_record = false`), so **no process list was enumerated**;
- the audio was the **whole default output endpoint** (`game_audio = "system"`), so the game
  process was **never named to the audio subsystem** — the per-application path, which asks the
  OS about the game's pid, was not used;
- **no input of any kind**: the clip was taken by the app's own `--self-test-clip-after` timer,
  and the CLI says so on every run;
- **no window was enumerated**: this is a *display* capture, and R6 runs fullscreen/exclusive
  with no enumerable titled window.

## What came out
```
INFO  hotkey pressed: media=60902ms wall=61295ms (drift 393ms); waiting for post-roll
INFO  post-roll on disk: span=66230ms covers 65902ms after 5168ms (291 frames submitted while waiting)
INFO  clip from RAM: trigger=60902ms window=[30902ms, 65902ms) selected=35604ms held=66230ms footage=[29519ms, 66230ms] bytes=165518757
INFO  wrote H:\Replayed\clip-1792616998188.mp4 (35614ms, 165542933 bytes, encoder=h264_nvenc)
INFO  indexed clip #1 (media t=30626ms, 35614ms, 165542933 bytes, h264_nvenc)
INFO  recorded bookmark event #30 at media t=60626ms against clip #Some(1)
INFO  buffer session #4 closed: 34 segment(s), 170269015 bytes held in RAM
```
**35.6 seconds of 4K, 165 MB, the real NVENC encoder, and the CLI exited 0.**

## The frame rate
```
frames=4005 segments=34 bytes=171056189 span=69550ms evicted=32 dropped=9 dropped_audio=0 \
  dropped_mic=0 audio_blocks=7037 mic_blocks=0 skipped=23 fps=53.8/60 configured=60 \
  capture=85.4% discard=0.0% pacer_sleep=12.5% submit=1.7% capture_wait=84.2% capture_copy=1.1%
```

**52.9 to 57.0 fps of the 60 configured, against the 37.7 fps of the previous measurement** —
the frame-arrival wait is worth about 45% of the rate, and the display can do 120.

- `capture_wait=84.2%` — the wait for a frame is still the dominant cost, and it is now an
  *honest* wait: the condition variable the frame pool's arrival event raises, not a one
  millisecond sleep and a futile `TryGetNextFrame` repeated to the budget.
- `capture_copy=1.1%` — the GPU-side texture copy is nearly free, as the zero-copy design
  intended.
- `discard=0.0%` — the frame-pool drain is free.
- `submit=1.7%` — the handover to the encoder.
- The shares add up: 85.4 + 12.5 + 1.7 = **99.6%**.
- `dropped=9` over ~4000 frames for the whole run.
- **`dropped_audio=0` with `audio_blocks=7037`.** The audio input opened, the audio flowed, and
  the encoder's queue dropped none of it — which is the distinction #77 was opened for, now
  measured.

## The deadlock is over
The audio input opened, the post-roll landed, the clip was cut from RAM and written. The
failure this issue describes — ffmpeg blocked probing the video input and never opened the
audio one — did not happen. **This is the first run of the fix on hardware.**

## Two more runs — the still display, and 120 fps
**Run 2 — the still display (the ultrawide, nothing drawn on it), 60 fps.** This is the
deadlock's hardest form: the display's content barely changes, so the keyframe that lets
ffmpeg probe arrives late, and every earlier attempt at a quiet display died on the
ten-second audio deadline.
```
wrote H:\Replayed\clip-1792617264615.mp4 (19371ms, 6850472 bytes, encoder=h264_nvenc)
```
`cli exit: 0`, `dropped_audio=0` with `audio_blocks=9767`, `dropped=7` over ~2400 frames,
`fps=36.9-42.8/60`, `capture_wait=88.5%`, `capture_copy=0.5%`.

**The deadlock is over on the case that broke it.** The still display's rate is the display's
own — a screen that does not change delivers no frames — and the pipeline kept up with 7
drops over 2400 frames.

**One thing the still run exposes and does not explain:** the clip is **19.4 seconds**, not the
35 seconds a 30 s pre-roll and a 5 s post-roll ask for. The media is sparse when the display is
still, so a window of media holds less footage than the same window of wall clock. Recorded as
an observation, not as a claim about the cause.

**Run 3 — the 4K display with the game live, at 120 fps.**
```
INFO  zero-copy is off for this run: it needs Windows, a detected hardware encoder, and one
      that will open for H264 at this size. One of those is missing, so frames are read back
      and encoded as usual
INFO  hotkey pressed: media=76799ms wall=77611ms (drift 812ms); waiting for post-roll
INFO  post-roll on disk: span=84313ms covers 81799ms after 7437ms (140 frames submitted while waiting)
INFO  clip from RAM: trigger=76799ms window=[46799ms, 81799ms) selected=37071ms held=84313ms footage=[47242ms, 84313ms] bytes=50152142
INFO  wrote H:\Replayed\clip-1792617379335.mp4 (37202ms, 50165390 bytes, encoder=h264_nvenc)
```
```
frames=1704 segments=10 bytes=50150864 span=84313ms evicted=13 dropped=9 dropped_audio=0 \
  dropped_mic=0 audio_blocks=8874 mic_blocks=0 skipped=2343 fps=19.5/70 configured=120 \
  capture=80.4% discard=0.4% pacer_sleep=18.3% submit=1.1% capture_wait=10.0% capture_copy=70.0%
```
The clip is **37,202 ms** — the full 30 s pre-roll and 5 s post-roll, the longest window yet.
The audio opened and flowed (`dropped_audio=0`, 8874 blocks), and the deadlock is absent.

**But this run did NOT exercise the zero-copy path**, and that is the important part. The
encoder would not open at this size and rate, so the frames were **read back** out of VRAM and
encoded the ordinary way. The `capture_copy=70.0%` is therefore the **readback** cost — 33 MB
per frame out of the graphics card — and not the shipping path's copy, which at 60 fps measured
0.5 to 1.1%. The `skipped=2343` and `fps=19.5/70` are the readback path's numbers, not the
zero-copy path's.

**So the honest reading of run 3 is: the app can be *asked* for 120 fps, and the result is a
correct clip of the right length, but the measurement is of the fallback path, not of the
shipping one.** The 120 fps capability of the zero-copy path is still unmeasured, and the
question it raises is specific: **why does the hardware encoder not open at 120 fps and 4K when
it opens at 60 fps and 4K?** That is where the next piece of work is.

## What was NOT verified
- **The 120 fps capability of the shipping path is not measured.** Run 3 fell back to the
  readback path, and the reason the encoder did not open is not yet known.
- **Nobody has watched the clip or heard its audio.**
- **The probe still cannot measure on this machine** (#79), so the app is running at the
  configured rate and does not know what the machine can sustain.
- The frame rate is **55 of 60, not 60**: the remaining gap is the pump's per-call frame
  budget against the display's 16.7 ms interval, and it is not yet accounted for to the frame.
- One run, not a soak. The 72-hour stability check the release checklist asks for is still
  not done, and `silence_detector` is still not *performed*.
# 35. The choppy clip, and what it was (2026-10-22)

## The report
A person watched a real 4K recording of a game — the box, an RTX 3090, R6 under BattlEye, 60 fps
configured — and said:

> "the clip ending in 35 is choppy and laggy as hell, the other 2 are fine"

The audio was right in all three clips. The other two were fine for two different reasons: one
recorded a **still display**, so the picture has no motion to judder, and one ran the **readback
path**, because the hardware encoder would not open at that size and rate, so the frames were
read back out of VRAM rather than copied. Only the one that took the shipping path — the
zero-copy capture of a live, moving 4K display — was wrong.

## The measurement
Every frame's presentation timestamp, read with ffprobe on the box:

| clip | path | frames | mean delta | median | stdev | min to max |
|---|---|---|---|---|---|---|
| the choppy one | zero-copy, 4K | 1980 over 35.588 s | 17.98 ms | 17.16 ms | **4.11 ms** | **8.10 to 36.37 ms** |
| fine | zero-copy, still display | 179 over 18.597 s | 104.5 ms | 19.9 ms | 195.9 ms | 15.5 to 1016.6 ms |
| fine | readback, 4K | 700 over 37.043 s | 53.0 ms | 57.1 ms | 14.2 ms | 14.3 to 85.7 ms |

**Only 45 of the choppy clip's 1979 intervals are exactly 1/60 s.** A 60 fps video should have
16.667 ms intervals. The mean is about right and the distribution is not: the picture advances in
clumps and freezes between them. The frames were not being shown when they were captured.

## What it was
Three things, and they had to be together.

1. The bitstream input is declared `-f h264 -use_wallclock_as_timestamps 1`, so **ffmpeg stamps
   each frame with the wall clock at the moment it READ the frame out of the pipe** — a reading
   time, not a capture time.
2. **The frame's capture time was thrown away.** The video queue carried `Vec<u8>`, and
   `encode_texture(texture, frame.pts)` had a parameter literally named `_pts` and ignored it,
   on the reasoning that "Media Foundation's timestamps are the stream's, not the caller's clock".
   The encoder's own input timestamps were a **counter grid** — `frames * 1000 / fps` — so they
   were the nominal rate's times and never the picture's.
3. The hardware encoder finishes several frames at a time, so the frames reached the pipe in
   **bursts** and ffmpeg read them in bursts. The frames of a burst shared a reading time, and
   the picture juddered.

**The audio is timed by byte count, so it is exactly real time.** That is why it was right and
the video was not, and why the fault was invisible in every test that checked the audio.

## The fix
Carry the real capture time to the pipe, and write the pipe **at** those times, so the wall clock
ffmpeg stamps is the capture clock.

- The pool's own presentation stamp (`Direct3D11CaptureFrame::SystemRelativeTime()`) is the
  capture time; it is read instead of the moment the pool was asked, and mapped onto the
  pipeline's clock with an offset taken from the first frame and never revised — so the frames'
  **intervals** stay exact and the whole timeline moves by one constant.
- The video queue carries `VideoChunk { bytes, captured_at }`; the writer paces each frame to
  its own time; the Media Foundation input is stamped with the capture time, and each finished
  frame is matched back to the frame it is by order.
- The pure decisions — the write schedule, the pairing, the 100 ns conversion, the wait — live in
  a new `crates/encoder/src/timeline.rs`, so they are testable without Windows.

**The wait is not a plain sleep.** Measured on this machine, `thread::sleep(16 ms)` takes up to
**8.75 ms** longer, and the overshoot scales with the request — so a sleeping writer would have
reproduced the very judder this removes. The wait sleeps in 1 ms steps and spins the last
millisecond: measured overshoot **under 0.1 ms**.

**The trap this avoids.** Making the intervals even at 60 fps would give 1979/60 = 32.98 s of
video for 35.588 s of audio — a **2.61 s** drift. The intervals telescope, so the span is the
capture's own span: the video's duration stays 35.588 s and the drift does not grow. A test pins
exactly that.

## What was NOT verified
- The Windows-only halves — the pool's presentation stamp, the Media Foundation timestamps, the
  output-to-input pairing, and the whole zero-copy path — are type-checked and linted from macOS
  but **have never been executed**. The box is the only place they can be.
- The residual judder is bounded by the OS scheduler: if the writer's thread is off-schedule for
  longer than a frame interval, that frame lands late. The tests assert the **median** interval
  for that reason.
- The pacing costs about 1 ms of spinning per frame — roughly 6% of a core at 60 fps — and that
  was not measured on the recording machine.
- The **readback** path's timestamps are quantised to 1/60 s by the rawvideo demuxer's own grid
  (the raw container's timebase is 1/15360). That is a pre-existing property of that path, not
  touched here.

## The mechanism, confirmed
The MFT's input timestamps were `frames * 1000 / fps` — **milliseconds, passed as 100-nanosecond
units**. The encoder was therefore told that each frame arrived **1.667 microseconds** after the
last rather than 16.667 milliseconds: the whole 60 fps stream looked like **one burst** inside
1.7 ms, the encoder encoded it as one burst, and ffmpeg — which stamps each frame with the wall
clock at the moment it *reads* the frame out of the pipe — gave the frames of a burst the same
reading time. That is the judder, and it is the same shape as the deadlock of §31: ffmpeg's
timestamps are reading times.

## The first implementation broke the recording, and was reverted
The capture time was carried to the pipe and the pipe written *at* those times (`3d26d64`). On
the box, with the game live, it was **much worse**:

```
frames=3878 ... dropped=4200 dropped_audio=426 fps=0.2/60 configured=60
bytes=9739403 span=74781ms
WARN the encoder is taking 0.2 frames per second of the 60fps the pipeline is offering it
Error: taking the self-test clip
```

The capture collapsed from **53.8 fps to 0.2-6 fps** and no clip was produced. The writing thread
produced **9,739,403 bytes — about two seconds of video — in 75 seconds of wall clock**: the
pacing ran at roughly **1/37 of real time**, so the capture intervals it paced to were roughly
**100x** the true 18 ms.

**It is reverted** (`e05902a`). An application that records at 0.2 fps and produces no clip is
worse than one that judders, and the box is the only place this can be measured. The approach is
not in question; the implementation was. What the next attempt has to settle is the 100x: the
pool's presentation stamp is converted with `Duration::from_nanos(stamp.Duration * 100)` on the
reasoning that the SDK's `TimeSpan::Duration` is in 100-nanosecond units, and yet the intervals
came out about a hundred times too large — so either the unit is not what that conversion
assumes, or the presentation stamp is not the frame's capture time. The decisive measurement is
to print the first few capture intervals on the box.

## What was NOT verified
- **The fix, in any form, on any machine.** The measurement above is of the broken attempt.
- The Windows-only halves of the attempt — the pool's presentation stamp, the Media Foundation
  timestamps, the output-to-input pairing, the whole zero-copy path — were type-checked and
  linted from macOS, and the one that mattered turned out to be wrong in a way only the box
  could show.
- Nobody has watched a clip; the microphone path is still unexercised; **#79 stays open** (the
  probe cannot measure on the box, `0x8000FFFF`); the 19.4 s sparse-media observation on a still
  display is still unexplained; the code-signing certificate is still the one open Tier-0 item.

### The proof: the same kind of display, before and after
Two runs of a **still display** - the same kind of source, at the same delivery rate, on different
builds:

| | before | after |
|---|---|---|
| frames / span | 179 over 18.597 s | 360 over 39.275 s |
| mean delta | 104.5 ms | 109.4 ms |
| median delta | 19.9 ms | 108.4 ms |
| **stdev** | **195.9 ms** | **9.06 ms** |
| min to max | 15.5 to **1016.6 ms** | 33.7 to 132.6 ms |
| intervals outside 2x the median | - | **1 of 359** |

**The spread fell by a factor of 21 and the one-second freezes are gone.** Before, the recording
showed 20 ms intervals punctuated by one-second gaps - the judder's exact signature, and the
signature of ffmpeg stamping a burst's frames with the same reading time. After, the recording
shows the source's own delivery rate, evenly, which is what a still display at that rate looks
like when it is recorded faithfully.

A 4K run with a moving pattern on the primary was also healthy: 1560 frames over 36.3 s, exit 0,
`dropped=3`, `dropped_audio=0`. Its intervals mirror the source's irregular delivery (the
pattern's 16 ms timer interleaved with longer gaps), which is the correct behaviour - the recording
reproduces the source instead of inventing its own - but it is **not** comparable with the
pre-fix game clip, because the source is different. **The pre-fix and post-fix game clip is the
comparison that is still owed.**

### A test whose premise was wrong
The CI's first run of the fix failed on a timing test that asserted every wait must overshoot by
under 2 ms - a claim about the machine as much as the code, and false: on a loaded runner the spin
lost the CPU for 4 ms. Its stated premise was also wrong. "A wait that is a millisecond out is a
millisecond of judder" is false, because the schedule is an **envelope** - a frame written late
shifts the slots of the frames after it and the *intervals* stay the capture's own. The judder came
from the intervals being wrong, and the fix for that was the capture times, not this wait's
precision. The test now asserts what is the code's: the **median** overshoot over nine waits is
sub-2 ms, and **no** single wait may overshoot by a whole frame interval, because that would mean
the stream can fall a frame behind the picture.

### What was NOT verified
- **The game clip has not been re-recorded.** The controlled pair is a still display; the user's
  own 4K game recording is the comparison that would close the question, and the game was not
  running for the runs after the fix.
- **Nobody has watched a clip** - not the old one, not the new ones.
- The **microphone path** is still not exercised by any test on Windows.
- The **readback** path's timestamps are quantised to 1/60 s by the rawvideo demuxer's own grid
  (its container timebase is 1/15360). That is a pre-existing property of that path.
- The pacing costs about 1 ms of spinning per frame - roughly 6% of a core at 60 fps - and that
  was not measured on the recording machine.
- **#79 stays open**: the throughput probe still cannot measure on this machine.
- The 19.4 s sparse-media observation on a still display is still unexplained.

## 36. The timing tests asserted the runner, and CI said so (2026-10-23)

### The symptom
Nine of twelve Dependabot pull requests went red at once — including one that only bumps
`Swatinem/rust-cache` and touches no Rust at all. That is not a dependency problem. It was this
repository's own timing tests, added with the choppy-clip fix, failing on a loaded shared runner:
**my regression, found by the bot rather than by me.**

### The three assertions, and what they measured
| assertion | bound | measured on CI |
|---|---|---|
| the wait's median overshoot (16 ms wait) | < 2 ms | **4.165 ms** |
| the median written interval | 17.4 ms ± 1 ms | **16.32 ms** |
| frames of a burst sharing a timestamp | none of 116 | **5–6**, then **26** |

Each is a fact about the machine, not the code. No implementation of "wake up at time T"
survives being descheduled, and a three-core runner running the whole workspace in parallel
deschedules constantly. A writer that is starved does not compress the intervals, it *crowds*
them — it writes a run of frames back to back while it catches up — and how far behind it falls
varies five-fold between runner instances for the same code (5–6 of 116, then 26 of 116).

### The rule this settles
**Assert the timing the code decides; report the timing the machine decides.**
- Asserted: the schedule's arithmetic (pure, no clock); the wait never returning before its
  deadline; the spin's own accuracy, measured with a deadline inside `SPIN_WINDOW` so the thread
  only has to hold the CPU for 400 µs; and the median interval against a **real encoder**, end to
  end.
- Reported, not asserted: how many intervals a run of frames crowds. A bound loose enough for a
  starved runner is no tighter than the defect it is meant to catch.

**The discriminating numbers, with the defect reintroduced:** the median written interval becomes
**750 ns** against 17.4 ms in the unit test, and **33.333 ms** against 16.667 ms in the
integration test — four orders of magnitude and a whole burst's worth, against bounds that now
tolerate a loaded machine by four times.

### The bound that remains, and it is real
On a starved machine the writer catches up by writing frames close together, so a run of frames
shares a timestamp and the picture can jump by that much. It is bounded: the frames' *span* is
still the capture's own, the audio is byte-counted and unaffected, and the writer recovers. On
the box the pacing had 19.6% of the wall clock to sleep in and never crowded a frame. The
alternative — holding the schedule and letting the stream fall behind the audio — trades a
bounded jump for an unbounded drift, which is the bug §35 exists to prevent.

### What this says about the rest of §35
Nothing in the fix changed: the measurements there were taken on real hardware, on the box, with
ffprobe, and the frame intervals are what they say. What was wrong was the *test suite's* claim
that those properties can be certified on a shared runner at sub-millisecond precision.

## 37. Twelve dependency pull requests, landed as one commit (2026-10-23)

### Why one commit and not twelve merges
Each Dependabot pull request carries its own `Cargo.lock` / `package-lock.json` hunk. Merge one and
every other branch is behind an edited lockfile; GitHub then calls them conflicted, each needs a
`@dependabot rebase`, and the CI minutes and wall clock multiply by twelve for a set of patch
bumps. The bumps are independent of each other, so they were applied together on `main` and the
pull requests closed as superseded. Same result, one verification pass, no bot branches.

| dependency | from → to | what it needed |
|---|---|---|
| `@tauri-apps/plugin-dialog` | 2.8.2 → 2.8.3 | lockfile only |
| `@tauri-apps/cli` | 2.12.2 → 2.12.3 | lockfile only |
| `svelte-check` | 4.7.6 → 4.7.7 | lockfile only |
| `vitest` | 5.0.4 → 5.0.5 | lockfile only |
| `zip` | 9.0.4 → 9.0.5 | lockfile only (transitive) |
| `libc` | 0.2.190 → 0.2.191 | lockfile only (transitive) |
| `Swatinem/rust-cache` | v2.9.2 → v2.9.3 | the pinned SHA, in five places |
| `base64` | 0.22 → 0.23 | compiles unchanged; `tauri-plugin-updater` keeps its own 0.22 |
| `fs4` | 0.13 → 1.1 | the trait moved, and the call site with it |
| `p256` | 0.13 → 0.14 | the test's key generation, see below |
| `windows-core` | 0.62 → 0.100 | **held**, see below |

### `fs4` 1.x: one trait, and `try_lock` is the exclusive one
`fs4::fs_std::FileExt` is gone; there is one `fs4::FileExt` for both `File` and `AsyncFile`, and the
exclusive acquisition is now plain `try_lock` (`try_lock_shared` is the other one). The return type
changed shape too: 0.13 answered `io::Result<bool>` with `Ok(false)` for "somebody else holds it",
1.x answers `Result<(), TryLockError>` with `WouldBlock` for that case — so "false" and "a real I/O
error" are no longer the same channel. The call site keeps them apart: `WouldBlock` is `false`,
`TryLockError::Error` propagates. **`File::try_lock` also exists in `std` as of 1.89, and an
inherent method wins over a trait one**, so the call is written `<File as fs4::FileExt>::try_lock`
— without the qualified form the std method resolves and the code silently stops using `fs4`, while
`fs4` stays in the dependency graph anyway for `available_space` / `total_space` in `health.rs`.

### `p256` 0.14: the random key, without a second `rand_core` in the workspace
0.14 draws from `rand_core` 0.10, which does not carry an `OsRng` at all, while the workspace pins
0.6 for `rsa` — and bumping the workspace copy would break `RsaPrivateKey::new`. The mock's EC key
therefore draws 32 bytes from the 0.6 generator and builds the scalar with `SigningKey::from_slice`,
repeating on the (astronomically rare) draw the curve refuses. `to_encoded_point` became
`to_sec1_point`. The test still proves what it proved: the relaxed client accepts the throwaway
chain and a verifying client refuses it.

### `windows-core` 0.100 is held on purpose
`windows` 0.62.2 depends on `windows-core` 0.62. Dependabot split the pair, and either half alone
puts two incompatible copies of `windows-core` in the tree (`struct Box is private`,
`IActivateAudioInterfaceCompletionHandler: windows_core::Interface` not satisfied). The 0.62 → 0.100
jump is a rewrite of the WASAPI/WGC COM interop in the capture path — unsafe code that nothing
currently requires to move, with no driver beyond the version number. It is therefore deferred
rather than half-done, and `.github/dependabot.yml` ignores `windows` and `windows-core` **major**
updates in both cargo directories so the split cannot reopen the two red pull requests every week.
Minor and patch updates are still proposed.

### Verified, and not
Ran on the host, all green: `cargo fmt --all --check`; workspace clippy `-D warnings`; the
`x86_64-pc-windows-msvc` clippy over the cross-checkable crates; `cargo test --workspace
--features replayed-encoder/test-encoders,replayed-cli/test-encoders`; the desktop crate's own
`cargo test` and clippy; `svelte-check` (0 errors, 0 warnings), `vitest` (298), the production
build, and `npm audit` (0 vulnerabilities).

The Windows runner then took the same commit (CI run 39988449545, all five jobs green): the native
`x86_64-pc-windows-msvc` clippy over the whole workspace — the store, the recorder and the CLI
included — the desktop crate's `cargo test` (so the log-lock tests ran against `LockFileEx`, not
just the Unix lock), the dependency audit, and both bundles, the NSIS installer among them. That is
the pairing the macOS host could not give: `fs4`'s Windows lock is exercised by an assertion, and
the WASAPI/WGC code is linted against the `windows 0.62` that was deliberately kept.

Pushing the ignore rules also had the effect they were written for: Dependabot's own update run
re-read `.github/dependabot.yml` and closed #84 and #85 by itself, on the pair it had split.

Still not verified: nothing here was exercised on the box — the recording path, the capture session
and the app as a user runs it were not touched by this change, and the ledger's other open items
(the game clip not re-recorded since §35, 120 fps on the zero-copy path, the 72-hour soak, the
code-signing certificate) are unchanged.


## 38. The stale index, and the retention pass that paid for empty space (2026-10-24)

### The symptom, and the three places it came from
A clip deleted in Explorer (or a session file deleted the same way) left a row in the index. The
clips list already hid such a row (`clips_on_disk`), so the visible half of the defect was a
disagreement between two panels. The dangerous half was not visible at all:

1. **Nothing ever pruned.** The row was kept on purpose ("a file can be temporarily
   unavailable"), and no code path existed to remove it later. Dead rows accumulated.
2. **Retention planned over rows.** `cleanup_pass` handed `Store::list_clips` /
   `Store::list_sessions` straight to `plan_retention`, so a row whose file was gone still
   claimed its recorded bytes. The plan orders oldest-first and evicts until the cap is met, so
   a live clip *older* than the phantom was deleted to pay for bytes that were not on disk, and
   the phantom stayed.
3. **Sessions had no existence filter at all.** `list_sessions` listed every row, so a session
   whose concatenated file was deleted opened a page with no footage to scrub.

### The fix
- `replayed-store::prune`: `clip_is_missing`, `session_is_missing` (a running session and a
  buffer session with no file are both *present*), `plan_prune`, `execute_prune`, and
  `Store::prune_clip_row` / `Store::prune_session_row` — one transaction each, removing the row
  **with** its tags and events (a prune removes phantoms; the retention delete keeps events as
  history and is untouched), and refusing a session that is still recording.
- `cleanup_pass` plans over the files that are on disk only. The dead rows are left alone by the
  pass: reconciling the index is a decision the user makes, not a retention read.
- The shell's `list_sessions` hides rows whose file is gone, exactly as the clips list does.
- The Storage panel counts both libraries' phantoms (`missing_count`, `missing_sessions`) and
  offers the two-step reconciliation (`prune_missing`). It refuses when the clips directory
  cannot be read — an offline share looks like a library whose files were all deleted — and a
  session is only a candidate when the directory its file lived in is still reachable. The
  startup line now states clips/sessions on disk and how many rows name a file that is gone.

### The evidence that the filter is what fixes it
The two retention tests were run with the filter removed: both fail
(`a_clip_row_whose_file_is_gone_does_not_make_the_cap_evict_a_live_clip`,
`a_session_row_whose_file_is_gone_does_not_make_the_cap_evict_a_live_session` — the live clip is
deleted, the phantom kept), and pass with it. In both, the phantom is *newer* than the live
artefact, which is the ordering that turns stale bytes into a deletion of real footage.

On the host, all green: `cargo fmt --all --check`; workspace clippy `-D warnings`; the workspace
test suite with `replayed-encoder/test-encoders`; the desktop crate's `cargo test` and clippy;
`svelte-check` (0 errors, 0 warnings); `vitest` (309); the production build. New tests: 10 in
`replayed-store::prune`, 2 in the recorder (the discriminating pair above), 7 in
`commands::prune`, 2 for the sessions list, 5 in `StoragePanel.test.ts`, 6 for `describePrune`.

### What is NOT verified
Nothing here ran on Windows, and no real library was reconciled. Specifically:

- The Clips-tab delete's suspected locked-file ghost was **not reproduced**, so nothing was
  changed for it. If a delete reports `orphaned_path`, the file is still on disk and a later
  prune will *not* remove its row (the file is not missing) — that is the honest state.
- The reconciliation button has been driven in jsdom (`StoragePanel.test.ts`) and the screenshot
  harness gained a `pruneMissing` fake, but no one has clicked it in a real window against a
  real directory.
- The reachability guard is tested by removing the clips directory outright. A real network
  mount that goes offline (stale handles, ESTALE) was not exercised.
## 39. Auto full-session becomes a feature instead of two buried toggles (2026-10-24)

### What was wrong with it
The engine could already do the thing asked for. `[games] auto_record` was the watcher and
`[recorder] save_session` was the "keep the whole match" switch, and they lived in two different
Settings groups with no sentence connecting them. A user could turn auto-record on — the switch
that sounds like "record my games" — and get hotkey clips only, because the second switch, the
one that keeps the match, was three groups away and off. The watch list was `config.toml` only,
so adding a title meant editing a file the window does not show.

### What changed
- **One section, two tiers.** *Settings → Automatic recording* now holds both switches, states
  which games clip themselves (League, CS2, Dota 2) and what every other watched game gets
  (whole session + hotkey clips), and shows a warning when auto-record is on with session saving
  off — the exact combination that records a match into nothing.
- **The watch list is editable in the panel.** Add by executable name (a pasted path is reduced
  to its file name), optional display name, detection by process name or the Live Client Data
  API; remove with ×. *Apply automatic recording* writes `[[games.watch]]` through `toml_edit`,
  so every other key and every comment in `config.toml` survives. The list is validated with the
  engine's own rule (`validate_watch_list`) and one panel rule: an executable that does not end
  in `.exe` can never match a process image, so it is refused rather than saved as a watch that
  watches nothing. An empty list is refused too, because the engine reads no list as the six
  defaults — a Remove button that brought six games back silently would be a defect.
- **Session titles say which kind of recording they are**: "Valorant — full session" beside the
  buffer run's "Valorant". The game name was already on the row (the supervisor passes it), so
  this is the display half of the parity item.

### Evidence
Host, all green: `cargo fmt --all --check` both workspaces; workspace clippy `-D warnings`; the
desktop crate's clippy; the desktop `cargo test` (271, up from 264); the `replayed-events` tests
(164, for `Signal::parse`); `svelte-check` 0/0; `vitest` 330 (up from 309); the production build.
New tests pin the parts a screenshot cannot: the JSON contract for `watch_games`, a watch list
written and read back entry for entry with the comment intact, an empty list removing the key so
the defaults apply, a duplicate or executable-less list refused with the file left
byte-identical, the update reporting `games.watch` as a restart key, and the panel's add /
remove / refuse-empty / warning behaviour in jsdom.

### What is NOT verified
- Nothing here ran on Windows, and the watcher itself is still unverified on Windows (the
  ledger's open item, unchanged by this work). The plan's Phase C step is what would close it.
- The panel's watch editor was driven in jsdom and in the screenshot harness's fake source; no
  one has added a game in a real window and watched a real title start recording.
- No real game has been auto-recorded through the new section, so "Fortnite in, one session out"
  is still a claim about code rather than a measurement.
## 40. The throughput probe hands over a texture per frame, and says what the encoder said (2026-10-24)

### The fault, as far as it is known
Issue #79: the Media Foundation throughput probe fails on the Windows test machine with
`IMFTransform::ProcessOutput: Catastrophic failure (0x8000FFFF)`, while the recording's own
encoder runs the whole session on the same machine at the same size, rate and mode. The probe is
the only thing that tells the app what the machine can sustain, so when it fails the recording
runs at the configured rate with no evidence of whether that is achievable — fail-soft, and
therefore quiet.

The issue listed three candidate differences between the probe and the recording. Two of them
were the probe's, and are now gone:

1. **It submitted the same texture every frame.** The recording hands over the texture it just
   captured, so every frame it encodes is a different D3D11 resource. The probe built one
   zero-filled surface and re-submitted it for the whole measurement — legal (the sample is
   refcounted and the encoder does not write to it) but not what the recording does, which made
   it the single difference between the two on the probe's side of the boundary. The probe now
   rotates a pool of four (`PROBE_TEXTURES`), so a surface the encoder still references is not
   the one handed over next.
2. **It told the encoder a one-frame-per-second timeline.** Its synthetic `captured_at` advanced
   by a whole second per frame while the sample duration said `1/fps`, so the stream's timestamps
   and its declared durations disagreed by 600× at 60fps — and the timeline is exactly what the
   hardware rate control budgets bits against. The synthetic clock now marches on the real frame
   interval (`frame_time`, in `throughput.rs` so it is tested on every host; 60 frames at 60fps
   is exactly one second).

And its diagnostics are no longer just the COM code. `probe_failure_note` reports, on the failure
path, what the transform will still answer: whether it is accepting input
(`MFT_INPUT_STATUS_ACCEPT_DATA`), whether a sample is ready (`MFT_OUTPUT_STATUS_SAMPLE_READY`),
which encoder it is, and how many times it renegotiated its type. That is the difference between
"a catastrophic failure" and "the encoder was already busy" or "the encoder never produced output".

### Evidence
`cargo check -p replayed-encoder --target x86_64-pc-windows-msvc` and the CI's Windows-target
clippy over the cross-checkable crates (`-D warnings`, `--all-targets`, `test-encoders`) both
pass, so the Windows-only code compiles and is lint-clean. The one part that is not Windows-only
— the synthetic timeline — has a host test (`the_probe_timeline_marches_at_the_frames_own_interval`)
and the encoder crate's suite is green.

### What is NOT verified
**Nothing here has run on Windows, and #79 is not closed by this.** Whether the fault follows the
input (the probe's fault) or the encoder/machine (not the probe's) is exactly what only a Windows
run can say: `cargo test -p replayed-encoder --test ...` for
`the_hardware_encoders_rate_is_measured_the_same_way_on_every_run` (which measures 4K three times
and asserts the spread) has still never run on the box, and the probe has not been re-run there.
The measurement step of the plan's Phase C remains open, as do the 4K120 question and the soak.
## 41. Release hygiene, and the non-goals stated (2026-10-24)

Small items from the plan's Phase D that need no Windows and no release:

- **The stray `created2.json` is gone.** A 28-byte `[65,66,67,68,69,70,71]` left in the
  repository root by an experiment; it was never referenced. It was untracked and git-ignored,
  which is why it survived — nothing here can say which experiment made it.
- **CI names its compiler.** Every `dtolnay/rust-toolchain` setup in `ci.yml` and
  `release.yml` now passes `toolchain: 1.99.1`, so a failure is reproducible against the
  compiler that produced it and a release tag is built by a named one. The action's SHA pin
  never covered this: it pins the action's code, not the Rust version the action installs. A
  repository-level `rust-toolchain.toml` was deliberately **not** added: it would force every
  contributor's rustup to download that exact toolchain, which is the maintainer's call rather
  than a side effect of this change. The plan's step 9 can be read as closed for the CI half
  and open for that one.
- **The non-goals are written down** in `README.md` ("What replayed will not do") and in
  `docs/USER-GUIDE.md` ("What it does not do"): highlights for the eight provider-only games
  (full sessions do work), upload/accounts, an effects editor, a webcam overlay, and a
  software-encoder fallback in a release build. Each with the reason, so the list reads as a
  decision rather than an absence.
- **`CHANGELOG.md`** carries the three changes above (the stale index and its reconciliation,
  the automatic-recording section and the watch list, the probe).

### What is NOT verified
Nothing in this section needed a run, which is the point of doing it here. The rest of Phase D
is open: publishing `v0.2.0`, installing it, publishing `v0.2.1` and proving the signed-minisign
self-update end to end are actions on the release page and on a real machine, and they were not
taken.
## 42. Combining clips, and the labelled re-encodes (2026-10-24)

### What was added
Two things on the same panel, and the difference between them is the point:

- **`edit::combine_lossless`** — whole clips concatenated with `-c copy`, through the same
  `check_concat_layout` / `write_concat_list` / `concat_lossless_sized` pair the session
  finaliser and the replay splicer use, so a fix to `-map 0` (the microphone track) lands in all
  three. The shell's `combine_clips` refuses fewer than two distinct ids, a file that is gone, a
  path outside the library (the index is user-writable), and — through the media layer — clips
  that do not share one stream layout. The result is probed and indexed as a real clip, because
  it is real footage; its game and tags are inherited only where every input agrees.
- **`export::{ExportPreset, export}`** — three labelled re-encodes: Discord (h264/AAC fitted
  inside 1280×720 at CRF 26), Vertical (centre-cropped 9:16, scaled to 1080×1920 at CRF 23), and
  GIF (480px, 15fps, two-pass palette, no audio). The one media path in this tree that decodes
  and encodes, in its own module so `edit`'s "never re-encode" contract stays checkable. The
  shell's `export_clip` writes beside the clip and **does not index** the file: an export is an
  artefact to upload, and a row for it would put a derived copy under the storage cap.

In the window: a tick on each library card and a *Combine N clips* bar in the header; an *Export*
section on a clip's page, with the one sentence that separates it from Trim ("re-encoded, on
purpose").

### Evidence
Real ffmpeg, on the host: `crates/media/tests/exports.rs` (7 tests) proves two clips combine into
one whose duration is their sum and whose codec and frame size are the sources' own; that the
temporary concat list is removed; that mixed sizes are refused with both shapes named and no
partial file; that one clip is refused; that each preset produces the frame it promises
(1280×720 with audio, 1080×1920, a `gif` stream with no audio); and that a failed export leaves
nothing behind. `crates/media/src/export.rs` adds 3 unit tests (wire names, the budget's floor and
growth, and that no preset is a stream copy — the test that would catch a "preset" implemented
with `-c copy`).

The desktop crate (279 tests) adds 8 command-level tests: the combined clip is on disk, in the
library, indexed once, ~2s for two 1s sources; fewer than two, a repeated id, a missing file and a
layout mismatch are refused with the right code and no new row; an export lands beside the clip,
is **not** indexed, keeps the original, writes a real GIF; an unknown preset is refused before
ffmpeg runs; a missing file is refused.

Frontend: `svelte-check` 0/0, `vitest` 335 (up from 330: three descriptor tests, two grid tests for
the combine tick), the production build, and the screenshot harness's fake source grew
`combineClips` / `exportClip` so the contract test stays true.

### What is NOT verified
Nothing here ran on Windows, and no real recording has been combined or exported: the fixtures are
`testsrc2` clips, so the *quality* of a Discord export of real gameplay (its size against the
"H.264 + AAC" claim) is a property of the encoder and the content, not something these tests
measure. The vertical preset's centre crop was asserted at the frame size, not by looking at a
picture.
## 43. An open-data-folder button, and the ledger that said there was none (2026-10-24)

The user guide used to say, in as many words, that there is no button that opens the data
directory — every "where is everything" question ended in "copy the path out of the Storage page
and paste it into Explorer". That is a sentence about the code, not about the user, so it is now
false: the Storage page's clips-directory figure carries **Open the data folder**, and
`open_data_dir` resolves it from the state the shell was opened with (`AppState::data_dir`), not
from a guess at `%LOCALAPPDATA%` or from the clips directory's parent — a configured
`storage.clips_dir` can be anywhere, so the parent of that is not the data folder.

The reveal itself is the same `background::reveal_command` the tray's *Open config file* and a
clip's *Show in folder* use: one place decides what "reveal this path" means per platform, and its
tests already pin all three forms.

### Evidence
`cargo test` in the desktop crate (279) asserts the accessor returns the directory the shell was
opened with; `StoragePanel.test.ts` asserts the button calls the handler; `ipc.test.ts` asserts the
wire name is `open_data_dir` and that the Rust handler list and the frontend agree;
`svelte-check` 0/0, `vitest` 336, the production build.

One thing the test suite caught and this records: the button was first placed as a sibling of the
`<dt>`/`<dd>` pair inside the `<dl>`, which axe rejects (`definition-list`: a `<dl>` may contain
only terms and descriptions, not a button). It is inside the `<dd>` now, and the a11y run for the
Storage view is what found it.

### What is NOT verified
The button has not been pressed in a real window on Windows; what it spawns (`explorer <folder>`)
is the same command the tray item already uses, and that has been run on the box (§10).
## 44. Start with Windows is a checkbox, and the first run explains itself (2026-10-24)

### The start-with-Windows switch
`[app] start_with_system` was the last setting the panel refused to edit, for a reason the guide
said out loud: "the file is the only switch, because an autostart entry that disagreed with the
file would be a second, invisible source of truth". That reason is still the design; the
conclusion was just too strong. The file stays the switch, and the panel can edit the file:

- `SettingEdit::StartWithSystem` writes `[app] start_with_system` through the same `toml_edit`
  path as every other setting, so comments and the rest of the file survive;
- `update_settings` then asks Windows, through `background::apply_autostart` — the same
  `sync_autostart` the startup already runs, with its own injected `reg.exe` boundary — and puts
  the answer in `UpdateSettingsOutcome.autostart` as a report rather than an error;
- a failed registry write is reported with the setting **saved** (`applied: false, detail: …`),
  because the next launch re-syncs the registry from the file. Failing the whole command would
  claim nothing was saved, which would be false;
- off Windows the same field carries `NotApplicable`, so the checkbox still persists what the
  file should say and the notice states that nothing was registered.

The split matters: `update_settings_file` has no executable path and must not spawn `reg.exe`, so
`apply_autostart_update` takes the boundary as a parameter and is tested with a closure. The
registry rules themselves were already tested (`background::sync_autostart`, a fake `reg.exe`);
what is new is the wiring and its reporting.

### The first-run checklist
An empty library now shows the three steps — start recording, press the chord this process
installed, find the clip in the grid — instead of one sentence. The chord comes from
`app_status.hotkey.chord` (the live one, not the documented default), and when nothing is
installed the card says the hotkey is not active rather than naming a key nothing answers.

### Evidence
Desktop crate 283 tests (+4): the setting round-trips through the writer and the reader; the
update reports `app.start_with_system` and leaves the registry alone (it has no `reg.exe` there);
`apply_autostart_update` maps Enabled/Removed/Unchanged/NotApplicable and a failure to the right
report; the DTO's JSON key set includes the new field. Frontend 341 tests (+2 in `settings.test.ts`
for the notice, +2 in `ClipList.test.ts` for the checklist with a live chord and with none),
`svelte-check` 0/0, the production build. The screenshot harness's settings fixture and its
`update_settings` fake gained the field and the off-Windows report, so the contract test stays
true.

### What is NOT verified
The registry write has still never run on Windows — the checkbox has not been clicked on the box,
and `reg.exe` is a boundary this host does not implement (the argv and the sync rules are tested,
the effect is not). The checklist has been rendered in jsdom only, never in a real window.
## 45. The clip hotkey can be rebound, and it is checked before it is saved (2026-10-24)

### What made this non-trivial
`RegisterHotKey` is system-wide and a registration belongs to the thread that made it, so the
chord is registered **once, at startup**, on a dedicated thread, and owned until the process
exits. A panel therefore cannot "apply now": it can only write the chord the next start will
install. Two consequences shaped this:

1. **A chord another application owns must be refused at save time.** Writing it would produce a
   file that loads, a process that starts, and a key nothing answers — for ever, since the panel
   would report it as saved. `hotkey::probe` registers the candidate on the calling thread with an
   id of its own, releases it at once, and reports what `RegisterHotKey` said. It answers "free
   as of this moment", not "free at the next launch", which is why the startup's own registration
   and its failure message still exist and are still what a user sees when it goes wrong.
2. **The panel must not promise immediate effect.** It shows the chord *in force* (from
   `app_status.hotkey`, which is what the process really holds) beside the saved one, and the
   notice says "…saved as Ctrl+F9; Ctrl+F8 stays in force until replayed restarts".

`update_settings` probes before `update_settings_and_prepare` writes, so a taken chord is refused
with nothing changed. The decision is `check_clip_hotkey` — pure, with the probe injected — so all
four outcomes are tested on a host that cannot register anything: `AlreadyInForce` (probing our
own chord would always fail, so it is skipped), `NotSupported` (not Windows: saved, not probed),
`Free`, and `Taken` (the registration's own message, carried through).

### A parser laxity this exposed
The chord parser took the **first** alphanumeric character of a key token, so `"Ctrl+Hovercraft"`
parsed as Ctrl+H, installed, and answered a key the user never named. Tolerable in a file someone
writes by hand; a trap in a panel with a text field. `Hotkey::parse` now requires the key to be
exactly one character or `F1`–`F24` and says so, with a test for both.

### Evidence
`replayed-events` 165 tests (+1: the multi-character key is refused; the same test pins that
`Ctrl+K` and `ctrl+k` still parse and that a bare `Ctrl` keeps its "no main key" message).
Desktop crate 287 (+4): the probe decision in all four outcomes, the note for a change with and
without a chord in force, `[hotkeys] clip` written as the parser's own spelling (`alt+f9` →
`Alt+F9`), an unparseable chord refused with nothing written, and the reader's default. Frontend
346 (+5: three chord-helper tests, two panel tests), `svelte-check` 0/0, the production build.
The Windows target compiles and lints the new probe (`cargo clippy -p replayed-events --target
x86_64-pc-windows-msvc -D warnings`).

### What is NOT verified
`probe` has never run on Windows: what is tested is the decision around it, not `RegisterHotKey`
itself. The panel has not been driven in a real window, and a rebind has not been tried against a
chord another application holds. The live-rebind design — a listener thread that can unregister
and register again on command — was deliberately **not** built: without a Windows host to run it,
a message-loop refactor whose failure mode is a silently dead hotkey is the wrong risk to take
for a convenience.
## 46. A screenshot hotkey, and what a PNG is in a library of clips (2026-10-24)

### The design question, and the answer taken
A screenshot is not a clip: no duration, no player, no trim, no combine, and its file *is* its
picture. The library, though, is exactly where a user looks for it, and everything a user does to
a clip — tag it, favourite it, protect it from the storage policy, delete it — is what they want
to do to a screenshot. So it is a row in the **same table**, with a `kind`:

- schema **v6** adds `clips.kind TEXT NOT NULL DEFAULT 'clip'`. The default is the only claim a v5
  row supports: before this column existed there was no way to make a screenshot, so every row is
  a video (tested: a v5 file with a row is rewound to v5, migrated, and comes back a clip).
- `Clip.kind` stays **text**, for the reason `Session.mode` does: a kind a newer build writes must
  be readable here. `Clip::is_screenshot()` is the only test a caller needs.
- `Store::insert_screenshot` writes a row with no duration, `codec = "png"`, no timeline — and
  `NewClip` is untouched, so nothing that inserts a video learned a new field to insert a still.

### The pipeline
`replayed_capture::grab_frame` opens a capture session, takes the first frame the backend offers,
and closes it again — on every path, failure included, because a WGC session left open holds the
display's frames. The wait is bounded (`SCREENSHOT_TIMEOUT`, 2 s) and its failure names the likely
causes rather than hanging the window. `edit::screenshot_png` writes the BGRA frame as a PNG
through ffmpeg's rawvideo pipe, refusing a buffer that is not `width × height × 4` before spawning
anything — ffmpeg would read a short buffer as a truncated image and exit 0.

`save_screenshot` is the command body: refuse a texture-only frame (a caller bug), write the PNG
into the clips directory, index it. `screenshot_now` builds the backend from the configured
`[capture]` target — on Windows WGC, off it the synthetic stub — and the recorder bar's
**Screenshot** button and the optional `[hotkeys] screenshot` chord both land there. The button is
not gated on a recording: a screenshot is one frame of the same target, and needing to start a
recording first would be a restriction with no cause.

In the window: a screenshot's card shows the file itself with a `PNG` chip and no duration, has no
combine tick (there is no stream to copy), and its page shows the image instead of a player, with
Trim and Export replaced by one sentence explaining why. `combine_clips` refuses a screenshot by
name rather than letting the media layer fail on a file with no video stream.

### Evidence
Store 88 (+2: the screenshot row round-trips with tags and a favourite; the v5→v6 upgrade). Capture
84 (+2: one frame from the stub, and a target that never presents failing with a named reason and
still stopping). Media 9 integration tests (+2: a frame written as a PNG of its own size, and a
short buffer refused before ffmpeg runs). Desktop 290 (+3: a captured frame saved, indexed and
named; a pixel-less frame refused; a screenshot refused by combine). Frontend 349 (+3: the view
model marks a screenshot and says "still", the card shows its own file with no tick, and the
`kind` fixtures). fmt and clippy clean in both workspaces; `svelte-check` 0/0; the build.

### What is NOT verified
The WGC half has never run: off Windows the frame comes from the synthetic stub, so what is proven
here is the pipeline *around* the capture, not a real screen's pixels. No screenshot has been
taken on the box, the hotkey chord has not been pressed, and the window has not rendered a real
PNG card. The schema upgrade was tested by rewinding a fresh database, not against a copy of a
released v0.2.0 library.
## 47. The Windows box ran it: suite, a real 4K screenshot, and `xtask verify` (2026-10-24)

The box was reachable over SSH, with no game or anti-cheat process running, a logged-in desktop
session, an RTX 3090 and ffmpeg 9.0.2 on `PATH`. The commits from this session were carried over
as a `git bundle` (nothing was pushed), cloned to `C:\Users\Flavio\replayed-plan`, and run there.
The existing `C:\Users\Flavio\replayed-src` checkout — which has an uncommitted change to
`wgc.rs` — was not touched.

### The workspace suite: exit 0, after three real findings
`cargo test --workspace --features replayed-encoder/test-encoders` on Windows **failed at first**
in `replayed-encoder`, in three tests that no macOS host could fail:

1. `a_child_that_exits_is_not_reported_as_killed` and
2. `a_child_that_will_not_go_is_reported_as_killed` spawn `sh -c …`. There is no `sh` on that
   machine's `PATH`, so both died with "program not found" **before reaching an assertion**, and
   had done since they were written. The shell is now chosen per platform (`cmd /C` on Windows,
   with `ping` as the sleep), and both tests run and pass there.
3. `the_encoder_publishes_a_sequence_header_a_raw_demuxer_can_read` failed with "the encoder's
   output type carries no MF_MT_MPEG_SEQUENCE_HEADER attribute". The lookup asked only
   `GetOutputAvailableType`; the sequence header is a property of the type the encoder is
   *producing*, and on this NVENC driver only `GetOutputCurrentType` carries it. Asked there
   first now (the enumeration stays as a fallback), and the test passes on the box.

That fix is **confirmed on real hardware**: `cargo test -p replayed-encoder --lib` on the box is
73 passed, 0 failed, and a second full workspace run is **EXIT=0** — every target, every crate
(capture 83, encoder 73, events 164, media 44, recorder 119, store 88, cli 13/30/4, xtask 43).

### A real 4K frame became a real PNG
`crates/capture/tests/wgc_frame.rs` (new this session) was run from the **interactive session**
through a one-shot `schtasks` task (an SSH shell is session 0 and has no desktop), and it passed:

```
captured 3840x2160 (33177600 bytes)
PNG 3840x2160 written: 8031042 bytes
test one_real_frame_is_captured_and_written_as_a_png ... ok
```

So the screenshot feature's Windows half is proven: WGC delivered a 33 MB BGRA frame of the real
desktop, the frame was not one flat colour, and the PNG writer produced an 8.03 MB 3840x2160 PNG
that ffprobe reads as `png`. What is still unproven for step 10 is only the *desktop's* half of the
UI: the button and the hotkey have not been pressed on the box.

### `xtask verify`: 27/28 passed, **one criterion failed**, and it is not noise
Run through `scripts/verify-in-interactive-session.ps1` (the script's own scheduling failed first —
see below), twice, `--seconds 20`, 4K capture, hardware encoder:

| run | verdict | the failure |
|---|---|---|
| 1 (by hand, `schtasks`) | 27 passed, 1 failed, 2 not performed | steady-state CPU **5.22%** of one core vs a **< 5%** target |
| 2 (by hand, `schtasks`) | 28 passed, 1 failed, 1 not performed | steady-state CPU **5.49%** of one core |
| 3 (the repo's own script, after the SID fix) | 28 passed, 1 failed, 1 not performed | steady-state CPU, same criterion |

Everything else that could run passed: capture started, the encoder sustained the rate
(`dropped=3`, all during encoder startup), audio arrived, the clip was written, the timeline was
correct, the throughput probe measured 156.4 fps on the hardware path at 3840x2160. The two
_not performed_ checks are the silence detector (it needs a non-48 kHz converted endpoint, a
machine-setup condition) and, in run 1 only, the stream-copy speed check (the extraction took
4.0 s against 8.0 s of media, which is inconclusive rather than wrong).

**So Phase C's `cargo xtask verify` clean is not met.** The criterion is missed by 0.22 and 0.49
percentage points on two runs, so it is reproducible on this machine as it is configured now —
against **3.1–4.0%** recorded in §15. Nothing in this session's changes plausibly causes it (the
sampled process is the recorder at steady state, and none of the new work runs during a
recording). What differs from §15 and is the first thing to look at: the machine now has two
**virtual display adapters** (SudoMaker, Sunshine) alongside the 3090, the desktop is
3840x2160 at 150%, and the harness ran at `configured=30` fps. Naming a cause without measuring
one would be a guess; the honest state is "the criterion is missed and the cause is unknown".

### The verify script had a bug of its own
`verify-in-interactive-session.ps1` could not register its task on that machine at all:
`Register-ScheduledTask : No mapping between account names and security IDs was done (0x80070534)`.
It named the principal `"$env:USERDOMAIN\$env:USERNAME"`, and an SSH session on a workgroup
machine reports `USERDOMAIN=WORKGROUP`, which does not resolve. The principal is named by **SID**
now (`WindowsIdentity::GetCurrent().User.Value`), which needs no domain lookup. Runs 1 and 2 used
the same one-shot `schtasks` pattern by hand; run 3 was the fixed script itself, which registered
the task, waited for the marker, read the exit code, printed the report's verdict and unregistered
the task — so the fix is confirmed on the machine that exposed the bug.

### Still not run on the box
The desktop shell's own `cargo test` (it needs `apps/desktop/dist`, which was shipped over, and
node is not on that machine's `PATH` — the shell's crate was not built there this round); a real
game session with an integration (LoL/CS2/Dota 2); the tray, hide-on-close and autostart as
observed behaviour; the 72-hour soak; and 4K120 (the verify run's probe sustained 156 fps at
60 fps configured, but the 120 fps question is about the live path, not the probe).
## 48. The CPU criterion is a cost per offered frame, and this box offers 58 a second (2026-10-24)

§47 recorded four `xtask verify` runs on the box, every one failing the same check: steady-state
CPU against a **< 5% of one core** target. This section is what four runs and the reports' own
numbers say about it.

| run | window | CPU | frames the display offered |
|---|---|---|---|
| 1 (`--seconds 20`) | 16.2 s | **5.22%** | ~58/s |
| 2 (`--seconds 20`) | 16.2 s | **5.49%** | 60.0/s |
| 3 (`--seconds 20`, the fixed script) | 16.2 s | **7.45%** | 59.4/s |
| 4 (`--seconds 45`, the default window) | 34.4 s | **5.55%** | 58.2/s |

**It is not the short window.** The 45-second run has a 34-second steady-state window and the
figure is unchanged, so this is steady state, reproducibly missed by 0.2–2.5 points.

**What the figure is a cost of.** The pipeline is judged on the frames the *display* presents, not
the frames the pacer keeps: at `configured=30` the pacer admits 30, skips the rest, and the run
offers **~58–60 frames/s** — because the desktop is continuously changing. That machine runs
**23 Chrome processes**, two virtual display adapters (SudoMaker, Sunshine) and a 3840×2160
desktop at 150%; connecting with the capture's own words, `capture_wait` and `capture` together
account for ~22–23% of the loop's wall clock and `pacer_sleep` for ~76%, so ~98% of the recorder
loop is *waiting*, and what the other threads do is hand ~60 4K frames a second from WGC to the
encoder — 33 MB each, none of it crossing to the CPU on the zero-copy path.

The ledger's **3.1–4.0%** (§15, and §13's 59.6/60 fps run) was measured on the same code path but
a **quiet screen**: WGC presents a frame when the screen *changes*, and a still desktop offers a
few frames a second, so the handover cost per second is a fraction of what an animating one
costs. The number is not wrong; the criterion is not conditioned on the thing that dominates it.

**What was done about it:** nothing that moves the target. The harness now reports the offered
rate next to the CPU figure and says, in the criterion itself, that the cost is per frame the
screen presents. The parsing is `offered_frame_rate`, tested against the shape of the box's own
status lines. **The threshold stays where it is.**

**What this means, and what it does not.** Phase C's "`cargo xtask verify` clean" is **not met on
this machine as configured**, and neither is it evidence of a regression from this session: none
of the new work (screenshots, the watch list, the probe) executes during a steady recording. The
honest options are the maintainer's:

1. run the criterion on a quiet desktop (the condition §15 measured it under) and record the offer
   rate with it;
2. make the criterion frame-rate-aware (e.g. CPU per offered frame, or a bound that scales with
   the offer rate) — a change to the acceptance criterion, which is a specification decision and
   not something to slip in under a failing check;
3. treat 5.2–7.5% on a 4K60 animating desktop as acceptable and raise the target with the numbers
   written down.

Until one of those is chosen, the ledger says the criterion fails on this box, with the numbers
and the context that explain why the same code once read 3.1–4.0%.
## 49. #79 does not reproduce — with the fix or without it — and the desktop suite is green (2026-10-24)

### Issue #79: the probe now measures on the box, and the fix is not why
The issue's symptom was `IMFTransform::ProcessOutput: Catastrophic failure (0x8000FFFF)` during the
hardware-encoder throughput probe on this machine. On the box today:

| code | the probe's Windows-only test (`the_hardware_encoders_rate_is_measured_the_same_way_on_every_run`) |
|---|---|
| **with** this session's probe changes (`main`) | **PASS** — 155.5, 156.4, 156.2 fps at 3840x2160 |
| **without** them (`68be4d8`, before the probe change) | **PASS** — 152.4, 156.9, 156.4 fps at 3840x2160 |

and the harness's own probe measured the hardware path three times in the verify runs
(156.4, 153.2 and 155.8 fps at 3840x2160).

**So the failure does not reproduce on this machine, and this session's change is not what fixed
it** — the pre-fix code passes the same test, three measurements in a row, minutes apart. What the
change did do is remove two real differences between the probe and the recording (it re-submitted
one texture where the recording hands over a new one; it told the encoder a one-frame-per-second
timeline where the recording's is `1/fps`) and make a failure report the transform's state. Those
are worth keeping on their own terms; claiming them as #79's fix would be claiming evidence this
run does not support.

What is *not* explained: what was different on 2026-10-21 (a driver update, a machine state, a
different session, or the probe failing only under some inputs). #79 can be closed as **not
reproducible**, with this table as the evidence, rather than as "fixed".

### The desktop crate's suite on the box: 283 passed, 0 failed
Ran over SSH after one finding: `the_hotkey_status_is_honest_about_what_this_build_can_do`
asserts that Ctrl+F8 registers on Windows, which it does **in the logged-on session** and not from
an SSH shell (session 0, no interactive desktop). Reproduced both ways on the box — fails over SSH,
passes through a scheduled task in the interactive session — so the test now skips with that reason
when `SESSIONNAME` is unset (the tell for an SSH/service session), and still asserts the real thing
wherever there is a desktop to judge it on. Everything else in that crate passes on Windows too,
the autostart sync included (its fake `reg.exe` boundary is what makes it host-independent).

### And the criterion that failed was then changed, with the owner's decision
§48 put three options to the owner rather than moving the target under a failing check. The answer
was **make it frame-rate-aware**, and that is what `xtask verify` does now:

- the verdict is `CPU <= 3.5% + 0.08% × offered frames/s` of one core;
- the old fixed `5%` is still printed beside the verdict, so both numbers are visible;
- with no frame data the old `5%` is the bound, so a run whose status lines did not parse still
  means something;
- the coefficients are **fitted** to the five measurements on record (3.1–4.0% on a quiet screen in
  §15; 5.22, 5.49, 5.55 and 7.45% at 58–60/s offered in §48), and the criterion text says so. A
  regression is still caught: +2 points at 4K60 sits outside the envelope, and issue #1's 50.8% is
  nowhere near it. The unit test pins all six numbers against the bound.

The 72-hour soak is **dropped from Phase C by the owner's decision**: it would capture their desktop
for three days on their machine and write hundreds of gigabytes. It is not a criterion that was
failed; it is a measurement that will not be made, and the plan says so.

### The run that passed
With the criterion in place, `xtask verify` ran on the box through the repo's own script
(`--seconds 45`) and returned **exit 0**:

> **PASS with 1 check(s) not performed (on this machine, this run)** — 29 passed, 0 failed, 1 not
> performed.

The CPU row reads: **6.55% of one core over a 34.3 s window, at 58.5 frames/s offered (pacer
skipped 1366), bound 8.18%** — and prints the old fixed target of 5.00% beside it. The one
_not performed_ check is still the silence detector, which needs a non-48 kHz converted endpoint.

**Phase C's `cargo xtask verify` gate is therefore met on the box**, on a build that contains every
change in this session. What that run does and does not cover is in the report itself: it drives
the real WGC capture, the real NVENC encode, the splicer and the index; it says nothing about a
real game being played, which is still open.

### GitHub, on the owner's instruction
- **#79 was closed as not reproducible**, with the A/B table from §49 attached as a comment. The
  comment says in as many words that this session's probe change is *not* what fixed it, and that
  the change is kept on its own merits.
- **#93 was opened** for the CPU criterion: the four failing runs, the offered-frame-rate analysis,
  the envelope that replaced the fixed target, and the open question of whether an envelope is the
  right long-term shape.
## 50. `start_with_system` writes and removes a real registry value (2026-10-24)

Phase C's autostart item was "`start_with_system` registry write/removal", listed in the ledger's
"never run anywhere" column since the switch was written. It has now run, on the box, against the
current build (`cargo build` of `apps/desktop/src-tauri` at `caf347c`, run from a scheduled task in
the logged-on session — over SSH the app cannot create a webview: `WebView2 … Invalid window
handle`, because session 0 has no desktop).

| `[app] start_with_system` | what the registry said afterwards |
|---|---|
| `true` | `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` → `replayed    REG_SZ    "C:\Users\Flavio\replayed-plan\apps\desktop\src-tauri\target\debug\replayed-desktop.exe"` |
| `false` | `ERROR: The system was unable to find the specified registry key or value.` |

Written on start with the executable path quoted, removed on the next start when the file says
false, per-user, value name `replayed` only. The installed `config.toml` was backed up first and
restored afterwards, the app process was stopped after each phase, and the Run key is absent again
now — the box is as it was. **The only thing still unverified about autostart is the Windows side
of *being started at logon*** (that is the OS's job, and it needs a reboot nobody is going to do
for a verification run).

### Two things the run showed by accident
- The startup line from this session's Phase A work ran on a real library:
  `clip index …\replayed.db (schema v6): 0 clip(s) on disk (11 row(s) name a file that is gone),
  0 bytes on disk, 0 session(s) …`. The box's library has 11 rows pointing at `H:\Replayed`, and
  the `H:` drive is not mounted — so those rows are exactly the case the prune guard refuses to
  touch (a file that is missing because its volume is absent is not a file the user deleted). The
  schema is v6 and the migration ran on a real, populated v5 file.
- The webview panic above is a *session* limit, not an app defect, and it is what an SSH-only run
  of the window would always see. The scheduled-task path is the one the repo's own
  `verify-in-interactive-session.ps1` uses for the same reason.
## 51. Closing the window hides it; the tray and the hotkey are really there (2026-10-24)

Two of Phase C's "a human has to watch" items turned out to be checkable without a human, with one
window message and the app's own log.

**Hide-on-close.** The app was started in the logged-on session (current build at `caf347c`), then
its main window was sent `WM_CLOSE` — `CloseMainWindow()`, a standard window message to our own
process, not synthesised input. Six seconds later: **one process still running**, and the log says

```
INFO replayed_desktop: the window is hidden: recording continues, and replayed stays in the tray.
Quit from the tray menu (there is no other way out).
```

That is the behaviour the plan asks about: closing does not quit, and the tray is where quitting
lives. (What a screenshot still cannot show is whether the tray *icon is visible* in the
notification area and what its menu items look like — that stays in the human-watches column, with
the 2026-09-26 screenshot session as the last observation.)

**The hotkey installs on real Windows,** in the same run:
`INFO replayed_desktop: the clip hotkey Ctrl+F8 is installed` — and the autostart sync's third
branch was observed too, after the §50 run restored the config:
`start_with_system = false: the start-with-Windows entry already matches [app] start_with_system`.

**Both runs left the box as they found it:** the app is stopped, the scheduled tasks are deleted,
the scripts are removed, and the config is the one that was there before (a backup was taken and
restored, and the `Run` value is absent).

### What Phase C still has left
A real game with an integration — LoL, CS2 or Dota 2 — is the one item that needs the owner at the
machine: it needs a game started, which this session will not do, and a match played. Everything
else in the sweep that a program can judge has now been judged on the box: the suite, the desktop
crate, `xtask verify`, a real 4K screenshot, autostart, hide-on-close, the hotkey installation, and
the startup line against a real library (§47–§51). 4K120 remains unmeasurable here: the display the
capture targets runs at ~60 Hz, so the picker cannot offer 120 and the live path cannot be measured
at 120 on this machine (the probe's own ceiling at 4K is ~155 fps, §49).
