//! The poll loop: one `GET` a second, the tracker, and the logging discipline.
//!
//! Poll about once a second, yet do not report "no game" as an error every second:
//!
//! * one poll a second, always, and a game's start is noticed within about that. What a poll
//!   costs when nothing listens depends on the platform: macOS and Linux refuse a closed
//!   loopback port at once, but Windows retries the connection and gives up only after about two
//!   seconds ([`crate::lol::client::CONNECT_TIMEOUT`] is the bound), which surfaces as a
//!   timeout, not a refusal. Both are the ordinary "no game" answer
//!   ([`crate::lol::FetchError::is_no_game`]), and on Windows a poll with no game therefore takes
//!   about two seconds, so the effective period there is the interval plus that, not one second;
//! * a condition that lasts is logged once, at the level it deserves, and its end is logged
//!   too. Nothing listening on `127.0.0.1:2999` is [`tracing::debug`], the normal state of a
//!   machine with no League game (or no League);
//! * a malformed answer (non-JSON body, an unexpected status) is a [`tracing::warn`], once per
//!   run, and does not end the game: one bad poll is not evidence the game stopped, so the
//!   tracker keeps what it knew.
//!
//! The same two seconds bound shutdown. [`LolHandle::stop`] waits for a poll in flight, and a
//! poll can be sitting in that connect, so stopping the poller can take about that long on
//! Windows when no game is running (a game that is running answers at once). The wait is bounded
//! by the client's timeouts, never open-ended.
//!
//! Derived events are logged as derived (`debug` each, `info` for transitions) and handed to
//! the sink. The poller does not decide what they are worth: a driver does
//! ([`crate::EventKind::is_highlight`]), since only it knows whether it can clip.

use crate::lol::client::{FetchError, LoopbackClient, Response};
use crate::lol::derive::Tracker;
use crate::lol::endpoint::Endpoint;
use crate::{EventSink, GameEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the Live Client is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The longest a `stop` waits for a sleeping poll loop, and the granularity at which the loop
/// notices a stop request.
const STOP_SLICE: Duration = Duration::from_millis(50);

/// Everything the poller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LolConfig {
    pub endpoint: Endpoint,
    /// The gap between polls: [`POLL_INTERVAL`] in production; a test polls as fast as its mock
    /// replies.
    pub interval: Duration,
}

impl LolConfig {
    /// The real configuration: Riot's endpoint, polled once a second.
    pub fn live_client() -> Self {
        Self {
            endpoint: Endpoint::live_client(),
            interval: POLL_INTERVAL,
        }
    }
}

impl Default for LolConfig {
    fn default() -> Self {
        Self::live_client()
    }
}

/// The poller: a client, a tracker, and the memory of what it has already complained about.
pub struct LolPoller {
    client: LoopbackClient,
    tracker: Tracker,
    interval: Duration,
    quiet: Quiet,
}

impl LolPoller {
    pub fn new(config: LolConfig) -> Result<Self, FetchError> {
        let client = LoopbackClient::for_endpoint(config.endpoint.clone())?;
        Ok(Self::with_client(config, client))
    }

    /// As [`Self::new`], trusting `root_pem` instead of Riot's root — how tests reach a mock.
    pub fn with_root(config: LolConfig, root_pem: &[u8]) -> Result<Self, FetchError> {
        let client = LoopbackClient::for_endpoint_with_root(config.endpoint.clone(), root_pem)?;
        Ok(Self::with_client(config, client))
    }

    fn with_client(config: LolConfig, client: LoopbackClient) -> Self {
        Self {
            client,
            tracker: Tracker::default(),
            interval: config.interval,
            quiet: Quiet::default(),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.client.endpoint()
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether a game is being played, as of the last poll.
    pub fn in_game(&self) -> bool {
        self.tracker.in_game()
    }

    /// Poll once and derive. Never fails: a poll that produced no document is a log line and an
    /// empty result, so no caller has to invent its own retry-and-keep-state policy.
    pub fn poll_once(&mut self) -> Vec<GameEvent> {
        let document = match self.client.get() {
            Ok(Response::Body(body)) => {
                if let Some(condition) = self.quiet.recovered() {
                    tracing::info!(
                        "the League Live Client is answering normally again ({condition})"
                    );
                }
                match crate::lol::payload::parse(&body) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        // A body we cannot read is a warning, once per run, and the tracker is left alone: one
                        // unreadable poll is not the end of a game.
                        if self.quiet.note(Condition::Unreadable(format!("{err}"))) {
                            tracing::warn!(
                                "the League Live Client answered a body replayed cannot read \
                                 ({err}); still watching"
                            );
                        }
                        return Vec::new();
                    }
                }
            }
            // The endpoint is up and says there is no game. That is also the end-of-game signal, so it
            // goes to the tracker, but it is not news.
            Ok(Response::NotServing) => {
                self.note_no_game("the endpoint reports no game");
                None
            }
            Err(err) if err.is_no_game() => {
                self.note_no_game(&format!("{err}"));
                None
            }
            Err(err) => {
                if self.quiet.note(Condition::Unreadable(format!("{err}"))) {
                    tracing::warn!(
                        "the League Live Client could not be read ({err}); still watching"
                    );
                }
                return Vec::new();
            }
        };

        let events = self.tracker.observe(document.as_ref());
        for event in &events {
            match event.kind {
                crate::EventKind::GameStart => tracing::info!(
                    "League game detected on {}: watching for events ({} already in progress \
                     were adopted, not emitted)",
                    self.tracker.attached_map(),
                    self.tracker.adopted_on_attach()
                ),
                crate::EventKind::GameEnd => {
                    tracing::info!("the League game is over");
                }
                kind => tracing::debug!("League event: {kind}"),
            }
        }
        events
    }

    /// Record that the poll found no game, at `debug` and once per run of it.
    fn note_no_game(&mut self, detail: &str) {
        if self.quiet.note(Condition::NoGame(detail.to_string())) {
            tracing::debug!(
                "no League game is being served ({detail}); this is the normal state when no \
                 game is running"
            );
        }
    }
}

/// A running poller, on its own thread. Dropping it stops the thread and waits for it, so no
/// poll is in flight afterwards and a stopped application leaves no thread behind.
pub struct LolHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    endpoint: Endpoint,
}

impl LolHandle {
    /// The endpoint being polled, for the log line a front-end prints at startup.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Stop polling and wait for the thread. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // A poll in flight finishes (bounded by the client's timeouts); the join makes "stopped" real.
            let _ = thread.join();
        }
    }
}

impl Drop for LolHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start polling. Every derived event goes to `sink` until the handle is dropped.
///
/// The sink keeps the poller out of the clipping business. A driver that stops caring drops
/// the receiver, and the thread notices and returns.
pub fn spawn(config: LolConfig, sink: EventSink) -> anyhow::Result<LolHandle> {
    let mut poller = LolPoller::new(config)?;
    let endpoint = poller.endpoint().clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    tracing::info!(
        "polling the League Live Client at {} once a second",
        endpoint.url()
    );

    let thread = std::thread::Builder::new()
        .name("replayed-lol".to_string())
        .spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let started = Instant::now();
                for event in poller.poll_once() {
                    // A disconnected sink means the driver is gone (shutting down); nothing is left to poll for.
                    if sink.send(event).is_err() {
                        tracing::debug!("the League poller stopped: nothing is receiving events");
                        return;
                    }
                }
                // The interval is a floor, not a wait: a poll that overran starts the next straight away.
                sleep_until(started + poller.interval(), &flag);
            }
            tracing::debug!("the League poller stopped");
        })
        .map_err(|e| anyhow::anyhow!("spawning the League poller thread: {e}"))?;

    Ok(LolHandle {
        stop,
        thread: Some(thread),
        endpoint,
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

/// What the poller has already complained about.
///
/// A condition is stated when it appears and when it changes, and its run length is not
/// repeated, so a one-second poll does not write 3600 identical lines an hour while a real
/// failure stays visible.
#[derive(Debug, Default)]
struct Quiet {
    active: Option<(Condition, u64)>,
}

/// A condition worth one line. `NoGame` is debug (the normal state), `Unreadable` a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Condition {
    NoGame(String),
    Unreadable(String),
}

impl std::fmt::Display for Condition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Condition::NoGame(detail) => write!(f, "no game: {detail}"),
            Condition::Unreadable(detail) => write!(f, "unreadable: {detail}"),
        }
    }
}

impl Quiet {
    /// Note a condition. Answers whether it is worth a line: the first of its kind, or a change.
    fn note(&mut self, condition: Condition) -> bool {
        match &mut self.active {
            Some((active, runs)) if *active == condition => {
                *runs += 1;
                false
            }
            Some(_) => {
                self.active = Some((condition, 1));
                true
            }
            None => {
                self.active = Some((condition, 1));
                true
            }
        }
    }

    /// The poll answered: end whatever condition was being reported.
    fn recovered(&mut self) -> Option<Condition> {
        self.active.take().map(|(condition, _)| condition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;

    /// The poller's own log discipline, without a socket: these tests only exercise `Quiet`.
    #[test]
    fn a_condition_that_lasts_is_reported_once() {
        let mut quiet = Quiet::default();
        assert!(
            quiet.note(Condition::NoGame("refused".into())),
            "the first is news"
        );
        for _ in 0..10 {
            assert!(
                !quiet.note(Condition::NoGame("refused".into())),
                "and the rest are not"
            );
        }
        assert_eq!(quiet.recovered(), Some(Condition::NoGame("refused".into())));
        assert_eq!(quiet.recovered(), None, "a recovery is reported once");
    }

    #[test]
    fn a_changed_condition_is_reported_again() {
        let mut quiet = Quiet::default();
        assert!(quiet.note(Condition::NoGame("refused".into())));
        assert!(
            quiet.note(Condition::Unreadable("expected value".into())),
            "a different condition is worth a line even while one was being reported"
        );
        assert!(!quiet.note(Condition::Unreadable("expected value".into())));
    }

    #[test]
    fn the_production_configuration_is_the_endpoint_the_spec_names() {
        let config = LolConfig::live_client();
        assert_eq!(
            config.endpoint.url(),
            "https://127.0.0.1:2999/liveclientdata/allgamedata"
        );
        assert_eq!(config.interval, POLL_INTERVAL);
        assert_eq!(
            LolConfig::default(),
            config,
            "the default is the production one"
        );
    }

    #[test]
    fn a_poller_can_be_pointed_at_a_loopback_mock_and_nowhere_else() {
        let addr: std::net::SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let endpoint = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let poller = LolPoller::new(LolConfig {
            endpoint,
            interval: Duration::from_millis(10),
        })
        .expect("a client for a loopback mock");
        assert!(poller.endpoint().is_loopback());
        assert!(!poller.in_game(), "a fresh poller knows nothing");
    }

    #[test]
    fn polling_something_that_is_not_listening_is_empty_and_does_not_touch_the_state() {
        // Port 1 on loopback is refused: the between-games case must be an empty result, not an
        // error, and must not invent a GameEnd for a game never seen.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        let endpoint = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let mut poller = LolPoller::new(LolConfig {
            endpoint,
            interval: POLL_INTERVAL,
        })
        .unwrap();
        for _ in 0..3 {
            assert!(poller.poll_once().is_empty());
        }
        assert!(!poller.in_game());
        assert!(EventKind::GameEnd.is_highlight());
    }
}
