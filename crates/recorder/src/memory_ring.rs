//! The in-memory replay ring and the thread that feeds it.
//!
//! `replayed_replay::MemoryRingBuffer` holds the footage (it takes bytes, so its rules are testable
//! without a pipe); this owns the reader thread on the encoder's output pipe and the lock that
//! lets the pump query the span while the reader adds to it.
//!
//! # A silent reader is the failure this module is built around
//!
//! If the reader stops, nothing raises an error: ffmpeg keeps encoding into a pipe that absorbs
//! several seconds, the pump keeps submitting, and the only symptom is short clips or a frozen
//! `span_ms`. So every exit from the read loop records why it stopped, a panic included (the loop
//! is in `catch_unwind` because a panic bypasses the `?` error path), and [`MemoryRing::trigger`]
//! refuses to serve a clip from a ring known to be dead.
//!
//! # Lock discipline
//!
//! The reader holds the lock while pushing one fragment. A clip assembles its bytes under the lock
//! and splices outside it: the splice runs ffmpeg for as long as the muxer takes, and holding the
//! lock would block the reader, fill the pipe, stall ffmpeg's stdin and the recording still running.

use anyhow::{bail, Context, Result};
use replayed_media::FfmpegBinaries;
use replayed_replay::{MemoryRingBuffer, MemoryStats};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ChildStdout;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// How much is read from the pipe at a time: several fragments on a busy stream, bounding the thread's allocation to 256KiB.
const READ_CHUNK: usize = 256 * 1024;

/// The file a session recorded off the ring's stream is written to, inside the session directory.
/// Fragmented MP4 as the encoder produced it; [`crate::session::finalise_part`] makes it an ordinary file.
pub const SESSION_PART: &str = "recording.part";

/// Bytes the session writer may be behind by before the file is given up on: 64MiB of RAM at worst.
///
/// Bounded in bytes, not in reads: a read is anywhere from a few bytes to [`READ_CHUNK`], so a count
/// of reads let the queue hold anywhere from a few KiB (a slow stream gives up on the file early) to
/// far more than intended, and said nothing about the memory it was meant to cap.
const SINK_QUEUE_BYTES: usize = 64 * 1024 * 1024;

/// The reader's hand-off to the session file's writer thread.
///
/// The reader must never wait on the disk: it is the only thing emptying the encoder's pipe, so a
/// stalled write would stall ffmpeg and the recording with it, and the ring exists to keep clips
/// working. So a chunk is queued, never written inline, and a queue that fills (a disk that cannot
/// keep up, an antivirus scan holding the file) or a write that fails ends the *file*, loudly, and
/// leaves the ring running.
struct SinkFeed {
    tx: Option<Sender<Vec<u8>>>,
    /// Bytes queued and not yet written; the writer subtracts as it writes.
    queued: Arc<AtomicUsize>,
    limit_bytes: usize,
    failure: Arc<Mutex<Option<String>>>,
}

impl SinkFeed {
    fn push(&mut self, chunk: &[u8]) {
        let Some(tx) = &self.tx else { return };
        // `fetch_add` first so the check and the reservation are one step; undone when refused.
        let before = self.queued.fetch_add(chunk.len(), Ordering::AcqRel);
        if before.saturating_add(chunk.len()) > self.limit_bytes {
            self.queued.fetch_sub(chunk.len(), Ordering::AcqRel);
            record_sink_failure(
                &self.failure,
                "the disk could not keep up with the recording".to_string(),
            );
            self.tx = None;
            return;
        }
        // The writer already recorded why it stopped.
        if tx.send(chunk.to_vec()).is_err() {
            self.tx = None;
        }
    }
}

/// Record the first reason the session file stopped growing.
fn record_sink_failure(slot: &Arc<Mutex<Option<String>>>, why: String) {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        tracing::error!(
            "the session file stopped growing: {why}. The replay buffer keeps working, but the              saved session will end where it stopped"
        );
        *guard = Some(why);
    }
}

/// Why a session file stopped growing, shared between its writer thread and the ring.
type SinkFailure = Arc<Mutex<Option<String>>>;

/// Start the writer thread for `path`; returns what the reader feeds, the thread to join and its failure slot.
fn start_sink(
    path: &Path,
    capacity_bytes: usize,
) -> Result<(SinkFeed, JoinHandle<()>, SinkFailure)> {
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("creating the session file {}", path.display()))?;
    let (tx, rx) = channel::<Vec<u8>>();
    let queued = Arc::new(AtomicUsize::new(0));
    let writer_queued = Arc::clone(&queued);
    let failure = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&failure);
    let writer = std::thread::Builder::new()
        .name("replayed-session-writer".into())
        .spawn(move || {
            while let Ok(chunk) = rx.recv() {
                let len = chunk.len();
                let written = file.write_all(&chunk);
                writer_queued.fetch_sub(len, Ordering::AcqRel);
                if let Err(err) = written {
                    record_sink_failure(&slot, format!("writing the session file failed: {err}"));
                    return;
                }
            }
            // The stream ended: make what was written durable before the remux reads it.
            if let Err(err) = file.sync_all() {
                record_sink_failure(&slot, format!("flushing the session file failed: {err}"));
            }
        })
        .context("spawning the session file's writer thread")?;
    Ok((
        SinkFeed {
            tx: Some(tx),
            queued,
            limit_bytes: capacity_bytes,
            failure: Arc::clone(&failure),
        },
        writer,
        failure,
    ))
}

/// Everything a ring needs that is not the stream itself.
///
/// A struct because five of the fields are bare numbers: a positional `pre_ms`/`cap_ms` swap would
/// silently hold the wrong footage.
///
/// `Clone` so a capture resize can start the next ring with the same setup as the first (a new
/// encoder writes a new stream, and a new stream needs a new ring).
#[derive(Clone)]
pub struct RingSetup {
    pub bin: FfmpegBinaries,
    pub clips_dir: PathBuf,
    /// Bytes of RAM the ring may hold before it evicts.
    pub ram_cap_bytes: u64,
    /// Media time the ring may hold before it evicts — the window a trigger can ask for.
    pub cap_ms: u64,
    /// Media time kept before a trigger.
    pub pre_ms: u64,
    /// Media time kept after a trigger.
    pub post_ms: u64,
    /// The encoder that actually ran, for the clip's metadata.
    pub encoder: String,
    /// Also write the stream to this file as it arrives (a saved session); `None` for a buffer alone.
    pub session_part: Option<PathBuf>,
}

pub struct MemoryRing {
    shared: Arc<Mutex<MemoryRingBuffer>>,
    /// Why the reader stopped before the stream ended, if it did. Without it a dead ring answers every
    /// clip request with "nothing covers that range", which reads as a short buffer.
    failure: Arc<Mutex<Option<String>>>,
    reader: Option<JoinHandle<()>>,
    bin: FfmpegBinaries,
    clips_dir: PathBuf,
    pre_ms: u64,
    post_ms: u64,
    encoder: String,
    /// The session file's writer thread, while a session is being saved and it has not been joined.
    sink_writer: Option<JoinHandle<()>>,
    /// Why the session file stopped growing early.
    sink_failure: Option<SinkFailure>,
}

impl MemoryRing {
    /// Start reading `stream` into a ring of at most `ram_cap_bytes` / `cap_ms`.
    ///
    /// `stream` is the encoder's stdout (it exists only with `EncodeOutput::FragmentedStream`), so this
    /// ring is its one reader. Creates the clips directory here so a permissions or path problem
    /// surfaces at recording start, not when the user presses the key.
    pub fn start(stream: ChildStdout, setup: RingSetup) -> Result<Self> {
        let RingSetup {
            bin,
            clips_dir,
            ram_cap_bytes,
            cap_ms,
            pre_ms,
            post_ms,
            encoder,
            session_part,
        } = setup;
        std::fs::create_dir_all(&clips_dir)
            .with_context(|| format!("creating the clips directory {}", clips_dir.display()))?;
        let (feed, sink_writer, sink_failure) = match &session_part {
            Some(path) => {
                let (feed, writer, failure) = start_sink(path, SINK_QUEUE_BYTES)?;
                (Some(feed), Some(writer), Some(failure))
            }
            None => (None, None, None),
        };

        let shared = Arc::new(Mutex::new(MemoryRingBuffer::new(ram_cap_bytes, cap_ms)));
        let failure = Arc::new(Mutex::new(None));
        let ring = Arc::clone(&shared);
        let slot = Arc::clone(&failure);
        let reader = std::thread::Builder::new()
            .name("replayed-ram-ring".into())
            .spawn(move || {
                // A panic would otherwise kill this thread silently and show up only as short clips later; catch, name and record it.
                let hook = Arc::clone(&slot);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    read_until_eof(stream, ring, Arc::clone(&hook), feed)
                }));
                if let Err(panic) = outcome {
                    record_failure(
                        &slot,
                        format!("the reader panicked: {}", describe_panic(&panic)),
                    );
                }
            })
            .context("spawning the in-memory ring's reader thread")?;
        Ok(Self {
            shared,
            failure,
            reader: Some(reader),
            bin,
            clips_dir,
            pre_ms,
            post_ms,
            encoder,
            sink_writer,
            sink_failure,
        })
    }

    /// The ring's lock, or the reason it cannot be taken.
    ///
    /// Never folds a failure into zero: a poisoned lock (the reader died) and a live but empty ring look
    /// identical as a number, and only the first is a bug.
    fn lock(&self) -> Result<MutexGuard<'_, MemoryRingBuffer>> {
        self.shared.lock().map_err(|_| {
            anyhow::anyhow!(
                "the in-memory ring's lock is poisoned, so the reader thread died holding it{}",
                self.failure
                    .lock()
                    .ok()
                    .and_then(|f| f.clone())
                    .map(|why| format!(": {why}"))
                    .unwrap_or_default()
            )
        })
    }

    /// Media time the ring can prove it holds, run-relative. Zero only when genuinely empty; an
    /// unreadable ring is an error.
    pub fn span_ms(&self) -> Result<u64> {
        Ok(self.lock()?.span_ms())
    }

    /// What the ring holds, for the status line.
    pub fn stats(&self) -> Result<MemoryStats> {
        Ok(self.lock()?.stats())
    }

    /// Bytes held in RAM.
    pub fn bytes(&self) -> Result<u64> {
        Ok(self.stats()?.ram_bytes)
    }

    /// A memory ring starts empty every run (nothing to adopt, no segment number to continue); the
    /// accessor exists so the engine can treat both rings alike.
    pub fn origin_ms(&self) -> u64 {
        0
    }

    /// Wait for the reader to finish.
    ///
    /// Called after the encoder is flushed and its child has exited, which closes the pipe, so this
    /// returns promptly. Returns whether the reader ended cleanly, so a shutdown can report a dead ring.
    pub fn join(&mut self) -> Result<()> {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // The reader's end of the hand-off is gone with it, so the writer drains what is queued and ends.
        if let Some(writer) = self.sink_writer.take() {
            let _ = writer.join();
        }
        match self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            Some(why) => bail!("the in-memory ring's reader stopped early: {why}"),
            None => Ok(()),
        }
    }

    /// Why the saved session's file stopped growing before the recording ended, if it did. Only
    /// meaningful after [`MemoryRing::join`].
    pub fn session_file_failure(&self) -> Option<String> {
        self.sink_failure
            .as_ref()
            .and_then(|failure| failure.lock().ok().and_then(|f| f.clone()))
    }

    /// Save a clip around `trigger_ms` out of the footage in RAM: window `[trigger - pre, trigger + post)`,
    /// written as `{stem}.mp4` in the clips directory, like the other ring.
    pub fn trigger(
        &self,
        trigger_ms: u64,
        stem: &str,
    ) -> Result<replayed_replay::splice::ClipMetadata> {
        self.trigger_window(trigger_ms, stem, self.pre_ms, self.post_ms)
    }

    pub fn bin(&self) -> &FfmpegBinaries {
        &self.bin
    }

    /// The directory the clips are written into.
    pub fn clips_dir(&self) -> &Path {
        &self.clips_dir
    }

    /// [`MemoryRing::trigger`] with this clip's own pre- and post-roll, already clamped to what the ring can hold.
    pub fn trigger_window(
        &self,
        trigger_ms: u64,
        stem: &str,
        pre_ms: u64,
        post_ms: u64,
    ) -> Result<replayed_replay::splice::ClipMetadata> {
        let out = self.clips_dir.join(format!("{stem}.mp4"));
        let source = self.clip_source(trigger_ms, pre_ms, post_ms)?;
        source.splice(&self.bin, &out, &self.encoder)
    }

    /// The footage a clip around `trigger_ms` is cut from, lifted out of the ring.
    ///
    /// The bytes are copied out from under the ring's lock and the window's own numbers travel
    /// with them, so the splice can run on a thread of its own: ffmpeg takes as long as the clip
    /// is long, and the ring has to keep filling while it does (see the module note).
    ///
    /// The window's start is the *resolved* one — the first fragment that overlaps the request —
    /// because that is where the clip's first frame really is. The requested start is only a
    /// bound on it, and dating a clip by the request is off by up to one fragment.
    pub(crate) fn clip_source(
        &self,
        trigger_ms: u64,
        pre_ms: u64,
        post_ms: u64,
    ) -> Result<crate::ClipSource> {
        if let Some(why) = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            bail!(
                "the in-memory ring stopped reading the encoder's stream, so there is no footage \
                 to clip: {why}"
            );
        }
        let start = trigger_ms.saturating_sub(pre_ms);
        let end = trigger_ms.saturating_add(post_ms);

        // Assemble under the lock, splice outside it (see the module note).
        let (bytes, audio_tracks, duration_ms, window_start_ms, held_ms, oldest_ms, newest_ms) = {
            let ring = self.lock()?;
            let held_ms = ring.span_ms();
            let oldest_ms = ring.segments().next().map(|s| s.start_ms).unwrap_or(0);
            let newest_ms = ring.segments().last().map(|s| s.end_ms).unwrap_or(0);
            let window = ring.window(start, end).with_context(|| {
                format!(
                    "nothing in the ring covers [{start}ms, {end}ms): it holds {held_ms}ms of \
                     footage from {oldest_ms}ms to {newest_ms}ms"
                )
            })?;
            window.validate()?;
            if window.truncated_front {
                tracing::warn!(
                    "only {}ms of pre-roll was buffered for a {}ms request",
                    window.duration_ms(),
                    pre_ms
                );
            }
            (
                window.assemble(),
                window.audio_tracks(),
                window.duration_ms(),
                window.start_ms(),
                held_ms,
                oldest_ms,
                newest_ms,
            )
        };

        // What was asked for, what the ring held and what the window spans: what a short clip is diagnosed from.
        tracing::info!(
            "clip from RAM: trigger={trigger_ms}ms window=[{start}ms, {end}ms) selected={duration_ms}ms \
             held={held_ms}ms footage=[{oldest_ms}ms, {newest_ms}ms] bytes={}",
            bytes.len()
        );

        Ok(crate::ClipSource::Stream {
            bytes,
            audio_tracks,
            duration_ms,
            // The ring's own origin, so the position is on the same timeline as a session's.
            start_ms: self.origin_ms().saturating_add(window_start_ms),
        })
    }
}

impl crate::pump::MediaRing for MemoryRing {
    /// Nothing to scan: this ring is fed by its own reader thread, which has pushed every fragment the
    /// encoder wrote, and a fragment is only pushed once its `mdat` arrived in full.
    fn scan(&mut self) -> Result<()> {
        Ok(())
    }

    /// Media time this ring can prove it holds — what the post-roll wait advances against.
    fn span_ms(&self) -> u64 {
        // The wait's call cannot return a `Result`. An unreadable ring reports 0, a span no post-roll
        // target is satisfied by, so the wait times out loudly rather than splicing footage that is not
        // there. The cause is already logged once by `record_failure`.
        MemoryRing::span_ms(self).unwrap_or(0)
    }
}

/// Read the encoder's stream into the ring until the stream ends.
fn read_until_eof(
    mut stream: ChildStdout,
    ring: Arc<Mutex<MemoryRingBuffer>>,
    failure: Arc<Mutex<Option<String>>>,
    mut feed: Option<SinkFeed>,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let read = match stream.read(&mut buf) {
            Ok(0) => return, // the encoder closed the pipe: a clean end of stream
            Ok(n) => n,
            // A signal interrupted the read; the stream is still open.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                // The child died or the pipe broke. Not worth failing the recording over, but the buffer is dead and a clip request must say so.
                record_failure(
                    &failure,
                    format!("reading the encoder's stream failed: {err}"),
                );
                return;
            }
        };
        // Before the ring's lock: the file gets every byte the ring does, parsed or not.
        if let Some(feed) = feed.as_mut() {
            feed.push(&buf[..read]);
        }
        let Ok(mut guard) = ring.lock() else {
            record_failure(&failure, "the ring's lock was poisoned".to_string());
            return;
        };
        if let Err(err) = guard.push(&buf[..read]) {
            record_failure(
                &failure,
                format!("the encoder's stream could not be parsed: {err:#}"),
            );
            return;
        }
    }
}

/// Record and log the first reason the reader stopped, and stay quiet about later ones.
fn record_failure(slot: &Arc<Mutex<Option<String>>>, why: String) {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        tracing::error!(
            "the in-memory replay ring has stopped: {why}. Recording continues, but no clip can \
             be taken from the buffer until it is restarted"
        );
        *guard = Some(why);
    }
}

/// A panic payload as text, for a log line and a returned error.
fn describe_panic(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_string()
    }
}

#[cfg(test)]
mod sink_tests {
    use super::*;

    #[test]
    fn the_session_file_gets_every_byte_pushed() {
        let dir = std::env::temp_dir().join(format!("lp-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_PART);
        let (mut feed, writer, failure) = start_sink(&path, 64 * 1024).unwrap();
        let mut expected = Vec::new();
        for i in 0..5u8 {
            let chunk = vec![i; 1000 + i as usize];
            feed.push(&chunk);
            expected.extend_from_slice(&chunk);
        }
        drop(feed);
        writer.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert!(failure.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writer_that_cannot_keep_up_ends_the_file_not_the_feed() {
        // A queue nobody drains, bounded at 1000 bytes: the second chunk would take it past the bound.
        let (tx, _rx) = channel::<Vec<u8>>();
        let failure = Arc::new(Mutex::new(None));
        let mut feed = SinkFeed {
            tx: Some(tx),
            queued: Arc::new(AtomicUsize::new(0)),
            limit_bytes: 1000,
            failure: Arc::clone(&failure),
        };
        feed.push(&[0u8; 600]);
        assert!(failure.lock().unwrap().is_none());
        feed.push(&[0u8; 600]);
        assert!(failure
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .contains("keep up"));
        // Later pushes are no-ops rather than blocking or panicking.
        feed.push(b"three");
        assert!(feed.tx.is_none());
    }

    #[test]
    fn the_queue_is_bounded_in_bytes_not_in_reads() {
        // Many tiny reads stay far under the byte bound: the old count-of-reads bound would have given up on them.
        let (tx, rx) = channel::<Vec<u8>>();
        let failure = Arc::new(Mutex::new(None));
        let queued = Arc::new(AtomicUsize::new(0));
        let mut feed = SinkFeed {
            tx: Some(tx),
            queued: Arc::clone(&queued),
            limit_bytes: 64 * 1024,
            failure: Arc::clone(&failure),
        };
        for _ in 0..1000 {
            feed.push(&[1u8; 16]);
        }
        assert!(failure.lock().unwrap().is_none());
        assert_eq!(queued.load(Ordering::Relaxed), 16_000);
        drop(rx);
        // And a few large reads past the byte bound are refused, whatever their count.
        let (tx, _rx) = channel::<Vec<u8>>();
        let mut feed = SinkFeed {
            tx: Some(tx),
            queued: Arc::new(AtomicUsize::new(0)),
            limit_bytes: 64 * 1024,
            failure: Arc::clone(&failure),
        };
        feed.push(&vec![2u8; 40 * 1024]);
        feed.push(&vec![2u8; 40 * 1024]);
        assert!(feed.tx.is_none());
        assert!(failure.lock().unwrap().is_some());
    }

    #[test]
    fn a_session_file_that_cannot_be_created_fails_the_start() {
        let path = std::env::temp_dir()
            .join("lp-no-such-dir-xyz")
            .join(SESSION_PART);
        assert!(start_sink(&path, 1).is_err());
    }
}
