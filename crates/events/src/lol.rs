//! League of Legends: the Live Client Data API.
//!
//! Riot's game client serves a JSON snapshot of the running game on
//! `https://127.0.0.1:2999/liveclientdata/allgamedata` and stops when the game is over. This
//! module polls it about once a second and derives [`GameEvent`]s from the difference between
//! consecutive snapshots.
//!
//! Pure logic first, I/O at the edge:
//!
//! | file | what it owns | I/O |
//! |---|---|---|
//! | [`endpoint`] | where the API lives, and the type that cannot point anywhere else | none |
//! | [`payload`] | the response document as typed Rust | none |
//! | [`derive`] | snapshot to events, as a pure state machine | none |
//! | [`client`] | the one HTTPS client, pinned to the endpoint | sockets |
//! | [`poller`] | the loop, the logging discipline, the event sink | sockets |
//!
//! `derive` is covered against canned payloads in `tests/fixtures/`; the network layer against
//! a local HTTPS mock (`tests/lol_mock.rs`) under a throwaway CA, serving either a certificate
//! for `127.0.0.1` or one naming another host (Riot's names none, so only the host-name
//! relaxation lets that connect). What the mock cannot be is Riot's real certificate chain: the
//! pinned root is a 2013 X.509 v1, SHA-1-signed certificate with no extensions, and a leaf
//! under it can only come from a live client, so that exact shape is untested here.
//!
//! No real League client has been checked against these shapes: the game runs kernel-level
//! anti-cheat and was never contacted. Payload shapes come from the documented API and are
//! untrusted input: every field is optional and no payload can panic the parser.
//!
//! Security: the endpoint is loopback by type ([`Endpoint::loopback`] refuses anything else and
//! is the only constructor). The client ([`client::LoopbackClient`]) trusts only Riot's pinned
//! root and skips just the host-name match; it is built from an `Endpoint` alone, so it can
//! only be pointed at this machine. `tests/no_egress.rs` asserts statically that
//! `danger_accept_invalid_certs` appears nowhere and `danger_accept_invalid_hostnames` only in
//! `lol/client.rs`.

pub mod client;
pub mod derive;
pub mod endpoint;
pub mod payload;
pub mod poller;

pub use client::{FetchError, LoopbackClient, Response};
pub use derive::Tracker;
pub use endpoint::Endpoint;
pub use payload::GameSnapshot;
pub use poller::{spawn, LolConfig, LolHandle, LolPoller, POLL_INTERVAL};
