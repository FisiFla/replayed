# Platform traps

Bugs that come from an OS behaviour **Linux and Windows do not share**, so a green local
run on one host, or a green CI run on another, can both miss them. Each has bitten this
codebase at least once. Read this before writing socket, file-path or process code that is
expected to work on Windows and macOS.

---

## Trap 1 — `accept()` makes the accepted socket non-blocking on macOS/BSD

**The rule: after `accept()`, set the socket's blocking mode explicitly. Never assume the
accepted socket is blocking.**

### What happens

A server often makes its **listener** non-blocking so the accept loop can enforce a
deadline (poll `accept()`, give up after a timeout) instead of parking forever in a
blocking `accept()`. That part is fine and portable.

The trap is what `accept()` *returns*. On **macOS and other BSD-derived platforms**, the
accepted socket **inherits the listener's `O_NONBLOCK` flag**. On **Linux and Windows it
does not** — the accepted socket is blocking regardless.

So a socket you deliberately made non-blocking "just for the listener" quietly comes back
non-blocking on the one host this project is developed on — and then:

- a `read`/`write` that would have blocked fails **immediately** with `WouldBlock`
  (`EAGAIN`, `os error 35`) the instant a buffer is full or empty, rather than waiting;
- read/write **timeouts set on the socket are ignored** on a non-blocking socket, so the
  bound you thought you had is not there;
- the symptom is **timing-dependent** — it appears when a buffer happens to fill, so it can
  pass on a warm local run and fail on a fresh CI runner.

The fix is always the same and is a no-op on Linux and Windows:

```rust
let (stream, _peer) = listener.accept()?;
stream.set_nonblocking(false)?;   // do not assume; macOS/BSD inherited O_NONBLOCK
stream.set_read_timeout(Some(timeout))?;
stream.set_write_timeout(Some(timeout))?;
```

### Where it has already happened here

1. **The audio link's accept loop** (originally `ffmpeg.rs::accept_within`, now in
   `crates/encoder/src/audio_link.rs`) — the encoder's audio transport. The listener is non-blocking to bound the "ffmpeg never connected" wait; the
   accepted socket then went non-blocking and `write_all` failed with `WouldBlock` as soon
   as ffmpeg's receive buffer filled. Fixed by `stream.set_nonblocking(false)` after accept
   (commit `82578bd fix(encoder): force the accepted audio socket to blocking mode`); the
   reasoning is at the call site.

2. **`crates/events/src/gsi.rs::handle`** — the CS2/Dota 2 GSI listener. Same listener
   pattern, same inheritance; a non-blocking socket ignored the request read timeout and
   `http` read a `WouldBlock` as a **malformed request (400)** instead of answering the
   client. Fixed by `stream.set_nonblocking(false)` before setting the timeouts (commit
   `bd59fce`). **This one was caught only by CI on a fresh runner** — the failing test
   `crates/events/tests/gsi_listener.rs::a_post_with_no_auth_block_at_all_is_rejected`
   asserted `403` and got `400`:

   ```console
   $ gh run view 35904179918 --log-failed
   … test a_post_with_no_auth_block_at_all_is_rejected … FAILED
   …   left: 400
   …  right: 403
   ```

3. **`crates/events/tests/lol_mock.rs`** — the League mock server's own listener is
   non-blocking so the mock can be stopped; it carries the same guard
   (`stream.set_nonblocking(false)`) for the same reason, with a comment saying so.

### Why the call-site comments are not enough

Each site already has a comment explaining this. They were added *when the bug was fixed*,
which is too late for the next person about to write socket code somewhere else. This file
is the place to look **first**.

---

## Trap 2 — `LOCAL_PEERPID` can say "not connected" for a connection that is about to be fine (macOS)

**The rule: when asking the kernel who is on the other end of a just-accepted Unix socket, expect
`ENOTCONN` for a moment on macOS, and retry that one error briefly.**

The audio link authenticates its peer (`crates/encoder/src/audio_link.rs`): the encoder uses a
connection only if the operating system says the connecting process is the ffmpeg child it spawned.
On Linux `SO_PEERCRED` answers at once. On macOS `accept` can return a socket whose peer is not yet
recorded, and `LOCAL_PEERPID` answers `ENOTCONN`; the first run of the microphone tests refused
ffmpeg's own connection because of it. The code retries that error (and only that error) for about
100 ms, then refuses the peer like any other it cannot identify. It does not fall back to an
unauthenticated link. On Windows the same check reads the system TCP table instead
(`GetExtendedTcpTable`) and has no such race.
