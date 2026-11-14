//! The local link ffmpeg's audio inputs arrive on, and the check that whoever dials it is ffmpeg.
//!
//! The recording's audio is PCM written to a socket the encoder listens on and the child dials
//! (`-i tcp://127.0.0.1:{port}` or `-i unix:{path}`). A listener accepts whoever connects first,
//! and "first" is not a property of the child: another program on the machine that dials freshly
//! opened loopback ports (a local app was measured doing exactly that, which is how this was
//! found) wins the race, becomes the audio input, and is fed the game's and the microphone's PCM
//! while ffmpeg's own connection is refused or queued behind it. Nothing in the byte stream says
//! who is on the other end, so the *socket* has to.
//!
//! So an accepted connection is only used when the operating system says its peer is the child
//! this encoder spawned, and anything else is dropped on the spot while the wait carries on until
//! the real deadline. **There is no fallback**: a peer whose owner cannot be determined is refused
//! exactly like one who is known to be someone else, and a platform with no way to ask has no
//! link at all ([`AudioListener::bind`] says so) rather than an unauthenticated one.
//!
//! * **Windows** keeps loopback TCP (a named pipe would need a different ffmpeg protocol) and asks
//!   the TCP table who owns the *client* end of the accepted connection (`GetExtendedTcpTable`,
//!   the same table `netstat -o` prints). That is a read of a system-wide table, not an open of
//!   any process: no handle to the game, or to anything else, is created.
//! * **Unix** uses a Unix-domain socket in a directory only this user can enter (mode `0700`,
//!   created exclusively so a directory someone planted there is an error rather than ours), and
//!   asks the kernel for the connecting process (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux).
//!   The directory alone already keeps other users out; the peer check is what keeps another
//!   process of this user out, which is the case that mattered.
//!
//! Neither is egress: the TCP listener is bound to `127.0.0.1` and refuses a non-loopback peer, and
//! the Unix socket has no network presence at all (`crates/events/tests/no_egress.rs`).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How often the wait for the child's connection looks again. A connection is not announced to a
/// non-blocking listener, so the wait polls; 20 ms is far below anything a recording can notice.
const POLL: Duration = Duration::from_millis(20);

/// What the encoder writes PCM into once the child has connected: the accepted socket, whichever
/// kind this platform's link is.
pub(crate) struct AudioStream(imp::Stream);

impl Write for AudioStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AudioStream {
    /// A bound on how long a write waits for ffmpeg to read; tests use it to tell a blocking socket
    /// from a non-blocking one.
    #[cfg(all(test, feature = "test-encoders"))]
    pub(crate) fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }
}

/// One accept attempt's outcome.
enum Poll {
    /// The child's connection, verified.
    Accepted(AudioStream),
    /// A connection was there and was dropped: it was not from the child (or could not be shown to be).
    Refused(String),
    /// Nothing is waiting.
    Idle,
}

/// What a Unix-socket link reports where a TCP link reports its port.
///
/// A Unix socket has no port, but `Encoder::mic_port` is how a caller learns that a microphone
/// input exists, and the recorder's status keeps it in a field where 0 means "none". An identifier
/// that is not 0 keeps that meaning true. Nothing dials it.
#[cfg(unix)]
pub(crate) const UNIX_LINK_ID: u16 = 1;

/// A listening audio input, bound before the child exists (its address is in the argument list).
pub(crate) struct AudioListener {
    imp: imp::Listener,
}

impl AudioListener {
    /// Bind a fresh, private input. `what` names it in errors (`"audio"`, `"microphone"`).
    pub(crate) fn bind(what: &str) -> anyhow::Result<Self> {
        Ok(Self {
            imp: imp::Listener::bind(what)?,
        })
    }

    /// The input URL the child is told (`-i`).
    pub(crate) fn url(&self) -> String {
        self.imp.url()
    }

    /// The loopback port, or `UNIX_LINK_ID` where the link is a Unix socket and has none.
    ///
    /// Reported through `Encoder::mic_port` as an identifier a caller can log and test for
    /// presence; nothing dials it. It is never 0 on either platform, because the recorder's
    /// status stores 0 as "no microphone input" and a Unix link reported as 0 read back as no
    /// microphone at all.
    pub(crate) fn port(&self) -> u16 {
        self.imp.port()
    }

    /// Wait for the process `expected_pid` to connect, and hand back the stream to it.
    ///
    /// Any other connection is dropped and the wait goes on: a refused peer costs the real one
    /// nothing, and a flood of them cannot starve it, because a refusal is followed by the next
    /// accept straight away rather than a sleep. The wait ends with:
    ///
    /// * [`io::ErrorKind::TimedOut`] after `timeout` (the child never connected; the message says
    ///   how many other connections were turned away, which is the diagnosis when it did try);
    /// * [`io::ErrorKind::ConnectionAborted`] once `abort` is set, which the encoder does when the
    ///   child has ended, since nothing can legitimately connect after that and waiting out the
    ///   deadline would only hold up the shutdown.
    pub(crate) fn accept_from(
        &self,
        expected_pid: u32,
        timeout: Duration,
        abort: &AtomicBool,
        what: &str,
    ) -> io::Result<AudioStream> {
        let deadline = Instant::now() + timeout;
        let mut refused: Vec<String> = Vec::new();
        loop {
            if abort.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!(
                        "the {what} input was abandoned because ffmpeg had already ended{}",
                        describe_refusals(&refused)
                    ),
                ));
            }
            match self.imp.poll(expected_pid, what)? {
                Poll::Accepted(stream) => return Ok(stream),
                Poll::Refused(why) => {
                    tracing::warn!("refused a connection to the {what} input: {why}");
                    refused.push(why);
                    // Straight back to accept: more may be queued behind it.
                    continue;
                }
                Poll::Idle => {}
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "ffmpeg never connected to the {what} input on {} within {timeout:?}{}",
                        self.imp.url(),
                        describe_refusals(&refused)
                    ),
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}

fn describe_refusals(refused: &[String]) -> String {
    match refused.len() {
        0 => String::new(),
        n => format!(
            "; {n} connection(s) from other processes were refused (last: {})",
            refused[n - 1]
        ),
    }
}

/// One row of the system's TCP table, reduced to what the owner lookup compares.
///
/// Ports are host order here; the table stores them in network order in the low 16 bits of a
/// 32-bit field ([`rows_from_table`] converts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct TcpRow {
    local_port: u16,
    remote_port: u16,
    owning_pid: u32,
}

/// The rows of a `MIB_TCPTABLE_OWNER_PID` laid out as `u32`s: the entry count, then six `u32`s
/// per row (`dwState`, `dwLocalAddr`, `dwLocalPort`, `dwRemoteAddr`, `dwRemotePort`,
/// `dwOwningPid`). Parsed from words rather than through a pointer cast, so the layout is a test
/// away from being checked on any host.
#[cfg_attr(not(windows), allow(dead_code))]
fn rows_from_table(words: &[u32]) -> Vec<TcpRow> {
    let Some((&count, rest)) = words.split_first() else {
        return Vec::new();
    };
    rest.as_chunks::<6>()
        .0
        .iter()
        .take(count as usize)
        .map(|row| TcpRow {
            local_port: u16::from_be((row[2] & 0xFFFF) as u16),
            remote_port: u16::from_be((row[4] & 0xFFFF) as u16),
            owning_pid: row[5],
        })
        .collect()
}

/// The process that owns the client end of a loopback connection.
///
/// A connection between two sockets on one host is two rows, one per end. The client's end is the
/// row whose local port is the port the connection came *from* and whose remote port is the
/// listener's, which is how the dialer is told apart from the accepted socket (whose local port
/// is the listener's) and from every unrelated connection that happens to share a port.
#[cfg_attr(not(windows), allow(dead_code))]
fn client_owner(rows: &[TcpRow], client_port: u16, listener_port: u16) -> Option<u32> {
    rows.iter()
        .find(|row| row.local_port == client_port && row.remote_port == listener_port)
        .map(|row| row.owning_pid)
}

#[cfg(windows)]
mod imp {
    use super::{client_owner, rows_from_table, AudioStream, Poll};
    use std::io;
    use std::net::{SocketAddr, TcpListener};
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::Networking::WinSock::AF_INET;

    pub(super) type Stream = std::net::TcpStream;

    pub(super) struct Listener {
        listener: TcpListener,
        port: u16,
    }

    impl Listener {
        pub(super) fn bind(what: &str) -> anyhow::Result<Self> {
            use anyhow::Context;
            let listener = TcpListener::bind("127.0.0.1:0")
                .with_context(|| format!("binding the {what} input listener on 127.0.0.1"))?;
            let port = listener
                .local_addr()
                .with_context(|| format!("reading the {what} input listener's port"))?
                .port();
            listener
                .set_nonblocking(true)
                .with_context(|| format!("making the {what} input listener non-blocking"))?;
            Ok(Self { listener, port })
        }

        pub(super) fn url(&self) -> String {
            format!("tcp://127.0.0.1:{}", self.port)
        }

        pub(super) fn port(&self) -> u16 {
            self.port
        }

        pub(super) fn poll(&self, expected_pid: u32, what: &str) -> io::Result<Poll> {
            let (stream, peer) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Poll::Idle),
                Err(e) => return Err(e),
            };
            if !peer.ip().is_loopback() {
                return Ok(Poll::Refused(format!("non-loopback peer {peer}")));
            }
            match owner_of_client(peer, self.port) {
                Ok(pid) if pid == expected_pid => {}
                Ok(pid) => {
                    return Ok(Poll::Refused(format!(
                        "{peer} belongs to process {pid}, not ffmpeg ({expected_pid})"
                    )))
                }
                // Refused like a stranger: an unknown owner is not a verified one.
                Err(e) => {
                    return Ok(Poll::Refused(format!(
                        "could not tell who owns {peer}: {e}"
                    )))
                }
            }
            // The listener is non-blocking only so the wait can enforce a deadline; that must not
            // leak into the stream handed to the pump. BSD-derived platforms propagate it to the
            // accepted socket and Windows may too, and a non-blocking socket fails `write_all`
            // with `WouldBlock` the instant ffmpeg's receive buffer fills.
            stream.set_nonblocking(false).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("making the accepted {what} input socket blocking: {e}"),
                )
            })?;
            // PCM arrives in small blocks read in real time, so Nagle only adds latency.
            stream.set_nodelay(true).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("disabling Nagle on the {what} input socket: {e}"),
                )
            })?;
            Ok(Poll::Accepted(AudioStream(stream)))
        }
    }

    /// The owner of the client end of the connection that came from `client`.
    ///
    /// The row for a connection that was accepted a moment ago is there, but the table is a
    /// snapshot, so a miss is retried briefly before it is called a failure.
    fn owner_of_client(client: SocketAddr, listener_port: u16) -> io::Result<u32> {
        for attempt in 0..5 {
            if let Some(pid) = client_owner(&tcp_rows()?, client.port(), listener_port) {
                return Ok(pid);
            }
            if attempt < 4 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the TCP table has no row for it",
        ))
    }

    /// The IPv4 TCP table with owning process ids.
    fn tcp_rows() -> io::Result<Vec<super::TcpRow>> {
        // Words, not bytes: the table is `u32`s and this gives the buffer their alignment.
        let mut words: Vec<u32> = vec![0; 1024];
        for _ in 0..8 {
            let mut size = (words.len() * 4) as u32;
            // SAFETY: the pointer is to `size` writable bytes of a buffer this function owns, and
            // `size` is the capacity it is told; on `ERROR_INSUFFICIENT_BUFFER` it reports the size
            // needed, which the next round allocates.
            let status = unsafe {
                GetExtendedTcpTable(
                    Some(words.as_mut_ptr().cast()),
                    &mut size,
                    false,
                    u32::from(AF_INET.0),
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            };
            if status == NO_ERROR.0 {
                return Ok(rows_from_table(&words));
            }
            if status == ERROR_INSUFFICIENT_BUFFER.0 {
                // A little slack: connections come and go between the two calls.
                words = vec![0; (size as usize).div_ceil(4) + 64];
                continue;
            }
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Err(io::Error::other("the TCP table kept growing between reads"))
    }
}

#[cfg(unix)]
mod imp {
    use super::{AudioStream, Poll};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) type Stream = std::os::unix::net::UnixStream;

    /// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, with the terminator.
    const SUN_PATH_BUDGET: usize = 100;

    pub(super) struct Listener {
        listener: UnixListener,
        dir: PathBuf,
        path: PathBuf,
    }

    impl Listener {
        pub(super) fn bind(what: &str) -> anyhow::Result<Self> {
            use anyhow::Context;
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            // The system temp dir rather than the scratch dir: a socket path has to fit in
            // `sun_path`, and a scratch dir under a long profile path does not.
            let base = std::env::temp_dir();
            let mut attempts = 0;
            let dir = loop {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);
                let candidate = base.join(format!(
                    "rp-{}-{}-{nanos:x}",
                    std::process::id(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed)
                ));
                // Exclusive creation with the mode set at creation: nothing is ever created
                // wider and narrowed after, and a directory that already exists (planted, or a
                // stale one) is not adopted.
                match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
                    Ok(()) => break candidate,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => {
                        attempts += 1;
                    }
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!(
                                "creating the private directory for the {what} input in {}",
                                base.display()
                            )
                        })
                    }
                }
            };
            let path = dir.join("in.sock");
            let bound = (|| -> anyhow::Result<UnixListener> {
                if path.as_os_str().len() > SUN_PATH_BUDGET {
                    anyhow::bail!(
                        "{} is too long for a socket path ({} bytes, at most {SUN_PATH_BUDGET}); \
                         set TMPDIR to a shorter directory",
                        path.display(),
                        path.as_os_str().len()
                    );
                }
                let listener = UnixListener::bind(&path).with_context(|| {
                    format!("binding the {what} input socket {}", path.display())
                })?;
                listener
                    .set_nonblocking(true)
                    .with_context(|| format!("making the {what} input listener non-blocking"))?;
                Ok(listener)
            })();
            match bound {
                Ok(listener) => Ok(Self {
                    listener,
                    dir,
                    path,
                }),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    Err(e)
                }
            }
        }

        pub(super) fn url(&self) -> String {
            format!("unix:{}", self.path.display())
        }

        pub(super) fn port(&self) -> u16 {
            super::UNIX_LINK_ID
        }

        pub(super) fn poll(&self, expected_pid: u32, what: &str) -> io::Result<Poll> {
            let (stream, _) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Poll::Idle),
                Err(e) => return Err(e),
            };
            match peer_pid_settled(stream.as_raw_fd()) {
                Ok(pid) if pid == expected_pid => {}
                Ok(pid) => {
                    return Ok(Poll::Refused(format!(
                        "process {pid} is not ffmpeg ({expected_pid})"
                    )))
                }
                Err(e) => return Ok(Poll::Refused(format!("could not tell who connected: {e}"))),
            }
            // BSD-derived platforms (macOS included) hand the accepted socket the listener's
            // O_NONBLOCK, Linux does not. A non-blocking socket fails `write_all` with `WouldBlock`
            // the instant ffmpeg's receive buffer fills.
            stream.set_nonblocking(false).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("making the accepted {what} input socket blocking: {e}"),
                )
            })?;
            Ok(Poll::Accepted(AudioStream(stream)))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Best effort: the directory is private and holds only this socket.
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    /// [`peer_pid`], asked again for a moment if the kernel says the socket is not connected yet.
    ///
    /// On macOS `accept` can hand back a socket whose peer is not yet recorded: asked at once,
    /// `LOCAL_PEERPID` answers `ENOTCONN` for a connection that is about to be fine, and ffmpeg's own
    /// connection was refused for it in the first run of the microphone tests. Only that one error is
    /// retried, and only briefly; a peer that has not shown up as connected by then is refused like
    /// any other that cannot be identified.
    fn peer_pid_settled(fd: std::os::fd::RawFd) -> io::Result<u32> {
        let mut attempt = 0;
        loop {
            match peer_pid(fd) {
                Err(e) if e.kind() == io::ErrorKind::NotConnected && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => return other,
            }
        }
    }

    /// The pid of the process that connected the socket at `fd`.
    #[cfg(target_os = "macos")]
    fn peer_pid(fd: std::os::fd::RawFd) -> io::Result<u32> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `fd` is a live socket; the out-pointer and length describe a `pid_t` this frame owns.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        u32::try_from(pid).map_err(|_| io::Error::other("the peer has no process id"))
    }

    /// The pid of the process that connected the socket at `fd`.
    #[cfg(target_os = "linux")]
    fn peer_pid(fd: std::os::fd::RawFd) -> io::Result<u32> {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `fd` is a live socket; the out-pointer and length describe a `ucred` this frame owns.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        u32::try_from(cred.pid).map_err(|_| io::Error::other("the peer has no process id"))
    }

    /// No way to ask on this platform, so no way to authenticate, so every peer is refused.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn peer_pid(_fd: std::os::fd::RawFd) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform cannot report a socket peer's process",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(rows: &[[u32; 6]]) -> Vec<u32> {
        let mut table = vec![rows.len() as u32];
        for row in rows {
            table.extend_from_slice(row);
        }
        table
    }

    /// The table stores ports in network order in the low half of a word, and the owner is the
    /// sixth word. Getting either wrong would match the wrong connection, so the layout is
    /// pinned here against a hand-built table rather than only against a live machine.
    #[test]
    fn the_tcp_table_is_read_with_ports_in_network_order() {
        // 0x5000 is port 80 as the table stores it (big-endian in the low 16 bits).
        let table = words(&[[5, 0x0100_007f, 0x5000, 0x0100_007f, 0x3412, 4242]]);
        assert_eq!(
            rows_from_table(&table),
            vec![TcpRow {
                local_port: 80,
                remote_port: 0x1234,
                owning_pid: 4242
            }]
        );
        // An entry count larger than the words present is not read past the end.
        let mut short = words(&[[5, 0, 0x5000, 0, 0x3412, 7]]);
        short[0] = 9;
        assert_eq!(rows_from_table(&short).len(), 1);
        assert!(rows_from_table(&[]).is_empty());
    }

    /// The dialer is the row whose local port is the connecting port and whose remote port is the
    /// listener's. The accepted socket's own row (the two ports swapped) is the same connection
    /// seen from the listener's side and belongs to *this* process, and matching it would
    /// authenticate everyone.
    #[test]
    fn the_client_end_is_told_from_the_accepted_end_and_from_strangers() {
        let rows = [
            // The accepted end: local is the listener's port, owned by us.
            TcpRow {
                local_port: 5000,
                remote_port: 40000,
                owning_pid: 1,
            },
            // The dialer's end.
            TcpRow {
                local_port: 40000,
                remote_port: 5000,
                owning_pid: 99,
            },
            // An unrelated connection on the same local port number.
            TcpRow {
                local_port: 40000,
                remote_port: 443,
                owning_pid: 7,
            },
        ];
        assert_eq!(client_owner(&rows, 40000, 5000), Some(99));
        assert_eq!(client_owner(&rows, 40001, 5000), None);
    }

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// An ffmpeg that dials `url` and writes `seconds` of silence into it: a peer that is a real
    /// separate process, the way the encoder's own child is, with no socket code of ours in the
    /// test (`no_egress.rs` allows an outbound connect in a fixed list of files only).
    fn dialer(url: &str, seconds: u32) -> std::process::Child {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-re"])
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"])
            .args(["-t", &seconds.to_string(), "-f", "s16le", url])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the dialing ffmpeg")
    }

    /// The process that dials is the one that is let in, and what it sends arrives.
    #[test]
    fn the_expected_process_is_accepted() {
        if !ffmpeg_available() {
            eprintln!("skipped: no ffmpeg on PATH");
            return;
        }
        let listener = AudioListener::bind("audio").expect("bind");
        let mut child = dialer(&listener.url(), 1);
        let stream = listener.accept_from(
            child.id(),
            Duration::from_secs(10),
            &AtomicBool::new(false),
            "audio",
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            stream.is_ok(),
            "the child that was expected must be accepted: {:?}",
            stream.err()
        );
    }

    /// The failure this exists for: a process that is not the child dials first and is turned
    /// away, and the wait goes on to its own deadline rather than adopting the stranger.
    ///
    /// The "stranger" is a real ffmpeg and the "expected child" is this test process, so the one
    /// connection that arrives is, by construction, from someone other than who is expected.
    #[test]
    fn a_connection_from_any_other_process_is_refused_and_the_wait_continues() {
        if !ffmpeg_available() {
            eprintln!("skipped: no ffmpeg on PATH");
            return;
        }
        let listener = AudioListener::bind("audio").expect("bind");
        let mut stranger = dialer(&listener.url(), 5);
        let started = Instant::now();
        let outcome = listener.accept_from(
            std::process::id(),
            Duration::from_millis(1500),
            &AtomicBool::new(false),
            "audio",
        );
        let _ = stranger.kill();
        let _ = stranger.wait();

        let err = match outcome {
            Ok(_) => panic!("a connection from a process that was not expected was accepted"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() >= Duration::from_millis(1400),
            "the wait must run to its deadline instead of ending on the stranger: {:?}",
            started.elapsed()
        );
        assert!(
            err.to_string().contains("were refused"),
            "the refusal is what explains the missing connection: {err}"
        );
    }

    /// A wait whose child has ended is over at once instead of at its deadline, and says so with
    /// a kind that is not the deadline's (a shutdown reports a missing connection differently
    /// from a wait that was abandoned).
    #[test]
    fn an_abandoned_wait_ends_at_once_and_is_not_a_deadline() {
        let listener = AudioListener::bind("audio").expect("bind");
        let abort = AtomicBool::new(true);
        let started = Instant::now();
        let err = match listener.accept_from(1, Duration::from_secs(30), &abort, "audio") {
            Ok(_) => panic!("nothing connected"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_lives_in_a_private_directory_that_is_removed_with_the_listener() {
        use std::os::unix::fs::PermissionsExt;
        let listener = AudioListener::bind("audio").expect("bind");
        let url = listener.url();
        let path = std::path::PathBuf::from(url.strip_prefix("unix:").expect("a unix url"));
        let dir = path.parent().expect("inside a directory").to_path_buf();
        assert_eq!(
            std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
            0o700,
            "only this user may enter the directory the socket is in"
        );
        drop(listener);
        assert!(!dir.exists(), "the directory goes with the listener");
    }
}
