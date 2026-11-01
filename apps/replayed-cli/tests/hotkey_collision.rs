//! A clip hotkey that cannot be registered must refuse the run BEFORE the recorder starts.
//!
//! # Why the order matters
//!
//! The usual reason `RegisterHotKey` refuses the chord is that another replayed is running on the
//! same `config.toml` (the desktop app, or a second CLI) and holds it. `Recorder::start` runs crash
//! recovery over the data directory, which closes, finalises or deletes every session row still
//! open, and nothing tells it that the row's owner is alive. The CLI used to install the hotkey
//! after the recorder had started, so a refused chord was reported only once the live run's
//! session had already been "recovered" out from under it. The chord is now claimed first.
//!
//! # What this proves
//!
//! A chord held in this process is refused with the hotkey error, and a session row seeded as
//! still open (the live run's) is untouched afterwards, with no recorder state created beside it.
//! The test is Windows-only because `RegisterHotKey` is: off Windows the chord is refused for a
//! different reason (nothing can register one), which says nothing about the order.
//!
//! The chord is deliberately obscure (Ctrl+Alt+Shift+F12) and is only ever registered, never
//! pressed: nothing here sends input of any kind.

#![cfg(windows)]
// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]

use replayed_cli::{run_buffer_with, BufferOptions};
use replayed_events::hotkey::{self, Hotkey};
use replayed_store::{Store, SESSION_MODE_BUFFER};

const CHORD: &str = "Ctrl+Alt+Shift+F12";

#[test]
fn a_refused_hotkey_leaves_a_live_sessions_row_alone() {
    let app_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        app_dir.path().join("config.toml"),
        include_str!("../../../config.example.toml")
            .replace("clip = \"Ctrl+F8\"", &format!("clip = \"{CHORD}\"")),
    )
    .unwrap();

    // The "other replayed": a session row that is open because its owner is alive and recording.
    let db = app_dir.path().join("replayed.db");
    let id = {
        let store = Store::open(&db).unwrap();
        store.migrate().unwrap();
        store
            .start_session(
                Some("game"),
                SESSION_MODE_BUFFER,
                1_000,
                "scratch-of-the-live-run",
                0,
            )
            .unwrap()
    };

    // ... which holds the chord. Kept alive for the whole test. Where the chord cannot be held
    // at all there is no collision to stage: an SSH session (Windows session 0) has no interactive
    // window station, so `RegisterHotKey` fails for everyone. That is reported, not passed over
    // silently, and the test is run from the desktop session (`docs/runbooks/`) to count.
    let _held = match hotkey::listen(Hotkey::parse(CHORD).unwrap()) {
        Ok(held) => held,
        Err(err) => {
            eprintln!(
                "SKIPPED: this session cannot register {CHORD} to hold it (an SSH or service \
                 session has no interactive window station), so no collision can be staged: {err:#}"
            );
            return;
        }
    };

    let err = run_buffer_with(BufferOptions {
        app_dir: app_dir.path().to_path_buf(),
        self_test_clip_after: None,
        dev_software_encoder: false,
        mode: None,
        stub_sources: false,
    })
    .expect_err("a chord held elsewhere must refuse the run");
    assert!(
        format!("{err:#}").contains("installing the clip hotkey"),
        "the refusal should name the hotkey, got: {err:#}"
    );

    let store = Store::open(&db).unwrap();
    let row = store
        .get_session(id)
        .unwrap()
        .expect("the session row is still there");
    assert_eq!(
        row.ended_at_ms, None,
        "the refused run must not have recovered (closed) another run's live session"
    );
    for dir in ["scratch", "clips"] {
        assert!(
            !app_dir.path().join(dir).exists(),
            "the recorder must not have started: it creates {dir}/ when it does"
        );
    }
}
