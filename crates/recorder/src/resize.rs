//! What a recording does when the thing it captures changes size.
//!
//! A window is resized, a game toggles fullscreen, a monitor changes mode: the capture backend
//! reports it ([`replayed_capture::CaptureBackend::take_size_change`]) and every frame after it
//! has the new size. The encoder's raw-video pipe was declared with the old one, so the encoder
//! cannot take them: a recording that ignores the report dies on the first new frame (the
//! pump's [`crate::pump::guard_frame_size`] refuses it, which is the right failure for a frame
//! nobody announced).
//!
//! The answer is to close the encoder and open one declared at the new size. *What that does to
//! the footage already recorded* depends on the mode, and that decision is the whole of this
//! module, kept pure so every row of it is a test rather than a hope:
//!
//! * **Full session.** The recording is one file made by concatenating its segments losslessly
//!   (`-c copy`). A stream copy carries the first segment's codec parameters and cannot follow a
//!   change of resolution, so every segment must come out the same size. The output size is
//!   therefore *held*: the new encoder reads the new source and scales it to the size the
//!   recording started at, with the same `scale` filter that `encode.output_size` uses. If the
//!   aspect ratio changed, the picture is stretched, which is wrong-looking but a file that
//!   plays; the alternative is a file that does not. A hardware-encoded stream
//!   ([`VideoInput::EncodedBitstream`]) cannot be scaled at all (that needs decoding, which is
//!   the cost it removes), so its output *is* its source and a resize would change the
//!   parameters mid-file: the recording ends instead, cleanly, with the reason.
//! * **Replay buffer.** The ring holds one fragmented-MP4 stream with a single header, so a new
//!   encoder means a new ring, and the footage held so far goes with the old one. Clips already
//!   asked for are cut from it first; the buffer then fills again from nothing. The output
//!   follows the new size unless `encode.output_size` pinned one, because a new stream has no
//!   other stream to agree with.
//! * **Replay buffer that also saves its session.** The saved session is the ring's stream
//!   written to a file as it arrives, then remuxed. A second stream appended to it would put a
//!   second header in the middle of one file, which the remux cannot be trusted with. The
//!   recording ends cleanly, with the reason, and the session is saved as far as it went.
//!
//! "Ends cleanly" is a [`ResizeRefused`]: not a failure of the recorder, so it is not reported as
//! one (see `Engine::end_cleanly`), but it is also never a silent downgrade to recording the
//! wrong thing.

use crate::RecordingMode;
use replayed_encoder::{EncodeConfig, VideoInput};

/// A resize this recording cannot follow, and why. The recording ends cleanly with the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResizeRefused(pub(crate) String);

impl std::fmt::Display for ResizeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResizeRefused {}

/// What to build for the new size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizePlan {
    /// The size the new encoder writes (its source is the capture's new size).
    pub(crate) output_size: (u32, u32),
}

/// Decide how a recording follows the capture to `size`, or why it cannot.
///
/// * `current` is the configuration the running encoder was spawned with.
/// * `configured_output` is `encode.output_size` when the user pinned one; `None` means "follow
///   the capture".
/// * `saves_session` is a replay buffer that is also writing a session file.
pub(crate) fn plan(
    current: &EncodeConfig,
    configured_output: Option<(u32, u32)>,
    mode: RecordingMode,
    saves_session: bool,
    size: (u32, u32),
) -> Result<ResizePlan, ResizeRefused> {
    let from = current.source_size;
    let scalable = matches!(current.video, VideoInput::RawPixels);
    if saves_session {
        return Err(ResizeRefused(format!(
            "the capture changed size from {}x{} to {}x{}, and a replay buffer that is also \
             saving its session cannot follow it: the saved session is one stream with one \
             header, and a new encoder would put a second header in the middle of the file. \
             The recording ended here and the session was saved as far as it went. Turn off \
             the saved session to keep a replay buffer across resizes",
            from.0, from.1, size.0, size.1
        )));
    }
    if mode.is_full_session() {
        // Held: every segment has to carry the first one's parameters for the lossless
        // concatenation to be a file that plays.
        let output_size = current.output_size;
        if !scalable && output_size != size {
            return Err(ResizeRefused(format!(
                "the capture changed size from {}x{} to {}x{}, and a session recorded with the \
                 hardware encoder's own stream cannot follow it: that stream is copied, not \
                 re-encoded, so it cannot be scaled back to {}x{}, and a file whose size \
                 changes in the middle does not play. The recording ended here and the \
                 session was saved as far as it went. Set `encode.zero_copy = false` to \
                 record a session across window resizes (it costs the readback)",
                from.0, from.1, size.0, size.1, output_size.0, output_size.1
            )));
        }
        return Ok(ResizePlan { output_size });
    }
    // A replay buffer: a new stream, so nothing to agree with except what the user pinned. A
    // copied hardware stream cannot be scaled, so its output is its source whatever was pinned.
    let output_size = if scalable {
        configured_output.unwrap_or(size)
    } else {
        size
    };
    Ok(ResizePlan { output_size })
}

/// Whether scaling `source` to `output` changes the picture's shape, so it ends up stretched.
///
/// Compared by cross-multiplication: no division, so no rounding decides it.
pub(crate) fn stretches(source: (u32, u32), output: (u32, u32)) -> bool {
    u64::from(source.0) * u64::from(output.1) != u64::from(source.1) * u64::from(output.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use replayed_encoder::{EncodeConfig, VideoCodec};
    use std::path::PathBuf;

    /// A raw-pixel configuration recording `source` into `output`.
    fn config(source: (u32, u32), output: (u32, u32)) -> EncodeConfig {
        let mut cfg = EncodeConfig::for_tests_software(
            VideoCodec::H264,
            source.0,
            source.1,
            30,
            PathBuf::new(),
            1000,
        );
        cfg.output_size = output;
        cfg
    }

    #[test]
    fn a_session_holds_its_output_size_and_scales_the_new_source_to_it() {
        // Started at 64x48, the window grew to 80x60: the new encoder writes 64x48 still, so the
        // segments concatenate into a file with one set of codec parameters.
        let plan = plan(
            &config((64, 48), (64, 48)),
            None,
            RecordingMode::FullSession,
            false,
            (80, 60),
        )
        .expect("a raw-pixel session follows a resize");
        assert_eq!(plan.output_size, (64, 48));
    }

    #[test]
    fn a_session_holds_the_size_it_is_already_scaling_to() {
        // Pinned at 1280x720 from a 1920x1080 source: a resize changes the source only.
        let plan = plan(
            &config((1920, 1080), (1280, 720)),
            Some((1280, 720)),
            RecordingMode::FullSession,
            false,
            (2560, 1440),
        )
        .expect("a raw-pixel session follows a resize");
        assert_eq!(plan.output_size, (1280, 720));
    }

    #[test]
    fn a_buffer_follows_the_new_size_unless_the_user_pinned_one() {
        let unpinned = plan(
            &config((64, 48), (64, 48)),
            None,
            RecordingMode::ReplayBuffer,
            false,
            (80, 60),
        )
        .expect("a buffer follows a resize");
        assert_eq!(
            unpinned.output_size,
            (80, 60),
            "a new stream has nothing to agree with"
        );

        let pinned = plan(
            &config((1920, 1080), (1280, 720)),
            Some((1280, 720)),
            RecordingMode::ReplayBuffer,
            false,
            (2560, 1440),
        )
        .expect("a buffer follows a resize");
        assert_eq!(
            pinned.output_size,
            (1280, 720),
            "a pinned size is the user's decision"
        );
    }

    #[test]
    fn a_buffer_that_saves_its_session_cannot_follow_a_resize() {
        let refused = plan(
            &config((64, 48), (64, 48)),
            None,
            RecordingMode::ReplayBuffer,
            true,
            (80, 60),
        )
        .expect_err("a second stream cannot be appended to the saved session");
        assert!(
            refused.0.contains("64x48")
                && refused.0.contains("80x60")
                && refused.0.contains("saved"),
            "the message names both sizes and the cause: {refused}"
        );
    }

    #[test]
    fn a_copied_hardware_stream_cannot_be_scaled_back_so_a_session_ends() {
        let mut copied = config((64, 48), (64, 48));
        copied.video = VideoInput::EncodedBitstream;
        let refused = plan(&copied, None, RecordingMode::FullSession, false, (80, 60))
            .expect_err("the stream is copied, so its size cannot be held");
        assert!(
            refused.0.contains("zero_copy"),
            "the message names the setting that avoids it: {refused}"
        );
        // A buffer has no file to keep consistent: its new ring takes the new size.
        let buffer = plan(
            &copied,
            Some((10, 10)),
            RecordingMode::ReplayBuffer,
            false,
            (80, 60),
        )
        .expect("a buffer's new ring may have another size");
        assert_eq!(
            buffer.output_size,
            (80, 60),
            "a copied stream's output is its source"
        );
    }

    #[test]
    fn a_stretch_is_a_change_of_shape_not_of_size() {
        assert!(!stretches((1920, 1080), (1280, 720)), "same 16:9 shape");
        assert!(!stretches((64, 48), (64, 48)));
        assert!(
            stretches((1280, 960), (1920, 1080)),
            "4:3 into 16:9 is stretched"
        );
        assert!(stretches((80, 60), (64, 64)));
    }
}
