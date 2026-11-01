//! Graceful shutdown for the headless CLI: Ctrl+C, Ctrl+Break and closing the console.
//!
//! # Why this exists
//!
//! The command's own loop could only be left through an error. The way a person actually quits a
//! console program, Ctrl+C or the window's close button, ran no Rust code at all: the default
//! handler ended the process, so [`replayed_recorder::Recorder::stop`] and `Drop` never ran.
//! A clip waiting out its post-roll was abandoned (no file, no index row), a full-session
//! recording was never concatenated into its session file (footage came back only through
//! recovery on the next start), and a script wrapping the CLI could not tell a user quitting
//! from a crash, because no exit path returned success.
//!
//! # What it does
//!
//! It installs a handler that does exactly one thing: set a flag ([`Shutdown::request`]). The
//! driver loop polls the flag every `HOTKEY_POLL`, stops the recorder (which flushes the encoder,
//! closes the capture session and writes the session file) and returns `Ok`. A clip already in
//! flight is not interrupted, because the handler returns before the loop notices: the loop is
//! inside `clip_now`, which finishes the clip, and only then reads the flag.
//!
//! # Press it twice to force it
//!
//! A shutdown that cannot finish (an encoder that will not exit, a session too long to
//! concatenate) must not be unkillable, and a handler that swallows Ctrl+C would make it so. The
//! first request is graceful; a second one while the first is pending is handed back to the
//! system's default behaviour, which ends the process.
//!
//! # Closing the console window
//!
//! On Windows the system ends the process when a close, logoff or shutdown handler returns, and
//! gives it only a few seconds. The handler for those events therefore *waits*, up to
//! [`CLOSE_GRACE`], for the run to report that the recorder has stopped
//! ([`Shutdown::mark_finished`]) before it returns. A session too long to finalise in that time
//! is cut off by the system, which is the same outcome as before and no worse; Ctrl+C has no such
//! limit.
//!
//! This is a console-control handler for this process's own console. It touches no other process,
//! installs no hook and synthesises no input (see `docs/anti-cheat.md`).

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// How long the close/logoff/shutdown handler waits for the recorder to stop, before it lets the
/// system end the process. Under the few seconds Windows grants such a handler.
#[cfg(windows)]
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_millis(4_500);

#[derive(Debug, Default)]
struct State {
    /// A shutdown was asked for (a signal, or a test).
    requested: AtomicBool,
    /// The run has stopped the recorder, so a handler that is waiting on it may return.
    finished: AtomicBool,
}

/// A request to stop, shared between whatever raises it (the process's signal handler, or a test)
/// and the driver loop that polls it. Cloning shares the same flag.
#[derive(Debug, Clone, Default)]
pub struct Shutdown(Arc<State>);

/// The instance the process-wide handler reports to. A handler is a bare function with no
/// context, so it finds the flag through this.
static INSTALLED: OnceLock<Shutdown> = OnceLock::new();

impl Shutdown {
    /// A flag nothing has raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a shutdown. Returns whether one had already been asked for.
    pub fn request(&self) -> bool {
        self.0.requested.swap(true, Ordering::SeqCst)
    }

    /// Whether a shutdown was asked for.
    pub fn is_requested(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    /// Record that the recorder has stopped, releasing a close handler that is waiting for it.
    pub fn mark_finished(&self) {
        self.0.finished.store(true, Ordering::SeqCst);
    }

    #[cfg(windows)]
    fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::SeqCst)
    }

    /// Make this flag the one the process's Ctrl+C / close / termination handling raises.
    ///
    /// Process-wide, so at most one flag can be installed; a second call is an error rather than
    /// a silently ignored one.
    pub fn install(&self) -> Result<()> {
        if INSTALLED.set(self.clone()).is_err() {
            bail!("a shutdown handler is already installed in this process");
        }
        platform::install()
    }
}

#[cfg(windows)]
mod platform {
    use super::{CLOSE_GRACE, INSTALLED};
    use anyhow::{bail, Result};
    use std::time::{Duration, Instant};
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    /// Runs on a thread the system creates for the event; it must stay short and touch nothing but
    /// the flag.
    unsafe extern "system" fn handler(event: u32) -> BOOL {
        let Some(shutdown) = INSTALLED.get() else {
            return 0;
        };
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                // `0` (not handled) passes a second request to the default handler, which ends
                // the process: the way out of a shutdown that is stuck.
                if shutdown.request() {
                    0
                } else {
                    1
                }
            }
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                shutdown.request();
                // The system ends the process the moment this returns, so wait for the run to
                // have stopped the recorder (bounded: the system also gives up on us).
                let deadline = Instant::now() + CLOSE_GRACE;
                while !shutdown.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                1
            }
            _ => 0,
        }
    }

    pub fn install() -> Result<()> {
        // SAFETY: `handler` has the signature the API requires, lives for the whole process, and
        // only reads a `OnceLock` and flips atomics.
        if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
            bail!(
                "SetConsoleCtrlHandler failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
}

#[cfg(unix)]
mod platform {
    use super::INSTALLED;
    use anyhow::{bail, Result};

    /// Signal handler: only async-signal-safe work (an atomic load and store, `signal`, `raise`).
    extern "C" fn handler(signal: libc::c_int) {
        if let Some(shutdown) = INSTALLED.get() {
            if !shutdown.request() {
                return;
            }
        }
        // A second request, or no flag to raise: restore the default action and re-deliver, which
        // ends the process, the way out of a shutdown that is stuck.
        // SAFETY: both calls are async-signal-safe.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    pub fn install() -> Result<()> {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: `handler` is an `extern "C" fn(c_int)`, which is what `signal` takes.
            let previous =
                unsafe { libc::signal(signal, handler as *const () as libc::sighandler_t) };
            if previous == libc::SIG_ERR {
                bail!(
                    "installing the handler for signal {signal} failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok(())
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use anyhow::Result;

    pub fn install() -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_seen_by_every_clone_and_the_second_one_is_reported() {
        let shutdown = Shutdown::new();
        let seen_by_the_loop = shutdown.clone();
        assert!(!seen_by_the_loop.is_requested());

        assert!(!shutdown.request(), "the first request is the graceful one");
        assert!(seen_by_the_loop.is_requested());
        assert!(
            shutdown.request(),
            "a second request while the first is pending says so: that is the force-quit"
        );
    }
}
