//! CS2 / Dota 2: Valve Game State Integration.
//!
//! A listener on `127.0.0.1` that consumes the JSON the game POSTs, derives events and hands
//! them to a driver. The generated `gamestate_integration_replayed.cfg` has to be in the
//! game's `cfg` directory for the game to post at all. **This crate never writes there**
//! ([`integration`] produces the text only, and nothing in this crate opens a file in a game
//! folder); the desktop shell's "install" action does, at the user's request, and so the
//! file (and the token in it) stays in the Steam library after replayed is uninstalled until
//! the user deletes it. A game still configured to post to a port nobody serves is harmless;
//! a different process that later binds that port would receive the game state and the token.
//! The token is per-install and the port is chosen per session (see below), which is what
//! limits that exposure.
//!
//! Three constraints:
//!
//! 1. Loopback only, by construction. [`bind`] refuses any non-loopback address and the only
//!    other way in, [`bind_loopback`], takes a port alone. No setting names a bind address, so
//!    a listener on `0.0.0.0` cannot be expressed.
//! 2. A route per game, and a token per game. A game posts to its own path (`/gsi/cs2`,
//!    `/gsi/dota2`); a path no game serves is a `404`, and the token that is expected on
//!    that path is that game's alone. The token is checked in constant time, and checked
//!    *before the body is parsed*. It travels in the payload's `auth` block (GSI has no
//!    header), and a request with a missing or different token is refused with `403` without
//!    the body ever being turned into a document ([`token_matches`], [`payload::auth_token`]).
//! 3. No attacker-controlled content in the log. A refused or unreadable request is counted,
//!    and the log line says how many and what kind; the body, headers and token are never
//!    written.
//!
//! The game is told apart by the route first (each game posts to its own path), then by
//! payload (app id, then provider name; see [`derive::Game`]).
//! The listener cannot react to the game exiting: a killed process sends no final POST, so
//! `GameEnd` is only derived from a payload that says so (`map.phase == "gameover"`, Dota's
//! `POST_GAME`). That is why League's end-of-game signal (the endpoint going quiet) differs.
//!
//! # The accept loop never blocks on a connection
//!
//! A peer that opens a connection and then says nothing must not keep the game's own POSTs
//! from being served: the recorder would keep running and the feature it exists for would be
//! silent, a denial of service that looks exactly like a game that is not playing. The
//! listener therefore collects each connection's request itself, without blocking, and gives
//! a reader ([`WORKERS`] of them) only a request that is already whole; the game's requests,
//! recognised by the token they carry, wait in a lane of their own and are not charged to the
//! budget for anonymous traffic. The design, and what it does and does not defend against, is
//! written out on `serve`.
//!
//! The readers fold the payloads into the tracker in the order the requests were completed. A
//! payload that arrives after a later one is dropped rather than folded in: the baseline would
//! move backwards, and the next delta would be reported as a jump. That needs two of the game's
//! own POSTs to be in flight at once, which the game does not do (it waits for the answer), so
//! it is a corner rather than a habit.
//!
//! # What the token does not protect against
//!
//! The token is written into the game's `cfg` directory, and on Windows that directory is
//! readable by the `Users` group. Any local account that can read the game installation can
//! therefore forge events: post a payload with the right token and a scoreboard that says
//! the player got a kill, and the recorder cuts a clip of nothing. The token cannot be
//! hidden — the game has to read it — so the defences are scope and freshness, not secrecy:
//!
//! * **Scope.** Each game has its own path and its own token ([`GsiGameBinding`]), and a
//!   binding can be pinned to an app id ([`GsiConfig::pinned_to`]). A token lifted from a
//!   Counter-Strike installation is refused on Dota 2's path, and a payload that names
//!   another app id is refused even on the path it was lifted for.
//! * **Freshness.** A port of `0` is an ephemeral port, chosen by the OS for the session, so
//!   a token captured from the wire is only good for the session it was captured in and a
//!   process squatting on the port cannot learn the token at all. The `.cfg` the game is
//!   given must name the port the listener actually bound ([`GsiListener::port`]).
//!
//! What is left is the residual risk: a local account that can read the game folder can
//! forge events for the game the listener is pinned to, for as long as the installation
//! keeps the token. Closing that needs an ACL on the `.cfg`, or not writing the token into
//! the game folder at all, and neither is this crate's to do.
//!
//! # The token file, and its second reader
//!
//! The listener reads the token **once**, when it starts. The desktop shell's installer
//! reads the same file **independently**, whenever the user presses "install". If the file
//! is deleted or replaced while a recording is running, the two disagree: the listener keeps
//! the token it started with, the installer writes the new one into the `.cfg`, and every
//! POST is answered `403` until the application restarts. This crate cannot fix that — the
//! installer is not its code — but it can make the divergence visible:
//! [`GsiListener::accepts_token`] answers whether the token in a file is one of the tokens in
//! force, and [`GsiListener::port`] answers the port the game must be told about. The shell
//! should read both from the running listener rather than from the file.

pub mod derive;
pub mod http;
pub mod integration;
pub mod payload;

pub use derive::{Game, Tracker};
pub use integration::{
    cs2_cfg_is_current, game_path, generate_token, integration_cfg, integration_cfg_dota,
    load_or_create_token, CS2_APP_ID, DEFAULT_PATH, DOTA2_APP_ID, TOKEN_BYTES,
};
pub use payload::{AuthToken, GsiSnapshot};

use crate::wire::write_response;
use crate::{EventSink, GameEvent};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::fmt;
use std::io::{ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// How long one request may take to arrive in full. A game POSTs a small body to loopback and
/// the whole request is on the wire within milliseconds, so 2 s is generous. A connection still
/// incomplete after this is answered `400` (or `413` if its head arrived and its body did not)
/// and closed. It costs the listener nothing but a slot in [`MAX_PENDING`] while it waits: no
/// reader is ever held by it (see "The accept loop never blocks on a connection").
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The most requests *that do not carry the game's token* served in one [`RATE_WINDOW`]. A
/// peer without the token is not the game; past this its requests are answered `429` unread.
///
/// Requests that carry the token for their path are not counted: the budget exists to bound
/// what an anonymous local process or web page can make the recorder spend, and charging the
/// game's own POSTs to it let that process spend the game's share. See "Starving the game".
pub const MAX_REQUESTS_PER_WINDOW: u32 = 100;

/// The length of the fixed window [`MAX_REQUESTS_PER_WINDOW`] is counted in.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);

/// The most wall-clock time one request may take to arrive in total: the same bound as
/// [`REQUEST_TIMEOUT`], kept as its own name because a trickling peer resets any per-read timeout
/// forever and only a total deadline stops it.
pub const REQUEST_DEADLINE: Duration = REQUEST_TIMEOUT;

/// How many connections are answered at once. Readers only ever see a request that is already
/// whole in memory, so a reader's work is a parse and a lock, not a wait on a peer; four is
/// plenty for one client posting a handful of times a second.
pub const WORKERS: usize = 4;

/// How many whole requests may wait for a reader, per lane (the game's, and everyone else's).
///
/// This is the in-flight bound on work, not on connections: a full lane answers the next request
/// `429`. Without it, a process sending complete requests as fast as it can would make the
/// recorder queue thousands of them.
pub const MAX_QUEUED: usize = 4;

/// How many connections still sending their request the listener holds at once.
///
/// Connections that have not finished their request wait here, in the listener's own thread,
/// with no reader attached. When it is full, the next connection takes the place of the most
/// idle one (see `evict`), so a peer that opens sockets and says nothing can fill this but
/// cannot keep the game's connection out of it. Bounded because each can hold up to a request's
/// worth of bytes.
pub const MAX_PENDING: usize = 16;

/// How often the accept loop checks its stop flag while nothing is connected: a sleep, not a
/// spin.
const ACCEPT_SLICE: Duration = Duration::from_millis(25);

/// How often the intake looks again while at least one connection is still sending.
const INTAKE_SLICE: Duration = Duration::from_millis(2);

/// The most connections taken from the listener in one pass, so a flood cannot keep the loop
/// from looking at the connections it already holds.
const ACCEPTS_PER_PASS: usize = 64;

/// The most bytes taken from one connection in one pass, for the same reason.
const READ_PER_PASS: usize = 64 * 1024;

/// A fixed-window request counter: [`MAX_REQUESTS_PER_WINDOW`] per [`RATE_WINDOW`].
#[derive(Debug)]
struct RateLimiter {
    window_start: Instant,
    count: u32,
}

impl RateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            count: 0,
        }
    }

    /// Whether a request arriving at `now` may be served.
    fn allow(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.window_start) >= RATE_WINDOW {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= MAX_REQUESTS_PER_WINDOW {
            return false;
        }
        self.count += 1;
        true
    }
}

/// One game the listener serves: the path it posts to, its own token, and the app id
/// its payloads must carry.
///
/// The binding is what scopes a token. The token is written into the game's `cfg`
/// directory, where any local account that can read the installation can read it, so a
/// token lifted from one game's file is only ever good for the path it was lifted for —
/// and, when the binding names an app id, only for that game's payloads.
#[derive(Clone, PartialEq, Eq)]
pub struct GsiGameBinding {
    /// The path the game posts to, e.g. `/gsi/cs2`. Anything else is a 404.
    pub path: String,
    /// The token for this game alone. Never logged, never printed.
    pub token: String,
    /// Valve's app id: 730 for Counter-Strike, 570 for Dota 2. `None` accepts any.
    pub app_id: Option<u32>,
}

impl GsiGameBinding {
    /// A binding for one game.
    pub fn new(path: impl Into<String>, token: impl Into<String>, app_id: Option<u32>) -> Self {
        Self {
            path: path.into(),
            token: token.into(),
            app_id,
        }
    }
}

/// The token is a credential, so it is never printed: the `Debug` of a binding shows the
/// path and the app id, and says the token is there without saying what it is.
impl fmt::Debug for GsiGameBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GsiGameBinding")
            .field("path", &self.path)
            .field("token", &"<redacted>")
            .field("app_id", &self.app_id)
            .finish()
    }
}

/// One game a listener serves, as the listener reports it: the path and the app id, and
/// never the token. The crate does not hand a token back out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsiGameInfo {
    /// The path the game posts to.
    pub path: String,
    /// The app id the binding is pinned to, if it is pinned to one.
    pub app_id: Option<u32>,
}

/// Everything the listener needs.
#[derive(Clone, PartialEq, Eq)]
pub struct GsiConfig {
    /// The port from `[events] gsi_port`. Zero means an ephemeral port: the OS picks one
    /// for the session, and the `.cfg` the game is given must name the port the listener
    /// actually bound ([`GsiListener::port`]). A fixed port can be squatted on before the
    /// application starts, and a process squatting on it receives the game's POSTs — and
    /// with them the token.
    pub port: u16,
    /// The games this listener serves. Must not be empty.
    pub games: Vec<GsiGameBinding>,
}

/// The tokens are credentials, so they are never printed: the `Debug` of a configuration
/// shows the port and the games, and each game's `Debug` redacts its token.
impl fmt::Debug for GsiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GsiConfig")
            .field("port", &self.port)
            .field("games", &self.games)
            .finish()
    }
}

impl GsiConfig {
    /// The production configuration: the default path, and the caller's token.
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            port,
            games: vec![GsiGameBinding::new(DEFAULT_PATH, token, None)],
        }
    }

    /// The same listener, serving one game only.
    ///
    /// A payload whose `provider.appid` is another game's is refused with `403`, so a token
    /// lifted from one game's installed `.cfg` cannot forge the other's events. The game is
    /// named by Valve's own app id: 730 for Counter-Strike, 570 for Dota 2.
    ///
    /// The app id is set on the first (and, in practice, only) binding: a listener serving
    /// two games is not pinned to one.
    pub fn pinned_to(mut self, app_id: u32) -> Self {
        if let Some(first) = self.games.first_mut() {
            first.app_id = Some(app_id);
        }
        self
    }

    /// The same listener, serving exactly one game: the given path, token and app id.
    ///
    /// The bindings are replaced rather than added to, so this reads as "this listener
    /// serves that one game" rather than as an addition to whatever it served before.
    pub fn with_game(
        mut self,
        path: impl Into<String>,
        token: impl Into<String>,
        app_id: Option<u32>,
    ) -> Self {
        self.games = vec![GsiGameBinding::new(path, token, app_id)];
        self
    }

    /// The same listener, serving one more game: the given path, token and app id.
    pub fn add_game(
        mut self,
        path: impl Into<String>,
        token: impl Into<String>,
        app_id: Option<u32>,
    ) -> Self {
        self.games.push(GsiGameBinding::new(path, token, app_id));
        self
    }

    /// The binding that serves `path`, if this listener serves it.
    ///
    /// The path is matched exactly: a request for a path no game serves is a 404, and a
    /// request for one that is served is checked against that game's own token.
    pub fn game(&self, path: &str) -> Option<&GsiGameBinding> {
        self.games.iter().find(|game| game.path == path)
    }
}

/// Bind a listener, refusing any address that is not loopback. This check is why the function
/// exists instead of a bare `TcpListener::bind`: `0.0.0.0` would silently expose the game's
/// state to the network.
pub fn bind(addr: SocketAddr) -> Result<TcpListener> {
    if !addr.ip().is_loopback() {
        bail!("the GSI listener is a loopback service and refuses to bind {addr}");
    }
    TcpListener::bind(addr).with_context(|| format!("binding the GSI listener to {addr}"))
}

/// Bind the GSI listener on `127.0.0.1:<port>`. The only constructor production uses.
pub fn bind_loopback(port: u16) -> Result<TcpListener> {
    bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

/// Whether `presented` is the expected token, compared in constant time.
///
/// Fails closed: an empty expected token is a programming error and matches nothing, since
/// it would accept every payload declaring an empty one. A token of a different length does
/// not match either, so the comparison is over the whole value and there is no prefix of a
/// token that is ever accepted.
fn token_equals(expected: &str, presented: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Whether `snapshot` carries the expected token.
///
/// Fails closed: no token, a token of a different length, and a right-length token differing
/// by one byte are all `false`. The comparison is constant time ([`subtle`]) so a local
/// process cannot learn the token byte by byte from rejection timing. An empty expected token
/// is a programming error and is also refused, since it would accept every payload declaring
/// an empty one.
pub fn token_matches(expected: &str, snapshot: &GsiSnapshot) -> bool {
    snapshot
        .auth
        .get("token")
        .is_some_and(|presented| token_equals(expected, presented))
}

/// A running listener.
pub struct GsiListener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    addr: SocketAddr,
    /// The tokens in force, one per game, so that a caller holding a token file can ask
    /// whether the file is one this listener accepts. The struct deliberately has no
    /// `Debug`: the tokens are never printed.
    tokens: Vec<String>,
    /// The games this listener serves, without their tokens: the crate never hands a token
    /// back out, so the paths and the app ids are all a caller can be told.
    games: Vec<GsiGameInfo>,
}

impl GsiListener {
    /// The address actually bound (an ephemeral port in tests); what matters is that its IP is
    /// loopback.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The port the listener is bound on.
    ///
    /// The `.cfg` the game is given must name this port and not a configured one: a port
    /// that was already taken, or an ephemeral one, are both ways for the game and the
    /// listener to disagree, and the game would then post into the void.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The games this listener serves, WITHOUT their tokens: the crate never hands a token
    /// back out.
    ///
    /// A caller that has to write one `.cfg` per game needs the path each game posts to
    /// and the app id its binding is pinned to; the token it already has.
    pub fn games(&self) -> Vec<GsiGameInfo> {
        self.games.clone()
    }

    /// Whether `candidate` is a token this listener accepts.
    ///
    /// The desktop shell reads the token file on its own to write the `.cfg`, and the
    /// listener read it once when it started. If the file has been replaced since, the two
    /// disagree and every POST is refused until the application restarts. This is how the
    /// shell can notice: it asks, instead of guessing from a file that may be stale. The
    /// comparison is constant time, like every other one of them, and the token is never
    /// handed back out of the crate.
    ///
    /// The listener serves one game per path, each with its own token, so the answer is
    /// "is this the token of any of them".
    pub fn accepts_token(&self, candidate: &str) -> bool {
        // Every token is compared, and the results are combined without an early exit: the
        // answer must not depend on which game the candidate belongs to.
        let mut accepted = false;
        for expected in &self.tokens {
            accepted |= token_equals(expected, candidate);
        }
        accepted
    }

    /// Stop accepting and wait for the thread. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GsiListener {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start the listener. Every derived event goes to `sink` until the handle is dropped.
pub fn spawn(config: GsiConfig, sink: EventSink) -> Result<GsiListener> {
    if config.games.is_empty() {
        // A listener that serves nothing answers every request with a 404, and looks like a
        // broken installation rather than a misconfigured one.
        bail!("the GSI listener needs at least one game to serve");
    }
    for game in &config.games {
        if game.token.is_empty() {
            // Refused at startup rather than failing every request later: a listener that cannot
            // authenticate anything should not be running.
            bail!(
                "the GSI listener needs a token for {}; generate one and put it in the game's cfg file",
                game.path
            );
        }
    }
    for (n, game) in config.games.iter().enumerate() {
        if config.games[..n]
            .iter()
            .any(|other| other.path == game.path)
        {
            // Two games on one path would share a route, and with it a token: the binding is
            // what scopes a token, so it has to be one game per path.
            bail!(
                "the GSI listener serves {} twice; each game needs its own path",
                game.path
            );
        }
    }
    let listener = bind_loopback(config.port)?;
    let addr = listener
        .local_addr()
        .context("reading the listener's address")?;
    listener
        .set_nonblocking(true)
        .context("making the listener non-blocking")?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let paths = config
        .games
        .iter()
        .map(|game| game.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!("the GSI listener is up on http://{addr} (loopback only), serving {paths}");

    let tokens = config.games.iter().map(|game| game.token.clone()).collect();
    let games = config
        .games
        .iter()
        .map(|game| GsiGameInfo {
            path: game.path.clone(),
            app_id: game.app_id,
        })
        .collect();
    let thread = std::thread::Builder::new()
        .name("replayed-gsi".to_string())
        .spawn(move || serve(listener, &config, &sink, &flag))
        .context("spawning the GSI thread")?;

    Ok(GsiListener {
        stop,
        thread: Some(thread),
        addr,
        tokens,
        games,
    })
}

/// One whole request, handed to a reader.
struct Job {
    stream: TcpStream,
    /// Every byte the connection sent up to the end of its request (`frame` said it was complete),
    /// so reading it touches no socket.
    request: Vec<u8>,
    /// The order the requests were completed in. The tracker is folded in this order, so a
    /// payload that arrives after a later one is dropped rather than moving the baseline
    /// backwards.
    seq: u64,
}

/// Which queue a request waits in. This decides only the order of service and which budget the
/// request is charged to; whether it is believed is decided by the token gate in `serve_one`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Carries the token for the path it posted to: very likely the game.
    Trusted,
    /// Anything else.
    Anonymous,
}

#[derive(Default)]
struct QueueState {
    trusted: VecDeque<Job>,
    anonymous: VecDeque<Job>,
    closed: bool,
}

/// Two bounded queues in front of the readers. The readers take the game's lane first, and the
/// bound on each lane is its own, so an anonymous peer filling its lane leaves the game's
/// untouched.
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl Queue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        }
    }

    /// Queue `job`, or hand it back when its lane is full or the queue is closed.
    fn push(&self, lane: Lane, job: Job) -> Result<(), Job> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(job);
        }
        let queue = match lane {
            Lane::Trusted => &mut state.trusted,
            Lane::Anonymous => &mut state.anonymous,
        };
        if queue.len() >= MAX_QUEUED {
            return Err(job);
        }
        queue.push_back(job);
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    /// The next job, the game's lane first; `None` once the queue is closed and empty.
    fn pop(&self) -> Option<Job> {
        let mut state = lock(&self.state);
        loop {
            if let Some(job) = state.trusted.pop_front() {
                return Some(job);
            }
            if let Some(job) = state.anonymous.pop_front() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }
}

/// A connection whose request is still arriving. It is held by the listener's own thread and read
/// without blocking: no reader waits on it.
struct Pending {
    stream: TcpStream,
    buf: Vec<u8>,
    arrived: Instant,
    /// The peer has closed its side. What it sent is all there will be.
    eof: bool,
}

/// What the listener keeps between connections.
#[derive(Debug, Default)]
struct State {
    /// The derivation's own state: the baseline the next payload is compared against.
    tracker: Tracker,
    /// How many requests of each kind the listener has seen.
    counters: Counters,
    /// The completion order of the last payload folded into the tracker.
    last_seq: u64,
}

/// The lock, ignoring poisoning.
///
/// A connection that panics must not take the listener with it: the poison is the record of
/// that panic, and the state behind it is still the truth about the game.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The accept loop, the intake and the readers.
///
/// # Starving the game
///
/// Everything the game's POST has to get through is reachable by any process on the machine, and
/// by any web page in the user's browser (`POST` with `text/plain` is a CORS simple request: no
/// preflight, and `mode: 'no-cors'` hides the failure from the page). An earlier version spent
/// its readers on whoever connected first. Eight sockets that connected and said nothing held
/// all four readers and the whole queue for two seconds, and more than a hundred connections a
/// second used up a global counter charged before anything was read; either way the game was
/// answered `429` and nothing in the UI said why. The recorder kept running, and the feature it
/// exists for looked exactly like a game that is not playing.
///
/// The shape now is:
///
/// 1. **No reader waits on a peer.** This thread accepts and collects each connection's bytes
///    without blocking ([`Pending`]). A connection is handed to a reader only once
///    [`http::frame`] says its request is whole, so a reader's work is a parse out of memory.
///    Sockets that send nothing, or send a head and stall, cost a slot in [`MAX_PENDING`] and
///    nothing else; at [`REQUEST_TIMEOUT`] they are answered and closed.
/// 2. **The pending set cannot be filled.** When it is full a new connection takes the place of
///    the most idle one (`evict`); the connection that has already sent bytes is never the
///    victim while an idle one exists. The game's request is normally in the socket before
///    `accept` returns, and it is read in the same pass.
/// 3. **The budget is charged by outcome, not by arrival.** A whole request is classified
///    ([`lane_of`]): one that carries the token for its path goes in the game's lane, which has
///    its own bound and is served first, and is not counted by the limiter. Everything else is
///    charged to [`MAX_REQUESTS_PER_WINDOW`] and waits in the anonymous lane. A peer without
///    the token can use up its own budget and its own lane; it cannot reach the game's.
/// 4. **Requests a web page made are refused outright.** Every fetch a browser makes carries
///    `Sec-Fetch-*` headers, which a script cannot set or remove and which no game sends
///    ([`http::Framing::Browser`]). They are answered `403` as soon as the head is in,
///    token or not, and take no budget. A browser too old to send them falls under 3.
///
/// What this does not change: the token is still checked in constant time, in full, by the
/// reader, before the body is parsed. Lane selection looks for the token as the value of a
/// `"token"` key at either end of the body ([`payload::carries_token`]); it decides only which
/// queue a request waits in, so fooling it without the token buys nothing, and a peer that has
/// the token can already forge events (see "What the token does not protect against"). What
/// it cannot defend against is a local process with enough CPU to starve the machine, which no
/// listener can: that is a denial of service against the whole PC, not against this feature.
///
/// The readers fold the payloads into the tracker in the order the requests were completed.
fn serve(listener: TcpListener, config: &GsiConfig, sink: &EventSink, stop: &Arc<AtomicBool>) {
    let queue = Arc::new(Queue::new());
    let state = Arc::new(Mutex::new(State::default()));

    let mut readers = Vec::with_capacity(WORKERS);
    for n in 0..WORKERS {
        let worker_queue = Arc::clone(&queue);
        let state = Arc::clone(&state);
        let config = config.clone();
        let sink = sink.clone();
        let flag = Arc::clone(stop);
        let reader = std::thread::Builder::new()
            .name(format!("replayed-gsi-{n}"))
            .spawn(move || read_loop(&worker_queue, &config, &sink, &state, &flag))
            .with_context(|| format!("spawning the GSI reader thread {n}"));
        match reader {
            Ok(handle) => readers.push(handle),
            Err(err) => {
                // Without a reader there is nothing to answer with, so the listener stops
                // rather than accepting connections it cannot serve.
                tracing::warn!("the GSI listener could not start a reader: {err:#}");
                queue.close();
                return;
            }
        }
    }

    let mut limiter = RateLimiter::new(Instant::now());
    let mut seq: u64 = 0;
    let mut pending: Vec<Pending> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let mut accepted_any = false;
        for _ in 0..ACCEPTS_PER_PASS {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    // `_peer` is not logged: on a loopback bind it is `127.0.0.1` by definition.
                    accepted_any = true;
                    // The accepted socket may inherit the listener's non-blocking flag (macOS, *BSD) or
                    // not (Linux, Windows). It must be non-blocking whichever: the intake never waits.
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let _ = stream.set_nodelay(true);
                    if pending.len() >= MAX_PENDING {
                        if let Some(victim) = evict(&mut pending) {
                            lock(&state).counters.busy();
                            turn_away(victim.stream);
                        }
                    }
                    pending.push(Pending {
                        stream,
                        buf: Vec::new(),
                        arrived: Instant::now(),
                        eof: false,
                    });
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => {
                    tracing::warn!("the GSI listener could not accept a connection: {err}");
                    std::thread::sleep(ACCEPT_SLICE);
                    break;
                }
            }
        }

        let now = Instant::now();
        let mut i = 0;
        while i < pending.len() {
            match intake_step(&mut pending[i], now, config) {
                Step::Wait => i += 1,
                step => {
                    let Pending { stream, buf, .. } = pending.swap_remove(i);
                    match step {
                        Step::Wait => {}
                        Step::Gone => {}
                        Step::Refuse(reject) => {
                            lock(&state).counters.refused(reject);
                            refuse(stream, reject.status());
                        }
                        Step::Dispatch(lane) => {
                            if lane == Lane::Anonymous && !limiter.allow(now) {
                                lock(&state).counters.rate_limited();
                                turn_away(stream);
                                continue;
                            }
                            seq += 1;
                            let job = Job {
                                stream,
                                request: buf,
                                seq,
                            };
                            if let Err(job) = queue.push(lane, job) {
                                // The lane is full: refused whole rather than held.
                                lock(&state).counters.busy();
                                turn_away(job.stream);
                            }
                        }
                    }
                }
            }
        }

        if !accepted_any {
            std::thread::sleep(if pending.is_empty() {
                ACCEPT_SLICE
            } else {
                INTAKE_SLICE
            });
        }
    }

    // The readers wait on the queue, and the queue ends when it is closed. Connections still
    // arriving are dropped.
    drop(pending);
    queue.close();
    for reader in readers {
        let _ = reader.join();
    }
    tracing::debug!("the GSI listener stopped");
}

/// What the intake decided about one connection in one pass.
enum Step {
    /// Still arriving: look again next pass.
    Wait,
    /// The peer left before it finished; there is nobody to answer.
    Gone,
    /// Answer with this refusal and close.
    Refuse(http::Reject),
    /// A whole request: queue it in this lane.
    Dispatch(Lane),
}

/// Take what a connection has sent so far and decide what to do with it. Never blocks.
fn intake_step(pending: &mut Pending, now: Instant, config: &GsiConfig) -> Step {
    let mut chunk = [0u8; 8 * 1024];
    let mut taken = 0;
    while taken < READ_PER_PASS && !pending.eof {
        match pending.stream.read(&mut chunk) {
            Ok(0) => pending.eof = true,
            Ok(n) => {
                pending.buf.extend_from_slice(&chunk[..n]);
                taken += n;
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Step::Gone,
        }
    }
    match http::frame(&pending.buf) {
        http::Framing::Invalid => Step::Refuse(http::Reject::Malformed),
        http::Framing::Browser => Step::Refuse(http::Reject::FromBrowser),
        http::Framing::Complete { body } => Step::Dispatch(lane_of(config, &pending.buf, body)),
        http::Framing::Incomplete { head_done } => {
            if pending.eof || now.saturating_duration_since(pending.arrived) >= REQUEST_DEADLINE {
                // A truncated head is not HTTP; a head with less body than it claimed is a body "shorter
                // than it claims", as the framing has always answered it.
                Step::Refuse(if head_done {
                    http::Reject::TooLarge
                } else {
                    http::Reject::Malformed
                })
            } else {
                Step::Wait
            }
        }
    }
}

/// Which lane a whole request waits in: [`Lane::Trusted`] when it is a POST to a path this listener
/// serves and its body carries that path's token, else [`Lane::Anonymous`].
fn lane_of(config: &GsiConfig, request: &[u8], body: std::ops::Range<usize>) -> Lane {
    let trusted = route_of(request)
        .and_then(|target| {
            let path = target.split('?').next().unwrap_or_default();
            config
                .game(path)
                .map(|game| payload::carries_token(&request[body], &game.token))
        })
        .unwrap_or(false);
    if trusted {
        Lane::Trusted
    } else {
        Lane::Anonymous
    }
}

/// The connection to give up when the pending set is full and another has arrived: the oldest of
/// those that have sent nothing, else the oldest. A connection that has sent bytes is never
/// preferred over one that has not, so the game's request (already in the socket) outlives any
/// number of idle ones.
fn evict(pending: &mut Vec<Pending>) -> Option<Pending> {
    let victim = pending
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| (!p.buf.is_empty(), p.arrived))
        .map(|(index, _)| index)?;
    Some(pending.swap_remove(victim))
}

/// One reader: take a whole request, answer it, take the next.
fn read_loop(
    queue: &Arc<Queue>,
    config: &GsiConfig,
    sink: &EventSink,
    state: &Arc<Mutex<State>>,
    stop: &Arc<AtomicBool>,
) {
    while let Some(job) = queue.pop() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        serve_one(job, config, sink, state, stop);
    }
}

/// Answer without reading the request, then give the socket up cleanly: closing a socket holding
/// unread data resets the connection, and the reset can destroy the response before the peer
/// reads it. The socket is non-blocking (the intake's), so none of this can wait on the peer.
fn refuse(mut stream: TcpStream, status: u16) {
    let _ = write_response(&mut stream, status);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut sink = [0u8; 4096];
    for _ in 0..4 {
        if !matches!(stream.read(&mut sink), Ok(n) if n == sink.len()) {
            break;
        }
    }
}

/// `429`, for a request the listener will not take.
fn turn_away(stream: TcpStream) {
    refuse(stream, 429);
}

/// The request's target, from the first line of `request`, or `None` when that is not a request
/// line.
fn route_of(request: &[u8]) -> Option<String> {
    let end = request.iter().position(|byte| *byte == b'\n')?;
    let line = String::from_utf8_lossy(&request[..end]);
    request_target(line.trim_end_matches('\r'))
}

/// The target of a request line, or `None` when the line is not one.
///
/// The line is read the way [`crate::wire`] reads it: a method, a target and a version,
/// separated by single spaces.
fn request_target(line: &str) -> Option<String> {
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
        return None;
    }
    Some(target.to_string())
}

/// Serve one whole request.
///
/// The order of the gates is the point of this function: the route is resolved, the request
/// is framed, the token is looked for in the raw body, and only a body that carries the
/// right token is ever turned into a document. A peer that cannot authenticate — a web page
/// in the user's browser can `POST` here with a `text/plain` content type, which is a CORS
/// "simple request" and is not preflighted — therefore cannot make the recorder parse
/// megabytes.
///
/// The request is already in memory ([`Job::request`]), so nothing here waits on the peer.
fn serve_one(
    job: Job,
    config: &GsiConfig,
    sink: &EventSink,
    state: &Arc<Mutex<State>>,
    stop: &Arc<AtomicBool>,
) {
    let Job {
        mut stream,
        request: raw,
        seq,
    } = job;
    // Blocking again, so the write below is bounded by a timeout and not by `WouldBlock`.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));

    // The route first: the path says which game is posting, and the game says which token
    // the body has to carry. A path no game serves is a 404, and the body is not even read.
    let Some(target) = route_of(&raw) else {
        lock(state).counters.refused(http::Reject::Malformed);
        let _ = write_response(&mut stream, http::Reject::Malformed.status());
        return;
    };
    // The path is what routes, and nothing in a query is ever read.
    let path = target.split('?').next().unwrap_or_default();
    let Some(game) = config.game(path) else {
        lock(state).counters.refused(http::Reject::NotFound);
        let _ = write_response(&mut stream, http::Reject::NotFound.status());
        return;
    };
    let request = match http::read_request(&mut std::io::Cursor::new(raw), &game.path) {
        Ok(request) => request,
        Err(reject) => {
            lock(state).counters.refused(reject);
            let _ = write_response(&mut stream, reject.status());
            return;
        }
    };

    // The token, read out of the raw body without parsing the rest of it.
    let presented = match payload::auth_token(&request.body) {
        payload::AuthToken::Present(token) => token,
        payload::AuthToken::Absent => {
            lock(state).counters.refused_token();
            let _ = write_response(&mut stream, 403);
            return;
        }
        payload::AuthToken::Unreadable => {
            lock(state).counters.malformed(request.body.len());
            let _ = write_response(&mut stream, 400);
            return;
        }
    };
    if !token_equals(&game.token, &presented) {
        lock(state).counters.refused_token();
        let _ = write_response(&mut stream, 403);
        return;
    }

    // The body is known to carry the right token, so it is worth parsing.
    let snapshot = match payload::parse(&request.body) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            // Deliberately silent about what was wrong: the body is attacker-controlled before the token
            // gate runs and `serde_json`'s message can quote it. The size and count suffice to debug an
            // install.
            lock(state).counters.malformed(request.body.len());
            let _ = write_response(&mut stream, 400);
            return;
        }
    };

    // The document is the authority on the token, not the look above: a body carrying two
    // `auth` blocks is parsed into the last one, and that is the one that counts.
    if !token_matches(&game.token, &snapshot) {
        lock(state).counters.refused_token();
        let _ = write_response(&mut stream, 403);
        return;
    }
    if let Some(app_id) = game.app_id {
        if snapshot.provider.appid != app_id {
            lock(state).counters.refused_game(app_id, &game.path);
            let _ = write_response(&mut stream, 403);
            return;
        }
    }

    // Fold it in, in the order the requests were completed. The events are emitted under
    // the same lock, so they leave in that order too.
    {
        let mut state = lock(state);
        if seq <= state.last_seq {
            // A payload that arrived after a later one: folding it in would move the
            // baseline backwards, and the next delta would be reported as a jump.
            tracing::debug!("the GSI listener dropped a payload that arrived out of order");
            return;
        }
        state.last_seq = seq;
        let events = state.tracker.observe(&snapshot);
        state.counters.accepted();
        for event in events {
            log_event(&event);
            // A disconnected sink means the driver is gone.
            if sink.send(event).is_err() {
                stop.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
    // Answer last: a client that reads the response knows the payload was taken.
    let _ = write_response(&mut stream, 200);
}

/// One line per derived event at the level it deserves: game transitions `info`, each kill
/// `debug`. The payload is written by [`derive`], never echoed from the request, so quoting it
/// is safe.
fn log_event(event: &GameEvent) {
    match event.kind {
        crate::EventKind::GameStart => {
            tracing::info!(
                "a GSI game is being watched: {}",
                event.payload.as_deref().unwrap_or("")
            )
        }
        crate::EventKind::GameEnd => tracing::info!("the GSI game is over"),
        kind => tracing::debug!(
            "GSI event: {kind} {}",
            event.payload.as_deref().unwrap_or("")
        ),
    }
}

/// How many requests of each kind the listener has seen, so a condition is stated once and
/// then counted; otherwise anything scanning loopback ports could fill the log.
#[derive(Debug, Default)]
struct Counters {
    accepted: u64,
    refused: u64,
    refused_tokens: u64,
    refused_games: u64,
    malformed: u64,
    rate_limited: u64,
    busy: u64,
}

impl Counters {
    fn accepted(&mut self) {
        self.accepted += 1;
    }

    /// A request that was refused before it was read (wrong method, wrong path, no length,
    /// too large, not HTTP).
    fn refused(&mut self, reject: http::Reject) {
        self.refused += 1;
        if self.refused == 1 {
            tracing::info!(
                "the GSI listener refused a request: {reject}. This is normal for anything \
                 other than the game's own POST; replayed answers it with HTTP {}.",
                reject.status()
            );
        } else {
            tracing::debug!("the GSI listener has refused {} requests", self.refused);
        }
    }

    /// A well-formed payload whose token did not match: what a user hits when the generated cfg
    /// was not re-installed after the token changed.
    fn refused_token(&mut self) {
        self.refused_tokens += 1;
        if self.refused_tokens == 1 {
            tracing::warn!(
                "the GSI listener rejected a payload whose token does not match. Install the \
                 generated gamestate_integration_replayed.cfg into the game's cfg directory \
                 (it carries the token) and restart the game."
            );
        } else {
            tracing::debug!(
                "the GSI listener has rejected {} payloads by token",
                self.refused_tokens
            );
        }
    }

    fn rate_limited(&mut self) {
        self.rate_limited += 1;
        if self.rate_limited == 1 {
            tracing::warn!(
                "the GSI listener is over {MAX_REQUESTS_PER_WINDOW} unauthenticated requests per \
                 second and is answering the excess with HTTP 429 (a request carrying the game's \
                 token is not counted)"
            );
        } else {
            tracing::debug!(
                "the GSI listener has rate-limited {} connections",
                self.rate_limited
            );
        }
    }

    /// A payload that authenticated but is another game's: the token was lifted from the
    /// wrong installed file.
    fn refused_game(&mut self, app_id: u32, path: &str) {
        self.refused_games += 1;
        if self.refused_games == 1 {
            tracing::warn!(
                "the GSI listener rejected a payload for another game: {path} serves app id \
                 {app_id} only"
            );
        } else {
            tracing::debug!(
                "the GSI listener has rejected {} payloads for another game",
                self.refused_games
            );
        }
    }

    /// A connection turned away because a bound was full: the open-connection set
    /// ([`MAX_PENDING`]) gave up its most idle member, or a lane of the reader queue was full. The
    /// in-flight bounds, not the per-second one.
    fn busy(&mut self) {
        self.busy += 1;
        if self.busy == 1 {
            tracing::warn!(
                "the GSI listener holds at most {MAX_PENDING} open connections and has \
                 {WORKERS} readers with a queue of {MAX_QUEUED} per lane, and is answering \
                 the excess with HTTP 429"
            );
        } else {
            tracing::debug!(
                "the GSI listener has refused {} busy connections",
                self.busy
            );
        }
    }

    fn malformed(&mut self, bytes: usize) {
        self.malformed += 1;
        if self.malformed == 1 {
            tracing::warn!(
                "the GSI listener could not parse a {bytes}-byte payload; the body was \
                 dropped and the request answered with HTTP 400"
            );
        } else {
            tracing::debug!(
                "the GSI listener has dropped {} unparseable payloads",
                self.malformed
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_limiter_allows_a_window_s_worth_then_refuses_until_the_next_window() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(t0);
        for n in 0..MAX_REQUESTS_PER_WINDOW {
            assert!(limiter.allow(t0), "request {n} is inside the cap");
        }
        assert!(!limiter.allow(t0), "the next one is over it");
        assert!(
            !limiter.allow(t0 + RATE_WINDOW / 2),
            "and so is one later in the same window"
        );
        assert!(
            limiter.allow(t0 + RATE_WINDOW),
            "a new window starts afresh"
        );
    }

    use std::net::SocketAddr;

    fn snapshot(body: &str) -> GsiSnapshot {
        payload::parse(body.as_bytes()).expect("the fixture parses")
    }

    #[test]
    fn a_listener_refuses_to_bind_anything_that_is_not_loopback() {
        for addr in ["0.0.0.0:0", "192.168.0.1:45671", "[::]:45671", "8.8.8.8:80"] {
            let addr: SocketAddr = addr.parse().expect("a socket address");
            let err = bind(addr)
                .map(|_| ())
                .expect_err("only loopback may be bound");
            assert!(
                err.to_string().contains("loopback"),
                "the error must name the rule: {err}"
            );
        }
    }

    #[test]
    fn the_listener_binds_loopback_and_an_ephemeral_port() {
        // Port 0 (ephemeral) keeps a test suite from fighting over a fixed port; still loopback.
        let listener = bind_loopback(0).expect("a loopback listener");
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback(), "bound to {addr}");
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0, "the OS assigned a real port");
    }

    #[test]
    fn the_token_gate_accepts_only_the_exact_token() {
        let expected = "0123456789abcdef0123456789abcdef";
        let with = |token: &str| {
            snapshot(&format!(
                r#"{{"auth":{{"token":"{token}"}},"provider":{{"appid":730}}}}"#
            ))
        };

        assert!(
            token_matches(expected, &with(expected)),
            "the same token is accepted"
        );
        assert!(
            !token_matches(expected, &with("0123456789abcdef0123456789abcdee")),
            "one byte off"
        );
        assert!(
            !token_matches(expected, &with("0123456789abcdef0123456789abcde")),
            "one byte short"
        );
        assert!(
            !token_matches(expected, &with("0123456789abcdef0123456789abcdefg")),
            "one byte long"
        );
        assert!(
            !token_matches(expected, &with("")),
            "an empty token is not a token"
        );
        assert!(
            !token_matches(expected, &snapshot(r#"{"provider":{"appid":730}}"#)),
            "a payload with no auth block is refused"
        );
        assert!(
            !token_matches(expected, &snapshot(r#"{"auth":{"kind":"cs2"}}"#)),
            "an auth block without a token is refused"
        );
        assert!(
            !token_matches("", &with("")),
            "an unconfigured token fails closed rather than accepting an empty one"
        );
    }

    #[test]
    fn the_token_is_compared_over_the_whole_value() {
        // A timing test would be flaky. Assert instead what constant time exists for: no prefix of a
        // token is ever accepted at any length, so there is no early-accept path to time.
        let expected = "abcdef0123456789abcdef0123456789";
        for end in 0..expected.len() {
            let prefix = &expected[..end];
            assert!(
                !token_matches(
                    expected,
                    &snapshot(&format!(r#"{{"auth":{{"token":"{prefix}"}}}}"#))
                ),
                "a {end}-byte prefix must not be accepted"
            );
        }
    }

    #[test]
    fn a_listener_without_a_token_refuses_to_start() {
        let (sink, _rx) = std::sync::mpsc::channel();
        let err = spawn(GsiConfig::new(0, ""), sink)
            .map(|_| ())
            .expect_err("no token");
        assert!(format!("{err:#}").contains("token"), "got: {err:#}");
    }

    #[test]
    fn the_listener_reports_the_port_it_bound_and_the_token_in_force() {
        // The `.cfg` the game is given must name the port the listener actually bound, and
        // a shell that reads the token file on its own must be able to tell whether the
        // file it read is the one in force: a file replaced while the recording runs is a
        // divergence the shell can only notice if the listener answers.
        let (sink, _rx) = std::sync::mpsc::channel();
        // A fixed token, not a credential: the digits of pi in hex, the same in every test.
        // The real one is generated from the OS entropy source at run time. `gitleaks:allow`
        // because a hex string assigned to a name containing "token" reads as a leak.
        let token = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
        let listener = spawn(GsiConfig::new(0, token), sink).expect("the listener starts");
        assert_eq!(listener.port(), listener.local_addr().port());
        assert_ne!(listener.port(), 0, "the OS assigned a real port");
        assert!(
            listener.accepts_token(token),
            "the token in force is accepted"
        );
        assert!(
            !listener.accepts_token("0123456789abcdef0123456789abcdee"),
            "one byte off is not"
        );
        assert!(
            !listener.accepts_token(""),
            "and an empty one is not a token"
        );
    }

    #[test]
    fn a_listener_serving_two_games_accepts_the_token_of_either_of_them() {
        // The shell asks whether the token it read is one in force: with a token per game,
        // the answer is "is it the token of any of them".
        let (sink, _rx) = std::sync::mpsc::channel();
        let cs2 = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
        let dota = "fedcba9876543210fedcba9876543210"; // gitleaks:allow
        let listener = spawn(
            GsiConfig::new(0, cs2)
                .with_game("/gsi/cs2", cs2, Some(730))
                .add_game("/gsi/dota2", dota, Some(570)),
            sink,
        )
        .expect("the listener starts");

        assert!(listener.accepts_token(cs2), "the first game's token");
        assert!(listener.accepts_token(dota), "the second game's token");
        assert!(
            !listener.accepts_token("0123456789abcdef0123456789abcdee"),
            "one byte off is not"
        );
        assert!(
            !listener.accepts_token(""),
            "and an empty one is not a token"
        );
    }

    #[test]
    fn a_configuration_never_prints_a_token() {
        // The token is a credential, and a `{:?}` ends up in a log line sooner or later.
        let token = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
        let config = GsiConfig::new(45671, token).pinned_to(730);

        let printed = format!("{config:?}");
        assert!(
            !printed.contains(token),
            "the token must not be printed: {printed}"
        );
        assert!(
            printed.contains("<redacted>"),
            "and it says there is one: {printed}"
        );
        assert!(printed.contains("45671"), "the port is printed: {printed}");
        assert!(
            printed.contains(DEFAULT_PATH),
            "the path is printed: {printed}"
        );
        assert!(printed.contains("730"), "the app id is printed: {printed}");

        let binding = GsiGameBinding::new("/gsi/cs2", token, Some(730));
        let printed = format!("{binding:?}");
        assert!(
            !printed.contains(token),
            "a binding prints no token either: {printed}"
        );
        assert!(
            printed.contains("/gsi/cs2"),
            "the path is printed: {printed}"
        );
    }

    #[test]
    fn a_configuration_serves_the_games_it_was_given() {
        // The default: one game, on the default path, with no app id to check.
        let one = GsiConfig::new(45671, "token");
        assert_eq!(one.port, 45671);
        assert_eq!(one.games.len(), 1);
        assert_eq!(one.games[0].path, DEFAULT_PATH);
        assert_eq!(one.games[0].app_id, None);
        assert!(one.game(DEFAULT_PATH).is_some());
        assert!(one.game("/gsi/cs2").is_none());

        // Pinned: the app id of the first (and only) binding.
        let pinned = one.clone().pinned_to(730);
        assert_eq!(pinned.games[0].app_id, Some(730));
        assert_eq!(
            pinned.game(DEFAULT_PATH).expect("the game").app_id,
            Some(730)
        );

        // Two games: one path and one token each.
        let two = GsiConfig::new(45671, "cs2-token")
            .with_game("/gsi/cs2", "cs2-token", Some(730))
            .add_game("/gsi/dota2", "dota-token", Some(570));
        assert_eq!(two.games.len(), 2);
        assert_eq!(two.game("/gsi/cs2").expect("cs2").token, "cs2-token");
        assert_eq!(two.game("/gsi/dota2").expect("dota2").token, "dota-token");
        assert_eq!(two.game("/gsi/dota2").expect("dota2").app_id, Some(570));
        assert!(two.game(DEFAULT_PATH).is_none());

        // `with_game` replaces what was there; `add_game` adds to it.
        let replaced = two.with_game("/gsi/only", "only-token", None);
        assert_eq!(replaced.games.len(), 1);
        assert_eq!(replaced.games[0].path, "/gsi/only");
    }

    #[test]
    fn a_listener_that_cannot_authenticate_anything_refuses_to_start() {
        // No games at all: a listener that serves nothing answers every request with a 404.
        let err = spawn(
            GsiConfig {
                port: 0,
                games: Vec::new(),
            },
            std::sync::mpsc::channel().0,
        )
        .map(|_| ())
        .expect_err("no game");
        assert!(format!("{err:#}").contains("game"), "got: {err:#}");

        // A game without a token: nothing it posts could be authenticated.
        let err = spawn(
            GsiConfig::new(0, "token").add_game("/gsi/dota2", "", Some(570)),
            std::sync::mpsc::channel().0,
        )
        .map(|_| ())
        .expect_err("no token");
        assert!(format!("{err:#}").contains("token"), "got: {err:#}");

        // Two games on one path: they would share a route, and with it a token.
        let err = spawn(
            GsiConfig::new(0, "token").add_game(DEFAULT_PATH, "other-token", Some(730)),
            std::sync::mpsc::channel().0,
        )
        .map(|_| ())
        .expect_err("one path twice");
        assert!(format!("{err:#}").contains("path"), "got: {err:#}");
    }

    #[test]
    fn the_route_is_the_target_of_the_request_line() {
        let raw = b"POST /gsi/cs2 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n{}";
        assert_eq!(route_of(raw).as_deref(), Some("/gsi/cs2"));

        // A line that is not a request line is refused rather than guessed at.
        for raw in [
            b"POST /gsi\r\n".to_vec(),
            b"GET\r\n".to_vec(),
            b"POST  /gsi HTTP/1.1\r\n".to_vec(),
            b"garbage\r\n".to_vec(),
            b"POST /gsi HTTP/1.1".to_vec(),
        ] {
            assert_eq!(route_of(&raw), None, "{raw:?} is not a request line");
        }
    }

    fn pending(bytes: &[u8], age: Duration, now: Instant) -> Pending {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        // Keep the client end alive for the life of the test process's sockets: leaking it is
        // fine in a unit test and keeps the connection open, as a stalled peer's would be.
        std::mem::forget(client);
        Pending {
            stream: server,
            buf: bytes.to_vec(),
            arrived: now - age,
            eof: false,
        }
    }

    #[test]
    fn a_full_pending_set_gives_up_the_most_idle_connection_and_never_the_one_that_has_sent_bytes()
    {
        let now = Instant::now();
        // Sixteen stalled peers (nothing sent), the oldest first, and the game's request, which
        // arrived last but is already whole in the socket.
        let mut set: Vec<Pending> = (0..MAX_PENDING as u64)
            .map(|n| pending(b"", Duration::from_millis(1000 - n), now))
            .collect();
        set.push(pending(
            b"POST /gsi HTTP/1.1\r\n",
            Duration::from_millis(1),
            now,
        ));
        for _ in 0..MAX_PENDING {
            let victim = evict(&mut set).expect("a victim");
            assert!(victim.buf.is_empty(), "an idle connection is the victim");
        }
        assert_eq!(set.len(), 1);
        assert!(!set[0].buf.is_empty(), "the one that sent bytes survived");
        // With nothing idle left, the oldest goes.
        set.push(pending(b"x", Duration::from_millis(500), now));
        let victim = evict(&mut set).unwrap();
        assert_eq!(victim.buf, b"x");
    }

    fn job_in(lane_queue: &Queue, lane: Lane, seq: u64) -> Result<(), Job> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        std::mem::forget(client);
        lane_queue.push(
            lane,
            Job {
                stream: server,
                request: Vec::new(),
                seq,
            },
        )
    }

    #[test]
    fn each_lane_has_its_own_bound_and_the_games_lane_is_served_first() {
        let queue = Queue::new();
        for n in 0..MAX_QUEUED as u64 {
            assert!(job_in(&queue, Lane::Anonymous, n).is_ok());
        }
        assert!(
            job_in(&queue, Lane::Anonymous, 99).is_err(),
            "the anonymous lane is full"
        );
        // A full anonymous lane does not touch the game's.
        for n in 0..MAX_QUEUED as u64 {
            assert!(job_in(&queue, Lane::Trusted, 100 + n).is_ok());
        }
        assert!(job_in(&queue, Lane::Trusted, 199).is_err());
        // The game's lane first, then the rest, each in order.
        let order: Vec<u64> = std::iter::from_fn(|| {
            let mut state = lock(&queue.state);
            state
                .trusted
                .pop_front()
                .or_else(|| state.anonymous.pop_front())
                .map(|job| job.seq)
        })
        .collect();
        assert_eq!(order, vec![100, 101, 102, 103, 0, 1, 2, 3]);
        // Closed: nothing more is taken, and a reader waiting is released.
        queue.close();
        assert!(job_in(&queue, Lane::Trusted, 1).is_err());
        assert!(queue.pop().is_none());
    }

    #[test]
    fn a_request_is_in_the_games_lane_only_when_it_carries_that_paths_token() {
        let config = GsiConfig::new(0, "tok-cs2")
            .with_game("/gsi/cs2", "tok-cs2", Some(730))
            .add_game("/gsi/dota2", "tok-dota", Some(570));
        let request = |path: &str, body: &str| {
            let raw = format!(
                "POST {path} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let framed = http::frame(raw.as_bytes());
            let http::Framing::Complete { body } = framed else {
                panic!("a whole request: {framed:?}")
            };
            lane_of(&config, raw.as_bytes(), body)
        };
        let auth =
            |token: &str| format!(r#"{{"provider":{{"appid":730}},"auth":{{"token":"{token}"}}}}"#);
        assert_eq!(request("/gsi/cs2", &auth("tok-cs2")), Lane::Trusted);
        assert_eq!(request("/gsi/dota2", &auth("tok-dota")), Lane::Trusted);
        // Another game's token on this path, a wrong token, none, an unknown path.
        assert_eq!(request("/gsi/cs2", &auth("tok-dota")), Lane::Anonymous);
        assert_eq!(request("/gsi/cs2", &auth("tok-cs3")), Lane::Anonymous);
        assert_eq!(request("/gsi/cs2", "{}"), Lane::Anonymous);
        assert_eq!(request("/gsi/other", &auth("tok-cs2")), Lane::Anonymous);
    }
}
