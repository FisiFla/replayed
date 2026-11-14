//! Bringing this application's own window forward, without synthesising input.
//!
//! The obvious call, the window's `set_focus`, is not available to this shell. On Windows, tao
//! (the windowing layer under Tauri) implements it by *sending the Alt key*: Windows refuses
//! `SetForegroundWindow` from a process that did not receive the last input event, so tao
//! presses and releases a synthetic Alt through `SendInput` to make the system believe the
//! process did, then calls it. That is synthesised input, which `docs/anti-cheat.md` rules out
//! for this whole application: Riot Vanguard treats an injected keystroke as an automation
//! heuristic, and a user who opens the window from the tray in the middle of a League of Legends
//! match must not be the test case. The call cannot be audited from this repository either: the
//! `SendInput` lives in a dependency, so `scripts/check-forbidden-apis.sh` cannot see it, and the
//! only guard is to never reach the function. [`tests::nothing_calls_the_windows_own_set_focus`]
//! is that guard: it fails if the call is spelled anywhere in this crate.
//!
//! What this does instead, in order, all of it to our own window and none of it to another
//! process or to the keyboard:
//!
//! 1. Restore the window if it is minimised and show it, which are plain window-state changes.
//! 2. Ask for the foreground with `SetForegroundWindow`, a call about *our own* window. When the
//!    system allows it (this process is already the foreground one, or the user just clicked the
//!    tray icon, which counts as input to us) the window comes forward.
//! 3. When the system refuses, flash the window's taskbar button with `FlashWindowEx`. The window
//!    is visible and the taskbar says it wants attention, which is what the platform offers a
//!    background application that is not allowed to take the foreground. It is deliberately not
//!    worked around: a window that cannot raise itself over a full-screen game is the behaviour a
//!    player wants.
//!
//! Off Windows (development builds) there is no such restriction and no synthesised input to
//! avoid, but the guard above is textual, so those platforms take the plain request for attention
//! too.

use tauri::WebviewWindow;

/// What [`bring_forward`] ended up doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The window was asked for the foreground and the system allowed it.
    Foreground,
    /// The system refused the foreground, so the window's taskbar button was flashed instead.
    Flashed,
}

/// The steps [`bring_forward`] takes, as a trait so the order and the fallback can be tested
/// without a window.
pub trait Surface {
    /// Restore the window if it is minimised. Best effort: a window that is not minimised says no.
    fn unminimize(&self);
    /// Make the window visible.
    fn show(&self) -> Result<(), String>;
    /// Ask the system for the foreground. `true` when it was granted.
    fn foreground(&self) -> bool;
    /// Flash the taskbar button: the answer to a refused [`Surface::foreground`].
    fn flash(&self) -> Result<(), String>;
}

/// Show the window, and take the foreground if the system allows it.
///
/// An `Err` means the window could not even be shown; a refused foreground is a normal outcome
/// ([`Focus::Flashed`]), not a failure.
pub fn bring_forward(surface: &impl Surface) -> Result<Focus, String> {
    // Unminimise first: a window restored from the taskbar is minimised as often as hidden, and
    // `show` alone would leave it in the taskbar.
    surface.unminimize();
    surface.show()?;
    if surface.foreground() {
        return Ok(Focus::Foreground);
    }
    // A flash that fails costs only the hint; the window is up either way.
    if let Err(err) = surface.flash() {
        tracing::debug!("the taskbar button could not be flashed: {err}");
    }
    Ok(Focus::Flashed)
}

/// Bring `window` forward. See the module documentation for what that does and does not do.
pub fn bring_window_forward(window: &WebviewWindow) -> Result<Focus, String> {
    bring_forward(&Real(window))
}

/// The real window.
struct Real<'a>(&'a WebviewWindow);

impl Surface for Real<'_> {
    fn unminimize(&self) {
        let _ = self.0.unminimize();
    }

    fn show(&self) -> Result<(), String> {
        self.0.show().map_err(|err| err.to_string())
    }

    #[cfg(windows)]
    fn foreground(&self) -> bool {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

        let Ok(hwnd) = self.0.hwnd() else {
            return false;
        };
        // Rebuilt from the raw handle: Tauri's `HWND` is the one from its own `windows` crate
        // version, which need not be this crate's.
        // SAFETY: the handle is this process's own live window, and the call reads nothing else.
        unsafe { SetForegroundWindow(HWND(hwnd.0)) }.as_bool()
    }

    #[cfg(not(windows))]
    fn foreground(&self) -> bool {
        // No foreground lock to negotiate: ask for attention and report that the system, not this
        // process, decides.
        let _ = self
            .0
            .request_user_attention(Some(tauri::UserAttentionType::Informational));
        false
    }

    #[cfg(windows)]
    fn flash(&self) -> Result<(), String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            FlashWindowEx, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY,
        };

        let hwnd = self.0.hwnd().map_err(|err| err.to_string())?;
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: HWND(hwnd.0),
            // The taskbar button only, until the window is foreground: no flashing caption.
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        // SAFETY: `info` is fully initialised, its size is stated, and it names this process's
        // own window.
        // The return value is the window's previous flash state, not a success flag.
        let _ = unsafe { FlashWindowEx(&info) };
        Ok(())
    }

    #[cfg(not(windows))]
    fn flash(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A window that records what was done to it, and answers the foreground as told.
    struct Fake {
        granted: bool,
        shown: bool,
        log: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(granted: bool) -> Self {
            Self {
                granted,
                shown: true,
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl Surface for Fake {
        fn unminimize(&self) {
            self.log.borrow_mut().push("unminimize");
        }
        fn show(&self) -> Result<(), String> {
            self.log.borrow_mut().push("show");
            if self.shown {
                Ok(())
            } else {
                Err("gone".into())
            }
        }
        fn foreground(&self) -> bool {
            self.log.borrow_mut().push("foreground");
            self.granted
        }
        fn flash(&self) -> Result<(), String> {
            self.log.borrow_mut().push("flash");
            Ok(())
        }
    }

    #[test]
    fn a_granted_foreground_is_taken_after_the_window_is_restored_and_shown() {
        let fake = Fake::new(true);

        assert_eq!(bring_forward(&fake), Ok(Focus::Foreground));
        assert_eq!(*fake.log.borrow(), ["unminimize", "show", "foreground"]);
    }

    #[test]
    fn a_refused_foreground_flashes_the_taskbar_instead_of_forcing_it() {
        let fake = Fake::new(false);

        assert_eq!(bring_forward(&fake), Ok(Focus::Flashed));
        assert_eq!(
            *fake.log.borrow(),
            ["unminimize", "show", "foreground", "flash"],
            "a refusal is answered with a hint, and nothing else is tried"
        );
    }

    #[test]
    fn a_window_that_cannot_be_shown_is_an_error_and_asks_for_nothing() {
        let mut fake = Fake::new(true);
        fake.shown = false;

        assert!(bring_forward(&fake).is_err());
        assert_eq!(*fake.log.borrow(), ["unminimize", "show"]);
    }

    /// **The guard that keeps the Windows-own focus call from coming back.**
    ///
    /// tao's implementation of it sends a synthetic Alt through `SendInput` (see the module
    /// documentation), and that call is inside a dependency, so the repository's own
    /// forbidden-API script cannot see it. The only defence is to never write the call, and this
    /// test is what makes "never" checkable: it reads every Rust file of this crate and fails if
    /// the method is called anywhere, in code or in a test.
    ///
    /// The needle is assembled at run time so this file does not contain what it forbids.
    #[test]
    fn nothing_calls_the_windows_own_set_focus() {
        let needle = format!(".{}(", "set_focus");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        let mut dirs = vec![root.join("src"), root.join("tests")];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (n, line) in text.lines().enumerate() {
                        let code = line.trim_start();
                        if !code.starts_with("//") && line.contains(&needle) {
                            offenders.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the window's own focus call sends a synthetic Alt on Windows (anti-cheat): use \
             `window_focus::bring_window_forward` instead. Found at: {offenders:?}"
        );
    }
}
