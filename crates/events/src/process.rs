//! Is a watched game running? Two signals, one answer.
//!
//! Auto-record needs one fact: is a watched title running now. Two sources answer it behind
//! one trait ([`GamePresence`]):
//!
//! | source | how it answers | what it can see |
//! |---|---|---|
//! | [`ProcessWatcher`] | a `CreateToolhelp32Snapshot` process list, compared by image name | any title whose executable is configured |
//! | [`LiveClientPresence`] | Riot's Live Client Data API, a `GET` on loopback | League of Legends, while a match is being played |
//!
//! Both are polled on a timer ([`spawn`], [`DEFAULT_POLL_INTERVAL`]) and answer the same
//! [`Presence`], so debounce, the "already running at start" case and "report a change exactly
//! once" are written once ([`PresenceTracker`]) and tested without a clock, socket or process
//! list.
//!
//! # Which signal for which title
//!
//! Riot publishes a game-state API for one title on the default list: League's Live Client Data
//! API, served on `127.0.0.1:2999` only while a match runs (not in the client, lobby or champ
//! select). League is detected through that read-only `GET`, which cannot fire outside a game.
//!
//! Everything else is recognised by executable name. That includes Valorant: Riot publishes no
//! local game-state service for it (the Riot Client's lockfile-protected HTTP API reports the
//! launcher's session, not a match, is undocumented, and is not used). Valorant is watched by
//! `VALORANT-Win64-Shipping.exe`, the game process present only during a match, not
//! `VALORANT.exe`, the launcher that would be a false positive all evening. Likewise
//! `League of Legends.exe` not `LeagueClient.exe`, and `cs2.exe` not `steam.exe`.
//!
//! # What the process watcher does and does not do
//!
//! [`ProcessWatcher`] calls `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` and walks the
//! `PROCESSENTRY32W` records, the enumeration Task Manager performs. It:
//!
//! * never calls `OpenProcess`: no process is opened and no access right requested; a name is
//!   copied out of the snapshot and compared to the configured list;
//! * so reads no memory, injects nothing, hooks nothing and touches no module, window, thread
//!   or handle of another process;
//! * opens no socket (`tests/no_egress.rs` asserts the only socket clients are the loopback
//!   modules);
//! * holds no handle between polls: the snapshot handle is closed before `poll` returns.
//!
//! This matters next to kernel-level anti-cheat. Enumerating image names is what every
//! launcher and overlay does and is the least invasive way to answer the question, but it is
//! still a new capability, so it is off by default: [`GamesSection::auto_record`] is `false`,
//! and with it off nothing is polled, no snapshot taken and no request made
//! ([`GamesSection::start`] returns `None`).
//!
//! # Unverified on Windows
//!
//! [`processes`] is type-checked against `x86_64-pc-windows-msvc` but its syscalls are
//! unproven here. It is kept to four calls (`CreateToolhelp32Snapshot`, `Process32FirstW`,
//! `Process32NextW`, `CloseHandle`) so the unproven part is small, and everything that decides
//! anything ([`presences_in`]) is pure and tested everywhere. If a protected process's image
//! name were absent from a snapshot under kernel anti-cheat, the result would be a game never
//! detected: a false negative, never a wrong match, since a match requires an equal file name.

use crate::lol::{Endpoint, LoopbackClient, Response};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a game is looked for by default. A few seconds is far below the time to reach a
/// game from launch, and the replay buffer's `pre_seconds` covers the gap, so a start noticed
/// a couple of seconds late keeps its footage.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(2_500);

/// The same interval, as the `poll_ms` a `config.toml` writes.
pub const DEFAULT_POLL_MS: u64 = 2_500;

/// The shortest interval a configured `poll_ms` can produce.
///
/// At a few milliseconds the watcher would burn CPU enumerating processes in a tight loop and,
/// under kernel anti-cheat, hammer `CreateToolhelp32Snapshot` hundreds of times a second, the
/// profile of a tool nobody wants beside their game. The clamp lives in the one function
/// turning configuration into an interval ([`clamp_interval`]) and is tested.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The longest interval a configured `poll_ms` can produce: past half a minute the feature no
/// longer does its job.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How many consecutive polls must agree before a change is believed.
///
/// Two, so one flickering poll (a game restarting a worker, a process mid-exit) cannot start
/// or end a session. The cost is one extra interval (~2.5 s) of transition latency, inside the
/// capture buffer's pre-roll.
pub const CONFIRMATIONS: u32 = 2;

/// How long [`WatchHandle`]'s thread sleeps at a time so `stop` does not wait a whole interval.
const STOP_SLICE: Duration = Duration::from_millis(50);

/// A game this build knows how to recognise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedGame {
    /// Human name shown in the tray/panel: "League of Legends".
    pub name: String,
    /// Windows executable to compare against, case-insensitively: "League of Legends.exe".
    pub exe: String,
    /// How this title is detected: [`Signal::LiveClientApi`] for Riot titles exposing the official
    /// Live Client Data API, otherwise [`Signal::Process`], the default when a configuration entry
    /// omits it. Only a title that publishes the API can be detected that way.
    #[serde(default)]
    pub signal: Signal,
}

impl WatchedGame {
    /// A title recognised by its executable's file name.
    pub fn by_process(name: impl Into<String>, exe: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            exe: exe.into(),
            signal: Signal::Process,
        }
    }

    /// A Riot title recognised by the Live Client Data API it publishes.
    pub fn by_live_client_api(name: impl Into<String>, exe: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            exe: exe.into(),
            signal: Signal::LiveClientApi,
        }
    }

    /// The file name this title is compared by: the `exe` with any path removed and surrounding
    /// whitespace trimmed.
    pub fn file_name(&self) -> &str {
        file_name(&self.exe)
    }
}

impl fmt::Display for WatchedGame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.exe)
    }
}

/// How a title is detected. The variants are not interchangeable for a given title: Riot serves
/// the Live Client Data API for League and nothing else on the default list, and a title with
/// no such API cannot be found by asking for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// By executable name from a process-list snapshot, the only mechanism for a title that
    /// publishes nothing on loopback.
    #[default]
    Process,
    /// Riot's Live Client Data API on loopback, served only while a match runs. League publishes
    /// one; no other default title does.
    LiveClientApi,
}

impl Signal {
    /// The signal a configuration file names, or `None` for anything else.
    ///
    /// The strings are the serde names (`process`, `live_client_api`), so a hand-written
    /// `[[games.watch]]` entry and a serialised one mean the same thing. Used by a reader that has
    /// to parse the file itself rather than through serde (the desktop shell's settings panel,
    /// which edits the list in place and must not reformat the rest of the file).
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "process" => Some(Signal::Process),
            "live_client_api" => Some(Signal::LiveClientApi),
            _ => None,
        }
    }
}

/// One poll's answer about one watched game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub game: WatchedGame,
    pub running: bool,
}

/// A detector polled on a timer.
///
/// Implementations must be cheap: [`spawn`] polls every few seconds for the life of the tray
/// process and must not show in Task Manager. Neither implementation holds a handle, socket or
/// connection between polls, or allocates anything outliving one.
///
/// The `Result` separates "I could not look this time" from "I looked and nothing is there";
/// [`GameWatcher::poll_once`] relies on it, since only the second is evidence about a game.
pub trait GamePresence: Send {
    fn poll(&mut self) -> Result<Vec<Presence>>;
}

/// Which of `watched` are present in this snapshot of image names?
///
/// The whole decision a process watcher makes, as a pure function over `&[String]` with no
/// system state, lock or clock, so it can be driven exhaustively on any platform.
/// [`ProcessWatcher::poll`] only hands it the names. Every watched game gets an answer, present
/// or not, so the result is as long as `watched`.
pub fn presences_in(watched: &[WatchedGame], observed: &[String]) -> Vec<Presence> {
    watched
        .iter()
        .map(|game| Presence {
            game: game.clone(),
            running: observed
                .iter()
                .any(|name| image_name_matches(&game.exe, name)),
        })
        .collect()
}

/// The process id of the first title in `watched` (list order) found among `observed`
/// `(image name, pid)` pairs.
pub fn pid_in(watched: &[WatchedGame], observed: &[(String, u32)]) -> Option<u32> {
    watched.iter().find_map(|game| {
        observed
            .iter()
            .find(|(name, _)| image_name_matches(&game.exe, name))
            .map(|&(_, pid)| pid)
    })
}

/// The process id of a running watched title, from the same snapshot [`ProcessWatcher`] takes.
///
/// For per-application audio capture, which hands the id to the audio service. The id is only
/// read out of the snapshot: the process is never opened.
pub fn running_game_pid(watched: &[WatchedGame]) -> Result<Option<u32>> {
    #[cfg(windows)]
    {
        Ok(pid_in(watched, &processes()?))
    }
    #[cfg(not(windows))]
    {
        let _ = watched;
        bail!("finding a game's process needs the Windows process-list snapshot")
    }
}

/// Whether an observed image name is the file `configured` names. Exactly:
///
/// * both sides are reduced to a file name first, so a snapshot reporting a full path and a
///   configuration value pasted as a path both mean the file they name;
/// * case-insensitive and ASCII-only on purpose: Windows' file-name case-insensitivity is
///   ASCII, and a Unicode fold would make a Cyrillic-С `СS2.EXE` match `cs2.exe`, a false
///   positive that starts a recording nobody asked for;
/// * never by substring: `notcs2.exe`, `cs2.exe.bak` and `mycs2.exe` differ from `cs2.exe`.
fn image_name_matches(configured: &str, observed: &str) -> bool {
    let configured = file_name(configured);
    let observed = file_name(observed);
    // An empty file name names no file on either side and matches nothing, not even another empty
    // name: an unreadable snapshot entry must not match an unwritable configuration value.
    !configured.is_empty() && configured.eq_ignore_ascii_case(observed)
}

/// The file-name part of a name that may be a path. A name that is only separators is returned
/// unchanged: there is no file name in it, and returning the empty tail would match nothing or
/// look like a match for an equally empty configuration value.
fn file_name(name: &str) -> &str {
    let name = name.trim();
    match name.rsplit(['\\', '/']).next() {
        Some(file) if !file.is_empty() => file,
        _ => name,
    }
}

/// The process-name watcher: compare a snapshot of image names against the configured list.
///
/// The thin half of detection: the comparison is [`presences_in`]; what remains is one
/// snapshot call, one walk and one handle close, in [`processes`].
///
/// One poll is one `CreateToolhelp32Snapshot` plus a walk of a few hundred entries, copying
/// each image name into a vector dropped before `poll` returns. Nothing about a process is
/// opened, read or held; the only long-lived state is the configured list. Measured at a few
/// hundred microseconds per [`DEFAULT_POLL_INTERVAL`].
#[derive(Debug)]
pub struct ProcessWatcher {
    watched: Vec<WatchedGame>,
}

impl ProcessWatcher {
    /// Watch `watched` by executable name.
    ///
    /// The list is validated here (see [`validate_watch_list`]): a duplicate or nameless entry is a
    /// configuration mistake, and a watcher silently watching nothing is the worst way to learn it.
    pub fn new(watched: Vec<WatchedGame>) -> Result<Self> {
        validate_watch_list(&watched)?;
        Ok(Self { watched })
    }

    /// What this watcher was told to look for.
    pub fn watched(&self) -> &[WatchedGame] {
        &self.watched
    }
}

impl GamePresence for ProcessWatcher {
    fn poll(&mut self) -> Result<Vec<Presence>> {
        #[cfg(windows)]
        {
            let observed: Vec<String> = processes()?.into_iter().map(|(name, _)| name).collect();
            Ok(presences_in(&self.watched, &observed))
        }
        #[cfg(not(windows))]
        {
            // Not an empty list: "we cannot look" differs from "nothing is there", and an empty list would
            // be read as every game stopped, a lie a driver may act on.
            bail!(
                "watching for a game by executable name needs the Windows process-list \
                 snapshot (CreateToolhelp32Snapshot), which this platform does not have; \
                 nothing was observed, and no game is reported as stopped"
            )
        }
    }
}

/// One snapshot of every process's image name and id: the only Windows code in this module.
///
/// `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` is Task Manager's enumeration: the kernel
/// returns a copy of the process list as `PROCESSENTRY32W` records and this copies the image
/// names out. It opens no process (no `OpenProcess`), so no access right is requested over
/// any game, no memory is read, and no module, thread or window is touched. The only handle
/// is the snapshot's own, closed before return. Kept to four calls because this half cannot
/// be tested on the development machine.
#[cfg(windows)]
fn processes() -> Result<Vec<(String, u32)>> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    /// The snapshot handle, closed however the function leaves — including through `?`.
    struct Snapshot(HANDLE);

    impl Drop for Snapshot {
        fn drop(&mut self) {
            // Best effort: a failed close is not actionable.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    let snapshot = Snapshot(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(|e| anyhow!("taking a process-list snapshot: {e}"))?,
    );

    // `dwSize` is the structure's size and checked by the API; a zeroed entry is how the
    // reference example passes it.
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // A first entry that cannot be read is an error, not an empty list: the snapshot was created,
    // so this is a genuine failure to look, and `poll_once` treats failure as "no observation",
    // not "everything stopped".
    unsafe { Process32FirstW(snapshot.0, &mut entry) }
        .map_err(|e| anyhow!("reading the first process of the snapshot: {e}"))?;

    let mut names = Vec::new();
    loop {
        // The name is a NUL-terminated UTF-16 buffer and the terminator is not always the last unit;
        // a buffer with none is taken whole rather than truncated.
        let name = match entry.szExeFile.iter().position(|unit| *unit == 0) {
            Some(len) => String::from_utf16_lossy(&entry.szExeFile[..len]),
            None => String::from_utf16_lossy(&entry.szExeFile),
        };
        names.push((name, entry.th32ProcessID));
        // A failure here is the documented end of the list, how a `Process32*` walk always ends.
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Ok(names)
}

/// Refuse a watch list that cannot mean what it says. Two mistakes get a startup error:
///
/// * an empty `exe` names no file and can never match, so a title the user believes is watched
///   is not;
/// * the same `exe` twice is one game (`exe` is the identity the matcher and tracker key on,
///   case-insensitively), so a repeat is a mistake, not two watches.
pub fn validate_watch_list(watched: &[WatchedGame]) -> Result<()> {
    let mut seen: Vec<&str> = Vec::with_capacity(watched.len());
    for game in watched {
        let exe = file_name(&game.exe);
        if exe.is_empty() {
            bail!("{} has no executable name to watch for", game.name);
        }
        if seen.iter().any(|other| other.eq_ignore_ascii_case(exe)) {
            bail!("{exe} is watched twice");
        }
        seen.push(exe);
    }
    Ok(())
}

/// League of Legends, via the API Riot publishes for it.
///
/// A game-state signal, not "some Riot process exists": the Live Client Data API answers on
/// `127.0.0.1:2999` while a match runs and stops when it ends, so `running: true` means a game
/// is being played.
///
/// It reuses the crate's [`LoopbackClient`], pinned to an [`Endpoint`] that can only be
/// loopback: no new HTTP client, no second place relaxing TLS, no new request path. It asks for
/// the same `/liveclientdata/allgamedata` as the event poller and reads only the answer's
/// status.
///
/// Limit: the endpoint is the game's, so a replay or spectated game served by the same client
/// looks like a match being played. It is still a game the user opened, so not the costly kind
/// of false positive.
pub struct LiveClientPresence {
    client: LoopbackClient,
    watching: Vec<WatchedGame>,
}

impl LiveClientPresence {
    /// The real thing: Riot's endpoint and the titles configured for it. Titles whose signal is
    /// [`Signal::Process`] are left out; [`CompositePresence`] hands each title to one source.
    pub fn new(watched: &[WatchedGame]) -> Result<Self> {
        Self::for_endpoint(watched, Endpoint::live_client())
    }

    /// The same detector against any loopback endpoint, for a test mock or a front-end with its
    /// own endpoint.
    pub fn for_endpoint(watched: &[WatchedGame], endpoint: Endpoint) -> Result<Self> {
        let watching = watched
            .iter()
            .filter(|game| game.signal == Signal::LiveClientApi)
            .cloned()
            .collect();
        Ok(Self {
            client: LoopbackClient::for_endpoint(endpoint)?,
            watching,
        })
    }

    /// The endpoint this detector reads.
    pub fn endpoint(&self) -> &Endpoint {
        self.client.endpoint()
    }

    /// The titles this detector speaks for.
    pub fn watching(&self) -> &[WatchedGame] {
        &self.watching
    }
}

impl GamePresence for LiveClientPresence {
    fn poll(&mut self) -> Result<Vec<Presence>> {
        if self.watching.is_empty() {
            // Nothing to answer for means not even the request: an unused source must not put traffic on
            // loopback every few seconds.
            return Ok(Vec::new());
        }

        let running = match self.client.get() {
            // A body means the API is serving a game; what it says is `lol::derive`'s business.
            Ok(Response::Body(_)) => true,
            // 404: the API is up and reports no game.
            Ok(Response::NotServing) => false,
            // A refused or timed-out loopback connection is the ordinary "no game is running" answer.
            Err(err) if err.is_no_game() => false,
            // A peer that is not the Live Client (not TLS, or not HTTP) is a failure to look; say so
            // rather than invent either answer.
            Err(err) => {
                return Err(anyhow!("the League Live Client could not be read: {err}"));
            }
        };

        Ok(self
            .watching
            .iter()
            .cloned()
            .map(|game| Presence { game, running })
            .collect())
    }
}

/// Both signals behind one [`GamePresence`].
///
/// The watch list is partitioned by [`Signal`], so each title is found by its configured
/// mechanism and nothing is polled twice. The process source exists only where the enumeration
/// does: elsewhere, titles needing it are named once at construction and no failing poll is
/// invented every interval.
pub struct CompositePresence {
    process: Option<ProcessWatcher>,
    live: Option<LiveClientPresence>,
}

impl CompositePresence {
    /// Watch `watched`, each title by the signal it names.
    pub fn new(watched: Vec<WatchedGame>) -> Result<Self> {
        validate_watch_list(&watched)?;
        let (by_process, by_api): (Vec<WatchedGame>, Vec<WatchedGame>) = watched
            .into_iter()
            .partition(|game| game.signal == Signal::Process);

        let live = if by_api.is_empty() {
            None
        } else {
            Some(LiveClientPresence::new(&by_api)?)
        };

        let process = if by_process.is_empty() {
            None
        } else if cfg!(windows) {
            Some(ProcessWatcher::new(by_process)?)
        } else {
            // Once, at startup, where the user can see it: otherwise a title silently unwatched on this
            // platform looks like a game that never starts a recording.
            let names = by_process
                .iter()
                .map(|game| game.exe.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                "watching for a game by executable name needs Windows, so these are not \
                 watched on this platform: {names}"
            );
            None
        };

        tracing::info!(
            "game presence: {} title(s) by executable name, {} by the League Live Client \
             Data API",
            process.as_ref().map_or(0, |w| w.watched().len()),
            live.as_ref().map_or(0, |l| l.watching().len())
        );

        Ok(Self { process, live })
    }

    /// The process-name source, where this platform has one.
    pub fn process(&self) -> Option<&ProcessWatcher> {
        self.process.as_ref()
    }

    /// The Live Client Data API source, when a title asks for it.
    pub fn live_client(&self) -> Option<&LiveClientPresence> {
        self.live.as_ref()
    }
}

impl GamePresence for CompositePresence {
    fn poll(&mut self) -> Result<Vec<Presence>> {
        let mut snapshot = Vec::new();
        if let Some(process) = &mut self.process {
            snapshot.extend(
                process
                    .poll()
                    .map_err(|err| anyhow!("reading the process list: {err}"))?,
            );
        }
        if let Some(live) = &mut self.live {
            snapshot.extend(
                live.poll()
                    .map_err(|err| anyhow!("polling the League Live Client: {err}"))?,
            );
        }
        Ok(snapshot)
    }
}

/// One change in what is running. The directions are variants, not a `bool`, because a start
/// is "arm the capture" and a stop is "the session is over". Neither carries a timestamp: the
/// driver knows when it received it, and a clock here would make the tracker untestable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceChange {
    /// The game is running now.
    Started(WatchedGame),
    /// The game has stopped.
    Stopped(WatchedGame),
}

impl PresenceChange {
    /// The game this change is about.
    pub fn game(&self) -> &WatchedGame {
        match self {
            PresenceChange::Started(game) | PresenceChange::Stopped(game) => game,
        }
    }

    /// Whether this is a start.
    pub fn is_start(&self) -> bool {
        matches!(self, PresenceChange::Started(_))
    }

    /// Whether this is a stop.
    pub fn is_stop(&self) -> bool {
        matches!(self, PresenceChange::Stopped(_))
    }
}

impl fmt::Display for PresenceChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = if self.is_start() {
            "started"
        } else {
            "stopped"
        };
        write!(f, "{} {verb}", self.game())
    }
}

/// One game's place in the tracker.
#[derive(Debug)]
struct Tracked {
    game: WatchedGame,
    /// What is believed about it, or `None` before the first observation.
    believed: Option<bool>,
    /// The state a change is heading for, and how many polls in a row have shown it.
    candidate: bool,
    runs: u32,
}

/// What is believed to be running, and the debounce that keeps a flicker out of it.
///
/// * The first observation of a game is believed immediately: a watcher started mid-session
///   (the normal tray case) says what is running now instead of waiting for a transition that
///   already happened.
/// * A later change is believed after [`CONFIRMATIONS`] consecutive polls (or what
///   [`PresenceTracker::with_confirmations`] asks for), so an alternating snapshot produces no
///   changes rather than one per poll.
/// * A game a snapshot does not mention is not evidence. Only an explicit `running: false`
///   counts as stopped, so a partial snapshot (the Live Client's answer on a platform with no
///   process enumeration) cannot end a session.
/// * A change is reported exactly once: the believed state is compared, so a game staying
///   stopped does not stop again, nor one staying running start again.
///
/// Pure: no clock, I/O or logging, so the half worth testing exhaustively is the half every
/// platform shares.
#[derive(Debug)]
pub struct PresenceTracker {
    confirmations: u32,
    tracked: Vec<Tracked>,
}

impl PresenceTracker {
    /// Track `watched` with the default debounce ([`CONFIRMATIONS`]).
    ///
    /// The list is the identity: an `exe` names a game once, case-insensitively. A repeat is kept
    /// the first time and ignored afterwards, so an unvalidated list still gets one answer per game.
    pub fn new(watched: &[WatchedGame]) -> Self {
        Self::with_confirmations(watched, CONFIRMATIONS)
    }

    /// The same with an explicit debounce. `0` is read as `1`, as a tracker believing nothing would
    /// never report anything.
    pub fn with_confirmations(watched: &[WatchedGame], confirmations: u32) -> Self {
        let mut tracked: Vec<Tracked> = Vec::with_capacity(watched.len());
        for game in watched {
            let duplicate = tracked.iter().any(|other| {
                other
                    .game
                    .file_name()
                    .eq_ignore_ascii_case(game.file_name())
            });
            if duplicate {
                continue;
            }
            tracked.push(Tracked {
                game: game.clone(),
                believed: None,
                candidate: false,
                runs: 0,
            });
        }
        Self {
            confirmations: confirmations.max(1),
            tracked,
        }
    }

    /// Fold one poll in, and answer what changed.
    pub fn observe(&mut self, snapshot: &[Presence]) -> Vec<PresenceChange> {
        let mut changes = Vec::new();
        for tracked in &mut self.tracked {
            let Some(running) = snapshot
                .iter()
                .find(|presence| {
                    presence
                        .game
                        .file_name()
                        .eq_ignore_ascii_case(tracked.game.file_name())
                })
                .map(|presence| presence.running)
            else {
                // Not mentioned: no observation, so nothing to believe and nothing to say.
                continue;
            };

            match tracked.believed {
                None => {
                    tracked.believed = Some(running);
                    tracked.candidate = running;
                    tracked.runs = 1;
                    if running {
                        changes.push(PresenceChange::Started(tracked.game.clone()));
                    }
                }
                Some(believed) if believed == running => {
                    // The steady state: silent, and no run to count.
                    tracked.candidate = running;
                    tracked.runs = 0;
                }
                Some(_) => {
                    if tracked.candidate == running {
                        tracked.runs += 1;
                    } else {
                        tracked.candidate = running;
                        tracked.runs = 1;
                    }
                    if tracked.runs >= self.confirmations {
                        tracked.believed = Some(running);
                        changes.push(if running {
                            PresenceChange::Started(tracked.game.clone())
                        } else {
                            PresenceChange::Stopped(tracked.game.clone())
                        });
                    }
                }
            }
        }
        changes
    }

    /// The games believed to be running, as of the last observation.
    pub fn running(&self) -> Vec<&WatchedGame> {
        self.tracked
            .iter()
            .filter(|tracked| tracked.believed == Some(true))
            .map(|tracked| &tracked.game)
            .collect()
    }

    /// Whether `exe` is believed to be running.
    pub fn is_running(&self, exe: &str) -> bool {
        self.tracked.iter().any(|tracked| {
            tracked.believed == Some(true)
                && tracked
                    .game
                    .file_name()
                    .eq_ignore_ascii_case(file_name(exe))
        })
    }

    /// How many games this tracker follows.
    pub fn watched(&self) -> usize {
        self.tracked.len()
    }

    /// The debounce this tracker was built with.
    pub fn confirmations(&self) -> u32 {
        self.confirmations
    }
}

/// The poll loop: ask the detector, fold the answer through the tracker, report the changes.
/// Split from the thread, like `lol::LolPoller::poll_once`, so "what does this sequence of
/// polls mean" can be driven with no clock or syscalls.
pub struct GameWatcher {
    detector: Box<dyn GamePresence>,
    tracker: PresenceTracker,
    interval: Duration,
    quiet: RunQuiet,
}

impl GameWatcher {
    /// Watch `games` with both signals, for real. The interval is bounded by [`clamp_interval`]
    /// here, so no configuration value can turn the watcher into a tight loop over the process list.
    pub fn new(games: Vec<WatchedGame>, interval: Duration) -> Result<Self> {
        let detector = CompositePresence::new(games.clone())?;
        Ok(Self::with_detector(
            Box::new(detector),
            &games,
            clamp_interval(interval),
        ))
    }

    /// The same loop around any [`GamePresence`], for tests and front-ends with a source of their
    /// own. The interval is used as given: the deliberate seam, so a test may poll ten times a
    /// second.
    pub fn with_detector(
        detector: Box<dyn GamePresence>,
        watched: &[WatchedGame],
        interval: Duration,
    ) -> Self {
        Self {
            detector,
            tracker: PresenceTracker::new(watched),
            interval,
            quiet: RunQuiet::default(),
        }
    }

    /// How often this watcher polls.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The games believed to be running as of the last poll.
    pub fn running(&self) -> Vec<&WatchedGame> {
        self.tracker.running()
    }

    /// Poll once and answer what changed.
    ///
    /// Never fails. A detector that could not answer is a log line (the first of a run, at `warn`)
    /// and not a transition: a failed snapshot is not a game that stopped, and ending a recording
    /// over one is the mistake this method prevents.
    pub fn poll_once(&mut self) -> Vec<PresenceChange> {
        let snapshot = match self.detector.poll() {
            Ok(snapshot) => {
                if let Some(condition) = self.quiet.recovered() {
                    tracing::info!("game detection is answering again ({condition})");
                }
                snapshot
            }
            Err(err) => {
                if self.quiet.note(format!("{err:#}")) {
                    tracing::warn!(
                        "could not look for a game ({err:#}); keeping the state the last poll \
                         found"
                    );
                }
                return Vec::new();
            }
        };

        let changes = self.tracker.observe(&snapshot);
        for change in &changes {
            match change {
                PresenceChange::Started(game) => tracing::info!("{game} is running"),
                PresenceChange::Stopped(game) => tracing::info!("{game} has stopped"),
            }
        }
        changes
    }
}

/// Where a watcher sends what changed. Unbounded for the reason [`crate::EventSink`] is: a
/// handful of changes an hour, and a full queue must never drop one.
pub type PresenceSink = Sender<PresenceChange>;

/// A running watcher, on its own thread. Dropping it stops the thread and waits for it, so a
/// stopped application leaves no thread behind. A caller must keep the handle for as long as it
/// wants to hear about games, which the attribute below states.
#[must_use = "dropping the watch handle stops the watcher"]
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchHandle {
    /// Stop watching and wait for the thread. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // A poll in flight finishes (bounded by its own syscall); the join makes "stopped" real.
            let _ = thread.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start watching on a thread of its own.
///
/// Every change goes to `sink` until the handle is dropped. A sink the driver dropped ends the
/// loop: nobody is left to tell.
///
/// The interval is a floor, not a wait: [`sleep_until`] sleeps [`STOP_SLICE`] at a time so
/// `stop` need not wait out an interval, and a poll that overran starts the next straight away.
/// It never spins.
pub fn spawn(watcher: GameWatcher, sink: PresenceSink) -> Result<WatchHandle> {
    let interval = watcher.interval();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let mut watcher = watcher;

    tracing::info!(
        "watching for a game to start, every {} ms (a change is believed after {} polls)",
        interval.as_millis(),
        watcher.tracker.confirmations()
    );

    let thread = std::thread::Builder::new()
        .name("replayed-games".to_string())
        .spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let started = Instant::now();
                for change in watcher.poll_once() {
                    if sink.send(change).is_err() {
                        tracing::debug!("the game watcher stopped: nothing is receiving changes");
                        return;
                    }
                }
                sleep_until(started + interval, &flag);
            }
            tracing::debug!("the game watcher stopped");
        })
        .map_err(|e| anyhow!("spawning the game watcher thread: {e}"))?;

    Ok(WatchHandle {
        stop,
        thread: Some(thread),
    })
}

/// Sleep until `deadline`, waking early — within [`STOP_SLICE`] — when asked to stop.
fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(STOP_SLICE));
    }
}

/// Bound a configured interval into [`MIN_POLL_INTERVAL`]..=[`MAX_POLL_INTERVAL`]. The one
/// place configuration becomes an interval, so the floor protecting the user's machine cannot
/// be bypassed by a field read elsewhere.
pub fn clamp_interval(requested: Duration) -> Duration {
    requested.clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL)
}

/// Keeps a repeating condition from writing a line every poll, with `lol::poller`'s
/// discipline: the first of a run and a changed condition are news, repetitions are not.
#[derive(Debug, Default)]
struct RunQuiet {
    active: Option<String>,
}

impl RunQuiet {
    /// Note a failure. Whether this one is worth a line.
    fn note(&mut self, condition: String) -> bool {
        match &self.active {
            Some(active) if *active == condition => false,
            _ => {
                self.active = Some(condition);
                true
            }
        }
    }

    /// The answer came back: end whatever was being reported, once.
    fn recovered(&mut self) -> Option<String> {
        self.active.take()
    }
}

/// The `[games]` section of `config.toml`: what to watch, and whether to watch. The types live
/// here because the polling crate owns the names it accepts; the configuration layer
/// deserialises this struct.
///
/// ```toml
/// [games]
/// auto_record = false
/// poll_ms = 2500
///
/// [[games.watch]]
/// name = "League of Legends"
/// exe = "League of Legends.exe"
/// signal = "live_client_api"
/// ```
///
/// `auto_record` defaults to `false`, the whole safety posture of the feature. With it off,
/// [`GamesSection::start`] starts nothing: no process list, no snapshot, no Live Client
/// request. A machine that never turns it on has nothing watched on its behalf, the right
/// default next to kernel-level anti-cheat.
///
/// `poll_ms` is clamped by [`GamesSection::interval`]; `0` becomes `250`, not a tight loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GamesSection {
    /// Whether a watched game starting is on its own a reason to start recording.
    pub auto_record: bool,
    /// How often to look, in milliseconds. Read through [`GamesSection::interval`], which bounds it.
    pub poll_ms: u64,
    /// The titles to recognise.
    pub watch: Vec<WatchedGame>,
}

impl Default for GamesSection {
    /// Nothing automatic, a few seconds apart, and the six titles of [`default_watch`].
    fn default() -> Self {
        Self {
            auto_record: false,
            poll_ms: DEFAULT_POLL_MS,
            watch: default_watch(),
        }
    }
}

impl GamesSection {
    /// The poll interval this section asks for, bounded into
    /// [`MIN_POLL_INTERVAL`]..=[`MAX_POLL_INTERVAL`]. Read it through this, not `poll_ms`: the
    /// clamp stops a `0` in a config file becoming a watcher that enumerates processes as fast as
    /// the CPU allows.
    pub fn interval(&self) -> Duration {
        clamp_interval(Duration::from_millis(self.poll_ms))
    }

    /// Whether this section asks for anything to be watched at all.
    pub fn watching(&self) -> bool {
        self.auto_record
    }

    /// Start the watcher this section describes, or `None` when `auto_record` is off (the
    /// default). The `None` makes the opt-in real: with the default section nothing is polled.
    ///
    /// Changes arrive on `sink`; drop the receiver to end the watcher, or drop (or
    /// [`WatchHandle::stop`]) the handle. A caller wanting changes must keep the handle alive.
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use replayed_events::process::{default_watch, GamesSection};
    /// use std::sync::mpsc::channel;
    ///
    /// let section = GamesSection::default(); // auto_record = false: nothing is watched
    /// let (sink, changes) = channel();
    /// let _watcher = section.start(sink)?;
    /// while let Ok(change) = changes.recv() {
    ///     println!("{change}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn start(&self, sink: PresenceSink) -> Result<Option<WatchHandle>> {
        if !self.auto_record {
            tracing::debug!("games.auto_record is off, so no game is being watched for");
            return Ok(None);
        }
        let watcher = GameWatcher::new(self.watch.clone(), self.interval())?;
        spawn(watcher, sink).map(Some)
    }
}

/// The titles a fresh configuration watches, and how each is recognised.
///
/// Only League is recognised by Riot's Live Client Data API, the only title here publishing
/// one. Everything else is recognised by its game executable (present only while the game runs,
/// never the launcher's), so a process-name match does not fire all evening on a client left
/// open:
///
/// | title | executable | signal |
/// |---|---|---|
/// | League of Legends | `League of Legends.exe` (not `LeagueClient.exe`) | Live Client Data API |
/// | Counter-Strike 2 | `cs2.exe` (not `steam.exe`) | process name |
/// | Dota 2 | `dota2.exe` | process name |
/// | Rainbow Six Siege | `RainbowSix.exe` | process name |
/// | Valorant | `VALORANT-Win64-Shipping.exe` (not `VALORANT.exe`) | process name |
/// | Overwatch 2 | `Overwatch.exe` | process name |
///
/// Valorant is a Riot title with no published local game-state API, so it is watched by the
/// game process, which exists only while a match runs.
pub fn default_watch() -> Vec<WatchedGame> {
    vec![
        WatchedGame::by_live_client_api("League of Legends", "League of Legends.exe"),
        WatchedGame::by_process("Counter-Strike 2", "cs2.exe"),
        WatchedGame::by_process("Dota 2", "dota2.exe"),
        WatchedGame::by_process("Rainbow Six Siege", "RainbowSix.exe"),
        WatchedGame::by_process("Valorant", "VALORANT-Win64-Shipping.exe"),
        WatchedGame::by_process("Overwatch 2", "Overwatch.exe"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    // ---- the pure decision -----------------------------------------------------------

    fn observed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn running_of(presences: &[Presence], exe: &str) -> Option<bool> {
        presences
            .iter()
            .find(|presence| presence.game.exe.eq_ignore_ascii_case(exe))
            .map(|presence| presence.running)
    }

    #[test]
    fn the_pid_is_the_first_watched_title_that_is_running() {
        let watch = default_watch();
        let running = |list: &[(&str, u32)]| -> Vec<(String, u32)> {
            list.iter()
                .map(|&(name, pid)| (name.to_string(), pid))
                .collect()
        };
        assert_eq!(
            pid_in(&watch, &running(&[("explorer.exe", 4), ("CS2.EXE", 77)])),
            Some(77)
        );
        // Watch-list order decides, not snapshot order: League is listed before cs2.
        assert_eq!(
            pid_in(
                &watch,
                &running(&[("cs2.exe", 77), ("League of Legends.exe", 12)])
            ),
            Some(12)
        );
        assert_eq!(pid_in(&watch, &running(&[("notcs2.exe", 5)])), None);
    }

    #[test]
    fn the_default_watch_list_is_the_titles_the_project_names() {
        let watch = default_watch();
        let named: Vec<(&str, &str, Signal)> = watch
            .iter()
            .map(|game| (game.name.as_str(), game.exe.as_str(), game.signal))
            .collect();
        assert_eq!(named.len(), 6, "the six titles the project names");
        assert_eq!(
            named[0],
            (
                "League of Legends",
                "League of Legends.exe",
                Signal::LiveClientApi
            )
        );
        assert_eq!(
            watch
                .iter()
                .filter(|g| g.signal == Signal::LiveClientApi)
                .count(),
            1,
            "Riot publishes the Live Client Data API for one title on this list, not for all"
        );
        assert!(
            watch.iter().all(|g| g.exe.ends_with(".exe")),
            "every title is watched by an executable's file name"
        );
    }

    #[test]
    fn a_name_that_merely_contains_the_executable_is_not_a_match() {
        // The false positive that would start a recording nobody asked for.
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        for name in [
            "notcs2.exe",  // a superset of the name
            "cs2.exe.bak", // not the same file
            "mycs2.exe",   // a different file that happens to end the same way
            "cs2",         // the same stem without the extension
            "xc s2.exe",   // a space where a character is
            "cs2.exe ",    // trailing space, trimmed away — matches, see below
        ] {
            let matches = running_of(&presences_in(&watch, &observed(&[name])), "cs2.exe");
            if name == "cs2.exe " {
                assert_eq!(
                    matches,
                    Some(true),
                    "{name:?} is the same file name once trimmed"
                );
            } else {
                assert_eq!(matches, Some(false), "{name:?} must not match cs2.exe");
            }
        }
    }

    #[test]
    fn a_path_bearing_name_is_reduced_to_its_file_name() {
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        for name in [
            r"C:\Program Files (x86)\Steam\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe",
            r"\\?\C:\games\cs2.exe",
            "C:/games/cs2.exe",
            "/Applications/cs2.exe",
            r"cs2.exe",
            "CS2.EXE", // matching is case-insensitive
        ] {
            assert_eq!(
                running_of(&presences_in(&watch, &observed(&[name])), "cs2.exe"),
                Some(true),
                "{name:?} names the file cs2.exe"
            );
        }
    }

    #[test]
    fn a_unicode_lookalike_is_not_a_match() {
        // Cyrillic С (U+0421), not Latin C. Windows' file-name case-insensitivity is ASCII; a Unicode
        // fold would call these equal.
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        assert_eq!(
            running_of(
                &presences_in(&watch, &observed(&["\u{421}S2.EXE"])),
                "cs2.exe"
            ),
            Some(false),
            "a lookalike letter is a different file name"
        );
    }

    #[test]
    fn a_configured_path_is_reduced_to_its_file_name() {
        let watch = vec![WatchedGame::by_process(
            "Counter-Strike 2",
            r"C:\Games\cs2.exe",
        )];
        assert_eq!(watch[0].file_name(), "cs2.exe");
        let presences = presences_in(&watch, &observed(&["cs2.exe"]));
        assert_eq!(running_of(&presences, r"C:\Games\cs2.exe"), Some(true));
    }

    #[test]
    fn every_watched_game_gets_an_answer_even_when_nothing_is_present() {
        let watch = default_watch();
        let presences = presences_in(&watch, &observed(&[]));
        assert_eq!(presences.len(), watch.len(), "one answer per watched title");
        assert!(presences.iter().all(|presence| !presence.running));
        assert!(
            presences_in(&[], &observed(&["cs2.exe"])).is_empty(),
            "an empty watch list answers nothing, and does not panic"
        );
    }

    #[test]
    fn two_different_games_can_be_present_at_once() {
        let watch = default_watch();
        let presences = presences_in(
            &watch,
            &observed(&[
                "explorer.exe",
                "cs2.exe",
                "VALORANT-Win64-Shipping.exe",
                "cs2.exe",
            ]),
        );
        assert_eq!(running_of(&presences, "cs2.exe"), Some(true));
        assert_eq!(
            running_of(&presences, "VALORANT-Win64-Shipping.exe"),
            Some(true)
        );
        assert_eq!(running_of(&presences, "dota2.exe"), Some(false));
        assert_eq!(running_of(&presences, "Overwatch.exe"), Some(false));
        assert_eq!(running_of(&presences, "League of Legends.exe"), Some(false));
        assert_eq!(
            presences.len(),
            watch.len(),
            "a duplicate in the snapshot is still one answer"
        );
    }

    #[test]
    fn a_configuration_value_that_is_only_a_path_separator_matches_nothing() {
        // `exe` cannot be empty or separators-only in a validated list, and the matcher must not turn
        // a separator into an empty file name equal to another empty one.
        assert_eq!(file_name(r"\\"), r"\\");
        assert!(!image_name_matches("", ""));
        assert!(!image_name_matches("   ", ""));
        assert!(!image_name_matches("cs2.exe", ""));
    }

    // ---- the tracker -----------------------------------------------------------------

    /// A snapshot in which exactly the named games are running, every title answered.
    fn poll_of(watched: &[WatchedGame], running: &[&str]) -> Vec<Presence> {
        watched
            .iter()
            .map(|game| Presence {
                game: game.clone(),
                running: running
                    .iter()
                    .any(|exe| game.file_name().eq_ignore_ascii_case(file_name(exe))),
            })
            .collect()
    }

    #[test]
    fn a_game_already_running_is_running_by_the_first_poll() {
        // The tray usually starts with the client already open: the first answer must be the truth
        // about now, not a wait for a transition that already happened.
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        let changes = tracker.observe(&poll_of(&watch, &["cs2.exe"]));
        assert_eq!(changes, vec![PresenceChange::Started(watch[1].clone())]);
        assert_eq!(tracker.running().len(), 1);
        assert!(
            tracker.is_running("CS2.EXE"),
            "and the answer is the believed state"
        );
    }

    #[test]
    fn a_game_that_is_not_running_says_nothing_at_all() {
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        for _ in 0..5 {
            assert!(
                tracker.observe(&poll_of(&watch, &[])).is_empty(),
                "a game that was never running is not a game that stopped"
            );
        }
        assert!(tracker.running().is_empty());
        assert!(!tracker.is_running("cs2.exe"));
    }

    #[test]
    fn a_start_is_believed_after_two_polls_and_a_stop_is_reported_exactly_once() {
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        assert_eq!(tracker.confirmations(), CONFIRMATIONS);

        // Nothing running, then a start: believed on the second agreeing poll.
        assert!(tracker.observe(&poll_of(&watch, &[])).is_empty());
        assert!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])).is_empty(),
            "one poll is not enough"
        );
        assert_eq!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])),
            vec![PresenceChange::Started(watch[1].clone())]
        );

        // Running and staying running is silent, however long it lasts.
        for _ in 0..5 {
            assert!(tracker.observe(&poll_of(&watch, &["cs2.exe"])).is_empty());
        }

        // And the stop is reported once, never twice.
        assert!(
            tracker.observe(&poll_of(&watch, &[])).is_empty(),
            "one poll is not enough to stop"
        );
        assert_eq!(
            tracker.observe(&poll_of(&watch, &[])),
            vec![PresenceChange::Stopped(watch[1].clone())]
        );
        for _ in 0..5 {
            assert!(
                tracker.observe(&poll_of(&watch, &[])).is_empty(),
                "a stop is reported once"
            );
        }
        assert!(tracker.running().is_empty());
    }

    #[test]
    fn a_flapping_snapshot_produces_no_event_storm() {
        // A process list alternating between "there" and "not there" (a worker restart, a snapshot
        // mid-exit) must not become one change, and so one recording, per poll.
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        assert!(tracker.observe(&poll_of(&watch, &[])).is_empty());

        let mut changes = 0;
        for poll in 0..40 {
            let snapshot = if poll % 2 == 0 {
                &["cs2.exe"][..]
            } else {
                &[][..]
            };
            changes += tracker.observe(&poll_of(&watch, snapshot)).len();
        }
        assert_eq!(
            changes, 0,
            "an alternating snapshot believes nothing and reports nothing"
        );
        assert!(tracker.running().is_empty());
    }

    #[test]
    fn two_games_running_at_once_are_representable_and_one_stopping_leaves_the_other() {
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        let changes = tracker.observe(&poll_of(&watch, &["cs2.exe", "dota2.exe"]));
        assert_eq!(
            changes,
            vec![
                PresenceChange::Started(watch[1].clone()),
                PresenceChange::Started(watch[2].clone()),
            ],
            "both answers arrive in one poll, in the order they are watched"
        );
        assert_eq!(tracker.running().len(), 2);

        // Counter-Strike stops; Dota 2 does not, and is not disturbed by it.
        let cs2_stopped = poll_of(&watch, &["dota2.exe"]);
        assert!(
            tracker.observe(&cs2_stopped).is_empty(),
            "one poll is not enough to stop"
        );
        assert_eq!(
            tracker.observe(&cs2_stopped),
            vec![PresenceChange::Stopped(watch[1].clone())]
        );
        assert_eq!(tracker.running(), vec![&watch[2]]);
    }

    #[test]
    fn a_snapshot_that_does_not_mention_a_game_leaves_it_alone() {
        // A partial snapshot: the Live Client answers on a platform with no process enumeration.
        // Silence about a game is not evidence about a game.
        let watch = default_watch();
        let mut tracker = PresenceTracker::new(&watch);
        assert_eq!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])),
            vec![PresenceChange::Started(watch[1].clone())]
        );

        let only_live: Vec<Presence> = poll_of(&watch, &[])
            .into_iter()
            .filter(|presence| presence.game.signal == Signal::LiveClientApi)
            .collect();
        for _ in 0..5 {
            assert!(
                tracker.observe(&only_live).is_empty(),
                "a game the snapshot does not mention is not a game that stopped"
            );
        }
        assert!(tracker.is_running("cs2.exe"));
    }

    #[test]
    fn the_tracker_follows_each_executable_once() {
        let one = WatchedGame::by_process("Counter-Strike 2", "cs2.exe");
        let repeat = WatchedGame::by_process("Counter-Strike 2, again", "CS2.EXE");
        let tracker = PresenceTracker::new(&[one, repeat.clone(), repeat.clone()]);
        assert_eq!(tracker.watched(), 1, "an exe names one game");

        let mut tracker = PresenceTracker::with_confirmations(std::slice::from_ref(&repeat), 0);
        assert_eq!(
            tracker.confirmations(),
            1,
            "a debounce of zero would believe nothing"
        );
        let watch = vec![WatchedGame::by_process("Counter-Strike 2", "cs2.exe")];
        assert_eq!(
            tracker.observe(&poll_of(&watch, &["cs2.exe"])),
            vec![PresenceChange::Started(repeat)],
            "one confirmation believes the first answer that differs, and the change names \
             the game the tracker knows"
        );
    }

    #[test]
    fn a_change_of_game_is_not_a_change_of_state() {
        // The tracker keys on the executable, as the matcher does, so a renamed display name is the
        // same game, not a second start.
        let original = WatchedGame::by_process("Counter-Strike 2", "cs2.exe");
        let mut tracker = PresenceTracker::new(std::slice::from_ref(&original));
        let renamed = WatchedGame::by_process("CS2", "cs2.exe");
        let snapshot = vec![Presence {
            game: renamed,
            running: true,
        }];
        assert_eq!(
            tracker.observe(&snapshot),
            vec![PresenceChange::Started(original)],
            "one start for the first observation, and the name it is reported under is the \
             one the tracker was built with"
        );
        assert!(tracker.observe(&snapshot).is_empty());
    }

    // ---- the loop, driven by a script -------------------------------------------------

    /// A detector that answers with the sequence a test wrote, in order.
    struct Scripted {
        answers: VecDeque<Result<Vec<Presence>>>,
    }

    impl Scripted {
        /// Boxed, which is why this is not called `new`: the loop wants a trait object.
        fn boxed(answers: Vec<Result<Vec<Presence>>>) -> Box<dyn GamePresence> {
            Box::new(Self {
                answers: answers.into(),
            })
        }
    }

    impl GamePresence for Scripted {
        fn poll(&mut self) -> Result<Vec<Presence>> {
            // Running out of script is a test failure; saying so loudly beats a detector quietly answering
            // "nothing" for the rest of the run.
            self.answers
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("the script has no answer left")))
        }
    }

    #[test]
    fn a_detector_that_cannot_answer_does_not_end_a_session() {
        let watch = default_watch();
        let running = poll_of(&watch, &["cs2.exe"]);
        let detector = Scripted::boxed(vec![
            Ok(running.clone()),
            Err(anyhow!("the process-list snapshot failed")),
            Err(anyhow!("the process-list snapshot failed")),
            Err(anyhow!("the process-list snapshot failed")),
            Ok(running.clone()),
        ]);
        let mut watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(10));

        // The first observation is believed straight away: the game is running.
        assert_eq!(
            watcher.poll_once(),
            vec![PresenceChange::Started(watch[1].clone())]
        );
        // Three failed polls: no change, and the state the last good poll found survives.
        for _ in 0..3 {
            assert!(
                watcher.poll_once().is_empty(),
                "a failed poll is not a stop"
            );
            assert_eq!(watcher.running().len(), 1);
        }
        // And the recovery is a return to the steady state, not a second start.
        assert!(watcher.poll_once().is_empty());
        assert_eq!(watcher.running().len(), 1);
    }

    #[test]
    fn the_watcher_drives_the_detector_once_per_poll() {
        let watch = default_watch();
        let detector = Scripted::boxed(vec![Ok(poll_of(&watch, &[])), Ok(poll_of(&watch, &[]))]);
        let mut watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(10));
        assert!(watcher.poll_once().is_empty());
        assert!(watcher.poll_once().is_empty());
        // The third call finds the script empty: an error, and still no change.
        assert!(watcher.poll_once().is_empty());
    }

    #[test]
    fn a_repeating_condition_is_reported_once() {
        let mut quiet = RunQuiet::default();
        assert!(
            quiet.note("the snapshot failed".into()),
            "the first is news"
        );
        for _ in 0..10 {
            assert!(
                !quiet.note("the snapshot failed".into()),
                "and the rest are not"
            );
        }
        assert!(
            quiet.note("something else failed".into()),
            "a change is news again"
        );
        assert_eq!(quiet.recovered(), Some("something else failed".into()));
        assert_eq!(quiet.recovered(), None, "a recovery is reported once");
        assert!(
            quiet.note("the snapshot failed".into()),
            "and it can be reported again later"
        );
    }

    // ---- the live client source -------------------------------------------------------

    #[test]
    fn the_league_source_keeps_only_the_titles_that_publish_an_api() {
        let source = LiveClientPresence::new(&default_watch()).expect("a client for the endpoint");
        assert_eq!(source.watching().len(), 1);
        assert_eq!(source.watching()[0].name, "League of Legends");
        assert_eq!(source.endpoint().addr().port(), 2999);
        assert!(
            source.endpoint().is_loopback(),
            "the address is loopback by construction"
        );
    }

    #[test]
    fn a_refused_loopback_connection_is_no_game_and_not_a_failure() {
        // Port 1 on loopback: nothing listens, as between games. The answer must be `false`, never an
        // error or panic.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().expect("a socket address");
        let endpoint =
            Endpoint::loopback(addr, "/liveclientdata/allgamedata").expect("a loopback endpoint");
        let watch = default_watch();
        let mut source =
            LiveClientPresence::for_endpoint(&watch, endpoint).expect("a client for a mock");

        let presences = source
            .poll()
            .expect("a refused loopback connection is 'no game'");
        assert_eq!(
            presences.len(),
            1,
            "one answer per title the source speaks for"
        );
        assert!(!presences[0].running);
        assert_eq!(presences[0].game.signal, Signal::LiveClientApi);
    }

    #[test]
    fn a_source_with_nothing_to_answer_for_does_not_even_ask() {
        // No title uses the API, so the endpoint is never contacted, which makes an unused source
        // free. Port 1 would refuse if asked.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().expect("a socket address");
        let endpoint =
            Endpoint::loopback(addr, "/liveclientdata/allgamedata").expect("a loopback endpoint");
        let only_process: Vec<WatchedGame> = default_watch()
            .into_iter()
            .filter(|game| game.signal == Signal::Process)
            .collect();
        let mut source =
            LiveClientPresence::for_endpoint(&only_process, endpoint).expect("a client");
        assert!(source.watching().is_empty());
        assert_eq!(source.poll().expect("nothing to answer"), Vec::new());
    }

    // ---- the composite -----------------------------------------------------------------

    #[test]
    fn the_composite_partitions_the_watch_list_by_signal() {
        let composite = CompositePresence::new(default_watch()).expect("a validated list");
        let live = composite.live_client().expect("League publishes an API");
        assert_eq!(live.watching().len(), 1);

        if cfg!(windows) {
            let process = composite
                .process()
                .expect("Windows has the process-list snapshot");
            assert_eq!(
                process.watched().len(),
                5,
                "everything else is watched by name"
            );
            assert!(process
                .watched()
                .iter()
                .all(|g| g.signal == Signal::Process));
        } else {
            assert!(
                composite.process().is_none(),
                "without the enumeration there is no process source to fail every interval"
            );
        }
    }

    #[test]
    fn the_composite_answers_for_every_title_it_can() {
        let watch = default_watch();
        let mut composite = CompositePresence::new(watch.clone()).expect("a validated list");
        let presences = composite.poll().expect("a poll answers");
        assert_eq!(
            presences.len(),
            if cfg!(windows) { watch.len() } else { 1 },
            "every title on Windows; only the Live Client's answer where the enumeration is absent"
        );
        assert!(presences.iter().all(|presence| !presence.running));
    }

    // ---- configuration ----------------------------------------------------------------

    #[test]
    fn a_configuration_file_without_the_games_section_gets_the_default_one() {
        // The shape the configuration layer parses: the section is optional, so a file predating the
        // feature, or one with the section removed, still gets a well-formed one with auto-record off.
        #[derive(Deserialize)]
        struct ConfigFile {
            #[serde(default)]
            games: GamesSection,
        }

        let parsed: ConfigFile = serde_json::from_value(json!({})).expect("an empty file parses");
        assert!(
            !parsed.games.auto_record,
            "nothing is automatic until the user turns it on"
        );
        assert!(!parsed.games.watching());
        assert_eq!(parsed.games.watch, default_watch());
        assert_eq!(parsed.games.poll_ms, DEFAULT_POLL_MS);
        assert_eq!(parsed.games.interval(), DEFAULT_POLL_INTERVAL);
        assert_eq!(
            DEFAULT_POLL_INTERVAL,
            Duration::from_millis(DEFAULT_POLL_MS)
        );

        // And a section that names only some fields keeps the defaults for the rest.
        let parsed: ConfigFile = serde_json::from_value(json!({ "games": { "poll_ms": 4000 } }))
            .expect("a partial section parses");
        assert!(!parsed.games.auto_record);
        assert_eq!(parsed.games.interval(), Duration::from_millis(4000));
        assert_eq!(parsed.games.watch, default_watch());
    }

    #[test]
    fn the_default_watch_list_round_trips_through_serde() {
        let section = GamesSection {
            auto_record: true,
            poll_ms: 1_000,
            watch: default_watch(),
        };
        let text = serde_json::to_string(&section).expect("a section serialises");
        let back: GamesSection = serde_json::from_str(&text).expect("and parses back");
        assert_eq!(back, section);
        assert!(
            text.contains("\"signal\":\"live_client_api\""),
            "the signal spelling is part of the format: {text}"
        );
        assert!(text.contains("\"signal\":\"process\""));
    }

    #[test]
    fn a_watch_entry_is_written_the_way_the_configuration_documents() {
        let parsed: WatchedGame = serde_json::from_value(json!({
            "name": "League of Legends",
            "exe": "League of Legends.exe",
            "signal": "live_client_api",
        }))
        .expect("an entry parses");
        assert_eq!(
            parsed,
            WatchedGame::by_live_client_api("League of Legends", "League of Legends.exe")
        );

        // A signal the user did not write is the general mechanism.
        let parsed: WatchedGame = serde_json::from_value(json!({
            "name": "Dota 2",
            "exe": "dota2.exe",
        }))
        .expect("an entry without a signal parses");
        assert_eq!(parsed.signal, Signal::Process);

        // And a spelling this build does not know is an error, loudly, not a guess.
        let unknown = serde_json::from_value::<WatchedGame>(json!({
            "name": "Mystery",
            "exe": "mystery.exe",
            "signal": "mindreading",
        }));
        assert!(
            unknown.is_err(),
            "an unknown signal must not be silently accepted"
        );
    }

    #[test]
    fn a_configured_poll_interval_cannot_become_a_busy_loop() {
        for (poll_ms, expected) in [
            (0, MIN_POLL_INTERVAL),
            (1, MIN_POLL_INTERVAL),
            (249, MIN_POLL_INTERVAL),
            (250, MIN_POLL_INTERVAL),
            (2_500, DEFAULT_POLL_INTERVAL),
            (30_000, MAX_POLL_INTERVAL),
            (60_000, MAX_POLL_INTERVAL),
            (u64::MAX, MAX_POLL_INTERVAL),
        ] {
            let section = GamesSection {
                poll_ms,
                ..GamesSection::default()
            };
            assert_eq!(section.interval(), expected, "poll_ms = {poll_ms}");
        }
        assert_eq!(clamp_interval(Duration::ZERO), MIN_POLL_INTERVAL);
        assert_eq!(
            clamp_interval(Duration::from_secs(3_600)),
            MAX_POLL_INTERVAL
        );

        // The production constructor is behind the same floor, so no path into the watcher can
        // produce a tight loop over the process list.
        let watcher = GameWatcher::new(default_watch(), Duration::ZERO).expect("a watcher");
        assert_eq!(watcher.interval(), MIN_POLL_INTERVAL);
        assert_eq!(
            GameWatcher::new(default_watch(), Duration::from_secs(3_600))
                .expect("a watcher")
                .interval(),
            MAX_POLL_INTERVAL
        );
    }

    #[test]
    fn a_watch_list_that_cannot_mean_what_it_says_is_refused() {
        let nameless = vec![WatchedGame::by_process("Mystery", "   ")];
        let err = ProcessWatcher::new(nameless.clone()).expect_err("no file name to watch for");
        assert!(
            err.to_string().contains("Mystery"),
            "the error names the title: {err}"
        );
        assert!(
            ProcessWatcher::new(Vec::new()).is_ok(),
            "an empty list is empty, not wrong"
        );

        let duplicate = vec![
            WatchedGame::by_process("Counter-Strike 2", "cs2.exe"),
            WatchedGame::by_process("Counter-Strike 2 as written by hand", "CS2.exe"),
        ];
        let err = ProcessWatcher::new(duplicate.clone()).expect_err("the same file twice");
        let message = err.to_string();
        assert!(
            message.to_lowercase().contains("cs2.exe") && message.contains("watched twice"),
            "the error names the file and the mistake: {message}"
        );
        assert!(
            CompositePresence::new(duplicate).is_err(),
            "the composite validates the list it is given too"
        );
        assert!(ProcessWatcher::new(default_watch()).is_ok());
    }

    // ---- the background thread ---------------------------------------------------------

    #[test]
    fn the_watcher_thread_reports_a_change_and_releases_the_sink_when_stopped() {
        let watch = default_watch();
        let detector = Scripted::boxed(vec![Ok(poll_of(&watch, &["dota2.exe"]))]);
        let watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(5));
        let (sink, changes) = std::sync::mpsc::channel();

        let handle = spawn(watcher, sink).expect("a watcher thread");
        let first = changes
            .recv_timeout(Duration::from_secs(5))
            .expect("the start arrives promptly");
        assert_eq!(first, PresenceChange::Started(watch[2].clone()));
        assert_eq!(first.game().name, "Dota 2");
        assert!(first.is_start() && !first.is_stop());

        // Stopping joins the thread, dropping its end of the channel: proof nothing polls after `stop`.
        handle.stop();
        assert!(
            changes.recv().is_err(),
            "the sink is closed once the watcher has stopped"
        );
    }

    #[test]
    fn a_watcher_whose_receiver_is_dropped_ends_by_itself() {
        let watch = default_watch();
        let detector = Scripted::boxed(
            (0..8)
                .map(|_| Ok(poll_of(&watch, &["dota2.exe"])))
                .collect(),
        );
        let watcher = GameWatcher::with_detector(detector, &watch, Duration::from_millis(5));
        let (sink, changes) = std::sync::mpsc::channel();
        let handle = spawn(watcher, sink).expect("a watcher thread");
        drop(changes); // the driver is gone
                       // Dropping the handle joins the thread; had the thread not noticed the closed sink this would
                       // hang. The assertion is that it returns.
        handle.stop();
    }
}
