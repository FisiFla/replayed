//! The League poller against a local HTTPS mock server: no game, no Riot, no network.
//!
//! The mock is a real TLS server on `127.0.0.1` with a self-signed certificate, the shape of
//! Riot's Live Client Data API. The whole path runs for real: relaxed TLS handshake,
//! hand-written request, response framing, payload parse, derivation, logging discipline and
//! sink.
//!
//! It proves what cannot otherwise be proved without a real client: the relaxed client accepts
//! a certificate nothing has signed, and a client with verification refuses the same
//! certificate. Both halves are asserted, so the relaxation is demonstrated behaviour.
//!
//! Not proven: that Riot's payloads look like the canned ones. They are shaped from the
//! documented API, no League client was contacted, and that check is still outstanding.
//!
//! Every socket is loopback and the poller only issues `GET`s to an in-process responder.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use native_tls::{Identity, TlsAcceptor, TlsConnector};
use replayed_events::lol::{Endpoint, LolConfig, LolPoller, Response};
use replayed_events::{EventKind, GameEvent};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A throwaway CA plus a leaf it signed, generated per mock so no private key is committed.
/// P-256 keys come from pure-Rust `p256` (see the workspace `rcgen` note) and are handed to
/// rcgen through its `SigningKey` trait.
mod tls {
    use p256::ecdsa::{signature::Signer, DerSignature, SigningKey};
    use rand_core::RngCore;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer,
        KeyUsagePurpose, PublicKeyData, SanType, SerialNumber, SignatureAlgorithm,
        SigningKey as RcgenSigningKey, PKCS_ECDSA_P256_SHA256, PKCS_RSA_SHA256,
    };
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::EncodePrivateKey;

    struct Key {
        sk: SigningKey,
        public: Vec<u8>,
    }

    impl Key {
        fn new() -> Self {
            // `p256` 0.14 draws from `rand_core` 0.10, which carries no OS generator, while the
            // workspace still pins 0.6 for `rsa`. So entropy comes from the 0.6 generator and the
            // scalar is built from the bytes; a draw outside the curve order (or zero) is refused
            // and simply repeats.
            let mut bytes = [0u8; 32];
            let sk = loop {
                rand_core::OsRng.fill_bytes(&mut bytes);
                if let Ok(sk) = SigningKey::from_slice(&bytes) {
                    break sk;
                }
            };
            let public = sk.verifying_key().to_sec1_point(false).as_bytes().to_vec();
            Self { sk, public }
        }
    }

    impl PublicKeyData for Key {
        fn der_bytes(&self) -> &[u8] {
            &self.public
        }
        fn algorithm(&self) -> &'static SignatureAlgorithm {
            &PKCS_ECDSA_P256_SHA256
        }
    }

    impl RcgenSigningKey for Key {
        fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
            let sig: DerSignature = self.sk.sign(msg);
            Ok(sig.as_bytes().to_vec())
        }
    }

    /// The RSA leaf's public half, for rcgen to put in the leaf certificate.
    struct RsaPublic(Vec<u8>);

    impl PublicKeyData for RsaPublic {
        fn der_bytes(&self) -> &[u8] {
            &self.0
        }
        fn algorithm(&self) -> &'static SignatureAlgorithm {
            &PKCS_RSA_SHA256
        }
    }

    /// PEM material for one throwaway chain.
    pub struct Chain {
        /// The CA certificate a client pins to trust the leaf.
        pub root_pem: Vec<u8>,
        pub leaf_pem: Vec<u8>,
        /// A second leaf from the same CA and key, but naming a host that is not `127.0.0.1`: the
        /// shape of Riot's real certificate, which carries nothing matching an IP literal. Only a
        /// client that skips the host-name match accepts it.
        pub wrong_name_leaf_pem: Vec<u8>,
        /// PKCS#8 PEM, the form `native_tls::Identity::from_pkcs8` takes.
        pub leaf_key_pem: Vec<u8>,
    }

    fn ca(name: &str) -> (CertificateParams, Key, rcgen::Certificate) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, name);
        dn.push(DnType::OrganizationName, "replayed test fixture");
        params.distinguished_name = dn;
        params.serial_number = Some(SerialNumber::from(1u64));
        let key = Key::new();
        let cert = params.self_signed(&key).expect("a self-signed test CA");
        (params, key, cert)
    }

    /// A CA certificate that signed nothing: the "some other authority" a pin must refuse.
    pub fn unrelated_root() -> Vec<u8> {
        ca("unrelated test CA").2.pem().into_bytes()
    }

    /// The chain every mock in this test binary shares. RSA-2048 key generation is slow in a
    /// debug build, so it happens once per process rather than once per mock.
    pub fn chain() -> &'static Chain {
        static CHAIN: std::sync::OnceLock<Chain> = std::sync::OnceLock::new();
        CHAIN.get_or_init(|| {
            let (ca_params, ca_key, ca_cert) = ca("replayed test CA");
            let rsa = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).expect("an RSA key");
            let public = RsaPublic(
                rsa.to_public_key()
                    .to_pkcs1_der()
                    .expect("PKCS#1")
                    .as_bytes()
                    .to_vec(),
            );

            let mut leaf_params = CertificateParams::default();
            leaf_params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, "replayed test leaf");
            leaf_params.distinguished_name = dn;
            leaf_params.serial_number = Some(SerialNumber::from(2u64));
            let issuer = Issuer::from_params(&ca_params, &ca_key);
            let leaf = leaf_params
                .signed_by(&public, &issuer)
                .expect("a leaf signed by the test CA");

            let mut other = CertificateParams::default();
            other.subject_alt_names = vec![SanType::DnsName(
                "livegame.riot.invalid".try_into().unwrap(),
            )];
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, "livegame.riot.invalid");
            other.distinguished_name = dn;
            other.serial_number = Some(SerialNumber::from(3u64));
            let wrong_name = other
                .signed_by(&public, &issuer)
                .expect("a second leaf signed by the test CA");

            Chain {
                root_pem: ca_cert.pem().into_bytes(),
                leaf_pem: leaf.pem().into_bytes(),
                wrong_name_leaf_pem: wrong_name.pem().into_bytes(),
                leaf_key_pem: rsa
                    .to_pkcs8_pem(Default::default())
                    .expect("PKCS#8")
                    .as_bytes()
                    .to_vec(),
            }
        })
    }
}

const IN_PROGRESS: &str = include_str!("fixtures/lol_in_progress.json");
const EVENTS_NEXT: &str = include_str!("fixtures/lol_events_next.json");

/// A game that is present but has reported no events yet (the loading screen), for tests about
/// what happens after an attach.
const LOADING: &str = r#"{"gameData":{"gameTime":-25.0,"mapName":"Map11"}}"#;

/// What the mock answers with.
#[derive(Debug, Clone, Copy)]
enum Reply {
    /// `HTTP/1.1 200` and this body.
    Body(&'static str),
    /// `HTTP/1.1 200` and this body.
    NoGame,
}

/// A stand-in for the Live Client: a self-signed HTTPS server on loopback answering each
/// request with the next queued reply. The last reply repeats, so a test can poll more times
/// than it queued answers.
struct MockGame {
    addr: SocketAddr,
    /// The CA a client must pin to trust this mock (fresh per mock).
    root_pem: Vec<u8>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<Mutex<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl MockGame {
    fn start(replies: Vec<Reply>) -> Self {
        Self::start_with(replies, false)
    }

    /// As [`Self::start`]; with `wrong_name` the certificate served names `livegame.riot.invalid`
    /// instead of `127.0.0.1`, as Riot's does.
    fn start_with(replies: Vec<Reply>, wrong_name: bool) -> Self {
        let chain = tls::chain();
        let leaf = if wrong_name {
            &chain.wrong_name_leaf_pem
        } else {
            &chain.leaf_pem
        };
        let identity = Identity::from_pkcs8(leaf, &chain.leaf_key_pem)
            .expect("the generated certificate loads");
        let acceptor = TlsAcceptor::new(identity).expect("a TLS acceptor");
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral loopback port");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        let addr = listener.local_addr().expect("the port the OS assigned");

        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Mutex::new(false));

        let thread = {
            let queue = Arc::clone(&queue);
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("lol-mock".to_string())
                .spawn(move || {
                    while !*stop.lock().expect("the stop flag") {
                        match listener.accept() {
                            Ok((stream, _)) => serve_one(stream, &acceptor, &queue, &requests),
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
                .expect("the mock thread starts")
        };

        Self {
            addr,
            root_pem: chain.root_pem.clone(),
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::loopback(self.addr, "/liveclientdata/allgamedata").expect("a loopback endpoint")
    }

    fn poller(&self) -> LolPoller {
        LolPoller::with_root(
            LolConfig {
                endpoint: self.endpoint(),
                interval: Duration::from_millis(20),
            },
            &self.root_pem,
        )
        .expect("a client for the mock")
    }

    /// The request lines the mock has seen, so a test can assert on the wire as well as the events.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("the request log").clone()
    }
}

impl Drop for MockGame {
    fn drop(&mut self) {
        if let Ok(mut stop) = self.stop.lock() {
            *stop = true;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read one request and answer it, or do nothing if the client never finishes sending one (the
/// strict-verification case: the handshake is refused and the connection dropped).
fn serve_one(
    stream: TcpStream,
    acceptor: &TlsAcceptor,
    queue: &Arc<Mutex<VecDeque<Reply>>>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    // The listener is non-blocking so the mock can be stopped; on BSD-derived platforms an
    // accepted socket inherits that, and a non-blocking handshake fails with `WouldBlock`.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut tls = match acceptor.accept(stream) {
        Ok(tls) => tls,
        Err(_) => return,
    };
    let mut buf = [0u8; 2048];
    let read = match tls.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buf[..read]).to_string();
    if let Some(line) = request.lines().next() {
        requests
            .lock()
            .expect("the request log")
            .push(line.to_string());
    }

    let reply = {
        let mut queue = queue.lock().expect("the reply queue");
        // The last reply repeats: polling five times against one queued answer asks "and if it says
        // the same thing again?"
        if queue.len() > 1 {
            queue.pop_front().unwrap_or(Reply::NoGame)
        } else {
            queue.front().copied().unwrap_or(Reply::NoGame)
        }
    };
    let response = match reply {
        Reply::Body(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        Reply::NoGame => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        }
    };
    let _ = tls.write_all(response.as_bytes());
    let _ = tls.flush();
    // Give the client time to read before the socket closes under it.
    std::thread::sleep(Duration::from_millis(20));
}

fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind).collect()
}

fn payload_of(events: &[GameEvent], kind: EventKind) -> &str {
    events
        .iter()
        .find(|e| e.kind == kind)
        .and_then(|e| e.payload.as_deref())
        .unwrap_or_else(|| panic!("no {kind} in {events:?}"))
}

#[test]
fn the_client_trusts_only_its_pinned_root() {
    // The security property the LoL arrangement rests on, from both sides: a server whose
    // certificate chains to the pinned root is served; the same server seen by a client pinned to
    // a different root, or a stock client, is refused.
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS)]);
    let addr = mock.addr;

    // 1. A stock client — platform roots — must refuse the throwaway CA.
    let strict = TlsConnector::builder()
        .build()
        .expect("a default connector");
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("loopback connects");
    assert!(
        strict.connect("127.0.0.1", tcp).is_err(),
        "the generated CA must be untrusted by default, or this test proves nothing"
    );

    // 2. replayed's client pinned to a different root (Riot's, as in production) refuses it too:
    //    a squatter on port 2999 with some other certificate gets nothing.
    let err = replayed_events::lol::LoopbackClient::for_endpoint(mock.endpoint())
        .expect("pinned to Riot's root")
        .get()
        .expect_err("a certificate Riot's CA did not sign must be refused");
    assert!(
        matches!(err, replayed_events::lol::FetchError::Tls(..)),
        "{err}"
    );
    let other = tls::unrelated_root();
    let err = replayed_events::lol::LoopbackClient::for_endpoint_with_root(mock.endpoint(), &other)
        .expect("pinned to an unrelated root")
        .get()
        .expect_err("an unrelated root must be refused too");
    assert!(
        matches!(err, replayed_events::lol::FetchError::Tls(..)),
        "{err}"
    );

    // 3. Pinned to the signing CA, the client gets the game document (the host-name mismatch on an
    //    IP literal is the only relaxation).
    let mut poller = mock.poller();
    let events = poller.poll_once();
    assert_eq!(
        kinds(&events),
        vec![EventKind::GameStart],
        "the attach, through real TLS"
    );
    assert!(poller.in_game());
}

#[test]
fn attaching_to_a_game_already_in_progress_does_not_replay_its_history() {
    // The event storm this prevents: six events are already in the document, two of them the
    // watched player's kill and death.
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS)]);
    let mut poller = mock.poller();

    assert_eq!(
        kinds(&poller.poll_once()),
        vec![EventKind::GameStart],
        "one event — the attach — not one per event in the history"
    );

    // And every further poll of the same document is silent: the poller is idempotent.
    for poll in 1..=5 {
        assert!(
            poller.poll_once().is_empty(),
            "poll {poll} of an unchanged game must emit nothing"
        );
    }

    // The request went to the expected path, over the endpoint the poller was given.
    let requests = mock.requests();
    assert!(requests.len() >= 2, "the mock saw the polls: {requests:?}");
    assert_eq!(requests[0], "GET /liveclientdata/allgamedata HTTP/1.1");
}

#[test]
fn a_certificate_naming_another_host_is_accepted_only_by_the_hostname_relaxation() {
    // Riot's certificate names no host, and ours is an IP literal, so the one relaxation in the
    // client (`danger_accept_invalid_hostnames`, no SNI) is what makes the real thing connect. The
    // other mock certificate has a SAN for 127.0.0.1 and never needs it, so this is the only test
    // that runs the relaxed path. Both halves are asserted: the same chain and the same pinned
    // root fail under a connector that checks the name, and succeed under the client.
    let mock = MockGame::start_with(vec![Reply::Body(IN_PROGRESS)], true);

    // A connector pinned to the same root, but with the stock host-name check, refuses it: the
    // name is the only thing wrong, so this is the relaxation's whole job.
    let root = native_tls::Certificate::from_pem(&mock.root_pem).expect("the mock's root parses");
    let checking = TlsConnector::builder()
        .disable_built_in_roots(true)
        .add_root_certificate(root)
        .build()
        .expect("a connector that pins the root and checks the name");
    let tcp = TcpStream::connect_timeout(&mock.addr, Duration::from_secs(2)).expect("connects");
    assert!(
        checking.connect("127.0.0.1", tcp).is_err(),
        "a certificate for another host must fail a name check, or this test proves nothing"
    );

    // The client, which relaxes only that, gets the document.
    let body = replayed_events::lol::LoopbackClient::for_endpoint_with_root(
        mock.endpoint(),
        &mock.root_pem,
    )
    .expect("pinned to the mock's root")
    .get()
    .expect("the relaxed client accepts a chain it trusts whatever name it carries");
    assert!(matches!(body, Response::Body(_)), "{body:?}");

    // And the relaxation is not a bypass: an unrelated root still fails for the same server.
    let other = tls::unrelated_root();
    let err = replayed_events::lol::LoopbackClient::for_endpoint_with_root(mock.endpoint(), &other)
        .expect("pinned to an unrelated root")
        .get()
        .expect_err("a chain the pin does not trust is refused, whatever name it carries");
    assert!(
        matches!(err, replayed_events::lol::FetchError::Tls(..)),
        "{err}"
    );
}

#[test]
fn presence_is_a_game_only_when_the_body_names_one() {
    use replayed_events::process::{default_watch, GamePresence, LiveClientPresence};

    // Each answer is one poll against a fresh mock. `{}` and `null` are the client between games:
    // a 200, but not a running game. The old check counted any 200.
    for (reply, running) in [
        (
            Reply::Body(r#"{"gameMode":"CLASSIC","gameTime":812.5,"mapName":"Map11"}"#),
            true,
        ),
        (Reply::Body(IN_PROGRESS), true),
        (Reply::Body("{}"), false),
        (Reply::Body("null"), false),
        (Reply::NoGame, false),
    ] {
        let mock = MockGame::start(vec![reply]);
        let mut source =
            LiveClientPresence::with_root(&default_watch(), mock.endpoint(), &mock.root_pem)
                .expect("a source for the mock");
        let presences = source.poll().expect("an answer");
        assert_eq!(presences.len(), 1);
        assert_eq!(presences[0].running, running, "for {reply:?}");
    }

    // Somebody else's body is a failure to look, not a stopped game.
    let mock = MockGame::start(vec![Reply::Body("<html>not the game</html>")]);
    let mut source =
        LiveClientPresence::with_root(&default_watch(), mock.endpoint(), &mock.root_pem)
            .expect("a source for the mock");
    assert!(source.poll().is_err());
}

#[test]
fn the_live_client_is_only_ever_asked_for_a_get() {
    // The Live Client Data API is read-only: the client asks it for a document and nothing
    // else. A `POST` or a `PUT` to the game client is the one thing that must never happen
    // next to kernel anti-cheat, and a grep cannot guard it — the method is a string in a
    // request line, and the guard would have to name every method it does not want. So it
    // is asserted on the wire, by a server that records what it was asked.
    let mock = MockGame::start(vec![Reply::Body(IN_PROGRESS)]);
    let mut poller = mock.poller();
    for _ in 0..3 {
        poller.poll_once();
    }
    // And the client on its own, which is the other way into the same endpoint.
    let _ = replayed_events::lol::LoopbackClient::for_endpoint_with_root(
        mock.endpoint(),
        &mock.root_pem,
    )
    .expect("pinned to the mock's root")
    .get();

    let requests = mock.requests();
    assert!(!requests.is_empty(), "the mock saw the polls: {requests:?}");
    for request in &requests {
        assert!(
            request.starts_with("GET "),
            "the Live Client Data API is read-only, and the client asked for {request:?}"
        );
    }
}

#[test]
fn a_full_event_history_is_derived_in_order_and_each_event_is_emitted_once() {
    // Two documents: the game with an empty history, then with the history. A real start looks
    // like this (the loading screen serves almost nothing), and it is the one case where the
    // history's events are new and must be reported.
    let mock = MockGame::start(vec![
        Reply::Body(LOADING),
        Reply::Body(IN_PROGRESS),
        Reply::Body(EVENTS_NEXT),
        Reply::Body(EVENTS_NEXT),
    ]);
    let mut poller = mock.poller();

    // Attach: a game, no history yet.
    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);

    // The history arrives: the player's kill, the player's death, a dragon — and nothing for
    // the champion kill between two other players.
    let history = poller.poll_once();
    assert_eq!(
        kinds(&history),
        vec![EventKind::Kill, EventKind::Death, EventKind::Dragon]
    );
    assert!(payload_of(&history, EventKind::Kill).contains("\"killer\":\"TestPlayer#EUW\""));
    assert!(payload_of(&history, EventKind::Death).contains("\"victim\":\"TestPlayer#EUW\""));
    assert!(payload_of(&history, EventKind::Dragon).contains("\"monster\":\"DRAGON\""));

    // Two more events arrive later, one at a time.
    assert_eq!(
        kinds(&poller.poll_once()),
        vec![EventKind::Kill, EventKind::Baron]
    );
    assert!(poller.poll_once().is_empty(), "the same document, again");
}

#[test]
fn the_game_ending_emits_one_end_event_and_every_later_poll_is_silent() {
    // 404s at the end: the Live Client's answer once the game is over.
    let mock = MockGame::start(vec![Reply::Body(EVENTS_NEXT), Reply::NoGame]);
    let mut poller = mock.poller();

    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);
    let ended = poller.poll_once();
    assert_eq!(kinds(&ended), vec![EventKind::GameEnd]);
    assert!(!poller.in_game());
    assert!(
        payload_of(&ended, EventKind::GameEnd).contains("last_game_time_s"),
        "the record says how long the game ran"
    );
    for poll in 1..=3 {
        assert!(
            poller.poll_once().is_empty(),
            "poll {poll} after the game ended"
        );
    }
}

#[test]
fn a_connection_that_is_refused_is_the_no_game_case() {
    // Nothing listening: the ordinary state of a machine with no League client. The poll must be
    // empty, not an error, and must not invent a GameEnd for a game never seen.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let addr = listener.local_addr().unwrap();
    drop(listener); // the port is now closed

    let endpoint = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
    let mut poller = LolPoller::new(LolConfig {
        endpoint,
        interval: Duration::from_millis(10),
    })
    .expect("a client for a closed port");

    for _ in 0..3 {
        assert!(
            poller.poll_once().is_empty(),
            "a refused connection is not an event"
        );
    }
    assert!(!poller.in_game());
}

#[test]
fn a_malformed_body_is_neither_an_event_nor_the_end_of_the_game() {
    // 1. A game is being played.
    // 2. The endpoint answers something that is not JSON.
    // 3. It recovers with one new event.
    //
    // The malformed answer must not end the game (state is kept) nor be reported as a stale event
    // later: step 3 emits exactly the one new line.
    let broken = r#"{"gameData": {"gameTime": "soon"}}"#;
    let mock = MockGame::start(vec![
        Reply::Body(LOADING),
        Reply::Body(IN_PROGRESS),
        Reply::Body(broken),
        Reply::Body(broken),
        Reply::Body(EVENTS_NEXT),
    ]);
    let mut poller = mock.poller();

    assert_eq!(kinds(&poller.poll_once()), vec![EventKind::GameStart]);
    assert_eq!(
        kinds(&poller.poll_once()),
        vec![EventKind::Kill, EventKind::Death, EventKind::Dragon]
    );
    assert!(
        poller.poll_once().is_empty(),
        "a body replayed cannot read is not an event"
    );
    assert!(
        poller.poll_once().is_empty(),
        "and it is not reported again"
    );
    assert!(
        poller.in_game(),
        "one unreadable response is not the end of a game"
    );

    // Recovery: only the two new events, nothing replayed.
    assert_eq!(
        kinds(&poller.poll_once()),
        vec![EventKind::Kill, EventKind::Baron]
    );
    assert!(poller.poll_once().is_empty());
}

#[test]
fn the_poller_only_ever_asks_for_loopback() {
    // The one constructor that accepts an address refuses anything else, so a configuration
    // mistake cannot turn the poller into an outbound HTTP client.
    let non_loopback: SocketAddr = "192.168.1.10:2999".parse().unwrap();
    let err =
        Endpoint::loopback(non_loopback, "/liveclientdata/allgamedata").expect_err("not loopback");
    assert!(err.to_string().contains("loopback"), "got: {err}");

    // The endpoint a poller was built with is the one it dialled, and it is loopback.
    let mock = MockGame::start(vec![Reply::NoGame]);
    let poller = mock.poller();
    assert!(poller.endpoint().is_loopback());
    assert_eq!(poller.endpoint().addr(), mock.addr);

    // And a response type is what the client reports for a 404 rather than an error.
    let client = replayed_events::lol::LoopbackClient::for_endpoint_with_root(
        mock.endpoint(),
        &mock.root_pem,
    )
    .expect("a pinned client for the mock");
    assert_eq!(
        client.get().expect("the mock answers"),
        Response::NotServing,
        "a 404 is a value, not an error: it is how the endpoint ends a game"
    );
}
