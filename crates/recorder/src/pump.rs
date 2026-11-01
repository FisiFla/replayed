//! Frame/audio pump shared by the steady-state loop and the clip trigger: one rate limiter,
//! the accounting, and the post-roll wait.
//!
//! Independent of [`crate::Recorder`] (backends, encoder and ring are arguments) so tests can
//! drive it with the synthetic stubs.

use anyhow::{bail, Context, Result};
use replayed_capture::{AudioBackend, CaptureBackend, Frame};
use replayed_encoder::Encoder;
use std::time::{Duration, Instant};

/// Longest a single `next_frame` call waits for a frame to become due.
///
/// Doubles as the pump's tick: there is no separate `sleep`, because sleeping starves the encoder.
pub const FRAME_POLL: Duration = Duration::from_millis(5);

/// A media timeline the pump can wait on: it can be scanned for new segments and reports its span.
///
/// `scan` is a no-op for rings fed by their own reader thread.
pub trait MediaRing {
    /// Find newly written segments and index them.
    fn scan(&mut self) -> Result<()>;
    /// Media time the ring can prove it holds, in ms, run-relative.
    fn span_ms(&self) -> u64;
}

/// How often the scratch directory is re-scanned while waiting for a post-roll.
///
/// Segments appear at most once per `buffer.segment_time`, so scanning every tick wastes I/O.
pub const POST_ROLL_SCAN_INTERVAL: Duration = Duration::from_millis(50);

/// Margin added to the post-roll when sizing the wait budget.
///
/// Covers the segment ffmpeg is still appending to, its finalisation, a scan interval and the
/// splice. Generous on purpose: giving up early loses the clip the user just asked for.
pub const POST_ROLL_MARGIN: Duration = Duration::from_secs(5);

/// How far the pacer may fall behind the wall clock before it resynchronises.
///
/// Two frame intervals: below that is scheduling jitter; above it the loop stalled, and catching
/// up would burst faster than `fps` and recreate the timeline skew. See [`FramePacer::commit`].
pub const PACER_RESYNC_AFTER_INTERVALS: u32 = 2;

/// How long the achieved frame rate is measured over.
///
/// A 200ms window holds ~6 frames and quantises to +-5fps; one second is stable yet reacts fast.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);

/// Admits at most `fps` frames per second into the encoder, and drops the rest.
///
/// Capture backends may deliver faster than `encode.fps`; feeding every frame makes ffmpeg stamp
/// them at the declared rate, so media time lags the wall clock and the post-roll never arrives.
///
/// Early frames are dropped, not queued (the buffer is encoded footage on disk), and not read
/// back: the caller asks [`FramePacer::is_due`] before taking a frame, so surplus frames are
/// closed without the GPU copy (see [`pump_once_counted`]). It is a rate limiter with no backlog.
pub struct FramePacer {
    /// The rate admitted; must equal the rate the encoder child was told.
    fps: u32,
    /// When the next frame may be submitted.
    next_due: Instant,
    /// `1 / fps` as a `Duration`; unlike an `f64` accumulator it cannot drift.
    interval: Duration,
    /// Deficit beyond which `commit` resynchronises instead of catching up.
    resync_after: Duration,
}

impl FramePacer {
    /// A pacer for `fps` frames per second, with the first frame due immediately.
    ///
    /// `fps` is clamped to at least 1 (it is a divisor). **It must be the same value the encoder
    /// child is told**: pacing to 30 while the child declares 24 yields a timeline at the wrong rate
    /// (see `Recorder::start_with_measure`, where `EncodeConfig::fps` is the single binding).
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
        Self {
            fps,
            next_due: Instant::now(),
            interval,
            resync_after: interval * PACER_RESYNC_AFTER_INTERVALS,
        }
    }

    /// The rate this pacer admits, in frames per second (published as `effective_fps`).
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Whether a frame is due at `now`, i.e. whether the caller should take one off the backend.
    ///
    /// Query only: ask it before the frame is materialised so surplus frames skip the readback. The
    /// slot is consumed by [`FramePacer::commit`], so an interval with no frame is not skipped.
    pub fn is_due(&self, now: Instant) -> bool {
        now >= self.next_due
    }

    /// Advance the schedule by one interval because a frame was obtained and submitted.
    ///
    /// Call once per frame, only after [`FramePacer::is_due`] returned `true`. When the deficit is
    /// [`PACER_RESYNC_AFTER_INTERVALS`] intervals or more, the schedule jumps to `now + interval`
    /// instead of catching up, so a stall cannot turn into a burst.
    pub fn commit(&mut self, now: Instant) {
        self.next_due += self.interval;
        if now.saturating_duration_since(self.next_due) >= self.resync_after {
            self.next_due = now + self.interval;
        }
    }

    /// How long until the next slot, `Duration::ZERO` if one is already due.
    ///
    /// Lets the pump wait without spinning: `discard_pending` never blocks, unlike `next_frame`.
    pub fn time_until_due(&self, now: Instant) -> Duration {
        self.next_due.saturating_duration_since(now)
    }
}

/// What one [`pump_once_counted`] call did with the frames the backend offered.
///
/// `submitted` + `skipped` account for every offered frame; skipped ones are closed without a
/// readback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PumpCounts {
    /// Frames handed to the encoder.
    pub submitted: u64,
    /// Frames the backend offered that the pacer will not keep, discarded without a CPU copy
    /// ([`CaptureBackend::discard_pending`]).
    pub skipped: u64,
    /// Game-audio blocks handed to the encoder.
    ///
    /// Counted because it is the one number that says whether the audio side is working at
    /// all: a run whose blocks all sit in the encoder's queue, dropped, never reached the
    /// child, and the encoder's own report names the child rather than the track.
    pub audio_blocks: u64,
    /// Microphone blocks handed to the encoder, counted the same way.
    pub mic_blocks: u64,
    /// Time spent inside the capture backend (`next_frame` + `discard_pending`): the GPU readback,
    /// the largest per-frame cost, so it is measured.
    pub capture: Duration,
    /// Time spent draining the capture backend's frame pool — the frames the pacer will not keep.
    ///
    /// It is *inside* [`PumpCounts::capture`], but it is a different cost: no readback happens
    /// here, only the pool is asked whether it has a frame and told to release it. Lumping the
    /// two together made the status line's `capture=` share mean two different things at once,
    /// which is how a 4K recording could spend three quarters of its wall clock in the capture
    /// backend and nobody could say how much of it was the readback.
    pub discard: Duration,
    /// Time the pump spent waiting for the next frame to be due.
    ///
    /// It was in neither [`PumpCounts::capture`] nor [`PumpCounts::submit`], so a share of
    /// every second was unattributed and the three visible shares did not add up.
    pub pacer_sleep: Duration,
    /// Time spent inside the encoder's submit calls. Only video blocks in practice, on the bounded
    /// queue draining: this is backpressure (waiting for ffmpeg).
    pub submit: Duration,
    /// The new frame geometry, when the capture backend reported that the captured thing changed size
    /// during this pump ([`CaptureBackend::take_size_change`]).
    ///
    /// The pump does not rebuild the encoder: the encoder is borrowed here and owned by the caller,
    /// who must finish the segment the old one is writing and start one declared for this size before
    /// the next pump. What the pump does is refuse to feed the old encoder a frame of the new size:
    /// the frame in hand when the change was reported is dropped (counted in
    /// [`PumpCounts::skipped`], one frame at the pacer's rate), and the pacer is not advanced for
    /// it, so the next pump takes the next frame at once. A caller that never reads this field is
    /// not silently wrong: the next frame fails [`guard_frame_size`], naming both sizes.
    pub resized: Option<(u32, u32)>,
}

impl PumpCounts {
    /// The counters and the timings added together, for a caller accumulating several pumps.
    pub fn plus(self, other: PumpCounts) -> PumpCounts {
        PumpCounts {
            submitted: self.submitted + other.submitted,
            skipped: self.skipped + other.skipped,
            audio_blocks: self.audio_blocks + other.audio_blocks,
            mic_blocks: self.mic_blocks + other.mic_blocks,
            capture: self.capture + other.capture,
            discard: self.discard + other.discard,
            pacer_sleep: self.pacer_sleep + other.pacer_sleep,
            submit: self.submit + other.submit,
            // The newer report wins; a size is not additive.
            resized: other.resized.or(self.resized),
        }
    }
}

/// Submit every video frame and audio block that is due right now; see [`pump_once_counted`].
pub fn pump_once(
    pacer: &mut FramePacer,
    capture: &mut dyn CaptureBackend,
    audio: Option<&mut (dyn AudioBackend + '_)>,
    encoder: &mut dyn Encoder,
) -> Result<()> {
    pump_once_counted(pacer, capture, audio, encoder)?;
    Ok(())
}
/// [`pump_once`], reporting what became of the frames the backend offered.
///
/// Both the steady-state loop and the post-roll wait pump through here. Video order matters:
///
/// 1. Ask the pacer first ([`FramePacer::is_due`]). When nothing is due, `discard_pending` closes
///    the held frames without the readback (a 33.2MB copy at 3840x2160).
/// 2. When due, take a frame (`next_frame` waits at most [`FRAME_POLL`]), then commit the slot, so
///    a timed-out `next_frame` leaves the pacer still due rather than skipping a slot.
/// 3. Ask the backend whether the captured thing changed size ([`CaptureBackend::take_size_change`]).
///    If so the frame in hand is dropped and the new size is reported in [`PumpCounts::resized`] for
///    the caller to rebuild the encoder; otherwise submit it once [`guard_frame_size`] has checked it
///    against the rawvideo pipe's size.
///
/// The not-due branch must wait (capped at [`FRAME_POLL`], never past the next slot):
/// `discard_pending` never blocks, so without it the loop would spin at 100% CPU.
///
/// Audio: drain every block already due, unpaced. Blocks are 10ms against 16.7ms video frames at
/// 60fps, so one block per iteration would run audio slow; its timeline is the exact 48kHz sample
/// count, so throttling it would create desync. `audio` is `None` when there is no game-audio track.
///
/// A frame the backend did not produce is not counted; a submit that errors aborts.
pub fn pump_once_counted(
    pacer: &mut FramePacer,
    capture: &mut dyn CaptureBackend,
    audio: Option<&mut (dyn AudioBackend + '_)>,
    encoder: &mut dyn Encoder,
) -> Result<PumpCounts> {
    pump_once_counted_with_mic(pacer, capture, audio, None, encoder)
}

/// [`pump_once_counted`], with the microphone input drained too.
///
/// The mic has its own writer thread inside the encoder, so this one pump thread submits to two
/// queues; writing both inputs synchronously from one thread deadlocks. `mic` is `None` when
/// the recording has no microphone track.
pub fn pump_once_counted_with_mic(
    pacer: &mut FramePacer,
    capture: &mut dyn CaptureBackend,
    audio: Option<&mut (dyn AudioBackend + '_)>,
    mic: Option<&mut (dyn AudioBackend + '_)>,
    encoder: &mut dyn Encoder,
) -> Result<PumpCounts> {
    let mut counts = PumpCounts::default();
    if pacer.is_due(Instant::now()) {
        let before = Instant::now();
        let frame = capture.next_frame(FRAME_POLL)?;
        counts.capture += before.elapsed();
        // The backend's contract: ask after *every* `next_frame`, whether or not it returned a frame,
        // and before the frame is used. A frame returned after a `Some` is of the new geometry, and
        // the encoder's rawvideo pipe is declared for the old one, so it must not reach it.
        let resized = capture.take_size_change();
        if let Some(size) = resized {
            counts.resized = Some(size);
            // One frame lost at a resize, accounted for as an offered frame the pump did not keep.
            counts.skipped += u64::from(frame.is_some());
        } else if let Some(frame) = frame {
            guard_frame_size(&frame, encoder.source_size())?;
            // Commit after the frame exists, so `is_due` stays a pure query.
            pacer.commit(Instant::now());
            // Ownership moves into the encoder so the buffer is queued, not cloned (33.2MB per frame at
            // 3840x2160 BGRA). Blocking here means the encoder queue is full, i.e. waiting on ffmpeg.
            let before = Instant::now();
            encoder.submit_video(frame)?;
            counts.submit += before.elapsed();
            counts.submitted += 1;
        }
    } else {
        let before = Instant::now();
        counts.skipped += capture.discard_pending()? as u64;
        let discarded = before.elapsed();
        counts.capture += discarded;
        // Measured on its own, so the status line can say how much of the capture backend's time
        // was the pool drain rather than the readback.
        counts.discard += discarded;
        // Measured from a fresh `now` because the discard took time; saturates to zero if already due.
        let before = Instant::now();
        std::thread::sleep(pacer.time_until_due(Instant::now()).min(FRAME_POLL));
        // The wait for the next slot is wall clock spent doing nothing. It was in neither
        // `capture` nor `submit`, so the shares the status line printed did not add up to a
        // second. This is the time the sleep actually took, not the time it asked for: a sleep
        // overshoots by the scheduler's granularity, and the overshoot is part of the cost.
        counts.pacer_sleep += before.elapsed();
    }
    let before = Instant::now();
    // `None`: no game audio was captured (`EncodeConfig::game_audio` false), nothing to drain.
    if let Some(audio) = audio {
        while let Some(block) = audio.next_buffer(Duration::ZERO)? {
            encoder.submit_audio(block)?;
            counts.audio_blocks += 1;
        }
    }
    // Drained identically: a block left in the backend is a hole in the voice track.
    if let Some(mic) = mic {
        while let Some(block) = mic.next_buffer(Duration::ZERO)? {
            encoder.submit_mic_audio(block)?;
            counts.mic_blocks += 1;
        }
    }
    counts.submit += before.elapsed();
    Ok(counts)
}

/// Bytes one BGRA pixel occupies: blue, green, red, alpha.
const BYTES_PER_PIXEL: usize = 4;

/// The bytes a frame of `width` × `height` BGRA pixels occupies.
///
/// Saturating, and in `usize`: the geometry is a pair of `u32`s from a capture backend, and a
/// frame's size is three of them multiplied, which does not fit in a `u32` at the sizes a
/// display can be — 3840x2160 is 33,177,600 bytes on its own, and a `u32` multiplication of
/// three such numbers wraps.
///
/// A wrapped size is worse than a wrong one, because it is *small*: a buffer sized for a
/// fraction of the frame, and the frame then read as if it were whole — diagonal banding and a
/// stream desync — instead of the size being refused. Saturating rather than checked, because
/// a size that cannot be represented is a frame no machine holds, and the caller compares this
/// against a buffer that does exist.
pub fn frame_bytes(width: u32, height: u32) -> usize {
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(BYTES_PER_PIXEL)
}

/// Refuse a frame whose geometry disagrees with the rawvideo pipe's declared size, or whose
/// pixels are not the size that geometry says.
///
/// The pipe is a flat byte stream with no framing, so a wrong-sized frame is mis-read silently
/// (diagonal banding, healthy-looking segments). Seen on a 4K desktop at 150% scaling, where the
/// pipe was declared 2560x1440 (DPI-virtualised `GetSystemMetrics`) but frames were 3840x2160.
/// A mismatch means `build_encode_config` and the backend disagree; stop loudly.
///
/// The geometry alone is not the whole contract: a frame that *claims* 3840x2160 and carries
/// 33,177,599 bytes is one byte short of the frame the pipe was declared for, and ffmpeg reads
/// the pipe as a byte stream — so the short frame is the start of the next frame's pixels. The
/// size is computed by [`frame_bytes`], which cannot wrap, so the comparison is against the
/// frame's real size and not against a wrapped one.
///
/// A frame with no pixels is not this check's business: on the zero-copy path the pixels are in
/// VRAM and the frame carries a texture, so the byte count is the texture's and not the
/// buffer's.
pub fn guard_frame_size(frame: &Frame, configured: (u32, u32)) -> Result<()> {
    if (frame.width, frame.height) != configured {
        bail!(
            "capture produced a {}x{} frame, but the encoder's raw video pipe was \
             declared for the configured source size {}x{}: ffmpeg reads that pipe as a \
             flat byte stream, so a frame of any other size would be mis-read (garbled \
             bands, stream desync) rather than reported",
            frame.width,
            frame.height,
            configured.0,
            configured.1
        );
    }
    if frame.has_pixels() {
        let expected = frame_bytes(frame.width, frame.height);
        if frame.data.len() != expected {
            bail!(
                "capture produced a {}x{} frame carrying {} bytes, but a frame of that \
                 size is {} bytes: ffmpeg reads the raw video pipe as a flat byte stream, \
                 so a frame of any other length is mis-read (garbled bands, stream desync) \
                 rather than reported",
                frame.width,
                frame.height,
                frame.data.len(),
                expected
            );
        }
    }
    Ok(())
}

/// Wait until the ring's segment span reaches `need_ms`, keeping the encoder fed.
///
/// Not a sleep: ffmpeg only finalises segments while frames keep arriving on the rawvideo pipe,
/// so a loop that merely re-scanned would never see `span_ms` move. It pumps through the same
/// [`FramePacer`] as the main loop so both paths share one rate limit.
///
/// `budget` is wall clock, `need_ms` and the span are media time (which only counts finished
/// segments), so callers size it as the post-roll plus [`POST_ROLL_MARGIN`].
///
/// Returns the pumped counts once the span covers `need_ms` (immediately if it already does);
/// errors if the budget elapses first or the capture/encode path fails. The ring and the
/// `audio`/`mic` inputs (`None` = no such track) are parameters so there is one wait loop.
// The wait loop needs all these handles to keep feeding one encoder; a struct would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn pump_until_span_on(
    pacer: &mut FramePacer,
    ring: &mut (dyn MediaRing + '_),
    capture: &mut dyn CaptureBackend,
    audio: Option<&mut (dyn AudioBackend + '_)>,
    mic: Option<&mut (dyn AudioBackend + '_)>,
    encoder: &mut dyn Encoder,
    need_ms: u64,
    budget: Duration,
) -> Result<PumpCounts> {
    let started = Instant::now();
    let deadline = started + budget;
    let mut counts = PumpCounts::default();
    // Scan on the first pass, then on the interval: the span only moves when a segment finalises.
    let mut next_scan = started;
    // Borrowed for the whole wait; re-taking an `Option<&mut dyn _>` per iteration will not borrow-check.
    let mut audio = audio;
    let mut mic = mic;

    loop {
        counts = counts.plus(pump_once_counted_with_mic(
            pacer,
            capture,
            audio.as_deref_mut(),
            mic.as_deref_mut(),
            encoder,
        )?);

        if Instant::now() >= next_scan {
            ring.scan().context("scanning for the post-roll")?;
            next_scan = Instant::now() + POST_ROLL_SCAN_INTERVAL;
            let span = ring.span_ms();
            if span >= need_ms {
                tracing::debug!(
                    "post-roll on disk: span={span}ms covers {need_ms}ms after {}ms \
                     ({} frames submitted while waiting)",
                    started.elapsed().as_millis(),
                    counts.submitted
                );
                return Ok(counts);
            }
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out after {}ms waiting for post-roll (span={}ms need={}ms): the \
                 encoder produced no segment covering the trigger",
                started.elapsed().as_millis(),
                ring.span_ms(),
                need_ms
            );
        }
    }
}

/// The rate frames are actually reaching the encoder at, averaged over `window`.
///
/// Distinct from the pacer's admitted rate and the source's rate; they differ when the machine
/// cannot encode the declared rate. `record` runs on every pump, and the reported value is the last
/// completed window, so a fresh meter reports 0.0 (a partial first window is noise).
#[derive(Debug)]
pub struct RateMeter {
    /// Window length; it ends at the first `record` after this much time.
    window: Duration,
    /// Frames recorded in the window that is currently open.
    frames: u64,
    /// When the open window started.
    since: Instant,
    /// The rate measured over the last *completed* window; 0.0 before the first one.
    fps: f64,
}

impl RateMeter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            frames: 0,
            since: Instant::now(),
            fps: 0.0,
        }
    }

    /// Add `frames` to the open window and return the achieved rate.
    ///
    /// `now` is a parameter so the arithmetic is testable without sleeping.
    pub fn record(&mut self, frames: u64, now: Instant) -> f64 {
        self.frames += frames;
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed >= self.window {
            // Divide by the elapsed time, not the nominal window: the window closes on a pump up to
            // [`FRAME_POLL`] late, and the nominal length would overstate the rate.
            self.fps = self.frames as f64 / elapsed.as_secs_f64();
            self.frames = 0;
            self.since = now;
        }
        self.fps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use replayed_capture::stub::StubAudio;
    use replayed_capture::{AudioBuffer, AudioFormat, PixelFormat};
    use replayed_encoder::{EncodeConfig, FfmpegEncoder, VideoCodec};

    /// A capture backend that spends a known time in `next_frame`, so `counts.capture` has a floor to check.
    struct SlowCapture {
        size: (u32, u32),
        spend: Duration,
    }

    impl CaptureBackend for SlowCapture {
        fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _timeout: Duration) -> anyhow::Result<Option<Frame>> {
            std::thread::sleep(self.spend);
            let (width, height) = self.size;
            Ok(Some(Frame {
                data: vec![0u8; frame_bytes(width, height)],
                pts: Duration::ZERO,
                width,
                height,
                format: PixelFormat::Bgra8,
                texture: None,
            }))
        }
        fn native_size(&self) -> (u32, u32) {
            self.size
        }
    }

    /// A capture backend that has nothing on the first ask and a frame on the second.
    ///
    /// This is the shape of a frame that arrives after the wait's budget ran out, which at
    /// 120fps is the ordinary case rather than a rare one: the interval is 8.3ms and the
    /// budget is 5ms.
    struct LateCapture {
        size: (u32, u32),
        asks: usize,
    }

    impl CaptureBackend for LateCapture {
        fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _timeout: Duration) -> anyhow::Result<Option<Frame>> {
            self.asks += 1;
            if self.asks == 1 {
                return Ok(None);
            }
            let (width, height) = self.size;
            Ok(Some(Frame {
                data: vec![0u8; frame_bytes(width, height)],
                pts: Duration::ZERO,
                width,
                height,
                format: PixelFormat::Bgra8,
                texture: None,
            }))
        }
        fn native_size(&self) -> (u32, u32) {
            self.size
        }
    }

    /// An encoder that counts what it was given and does nothing with it.
    ///
    /// The pump's questions are about pacing and attribution, not about the encode, and a
    /// real ffmpeg child would make every one of them cost a process spawn.
    struct CountingEncoder {
        size: (u32, u32),
        fps: u32,
        submitted: u64,
    }

    impl Encoder for CountingEncoder {
        fn submit_video(&mut self, _frame: Frame) -> anyhow::Result<()> {
            self.submitted += 1;
            Ok(())
        }
        fn submit_audio(&mut self, _audio: AudioBuffer) -> anyhow::Result<()> {
            Ok(())
        }
        /// The microphone input is accepted, so a pump can hand one over: the default
        /// implementation refuses it, and a refused block is not a handed-over block.
        fn submit_mic_audio(&mut self, _audio: AudioBuffer) -> anyhow::Result<()> {
            Ok(())
        }
        fn finish(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn active_encoder(&self) -> &'static str {
            "counting"
        }
        fn source_size(&self) -> (u32, u32) {
            self.size
        }
        fn input_fps(&self) -> u32 {
            self.fps
        }
    }

    /// An encoder whose hardware thread has died: every submit says so.
    struct DeadHardwareEncoder;

    impl Encoder for DeadHardwareEncoder {
        fn submit_video(&mut self, _frame: Frame) -> anyhow::Result<()> {
            anyhow::bail!("the hardware encoder stopped: the MFT worker thread exited")
        }
        fn submit_audio(&mut self, _audio: AudioBuffer) -> anyhow::Result<()> {
            Ok(())
        }
        fn finish(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn active_encoder(&self) -> &'static str {
            "dead"
        }
        fn source_size(&self) -> (u32, u32) {
            (8, 8)
        }
        fn input_fps(&self) -> u32 {
            1_000
        }
    }

    /// A hardware encoder that stopped is a failure the pump reports, once, with the encoder's reason.
    ///
    /// The pump must not swallow it and carry on (a recording of nothing that looks healthy), and the
    /// engine's loop ends on the first error it is handed (`Engine::stop_for`), so it cannot spin on it.
    #[test]
    fn a_hardware_encoder_that_stopped_fails_the_pump_with_its_reason() {
        use replayed_capture::stub::{StubCapture, StubConfig};
        let mut capture = StubCapture::new(StubConfig {
            width: 8,
            height: 8,
            fps: 1_000,
        });
        capture.start().expect("start the stub capture");
        let (mut pacer, _) = pacer(1_000);
        let mut encoder = DeadHardwareEncoder;
        let mut outcome = Ok(PumpCounts::default());
        for _ in 0..200 {
            outcome = pump_once_counted(&mut pacer, &mut capture, None, &mut encoder);
            if outcome.is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let err = outcome.expect_err("the dead encoder's submit failure reaches the caller");
        assert!(
            format!("{err:#}").contains("the hardware encoder stopped"),
            "the reason is the encoder's own: {err:#}"
        );
    }

    /// A real ffmpeg child at 8x8, in its own temp dir (the returned guard keeps it alive) so
    /// tests do not interfere through a shared scratch directory.
    fn tiny_encoder() -> (FfmpegEncoder, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir for the encoder's scratch");
        let cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            8,
            8,
            30,
            dir.path().to_path_buf(),
            1_000,
        );
        let bin = replayed_media::FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
        (
            FfmpegEncoder::spawn(&bin, &cfg).expect("spawn the encoder"),
            dir,
        )
    }

    /// The attribution counters measure the calls they claim to, and `plus` accumulates every field.
    #[test]
    fn the_pump_attributes_its_time_to_the_capture_and_to_the_encoder() {
        let spend = Duration::from_millis(25);
        let mut capture = SlowCapture {
            size: (8, 8),
            spend,
        };
        let mut audio = StubAudio::new(AudioFormat::default());
        audio.start().expect("start the stub audio");
        let (mut encoder, _scratch) = tiny_encoder();
        let (mut pacer, _) = pacer(30);

        let started = Instant::now();
        let counts = pump_once_counted(&mut pacer, &mut capture, Some(&mut audio), &mut encoder)
            .expect("one pump against a real encoder");
        let elapsed = started.elapsed();
        encoder.finish().expect("flush");

        assert_eq!(counts.submitted, 1, "the frame was admitted");
        assert!(
            counts.capture >= spend,
            "the backend's own time must be attributed to the backend: {:?} against a {:?} \
             sleep in next_frame",
            counts.capture,
            spend
        );
        assert!(
            counts.capture <= elapsed,
            "and it cannot exceed the pump that contains it: {:?} of {:?}",
            counts.capture,
            elapsed
        );

        // A healthy submit never blocks, so this only asserts the counter is live.
        assert!(
            counts.submit <= elapsed,
            "the submit's wait is inside the pump as well"
        );

        // The two new shares are given different values on each side, so an accumulator that
        // dropped or replaced one of them would show up here rather than in the status line.
        let discard = Duration::from_millis(5);
        let pacer_sleep = Duration::from_millis(7);
        let summed = PumpCounts {
            submitted: 2,
            skipped: 3,
            audio_blocks: 4,
            mic_blocks: 5,
            capture: spend,
            discard,
            pacer_sleep,
            submit: spend,
            resized: None,
        }
        .plus(PumpCounts {
            submitted: 5,
            skipped: 7,
            audio_blocks: 6,
            mic_blocks: 7,
            capture: spend,
            discard: Duration::from_millis(1),
            pacer_sleep: Duration::from_millis(2),
            submit: spend,
            resized: Some((4, 4)),
        });
        assert_eq!(
            (
                summed.submitted,
                summed.skipped,
                summed.audio_blocks,
                summed.mic_blocks,
                summed.capture,
                summed.discard,
                summed.pacer_sleep,
                summed.submit
            ),
            (
                7,
                10,
                10,
                12,
                spend * 2,
                discard + Duration::from_millis(1),
                pacer_sleep + Duration::from_millis(2),
                spend * 2
            ),
            "every field accumulates, timings included"
        );
        assert_eq!(
            summed.resized,
            Some((4, 4)),
            "a size report survives the sum"
        );
    }

    /// The audio blocks the pump handed to the encoder are counted.
    ///
    /// The count is what says whether the audio side is working: the encoder's own report
    /// names the child, not the track, so a run whose blocks were all dropped on the way to
    /// the child looks the same as a run whose audio never started unless this is counted.
    #[test]
    fn the_pump_counts_the_audio_blocks_it_handed_over() {
        let mut capture = SlowCapture {
            size: (8, 8),
            spend: Duration::ZERO,
        };
        let mut audio = StubAudio::new(AudioFormat::default());
        audio.start().expect("start the stub audio");
        let mut mic = replayed_capture::StubMicrophone::new(AudioFormat::default(), 10);
        mic.start().expect("start the stub microphone");
        let mut encoder = CountingEncoder {
            size: (8, 8),
            fps: 30,
            submitted: 0,
        };
        let (mut pacer, _) = pacer(30);

        let counts = pump_once_counted_with_mic(
            &mut pacer,
            &mut capture,
            Some(&mut audio),
            Some(&mut mic),
            &mut encoder,
        )
        .expect("one pump");

        assert_eq!(
            counts.audio_blocks, 1,
            "the block the game-audio backend had due was handed over and counted"
        );
        assert_eq!(
            counts.mic_blocks, 1,
            "the block the microphone had due was handed over and counted"
        );
    }

    /// A resize is reported to the caller, and the frame of the new size never reaches the old encoder.
    ///
    /// The capture contract is "ask after every `next_frame`, before the frame is used". Without
    /// the ask, the first frame of the new size fails [`guard_frame_size`] and ends the recording; with
    /// it, the caller gets the new size, rebuilds the encoder, and the next frame is admitted.
    #[test]
    fn a_resize_is_reported_and_the_frame_in_hand_is_not_fed_to_the_old_encoder() {
        use replayed_capture::stub::{StubCapture, StubConfig};
        let mut capture = StubCapture::new(StubConfig {
            width: 8,
            height: 8,
            fps: 1_000,
        })
        .resize_at_frame(2, (16, 12));
        capture.start().expect("start the stub capture");
        let mut encoder = CountingEncoder {
            size: (8, 8),
            fps: 1_000,
            submitted: 0,
        };
        let (mut pacer, _) = pacer(1_000);

        let mut total = PumpCounts::default();
        for _ in 0..200 {
            let counts = pump_once_counted(&mut pacer, &mut capture, None, &mut encoder)
                .expect("a resize is not an error: the guard must not see the new frame");
            total = total.plus(counts);
            if counts.resized.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(total.resized, Some((16, 12)));
        assert_eq!(
            encoder.submitted, 2,
            "the two frames of the old size were submitted and the new one was not"
        );
        assert!(
            total.skipped >= 1,
            "the dropped frame is an offered frame that was not kept: {total:?}"
        );

        // The caller rebuilt its encoder; the very next frame is of the new size and is admitted.
        encoder.size = (16, 12);
        let mut after = PumpCounts::default();
        for _ in 0..200 {
            after = after.plus(
                pump_once_counted(&mut pacer, &mut capture, None, &mut encoder)
                    .expect("frames of the new size are admitted by the rebuilt encoder"),
            );
            if after.submitted > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(after.resized, None, "a change is reported once");
        assert_eq!(encoder.submitted, 3);
    }

    /// A pacer plus an instant just after its schedule start; tests express times relative to it
    /// (margins of >= 1ms dwarf the offset) and never sleep.
    fn pacer(fps: u32) -> (FramePacer, Instant) {
        let p = FramePacer::new(fps);
        (p, Instant::now())
    }

    /// The video half of [`pump_once_counted`]: ask whether a slot is due, spend one only if so.
    fn frame_arrived(p: &mut FramePacer, now: Instant) -> bool {
        if !p.is_due(now) {
            return false; // the pump's discard branch: no frame is taken off the backend
        }
        p.commit(now);
        true
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_frame_that_is_due_is_admitted() {
        let (mut p, t0) = pacer(10);
        assert!(
            frame_arrived(&mut p, t0),
            "the first frame is due immediately"
        );
        assert!(
            frame_arrived(&mut p, t0 + 100 * MS),
            "the frame at the next slot is due"
        );
    }

    #[test]
    fn a_frame_that_arrives_early_is_dropped() {
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0));
        // Half an interval early: the capture ran ahead of the encoder's rate, and in the pump
        // this frame is never read back.
        assert!(
            !frame_arrived(&mut p, t0 + 50 * MS),
            "a frame before its slot must be dropped, not queued and not submitted"
        );
        // Dropping it did not move the schedule.
        assert!(!frame_arrived(&mut p, t0 + 99 * MS), "still early");
        assert!(
            frame_arrived(&mut p, t0 + 100 * MS),
            "the slot itself is admitted"
        );
    }

    #[test]
    fn a_capture_running_faster_than_configured_is_limited_to_the_configured_rate() {
        // 60fps capture against a 30fps config: half the frames are dropped, not counted as submitted.
        let (mut p, t0) = pacer(30);
        let mut admitted = 0;
        for i in 0..60 {
            if frame_arrived(&mut p, t0 + Duration::from_micros(i * 1_000_000 / 60)) {
                admitted += 1;
            }
        }
        assert!(
            (29..=31).contains(&admitted),
            "one second of 60fps capture must yield ~30 admitted frames, got {admitted}"
        );
    }

    /// The pacer admits the rate it is given, including the rates a display runs at.
    ///
    /// A 120Hz panel is worth recording at 120fps, and the pacer is what turns that number
    /// into slots. The clock is simulated, as the other pacer tests do, so this asserts the
    /// schedule and not the machine's speed.
    #[test]
    fn the_pacer_admits_a_hundred_and_twenty_frames_a_second() {
        let (mut p, t0) = pacer(120);
        let mut admitted = 0;
        for i in 0..120u64 {
            // Nanoseconds, not microseconds: the 120fps interval is 8,333,333.33ns, and a
            // clock in whole microseconds drifts a third of a microsecond per frame, which
            // by the hundred-and-nineteenth frame is a whole frame late.
            if frame_arrived(&mut p, t0 + Duration::from_nanos(i * 1_000_000_000 / 120)) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, 120,
            "every one of the 120 slots in a second is admitted"
        );
    }

    /// A frame that arrives after the wait's budget is not lost.
    ///
    /// The wait is bounded so the pump cannot stall on a backend that never delivers. At
    /// 120fps the frame interval is 8.3ms and the budget is 5ms, so a frame that lands
    /// late is the ordinary case rather than the rare one — and it must not be dropped. The
    /// pacer stays due after a timed-out wait, so the next turn takes the frame instead of
    /// skipping the slot.
    #[test]
    fn a_frame_that_arrives_after_the_wait_budget_is_taken_on_the_next_turn() {
        let (mut p, _t0) = pacer(120);
        let mut capture = LateCapture {
            size: (8, 8),
            asks: 0,
        };
        let mut encoder = CountingEncoder {
            size: (8, 8),
            fps: 120,
            submitted: 0,
        };

        let first = pump_once_counted(&mut p, &mut capture, None, &mut encoder)
            .expect("a pump whose frame has not arrived");
        assert_eq!(first.submitted, 0, "there was no frame to take");
        assert!(
            p.is_due(Instant::now()),
            "a timed-out wait must leave the slot due rather than skip it"
        );

        let second = pump_once_counted(&mut p, &mut capture, None, &mut encoder)
            .expect("a pump whose frame has arrived");
        assert_eq!(
            second.submitted, 1,
            "the frame that arrived late is taken, not skipped"
        );
        assert_eq!(encoder.submitted, 1, "and it reached the encoder");
    }

    #[test]
    fn a_long_stall_resynchronises_instead_of_bursting() {
        // 10fps: 100ms per frame, so the resync threshold is 200ms of deficit.
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0));

        // A five-second stall: the late frame is admitted but the schedule must not stay in the past.
        let after_stall = t0 + Duration::from_secs(5);
        assert!(
            frame_arrived(&mut p, after_stall),
            "a late frame is still a frame to encode"
        );

        // A catch-up burst would admit frames until the deficit was paid; instead the pacer resynced
        // to `after_stall + interval`.
        let burst = (1..=5)
            .filter(|i| frame_arrived(&mut p, after_stall + Duration::from_micros(i * 10_000)))
            .count();
        assert_eq!(
            burst, 0,
            "a stalled pacer must resume at the configured rate, not emit a catch-up burst"
        );
        assert!(
            frame_arrived(&mut p, after_stall + 100 * MS),
            "the schedule resumed one interval after the stall"
        );
    }

    #[test]
    fn a_small_lag_is_absorbed_without_resynchronising() {
        let (mut p, t0) = pacer(10);
        assert!(frame_arrived(&mut p, t0));
        // 50ms late is jitter: the schedule stays on the original grid.
        assert!(frame_arrived(&mut p, t0 + 150 * MS));
        assert!(
            frame_arrived(&mut p, t0 + 205 * MS),
            "the grid must still be at t0 + 200ms: a resync here would have pushed it to \
             t0 + 250ms and dropped this frame"
        );
    }

    #[test]
    fn asking_whether_a_frame_is_due_does_not_spend_the_slot() {
        // The pump asks before paying for a frame; only a frame that arrives spends a slot.
        let (mut p, t0) = pacer(30);
        for i in 0..20 {
            let now = t0 + Duration::from_micros(i * 2_000);
            assert!(
                p.is_due(now),
                "no frame was taken, so no slot may have been spent"
            );
            assert_eq!(
                p.time_until_due(now),
                Duration::ZERO,
                "a due slot has no wait"
            );
        }
        // One slot is spent, not twenty.
        assert!(frame_arrived(&mut p, t0 + 20 * MS));
        assert!(
            !p.is_due(t0 + 30 * MS),
            "the interval that frame consumed has not elapsed"
        );
        assert!(
            p.is_due(t0 + 34 * MS),
            "the next slot is one interval after the last"
        );
    }

    #[test]
    fn a_window_that_has_closed_is_reported_as_frames_per_second() {
        // 30 frames over one second is the configured rate.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        fps_is(
            m.record(30, t0 + Duration::from_secs(1)),
            30.0,
            "the window that closed at 1.0s carried 30 frames",
        );
        // The next window is measured from its own start.
        fps_is(
            m.record(15, t0 + Duration::from_millis(1_500)),
            30.0,
            "15 frames in the half second since the window before",
        );
    }

    #[test]
    fn the_rate_is_zero_until_a_window_has_closed() {
        // A fresh meter reports 0.0 until a whole window is measured; a partial window is noise.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        assert_eq!(m.record(6, t0 + Duration::from_millis(200)), 0.0);
        assert_eq!(m.record(6, t0 + Duration::from_millis(400)), 0.0);
        assert_eq!(m.record(6, t0 + Duration::from_millis(600)), 0.0);
        assert_eq!(m.record(6, t0 + Duration::from_millis(800)), 0.0);
        fps_is(
            m.record(6, t0 + Duration::from_millis(1_000)),
            30.0,
            "the window closed",
        );
    }

    #[test]
    fn a_window_is_divided_by_the_time_it_really_covered() {
        // The window closes on the first record after it is due, so dividing by the nominal length would inflate the rate.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        fps_is(
            m.record(30, t0 + Duration::from_millis(1_200)),
            25.0,
            "30 frames over the 1.2s the window really covered",
        );
    }

    #[test]
    fn the_meter_reports_the_frames_that_reached_the_encoder_not_the_ones_admitted() {
        // The pacer admits 30fps, the encoder queue drops six: the meter must say 24, not the 30 `frames=` counts.
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        let mut fps = 0.0;
        for (i, reached) in [5u64, 5, 5, 5, 4].into_iter().enumerate() {
            let now = t0 + Duration::from_millis((i as u64 + 1) * 200);
            fps = m.record(reached, now);
        }
        fps_is(
            fps,
            24.0,
            "30 submitted, 6 dropped: 24 frames per second reached ffmpeg",
        );
    }

    /// Compare a measured rate to the expected one within 0.1fps (the meter's reference clock is
    /// a few microseconds off the test's `t0`).
    fn fps_is(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() < 0.1,
            "{what}: expected about {expected}fps, measured {actual}"
        );
    }
}
