//! The HTTP/1.1 framing the two loopback integrations share, and nothing else.
//!
//! The League poller makes one `GET` a second and reads one response; the GSI listener reads
//! one `POST` per state change. They need framing that is bounded, cannot panic and cannot be
//! made to allocate without limit by the peer, which is the part a hand-written reader must
//! get right and so has the exhaustive tests at the bottom.
//!
//! Deliberately absent: pooling, keep-alive, redirects, cookies, compression and chunked
//! requests. A chunked response body is decoded; a request body must carry `Content-Length`,
//! which [`crate::gsi`] requires.
//!
//! [`Framed`] keeps whatever it read past the end of the head, because a response often
//! arrives headers and body in one segment; a reader that discarded the surplus would pass
//! tests and fail in the field.

use anyhow::{bail, Context, Result};
use std::io::{ErrorKind, Read};

/// The largest message head (start line plus headers) either side will read: enough for any
/// header a game client or local HTTPS server sends, small enough that a peer cannot make the
/// process allocate a megabyte per connection.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// How much is taken from the socket at a time.
const CHUNK: usize = 4 * 1024;

/// How long a chunk-size line may be (hex digits, an extension, CRLF).
const MAX_CHUNK_LINE: usize = 64;

/// The first line of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartLine {
    /// `POST /gsi HTTP/1.1` — `method` upper-cased, `target` as sent.
    Request { method: String, target: String },
    /// `HTTP/1.1 200 OK` — only the code is kept; the reason phrase carries nothing.
    Status { code: u16 },
}

/// A parsed message head. Header names are lower-cased on the way in, because HTTP header
/// names are case-insensitive and everything downstream looks them up by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub start: StartLine,
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// The first value of `name` (case-insensitive), or `None`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// `Content-Length`, when present and a valid number. A non-number is treated as absent
    /// rather than failing the whole message; the caller decides what that means.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse().ok()
    }

    /// Whether the body is chunked. Any other `Transfer-Encoding` value is not chunked.
    pub fn chunked(&self) -> bool {
        self.header("transfer-encoding").is_some_and(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
        })
    }

    /// The status code, on a response.
    pub fn status(&self) -> Option<u16> {
        match self.start {
            StartLine::Status { code } => Some(code),
            StartLine::Request { .. } => None,
        }
    }

    /// The method, on a request.
    pub fn method(&self) -> Option<&str> {
        match &self.start {
            StartLine::Request { method, .. } => Some(method),
            StartLine::Status { .. } => None,
        }
    }

    /// The request target, on a request.
    pub fn target(&self) -> Option<&str> {
        match &self.start {
            StartLine::Request { target, .. } => Some(target),
            StartLine::Status { .. } => None,
        }
    }
}

/// A reader that frames HTTP/1.1 messages out of a byte stream and keeps its place.
pub struct Framed<R> {
    inner: R,
    /// Bytes read from `inner` not yet consumed. `pos` is how far this reader has got, so a head
    /// can be parsed and its surplus left for the body.
    buf: Vec<u8>,
    pos: usize,
}

impl<R: Read> Framed<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// Read one message head. Fails, never panics and never allocates past [`MAX_HEAD_BYTES`], if
    /// the peer closes first or the head is not HTTP.
    pub fn head(&mut self) -> Result<Head> {
        loop {
            if let Some(end) = find_head_end(&self.buf[self.pos..]) {
                let text = self.buf[self.pos..self.pos + end].to_vec();
                self.pos += end + terminator_len(&self.buf[self.pos + end..]);
                return parse_head(&text);
            }
            let available = self.buf.len() - self.pos;
            if available > MAX_HEAD_BYTES {
                bail!("the message head exceeds {MAX_HEAD_BYTES} bytes");
            }
            if self.fill()? == 0 {
                if available == 0 {
                    bail!("the peer closed the connection without sending a message");
                }
                bail!("the peer closed the connection before the end of the message head");
            }
        }
    }

    /// Read the body a head describes, at most `max` bytes.
    ///
    /// * chunked: decoded, trailers consumed;
    /// * `Content-Length`: read exactly, an early close is an error;
    /// * neither: read to end of stream (a `Connection: close` responder that omitted the length).
    ///   Not for requests: the caller must refuse a request with no length, since reading to EOF
    ///   waits for the client to hang up.
    pub fn body(&mut self, head: &Head, max: usize) -> Result<Vec<u8>> {
        if head.chunked() {
            return self.chunked_body(max);
        }
        match head.content_length() {
            Some(len) => {
                let len = usize::try_from(len)
                    .ok()
                    .filter(|len| *len <= max)
                    .with_context(|| {
                        format!("a body of {len} bytes is over this caller's {max}-byte limit")
                    })?;
                self.exactly(len).context("reading the body")
            }
            None => self.rest_of_stream(max).context("reading the body"),
        }
    }

    /// Read and discard the rest of the stream, up to `max`, so a server can drain a rejected
    /// request and the client sees the response rather than a connection reset.
    pub fn drain(&mut self, max: usize) -> Result<()> {
        self.rest_of_stream(max).map(|_| ())
    }

    /// Exactly `n` bytes.
    fn exactly(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n.min(CHUNK));
        while out.len() < n {
            if self.pos == self.buf.len() && self.fill()? == 0 {
                bail!("the peer closed after {} of {n} body bytes", out.len());
            }
            let take = (n - out.len()).min(self.buf.len() - self.pos);
            out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
        }
        Ok(out)
    }

    /// Everything up to EOF, at most `max`.
    fn rest_of_stream(&mut self, max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            if self.pos < self.buf.len() {
                // Refuse before copying. The old code clamped the copy to what room was left, so once
                // `out` held exactly `max` bytes it took zero, `pos` never moved, the `> max` test
                // below it could never be true, and the loop spun forever on a body one byte over.
                let available = self.buf.len() - self.pos;
                if available > max.saturating_sub(out.len()) {
                    bail!("the body exceeds the {max}-byte limit");
                }
                out.extend_from_slice(&self.buf[self.pos..]);
                self.pos = self.buf.len();
                continue;
            }
            if self.fill()? == 0 {
                return Ok(out);
            }
        }
    }

    /// One line, CRLF (or bare LF) terminated, with the terminator removed.
    fn line(&mut self) -> Result<Vec<u8>> {
        loop {
            let start = self.pos;
            if let Some(end) = self.buf[start..].iter().position(|b| *b == b'\n') {
                let mut line = self.buf[start..start + end].to_vec();
                self.pos = start + end + 1;
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.buf.len() - start > MAX_HEAD_BYTES {
                bail!("a line longer than {MAX_HEAD_BYTES} bytes");
            }
            if self.fill()? == 0 {
                bail!("the peer closed in the middle of a line");
            }
        }
    }

    fn chunked_body(&mut self, max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let line = self.line()?;
            if line.len() > MAX_CHUNK_LINE {
                bail!("a chunk-size line longer than {MAX_CHUNK_LINE} bytes");
            }
            // `1a;name=value` — the size is what precedes any extension.
            let size_field = line.split(|b| *b == b';').next().unwrap_or(&line);
            let size_field = std::str::from_utf8(size_field)
                .context("a chunk size that is not ASCII")?
                .trim();
            let size =
                u64::from_str_radix(size_field, 16).context("a chunk size that is not hex")?;
            if size == 0 {
                // Trailers: header lines until a blank one. They are unused but must come off the wire.
                loop {
                    if self.line()?.is_empty() {
                        return Ok(out);
                    }
                }
            }
            let size = usize::try_from(size)
                .ok()
                .filter(|s| out.len() + *s <= max)
                .with_context(|| format!("a chunked body over this caller's {max}-byte limit"))?;
            let chunk = self.exactly(size)?;
            out.extend_from_slice(&chunk);
            // The chunk's own CRLF.
            let terminator = self.exactly(2)?;
            if terminator.as_slice() != b"\r\n" {
                bail!("a chunk that is not followed by CRLF");
            }
        }
    }

    /// Read more from the inner reader. Returns the number of bytes added (0 = EOF).
    fn fill(&mut self) -> Result<usize> {
        // Reclaim what has been consumed before growing, so a long-lived connection does not
        // accumulate.
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut chunk = [0u8; CHUNK];
        loop {
            match self.inner.read(&mut chunk) {
                Ok(0) => return Ok(0),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading from the peer"),
            }
        }
    }
}

/// Where the head ends within `bytes`, if it does.
pub(crate) fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n").or_else(|| {
        // A peer that sends bare LFs (tolerated, not encouraged).
        bytes.windows(2).position(|w| w == b"\n\n")
    })
}

/// How many bytes the terminator at `rest` occupies.
pub(crate) fn terminator_len(rest: &[u8]) -> usize {
    if rest.starts_with(b"\r\n\r\n") {
        4
    } else if rest.starts_with(b"\n\n") {
        2
    } else {
        0
    }
}

pub(crate) fn parse_head(bytes: &[u8]) -> Result<Head> {
    let text = std::str::from_utf8(bytes).context("the message head is not ASCII")?;
    let mut lines = text.split('\n');
    let start_line = lines.next().unwrap_or_default().trim_end_matches('\r');
    let start = parse_start_line(start_line)?;

    let mut headers = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .context("a header line with no colon in it")?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Head { start, headers })
}

fn parse_start_line(line: &str) -> Result<StartLine> {
    let mut parts = line.split(' ');
    let first = parts.next().unwrap_or_default();
    if first.starts_with("HTTP/") {
        let code = parts.next().context("a status line with no status code")?;
        let code: u16 = code
            .parse()
            .context("a status line whose code is not a number")?;
        if !(100..=599).contains(&code) {
            bail!("a status code outside 100..=599");
        }
        return Ok(StartLine::Status { code });
    }
    if first.is_empty() {
        bail!("an empty start line");
    }
    let target = parts.next().context("a request line with no target")?;
    let version = parts
        .next()
        .context("a request line with no HTTP version")?;
    if !version.starts_with("HTTP/") {
        bail!("a request line that does not name HTTP");
    }
    Ok(StartLine::Request {
        method: first.to_ascii_uppercase(),
        target: target.to_string(),
    })
}

/// Write a response with no body, which is all either integration answers with. `status` is
/// written as given, with no reason phrase (permitted by HTTP/1.1).
pub fn write_response(out: &mut impl std::io::Write, status: u16) -> std::io::Result<()> {
    out.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A reader that hands out one byte per call, proving the framing works off a stream arriving
    /// in arbitrary pieces, not just a buffer holding the whole reply.
    struct OneByteAtATime<'a>(&'a [u8]);

    impl Read for OneByteAtATime<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    fn read_all(bytes: &[u8]) -> Result<(Head, Vec<u8>)> {
        let mut framed = Framed::new(Cursor::new(bytes.to_vec()));
        let head = framed.head()?;
        let body = framed.body(&head, 64 * 1024)?;
        Ok((head, body))
    }

    #[test]
    fn reads_a_response_with_a_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"a\":\"hello\"}";
        let (head, body) = read_all(raw).expect("a well-formed response");
        assert_eq!(head.status(), Some(200));
        assert_eq!(head.content_length(), Some(13));
        assert_eq!(
            head.header("CONTENT-TYPE"),
            Some("application/json"),
            "lookup ignores case"
        );
        assert_eq!(body, b"{\"a\":\"hello\"}");
    }

    #[test]
    fn reads_a_response_with_a_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n9\r\n:\"value\"}\r\n0\r\n\r\n";
        let (head, body) = read_all(raw).expect("a chunked response");
        assert!(head.chunked());
        assert_eq!(body, b"{\"a\":\"value\"}");
    }

    #[test]
    fn reads_a_response_with_no_length_by_reading_to_the_end() {
        let raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"a\":1}";
        let (head, body) = read_all(raw).expect("a close-delimited response");
        assert_eq!(head.content_length(), None);
        assert_eq!(body, b"{\"a\":1}");
    }

    #[test]
    fn a_close_delimited_body_over_the_limit_is_an_error_and_not_an_endless_loop() {
        // `rest_of_stream` used to clamp its copy to the room left, so a body longer than `max` with
        // no `Content-Length` filled `out` to exactly `max`, then took zero bytes per pass forever.
        // Run it on a thread so a regression fails the test instead of hanging the suite.
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n0123456789ABCDEF";
            let mut framed = Framed::new(Cursor::new(raw.to_vec()));
            let head = framed.head().unwrap();
            let _ = done.send(framed.body(&head, 10).map_err(|e| format!("{e:#}")));
        });
        let result = outcome
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read finished instead of spinning");
        assert!(
            result.unwrap_err().contains("exceeds"),
            "the refusal says the limit was hit"
        );

        // A body of exactly `max` bytes is still fine, and so is one split across reads.
        let raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n0123456789";
        let mut framed = Framed::new(Cursor::new(raw.to_vec()));
        let head = framed.head().unwrap();
        assert_eq!(framed.body(&head, 10).unwrap(), b"0123456789");
        let mut framed = Framed::new(OneByteAtATime(raw));
        let head = framed.head().unwrap();
        assert_eq!(framed.body(&head, 10).unwrap(), b"0123456789");
        let mut framed = Framed::new(OneByteAtATime(b"HTTP/1.1 200 OK\r\n\r\n0123456789A"));
        let head = framed.head().unwrap();
        assert!(framed.body(&head, 10).is_err());
    }

    #[test]
    fn framing_works_when_the_message_arrives_one_byte_at_a_time() {
        // The realistic case: headers and body in one segment, delivered in arbitrary pieces. A
        // reader that lost what it read past the head would return an empty body here and still pass
        // the Cursor tests above.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"a\":\"hello\"}";
        let mut framed = Framed::new(OneByteAtATime(raw));
        let head = framed.head().expect("a head split over 45 reads");
        assert_eq!(head.status(), Some(200));
        assert_eq!(framed.body(&head, 1024).unwrap(), b"{\"a\":\"hello\"}");
    }

    #[test]
    fn reads_a_request_head_and_its_body() {
        let raw = b"POST /gsi HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 9\r\nAuthorization: Bearer x\r\n\r\n{\"a\":  1}";
        let (head, body) = read_all(raw).expect("a well-formed request");
        assert_eq!(head.method(), Some("POST"));
        assert_eq!(head.target(), Some("/gsi"));
        assert_eq!(head.status(), None, "a request is not a response");
        assert_eq!(body, b"{\"a\":  1}");
    }

    #[test]
    fn a_lf_only_head_is_tolerated() {
        let raw = b"HTTP/1.1 200 OK\nContent-Length: 2\n\nhi";
        let (head, body) = read_all(raw).expect("bare LF line endings");
        assert_eq!(head.status(), Some(200));
        assert_eq!(body, b"hi");
    }

    #[test]
    fn a_head_that_never_ends_is_an_error_and_not_an_allocation() {
        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        raw.extend(vec![b'X'; 200_000]);
        let err = Framed::new(Cursor::new(raw))
            .head()
            .expect_err("no terminator");
        let message = format!("{err:#}");
        assert!(
            message.contains("exceeds"),
            "the cap must be what stops it: {message}"
        );
    }

    #[test]
    fn a_body_over_the_callers_limit_is_refused_before_it_is_read() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 900000\r\n\r\n";
        let mut framed = Framed::new(Cursor::new(raw.to_vec()));
        let head = framed.head().unwrap();
        let err = framed
            .body(&head, 1024)
            .expect_err("30 bytes is over a 1024-byte limit");
        assert!(format!("{err:#}").contains("limit"), "got: {err:#}");
    }

    #[test]
    fn a_body_that_stops_early_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\nshort";
        let err = read_all(raw).expect_err("the peer closed early");
        assert!(format!("{err:#}").contains("closed"), "got: {err:#}");
    }

    #[test]
    fn a_connection_that_sends_nothing_is_an_error() {
        let err = Framed::new(Cursor::new(Vec::new()))
            .head()
            .expect_err("nothing to read");
        assert!(
            format!("{err:#}").contains("without sending"),
            "got: {err:#}"
        );

        let err = Framed::new(Cursor::new(b"HTTP/1.1 200 OK\r\n".to_vec()))
            .head()
            .expect_err("a half head");
        assert!(
            format!("{err:#}").contains("before the end"),
            "got: {err:#}"
        );
    }

    #[test]
    fn a_start_line_that_is_not_http_is_an_error() {
        assert!(Framed::new(Cursor::new(b"garbage\r\n\r\n".to_vec()))
            .head()
            .is_err());
        assert!(Framed::new(Cursor::new(b"\r\n\r\n".to_vec()))
            .head()
            .is_err());
        assert!(
            Framed::new(Cursor::new(b"GET /\r\n\r\n".to_vec()))
                .head()
                .is_err(),
            "a request line with no HTTP version"
        );
        assert!(
            Framed::new(Cursor::new(b"HTTP/1.1 abc OK\r\n\r\n".to_vec()))
                .head()
                .is_err(),
            "a status code that is not a number"
        );
    }

    #[test]
    fn a_header_line_without_a_colon_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nthis is not a header\r\n\r\n";
        let err = Framed::new(Cursor::new(raw.to_vec()))
            .head()
            .expect_err("no colon");
        assert!(format!("{err:#}").contains("colon"), "got: {err:#}");
    }

    #[test]
    fn a_chunk_size_that_is_not_hex_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nbody\r\n0\r\n\r\n";
        let err = read_all(raw).expect_err("not a chunk size");
        assert!(format!("{err:#}").contains("hex"), "got: {err:#}");
    }

    #[test]
    fn a_chunk_without_its_terminator_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhiXX0\r\n\r\n";
        let err = read_all(raw).expect_err("a chunk not followed by CRLF");
        assert!(format!("{err:#}").contains("CRLF"), "got: {err:#}");
    }

    #[test]
    fn chunked_trailers_are_consumed() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Checksum: 1\r\n\r\n";
        let (_, body) = read_all(raw).expect("a chunked body with a trailer");
        assert_eq!(body, b"abc");
    }

    #[test]
    fn a_response_is_written_with_a_length_and_no_keep_alive() {
        let mut out = Vec::new();
        write_response(&mut out, 403).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 403\r\n"), "got: {text}");
        assert!(text.contains("Content-Length: 0\r\n"), "got: {text}");
        assert!(text.contains("Connection: close\r\n"), "got: {text}");
        assert!(
            text.ends_with("\r\n\r\n"),
            "the head ends, and there is no body: {text}"
        );
    }
}
