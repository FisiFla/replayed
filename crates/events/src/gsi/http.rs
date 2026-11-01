//! What the GSI listener accepts on the wire, and what it answers with.
//!
//! The listener is a server on a socket any local process can reach, so this module is a
//! gate: the request must be a `POST` to the configured path, declare its body length, and the
//! body is bounded before a byte of it is read. Every failure is answered with a status and a
//! closed connection. Nothing is logged here, because every byte is still attacker-controlled
//! and the logging decision belongs to [`crate::gsi`].

use crate::wire::{find_head_end, parse_head, terminator_len, Framed, Head, MAX_HEAD_BYTES};
use std::io::Read;

/// The largest body accepted. A CS2 payload with `allplayers` is a few hundred KB and Dota 2's
/// is smaller; 2 MB is generous for a loopback POST and still bounds what a local process can
/// make the recorder parse.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// A request that got as far as the token gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub body: Vec<u8>,
}

/// Why a request was refused before it could be looked at. Each variant carries the status the
/// connection is answered with: 405/404 for a wrong method or path (a client mistake), the
/// rest refuse a body replayed will not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Not a `POST`.
    MethodNotAllowed,
    /// A path other than the configured one.
    NotFound,
    /// No `Content-Length`, or a chunked body. GSI sends a length; refusing anything else means
    /// the listener never guesses where a request ends.
    LengthRequired,
    /// A body over [`MAX_BODY_BYTES`], or one that is shorter than it claims.
    TooLarge,
    /// Not HTTP, or not something this reader understands.
    Malformed,
    /// A request a web page made (it carries `Sec-Fetch-*` headers). See [`frame`].
    FromBrowser,
}

impl Reject {
    pub fn status(self) -> u16 {
        match self {
            Reject::MethodNotAllowed => 405,
            Reject::NotFound => 404,
            Reject::LengthRequired => 411,
            Reject::TooLarge => 413,
            Reject::Malformed => 400,
            Reject::FromBrowser => 403,
        }
    }
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Reject::MethodNotAllowed => "the method is not POST",
            Reject::NotFound => "the path is not the configured one",
            Reject::LengthRequired => "the body has no declared length",
            Reject::TooLarge => "the body is over the limit or shorter than it claims",
            Reject::Malformed => "the request is not HTTP this listener understands",
            Reject::FromBrowser => "the request was made by a web page, not by the game",
        };
        f.write_str(text)
    }
}

/// How far a buffer of bytes read so far has got towards being one request. See [`frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framing {
    /// More bytes are needed. `head_done` says whether the head has arrived: a connection that gives
    /// up here has sent a truncated head (`400`) or a body shorter than it claimed (`413`).
    Incomplete { head_done: bool },
    /// Nothing about this can become a request: not HTTP, or a head that never ends.
    Invalid,
    /// A web page made this request. The game never sends `Sec-Fetch-*`.
    Browser,
    /// Everything [`read_request`] needs is in the buffer, so reading it will not touch the socket.
    /// `body` is the range of the body within the buffer; it is empty when the head alone settles
    /// the answer (wrong method, no length, too large), because [`read_request`] refuses those
    /// before it reads a body.
    Complete { body: std::ops::Range<usize> },
}

/// Decide, from the bytes read so far and without blocking, whether they are one whole request.
///
/// This is what lets the listener keep a slow or stalled peer away from its readers. A reader
/// that read from the socket held a thread for as long as the peer cared to dawdle, and a
/// handful of peers that sent nothing made the game's own POSTs wait. The listener now collects
/// the bytes itself, without blocking, and hands a reader only a request that is already whole:
/// reading it is a copy out of memory, so no reader ever waits on the network.
///
/// A request is [`Framing::Complete`] as soon as the head settles the answer even if the body is
/// not here: [`read_request`] refuses a wrong method, a wrong target, a chunked body, a missing
/// or oversized `Content-Length` from the head alone, without reading the body, and so does this.
pub fn frame(buf: &[u8]) -> Framing {
    let Some(end) = find_head_end(buf) else {
        return if buf.len() > MAX_HEAD_BYTES {
            Framing::Invalid
        } else {
            Framing::Incomplete { head_done: false }
        };
    };
    if end > MAX_HEAD_BYTES {
        return Framing::Invalid;
    }
    let body_start = end + terminator_len(&buf[end..]);
    let Ok(head) = parse_head(&buf[..end]) else {
        return Framing::Invalid;
    };
    // The headers a script cannot set: every fetch a browser makes carries them, no game does.
    if head
        .headers
        .iter()
        .any(|(name, _)| name.starts_with("sec-fetch-"))
    {
        return Framing::Browser;
    }
    let settled_by_head = Framing::Complete {
        body: body_start..body_start,
    };
    if head.method() != Some("POST") || head.chunked() {
        return settled_by_head;
    }
    match head.content_length() {
        Some(length) if length <= MAX_BODY_BYTES as u64 => {
            let end = body_start + length as usize;
            if buf.len() >= end {
                Framing::Complete {
                    body: body_start..end,
                }
            } else {
                Framing::Incomplete { head_done: true }
            }
        }
        _ => settled_by_head,
    }
}

/// Read one request from `stream`, which must be `POST <path>` with a declared body length.
pub fn read_request(stream: &mut impl Read, path: &str) -> Result<Request, Reject> {
    let mut framed = Framed::new(stream);
    let head = framed.head().map_err(|_| Reject::Malformed)?;

    if head.method() != Some("POST") {
        return Err(Reject::MethodNotAllowed);
    }
    if !target_matches(&head, path) {
        return Err(Reject::NotFound);
    }
    if head.chunked() {
        return Err(Reject::LengthRequired);
    }
    let Some(length) = head.content_length() else {
        return Err(Reject::LengthRequired);
    };
    if length > MAX_BODY_BYTES as u64 {
        return Err(Reject::TooLarge);
    }
    let body = framed
        .body(&head, MAX_BODY_BYTES)
        .map_err(|_| Reject::TooLarge)?;
    Ok(Request { body })
}

/// Whether the request target is the configured path. A query string is ignored: the path
/// is what routes, and nothing in a query is ever read.
fn target_matches(head: &Head, path: &str) -> bool {
    match head.target() {
        Some(target) => target.split('?').next() == Some(path),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read(raw: &[u8]) -> Result<Request, Reject> {
        read_request(&mut Cursor::new(raw.to_vec()), "/gsi")
    }

    #[test]
    fn reads_a_post_to_the_configured_path() {
        let raw = b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1:45671\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let request = read(raw).expect("a well-formed GSI POST");
        assert_eq!(request.body, b"{\"a\":1}");
    }

    #[test]
    fn a_query_string_does_not_hide_the_path() {
        let raw = b"POST /gsi?token=x HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
        assert!(
            read(raw).is_ok(),
            "the query is ignored, not part of the route"
        );
    }

    #[test]
    fn a_method_other_than_post_is_405() {
        for method in ["GET", "PUT", "DELETE", "HEAD", "post"] {
            // The reader upper-cases the method, so a lower-case `post` is also a POST.
            let raw = format!("{method} /gsi HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}");
            let result = read(raw.as_bytes());
            if method == "post" {
                assert_eq!(
                    result,
                    Ok(Request {
                        body: b"{}".to_vec()
                    }),
                    "the method is case-insensitive"
                );
            } else {
                assert_eq!(result, Err(Reject::MethodNotAllowed), "{method}");
            }
        }
    }

    #[test]
    fn another_path_is_404() {
        for target in ["/", "/liveclientdata/allgamedata", "/gsi/", "/gsix", ""] {
            let raw = format!("POST {target} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}");
            assert_eq!(read(raw.as_bytes()), Err(Reject::NotFound), "{target:?}");
        }
    }

    #[test]
    fn a_body_without_a_declared_length_is_411() {
        let raw = b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n{\"a\":1}";
        assert_eq!(read(raw), Err(Reject::LengthRequired));
    }

    #[test]
    fn a_chunked_body_is_refused_rather_than_guessed_at() {
        let raw =
            b"POST /gsi HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n";
        assert_eq!(read(raw), Err(Reject::LengthRequired), "GSI sends a length");
    }

    #[test]
    fn a_body_over_the_limit_is_413_before_it_is_read() {
        let raw = format!(
            "POST /gsi HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(read(raw.as_bytes()), Err(Reject::TooLarge));
    }

    #[test]
    fn a_body_shorter_than_it_claims_is_413_and_not_a_partial_read() {
        let raw = b"POST /gsi HTTP/1.1\r\nContent-Length: 500\r\n\r\n{\"a\":1}";
        assert_eq!(read(raw), Err(Reject::TooLarge));
    }

    #[test]
    fn a_request_that_is_not_http_is_400() {
        assert_eq!(read(b"garbage\r\n\r\n"), Err(Reject::Malformed));
        assert_eq!(read(b""), Err(Reject::Malformed));
        assert_eq!(
            read(b"POST /gsi\r\n\r\n"),
            Err(Reject::Malformed),
            "no HTTP version"
        );
    }

    #[test]
    fn an_over_long_head_is_400() {
        let mut raw = b"POST /gsi HTTP/1.1\r\n".to_vec();
        raw.extend(vec![b'X'; 64 * 1024]);
        assert_eq!(read(&raw), Err(Reject::Malformed));
    }

    #[test]
    fn framing_waits_for_the_head_then_for_the_body_and_never_reads_past_the_length() {
        let raw = b"POST /gsi HTTP/1.1\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        // Every proper prefix is incomplete, and says whether the head had arrived.
        for cut in 0..raw.len() {
            let head_len = raw.len() - 7;
            assert_eq!(
                frame(&raw[..cut]),
                Framing::Incomplete {
                    head_done: cut >= head_len
                },
                "{cut} bytes"
            );
        }
        let whole = frame(raw);
        assert_eq!(
            whole,
            Framing::Complete {
                body: raw.len() - 7..raw.len()
            }
        );
        // Bytes past the declared body (a pipelined next request) are not part of this one.
        let mut more = raw.to_vec();
        more.extend_from_slice(b"POST /gsi HTTP/1.1\r\n");
        assert_eq!(frame(&more), whole);
    }

    #[test]
    fn a_head_that_settles_the_answer_is_complete_without_its_body() {
        // The same cases `read_request` refuses before it reads a body: a reader handed these
        // must not wait for bytes that will never come.
        for raw in [
            "GET /gsi HTTP/1.1\r\nContent-Length: 500\r\n\r\n".to_string(),
            "POST /gsi HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_string(),
            "POST /gsi HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
            format!(
                "POST /gsi HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            ),
        ] {
            assert!(
                matches!(frame(raw.as_bytes()), Framing::Complete { .. }),
                "{raw:?}"
            );
            // And reading it from memory answers without blocking, whatever the answer is.
            assert!(read_request(&mut Cursor::new(raw.as_bytes().to_vec()), "/gsi").is_err());
        }
    }

    #[test]
    fn something_that_is_not_http_is_invalid_and_a_head_that_never_ends_is_too() {
        assert_eq!(frame(b"garbage\r\n\r\n"), Framing::Invalid);
        assert_eq!(frame(b"POST /gsi\r\n\r\n"), Framing::Invalid);
        let mut raw = b"POST /gsi HTTP/1.1\r\n".to_vec();
        raw.extend(vec![b'X'; 64 * 1024]);
        assert_eq!(frame(&raw), Framing::Invalid);
        // A head that is simply not finished yet is not invalid.
        assert_eq!(
            frame(b"POST /gsi HTTP/1.1\r\nHost: 127."),
            Framing::Incomplete { head_done: false }
        );
    }

    #[test]
    fn a_request_a_web_page_made_is_recognised_by_the_headers_a_script_cannot_set() {
        for header in [
            "Sec-Fetch-Site: cross-site",
            "sec-fetch-mode: no-cors",
            "Sec-Fetch-Dest: empty",
        ] {
            let raw = format!("POST /gsi HTTP/1.1\r\n{header}\r\nContent-Length: 2\r\n\r\n{{}}");
            assert_eq!(frame(raw.as_bytes()), Framing::Browser, "{header}");
            // Even before the body has arrived: there is no reason to wait for it.
            let head_only =
                format!("POST /gsi HTTP/1.1\r\n{header}\r\nContent-Length: 2000\r\n\r\n");
            assert_eq!(frame(head_only.as_bytes()), Framing::Browser);
        }
        // `Origin` alone is not enough to refuse: a native client may send one.
        let raw = b"POST /gsi HTTP/1.1\r\nOrigin: null\r\nContent-Length: 2\r\n\r\n{}";
        assert!(matches!(frame(raw), Framing::Complete { .. }));
    }

    #[test]
    fn the_statuses_the_gate_answers_with_are_the_ones_it_documents() {
        assert_eq!(Reject::Malformed.status(), 400);
        assert_eq!(Reject::NotFound.status(), 404);
        assert_eq!(Reject::MethodNotAllowed.status(), 405);
        assert_eq!(Reject::LengthRequired.status(), 411);
        assert_eq!(Reject::TooLarge.status(), 413);
        assert_eq!(Reject::FromBrowser.status(), 403);
        for reject in [
            Reject::Malformed,
            Reject::NotFound,
            Reject::MethodNotAllowed,
            Reject::LengthRequired,
            Reject::TooLarge,
            Reject::FromBrowser,
        ] {
            // Every refusal explains itself, since a user debugging an install sees these in the log.
            assert!(!reject.to_string().is_empty());
        }
    }
}
