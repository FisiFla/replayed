//! The one place in replayed that relaxes any part of TLS verification.
//!
//! Riot's Live Client Data API is HTTPS on `127.0.0.1:2999`, with a certificate issued by
//! Riot's "LoL Game Engineering Certificate Authority" that carries no name matching an IP
//! literal. The chain is verified: the client trusts only that pinned root (`riotgames.pem`,
//! published at `static.developer.riotgames.com/docs/lol/riotgames.pem`) and none of the
//! platform's roots. Only the host-name match is relaxed, as it has nothing to match. A process
//! squatting on port 2999 with any other certificate fails the handshake.
//!
//! * The relaxation is a private field of [`LoopbackClient`], built here and nowhere else; no
//!   public API hands out a connector or builds this type from a host name or URL.
//! * The destination is an [`Endpoint`], which can only be loopback, so the relaxed connector
//!   can only be pointed at this machine.
//! * `tests/no_egress.rs` asserts `danger_accept_invalid_certs` appears nowhere and
//!   `danger_accept_invalid_hostnames` only here.
//!
//! The request is written by hand (`GET <path> HTTP/1.1`, `Host: 127.0.0.1:2999`, SNI disabled
//! since the certificate matches no name we could send); [`crate::wire`] reads the response.

use crate::lol::endpoint::Endpoint;
use crate::wire::Framed;
use native_tls::{Certificate, HandshakeError, TlsConnector};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Riot's root certificate for the Live Client, this client's only trust anchor. Pinned in the
/// tree so nothing is downloaded at runtime.
const RIOT_ROOT_PEM: &[u8] = include_bytes!("riotgames.pem");

/// What the client calls itself: the crate's own version (the workspace's, so the app's), not a
/// number written here that nobody updates.
const USER_AGENT: &str = concat!("replayed/", env!("CARGO_PKG_VERSION"));

/// How long to wait for the TCP connection. Loopback answers at once or not at all, and the
/// once-a-second poller must not fall behind waiting on a socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one read or write may take.
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// The largest response body accepted. `allgamedata` is a few hundred KB for ten players; this
/// still bounds what a hostile local process could make the recorder allocate.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// What one poll of the endpoint found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The endpoint served a body (HTTP 200).
    Body(Vec<u8>),
    /// The endpoint is up and says there is no game to report (HTTP 404): the normal answer
    /// between games, and the signal that a game has ended.
    NotServing,
}

/// Why a poll could not produce a payload.
///
/// Variants are split by what a caller does with them: [`FetchError::is_no_game`] is the
/// "nothing is wrong" answer the poller must not log as an error every second.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The connection failed (refused, reset, interface gone): the ordinary case when nothing is
    /// playing.
    #[error("the connection to {0} failed: {1}")]
    Connection(SocketAddr, std::io::Error),
    /// The socket timed out. Not expected on loopback under normal load; a game loading its world
    /// can cause it.
    #[error("the Live Client at {0} did not answer in time")]
    Timeout(SocketAddr),
    /// The TLS handshake failed: the peer is not the Live Client, or not speaking TLS.
    #[error("the TLS handshake with {0} failed: {1}")]
    Tls(SocketAddr, String),
    /// The peer answered, but not with HTTP this reader understands.
    #[error("the Live Client answered something malformed: {0}")]
    Protocol(String),
    /// The peer answered with a status replayed does not expect from this endpoint.
    #[error("the Live Client answered HTTP {0}")]
    UnexpectedStatus(u16),
}

impl FetchError {
    /// Whether this is the ordinary "no game is running" answer (a refusal or 404 between games
    /// must not be reported as a failure).
    pub fn is_no_game(&self) -> bool {
        matches!(self, FetchError::Connection(..) | FetchError::Timeout(..))
    }
}

/// The client that talks to the Live Client Data API and nothing else.
///
/// Its address is fixed at construction from an [`Endpoint`], so the relaxed verification
/// cannot be combined with an arbitrary host.
pub struct LoopbackClient {
    endpoint: Endpoint,
    connector: TlsConnector,
}

impl LoopbackClient {
    /// The client for the real endpoint, as production uses it.
    pub fn live_client() -> Result<Self, FetchError> {
        Self::for_endpoint(Endpoint::live_client())
    }

    /// The same client for any loopback endpoint, which is how tests reach a mock server. It takes
    /// the whole [`Endpoint`], not an address, so the loopback check in [`Endpoint::loopback`]
    /// cannot be bypassed.
    pub fn for_endpoint(endpoint: Endpoint) -> Result<Self, FetchError> {
        Self::for_endpoint_with_root(endpoint, RIOT_ROOT_PEM)
    }

    /// As [`Self::for_endpoint`], trusting `root_pem` instead of Riot's root, so tests can stand
    /// up a mock under their own root. Still a pin (platform roots stay disabled), never a way to
    /// skip verification.
    pub fn for_endpoint_with_root(endpoint: Endpoint, root_pem: &[u8]) -> Result<Self, FetchError> {
        debug_assert!(
            endpoint.is_loopback(),
            "an Endpoint is loopback by construction"
        );
        let tls_err = |e: &dyn std::fmt::Display| FetchError::Tls(endpoint.addr(), format!("{e}"));
        let root = Certificate::from_pem(root_pem).map_err(|e| tls_err(&e))?;
        let connector = TlsConnector::builder()
            // Trust the pinned root and nothing else: a certificate signed by any other authority, or
            // self-signed, must fail.
            .disable_built_in_roots(true)
            .add_root_certificate(root)
            // The one relaxation left, scoped to a connector stored in this struct and never handed out:
            // Riot's certificate names no host, and ours is an IP literal.
            .danger_accept_invalid_hostnames(true)
            // No SNI: the Host header is an IP literal, and a name would only involve a resolver.
            .use_sni(false)
            .build()
            .map_err(|e| tls_err(&e))?;
        Ok(Self {
            endpoint,
            connector,
        })
    }

    /// The endpoint this client is pinned to.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// One `GET`. Returns the body, or [`Response::NotServing`] for a 404.
    ///
    /// A connection per call, deliberately: the poller asks once a second, a loopback handshake
    /// costs milliseconds, and holding a connection to a game that may be exiting is worse. State
    /// between polls lives in [`crate::lol::derive`], which is pure.
    pub fn get(&self) -> Result<Response, FetchError> {
        let addr = self.endpoint.addr();
        let stream =
            TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| self.io_error(e))?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|e| self.io_error(e))?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|e| self.io_error(e))?;
        // The payload is small and the poller synchronous: a delayed ACK would be the slowest part of
        // a one-second poll.
        let _ = stream.set_nodelay(true);

        let host = self.endpoint.host_header();
        let mut tls = match self.connector.connect(&host, stream) {
            Ok(tls) => tls,
            Err(HandshakeError::Failure(e)) => return Err(FetchError::Tls(addr, format!("{e}"))),
            // The socket is blocking and `connect` drives the handshake to completion; a non-blocking
            // handshake would be a bug.
            Err(HandshakeError::WouldBlock(_)) => return Err(FetchError::Timeout(addr)),
        };

        // Built from the endpoint's validated path, so nothing upstream can break the request line
        // (see `endpoint::is_valid_path`).
        let request = self.request_text(&host);
        tls.write_all(request.as_bytes())
            .map_err(|e| self.io_error(e))?;
        tls.flush().map_err(|e| self.io_error(e))?;

        let mut framed = Framed::new(tls);
        let head = framed
            .head()
            .map_err(|e| FetchError::Protocol(format!("{e:#}")))?;
        let status = head
            .status()
            .ok_or_else(|| FetchError::Protocol("the response has no status line".into()))?;
        match status {
            200 => {
                let body = framed
                    .body(&head, MAX_RESPONSE_BYTES)
                    .map_err(|e| FetchError::Protocol(format!("{e:#}")))?;
                Ok(Response::Body(body))
            }
            404 => Ok(Response::NotServing),
            other => Err(FetchError::UnexpectedStatus(other)),
        }
    }

    /// The request, written by hand: a `GET`, never another method, with a `User-Agent` carrying
    /// the version of the build that sent it, so a Riot-side or support log names the real one.
    fn request_text(&self, host: &str) -> String {
        format!(
            "GET {} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\n\
             User-Agent: {USER_AGENT}\r\nConnection: close\r\n\r\n",
            self.endpoint.path()
        )
    }

    fn io_error(&self, e: std::io::Error) -> FetchError {
        let addr = self.endpoint.addr();
        match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                FetchError::Timeout(addr)
            }
            _ => FetchError::Connection(addr, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_is_pinned_to_the_endpoint_it_was_given() {
        let addr: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let client = LoopbackClient::for_endpoint(ep.clone()).expect("a relaxed connector");
        assert_eq!(client.endpoint(), &ep);
        assert!(client.endpoint().is_loopback());
    }

    #[test]
    fn the_request_is_a_get_that_names_the_build_that_sent_it() {
        let client = LoopbackClient::live_client().expect("a client");
        let request = client.request_text("127.0.0.1:2999");
        assert!(
            request.starts_with("GET /liveclientdata/allgamedata HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("Host: 127.0.0.1:2999\r\n"), "{request}");
        let agent = format!("User-Agent: replayed/{}\r\n", env!("CARGO_PKG_VERSION"));
        assert!(request.contains(&agent), "{request}");
        assert!(
            !request.contains("replayed/0.1\r\n"),
            "the stale hard-coded version is gone: {request}"
        );
    }

    #[test]
    fn the_pinned_root_is_riots_and_parses() {
        let pem = std::str::from_utf8(RIOT_ROOT_PEM).expect("a PEM is ASCII");
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        let der = Certificate::from_pem(RIOT_ROOT_PEM)
            .expect("the pinned root parses")
            .to_der()
            .unwrap();
        let needle = b"LoL Game Engineering Certificate Authority";
        assert!(
            der.windows(needle.len()).any(|w| w == needle),
            "it is Riot's CA"
        );
    }

    #[test]
    fn a_garbage_root_is_a_tls_error_and_never_an_unpinned_client() {
        let ep = Endpoint::live_client();
        let err = LoopbackClient::for_endpoint_with_root(ep, b"not a certificate")
            .err()
            .expect("a bad root must fail closed");
        assert!(matches!(err, FetchError::Tls(..)), "{err}");
    }

    #[test]
    fn the_live_client_constructs_and_is_on_loopback() {
        // Constructing it connects to nothing, so this passes on a machine with no League client.
        let client = LoopbackClient::live_client().expect("the relaxed connector builds");
        assert_eq!(client.endpoint().addr().port(), 2999);
        assert!(client.endpoint().is_loopback());
    }

    #[test]
    fn a_closed_loopback_port_is_the_no_game_case_and_not_a_failure() {
        // Port 1 on loopback: nothing listens, which is what polling between games looks like. The
        // OS decides which error a closed port gives: macOS refuses immediately (`Connection`), while
        // on Windows the attempt times out (measured on the Windows test machine). The property pinned
        // is that either is a connection result reading as "no game", never a protocol error.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let client = LoopbackClient::for_endpoint(ep).unwrap();

        let err = client.get().expect_err("nothing is listening on port 1");
        assert!(err.is_no_game(), "{err} must read as 'no game is running'");
        assert!(
            matches!(err, FetchError::Connection(..) | FetchError::Timeout(..)),
            "a closed loopback port is a connection outcome — refused or timed out — and never \
             a protocol error: {err}"
        );
    }

    #[test]
    fn an_endpoint_that_only_accepts_and_closes_is_a_protocol_error() {
        // A socket that accepts and hangs up: the handshake cannot complete, which must be reported
        // as such and never as a panic.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        let ep = Endpoint::loopback(addr, "/liveclientdata/allgamedata").unwrap();
        let client = LoopbackClient::for_endpoint(ep).unwrap();
        let err = client.get().expect_err("the peer is not a TLS server");
        assert!(
            matches!(err, FetchError::Tls(..)),
            "a peer that is not speaking TLS is a TLS error: {err}"
        );
        assert!(
            !err.is_no_game(),
            "and it is not silently read as 'no game'"
        );
        let _ = server.join();
    }
}
