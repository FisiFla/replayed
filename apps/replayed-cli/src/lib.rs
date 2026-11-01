//! The replayed CLI: the headless front-end over the recording engine.
//!
//! A driver and nothing else. The pipeline (capture, audio, encode, segment ring, clip) lives in
//! `replayed-recorder`, which the desktop shell drives too; this crate only:
//!
//! 1. parses the config file (the CLI owns `[hotkeys]` and `[events]`; the recorder's
//!    `[buffer]`/`[encode]`/`[storage]` sections are shared types),
//! 2. starts a [`Recorder`],
//! 3. starts the game-event sources `[events]` enables, and installs the global hotkey,
//! 4. turns a press or a derived event into a clip through one call
//!    ([`Recorder::clip_now_with`]), and stops on the way out.
//!
//! # The two event sources
//!
//! `[events] lol_poll_enabled` starts the League Live Client poller; `[events] gsi_port`
//! (non-zero) starts the CS2 / Dota 2 GSI listener. Both are loopback-only by construction (see
//! `replayed-events` and its static guard test) and report over one channel, which this loop
//! drains. A source that fails to start is reported and skipped: a machine with no League
//! install must still record on the hotkey.
//!
//! The loop's only policy: a highlight ([`replayed_events::EventKind::is_highlight`]: a kill, a
//! bomb, the end of a game) takes a clip through the hotkey's own trigger, and a marker (a game
//! or round boundary) is recorded in the `events` table without one. Neither is a second trigger
//! mechanism: the clip is always [`Recorder::clip_now_with`].
//!
//! # What this loop does not do
//!
//! It does not log the status line (`frames= segments= bytes= span= ...`). The engine emits it
//! on the thread that owns the counters, so it reads the same whichever front-end started the
//! recording. What this loop owns is the one thing the engine does not: the hotkey.
//!
//! # Taking a clip without touching the keyboard
//!
//! `Ctrl+F8` is the shipping trigger, but pressing it from a script means synthesising input,
//! which a machine running kernel-level anti-cheat treats as hostile.
//!
//! `buffer --self-test-clip-after <SECONDS>` is a diagnostic, not a feature: it waits until the
//! ring holds a full window, calls the same [`Recorder::clip_now`] the hotkey calls (once), and
//! stops. No keyboard or mouse input is sent, simulated or injected anywhere in this crate and
//! this crate enumerates no windows (the capture backend does, only to find `[capture] window`
//! when one is configured, and the self-test's log says which); the trigger is an in-process call
//! on the engine's media-time path, and it produces the hotkey's own log lines.
//!
//! # Quitting
//!
//! Ctrl+C, Ctrl+Break, closing the console and SIGTERM stop the recorder and exit `0` (see
//! [`shutdown`]): a clip in flight is finished and a full session is written out. Press Ctrl+C a
//! second time to quit at once.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

pub mod config;
pub mod shutdown;

use anyhow::{bail, Context, Result};
use config::Config;
#[cfg(feature = "test-encoders")]
use replayed_capture::stub::StubConfig;
use replayed_events::hotkey;
use replayed_media::FfmpegBinaries;
use replayed_recorder::event_driver::{act_on_burst, start_event_sources, EventSources, Policy};
#[cfg(feature = "test-encoders")]
use replayed_recorder::STUB_CAPTURE_SIZE;
use replayed_recorder::{Recorder, RecorderConfig, RecorderOptions, RecordingMode, Sources};
use shutdown::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// How long the hotkey wait blocks before the loop looks at the recorder again.
///
/// Short enough that a trigger and a shutdown are noticed promptly, long enough that a platform
/// with no hotkey listener (everything off Windows, where `wait_for_press` returns immediately)
/// sleeps here instead of spinning.
const HOTKEY_POLL: Duration = Duration::from_millis(10);

/// The `buffer` subcommand's own help text (see `main.rs`).
///
/// It documents `--self-test-clip-after` beside the flags a user may pass, and says what the
/// flag does not do: "does this touch my input?" is the first question such a diagnostic must
/// answer.
pub fn help() -> String {
    let mut text = String::from(
        "\
replayed — Phase 1 headless replay buffer

usage:
  replayed-cli buffer [OPTIONS]
  replayed-cli displays

subcommands:
  displays
        List the displays the capture can target, with the index `[capture] monitor`
        takes for each, its current size and the refresh rate it runs at. The list
        comes from the capture backend's own enumeration of the monitors, so the
        index it prints is the index the capture uses; a list assembled elsewhere
        can order the displays differently and then name a different screen.

options:
  --mode <buffer|session>
        Which recording to make, overriding the file's `[recorder] mode`.
          buffer  — the rolling replay buffer: a bounded scratch ring, a clip per
                    hotkey press. The default, and what every earlier build did.
          session — record the whole session, then concatenate it losslessly into
                    one `session-<timestamp>.mp4` under the sessions area when the
                    recording stops. The scratch cap does not apply to a session.
  --self-test-clip-after <SECONDS>
        Verification aid, not an end-user feature. Once the ring holds SECONDS of
        media — and never before it holds a full pre-roll plus post-roll — take
        exactly one clip through the same media-time trigger the clip hotkey
        uses, log what that path logs (the trigger instant with its media time,
        wall time and drift, then the written clip), and stop. The clip hotkey
        remains the shipping mechanism; this flag is an *additional* trigger.

        It sends, simulates and injects no keyboard or mouse input, and enumerates
        no windows itself: the capture backend looks windows up only when
        `[capture] window` names one, and the run's log says whether that applies.
        It exists so that a machine whose anti-cheat treats synthetic input as
        hostile can still have its clip path verified. Interrupting it (Ctrl+C)
        stops the recorder cleanly and exits with an error, since no clip was taken.
  --dev-software-encoder
        Only in builds carrying the `test-encoders` feature (never a release
        build): use libx264 instead of a GPU hardware encoder, for pipeline
        smoke tests on a host with no GPU encoder.
  --dev-stub-sources
        Only in builds carrying the `test-encoders` feature (never a release
        build): capture from the synthetic sources instead of the display, the
        speakers and a microphone. **Nothing real is captured.** It exists so the
        whole pipeline — including the microphone path, which has no non-Windows
        backend — can be driven end to end on a development host.

configuration:
  %LOCALAPPDATA%\\replayed\\config.toml on Windows (the directory
  $LOCALAPPDATA points at may be redirected for a scratch run); the example
  file is `config.example.toml`. replayed never reads configuration from
  environment variables.
",
    );
    // The hotkey belongs to the runtime, not the flag list; naming the chord here would mean
    // reading the config for `--help`, which the flag parser does not do.
    text.push_str("\nThe clip hotkey (config `[hotkeys] clip`, default Ctrl+F8) is unchanged.\n");
    text
}

/// Options for the `buffer` subcommand, from the command line (and the environment for the
/// data directory only).
#[derive(Debug, Clone)]
pub struct BufferOptions {
    /// The application data directory: `config.toml`, `scratch/`, `clips/` and the index.
    /// `app_data_dir()` by default; a verification harness points it somewhere disposable.
    pub app_dir: PathBuf,
    /// `--self-test-clip-after <SECONDS>`. `None` is the shipping behaviour.
    pub self_test_clip_after: Option<u64>,
    /// `--dev-software-encoder`.
    pub dev_software_encoder: bool,
    /// `--mode <buffer|session>`: an override of `[recorder] mode` from the file. `None`
    /// means "whatever the file says".
    pub mode: Option<RecordingMode>,
    /// `--dev-stub-sources`: the synthetic capture/audio/microphone sources, for a host that is not
    /// the platform this application records on. Feature-gated like `--dev-software-encoder`: a
    /// build that captured nothing while looking like a recording would be worse than one that
    /// cannot be demonstrated.
    pub stub_sources: bool,
}

/// The flags the `buffer` subcommand accepted, before the config file is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferArgs {
    pub self_test_clip_after: Option<u64>,
    pub dev_software_encoder: bool,
    pub mode: Option<RecordingMode>,
    pub stub_sources: bool,
}

/// Parse the flags the `buffer` subcommand accepts (everything after `buffer`).
///
/// Strict on purpose: an unrecognised argument is an error, because this parser exists for a
/// verification flag that must provably have been understood.
pub fn parse_buffer_args(args: &[String]) -> Result<BufferArgs> {
    let mut parsed = BufferArgs {
        self_test_clip_after: None,
        dev_software_encoder: false,
        mode: None,
        stub_sources: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value_of = |it: &mut std::slice::Iter<'_, String>, flag: &str| -> Result<String> {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--self-test-clip-after" => {
                let value = value_of(&mut it, "--self-test-clip-after")?;
                parsed.self_test_clip_after = Some(parse_self_test_seconds(&value)?);
            }
            "--mode" => {
                let value = value_of(&mut it, "--mode")?;
                parsed.mode = Some(value.parse::<RecordingMode>().context("--mode")?);
            }
            "--dev-software-encoder" => parsed.dev_software_encoder = true,
            "--dev-stub-sources" => parsed.stub_sources = true,
            other => match other.strip_prefix("--self-test-clip-after=") {
                Some(value) => parsed.self_test_clip_after = Some(parse_self_test_seconds(value)?),
                None => match other.strip_prefix("--mode=") {
                    Some(value) => {
                        parsed.mode = Some(value.parse::<RecordingMode>().context("--mode")?)
                    }
                    None => bail!("unexpected argument {other:?}\n\n{}", help()),
                },
            },
        }
    }
    Ok(parsed)
}

fn parse_self_test_seconds(value: &str) -> Result<u64> {
    let seconds: u64 = value.parse().with_context(|| {
        format!("--self-test-clip-after expects whole seconds, e.g. 45 (got {value:?})")
    })?;
    if seconds == 0 {
        bail!("--self-test-clip-after must be at least 1 second");
    }
    Ok(seconds)
}

/// How much media the ring must hold before a self-test clip is taken, in ms.
///
/// The requested hold (`--self-test-clip-after`) is a floor, not the whole rule: a clip spliced
/// before the pre-roll is complete would say nothing about the configured window, so the wait
/// is never shorter than one full pre-roll plus post-roll. Pure, so a unit test pins the rule.
pub fn self_test_need_ms(after_seconds: u64, pre_seconds: u64, post_seconds: u64) -> u64 {
    (after_seconds * 1000).max((pre_seconds + post_seconds) * 1000)
}

/// The `displays` subcommand: list the displays the capture can target.
///
/// The index printed here is the index `[capture] monitor` takes, because both come from the
/// capture backend's own enumeration of the monitors. A display list assembled elsewhere — by a
/// script, by the user, by another API — can order the displays differently, and then its
/// "index 1" names a different screen than the capture's "index 1". That is not a hypothetical:
/// it is how a measurement came to capture the wrong display and report a frame rate for it.
///
/// The command needs no configuration and starts no capture: it only asks the platform what is
/// there.
pub fn run_displays() -> Result<()> {
    for line in describe_displays(&replayed_capture::displays()) {
        println!("{line}");
    }
    Ok(())
}

/// The display list as lines of text, one per display.
///
/// A display the platform would not describe keeps its index and says so: the size and the
/// refresh rate are reported as unknown rather than guessed, because a wrong number here is
/// worse than no number — it would be copied into a configuration file.
pub fn describe_displays(displays: &[replayed_capture::DisplayInfo]) -> Vec<String> {
    if displays.is_empty() {
        return vec!["no display could be described on this machine".to_string()];
    }
    displays
        .iter()
        .map(|display| {
            let size = if display.width == 0 || display.height == 0 {
                "size not reported".to_string()
            } else {
                format!("{}x{}", display.width, display.height)
            };
            let rate = match display.refresh_hz {
                Some(hz) => format!("{hz} Hz"),
                None => "refresh rate not reported".to_string(),
            };
            format!("index={} {} {}", display.index, size, rate)
        })
        .collect()
}

/// The `buffer` subcommand, as `main.rs` calls it: flags from the process's own arguments.
///
/// Installs the process's Ctrl+C / console-close / SIGTERM handling (see [`shutdown`]) before
/// anything starts, so that a quit stops the recorder and the process exits `0`, instead of
/// being killed with a clip half-written and a session never finalised.
pub fn run_buffer() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let parsed = parse_buffer_args(&args)?;
    let shutdown = Shutdown::new();
    shutdown.install()?;
    let result = run_buffer_with_shutdown(
        BufferOptions {
            app_dir: app_data_dir(),
            self_test_clip_after: parsed.self_test_clip_after,
            dev_software_encoder: parsed.dev_software_encoder,
            mode: parsed.mode,
            stub_sources: parsed.stub_sources,
        },
        &shutdown,
    );
    // Whatever the outcome, the recorder has stopped by now: release a console-close handler
    // that is waiting for exactly that.
    shutdown.mark_finished();
    result
}

/// The `buffer` subcommand: capture, encode, segment ring, hotkey, clip.
///
/// Split from [`run_buffer`] so an integration test can drive the whole command in-process with
/// its own data directory; the self-test trigger must be reachable from a test. Nothing raises a
/// shutdown here: use [`run_buffer_with_shutdown`] to be able to stop it.
pub fn run_buffer_with(opts: BufferOptions) -> Result<()> {
    run_buffer_with_shutdown(opts, &Shutdown::new())
}

/// [`run_buffer_with`], stoppable: once `shutdown` is requested the loop stops the recorder
/// (finishing a clip already in flight, closing a session) and returns `Ok(())`.
pub fn run_buffer_with_shutdown(opts: BufferOptions, shutdown: &Shutdown) -> Result<()> {
    let app_dir = opts.app_dir;
    let cfg_path = app_dir.join("config.toml");
    let cfg = if cfg_path.is_file() {
        Config::load(&cfg_path)?
    } else {
        Config::from_toml(include_str!("../../../config.example.toml"))?
    };
    let Config {
        recorder,
        buffer,
        encode,
        capture,
        audio,
        storage,
        mic,
        hotkeys,
        events,
        games,
        ..
    } = cfg;
    // `[audio] enabled = false` is carried to the engine rather than interpreted here: it asks for
    // a video-only recording, with no audio input declared, no loopback listener and no audio
    // stream. (Microphone on with game audio off, whose one-track container would be labelled
    // `Game Audio`, is refused in the recorder.)
    // Read before the move: these feed the CLI's own log line, written after the engine is
    // capturing. The rate printed comes from the engine rather than `encode.fps`, because the
    // engine may have measured that this machine cannot hold the configured rate and be pacing
    // lower (see `RecorderStatus::effective_fps`).
    let (pre_seconds, post_seconds) = (buffer.pre_seconds, buffer.post_seconds);
    // Read before `capture` moves into the recorder: whether the capture backend will look for a
    // window by name, which the self-test says in its log rather than claiming it enumerates none.
    let capture_window_set = capture.target().window.is_some();
    let policy = Policy::new(&events, pre_seconds)?;
    // `--mode` overrides the file, and the resolved value is what the CLI logs and what the
    // engine is told: decided once here, never re-derived below.
    let mode = opts.mode.unwrap_or(recorder.mode);
    let encode_fps = encode.fps;

    let bin = FfmpegBinaries::discover(None)?;
    let hotkey = replayed_events::hotkey::Hotkey::parse(&hotkeys.clip)?;
    // The chord is claimed BEFORE the recorder starts, and that order is the point.
    // `Recorder::start` runs crash recovery over the data directory, which closes, finalises or
    // deletes any session row still open, and nothing tells it that the owner is alive. The usual
    // reason the chord cannot be registered is that another replayed (the desktop app on the same
    // `config.toml`, or a second CLI) is running and recording; a refusal that came *after* the
    // start had already "recovered" that run's live session out from under it.
    //
    // `listen` fails loudly when the chord cannot be registered (held by another application, or
    // the desktop app on the same `config.toml`); a buffer run with a silently dead hotkey is not
    // worth starting.
    let hotkeys = hotkey::listen(hotkey).context("installing the clip hotkey")?;
    // The event sources need the application data directory too (GSI token, generated cfg file),
    // and the recorder takes it by value below.
    let data_dir = app_dir.clone();

    // `--dev-software-encoder` exists only with the test-encoders feature; it selects libx264,
    // needs no GPU vendor, and is never reachable from the config file. The engine applies the
    // gate (see `replayed_recorder::resolve_encoder`).
    let dev_software = opts.dev_software_encoder;
    let stub_sources = opts.stub_sources;

    let recorder = Recorder::start_with_options(
        RecorderConfig {
            bin,
            app_data_dir: app_dir,
            buffer,
            encode,
            storage,
            capture,
            // The `[audio]` section's only parser is the CLI, so it carries the values across: `enabled`
            // and `bitrate_kbps` are honoured (video-only recording, ffmpeg's `-b:a`); `codec` and
            // `source` are refused in `Config::validate`, each having one implemented value.
            audio_bitrate_kbps: Some(audio.bitrate_kbps),
            audio_enabled: Some(audio.enabled),
            // WGC + WASAPI on Windows, synthetic stubs elsewhere (`replayed_capture::platform` decides,
            // and on Windows refuses to fall back to a stub). `--dev-stub-sources` selects the synthetic
            // pair explicitly, microphone included (which has no non-Windows backend), with the same gate
            // as `--dev-software-encoder`.
            sources: choose_sources(stub_sources, encode_fps)?,
            dev_software_encoder: dev_software,
        },
        RecorderOptions {
            mode,
            save_session: recorder.save_session,
            mic,
            games,
            game: None,
        },
    )?;

    // A press during startup (which can take seconds: encoder probe, capture open) was made before
    // there was anything to clip; it must not turn into a clip the moment the loop starts.
    while hotkeys.try_recv().is_ok() {}

    // From here the engine is capturing, or with `[games] auto_record` on, armed and waiting for a
    // game. This line is the CLI's own (the hotkey is what only it knows). The chord printed is the
    // parsed one, so `ctrl+f8` reads back as `Ctrl+F8` as in the desktop shell. The rate is the one
    // the pipeline runs at; when below the configured `encode.fps` the engine has already logged
    // why (see `log_rate_decision` in `replayed-recorder`).
    let status = recorder.status();
    if status.watching_games {
        tracing::info!(
            "armed in {} mode at up to {}fps: nothing is captured until a watched game \
             starts (games.auto_record = true), and each game is recorded as its own \
             session; press {} to clip during one",
            mode,
            status.configured_fps,
            hotkey
        );
    } else {
        tracing::info!(
            "{} {pre_seconds}s pre / {post_seconds}s post at {}fps; press {} to clip",
            if mode.is_full_session() {
                "recording the whole session,"
            } else {
                "buffering"
            },
            status.effective_fps,
            hotkey
        );
    }

    // The game-event sources. Started after the recorder (a source cannot ask for a clip before
    // there is anything to clip) and before the hotkey, so an early event is not lost to a wait
    // already running.
    //
    // Self-test mode does not start them: it takes exactly one clip, and a source asking for one
    // of its own would break that (and would bind a port or poll a game on a run meant to touch
    // nothing but its own capture).
    let sources = match opts.self_test_clip_after {
        Some(_) => {
            tracing::info!(
                "self-test mode: the game-event sources are not started (the run takes \
                 exactly one clip)"
            );
            let (_sink, events) = mpsc::channel();
            EventSources {
                events,
                _lol: None,
                _gsi: None,
                reports: Vec::new(),
            }
        }
        None => start_event_sources(&data_dir, &events),
    };

    // The hotkey listener was installed above, in both modes: it is part of the shipping startup path,
    // and self-test mode must not be a different program. Self-test just never reads it.

    // The verification-only trigger (`--self-test-clip-after`): an in-process call to the same
    // media-time trigger the hotkey drives, never synthesised input.
    if let Some(after_seconds) = opts.self_test_clip_after {
        if recorder.status().watching_games {
            tracing::warn!(
                "self-test: games.auto_record is on, so the recording this run clips from \
                 does not exist yet — the self-test waits for a watched game to start"
            );
        }
        return self_test_clip(
            &recorder,
            after_seconds,
            pre_seconds,
            post_seconds,
            capture_window_set,
            shutdown,
        );
    }

    // The driver loop. Every path out of it stops the recorder, which flushes the encoder, closes
    // the capture session and (in session mode) writes the session file: an error, the engine
    // stopping on its own, and a requested shutdown (Ctrl+C, closing the console), which is the
    // way a person actually quits and returns `Ok`.
    loop {
        if shutdown.is_requested() {
            tracing::info!(
                "shutting down: stopping the recorder (press Ctrl+C again to quit at once)"
            );
            return recorder.stop().context("stopping the recorder on shutdown");
        }

        if hotkey::wait_for_press(&hotkeys, HOTKEY_POLL) {
            if let Err(err) = recorder.clip_now() {
                note_failure(&recorder, "the hotkey could not take a clip", &err);
            }
        } else {
            std::thread::sleep(HOTKEY_POLL);
        }

        // Anything the integrations derived, one at a time. A highlight is a clip through the call the
        // hotkey just made; a marker is recorded and takes no footage. `try_recv` rather than `recv`,
        // so a quiet game cannot make the hotkey wait.
        while let Ok(event) = sources.events.try_recv() {
            if let Err(err) = act_on_burst(&recorder, event, &sources.events, &policy) {
                note_failure(&recorder, "the game event could not be acted on", &err);
            }
        }

        if !recorder.is_running() && !recorder.is_armed() {
            // The engine stopped on its own (failed capture, scratch cap violation) and kept the reason in
            // its status; `stop()` reports it.
            recorder.stop()?;
            bail!("the recorder stopped; see the log above");
        }
    }
}

/// What a failed clip or event means for the driver loop.
#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    /// Armed for a game that has not started: there is no recording to clip or mark. A state,
    /// not a failure.
    NotRecordingYet,
    /// A recording exists and one request against it failed.
    Transient,
    /// Neither recording nor armed: the engine has stopped, and the loop's own check is about to
    /// say why.
    EngineStopped,
}

fn classify_failure(running: bool, armed: bool) -> FailureKind {
    match (running, armed) {
        (false, true) => FailureKind::NotRecordingYet,
        (false, false) => FailureKind::EngineStopped,
        (true, _) => FailureKind::Transient,
    }
}

/// Log a clip or event that failed, and carry on.
///
/// The loop used to stop the recorder and exit on any such error, which threw away the whole
/// in-memory ring for one transient fault (a clip write error, a marker the index would not take,
/// a splice that failed) while the player was in a game, with the console behind it: the next
/// press did nothing, because the process was gone, and the user learnt of it after the match.
/// The desktop shell's `EventDriver` warns and keeps recording for the same calls, and so does this.
///
/// It never decides the engine is finished: the loop's own check (`is_running` / `is_armed`) does
/// that, and exits with the engine's reason when it really has stopped.
fn note_failure(recorder: &Recorder, what: &str, err: &anyhow::Error) {
    match classify_failure(recorder.is_running(), recorder.is_armed()) {
        FailureKind::NotRecordingYet => tracing::info!(
            "nothing is being recorded yet ({err:#}); a clip or event is taken once a watched \
             game starts"
        ),
        FailureKind::Transient => tracing::warn!("{what}: {err:#}; still recording"),
        FailureKind::EngineStopped => tracing::warn!("{what}: {err:#}"),
    }
}

/// The verification-only self-test trigger: wait for a full window, take one clip, stop.
///
/// * the wait is on the ring's media timeline (`span_ms`), the clock the trigger uses, and is
///   never shorter than a full pre-roll plus post-roll ([`self_test_need_ms`]);
/// * the trigger is [`Recorder::clip_now`], the hotkey's own call, so the trigger line
///   (`hotkey pressed: media=... wall=... (drift ...ms)`) and the `wrote ...` line are exactly
///   the hotkey's;
/// * nothing here sends, simulates or injects a keyboard or mouse event. It does not enumerate
///   windows itself; the capture backend does, but only when `[capture] window` names one
///   (`capture_window_set`), and the log line says which.
///
/// Afterwards the recorder is stopped and the process exits with the clip's own outcome, which
/// makes the run scriptable. A shutdown requested before the clip was taken stops the recorder
/// cleanly and is an **error**: the diagnostic's job is to produce a clip, and a script reading
/// the exit code must not take "interrupted" for "verified".
fn self_test_clip(
    recorder: &Recorder,
    after_seconds: u64,
    pre_seconds: u64,
    post_seconds: u64,
    capture_window_set: bool,
    shutdown: &Shutdown,
) -> Result<()> {
    let need_ms = self_test_need_ms(after_seconds, pre_seconds, post_seconds);
    tracing::info!(
        "self-test: one clip will be taken once the ring holds {need_ms}ms of media \
         (--self-test-clip-after {after_seconds}s, at least {pre_seconds}s pre + \
         {post_seconds}s post); no keyboard or mouse input is sent, simulated or injected{}",
        if capture_window_set {
            ", and the only windows enumerated are by the capture backend, to find the \
             [capture] window you configured"
        } else {
            ", and no window is enumerated ([capture] window is not set)"
        }
    );

    loop {
        if shutdown.is_requested() {
            recorder
                .stop()
                .context("stopping the recorder on shutdown")?;
            bail!("the self-test was interrupted before it took its clip");
        }
        let span_ms = recorder.status().span_ms;
        if span_ms >= need_ms {
            tracing::info!("self-test: the ring holds {span_ms}ms of media; taking the clip");
            break;
        }
        if self_test_engine_gone(recorder.is_running(), recorder.is_armed()) {
            // The engine stopped on its own and kept the reason in its status.
            recorder.stop()?;
            bail!("the recorder stopped while the self-test was waiting for {need_ms}ms of media");
        }
        std::thread::sleep(HOTKEY_POLL);
    }

    if let Err(err) = recorder.clip_now() {
        let _ = recorder.stop();
        return Err(err.context("taking the self-test clip"));
    }
    // Flush the encoder and close the capture session before exiting, so the exit code comes from
    // the engine's own shutdown and not from a drop().
    recorder.stop()
}

/// Whether the self-test's wait is over because the engine has stopped for good.
///
/// With `[games] auto_record` on the recorder is *armed* and not running until a watched game
/// starts, and the self-test logs that it waits for one; treating "not running" as "stopped" bailed
/// on the first iteration with a message that misdescribed the state. The driver loop makes the
/// same distinction.
fn self_test_engine_gone(running: bool, armed: bool) -> bool {
    !running && !armed
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("replayed");
    migrate_legacy_data_dir(&base, &dir);
    dir
}

/// Move the data directory over from the name the project had before the rename.
///
/// The product was renamed; the directory holding the index, the clips, the sessions and the
/// settings was not, and a user who had been running the older build would have found an
/// empty library.
///
/// The new directory usually **already exists** by the time this runs, and that is the whole
/// subtlety: the installer puts the application in `%LOCALAPPDATA%\replayed`, and the data
/// directory is that same path, so the installer creates the directory before the application
/// has ever started. A rule of "the old directory is adopted only if the new one is absent"
/// therefore never fires on a real upgrade, and the user gets an empty library with their old
/// one stranded next to it.
///
/// What decides it is not the directory but the *data in it*. If the new directory already
/// holds an index or a configuration of its own, this build has been used, and the old
/// directory is left exactly where it is: merging two libraries is not something to do to
/// somebody's clips without being asked.
///
/// Otherwise each entry of the old directory is moved across one at a time, skipping anything
/// whose destination already exists, and the index is renamed with the project. Nothing is
/// copied, so a failure part way through loses nothing: whatever did not move is still in the
/// old directory, which is left in place, and the next start tries again.
fn migrate_legacy_data_dir(base: &Path, dir: &Path) {
    use std::ffi::{OsStr, OsString};

    // The name before the rename, spelled out rather than derived: it must stay this
    // literal forever, because it is the name of a directory that exists on disk.
    const LEGACY: &str = "localplay";
    // The index was renamed with the project, so the old file has to be renamed on the way.
    const LEGACY_INDEX: &str = "localplay.db";
    const INDEX: &str = "replayed.db";

    let legacy = base.join(LEGACY);
    if !legacy.is_dir() {
        return;
    }
    if dir.join(INDEX).exists() || dir.join("config.toml").exists() {
        // The new directory has data of its own. It is not ours to merge into.
        return;
    }
    let entries = match std::fs::read_dir(&legacy) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                "the old data directory at `{}` could not be read: {err}",
                legacy.display()
            );
            return;
        }
    };
    let mut moved: Vec<String> = Vec::new();
    let mut left: Vec<String> = Vec::new();
    // The entries that make the new directory "used" go last (see `migration_order`).
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| migration_order(&entry.file_name()));
    for entry in entries {
        let name = entry.file_name();
        let target_name: OsString = if name.as_os_str() == OsStr::new(LEGACY_INDEX) {
            OsString::from(INDEX)
        } else {
            name.clone()
        };
        let target = dir.join(&target_name);
        if target.exists() {
            left.push(name.to_string_lossy().into_owned());
            continue;
        }
        // The new directory may not exist yet, when the application is run from a build
        // rather than from the installer.
        let _ = std::fs::create_dir_all(dir);
        match std::fs::rename(entry.path(), &target) {
            Ok(()) => moved.push(name.to_string_lossy().into_owned()),
            Err(err) => {
                tracing::warn!(
                    "`{}` could not be moved to `{}`: {err}",
                    name.to_string_lossy(),
                    target.display()
                );
                left.push(name.to_string_lossy().into_owned());
            }
        }
    }
    // Only an empty directory is removed, and `remove_dir` refuses anything else — so this
    // cannot delete a leftover the user still needs.
    let _ = std::fs::remove_dir(&legacy);
    if !moved.is_empty() {
        tracing::info!(
            "the data from the old `{}` directory was moved to `{}`: {}",
            legacy.display(),
            dir.display(),
            moved.join(", ")
        );
    }
    if !left.is_empty() {
        tracing::warn!(
            "these are still in the old `{}` directory and were not moved: {}",
            legacy.display(),
            left.join(", ")
        );
    }
}

/// The position of a legacy entry in the order the migration moves them: the entries whose
/// presence in the new directory means "this build has been used" move **last**.
///
/// The guard at the top of [`migrate_legacy_data_dir`] returns early once the new directory holds
/// an index or a `config.toml`. The old directory is read in whatever order the filesystem lists
/// it, so the index could move first, then `clips` fail to move (a handle open inside it, a virus
/// scan), and the next start would see the new index, return early and never retry: the
/// doc comment's "the next start tries again" held only for failures before the first of those two
/// moved. Moving them last keeps the guard false until everything else has moved.
fn migration_order(name: &std::ffi::OsStr) -> u8 {
    if name == "localplay.db" || name == "config.toml" {
        1
    } else {
        0
    }
}

/// The capture, audio and microphone sources this run uses.
///
/// `Sources::Platform` is the shipping choice: WGC + WASAPI on Windows, the synthetic pair
/// elsewhere (`replayed_capture::platform` refuses to substitute a stub on Windows).
///
/// `--dev-stub-sources` selects the synthetic sources explicitly, microphone included; the
/// microphone has no non-Windows backend, so this is the only way to drive that path on a
/// development host. Gated like `--dev-software-encoder`.
fn choose_sources(stub_sources: bool, fps: u32) -> Result<Sources> {
    if !stub_sources {
        return Ok(Sources::Platform);
    }
    #[cfg(feature = "test-encoders")]
    {
        tracing::warn!(
            "--dev-stub-sources: the synthetic capture, audio and microphone sources are in \
             use. NOTHING REAL IS CAPTURED — this is for pipeline smoke tests only, and the \
             recording it produces will show a test pattern, two synthetic tones and silence \
             from the display."
        );
        Ok(Sources::Stub(StubConfig {
            width: STUB_CAPTURE_SIZE.0,
            height: STUB_CAPTURE_SIZE.1,
            fps,
        }))
    }
    #[cfg(not(feature = "test-encoders"))]
    {
        let _ = fps;
        bail!(
            "--dev-stub-sources requires building with `--features test-encoders` (the same \
             gate --dev-software-encoder has): a build that captures nothing while looking \
             like a recording is not something to ship"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A base directory holding the pre-rename data directory beside the new one, with the new
    /// one **already there** — which is what the installer leaves behind, because it puts the
    /// application in `%LOCALAPPDATA%\replayed` and that is also where the data lives.
    fn legacy_case() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let base = tmp.path().to_path_buf();
        let legacy = base.join("localplay");
        let dir = base.join("replayed");
        std::fs::create_dir_all(&legacy).expect("the legacy directory");
        std::fs::create_dir_all(&dir).expect("the new directory, as the installer leaves it");
        (tmp, base, dir)
    }

    #[test]
    fn the_legacy_data_is_adopted_even_when_the_new_directory_already_exists() {
        // The regression this whole function exists for: the installer creates the new directory
        // before the application has ever run, so a rule of "adopt the old one only if the new one
        // is absent" never fires on a real upgrade, and the user's clips and settings stay behind
        // in the old directory while the application reads and writes the new one.
        let (_tmp, base, dir) = legacy_case();
        let legacy = base.join("localplay");
        std::fs::write(legacy.join("config.toml"), b"[storage]\n").expect("the old settings");
        std::fs::write(legacy.join("localplay.db"), b"index").expect("the old index");
        std::fs::create_dir_all(legacy.join("clips")).expect("the old clips");

        migrate_legacy_data_dir(&base, &dir);

        assert!(
            dir.join("config.toml").is_file(),
            "the settings moved across"
        );
        assert!(
            dir.join("replayed.db").is_file(),
            "the index arrived under its new name"
        );
        assert!(dir.join("clips").is_dir(), "the clips moved across");
        assert!(
            !legacy.join("config.toml").exists(),
            "the old settings are gone"
        );
        assert!(
            !legacy.join("localplay.db").exists(),
            "the old index is gone"
        );
    }

    #[test]
    fn a_new_directory_with_its_own_data_is_left_alone() {
        // The new build has been used and has settings of its own. Merging somebody's library
        // into it without being asked is not something to do.
        let (_tmp, base, dir) = legacy_case();
        let legacy = base.join("localplay");
        std::fs::write(legacy.join("config.toml"), b"old").expect("the old settings");
        std::fs::write(dir.join("config.toml"), b"new").expect("the new settings");

        migrate_legacy_data_dir(&base, &dir);

        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("the new settings"),
            "new",
            "the new directory's own settings are untouched"
        );
        assert!(
            legacy.join("config.toml").is_file(),
            "the old settings are still where they were"
        );
    }

    #[test]
    fn an_entry_whose_target_already_exists_is_skipped_and_the_rest_moves() {
        // One file's name is taken; the rest still moves, and the skipped file is left exactly
        // where it was rather than being overwritten.
        let (_tmp, base, dir) = legacy_case();
        let legacy = base.join("localplay");
        std::fs::write(legacy.join("notes.txt"), b"old").expect("the old file");
        std::fs::write(dir.join("notes.txt"), b"new").expect("the new file");
        std::fs::write(legacy.join("localplay.db"), b"index").expect("the old index");

        migrate_legacy_data_dir(&base, &dir);

        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).expect("the new file"),
            "new",
            "an existing file is not overwritten"
        );
        assert!(dir.join("replayed.db").is_file(), "the rest moved across");
        assert!(
            legacy.join("notes.txt").is_file(),
            "the skipped file is still where it was"
        );
    }

    #[test]
    fn the_entries_that_mark_the_new_directory_as_used_move_last() {
        // `config.toml` and the index make the early-return guard fire on the next start, so a move
        // that fails after they have moved is never retried. Whatever order the filesystem lists
        // them in, they must come after every other entry.
        let mut names: Vec<std::ffi::OsString> =
            ["localplay.db", "clips", "config.toml", "sessions"]
                .iter()
                .map(Into::into)
                .collect();
        names.sort_by_key(|name| migration_order(name));
        let last_two: Vec<_> = names[2..].iter().map(|n| n.to_string_lossy()).collect();
        assert!(last_two.contains(&"localplay.db".into()), "{names:?}");
        assert!(last_two.contains(&"config.toml".into()), "{names:?}");
        assert!(names[..2].iter().all(|n| n == "clips" || n == "sessions"));
    }

    #[test]
    fn a_failed_clip_or_event_does_not_end_the_run_unless_the_engine_has_stopped() {
        // Armed and waiting for a game: no recording to clip, a state.
        assert_eq!(classify_failure(false, true), FailureKind::NotRecordingYet);
        // Recording: one request failed, and the ring is worth more than one failed clip.
        assert_eq!(classify_failure(true, false), FailureKind::Transient);
        assert_eq!(classify_failure(true, true), FailureKind::Transient);
        // Neither: the engine has stopped, which the loop's own check turns into the exit.
        assert_eq!(classify_failure(false, false), FailureKind::EngineStopped);
    }

    #[test]
    fn the_self_test_keeps_waiting_while_the_recorder_is_armed_for_a_game() {
        // `games.auto_record`: armed, not running. The self-test logs that it waits for a watched
        // game, so it must; it used to bail on the first iteration.
        assert!(!self_test_engine_gone(false, true));
        assert!(!self_test_engine_gone(true, false));
        assert!(self_test_engine_gone(false, false));
    }

    #[test]
    fn the_buffer_flags_default_to_the_shipping_behaviour() {
        // No flags: the hotkey path is unchanged, the self-test is not armed, the mode comes from the
        // file and the real sources are used. The case a user runs.
        let parsed = parse_buffer_args(&[]).expect("no flags is a valid command line");
        assert_eq!(parsed.self_test_clip_after, None);
        assert!(!parsed.dev_software_encoder);
        assert_eq!(parsed.mode, None, "the file's [recorder] mode decides");
        assert!(
            !parsed.stub_sources,
            "the synthetic sources are never implicit"
        );
    }

    #[test]
    fn the_self_test_flag_parses_seconds_in_either_form() {
        assert_eq!(
            parse_buffer_args(&args(&["--self-test-clip-after", "45"]))
                .expect("spaced form")
                .self_test_clip_after,
            Some(45)
        );
        assert_eq!(
            parse_buffer_args(&args(&["--self-test-clip-after=45"]))
                .expect("= form")
                .self_test_clip_after,
            Some(45)
        );
        // The two forms are one setting: the last one wins, and an unrelated flag is unaffected.
        let parsed = parse_buffer_args(&args(&[
            "--self-test-clip-after=10",
            "--dev-software-encoder",
            "--self-test-clip-after",
            "20",
        ]))
        .expect("last one wins");
        assert_eq!(parsed.self_test_clip_after, Some(20));
        assert!(parsed.dev_software_encoder);
    }

    /// The mode is settable from the command line in either form, as an override: with no flag the
    /// file decides (`None`).
    #[test]
    fn the_mode_flag_parses_and_overrides_the_file() {
        assert_eq!(
            parse_buffer_args(&args(&["--mode", "session"]))
                .expect("the spaced form")
                .mode,
            Some(RecordingMode::FullSession)
        );
        assert_eq!(
            parse_buffer_args(&args(&["--mode=buffer"]))
                .expect("the = form")
                .mode,
            Some(RecordingMode::ReplayBuffer)
        );
        let err = parse_buffer_args(&args(&["--mode", "everything"]))
            .expect_err("an unknown mode must be refused")
            .to_string();
        assert!(
            err.contains("mode"),
            "the error names the flag and the setting: {err}"
        );
    }

    /// The stub-sources flag exists only in a build carrying the software-encoder feature, the same
    /// gate as `--dev-software-encoder`.
    #[test]
    fn the_stub_sources_flag_is_gated_behind_the_test_encoders_feature() {
        if cfg!(feature = "test-encoders") {
            let sources = choose_sources(true, 60).expect("with the feature it is selectable");
            assert!(matches!(sources, Sources::Stub(_)));
            assert!(matches!(
                choose_sources(false, 60).unwrap(),
                Sources::Platform
            ));
        } else {
            let err = choose_sources(true, 60)
                .expect_err("without the feature the flag must be refused")
                .to_string();
            assert!(
                err.contains("test-encoders"),
                "the error names the gate: {err}"
            );
        }
    }

    #[test]
    fn a_bad_self_test_value_is_refused_rather_than_defaulted() {
        // A verification flag that silently did nothing would be worse than a crash: the run would look
        // like it verified a trigger it never armed.
        for bad in [
            args(&["--self-test-clip-after"]),
            args(&["--self-test-clip-after", "0"]),
            args(&["--self-test-clip-after", "five"]),
            args(&["--self-test-clip-after=-1"]),
        ] {
            let err = parse_buffer_args(&bad)
                .expect_err("a bad value must be refused, never defaulted")
                .to_string();
            assert!(
                err.contains("self-test-clip-after"),
                "the error must name the flag, got: {err}"
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_an_error_that_prints_the_help() {
        let err = parse_buffer_args(&args(&["--clip-after", "5"]))
            .expect_err("an unknown flag must be refused")
            .to_string();
        assert!(
            err.contains("--clip-after"),
            "must name the bad argument: {err}"
        );
        assert!(err.contains("usage:"), "must show the usage: {err}");
    }

    #[test]
    fn the_help_documents_that_no_input_is_synthesised() {
        // The flag's whole justification; if this sentence is dropped, the reason for the flag is lost.
        let help = help();
        assert!(help.contains("--self-test-clip-after"));
        assert!(help.contains("no keyboard or mouse input"));
        assert!(
            help.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("enumerates no windows itself"),
            "the help says what the flag does not do, and does not claim more than is true"
        );
        // And the mode flags are documented where a user looks for them.
        assert!(help.contains("--mode <buffer|session>"));
        assert!(help.contains("--dev-stub-sources"));
        assert!(
            help.contains("Nothing real is captured"),
            "the stub flag says what it costs"
        );
        // And the display list is documented where a user looks for it.
        assert!(help.contains("replayed-cli displays"));
        assert!(
            help.contains("index it prints is the index the capture uses"),
            "the display list says why its order is trustworthy"
        );
    }

    #[test]
    fn the_self_test_wait_is_never_shorter_than_the_configured_window() {
        // Requested hold above the window: the request wins.
        assert_eq!(self_test_need_ms(45, 5, 3), 45_000);
        // Requested hold below it: a full pre-roll plus post-roll is the floor.
        assert_eq!(self_test_need_ms(2, 5, 3), 8_000);
        assert_eq!(self_test_need_ms(8, 5, 3), 8_000);
    }

    #[test]
    fn the_display_list_names_the_index_the_capture_uses_and_says_what_it_does_not_know() {
        // A display the platform described, and one it would not.
        let lines = describe_displays(&[
            replayed_capture::DisplayInfo {
                index: 0,
                width: 3840,
                height: 2160,
                refresh_hz: Some(120),
            },
            replayed_capture::DisplayInfo {
                index: 1,
                width: 0,
                height: 0,
                refresh_hz: None,
            },
        ]);
        assert_eq!(lines[0], "index=0 3840x2160 120 Hz");
        // A display the platform would not describe keeps its index and says so, rather than
        // guessing a size: a wrong number here is copied into a configuration file.
        assert_eq!(
            lines[1],
            "index=1 size not reported refresh rate not reported"
        );
        // And a machine with no display to describe says so, rather than printing nothing.
        assert_eq!(
            describe_displays(&[]),
            vec!["no display could be described on this machine".to_string()]
        );
    }
}
