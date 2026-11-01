//! The video's timeline: when a captured frame may be handed to the child, and which captured
//! frame a finished one is.
//!
//! # Why the timeline is not ffmpeg's to decide
//!
//! ffmpeg does not read timestamps out of the video it is given. It stamps each frame with the
//! wall clock at the moment it *read* that frame from the pipe, which is why the input is declared
//! with `-use_wallclock_as_timestamps 1`. A child that reads a burst of frames at once therefore
//! gets a burst of timestamps, and the picture judders: the frames of one burst share a timestamp
//! and the picture freezes between the bursts.
//!
//! That is what a 4K recording on the zero-copy path looked like. The hardware encoder finished
//! several frames at a time, so the reads from the pipe came in bursts, and the measured frame
//! intervals swung between 8.1 ms and 36.4 ms against the 16.7 ms a 60fps video has: only 2.3% of
//! them were right. The picture was not shown when it was captured, and it was not shown evenly.
//!
//! So the capture times are carried to the pipe and the pipe is written *at* those times. The
//! child then reads the frames one at a time, spread out the way the picture was, and the wall
//! clock it stamps them with is the capture clock.
//!
//! # Why the total duration does not move
//!
//! The schedule spaces the frames by the gaps between their *capture* times, so the span of the
//! written frames is the span of the capture: the video is as long as the picture was, and no
//! longer. A video whose intervals were even at the configured rate would be shorter than the
//! audio — the capture delivers about 57.6 frames a second of real time, not 60 — and that is a
//! different defect (an A/V drift), not a fix.
//!
//! The decisions that take are here, all pure, so they can be tested off Windows:
//!
//! * [`FrameSchedule`] says when a frame may be written.
//! * [`FramePairing`] says which captured frame a finished one is.
//! * [`AudioOrigin`] says where each audio input starts, relative to the first frame: ffmpeg
//!   zeroes every input at its own first data, so without it the audio and the picture start
//!   together only if their first data happened to arrive together.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long before a frame's slot the writer stops sleeping and starts waiting for it exactly.
///
/// An operating system's sleep is not accurate to the microsecond, and the error lands on the
/// frame's interval: a machine whose timers are coalesced can take 24ms for a 16ms sleep, and the
/// measured intervals of a recording then swing by as much as the sleep was wrong. The last
/// stretch is therefore waited for rather than slept through — a fraction of a millisecond of
/// spinning, once per frame.
pub const SPIN_WINDOW: Duration = Duration::from_millis(1);

/// The longest sleep the writer takes in one go.
///
/// A long sleep can overshoot by more than a step, and the overshoot is the interval's error, so
/// the wait is taken in steps and the last stretch is waited for.
pub const SLEEP_STEP: Duration = Duration::from_millis(1);

/// Wait until `due`, as precisely as this machine allows.
///
/// The wait is taken in short sleeps and the last [`SPIN_WINDOW`] is waited for exactly. Both
/// halves matter: a sleep alone is only accurate to a millisecond or so (on a machine whose
/// timers are coalesced, to several), and the error is the frame interval's error; and spinning
/// alone would burn a core for the whole of every frame interval.
pub fn wait_until(due: Instant) {
    loop {
        let now = Instant::now();
        if now >= due {
            return;
        }
        let remaining = due.saturating_duration_since(now);
        if remaining <= SPIN_WINDOW {
            // The last stretch: waited for, because a sleep cannot be this precise.
            while Instant::now() < due {
                std::hint::spin_loop();
            }
            return;
        }
        // Short enough that its own overshoot is small: the wait is repeated until the slot is
        // close enough to be waited for exactly.
        std::thread::sleep(remaining.min(SLEEP_STEP));
    }
}

/// How far behind the capture the writer may run before the schedule starts catching up.
///
/// A hardware encoder's burst puts the writer a few frame intervals behind for a moment, and that
/// is what the schedule exists to smooth, so a lag up to this is left alone. A lag beyond it can
/// only be a stall (ffmpeg not reading for a moment), and it does not go away by itself: see
/// [`FrameSchedule`].
pub const MAX_LAG: Duration = Duration::from_millis(100);

/// When each frame may be written to the child.
///
/// One schedule for the whole stream: the frames are written in the order they were captured, and
/// each one is written one *capture interval* after the last. The intervals are therefore the
/// capture's own, and the stream on the pipe is as long as the picture was.
///
/// **The schedule is never late.** Each frame's slot is the later of "the slot the last frame's
/// slot plus the gap between the two frames' capture times" and "the moment the frame arrived",
/// so the schedule's level is the *envelope* of the arrivals: a frame that arrives early waits
/// for its turn, and a frame that arrives late pushes the schedule on, and the frames after it
/// are written one capture interval apart again. That is what makes a burst of frames — the
/// hardware encoder finishing several at once — come out as a steady stream.
///
/// **A lag is drained, not carried.** Spacing the frames by their capture intervals is exactly
/// the rate at which new frames arrive, so once the writer has fallen behind (ffmpeg stops
/// reading for a moment, which it does at the first segment boundary on Windows) it stays behind
/// by the same amount for ever: the frames wait in the queue in front of the writer, the queue is
/// bounded and drops when it is full, and a half-second stall became a third of the frames
/// dropped for as long as the recording ran. The lag is measured as how much later the writer
/// took a frame, relative to its capture time, than it ever took one before; while it is more
/// than [`MAX_LAG`] the frames are written at half their capture spacing, which is faster than
/// they arrive and so empties the queue. Burst smoothing is untouched below that.
#[derive(Debug, Default)]
pub struct FrameSchedule {
    /// The capture time of the last frame written.
    last_capture: Option<Duration>,
    /// When the last frame was written.
    last_written: Option<Instant>,
    /// The moment of the first frame, the zero the offsets below are counted from.
    origin: Option<Instant>,
    /// The smallest `(now - origin) - captured_at` seen, in nanoseconds: the pipeline's best
    /// latency, which a frame that was not delayed shows.
    best_offset: Option<i128>,
}

impl FrameSchedule {
    /// How long the writer must wait before the frame captured at `captured_at` may be written.
    ///
    /// `now` is the moment the frame arrived. The answer is never negative: a frame that is
    /// already late is written at once.
    ///
    /// The rule, and why:
    ///
    /// * the picture was on the screen from one frame's capture to the next's, so the next frame
    ///   is written one capture interval after the last one was — that is what keeps the stream on
    ///   the pipe even;
    /// * a frame that arrives *early* (the encoder finished several at once) waits for its turn,
    ///   and that is what turns the encoder's burst into a steady stream;
    /// * a frame that arrives *late* (the encoder could not keep up) is written at once, because
    ///   the timeline may not be stretched to hide it.
    ///
    /// The first frame is written as soon as it is here, and it is the origin the others are
    /// measured from: an absolute schedule would have to know the pipeline's latency before the
    /// pipeline has run.
    pub fn wait_for(&mut self, captured_at: Duration, now: Instant) -> Duration {
        // How much later than its best case this frame was taken, which is the lag.
        let origin = *self.origin.get_or_insert(now);
        let offset = now.saturating_duration_since(origin).as_nanos() as i128
            - captured_at.as_nanos() as i128;
        let best = self.best_offset.map_or(offset, |best| best.min(offset));
        self.best_offset = Some(best);
        let draining = offset - best > MAX_LAG.as_nanos() as i128;

        let due = match (self.last_capture, self.last_written) {
            (Some(previous_capture), Some(previous_written)) => {
                let gap = captured_at.saturating_sub(previous_capture);
                // Faster than the frames arrive while there is a lag to be rid of.
                let gap = if draining { gap / 2 } else { gap };
                previous_written
                    // `checked_add`: a capture time far in the future would overflow an `Instant`,
                    // and a frame that cannot be placed is written now rather than never.
                    .checked_add(gap)
                    .unwrap_or(now)
            }
            _ => now,
        };
        let due = if due > now { due } else { now };
        self.last_capture = Some(captured_at);
        self.last_written = Some(due);
        due.saturating_duration_since(now)
    }
}

/// The frames an encoder has been given, and the capture time of the one it finishes next.
///
/// The encoder returns one frame for each frame it was given, in the same order, so the frames
/// that went in are the queue the frames that come out are matched against: the oldest frame
/// handed over is the next frame finished.
///
/// The pairing is a queue rather than a counter because a frame can be lost on the way in — the
/// queue in front of the encoder is bounded and drops when it is full — and a counter would then
/// be off by one for the rest of the stream. A queue loses the dropped frame with it.
#[derive(Debug, Default)]
pub struct FramePairing {
    /// The capture times of the frames handed over, oldest first.
    pending: VecDeque<Duration>,
}

impl FramePairing {
    /// A frame captured at `captured_at` was handed to the encoder.
    pub fn handed_over(&mut self, captured_at: Duration) {
        self.pending.push_back(captured_at);
    }

    /// The encoder finished a frame: the capture time of the frame it is.
    ///
    /// `None` when the encoder produced a frame that was never handed over, which is a mismatch
    /// rather than a fact about the picture. The caller writes such a frame as soon as it can,
    /// which is what happened before this pairing existed.
    pub fn finished(&mut self) -> Option<Duration> {
        self.pending.pop_front()
    }

    /// How many frames are waiting to be matched.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// The most audio an input holds while it waits for the first video frame.
///
/// The hold exists so that audio captured *before* the picture can be cut at the picture's first
/// frame (see [`AudioOrigin`]). It is bounded because a picture that never comes must not make the
/// process grow without limit, and it is kept under the audio queue's depth
/// (`AUDIO_QUEUE_BLOCKS` in `ffmpeg.rs`) so that flushing it into that queue does not overflow it.
/// When it overflows, the *oldest* audio goes: that is the audio furthest before the picture.
pub const AUDIO_HOLD_LIMIT: Duration = Duration::from_secs(2);

/// The largest gap between the first video frame and an audio input's first block that is treated
/// as a start-up offset worth correcting.
///
/// The correction assumes the two timestamps are on one clock. A gap larger than this is far more
/// likely to be two clocks that are not comparable (a backend that counts from its own start)
/// than a recording whose audio really began more than five seconds off the picture, and
/// "correcting" it would cut the whole track away or pad it with a long silence. Beyond this the
/// input is passed through as it always was, and that is the only bypass: it is logged by the
/// caller and exercised by a test.
pub const MAX_ORIGIN_SKEW: Duration = Duration::from_secs(5);

/// One block of interleaved PCM and when it was captured, as [`AudioOrigin`] sees it.
#[derive(Debug)]
pub struct TimedAudio {
    /// When the block's first frame was captured, on the same clock as the video's capture times.
    pub pts: Duration,
    /// The number of sample frames in `data`.
    pub frames: usize,
    /// The sample rate, in Hz.
    pub sample_rate: u32,
    /// The interleaved PCM.
    pub data: Vec<u8>,
}

impl TimedAudio {
    /// The bytes in one sample frame, or `None` when `data` is not a whole number of them (a
    /// block that cannot be cut at a frame boundary is never cut).
    fn bytes_per_frame(&self) -> Option<usize> {
        if self.frames == 0 || !self.data.len().is_multiple_of(self.frames) {
            return None;
        }
        Some(self.data.len() / self.frames)
    }

    /// How long the block is.
    fn span(&self) -> Duration {
        frames_to_duration(self.frames, self.sample_rate)
    }
}

/// `frames` sample frames at `sample_rate`, as a duration. Zero for a rate of zero.
fn frames_to_duration(frames: usize, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos((frames as u128 * 1_000_000_000 / u128::from(sample_rate)) as u64)
}

/// `span` as a number of sample frames at `sample_rate`, rounded to the nearest.
fn duration_to_frames(span: Duration, sample_rate: u32) -> usize {
    let frames = (span.as_nanos() * u128::from(sample_rate) + 500_000_000) / 1_000_000_000;
    usize::try_from(frames).unwrap_or(usize::MAX)
}

/// Where an audio input starts, relative to the first video frame.
///
/// # Why this exists
///
/// ffmpeg zeroes each input at the first thing it reads from it: the video at its first frame and
/// each audio input at its first block. Two inputs that were captured from the same clock but
/// whose *first* data arrived at different moments therefore start at the same output time, and
/// the later-starting one is shifted early by the difference for the rest of the file. Audio
/// capture normally starts before the first frame is delivered (a desktop that has not changed
/// has no frame to deliver), so the sound ran ahead of the picture by the length of that wait, a
/// permanent offset that nothing afterwards corrected.
///
/// The fix is to make both inputs start at the *same instant on the capture clock*: the first
/// video frame's capture time is the origin, and each audio input is cut or padded at it.
///
/// * audio that ended at or before the origin is discarded: it is from before the picture;
/// * a block that straddles the origin has its leading frames cut, to the sample;
/// * a first block that starts after the origin is preceded by silence for the gap, so that the
///   sound begins when it did relative to the picture and not at the picture's start.
///
/// Everything after the first block passes straight through; it is timed by its sample count from
/// there, which is how ffmpeg times raw PCM anyway.
///
/// # The hold
///
/// Until the origin is known the blocks are *held*, not written, because a block cannot be cut
/// at a time that is not known yet. The hold ends when the first frame is *submitted*
/// ([`AudioOrigin::open`]), never when ffmpeg reads it: ffmpeg reads the audio input's header
/// before it looks at the video (issue #78), so tying the release to ffmpeg's read would have
/// each side waiting for the other. The hold is bounded by [`AUDIO_HOLD_LIMIT`].
#[derive(Debug, Default)]
pub struct AudioOrigin {
    /// The first video frame's capture time, once it has been submitted.
    origin: Option<Duration>,
    /// Whether the first block after the origin has been placed. Later blocks are not touched.
    placed: bool,
    /// Blocks received before the origin was known, oldest first.
    held: VecDeque<TimedAudio>,
    /// How much audio `held` covers.
    held_span: Duration,
}

impl AudioOrigin {
    /// A block arrived. Returns the chunks of PCM to write now, in order: none while the origin
    /// is not known, otherwise the block (cut, or preceded by silence, if it is the first).
    pub fn push(&mut self, block: TimedAudio) -> Vec<Vec<u8>> {
        let Some(origin) = self.origin else {
            self.held_span += block.span();
            self.held.push_back(block);
            // The oldest audio is the furthest from the picture, so it is what is let go.
            while self.held_span > AUDIO_HOLD_LIMIT && self.held.len() > 1 {
                if let Some(oldest) = self.held.pop_front() {
                    self.held_span = self.held_span.saturating_sub(oldest.span());
                }
            }
            return Vec::new();
        };
        self.place(origin, block)
    }

    /// The first video frame, captured at `origin`, was submitted: the held audio is released.
    ///
    /// Idempotent: the origin is the *first* frame's, and a second call changes nothing.
    pub fn open(&mut self, origin: Duration) -> Vec<Vec<u8>> {
        if self.origin.is_some() {
            return Vec::new();
        }
        self.origin = Some(origin);
        self.held_span = Duration::ZERO;
        let held = std::mem::take(&mut self.held);
        held.into_iter()
            .flat_map(|block| self.place(origin, block))
            .collect()
    }

    /// How many blocks are waiting for the origin.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Cut, pad or pass one block, relative to `origin`.
    fn place(&mut self, origin: Duration, block: TimedAudio) -> Vec<Vec<u8>> {
        if self.placed {
            return vec![block.data];
        }
        let start = block.pts;
        let Some(bytes_per_frame) = block.bytes_per_frame() else {
            // A block that is not a whole number of frames cannot be cut on a frame boundary.
            self.placed = true;
            return vec![block.data];
        };
        // Two clocks that are not comparable: left as they were (see `MAX_ORIGIN_SKEW`).
        if start.abs_diff(origin) > MAX_ORIGIN_SKEW {
            self.placed = true;
            return vec![block.data];
        }
        let end = start + block.span();
        if end <= origin {
            // From before the picture began. The next block may still be, so not `placed`.
            return Vec::new();
        }
        self.placed = true;
        if start < origin {
            let cut = duration_to_frames(origin - start, block.sample_rate).min(block.frames);
            return vec![block.data[cut * bytes_per_frame..].to_vec()];
        }
        let gap = duration_to_frames(start - origin, block.sample_rate);
        if gap == 0 {
            return vec![block.data];
        }
        vec![vec![0u8; gap * bytes_per_frame], block.data]
    }
}

/// The capture time of a frame in the 100-nanosecond units Media Foundation counts in, measured
/// from the first frame handed over.
///
/// The encoder's stream starts at zero and its timestamps march by the real capture intervals, so
/// the stream the encoder sees is the capture's own timeline rather than a nominal grid: a frame
/// that was captured 17.4 ms after the one before it says so.
///
/// Saturating, because a capture time that cannot be represented is a frame no clock can place,
/// and the largest timestamp is closer to the truth than a wrapped one.
pub fn capture_pts(origin: Duration, captured_at: Duration) -> i64 {
    let units = captured_at.saturating_sub(origin).as_nanos() / 100;
    i64::try_from(units).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the tests can place frames on exactly.
    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    /// The first frame is written as soon as it is here: there is nothing to be spaced from.
    #[test]
    fn the_first_frame_is_written_at_once() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        let wait = schedule.wait_for(Duration::from_millis(17), at(base, 5));
        assert_eq!(
            wait,
            Duration::ZERO,
            "the first frame has no predecessor to be spaced from"
        );
    }

    /// The frames are written one capture interval apart, whatever the pipeline did in between.
    ///
    /// The defect this prevents: the encoder finished three frames at once, the child read them
    /// at once, and the three shared a timestamp — a burst of frames with no time between them
    /// and a frozen picture between the bursts.
    #[test]
    fn the_frames_are_written_one_capture_interval_apart() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        // The three frames arrive together (the encoder finished a burst) but were captured 17 ms
        // apart. The writer asks how long to wait and then sleeps for it, so its clock advances
        // by the wait — which is what the loop below does.
        let mut now = at(base, 5);
        let mut waits = Vec::new();
        for frame in 0..3u64 {
            let wait = schedule.wait_for(Duration::from_millis(frame * 17), now);
            now += wait;
            waits.push(wait);
        }
        assert_eq!(
            waits[0],
            Duration::ZERO,
            "the first frame is written at once"
        );
        assert_eq!(
            waits[1],
            Duration::from_millis(17),
            "the second waits for the interval its capture time says"
        );
        assert_eq!(
            waits[2],
            Duration::from_millis(17),
            "and the third is measured from the frame before it, not from the burst they arrived in"
        );
        assert_eq!(
            now - base,
            Duration::from_millis(39),
            "the three frames are written 17ms apart: the capture's own intervals"
        );
    }

    /// A frame that arrives late is written at once: the timeline may not be stretched to hide a
    /// pipeline that could not keep up.
    #[test]
    fn a_frame_that_arrives_late_is_not_held_back() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.wait_for(Duration::from_millis(0), at(base, 0));
        // Captured 17 ms after the first, but it only turned up 100 ms later.
        let wait = schedule.wait_for(Duration::from_millis(17), at(base, 100));
        assert_eq!(
            wait,
            Duration::ZERO,
            "a late frame is written at once, not at the slot the capture time would have given it"
        );
    }

    /// A capture time that goes backwards (a stale frame) must not move the schedule backwards.
    #[test]
    fn a_capture_time_that_goes_backwards_does_not_move_the_schedule_back() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        schedule.wait_for(Duration::from_millis(100), at(base, 0));
        let wait = schedule.wait_for(Duration::from_millis(40), at(base, 1));
        assert_eq!(
            wait,
            Duration::ZERO,
            "the gap is clamped at zero, so the frame is written straight after the last one"
        );
    }

    /// The schedule is measured from the frames' capture times, so the stream it writes is as long
    /// as the picture was — the property that keeps the video's duration on the audio's.
    #[test]
    fn the_schedule_spans_the_capture_and_not_the_configured_rate() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        // 60 frames captured 17.4 ms apart: the capture ran at 57.5 fps, not at 60.
        let mut written_at = base;
        for frame in 0..60u64 {
            let captured_at = Duration::from_micros(frame * 17_400);
            let wait = schedule.wait_for(captured_at, written_at);
            written_at += wait;
        }
        let span = written_at.duration_since(base);
        assert!(
            span >= Duration::from_millis(1_026) && span <= Duration::from_millis(1_044),
            "sixty frames captured 17.4ms apart span about 1.03s, not the 1.0s a 60fps grid \
             would claim: {span:?}"
        );
    }

    /// The pairing matches the frames in order, and a frame lost on the way in is lost with them.
    #[test]
    fn the_finished_frame_is_the_oldest_one_handed_over() {
        let mut pairing = FramePairing::default();
        pairing.handed_over(Duration::from_millis(0));
        pairing.handed_over(Duration::from_millis(17));
        pairing.handed_over(Duration::from_millis(34));
        assert_eq!(pairing.pending(), 3);
        assert_eq!(pairing.finished(), Some(Duration::from_millis(0)));
        assert_eq!(pairing.finished(), Some(Duration::from_millis(17)));
        assert_eq!(pairing.finished(), Some(Duration::from_millis(34)));
        assert_eq!(
            pairing.finished(),
            None,
            "and there is nothing left to match"
        );
    }

    /// The encoder's timestamps are the capture's own, measured from the first frame.
    #[test]
    fn the_encoder_is_told_the_real_capture_times() {
        let origin = Duration::from_millis(1_000);
        assert_eq!(
            capture_pts(origin, origin),
            0,
            "the first frame is the stream's origin"
        );
        assert_eq!(
            capture_pts(origin, origin + Duration::from_micros(17_400)),
            174_000,
            "17.4ms is 174000 units of 100ns"
        );
        // A capture time before the origin cannot be placed, and is clamped rather than wrapped.
        assert_eq!(capture_pts(origin, Duration::ZERO), 0);
    }

    /// A capture time far beyond what a timestamp can hold is clamped, not wrapped: a wrapped
    /// timestamp would put the frame at the wrong end of the stream.
    #[test]
    fn an_impossible_capture_time_is_clamped() {
        let absurd = Duration::from_secs(u64::MAX);
        assert_eq!(
            capture_pts(Duration::ZERO, absurd),
            i64::MAX,
            "a timestamp that cannot be represented is the largest one there is"
        );
    }

    /// The writer stalls (ffmpeg stops reading for a moment) and the frames that piled up behind it
    /// are written one capture interval apart, exactly as fast as new ones arrive, so the pile never
    /// shrinks and the bounded queue in front of the writer stays full and drops frames long after
    /// the stall has ended. The schedule must drain the lag, not carry it for ever.
    ///
    /// The writer is modelled as the real one is: it takes the next frame as soon as it is free (or
    /// as soon as it arrives, if it is idle), asks the schedule how long to wait, and writes. The
    /// lag is how long a frame sat before the writer took it.
    #[test]
    fn a_lag_left_by_a_stall_is_drained() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        let interval = Duration::from_micros(16_667);
        let latency = Duration::from_millis(5);
        let mut writer_free = base;
        let mut lags = Vec::new();
        for frame in 0..240u32 {
            let captured_at = interval * frame;
            let arrived = base + captured_at + latency;
            let now = arrived.max(writer_free);
            let wait = schedule.wait_for(captured_at, now);
            writer_free = now + wait;
            if frame == 30 {
                // ffmpeg stops reading for half a second, inside this frame's write.
                writer_free += Duration::from_millis(500);
            }
            lags.push(now.saturating_duration_since(arrived));
        }
        assert!(
            lags[31] >= Duration::from_millis(450),
            "the stall left a lag of half a second: {:?}",
            lags[31]
        );
        // Two seconds of capture after the stall, the pile is gone (within a few frames' burst).
        let later = lags[30 + 120];
        assert!(
            later <= MAX_LAG + interval * 3,
            "the lag was still {later:?} two seconds after the stall: the queue stays full and \
             drops frames until the stream ends"
        );
    }

    /// With no stall the schedule never speeds up: the drain is only for a lag.
    #[test]
    fn a_steady_stream_is_never_drained() {
        let base = Instant::now();
        let mut schedule = FrameSchedule::default();
        let interval = Duration::from_micros(16_667);
        let mut writer_free = base;
        for frame in 0..120u32 {
            let captured_at = interval * frame;
            let arrived = base + captured_at + Duration::from_millis(5);
            let now = arrived.max(writer_free);
            let wait = schedule.wait_for(captured_at, now);
            if frame > 0 {
                assert!(
                    now + wait <= writer_free.max(arrived) + interval,
                    "frame {frame} was written early"
                );
            }
            writer_free = now + wait;
        }
        // The span of the writes is the capture's own.
        let span = writer_free.duration_since(base);
        assert!(
            span >= interval * 119 && span <= interval * 119 + Duration::from_millis(10),
            "{span:?}"
        );
    }

    /// A 10 ms block of 48 kHz stereo s16le (480 frames of 4 bytes) captured at `pts_ms`, filled
    /// with `fill` so a test can tell which block a byte came from.
    fn block(pts_ms: u64, fill: u8) -> TimedAudio {
        TimedAudio {
            pts: Duration::from_millis(pts_ms),
            frames: 480,
            sample_rate: 48_000,
            data: vec![fill; 480 * 4],
        }
    }

    fn total_bytes(chunks: &[Vec<u8>]) -> usize {
        chunks.iter().map(Vec::len).sum()
    }

    /// Audio that arrives before the first frame is held, not written: it cannot be cut at a time
    /// that is not known yet.
    #[test]
    fn audio_is_held_until_the_first_frame_is_submitted() {
        let mut origin = AudioOrigin::default();
        assert!(origin.push(block(0, 1)).is_empty());
        assert!(origin.push(block(10, 2)).is_empty());
        assert_eq!(origin.held(), 2, "both blocks wait for the picture");
        let released = origin.open(Duration::from_millis(0));
        assert_eq!(released.len(), 2, "and are released when it is submitted");
        assert_eq!(origin.held(), 0);
    }

    /// The defect: audio started 300 ms before the first frame, ffmpeg zeroed both at their first
    /// data, and the sound ran 300 ms ahead of the picture for the whole file. The audio from
    /// before the picture is cut away so both start at the same instant.
    #[test]
    fn audio_from_before_the_first_frame_is_discarded() {
        let mut origin = AudioOrigin::default();
        // 30 blocks: 0..300 ms, then the picture's first frame at 300 ms, then more audio.
        for index in 0..30 {
            assert!(origin.push(block(index * 10, 1)).is_empty());
        }
        let released = origin.open(Duration::from_millis(300));
        assert!(
            released.is_empty(),
            "everything held ended at or before the first frame: {} bytes released",
            total_bytes(&released)
        );
        let next = origin.push(block(300, 9));
        assert_eq!(
            next,
            vec![vec![9u8; 480 * 4]],
            "the block that starts at the picture is the first thing written, whole"
        );
    }

    /// A block that straddles the first frame is cut to the sample.
    #[test]
    fn a_block_that_straddles_the_first_frame_is_cut_to_the_sample() {
        let mut origin = AudioOrigin::default();
        origin.push(block(0, 1));
        // 2.5 ms into the block is 120 frames of 48 kHz.
        let released = origin.open(Duration::from_micros(2_500));
        assert_eq!(released.len(), 1);
        assert_eq!(
            released[0].len(),
            (480 - 120) * 4,
            "the first 120 frames, which are before the picture, are cut"
        );
    }

    /// Audio that begins *after* the picture does is preceded by silence for the gap, so the
    /// sound is as late relative to the picture as it really was.
    #[test]
    fn a_first_block_after_the_first_frame_is_preceded_by_silence() {
        let mut origin = AudioOrigin::default();
        assert!(origin.open(Duration::from_millis(100)).is_empty());
        let chunks = origin.push(block(150, 7));
        assert_eq!(chunks.len(), 2, "silence, then the block");
        assert_eq!(
            chunks[0],
            vec![0u8; 2_400 * 4],
            "50 ms is 2400 frames of silence"
        );
        assert_eq!(chunks[1], vec![7u8; 480 * 4]);
    }

    /// Only the *first* block is placed: after it the audio is timed by its sample count.
    #[test]
    fn blocks_after_the_first_pass_through_untouched() {
        let mut origin = AudioOrigin::default();
        origin.open(Duration::from_millis(0));
        origin.push(block(3, 1));
        // A later block with a timestamp that jumps is not padded: the dropped-block silence in
        // the encoder already keeps the track on its timeline, and padding twice would double it.
        let chunks = origin.push(block(500, 2));
        assert_eq!(chunks, vec![vec![2u8; 480 * 4]]);
    }

    /// The origin is the first frame's; a second `open` changes nothing.
    #[test]
    fn the_origin_is_the_first_frames() {
        let mut origin = AudioOrigin::default();
        origin.open(Duration::from_millis(100));
        assert!(origin.open(Duration::from_millis(900)).is_empty());
        let chunks = origin.push(block(100, 3));
        assert_eq!(
            chunks,
            vec![vec![3u8; 480 * 4]],
            "still measured from 100 ms"
        );
    }

    /// A picture that never comes must not grow the hold without limit; what is let go is the
    /// oldest audio, the furthest from the picture.
    #[test]
    fn the_hold_is_bounded_and_lets_go_of_the_oldest() {
        let mut origin = AudioOrigin::default();
        for index in 0..1_000u64 {
            origin.push(block(index * 10, 1));
        }
        let limit_blocks = (AUDIO_HOLD_LIMIT.as_millis() / 10) as usize;
        assert!(
            origin.held() <= limit_blocks,
            "{} blocks held, the limit is {limit_blocks}",
            origin.held()
        );
        assert!(
            origin.held() >= limit_blocks - 1,
            "and it is not shed early"
        );
    }

    /// Timestamps that are not on one clock are not corrected: the whole track would be cut away
    /// or padded with a long silence on the strength of a guess.
    #[test]
    fn clocks_that_cannot_be_compared_are_left_alone() {
        let mut origin = AudioOrigin::default();
        let released = origin.open(Duration::from_secs(600));
        assert!(released.is_empty());
        let chunks = origin.push(block(0, 4));
        assert_eq!(
            chunks,
            vec![vec![4u8; 480 * 4]],
            "a ten-minute gap is two clocks, not a ten-minute offset"
        );
    }

    /// A block that is not a whole number of frames is never cut: it would be cut off-grid.
    #[test]
    fn a_ragged_block_is_never_cut() {
        let mut origin = AudioOrigin::default();
        origin.open(Duration::from_millis(5));
        let ragged = TimedAudio {
            pts: Duration::ZERO,
            frames: 480,
            sample_rate: 48_000,
            data: vec![1u8; 480 * 4 + 1],
        };
        assert_eq!(origin.push(ragged).len(), 1);
    }

    /// The spin at the end of the wait meets a sub-millisecond deadline.
    ///
    /// This is the part of the wait that replaces an ordinary sleep, and the part whose accuracy
    /// is the code's: the deadline is inside [`SPIN_WINDOW`], so the wait goes straight to the
    /// spin and the thread only has to hold the CPU for a few hundred microseconds. A deadline
    /// that short is rarely preempted, even on a loaded machine, so what is measured is the spin
    /// and not the scheduler. A wait that slept instead would overshoot by the timer's
    /// granularity — milliseconds — and fail by an order of magnitude.
    ///
    /// The wait's accuracy over a *whole frame interval* is deliberately not asserted here. A
    /// shared CI runner deschedules a thread for milliseconds at a stretch, and no implementation
    /// of "wake up at time T" survives that: asserting it measured the machine, and it failed CI
    /// with a 4ms median on a 16ms wait. It was measured where it can be — `thread::sleep(16ms)`
    /// overshoots by up to 8.75ms on real hardware and this wait by under 0.1ms — and the property
    /// the wait exists for is asserted end to end, against a real encoder, in
    /// `ffmpeg::tests::the_frames_are_written_at_their_capture_times` and in the timeline
    /// integration test.
    #[test]
    fn the_spin_meets_a_sub_millisecond_deadline() {
        let deadline = Duration::from_micros(400);
        let mut overshoots = Vec::new();
        for _ in 0..9 {
            let started = Instant::now();
            wait_until(started + deadline);
            let elapsed = started.elapsed();
            assert!(
                elapsed >= deadline,
                "the wait returned {elapsed:?} into a {deadline:?} deadline: it must never return \
                 before the time it was asked for",
            );
            overshoots.push(elapsed - deadline);
        }
        overshoots.sort();
        let median = overshoots[overshoots.len() / 2];
        assert!(
            median < Duration::from_millis(1),
            "the spin's median overshoot was {median:?}: a wait that sleeps instead of spinning \
             overshoots by the timer's granularity, and {overshoots:?} is what a spinner must \
             never look like",
        );
    }

    /// The wait never returns before the time it was asked for, however near or far that is.
    #[test]
    fn the_wait_never_returns_early() {
        for requested in [
            Duration::ZERO,
            Duration::from_micros(1),
            Duration::from_micros(900),
            Duration::from_millis(3),
            Duration::from_millis(16),
        ] {
            let started = Instant::now();
            wait_until(started + requested);
            let elapsed = started.elapsed();
            assert!(
                elapsed >= requested,
                "a {requested:?} wait returned after {elapsed:?}, before its deadline",
            );
        }
    }
}
