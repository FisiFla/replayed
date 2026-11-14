//! Quitting the CLI: a requested shutdown stops the recorder, finishes the work in hand and
//! exits `0`.
//!
//! Before this existed, Ctrl+C ended the process with the default handler: no Rust code ran, a
//! clip waiting out its post-roll was abandoned, a full-session recording was never written out
//! (footage came back only through crash recovery on the next start), and a script wrapping the
//! CLI could not tell a user quitting from a crash.
//!
//! Two layers, because the claim has two halves:
//!
//! * in-process, the loop's own behaviour: [`replayed_cli::run_buffer_with_shutdown`] returns
//!   `Ok` when the flag is set and a full session is on disk afterwards;
//! * (Unix only) the real thing: the shipped binary is sent `SIGINT` and exits `0` with the
//!   session written. The Windows half of the handler (`SetConsoleCtrlHandler`) cannot be driven
//!   from a test without a console; `docs/runbooks/` carries the manual step.
//!
//! The synthetic sources are used wherever they are reachable (see `self_test_clip.rs` for why
//! the real ones are never opened by a test). Nothing here sends input of any kind.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]

use replayed_cli::shutdown::Shutdown;
use replayed_cli::{run_buffer_with_shutdown, BufferOptions};
use replayed_recorder::RecordingMode;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Small and fast: the point is the exit path, not a soak.
const CONFIG: &str = r#"
[buffer]
pre_seconds = 2
post_seconds = 1
segment_time = 1
scratch_dir = ""

[encode]
vendor = "auto"
codec = "h264"
bitrate_kbps = 2000
fps = 10
output_size = ""

[audio]
enabled = true
source = "loopback"
codec = "aac"
bitrate_kbps = 64

[storage]
clips_dir = ""
max_total_bytes = 536870912
max_age_days = 1

[hotkeys]
clip = "Ctrl+F8"

[events]
lol_poll_enabled = false
gsi_port = 0
"#;

/// Whether this build can run without opening a real display or audio endpoint: off Windows the
/// platform's sources are the synthetic pair, and on Windows only the opt-in feature selects them.
const CAN_RUN_SYNTHETIC: bool = cfg!(feature = "test-encoders") || !cfg!(windows);

/// Every file under `dir` (recursively) whose name starts with `prefix` and ends in `.mp4`.
fn mp4s_named(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".mp4"))
            {
                found.push(path);
            }
        }
    }
    found
}

fn skip_on_real_sources() -> bool {
    if !CAN_RUN_SYNTHETIC {
        eprintln!(
            "SKIPPED: this build would open the machine's real display and audio endpoint; \
             build with --features replayed-cli/test-encoders to run it here"
        );
    }
    !CAN_RUN_SYNTHETIC
}

#[test]
fn a_requested_shutdown_stops_the_recorder_and_returns_ok_with_the_session_written() {
    if skip_on_real_sources() {
        return;
    }
    let app_dir = tempfile::tempdir().unwrap();
    std::fs::write(app_dir.path().join("config.toml"), CONFIG).unwrap();

    let shutdown = Shutdown::new();
    let run = {
        let shutdown = shutdown.clone();
        let app_dir = app_dir.path().to_path_buf();
        std::thread::spawn(move || {
            run_buffer_with_shutdown(
                BufferOptions {
                    app_dir,
                    self_test_clip_after: None,
                    dev_software_encoder: true,
                    mode: Some(RecordingMode::FullSession),
                    stub_sources: cfg!(feature = "test-encoders"),
                },
                &shutdown,
            )
        })
    };

    // Let a few segments be written, then quit the way Ctrl+C does.
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !run.is_finished(),
        "the loop must run until it is told to stop"
    );
    shutdown.request();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !run.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the run did not stop within 30s of the request"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    run.join()
        .expect("the run does not panic")
        .expect("a requested shutdown is a clean exit, not an error");

    let sessions = mp4s_named(app_dir.path(), "session-");
    assert_eq!(
        sessions.len(),
        1,
        "the full session must have been written out by the stop; found {sessions:?}"
    );
    assert!(
        std::fs::metadata(&sessions[0]).unwrap().len() > 0,
        "and it is not empty"
    );
}

/// The process-level claim: the shipped binary, sent the signal a terminal sends on Ctrl+C,
/// exits `0` and leaves the session on disk.
#[cfg(unix)]
#[test]
fn sigint_makes_the_binary_stop_the_recorder_and_exit_zero() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    if skip_on_real_sources() {
        return;
    }
    let data_home = tempfile::tempdir().unwrap();
    let app_dir = data_home.path().join("replayed");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("config.toml"), CONFIG).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_replayed-cli"))
        .args(["buffer", "--mode", "session", "--dev-software-encoder"])
        .args(if cfg!(feature = "test-encoders") {
            vec!["--dev-stub-sources"]
        } else {
            vec![]
        })
        // `app_data_dir()` is `$LOCALAPPDATA` or `$XDG_DATA_HOME`, plus `replayed`.
        .env_remove("LOCALAPPDATA")
        .env("XDG_DATA_HOME", data_home.path())
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawning the CLI binary");

    // The tracing subscriber writes to stdout. Read until the engine says it is recording, on a
    // thread so a silent child cannot hang the test.
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<String>();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut log = String::new();
    let started = Instant::now();
    while !log.contains("press Ctrl+F8 to clip") {
        match lines_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                log.push_str(&line);
                log.push('\n');
            }
            Err(_) => assert!(
                started.elapsed() < Duration::from_secs(60) && child.try_wait().unwrap().is_none(),
                "the CLI never reported that it was recording; log:\n{log}"
            ),
        }
    }
    std::thread::sleep(Duration::from_secs(3));

    // SAFETY: a signal to our own live child.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the CLI did not exit within 30s of SIGINT; log:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    while let Ok(line) = lines_rx.try_recv() {
        log.push_str(&line);
        log.push('\n');
    }
    assert_eq!(
        status.code(),
        Some(0),
        "a user quitting is exit 0, not a crash (a killed process has no code); log:\n{log}"
    );
    assert!(log.contains("shutting down"), "log:\n{log}");
    assert_eq!(
        mp4s_named(data_home.path(), "session-").len(),
        1,
        "the session must have been finalised by the stop; log:\n{log}"
    );
}
