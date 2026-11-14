//! The decisions the video backend takes about a capture that is not going to plan, free of any
//! platform so they are tested on every host.
//!
//! The Windows backend is the only thing that can *observe* a window resize, a minimised target or
//! a lost graphics device, and none of that can be provoked from a CI runner or from macOS. What
//! can be tested anywhere is what the backend *does* with the observation: whether a frame is a
//! resize, whether a quiet capture has stalled, whether a lost device is worth another attempt.
//! Those are the three questions here, and the backend is left with the calls that ask the OS.

use std::time::{Duration, Instant};

/// How a frame's own size compares with the size the frame pool was last built for.
///
/// Windows Graphics Capture hands out surfaces of the *pool's* size, whatever the captured thing
/// has become. When a window grows past the pool, the surface is a crop of it; when it shrinks or
/// the display drops to a lower mode, the surface is padded. The only place the truth is reported
/// is the frame's `ContentSize`, which is why every frame's is compared with the pool's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameGeometry {
    /// The frame is the size the pool was built for: copy it.
    Current,
    /// The captured content is now this size, and the pool is not: rebuild the pool at this size and
    /// do not copy this frame, which is a crop or a padding of the content it claims to be.
    Resized((u32, u32)),
    /// A frame that was already in the pool when it was rebuilt, still at the old size and whole:
    /// close it without copying and without treating it as another resize.
    Stale,
    /// The content reports no area (a minimised window). There is nothing to copy and nothing to
    /// rebuild a pool for, since a pool cannot have a zero size: close the frame and wait.
    Empty,
}

/// Classify one frame.
///
/// `content` is the frame's `ContentSize()`, `texture` the size of its surface and `pool` the size
/// the pool was last built for (the size the backend reports for frames).
///
/// A frame whose surface is not the pool's size is one the pool queued before it was rebuilt, and
/// is never a frame to return, whatever its content says: its surface is not what the backend
/// reports. Recognising it as a leftover matters because it also disagrees with the pool's size, and
/// mistaking it for a resize would rebuild the pool again for every frame the old pool had left.
pub(crate) fn classify_frame(
    content: (i32, i32),
    texture: (u32, u32),
    pool: (u32, u32),
) -> FrameGeometry {
    if content.0 < 1 || content.1 < 1 {
        return FrameGeometry::Empty;
    }
    let content = (content.0 as u32, content.1 as u32);
    if texture == pool {
        return if content == pool {
            FrameGeometry::Current
        } else {
            FrameGeometry::Resized(content)
        };
    }
    // The surface is not the pool's size, so the frame was queued by the pool before it was
    // rebuilt. It is a leftover, not a picture of the current size, whatever its content says: whole
    // at the old size, or a crop or padding of content that has since been rebuilt for. Only content
    // that is neither the old surface's size nor the pool's means the thing changed again.
    if content == texture || content == pool {
        return FrameGeometry::Stale;
    }
    FrameGeometry::Resized(content)
}

/// How long a capture may deliver nothing before it is called stalled.
///
/// A screen that is not changing presents no frames, and that is not a fault: an idle desktop can
/// be quiet for a long time, so a stall is a *state to report*, not an error to fail on. Five
/// seconds is long enough that no ordinary pause in a game's picture reaches it (a loading screen
/// still redraws), and short enough that a minimised window is named while the person who
/// minimised it is still watching the log.
pub(crate) const STALL_AFTER: Duration = Duration::from_secs(5);

/// What a frame, or the lack of one, changed about the capture's liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// Nothing to report.
    Unchanged,
    /// No frame for [`STALL_AFTER`]: reported once per stall.
    Stalled(Duration),
    /// A frame arrived after a reported stall, which lasted this long.
    Resumed(Duration),
}

/// Tracks the time since the last frame, so a capture that has gone quiet is reported once and
/// reported as over when it ends.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StallWatch {
    last_frame: Instant,
    reported: bool,
}

impl StallWatch {
    /// A watch whose clock starts at `now`: the capture was only just started or rebuilt, so it
    /// has not yet had time to be quiet.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last_frame: now,
            reported: false,
        }
    }

    /// A frame arrived at `now`.
    pub(crate) fn frame(&mut self, now: Instant) -> Liveness {
        let was = self.stalled_for(now);
        self.last_frame = now;
        if std::mem::take(&mut self.reported) {
            // `was` is `Some` whenever `reported` is, because the report is what set it.
            Liveness::Resumed(was.unwrap_or_default())
        } else {
            Liveness::Unchanged
        }
    }

    /// No frame has arrived as of `now`. Reports the stall the first time it is long enough.
    pub(crate) fn idle(&mut self, now: Instant) -> Liveness {
        match self.stalled_for(now) {
            Some(quiet) if !self.reported => {
                self.reported = true;
                Liveness::Stalled(quiet)
            }
            _ => Liveness::Unchanged,
        }
    }

    /// How long no frame has arrived, once that is at least [`STALL_AFTER`].
    pub(crate) fn stalled_for(&self, now: Instant) -> Option<Duration> {
        let quiet = now.saturating_duration_since(self.last_frame);
        (quiet >= STALL_AFTER).then_some(quiet)
    }

    /// Restart the clock without reporting anything: the capture was rebuilt, and the quiet while
    /// it was down is not the target's.
    pub(crate) fn restart(&mut self, now: Instant) {
        *self = Self::new(now);
    }
}

/// What a recovery loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryStep {
    /// Make an attempt now.
    Try,
    /// The next attempt is not due for this long.
    Wait(Duration),
    /// The attempts are used up: stop trying and fail.
    GiveUp,
}

/// How many times a lost graphics device is rebuilt before the recording is failed.
#[cfg(windows)]
pub(crate) const DEVICE_RETRIES: u32 = 20;

/// How long between those attempts.
///
/// A driver reset takes the display a second or two to come back from, and a rebuild that
/// happens inside it fails the same way the last one did. Half a second between attempts, twenty
/// times, is ten seconds of patience: a TDR costs the recording seconds, not the session, and a
/// device that is not coming back is reported within a time someone will wait for.
#[cfg(windows)]
pub(crate) const DEVICE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// A bounded number of attempts, spaced out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryBudget {
    interval: Duration,
    max_attempts: u32,
    attempts: u32,
    next_at: Option<Instant>,
}

impl RetryBudget {
    /// `max_attempts` attempts, `interval` apart, the first due at once.
    pub(crate) fn new(max_attempts: u32, interval: Duration) -> Self {
        Self {
            interval,
            max_attempts,
            attempts: 0,
            next_at: None,
        }
    }

    /// Attempts made so far.
    pub(crate) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// What to do at `now`. A [`RetryStep::Try`] counts as an attempt: the caller makes it, and
    /// asks again if it failed.
    pub(crate) fn poll(&mut self, now: Instant) -> RetryStep {
        if self.attempts >= self.max_attempts {
            return RetryStep::GiveUp;
        }
        match self.next_at {
            Some(at) if now < at => RetryStep::Wait(at - now),
            _ => {
                self.attempts += 1;
                self.next_at = Some(now + self.interval);
                RetryStep::Try
            }
        }
    }
}

/// `DXGI_ERROR_DEVICE_REMOVED`: the adapter was physically removed, or its driver was updated.
const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A_0005_u32 as i32;
/// `DXGI_ERROR_DEVICE_HUNG`: the device took too long and the OS reset it.
const DXGI_ERROR_DEVICE_HUNG: i32 = 0x887A_0006_u32 as i32;
/// `DXGI_ERROR_DEVICE_RESET`: the device failed and was reset (a timeout detection and recovery).
const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A_0007_u32 as i32;
/// `DXGI_ERROR_DRIVER_INTERNAL_ERROR`: the driver hit an error and the device is gone with it.
const DXGI_ERROR_DRIVER_INTERNAL_ERROR: i32 = 0x887A_0020_u32 as i32;

/// Whether an HRESULT says the graphics device is gone and a new one can be made.
///
/// The codes a D3D11 call returns once the device has been removed or reset. Every one of them is
/// permanent for the device that returned it and recoverable by building another, which is the
/// difference from an error that no rebuild would change.
pub(crate) fn is_device_lost_code(hresult: i32) -> bool {
    matches!(
        hresult,
        DXGI_ERROR_DEVICE_REMOVED
            | DXGI_ERROR_DEVICE_HUNG
            | DXGI_ERROR_DEVICE_RESET
            | DXGI_ERROR_DRIVER_INTERNAL_ERROR
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: (u32, u32) = (1920, 1080);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_frame_the_size_of_the_pool_is_copied() {
        assert_eq!(
            classify_frame((1920, 1080), POOL, POOL),
            FrameGeometry::Current
        );
    }

    #[test]
    fn content_that_outgrew_the_pool_is_a_resize_not_a_cropped_frame() {
        // A window dragged larger: the surface is still pool-sized, so copying it records the
        // left top corner of the window. The content size is the only thing that says so.
        assert_eq!(
            classify_frame((2560, 1440), POOL, POOL),
            FrameGeometry::Resized((2560, 1440))
        );
    }

    #[test]
    fn content_that_shrank_inside_the_pool_is_a_resize_not_a_padded_frame() {
        // A display mode change to a lower mode: the surface is the old size, mostly padding.
        assert_eq!(
            classify_frame((1280, 720), POOL, POOL),
            FrameGeometry::Resized((1280, 720))
        );
    }

    #[test]
    fn a_change_in_one_dimension_only_is_still_a_resize() {
        assert_eq!(
            classify_frame((1920, 1081), POOL, POOL),
            FrameGeometry::Resized((1920, 1081))
        );
    }

    #[test]
    fn a_whole_frame_at_the_old_size_after_a_rebuild_is_stale_not_another_resize() {
        // The pool was rebuilt for 1280x720 and still held one 1920x1080 frame from before.
        let rebuilt = (1280, 720);
        assert_eq!(
            classify_frame((1920, 1080), (1920, 1080), rebuilt),
            FrameGeometry::Stale
        );
        // ...and a frame at the new size is current.
        assert_eq!(
            classify_frame((1280, 720), (1280, 720), rebuilt),
            FrameGeometry::Current
        );
    }

    #[test]
    fn a_crop_queued_by_the_old_pool_is_stale_even_when_its_content_is_the_new_size() {
        // The window grew 800x600 -> 1000x700 and the pool was rebuilt at 1000x700. A frame the old
        // pool had queued still has an 800x600 surface, cropped from content that already reads
        // 1000x700: its content equals the pool's size, but it is not a frame of it.
        assert_eq!(
            classify_frame((1000, 700), (800, 600), (1000, 700)),
            FrameGeometry::Stale
        );
    }

    #[test]
    fn a_resize_during_the_old_pools_leftovers_is_still_seen() {
        // A stale surface whose content has *also* changed is not whole: it is a new size.
        assert_eq!(
            classify_frame((800, 600), (1920, 1080), (1280, 720)),
            FrameGeometry::Resized((800, 600))
        );
    }

    #[test]
    fn content_with_no_area_is_empty_so_no_pool_is_built_for_it() {
        assert_eq!(classify_frame((0, 0), POOL, POOL), FrameGeometry::Empty);
        assert_eq!(classify_frame((1920, 0), POOL, POOL), FrameGeometry::Empty);
        assert_eq!(classify_frame((-1, 5), POOL, POOL), FrameGeometry::Empty);
    }

    #[test]
    fn a_quiet_capture_is_stalled_only_after_the_threshold_and_reported_once() {
        let start = Instant::now();
        let mut watch = StallWatch::new(start);
        assert_eq!(watch.idle(start + secs(4)), Liveness::Unchanged);
        assert_eq!(watch.stalled_for(start + secs(4)), None);

        assert_eq!(watch.idle(start + secs(5)), Liveness::Stalled(secs(5)));
        assert_eq!(watch.stalled_for(start + secs(9)), Some(secs(9)));
        // Reported once, not on every poll of a minimised window.
        assert_eq!(watch.idle(start + secs(9)), Liveness::Unchanged);
    }

    #[test]
    fn a_frame_after_a_stall_says_how_long_it_lasted() {
        let start = Instant::now();
        let mut watch = StallWatch::new(start);
        assert_eq!(watch.idle(start + secs(7)), Liveness::Stalled(secs(7)));

        assert_eq!(watch.frame(start + secs(30)), Liveness::Resumed(secs(30)));
        assert_eq!(
            watch.stalled_for(start + secs(31)),
            None,
            "no longer stalled"
        );
        // A second stall is reported afresh.
        assert_eq!(
            watch.idle(start + secs(36)),
            Liveness::Stalled(secs(6)),
            "measured from the frame, not from the start"
        );
    }

    #[test]
    fn frames_that_keep_arriving_are_never_a_stall() {
        let start = Instant::now();
        let mut watch = StallWatch::new(start);
        for i in 1..100 {
            assert_eq!(
                watch.frame(start + Duration::from_millis(16 * i)),
                Liveness::Unchanged
            );
        }
    }

    #[test]
    fn a_rebuilt_capture_starts_its_clock_over() {
        let start = Instant::now();
        let mut watch = StallWatch::new(start);
        watch.restart(start + secs(20));
        assert_eq!(watch.idle(start + secs(22)), Liveness::Unchanged);
        assert_eq!(watch.idle(start + secs(25)), Liveness::Stalled(secs(5)));
    }

    #[test]
    fn the_retry_budget_tries_at_once_waits_between_attempts_and_gives_up() {
        let start = Instant::now();
        let mut budget = RetryBudget::new(3, Duration::from_millis(500));

        assert_eq!(budget.poll(start), RetryStep::Try);
        assert_eq!(
            budget.poll(start + Duration::from_millis(100)),
            RetryStep::Wait(Duration::from_millis(400))
        );
        assert_eq!(
            budget.poll(start + Duration::from_millis(500)),
            RetryStep::Try
        );
        assert_eq!(
            budget.poll(start + Duration::from_millis(1000)),
            RetryStep::Try
        );
        assert_eq!(budget.attempts(), 3);
        // The third attempt failed: no more waiting for one that is not coming.
        assert_eq!(
            budget.poll(start + Duration::from_millis(1000)),
            RetryStep::GiveUp
        );
    }

    #[test]
    fn the_device_codes_a_lost_device_returns_are_recognised_and_others_are_not() {
        assert!(
            is_device_lost_code(0x887A_0005_u32 as i32),
            "DEVICE_REMOVED"
        );
        assert!(is_device_lost_code(0x887A_0006_u32 as i32), "DEVICE_HUNG");
        assert!(is_device_lost_code(0x887A_0007_u32 as i32), "DEVICE_RESET");
        assert!(
            is_device_lost_code(0x887A_0020_u32 as i32),
            "DRIVER_INTERNAL_ERROR"
        );
        assert!(!is_device_lost_code(0), "S_OK");
        assert!(!is_device_lost_code(0x8000_4005_u32 as i32), "E_FAIL");
        // DXGI_ERROR_WAS_STILL_DRAWING is a retry-later, not a lost device.
        assert!(!is_device_lost_code(0x887A_000A_u32 as i32));
    }
}
