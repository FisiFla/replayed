//! The error type every command returns.

use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// The class of a failure. The frontend switches on this; `message` is for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No clip with that id is in the index.
    ClipNotFound,
    /// No recording session with that id is in the index.
    SessionNotFound,
    /// The session is still recording. Its scratch directory is the only copy of the
    /// footage, so there is nothing safe to cut from it or delete yet.
    SessionStillRecording,
    /// The range has no length (`end <= start`). Rejected before ffmpeg is ever spawned.
    InvalidRange,
    /// The range reaches past the end of the clip, or a timestamp does.
    OutOfRange,
    /// Something else about the input was unusable.
    InvalidInput,
    /// No usable ffmpeg/ffprobe was found.
    FfmpegUnavailable,
    /// The SQLite index refused the operation.
    Store,
    /// ffmpeg or ffprobe ran and failed.
    Media,
    /// The recording engine refused the operation: no hardware encoder, a recorder that
    /// is already running, a config file that cannot describe a recording.
    Recording,
    /// A recording is running and the operation would end it, so it was refused rather than
    /// done: the caller has to stop the recording, or say that stopping it is fine, first. The
    /// update install asks (`prepare_for_update`), because the installer ends this process.
    RecordingActive,
    /// A filesystem operation failed.
    Io,
}

/// A structured, serialisable command failure.
///
/// Commands never panic and never return a bare string: the frontend gets a machine-readable
/// `code` and a message written for a person. Tauri's IPC boundary requires `Serialize`, the
/// only constraint this plain value carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandError {
    pub code: ErrorCode,
    pub message: String,
}

impl CommandError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn clip_not_found(id: i64) -> Self {
        Self::new(
            ErrorCode::ClipNotFound,
            format!("no clip with id {id} is in the index"),
        )
    }

    pub fn session_not_found(id: i64) -> Self {
        Self::new(
            ErrorCode::SessionNotFound,
            format!("no session with id {id} is in the index"),
        )
    }

    /// `{:#}` on the wrapped error keeps the `context` chain, which is where the useful
    /// half of an `anyhow` error usually is.
    pub(super) fn store(action: &str, err: anyhow::Error) -> Self {
        Self::new(ErrorCode::Store, format!("{action} failed: {err:#}"))
    }

    pub(super) fn media(action: &str, err: anyhow::Error) -> Self {
        Self::new(ErrorCode::Media, format!("{action} failed: {err:#}"))
    }

    pub(crate) fn io(action: &str, path: &Path, err: std::io::Error) -> Self {
        Self::new(
            ErrorCode::Io,
            format!("{action} failed for {}: {err}", path.display()),
        )
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.message, self.code)
    }
}

impl std::error::Error for CommandError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_error_serialises_with_a_machine_readable_code() {
        let value = serde_json::to_value(CommandError::clip_not_found(7)).unwrap();
        assert_eq!(value["code"], "clip_not_found");
        assert_eq!(value["message"], "no clip with id 7 is in the index");
    }
}
