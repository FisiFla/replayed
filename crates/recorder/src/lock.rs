//! The data directory's recorder lock: one live recorder per data directory, across processes.
//!
//! # Why it exists
//!
//! The desktop shell and the CLI share one data directory and one `replayed.db`. Crash recovery
//! ([`crate::session::recover_sessions`]) treats every `sessions` row with `ended_at IS NULL` as
//! belonging to a run that died, and finalises or closes it. With a second process in play that is
//! false: a CLI started while the desktop is recording would "recover" the desktop's live session,
//! concatenating segments ffmpeg is still writing and deleting the directory under it, and its
//! cleanup pass would plan against a store another recorder is appending to.
//!
//! # What it is
//!
//! An exclusive advisory lock on `<data dir>/recorder.lock`, taken before the store is opened and
//! held until the last holder drops it. The operating system releases it when the process exits
//! however it exits, so a crash never leaves a stale lock behind, which is why this is a file lock
//! and not a pid file that a recovery would have to second-guess.
//!
//! Recovery takes the lock as an argument ([`DataDirLock`] is a witness): it cannot be called
//! without holding it, so it cannot run beside a live owner. A row left running while the lock is
//! free belongs to a dead process, by construction.
//!
//! # Lifetime
//!
//! The lock is shared (`Arc`) between the [`crate::Recorder`] and the threads that outlive a
//! call to it, so it is released when the engine thread (and, in games mode, the supervisor) is
//! really gone: a recorder replaced in the same process cannot start while the old engine is still
//! writing, and a panicked engine thread still releases it on unwind.
//!
//! Uses the standard library's `File::try_lock` (`flock` on Unix, `LockFileEx` on Windows), so no
//! dependency is added.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// The lock file's name inside the data directory.
pub const LOCK_FILE_NAME: &str = "recorder.lock";

/// Exclusive ownership of a data directory's recorder; released on drop.
#[derive(Debug)]
pub struct DataDirLock {
    /// Held open: closing the handle is what releases the lock.
    _file: File,
    path: PathBuf,
}

impl DataDirLock {
    /// Take the lock for `data_dir`, creating the directory and the lock file if they are missing.
    ///
    /// Fails, never waits, when another live recorder holds it: waiting would hang a CLI behind a
    /// desktop that records for hours, and the user is better served by a clear refusal.
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating the data directory {}", data_dir.display()))?;
        let path = data_dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening the recorder lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(TryLockError::WouldBlock) => bail!(
                "another replayed process is already recording with the data directory {}: only one \
                 recorder may use it at a time, because a second one would take the first one's \
                 running session for a crashed one. Stop the other recorder (the desktop app or a \
                 CLI), then start again.",
                data_dir.display()
            ),
            Err(TryLockError::Error(err)) => Err(err)
                .with_context(|| format!("locking the recorder lock {}", path.display())),
        }
    }

    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_and_the_first_release_frees_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).expect("the first holder gets it");
        let err = DataDirLock::acquire(dir.path()).expect_err("a second one must not");
        assert!(format!("{err:#}").contains("already recording"), "{err:#}");
        drop(first);
        DataDirLock::acquire(dir.path()).expect("free again once the holder is gone");
    }

    #[test]
    fn different_data_directories_do_not_contend() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _a = DataDirLock::acquire(a.path()).unwrap();
        let _b = DataDirLock::acquire(b.path()).expect("another directory is its own lock");
    }

    #[test]
    fn a_missing_data_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("not").join("yet");
        let lock = DataDirLock::acquire(&nested).expect("created on demand");
        assert!(lock.path().is_file());
    }
}
