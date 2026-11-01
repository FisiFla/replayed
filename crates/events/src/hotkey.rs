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
#[cfg(any(windows, test))]
use std::sync::mpsc::{sync_channel, TrySendError};
#[cfg(windows)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
#[cfg(any(windows, test))]
use std::time::Instant;

/// How long [`listen`] waits for the registering thread to report its outcome. The call itself
/// takes microseconds; the bound keeps a thread that never reaches it from hanging startup.
#[cfg(windows)]
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);

/// The `fsModifiers` bits `RegisterHotKey` takes, spelled out here so the flag set is a plain
/// number this module can unit-test on any host (the `windows` crate's wrappers only exist on
/// Windows). `tests::modifier_bits_match_the_windows_constants` pins them to the real ones.
#[cfg(any(windows, test))]
mod mod_bits {
    pub const ALT: u32 = 0x0001;
    pub const CONTROL: u32 = 0x0002;
    pub const SHIFT: u32 = 0x0004;
    /// `MOD_NOREPEAT`: holding the chord yields one `WM_HOTKEY`, not one per auto-repeat tick.
    pub const NOREPEAT: u32 = 0x4000;
}

/// How many presses may wait, unread, between the hotkey thread and its consumer. A consumer
/// that has stalled (a clip being saved, a slow recorder lock) must not let an unbounded queue
/// of presses build up behind it and then fire as a burst of clips once it wakes: a press that
/// finds the queue full is dropped. Eight is far more than a human can queue deliberately.
#[cfg(any(windows, test))]
const PRESS_QUEUE: usize = 8;

/// Two presses of one chord closer together than this count as one. `MOD_NOREPEAT` already
/// removes the keyboard's auto-repeat; this is the second line at the event source, so that
/// no consumer has to debounce for itself (a held chord that slipped past the flag, a bouncing
/// switch, a double tap that would otherwise save the same moment twice or stop a recording
/// the instant it started).
#[cfg(any(windows, test))]
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Collapses bursts of presses of one chord into a single press.
///
/// The quiet period is measured from the last press *seen*, not the last one *let through*: a
/// key that keeps repeating keeps itself suppressed until it is released, instead of sneaking
/// one press out every `DEBOUNCE`.
#[cfg(any(windows, test))]
struct PressGate {
    last_seen: Vec<Option<Instant>>,
}

#[cfg(any(windows, test))]
impl PressGate {
    fn new(chords: usize) -> Self {
        Self {
            last_seen: vec![None; chords],
        }
    }

    /// Whether the press of chord `index` at `now` should be delivered. An index outside the
    /// registered chords is never delivered.
    fn accept(&mut self, index: usize, now: Instant) -> bool {
        let Some(slot) = self.last_seen.get_mut(index) else {
            return false;
        };
        let quiet = slot.is_none_or(|prev| now.saturating_duration_since(prev) >= DEBOUNCE);
        *slot = Some(now);
        quiet
    }
}

/// The `RegisterHotKey` modifier flags for `hk`: its modifiers plus `MOD_NOREPEAT`.
///
/// `MOD_NOREPEAT` (Windows 7+) is what stops a held chord generating a `WM_HOTKEY` per
/// auto-repeat tick. Without it, holding Ctrl+F8 for a second saves dozens of clips (or flaps a
/// start/stop toggle). The probe and the listener both use this, so a chord the probe calls free
/// is registered exactly as the probe asked.
#[cfg(any(windows, test))]
fn modifier_bits(hk: &Hotkey) -> u32 {
    let mut bits = mod_bits::NOREPEAT;
    if hk.ctrl {
        bits |= mod_bits::CONTROL;
    }
    if hk.alt {
        bits |= mod_bits::ALT;
    }
    if hk.shift {
        bits |= mod_bits::SHIFT;
    }
    bits
}

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
    ///
    /// A letter or digit must carry Ctrl or Alt (Shift alone does not count; see the check
    /// below). A function key is accepted bare, with Shift, or with anything else.
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
        // `RegisterHotKey` claims the keypress system-wide, so a letter or digit chord would
        // stop the user typing that character in game chat or anywhere else for as long as
        // replayed runs. Only a function key has no character to take, so only it may stand
        // alone or with Shift alone. Shift is not enough for a letter or digit either: Shift+F
        // is how a capital F is typed, so it would swallow every capital F in chat. Ctrl or Alt
        // is what turns a keypress into a command. (The parser has no Win modifier.) The check
        // is on the parsed chord rather than the text, so no spelling of "no modifier" (`"K"`,
        // `" k "`, `"+K"`) can slip past it.
        let is_function_key = (0x70..=0x87).contains(&hk.vk);
        if !(hk.ctrl || hk.alt || is_function_key) {
            anyhow::bail!(
                "a letter or digit needs Ctrl or Alt: {spec:?} would stop you typing that key \
                 everywhere (Shift alone still types it). Use \"Ctrl+K\" or \"Alt+K\", or a \
                 function key (\"F8\", \"Shift+F8\")"
            );
        }
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
    // `f` followed by digits is a function key; a bare `f` is the letter F (`"Ctrl+F"`), which the
    // function-key branch used to swallow and reject as a malformed `F`.
    if let Some(n) = key
        .strip_prefix('f')
        .filter(|n| n.starts_with(|c: char| c.is_ascii_digit()))
    {
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

/// Start listening for one chord. Yields `()` once per press. A convenience over [`listen_all`]
/// for callers (the CLI) holding a single chord.
pub fn listen(hk: Hotkey) -> Result<HotkeyPresses<()>> {
    listen_mapped(vec![hk], |_| ())
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
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
    };
    // Away from the ids `listen_all` uses (1..=n) and from any id an application picks by
    // convention. Hotkey ids are per-thread, and this registration lasts microseconds.
    const PROBE_ID: i32 = 0x4000;
    let modifiers = HOT_KEY_MODIFIERS(modifier_bits(&hk));
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

/// Start listening for several chords. Yields the index (into `hks`) of the chord pressed.
///
/// Blocks until the registration outcome is known, so the `Err` is the truth about this
/// process. On Windows `RegisterHotKey` runs on a dedicated thread that owns the message queue
/// receiving `WM_HOTKEY`, and its error is carried back here. All-or-nothing: if any chord
/// fails, the ones already held are released and the error names the failed chord and the
/// likely cause, so a half-installed set never looks like a working one.
pub fn listen_all(hks: Vec<Hotkey>) -> Result<HotkeyPresses<usize>> {
    listen_mapped(hks, |index| index)
}

/// The presses of the chords a [`listen`] or [`listen_all`] registered, and the hold on them.
///
/// Read it like the [`Receiver`] it dereferences to. Dropping it gives the chords back: on
/// Windows `drop` stops the listener thread and waits for it, and the thread unregisters every
/// chord before it ends, so once `drop` returns the same chord can be claimed again — by this
/// process or by another. It used to be a bare `Receiver`, whose drop never woke the thread, so
/// a chord stayed registered for the life of the process.
pub struct HotkeyPresses<T> {
    presses: Receiver<T>,
    #[cfg(windows)]
    listener: Option<Listener>,
}

impl<T> std::ops::Deref for HotkeyPresses<T> {
    type Target = Receiver<T>;

    fn deref(&self) -> &Receiver<T> {
        &self.presses
    }
}

/// The thread holding the registrations, and how to tell it to let go.
#[cfg(windows)]
struct Listener {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

#[cfg(windows)]
impl<T> Drop for HotkeyPresses<T> {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.stop.store(true, Ordering::Release);
            // The thread sees `stop` within `STOP_POLL_MS` and unregisters its chords on the way
            // out; joining is what makes "dropped" mean "released".
            let _ = listener.thread.join();
        }
    }
}

/// How often the listener thread, idle, looks at its stop flag. A press wakes it at once; this
/// bounds only how long dropping a [`HotkeyPresses`] takes.
#[cfg(windows)]
const STOP_POLL_MS: u32 = 100;

#[cfg(windows)]
fn listen_mapped<T: Send + 'static>(
    hks: Vec<Hotkey>,
    press: fn(usize) -> T,
) -> Result<HotkeyPresses<T>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        MsgWaitForMultipleObjects, PeekMessageW, MSG, PM_REMOVE, QS_HOTKEY, WM_HOTKEY,
    };

    let chords = hks
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    // Bounded: see `PRESS_QUEUE`. The thread below drops a press the consumer has no room for.
    let (tx, rx) = sync_channel(PRESS_QUEUE);
    // The registration outcome: `Ok(())` once every chord is held, else `Err(message)`. A separate
    // channel from the presses, because one is a handshake and the other a stream.
    let (report_tx, report_rx) = channel::<std::result::Result<(), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_seen = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("replayed-hotkey".into())
        .spawn(move || {
            // SAFETY: called on a dedicated thread that owns the message queue.
            unsafe {
                for (index, hk) in hks.iter().enumerate() {
                    let modifiers = HOT_KEY_MODIFIERS(modifier_bits(hk));
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
                let mut gate = PressGate::new(hks.len());
                'listen: while !stop_seen.load(Ordering::Acquire) {
                    // Returns as soon as a `WM_HOTKEY` is queued, else after `STOP_POLL_MS`.
                    let _ = MsgWaitForMultipleObjects(None, false, STOP_POLL_MS, QS_HOTKEY);
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message != WM_HOTKEY {
                            continue;
                        }
                        let index = msg.wParam.0.saturating_sub(1);
                        if !gate.accept(index, Instant::now()) {
                            continue;
                        }
                        match tx.try_send(press(index)) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => break 'listen,
                        }
                    }
                }
                for index in 0..hks.len() {
                    let _ = UnregisterHotKey(None, index as i32 + 1);
                }
            }
        })?;
    let listener = Listener { stop, thread };

    match report_rx.recv_timeout(REGISTRATION_TIMEOUT) {
        Ok(Ok(())) => Ok(HotkeyPresses {
            presses: rx,
            listener: Some(listener),
        }),
        Ok(Err(message)) => {
            // The thread has already released what it held and returned.
            let _ = listener.thread.join();
            Err(anyhow::anyhow!(message))
        }
        Err(RecvTimeoutError::Timeout) => {
            // Not joined (it may be stuck), but told to stop: should it register late, it lets go
            // at its first look at the flag rather than holding chords nobody is reading.
            listener.stop.store(true, Ordering::Release);
            Err(anyhow::anyhow!(
                "the chords {chords} were not registered within {REGISTRATION_TIMEOUT:?}: the \
                 listener thread did not report back. Treat the hotkeys as NOT installed."
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = listener.thread.join();
            Err(anyhow::anyhow!(
                "the hotkey listener thread died before it could register {chords}. Treat \
                 the hotkeys as NOT installed."
            ))
        }
    }
}

/// Non-Windows builds have no global hotkey.
///
/// Deliberately still `Ok`: the CLI and desktop shell share this call site, and off Windows
/// they get a channel that never yields. [`supported`] lets a front-end say so.
#[cfg(not(windows))]
fn listen_mapped<T>(_hks: Vec<Hotkey>, _press: fn(usize) -> T) -> Result<HotkeyPresses<T>> {
    let (_tx, rx) = channel();
    Ok(HotkeyPresses { presses: rx })
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
    fn a_letter_or_digit_needs_ctrl_or_alt_but_a_function_key_needs_nothing() {
        // `RegisterHotKey` is system-wide and swallows the keypress: a bare `F` would stop the
        // user typing the letter F in game chat, in a search box, anywhere. A letter or a digit
        // therefore needs Ctrl or Alt, and Shift does not count: Shift+F is a capital F. A
        // function key has no character to eat, so it needs nothing.
        for bare in [
            "F", "f", "K", "5", "0", " k ", "Shift+F", "shift+k", "Shift+5",
        ] {
            let err = Hotkey::parse(bare).expect_err(bare);
            let text = err.to_string();
            assert!(
                text.contains("Ctrl or Alt"),
                "{bare:?}: the refusal says what is missing: {text}"
            );
        }
        // Still refused when the modifier is only spelled, not held: no key, no chord.
        assert!(Hotkey::parse("+K").is_err());
        // Bare function keys and any modified chord stay valid.
        for ok in [
            "F1",
            "f8",
            "F24",
            "Ctrl+K",
            "Alt+5",
            "Ctrl+F",
            "Shift+F8",
            "Shift+F24",
            "Ctrl+Shift+F",
            "Alt+Shift+5",
            "Ctrl+Alt+Shift+K",
        ] {
            assert!(Hotkey::parse(ok).is_ok(), "{ok} stays a valid chord");
        }
    }

    #[test]
    fn the_letter_f_is_a_key_and_f_digits_is_a_function_key() {
        // `"Ctrl+F"` is the letter F (VK 0x46). The function-key branch used to take any token
        // starting with `f`, fail to parse the empty remainder, and refuse it as a malformed `F`.
        let hk = Hotkey::parse("Ctrl+F").expect("the letter F is a valid chord key");
        assert_eq!(hk.vk, u32::from(b'F'));
        assert_eq!(hk.to_string(), "Ctrl+F");
        assert_eq!(Hotkey::parse(&hk.to_string()).unwrap(), hk);
        assert_eq!(Hotkey::parse("ctrl+f").unwrap().vk, 0x46);
        // Function keys are untouched, and a malformed one is still named as such.
        assert_eq!(Hotkey::parse("Ctrl+F1").unwrap().vk, 0x70);
        assert_eq!(Hotkey::parse("Ctrl+F24").unwrap().vk, 0x87);
        assert!(Hotkey::parse("Ctrl+F1x")
            .unwrap_err()
            .to_string()
            .contains("bad function key"));
        assert!(Hotkey::parse("Ctrl+Fx").is_err());
    }

    #[test]
    fn every_registration_carries_no_repeat() {
        // Holding the chord must be one press, not a stream: `MOD_NOREPEAT` is what asks the OS.
        let hk = Hotkey::parse("Ctrl+Alt+Shift+K").unwrap();
        let bits = modifier_bits(&hk);
        assert_eq!(
            bits,
            mod_bits::NOREPEAT | mod_bits::CONTROL | mod_bits::ALT | mod_bits::SHIFT
        );
        // Even a chord with no modifier (a bare function key) asks for no-repeat.
        let bare = Hotkey::parse("F8").unwrap();
        assert_eq!(modifier_bits(&bare), mod_bits::NOREPEAT);
        assert_eq!(
            modifier_bits(&Hotkey::parse("Ctrl+F8").unwrap()),
            mod_bits::NOREPEAT | mod_bits::CONTROL
        );
    }

    #[test]
    #[cfg(windows)]
    fn modifier_bits_match_the_windows_constants() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
        };
        assert_eq!(mod_bits::ALT, MOD_ALT.0);
        assert_eq!(mod_bits::CONTROL, MOD_CONTROL.0);
        assert_eq!(mod_bits::SHIFT, MOD_SHIFT.0);
        assert_eq!(mod_bits::NOREPEAT, MOD_NOREPEAT.0);
    }

    #[test]
    fn a_burst_of_presses_of_one_chord_is_one_press() {
        let t0 = Instant::now();
        let mut gate = PressGate::new(2);
        assert!(gate.accept(0, t0), "the first press always passes");
        // A key repeating every 30 ms for two seconds delivers nothing more: the quiet period
        // runs from the last press seen, so a held key never trickles one out per window.
        for tick in 1..=66u64 {
            assert!(
                !gate.accept(0, t0 + Duration::from_millis(30 * tick)),
                "tick {tick} of a held chord must be swallowed"
            );
        }
        // Released, then pressed again after a quiet period: a new press.
        assert!(gate.accept(0, t0 + Duration::from_millis(30 * 66) + DEBOUNCE));
    }

    #[test]
    fn debounce_is_per_chord_and_refuses_an_unknown_chord() {
        let t0 = Instant::now();
        let mut gate = PressGate::new(2);
        assert!(gate.accept(0, t0));
        // A different chord pressed in the same instant is not a repeat of the first.
        assert!(gate.accept(1, t0));
        assert!(!gate.accept(1, t0 + Duration::from_millis(10)));
        // A WM_HOTKEY id this listener never registered is dropped, not indexed out of bounds.
        assert!(!gate.accept(7, t0));
        assert!(!gate.accept(usize::MAX, t0));
    }

    #[test]
    fn the_press_queue_is_bounded_and_drops_instead_of_blocking() {
        // The forwarding rule both queues use: a full queue loses the press, and does not block
        // the thread that owns the message pump.
        let (tx, rx) = sync_channel::<usize>(PRESS_QUEUE);
        for i in 0..PRESS_QUEUE * 4 {
            match tx.try_send(i) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => panic!("receiver is alive"),
            }
        }
        assert_eq!(rx.try_iter().count(), PRESS_QUEUE);
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

    /// Dropping the listener gives its chord back, so this process can claim it again.
    ///
    /// The bug this rules out: the listener thread sat in `GetMessageW` for the life of the
    /// process, and dropping the receiver never woke it, so the chord stayed registered. The next
    /// `listen` on the same chord in the same process was refused as "already registered" —
    /// which is what the opt-in real-capture self-test hit when it ran after the hotkey-path one.
    ///
    /// The chord is obscure (Ctrl+Alt+Shift+F11) and is only registered, never pressed.
    #[test]
    #[cfg(windows)]
    fn a_dropped_listener_gives_its_chord_back_to_this_process() {
        let chord = Hotkey::parse("Ctrl+Alt+Shift+F11").unwrap();
        let first = match listen(chord) {
            Ok(presses) => presses,
            Err(err) => {
                eprintln!(
                    "SKIPPED: this session cannot register {chord} at all (an SSH or service \
                     session has no interactive window station): {err:#}"
                );
                return;
            }
        };
        assert!(
            listen(chord).is_err(),
            "while the first listener is alive the chord is held, so a second claim is refused"
        );
        drop(first);
        drop(listen(chord).expect("a dropped `listen` must release its chord"));
        drop(listen_all(vec![chord]).expect("the chord released by `listen` can be claimed again"));
        probe(chord).expect("a dropped `listen_all` must release its chord too");
    }
}
