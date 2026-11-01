//! The GSI listener over real loopback sockets.
//!
//! The listener is real (bound socket, request gate, token comparison, derivation) and driven
//! by a hand-written HTTP client. The only stand-in is the game: no CS2 or Dota 2 client was
//! run, so the real-client check is still outstanding. Every socket is on `127.0.0.1` and
//! every payload is a canned fixture.
//!
//! Asserted:
//! * a POST with the configured token is accepted and yields the expected events;
//! * a wrong token, or no auth block, is rejected and yields nothing (fail closed);
//! * a wrong token is rejected *before* the body is parsed, however large the body is;
//! * a malformed body yields nothing, does not panic and does not stop the listener;
//! * the game is served while a peer holds a connection open, however many peers do (they
//!   cost the listener a slot, never a reader), and while anonymous peers flood it;
//! * a request a web page made is refused whether or not it carries the token;
//! * a payload whose `auth` block comes last, behind a large scoreboard, is heard;
//! * a listener pinned to one game refuses the other game's payload;
//! * the listener binds loopback only and refuses anything else.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use replayed_events::gsi::{self, GsiConfig, GsiListener};
use replayed_events::{EventKind, GameEvent};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const CS2_COMPETITIVE: &str = include_str!("fixtures/gsi_cs2_competitive.json");
const CS2_MENU: &str = include_str!("fixtures/gsi_cs2_menu.json");
const DOTA_IN_GAME: &str = include_str!("fixtures/gsi_dota_in_game.json");

/// The token the fixtures carry. Not a secret; every rejection case posts something else.
const TOKEN: &str = "fixture-token-not-a-secret";

/// A listener plus the channel its events arrive on.
struct Harness {
    listener: GsiListener,
    events: Receiver<GameEvent>,
}

impl Harness {
    fn start() -> Self {
        Self::start_with(GsiConfig::new(0, TOKEN))
    }

    /// A listener pinned to one game: the app id the payload has to name.
    fn pinned_to(app_id: u32) -> Self {
        Self::start_with(GsiConfig::new(0, TOKEN).pinned_to(app_id))
    }

    fn start_with(config: GsiConfig) -> Self {
        let (sink, events) = channel();
        let listener = gsi::spawn(config, sink).expect("the listener starts on a free port");
        Self { listener, events }
    }

    fn addr(&self) -> SocketAddr {
        self.listener.local_addr()
    }

    /// Every event the listener derived, within a short window.
    fn drained(&self) -> Vec<GameEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.recv_timeout(Duration::from_millis(400)) {
            out.push(event);
        }
        out
    }
}

fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind).collect()
}

/// Send raw bytes and read whatever comes back.
fn raw_request(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("a read timeout");
    stream.write_all(request).expect("the request is sent");
    stream.flush().expect("flushed");
    let mut raw = Vec::new();
    // The listener answers and closes, so reading to the end terminates.
    stream.read_to_end(&mut raw).expect("the response is read");
    String::from_utf8_lossy(&raw).to_string()
}

/// A well-formed POST to `path` with `body`.
fn post(addr: SocketAddr, path: &str, body: &str) -> String {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    raw_request(addr, request.as_bytes())
}

fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"))
}

/// A document with a token: the fixture, with its token replaced.
fn with_token(fixture: &str, token: &str) -> String {
    fixture.replace(TOKEN, token)
}

/// A document with no `auth` block at all, as a cfg file without one produces.
fn without_auth(fixture: &str) -> String {
    let start = fixture
        .find("\"auth\"")
        .expect("the fixture has an auth block");
    let end = fixture[start..].find("},").expect("the block ends") + start + 2;
    fixture.replace(&fixture[start..end], "")
}

#[test]
fn the_listener_is_bound_to_loopback_and_answers_on_the_configured_path() {
    let harness = Harness::start();
    let addr = harness.addr();
    assert!(addr.ip().is_loopback(), "bound to {addr}");
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0, "the OS assigned a real port");

    // A wrong path, and a method that is not POST, are refused by status.
    assert_eq!(status_of(&post(addr, "/", CS2_COMPETITIVE)), 404);
    assert_eq!(
        status_of(&post(addr, "/liveclientdata/allgamedata", CS2_COMPETITIVE)),
        404
    );
    let get = format!("GET /gsi HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    assert_eq!(status_of(&raw_request(addr, get.as_bytes())), 405);
    assert!(
        harness.drained().is_empty(),
        "nothing was derived from any of those"
    );
}

#[test]
fn a_post_with_the_configured_token_is_accepted_and_derives_the_expected_events() {
    let harness = Harness::start();
    let addr = harness.addr();

    // 1. First payload of a match: an attach. Its scoreboard (12 kills, 5 deaths, 3 assists) is
    //    adopted, not reported.
    let response = post(addr, "/gsi", CS2_COMPETITIVE);
    assert_eq!(
        status_of(&response),
        200,
        "the token is the configured one: {response}"
    );
    let attach = harness.drained();
    assert_eq!(kinds(&attach), vec![EventKind::GameStart]);
    assert_eq!(attach[0].source.as_str(), "gsi");
    assert!(
        attach[0]
            .payload
            .as_deref()
            .unwrap_or("")
            .contains("\"map\":\"de_dust2\""),
        "the attach records which map: {:?}",
        attach[0].payload
    );

    // 2. A heartbeat that repeats the state exactly: nothing.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert!(
        harness.drained().is_empty(),
        "a repeated payload is not an event"
    );

    // 3. Round 5 ends, and the next round begins with a kill for the player.
    let mut next: serde_json::Value =
        serde_json::from_str(CS2_COMPETITIVE).expect("the fixture is JSON");
    next["round"]["phase"] = serde_json::json!("over");
    next["round"]["win_team"] = serde_json::json!("CT");
    let round_end = post(addr, "/gsi", &next.to_string());
    assert_eq!(status_of(&round_end), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::RoundEnd]);

    next["round"]["phase"] = serde_json::json!("freezetime");
    next["map"]["round"] = serde_json::json!(6);
    next["map"]["team_ct"]["score"] = serde_json::json!(4);
    next["bomb"]["state"] = serde_json::json!("carried");
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::RoundStart]);

    next["round"]["phase"] = serde_json::json!("live");
    next["player"]["match_stats"]["kills"] = serde_json::json!(13);
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    let kill = harness.drained();
    assert_eq!(kinds(&kill), vec![EventKind::Kill]);
    let payload = kill[0].payload.as_deref().unwrap_or("");
    assert!(payload.contains("\"total\":13"), "got {payload}");
    assert!(payload.contains("\"round\":6"), "got {payload}");

    // 4. The match ends.
    next["map"]["phase"] = serde_json::json!("gameover");
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameEnd]);
}

#[test]
fn a_post_with_a_wrong_token_is_rejected_and_derives_nothing() {
    let harness = Harness::start();
    let addr = harness.addr();

    // A token one character different, shorter, longer, or empty: all refused, none advances the
    // game state.
    for wrong in [
        "fixture-token-not-a-secreu",
        "fixture-token-not-a-secret ",
        "fixture-token-not-a-secretx",
        "",
    ] {
        let body = with_token(CS2_COMPETITIVE, wrong);
        let response = post(addr, "/gsi", &body);
        assert_eq!(status_of(&response), 403, "token {wrong:?} must be refused");
        assert!(
            !response.contains(TOKEN),
            "the response must not echo the configured token: {response}"
        );
    }
    assert!(
        harness.drained().is_empty(),
        "a refused payload derives nothing"
    );
}

#[test]
fn a_post_with_no_auth_block_at_all_is_rejected() {
    let harness = Harness::start();
    let addr = harness.addr();

    let body = without_auth(CS2_COMPETITIVE);
    assert!(
        !body.contains("auth"),
        "the fixture really has no auth block: {body}"
    );
    assert_eq!(status_of(&post(addr, "/gsi", &body)), 403);
    assert!(harness.drained().is_empty());

    // A rejection is not a broken listener: a good payload is still served.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_malformed_body_yields_nothing_and_does_not_stop_the_listener() {
    let harness = Harness::start();
    let addr = harness.addr();

    for body in ["not json at all", "{", "[]", ""] {
        let response = post(addr, "/gsi", body);
        assert_eq!(
            status_of(&response),
            400,
            "{body:?} must be refused, got {response}"
        );
    }

    // A document that carries no token is refused as unauthorised, whatever else is wrong
    // with it: the token gate comes before the document is parsed, so the answer is 403
    // rather than 400. What matters either way is that nothing is derived.
    for body in [
        r#"{"provider": {"appid": 730}, "map": {"round": "five"}}"#,
        r#"{"auth": {"kind": "cs2"}}"#,
    ] {
        let response = post(addr, "/gsi", body);
        assert_eq!(
            status_of(&response),
            403,
            "{body:?} must be refused, got {response}"
        );
    }

    // A document that carries the right token and is broken anyway is malformed: now that
    // the token has been accepted, the body is worth parsing.
    for body in [
        r#"{"auth":{"token":"fixture-token-not-a-secret""#, // truncated mid-token
        r#"{"auth":{"token":"fixture-token-not-a-secret"},"map":{"round":"five"}}"#,
    ] {
        let response = post(addr, "/gsi", body);
        assert_eq!(
            status_of(&response),
            400,
            "{body:?} must be refused, got {response}"
        );
    }
    assert!(harness.drained().is_empty(), "no payload derived anything");

    // The listener survived all of that and still works — the point of the test.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_body_with_no_declared_length_is_refused_before_it_is_read() {
    let harness = Harness::start();
    let addr = harness.addr();

    let request = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
        CS2_COMPETITIVE
    );
    assert_eq!(status_of(&raw_request(addr, request.as_bytes())), 411);
    assert!(harness.drained().is_empty());

    let chunked = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n"
    );
    assert_eq!(
        status_of(&raw_request(addr, chunked.as_bytes())),
        411,
        "GSI sends a length"
    );
}

#[test]
fn an_over_long_body_is_refused_and_a_shorter_one_than_claimed_is_too() {
    let harness = Harness::start();
    let addr = harness.addr();

    let huge = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        gsi::http::MAX_BODY_BYTES + 1
    );
    assert_eq!(status_of(&raw_request(addr, huge.as_bytes())), 413);

    let lying = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{{}}"
    );
    assert_eq!(status_of(&raw_request(addr, lying.as_bytes())), 413);

    assert!(harness.drained().is_empty());
}

#[test]
fn a_dota_payload_is_derived_from_its_own_scoreboard() {
    let harness = Harness::start();
    let addr = harness.addr();

    assert_eq!(status_of(&post(addr, "/gsi", DOTA_IN_GAME)), 200);
    let attach = harness.drained();
    assert_eq!(kinds(&attach), vec![EventKind::GameStart]);
    let payload = attach[0].payload.as_deref().unwrap_or("");
    assert!(payload.contains("\"game\":\"dota2\""), "got {payload}");
    assert!(payload.contains("\"adopted_scoreboard\""), "got {payload}");

    let mut next: serde_json::Value = serde_json::from_str(DOTA_IN_GAME).expect("JSON");
    next["player"]["kills"] = serde_json::json!(5);
    next["player"]["deaths"] = serde_json::json!(3);
    assert_eq!(status_of(&post(addr, "/gsi", &next.to_string())), 200);
    assert_eq!(
        kinds(&harness.drained()),
        vec![EventKind::Kill, EventKind::Death]
    );
}

#[test]
fn the_main_menu_never_produces_an_event() {
    // A client in a menu posts the same shape as a match not yet started; it must never be taken
    // for one, nor end a game.
    let harness = Harness::start();
    let addr = harness.addr();

    for _ in 0..3 {
        assert_eq!(status_of(&post(addr, "/gsi", CS2_MENU)), 200);
    }
    assert!(harness.drained().is_empty());

    // A match starts, and the menu afterwards ends it — once.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
    for _ in 0..3 {
        assert_eq!(status_of(&post(addr, "/gsi", CS2_MENU)), 200);
    }
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameEnd]);
}

#[test]
fn a_listener_started_without_a_token_refuses_to_start() {
    let (sink, _events) = channel();
    let err = gsi::spawn(GsiConfig::new(0, ""), sink)
        .map(|_| ())
        .expect_err("no token");
    assert!(format!("{err:#}").contains("token"), "got: {err:#}");
    assert_eq!(with_token(CS2_COMPETITIVE, TOKEN), CS2_COMPETITIVE);
}

#[test]
fn a_flood_of_connections_is_answered_429_and_the_listener_recovers() {
    let harness = Harness::start();
    let addr = harness.addr();
    // Well over the per-second cap, from several threads so it lands in one window. Every
    // connection is answered, the excess with 429, and none becomes an event.
    let total = gsi::MAX_REQUESTS_PER_WINDOW as usize * 3;
    let workers: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                (0..total / 8)
                    .map(|_| status_of(&post(addr, "/gsi", "{}")))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let statuses: Vec<u16> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    assert!(statuses.contains(&429), "the excess is refused with 429");
    assert!(
        statuses.iter().all(|s| *s == 403 || *s == 429),
        "and nothing else is served"
    );
    assert!(harness.drained().is_empty(), "a flood derives no events");

    // The window rolls over, and the listener serves again.
    std::thread::sleep(gsi::RATE_WINDOW + Duration::from_millis(100));
    assert_eq!(
        status_of(&post(addr, "/gsi", "{}")),
        403,
        "served again (403: no token)"
    );
}

#[test]
fn a_wrong_token_is_refused_before_the_body_is_parsed() {
    // The body is a megabyte of JSON that the document parser would reject: the round is
    // a string where a number belongs, so a parse-first gate answers 400. The token gate
    // comes first, so the answer is 403 and the body is never turned into a document.
    //
    // This is the difference between a peer that can make the recorder parse megabytes
    // and one that cannot: a web page in the user's browser can POST here with a
    // `text/plain` content type, which is a CORS "simple request" and is not preflighted.
    let harness = Harness::start();
    let addr = harness.addr();

    let mut body =
        String::from(r#"{"auth":{"token":"not-the-token"},"map":{"round":"five"},"junk":["#);
    while body.len() < 1024 * 1024 {
        body.push_str(r#""yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy","#);
    }
    body.push_str(r#"""]}"#);

    let started = Instant::now();
    let response = post(addr, "/gsi", &body);
    let elapsed = started.elapsed();
    assert_eq!(
        status_of(&response),
        403,
        "the token is checked first, so the body is never parsed: {response}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "and the refusal is prompt, not the cost of a parse: {elapsed:?}"
    );
    assert!(
        harness.drained().is_empty(),
        "a refused payload derives nothing"
    );

    // The listener is unharmed: the game still gets through.
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn the_game_is_served_while_a_peer_holds_a_connection_open() {
    // A peer that opens a connection and then says nothing holds a reader for the whole
    // request deadline. The accept loop used to serve connections one at a time, so a
    // handful of such connections kept the game's own POST from being accepted at all:
    // the recorder kept running and the feature it exists for was silent.
    let harness = Harness::start();
    let addr = harness.addr();

    let mut held = Vec::new();
    // More than every reader, every queue slot and every pending slot: eight was enough to starve the
    // listener it replaced, and a peer that opens sockets is not limited to eight.
    for _ in 0..(gsi::WORKERS + 2 * gsi::MAX_QUEUED + gsi::MAX_PENDING) * 3 {
        let mut stream = TcpStream::connect(addr).expect("loopback connects");
        // A head that never ends: the reader waits for the rest of it.
        stream
            .write_all(b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            .expect("the head is sent");
        held.push(stream);
    }
    // Idle sockets too: connected, and nothing sent at all.
    for _ in 0..gsi::MAX_PENDING * 2 {
        held.push(TcpStream::connect(addr).expect("loopback connects"));
    }
    // Give the listener a moment to take the connections.
    std::thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    let response = post(addr, "/gsi", CS2_COMPETITIVE);
    let elapsed = started.elapsed();
    assert_eq!(status_of(&response), 200, "the game is served: {response}");
    assert!(
        elapsed < Duration::from_secs(1),
        "the game waited {elapsed:?} for a reader; a held connection must not hold the game"
    );
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);

    // The held connections are closed, so the readers they occupy are free again.
    drop(held);
}

#[test]
fn anonymous_traffic_cannot_spend_the_games_share() {
    // The limiter used to count every connection, so more than 100 a second from anyone
    // answered the game with 429. Now only requests that do not carry the token are counted,
    // and the game's lane is its own. Hammer the port from several threads with token-less
    // POSTs for the whole test, and have the game post throughout.
    let harness = Harness::start();
    let addr = harness.addr();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flooders: Vec<_> = (0..6)
        .map(|_| {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut sent = 0u32;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = TcpStream::connect(addr).and_then(|mut stream| {
                        stream.write_all(b"POST /gsi HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")?;
                        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                        let mut sink = [0u8; 64];
                        let _ = stream.read(&mut sink);
                        Ok(())
                    });
                    sent += 1;
                    // Paced, not unbounded: a tight connect loop exhausts Windows' ephemeral ports
                    // (and their `TIME_WAIT`) within seconds, and then the *game's* connect fails
                    // with `AddrInUse` in the test process, which says nothing about the listener.
                    // Several hundred connections a second is still several times the 100-a-second
                    // budget the limiter has.
                    std::thread::sleep(Duration::from_millis(2));
                }
                sent
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(200));

    let started = Instant::now();
    let mut statuses = Vec::new();
    while started.elapsed() < Duration::from_millis(2500) {
        statuses.push(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)));
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let sent: u32 = flooders.into_iter().map(|f| f.join().unwrap()).sum();
    assert!(
        sent > gsi::MAX_REQUESTS_PER_WINDOW * 2,
        "the flood really exceeded the anonymous budget ({sent} requests)"
    );
    assert!(statuses.len() > 20, "the game posted throughout");
    assert!(
        statuses.iter().all(|s| *s == 200),
        "every one of the game's POSTs was served, none was a 429: {statuses:?}"
    );
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_request_split_across_packets_is_still_the_games_request() {
    // The listener collects a request without blocking, so a head and a body that arrive
    // apart (the game's HTTP client may write them separately) must still be one request.
    let harness = Harness::start();
    let addr = harness.addr();
    let body = CS2_COMPETITIVE;
    let head = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(head.as_bytes()).unwrap();
    stream.flush().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let (first, second) = body.as_bytes().split_at(body.len() / 2);
    stream.write_all(first).unwrap();
    stream.flush().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    stream.write_all(second).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    assert_eq!(status_of(&String::from_utf8_lossy(&raw)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_stalled_request_is_answered_at_the_deadline_and_holds_nothing() {
    let harness = Harness::start();
    let addr = harness.addr();
    let mut stalled = TcpStream::connect(addr).unwrap();
    stalled
        .set_read_timeout(Some(gsi::REQUEST_TIMEOUT * 3))
        .unwrap();
    stalled
        .write_all(b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 50\r\n\r\n{}")
        .unwrap();
    let started = Instant::now();
    let mut raw = Vec::new();
    stalled.read_to_end(&mut raw).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        status_of(&String::from_utf8_lossy(&raw)),
        413,
        "a body shorter than it claims"
    );
    assert!(
        elapsed >= gsi::REQUEST_TIMEOUT - Duration::from_millis(200)
            && elapsed < gsi::REQUEST_TIMEOUT + Duration::from_secs(1),
        "answered at the deadline, not before and not long after: {elapsed:?}"
    );
}

#[test]
fn a_request_a_web_page_made_is_refused_even_with_the_right_token() {
    // `fetch(..., {mode: 'no-cors'})` from any page sends Sec-Fetch-* headers that the page
    // cannot remove. The game never does, so these are refused outright and cost no budget.
    let harness = Harness::start();
    let addr = harness.addr();
    let body = CS2_COMPETITIVE;
    let request = format!(
        "POST /gsi HTTP/1.1\r\nHost: {addr}\r\nOrigin: https://example.test\r\n\
         Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: empty\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(status_of(&raw_request(addr, request.as_bytes())), 403);
    assert!(
        harness.drained().is_empty(),
        "a page's request derives nothing, token or not"
    );
    // Many of them leave the game's budget alone.
    for _ in 0..gsi::MAX_REQUESTS_PER_WINDOW * 2 {
        assert_eq!(status_of(&raw_request(addr, request.as_bytes())), 403);
    }
    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
}

#[test]
fn a_payload_whose_auth_block_is_last_behind_a_large_scoreboard_is_heard() {
    // The game writes `auth` after the other blocks. The token probe used to read 64 KiB and
    // stop, so a payload bigger than that was answered 400 and the game went silent.
    let harness = Harness::start();
    let addr = harness.addr();
    let mut body = without_auth(CS2_COMPETITIVE);
    let close = body.rfind('}').expect("a JSON object");
    let mut padding = String::from(r#","allgrenades":["#);
    while padding.len() < 200 * 1024 {
        padding.push_str(r#"{"type":"flashbang","owner":"76561198000000000"},"#);
    }
    padding.push_str(r#"{}]"#);
    body.truncate(close);
    body.push_str(&padding);
    body.push_str(&format!(r#","auth":{{"token":"{TOKEN}"}}}}"#));
    assert!(body.len() > 64 * 1024 && body.find("\"auth\"").unwrap() > 64 * 1024);

    assert_eq!(status_of(&post(addr, "/gsi", &body)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);
}

#[test]
fn a_listener_pinned_to_one_game_refuses_a_payload_from_another() {
    // The token travels in the game's `cfg` directory, where any local account can read
    // it. Pinning the listener to one game means a token lifted from the other game's
    // installed file cannot forge this one's events.
    let harness = Harness::pinned_to(730);
    let addr = harness.addr();

    assert_eq!(status_of(&post(addr, "/gsi", CS2_COMPETITIVE)), 200);
    assert_eq!(kinds(&harness.drained()), vec![EventKind::GameStart]);

    assert_eq!(
        status_of(&post(addr, "/gsi", DOTA_IN_GAME)),
        403,
        "a Dota payload is not this listener's game"
    );
    assert!(
        harness.drained().is_empty(),
        "and it derives nothing: the scoreboard in it is not the watched player's"
    );
}
