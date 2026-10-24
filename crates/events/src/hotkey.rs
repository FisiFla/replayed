//! Global hotkey listener via `RegisterHotKey`.
//!
//! [`listen`] returns `Err` naming the chord when it cannot be registered, never failing
//! silently: a hotkey that does not exist and says nothing is the worst outcome here. The
//! usual cause is another process owning the combination, including a second copy of
//! replayed, because `RegisterHotKey` is system-wide. The CLI and the desktop app on the same
//! `config.toml` cannot both hold it; the loser is told, so neither clips twice.
//!
//! Off Windows [`listen`] returns a channel that never yields and [`supported`] answers
//! `false`, so a front-end can say so rather than promise a keypress nobody will hear.

use anyhow::Result;
use std::fmt;
#[cfg(windows)]
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// How long [`listen`] waits for the registering thread to report its outcome. The call itself
/// takes microseconds; the bound keeps a thread that never reaches it from hanging startup.
#[cfg(windows)]
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Key chord to listen for, parsed from `[hotkeys] clip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Virtual-key code, e.g. `0x77` for F8.
    pub vk: u32,
}

impl Hotkey {
    /// Accepts the `"Ctrl+F8"` form used in `config.example.toml`.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut hk = Self {
            ctrl: false,
            alt: false,
            shift: false,
            vk: 0,
        };
        let mut key = None;
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" => hk.ctrl = true,
                "alt" => hk.alt = true,
                "shift" => hk.shift = true,
                other => key = Some(other.to_string()),
            }
        }
        hk.vk = parse_key(key.as_deref())?;
        Ok(hk)
    }
}

/// The chord as a user should read it: modifiers first in a fixed order, then the key. Both
/// front-ends print this spelling (`ctrl+f8` in config shows as `Ctrl+F8`), and a registration
/// failure names it.
impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (set, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
        ] {
            if set {
                write!(f, "{name}+")?;
            }
        }
        match key_name(self.vk) {
            Some(name) => f.write_str(&name),
            // Unreachable for a `Hotkey` from `parse`; print the virtual-key code rather than something
            // that looks like a key.
            None => write!(f, "VK({:#04x})", self.vk),
        }
    }
}

/// The name of a virtual-key code this module can produce, or `None` for any other.
fn key_name(vk: u32) -> Option<String> {
    match vk {
        // VK_F1..VK_F24
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        // VK_A..VK_Z and VK_0..VK_9 are the ASCII codes of their characters.
        v if (u32::from(b'A')..=u32::from(b'Z')).contains(&v)
            || (u32::from(b'0')..=u32::from(b'9')).contains(&v) =>
        {
            Some(char::from(v as u8).to_string())
        }
        _ => None,
    }
}

/// Whether this build can install a global hotkey at all: `true` on Windows. A front-end that
/// shows a chord without asking this would promise a keypress that cannot arrive.
pub const fn supported() -> bool {
    cfg!(windows)
}

fn parse_key(key: Option<&str>) -> Result<u32> {
    let key = key.ok_or_else(|| anyhow::anyhow!("hotkey has no main key (e.g. \"Ctrl+F8\")"))?;
    if let Some(n) = key.strip_prefix('f') {
        let n: u32 = n
            .parse()
            .map_err(|_| anyhow::anyhow!("bad function key: F{n}"))?;
        if (1..=24).contains(&n) {
            return Ok(0x70 + n - 1); // VK_F1..VK_F24
        }
        anyhow::bail!("function key out of range: F{n}");
    }
    // Exactly one character. Taking the first character of a longer token made a typo look like a
    // working chord: `"Ctrl+Hovercraft"` parsed as Ctrl+H, installed, and answered a key the user
    // never named. A rebinding panel turns that from a curiosity into a trap.
    let mut chars = key.chars();
    let ch = chars.next().ok_or_else(|| anyhow::anyhow!("empty key"))?;
    if chars.next().is_some() {
        anyhow::bail!(
            "unsupported key: {key:?} — a chord's key is one character (\"K\", \"5\") or a \
             function key (\"F8\")"
        );
    }
    if ch.is_ascii_alphanumeric() {
        return Ok(ch.to_ascii_uppercase() as u32); // VK_A..VK_Z / VK_0..VK_9
    }
    anyhow::bail!("unsupported key: {key:?}")
}

/// Start listening for one chord. Returns a channel that yields once per press. A convenience
/// over [`listen_all`] for callers (the CLI) holding a single chord.
pub fn listen(hk: Hotkey) -> Result<Receiver<()>> {
    let presses = listen_all(vec![hk])?;
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("replayed-hotkey-fwd".into())
        .spawn(move || {
            while presses.recv().is_ok() {
                if tx.send(()).is_err() {
                    break;
                }
            }
        })?;
    Ok(rx)
}

/// Ask whether the platform will let this process own `hk` right now, and give it back.
///
/// `RegisterHotKey` is system-wide, so a chord's availability is only knowable by asking to own
/// it: a rebinding panel that wrote a chord without asking would produce a file that loads, a
/// process that starts, and a key nothing answers. The probe registers and releases immediately,
/// on the calling thread, with an id of its own (the listener thread's ids start at 1).
///
/// # What it does not promise
///
/// That the chord is free *later*. Another application can take it between the probe and the next
/// launch, which is why the chord is still installed (and can still fail, loudly) at startup. The
/// answer is "free as of this moment", which is exactly what a user choosing a chord needs.
///
/// Off Windows this returns an error rather than `Ok`: a caller that ignored [`supported`] would
/// otherwise report a chord free on a machine where no chord can ever fire.
#[cfg(windows)]
pub fn probe(hk: Hotkey) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
    };
    // Away from the ids `listen_all` uses (1..=n) and from any id an application picks by
    // convention. Hotkey ids are per-thread, and this registration lasts microseconds.
    const PROBE_ID: i32 = 0x4000;
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    if hk.ctrl {
        modifiers |= MOD_CONTROL;
    }
    if hk.alt {
        modifiers |= MOD_ALT;
    }
    if hk.shift {
        modifiers |= MOD_SHIFT;
    }
    // SAFETY: a registration on this thread, released by this thread below.
    unsafe {
        match RegisterHotKey(None, PROBE_ID, modifiers, hk.vk) {
            Ok(()) => {
                let _ = UnregisterHotKey(None, PROBE_ID);
                Ok(())
            }
            Err(err) => Err(format!(
                "{hk} is already held by another application (RegisterHotKey failed: {err}). \
                 Another replayed counts too: the CLI and this app cannot both hold one chord. \
                 Close the other application, or choose a different chord."
            )),
        }
    }
}

/// Off Windows there is no registration to probe.
#[cfg(not(windows))]
pub fn probe(_hk: Hotkey) -> Result<(), String> {
    Err("a global hotkey is a Windows feature, and this build has nothing to register".to_string())
}

/// Start listening for several chords. The channel yields the index (into `hks`) of the chord
/// pressed.
///
/// Blocks until the registration outcome is known, so the `Err` is the truth about this
/// process. On Windows `RegisterHotKey` runs on a dedicated thread that owns the message queue
/// receiving `WM_HOTKEY`, and its error is carried back here. All-or-nothing: if any chord
/// fails, the ones already held are released and the error names the failed chord and the
/// likely cause, so a half-installed set never looks like a working one.
#[cfg(windows)]
pub fn listen_all(hks: Vec<Hotkey>) -> Result<Receiver<usize>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    let chords = hks
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let (tx, rx) = channel();
    // The registration outcome: `Ok(())` once every chord is held, else `Err(message)`. A separate
    // channel from the presses, because one is a handshake and the other a stream.
    let (report_tx, report_rx) = channel::<std::result::Result<(), String>>();
    std::thread::Builder::new()
        .name("replayed-hotkey".into())
        .spawn(move || {
            // SAFETY: called on a dedicated thread that owns the message queue.
            unsafe {
                for (index, hk) in hks.iter().enumerate() {
                    let mut modifiers = HOT_KEY_MODIFIERS(0);
                    if hk.ctrl {
                        modifiers |= MOD_CONTROL;
                    }
                    if hk.alt {
                        modifiers |= MOD_ALT;
                    }
                    if hk.shift {
                        modifiers |= MOD_SHIFT;
                    }
                    if let Err(err) = RegisterHotKey(None, index as i32 + 1, modifiers, hk.vk) {
                        for held in 0..index {
                            let _ = UnregisterHotKey(None, held as i32 + 1);
                        }
                        let _ = report_tx.send(Err(format!(
                            "the chord {hk} could not be registered (RegisterHotKey failed: \
                             {err}). Another application — or another replayed (the CLI and \
                             this app cannot both hold one chord) — already owns it. Nothing \
                             will happen when {hk} is pressed. Close the other application, \
                             or set a different chord under [hotkeys] in config.toml and \
                             restart."
                        )));
                        return;
                    }
                }
                let _ = report_tx.send(Ok(()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY {
                        let index = msg.wParam.0.saturating_sub(1);
                        if tx.send(index).is_err() {
                            break;
                        }
                    }
                }
            }
        })?;

    match report_rx.recv_timeout(REGISTRATION_TIMEOUT) {
        Ok(Ok(())) => Ok(rx),
        Ok(Err(message)) => Err(anyhow::anyhow!(message)),
        Err(RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
            "the chords {chords} were not registered within {REGISTRATION_TIMEOUT:?}: the \
             listener thread did not report back. Treat the hotkeys as NOT installed."
        )),
        Err(RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
            "the hotkey listener thread died before it could register {chords}. Treat \
             the hotkeys as NOT installed."
        )),
    }
}

/// Non-Windows builds have no global hotkey.
///
/// Deliberately still `Ok`: the CLI and desktop shell share this call site, and off Windows
/// they get a channel that never yields. [`supported`] lets a front-end say so.
#[cfg(not(windows))]
pub fn listen_all(_hks: Vec<Hotkey>) -> Result<Receiver<usize>> {
    let (_tx, rx) = channel();
    Ok(rx)
}

/// Poll helper so both platforms share one call site.
pub fn wait_for_press(rx: &Receiver<()>, timeout: Duration) -> bool {
    rx.recv_timeout(timeout).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_function_key_chord() {
        let hk = Hotkey::parse("Ctrl+F8").expect("Ctrl+F8 must parse");
        assert_eq!(
            hk,
            Hotkey {
                ctrl: true,
                alt: false,
                shift: false,
                vk: 0x77
            }
        );
    }

    #[test]
    fn a_key_that_is_not_one_character_is_refused_rather_than_cut_down_to_one() {
        // The bug this rules out: `"Ctrl+Hovercraft"` used to parse as Ctrl+H, so a typo became a
        // working hotkey for a key the user never named.
        let err = Hotkey::parse("Ctrl+Hovercraft").unwrap_err();
        assert!(
            format!("{err}").contains("one character"),
            "the refusal says what a key is: {err}"
        );
        // One character is still a chord, in either case.
        assert_eq!(Hotkey::parse("Ctrl+K").unwrap().vk, u32::from(b'K'));
        assert_eq!(Hotkey::parse("ctrl+k").unwrap().vk, u32::from(b'K'));
        // And a bare modifier is still the old, named error.
        assert!(format!("{}", Hotkey::parse("Ctrl").unwrap_err()).contains("no main key"));
    }

    #[test]
    fn parses_the_example_config_default() {
        // config.example.toml sets `clip = "Ctrl+F8"`; the parsed vk is VK_F8.
        let hk = Hotkey::parse("Ctrl+F8").unwrap();
        assert_eq!(hk.vk, 0x70 + 8 - 1);
    }

    #[test]
    fn rejects_a_spec_with_no_main_key() {
        let err = Hotkey::parse("Ctrl+Shift").unwrap_err();
        assert!(
            err.to_string().contains("no main key"),
            "expected a missing-main-key error, got: {err}"
        );
    }

    #[test]
    fn rejects_an_out_of_range_function_key() {
        assert!(Hotkey::parse("Ctrl+F25").is_err());
        assert!(Hotkey::parse("Ctrl+F0").is_err());
    }

    #[test]
    fn rejects_an_unsupported_symbol_key() {
        assert!(Hotkey::parse("Ctrl+@").is_err());
    }

    #[test]
    fn displays_the_chord_in_a_fixed_readable_form() {
        // The same spelling both front-ends print, whatever case or order the config used.
        assert_eq!(Hotkey::parse("Ctrl+F8").unwrap().to_string(), "Ctrl+F8");
        assert_eq!(Hotkey::parse("ctrl+f8").unwrap().to_string(), "Ctrl+F8");
        assert_eq!(
            Hotkey::parse("shift+ctrl+alt+f24").unwrap().to_string(),
            "Ctrl+Alt+Shift+F24"
        );
        assert_eq!(Hotkey::parse("Alt+A").unwrap().to_string(), "Alt+A");
        assert_eq!(Hotkey::parse("Ctrl+7").unwrap().to_string(), "Ctrl+7");
        assert_eq!(Hotkey::parse("F1").unwrap().to_string(), "F1");
    }

    #[test]
    fn the_displayed_chord_parses_back_to_the_same_hotkey() {
        // What the UI shows is what the listener holds: a display that dropped a modifier
        // would tell the user to press a chord that does nothing.
        for spec in ["Ctrl+F8", "Ctrl+Alt+Shift+K", "Shift+F12", "Ctrl+9"] {
            let parsed = Hotkey::parse(spec).unwrap();
            assert_eq!(
                Hotkey::parse(&parsed.to_string()).unwrap(),
                parsed,
                "round trip: {spec}"
            );
        }
    }

    #[test]
    fn a_chord_outside_the_key_names_prints_its_virtual_key_code() {
        // No `parse` path can build this, and the display must not invent a key name for it.
        let odd = Hotkey {
            ctrl: true,
            alt: false,
            shift: false,
            vk: 0x5B,
        };
        assert_eq!(odd.to_string(), "Ctrl+VK(0x5b)");
    }

    #[test]
    fn a_global_hotkey_exists_only_on_windows() {
        // The desktop shell asks this before telling a user which chord to press: off
        // Windows `listen` returns a channel nothing ever writes to.
        assert_eq!(supported(), cfg!(windows));
    }

    #[test]
    #[cfg(not(windows))]
    fn listening_off_windows_reports_no_failure_and_never_fires() {
        // The non-Windows stub is a working "there is no hotkey here", not an error: the CLI and the
        // desktop shell share this call site. Gated to non-Windows because that is the claim: on
        // Windows `listen` really registers the chord, which fails without an interactive window
        // station (`0x800705B3`: SSH, a service account, a headless CI runner), so an assertion
        // there would be about the host, not the code.
        let chord = Hotkey::parse("Ctrl+F8").unwrap();
        let rx = listen(chord).expect("the non-Windows listener is a stub, not a failure");
        assert!(
            !wait_for_press(&rx, Duration::from_millis(50)),
            "nothing may fire off Windows"
        );
        assert!(!supported());
    }

    /// The Windows counterpart: a chord this process cannot hold is reported, not panicked and not
    /// silently a dead key.
    ///
    /// Whether it is held depends on the session (an interactive desktop holds it; SSH, a service
    /// or headless CI cannot); both outcomes are accepted. This test never synthesises a keypress:
    /// that is forbidden on a machine running an anti-cheat. What is checkable without input is
    /// that a failure is a message naming the chord.
    #[test]
    #[cfg(windows)]
    fn a_chord_this_process_cannot_hold_is_reported_and_never_panics() {
        let chord = Hotkey::parse("Ctrl+F8").unwrap();
        if let Err(err) = listen(chord) {
            let text = err.to_string();
            assert!(
                text.contains("Ctrl+F8"),
                "the failure must name the chord: {text}"
            );
            assert!(
                text.contains("could not be registered") || text.contains("NOT installed"),
                "the failure must say what went wrong, not just that something did: {text}"
            );
        }
    }
}
