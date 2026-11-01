//! Deterministic stand-ins so the whole pipeline is testable off-Windows.

use crate::{AudioBackend, AudioBuffer, AudioFormat, CaptureBackend, Frame, PixelFormat};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct StubConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Synthetic video: a moving gradient so the encoder sees real inter-frame change.
pub struct StubCapture {
    cfg: StubConfig,
    frame_index: u64,
    started_at: Option<std::time::Instant>,
    /// Frames whose pixels were actually produced (see [`Self::frames_read_back`]).
    read_back: u64,
    /// Frames consumed without being rendered (see [`Self::frames_discarded`]).
    discarded: u64,
    /// A resize the source will make: the frame index it happens at and the size it changes to
    /// (see [`Self::resize_at_frame`]).
    resize: Option<(u64, (u32, u32))>,
    /// The geometry a caller has not yet been told about (see [`CaptureBackend::take_size_change`]).
    announced: Option<(u32, u32)>,
}

impl StubCapture {
    pub fn new(cfg: StubConfig) -> Self {
        Self {
            cfg,
            frame_index: 0,
            started_at: None,
            read_back: 0,
            discarded: 0,
            resize: None,
            announced: None,
        }
    }

    /// Make the source change size, as a window dragged or a display mode switched would: the
    /// frame at `frame_index` and every later one is `size`.
    ///
    /// Models the contract a real backend keeps ([`CaptureBackend::take_size_change`]): the change
    /// is reported before the first frame of the new size is returned, and
    /// [`CaptureBackend::native_size`] answers the new size from then on. It exists so a consumer
    /// of that contract can be tested on a host that has no screen to resize.
    pub fn resize_at_frame(mut self, frame_index: u64, size: (u32, u32)) -> Self {
        self.resize = Some((frame_index, size));
        self
    }

    /// How many frames this source has materialised: pixel buffers built by [`Self::render_next`],
    /// the readback a real backend pays a GPU copy and a 33MB CPU copy for. The pacer's discard-path
    /// regression test asserts on it: a source offering frames faster than the target rate must
    /// materialise only about the target rate.
    pub fn frames_read_back(&self) -> u64 {
        self.read_back
    }

    /// How many frames [`CaptureBackend::discard_pending`] consumed: offered, skipped by the pacer
    /// and never rendered. Kept apart from [`Self::frames_read_back`] because these frames cost nothing.
    pub fn frames_discarded(&self) -> u64 {
        self.discarded
    }

    /// When the frame at `frame_index` is due, measured from `start`.
    ///
    /// One formula for the whole source: `next_frame` and `discard_pending` must agree on which frame
    /// is due, or a discard would consume a frame the pump is about to ask for.
    fn due_after(&self) -> Duration {
        Duration::from_micros(self.frame_index * 1_000_000 / self.cfg.fps.max(1) as u64)
    }

    /// Whether the next frame is due at `elapsed` after `start`.
    fn is_due(&self, elapsed: Duration) -> bool {
        elapsed >= self.due_after()
    }

    /// Produce the frames for `elapsed`, without sleeping.
    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<Frame> {
        let wanted = (elapsed.as_secs_f64() * self.cfg.fps as f64).floor() as u64;
        (0..wanted).map(|_| self.render_next()).collect()
    }

    /// Apply the scheduled resize when the frame about to be produced is the one it happens at.
    fn apply_resize(&mut self) {
        if let Some((at, size)) = self.resize {
            if self.frame_index >= at {
                self.resize = None;
                self.cfg.width = size.0;
                self.cfg.height = size.1;
                self.announced = Some(size);
            }
        }
    }

    fn render_next(&mut self) -> Frame {
        self.apply_resize();
        self.read_back += 1;
        let (w, h) = (self.cfg.width, self.cfg.height);
        let phase = (self.frame_index % 256) as u8;
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[
                    (x as u8).wrapping_add(phase), // B
                    (y as u8),                     // G
                    phase,                         // R
                    255,                           // A
                ]);
            }
        }
        let pts = Duration::from_micros(self.frame_index * 1_000_000 / self.cfg.fps.max(1) as u64);
        self.frame_index += 1;
        Frame {
            data,
            pts,
            width: w,
            height: h,
            format: PixelFormat::Bgra8,
            texture: None,
        }
    }
}

impl CaptureBackend for StubCapture {
    fn start(&mut self) -> anyhow::Result<()> {
        self.frame_index = 0;
        self.started_at = Some(std::time::Instant::now());
        // A restart is a new run: the frame counter and both telemetry counters restart together.
        self.read_back = 0;
        self.discarded = 0;
        Ok(())
    }

    /// Real-time paced, so the CLI's buffer runs at 1x like a real capture source.
    ///
    /// Returns `None` when the next frame is not due within `timeout`, which lets a single-threaded
    /// caller drive video and audio together without either starving. `drain_for` bypasses pacing.
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("capture not started"))?;
        let Some(sleep) = self.due_after().checked_sub(started.elapsed()) else {
            return Ok(Some(self.render_next())); // already due
        };
        if sleep > timeout {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        Ok(Some(self.render_next()))
    }

    /// Consume the frame that is due right now, if any, without rendering it.
    ///
    /// The stub holds no frame pool: "pending" is exactly "the next frame's due time has passed", so
    /// there is never more than one, and consuming it just advances `frame_index` (the pixels are a
    /// deterministic function of it). Nothing is allocated and no pixel touched, so this path must
    /// never appear in [`Self::frames_read_back`].
    fn discard_pending(&mut self) -> anyhow::Result<usize> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("capture not started"))?;
        if !self.is_due(started.elapsed()) {
            return Ok(0);
        }
        // A discarded frame is still a frame the source offered, so a resize scheduled for it
        // happens at it: the announcement must not wait for a frame the caller keeps.
        self.apply_resize();
        self.frame_index += 1;
        self.discarded += 1;
        Ok(1)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn native_size(&self) -> (u32, u32) {
        (self.cfg.width, self.cfg.height)
    }

    fn take_size_change(&mut self) -> Option<(u32, u32)> {
        self.announced.take()
    }
}

/// Synthetic audio: silence, so A/V muxing is exercised without needing real audio.
pub struct StubAudio {
    format: AudioFormat,
    block_index: u64,
    started_at: Option<std::time::Instant>,
}

impl StubAudio {
    pub fn new(format: AudioFormat) -> Self {
        Self {
            format,
            block_index: 0,
            started_at: None,
        }
    }

    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<AudioBuffer> {
        let blocks = (elapsed.as_millis() / 10) as u64;
        (0..blocks).map(|_| self.next_block()).collect()
    }

    fn next_block(&mut self) -> AudioBuffer {
        let bytes = self.format.bytes_per_10ms();
        let pts = Duration::from_millis(self.block_index * 10);
        self.block_index += 1;
        AudioBuffer {
            data: vec![0u8; bytes],
            frames: self.format.sample_rate as usize / 100,
            pts,
            format: self.format,
        }
    }
}

impl AudioBackend for StubAudio {
    fn start(&mut self) -> anyhow::Result<()> {
        self.block_index = 0;
        self.started_at = Some(std::time::Instant::now());
        Ok(())
    }

    /// Real-time paced like `StubCapture::next_frame`. Returns `None` when the next 10ms block is
    /// not due within `timeout`; a zero timeout is a non-blocking "is anything due?" check.
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("audio capture not started"))?;
        let due = Duration::from_millis(self.block_index * 10);
        let Some(sleep) = due.checked_sub(started.elapsed()) else {
            return Ok(Some(self.next_block())); // already due
        };
        if sleep > timeout {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        Ok(Some(self.next_block()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stub_emits_the_requested_frame_count_at_the_requested_rate() {
        let cfg = StubConfig {
            width: 64,
            height: 48,
            fps: 60,
        };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // 0.25s of timeline.
        let frames = cap.drain_for(Duration::from_millis(250));
        assert_eq!(frames.len(), 15, "60fps for 250ms");
    }

    #[test]
    fn stub_frame_pts_are_monotonic_and_frame_sized() {
        let cfg = StubConfig {
            width: 64,
            height: 48,
            fps: 30,
        };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        let frames = cap.drain_for(Duration::from_millis(100));
        assert!(
            frames.windows(2).all(|w| w[0].pts < w[1].pts),
            "pts must increase"
        );
        // BGRA8 = 4 bytes per pixel.
        assert_eq!(frames[0].data.len(), 64 * 48 * 4);
    }

    #[test]
    fn stub_audio_emits_silence_in_whole_10ms_blocks() {
        let mut a = StubAudio::new(AudioFormat::default());
        a.start().unwrap();
        let blocks = a.drain_for(Duration::from_millis(50));
        assert_eq!(blocks.len(), 5);
        // 48000Hz / 100 blocks = 480 frames = 960 samples stereo = 1920 bytes.
        assert_eq!(blocks[0].data.len(), 1920);
        assert!(
            blocks[0].data.iter().all(|b| *b == 0),
            "stub audio is silence"
        );
    }

    #[test]
    fn next_frame_with_zero_timeout_returns_none_when_not_due() {
        // 10 fps => frames 100ms apart, a comfortable margin over the microseconds between the calls below.
        let cfg = StubConfig {
            width: 8,
            height: 8,
            fps: 10,
        };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // The initial frame (pts 0) is due immediately.
        assert!(cap.next_frame(Duration::ZERO).unwrap().is_some());
        // The next frame is due 100ms later: a zero timeout must not invent it.
        let started = std::time::Instant::now();
        assert!(
            cap.next_frame(Duration::ZERO).unwrap().is_none(),
            "must return None rather than block or fabricate a frame"
        );
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "a zero timeout must not block waiting for the frame"
        );
    }

    #[test]
    fn next_frame_with_a_generous_timeout_returns_some() {
        let cfg = StubConfig {
            width: 8,
            height: 8,
            fps: 10,
        };
        let mut cap = StubCapture::new(cfg);
        cap.start().unwrap();
        // Consume the immediately-due frame, then wait for the next one (100ms out).
        assert!(cap.next_frame(Duration::ZERO).unwrap().is_some());
        assert!(
            cap.next_frame(Duration::from_secs(1)).unwrap().is_some(),
            "a generous timeout must sleep until due and return the frame"
        );
    }

    #[test]
    fn discarding_the_pending_frame_does_not_read_it_back() {
        // 10fps: frames are 100ms apart and the frame at pts 0 is due immediately, so the calls below
        // are deterministic without sleeping.
        let cfg = StubConfig {
            width: 8,
            height: 8,
            fps: 10,
        };
        let mut cap = StubCapture::new(cfg);
        cap.start().expect("start the stub");

        assert_eq!(
            cap.discard_pending().expect("discard the pending frame"),
            1,
            "the frame due at pts 0 is the one and only pending frame"
        );
        assert_eq!(
            cap.frames_read_back(),
            0,
            "a discarded frame must not be materialised: that copy is what the discard \
             path exists to avoid"
        );
        assert_eq!(cap.frames_discarded(), 1);

        // The schedule moved with it: frame 1 is 100ms out, so nothing is pending and nothing is invented.
        assert_eq!(
            cap.discard_pending().expect("discard anything due"),
            0,
            "the next frame is not due yet"
        );
        assert!(cap.next_frame(Duration::ZERO).expect("poll").is_none());
        assert_eq!(cap.frames_read_back(), 0);

        // A frame the pump does take is counted as read back.
        assert!(cap
            .next_frame(Duration::from_secs(1))
            .expect("wait for the frame")
            .is_some());
        assert_eq!(cap.frames_read_back(), 1);
        assert_eq!(cap.frames_discarded(), 1);
    }

    #[test]
    fn discarding_before_start_is_an_error() {
        let mut cap = StubCapture::new(StubConfig {
            width: 8,
            height: 8,
            fps: 10,
        });
        let err = cap
            .discard_pending()
            .expect_err("the stub has no schedule before start");
        assert!(err.to_string().contains("not started"), "got: {err}");
    }

    #[test]
    fn a_resize_is_reported_once_before_the_first_frame_of_the_new_size_is_used() {
        // The contract a real backend keeps: frames before the report are the old size, the
        // report comes with the first frame of the new one, and `native_size` follows it.
        let cfg = StubConfig {
            width: 8,
            height: 6,
            fps: 1000,
        };
        let mut cap = StubCapture::new(cfg).resize_at_frame(3, (16, 12));
        cap.start().unwrap();
        assert_eq!(cap.take_size_change(), None, "nothing has changed yet");

        let mut sizes = Vec::new();
        for _ in 0..5 {
            let frame = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            let change = cap.take_size_change();
            sizes.push(((frame.width, frame.height), change));
        }

        assert_eq!(
            sizes,
            vec![
                ((8, 6), None),
                ((8, 6), None),
                ((8, 6), None),
                ((16, 12), Some((16, 12))),
                ((16, 12), None),
            ],
            "reported once, with the first frame that needs it"
        );
        assert_eq!(cap.native_size(), (16, 12));
    }

    #[test]
    fn a_resize_that_lands_on_a_discarded_frame_is_still_reported() {
        let cfg = StubConfig {
            width: 8,
            height: 6,
            fps: 1000,
        };
        let mut cap = StubCapture::new(cfg).resize_at_frame(0, (4, 4));
        cap.start().unwrap();
        assert_eq!(cap.discard_pending().unwrap(), 1);
        assert_eq!(cap.take_size_change(), Some((4, 4)));
        assert_eq!(cap.native_size(), (4, 4));
    }

    #[test]
    fn next_buffer_with_zero_timeout_returns_none_when_not_due() {
        let mut a = StubAudio::new(AudioFormat::default());
        a.start().unwrap();
        // Block 0 is due immediately.
        assert!(a.next_buffer(Duration::ZERO).unwrap().is_some());
        // Block 1 is due 10ms later: a zero timeout must not block for it.
        assert!(
            a.next_buffer(Duration::ZERO).unwrap().is_none(),
            "must return None rather than block or fabricate a block"
        );
    }
}
