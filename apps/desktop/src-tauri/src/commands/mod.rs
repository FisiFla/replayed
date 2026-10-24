//! The desktop shell's command layer: everything the IPC commands actually do.
//!
//! # Why the logic lives here and not in the `#[tauri::command]` functions
//!
//! Every function here is a plain, non-async function over explicit dependencies (a [`Store`],
//! an optional [`FfmpegBinaries`], a set of paths, a storage policy), taking no `State`,
//! `AppHandle` or window. The wrappers in `lib.rs` resolve the application state and delegate.
//! That lets the tests drive the real command bodies against a real SQLite store and real
//! ffmpeg-produced files, with no Tauri runtime and no window.
//!
//! # What these commands do not do
//!
//! No command here captures the screen, enumerates windows or synthesises input, and none
//! re-encodes: `trim_clip` is [`replayed_media::edit::trim_lossless`], a stream copy
//! (`-c copy`), and the UI says so.

use replayed_media::FfmpegBinaries;
use replayed_store::Store;
use std::path::{Path, PathBuf};

mod clips;
mod diagnostics;
mod dto;
mod encoder;
mod error;
mod prune;
mod recorder;
mod sessions;
mod settings;
#[cfg(test)]
mod test_support;

pub use clips::*;
pub use diagnostics::*;
pub use dto::*;
pub use encoder::*;
pub use error::*;
pub use prune::*;
pub use recorder::*;
pub use sessions::*;
pub use settings::*;

/// The app-data-relative directories the shell reads and writes.
///
/// `clips_dir` is where the recorder writes clips and where a trim's new file goes, next to its
/// parent, so a trimmed clip is visible to the CLI's storage policy with no extra wiring.
/// `thumbnails_dir` is a cache only this app reads; losing it costs one ffmpeg run per clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub clips_dir: PathBuf,
    pub thumbnails_dir: PathBuf,
    pub db_path: PathBuf,
}

impl AppPaths {
    /// Resolve the paths for `app_data_dir` under the `[storage]` settings.
    ///
    /// An empty `clips_dir` means the `clips` directory under the application data directory, the
    /// CLI's rule, so the two binaries operate on one library.
    pub fn resolve(app_data_dir: &Path, clips_dir_setting: &str) -> Self {
        let clips_dir = if clips_dir_setting.is_empty() {
            app_data_dir.join("clips")
        } else {
            PathBuf::from(clips_dir_setting)
        };
        Self {
            clips_dir,
            thumbnails_dir: app_data_dir.join("thumbnails"),
            db_path: app_data_dir.join("replayed.db"),
        }
    }

    /// The directories the asset protocol must be allowed to serve.
    ///
    /// Playback requests only paths under `clips_dir`; the thumbnails directory is here because a
    /// cached JPEG is served to an `<img>` the same way. Both go to the runtime scope in `lib.rs`;
    /// the static scope in `tauri.conf.json` covers the default layout only, and a configured
    /// `storage.clips_dir` needs the runtime one.
    pub fn asset_roots(&self) -> [&Path; 2] {
        [self.clips_dir.as_path(), self.thumbnails_dir.as_path()]
    }
}

/// Everything a command needs, passed explicitly.
///
/// `bins` is optional because ffmpeg is discovered at startup and a missing one must not stop
/// the shell opening: listing clips and the storage policy need none, and the two commands that
/// do fail with [`ErrorCode::FfmpegUnavailable`] and the discovery error.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub store: &'a Store,
    pub bins: Option<&'a FfmpegBinaries>,
    pub paths: &'a AppPaths,
    pub storage: &'a StorageConfigView,
    /// Problems found at startup that the UI should surface rather than hide.
    pub warnings: &'a [String],
}

impl<'a> Deps<'a> {
    fn binaries(&self) -> Result<&'a FfmpegBinaries, CommandError> {
        self.bins.ok_or_else(|| {
            CommandError::new(
                ErrorCode::FfmpegUnavailable,
                "ffmpeg was not found, so clips cannot be trimmed or thumbnailed. \
                 Install ffmpeg on PATH or place the sidecar binaries next to the \
                 application.",
            )
        })
    }
}

/// The storage policy as the commands see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageConfigView {
    pub max_total_bytes: u64,
    pub max_age_days: u64,
}

impl From<&crate::config::StorageConfig> for StorageConfigView {
    fn from(cfg: &crate::config::StorageConfig) -> Self {
        Self {
            max_total_bytes: cfg.max_total_bytes,
            max_age_days: cfg.max_age_days,
        }
    }
}
