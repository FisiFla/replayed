//! A size-capped, rotating log file.
//!
//! The shell can run for months in the background, so an append-only log would grow without
//! bound. `base.log` is renamed to `base.log.1` (and `.1` to `.2`, ...) once it reaches
//! `max_bytes`; at most `keep` rotated files survive, bounding the directory to roughly
//! `(keep + 1) * max_bytes`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: File,
    written: u64,
    /// The lock file's handle while this process holds the single-writer lock. The lock lives as
    /// long as the handle, so it is kept for the life of the writer and never touched by the
    /// rotation: it is on a file of its own, not on the log, so renaming the log neither loses nor
    /// needs to retake it.
    _lock: Option<File>,
    /// Whether this process is the log's only writer, and so may rename it. Rotation moves the
    /// file, which is only safe for the process that holds the lock (or that owns a file nobody
    /// else opens).
    may_rotate: bool,
}

impl RotatingFile {
    pub fn open(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Self::from_file(file, path, max_bytes, keep, true)
    }

    /// Open the log and try to take the single-writer lock beside it.
    ///
    /// Two shells (the second launch is the single-instance plugin's handover) would otherwise
    /// interleave into one file, and the rotation renames it. The lock is the cheapest way to keep
    /// one file whole: the process that cannot take it writes a file of its own.
    ///
    /// **The lock is on a file of its own, never on the log.** On Windows `LockFileEx` is a
    /// *mandatory* lock over the range it names, and fs4 names the whole file: every read of the
    /// locked file through any other handle, this process's own included, then fails with
    /// `ERROR_LOCK_VIOLATION`. The health page and the diagnostics report both read the log while
    /// the app is running, so a lock on the log blinded exactly the screens that exist to explain a
    /// problem. The lock file (`<name>.lock`, with the log's extension replaced) holds no data and
    /// nobody reads it, so a mandatory lock on it costs nothing. The operating system drops the
    /// lock when the process dies, so a crash leaves nothing stale behind.
    ///
    /// The rotation at open follows the lock decision: only the process that holds the lock may
    /// rename the log. A launch that lost the lock opens the log without rotating it, and the
    /// caller is expected to write a file of its own.
    ///
    /// The boolean is `false` when another process holds the lock.
    pub fn open_locked(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<(Self, bool)> {
        let lock_path = path.with_extension("lock");
        // `read(true)` is not decoration. On Windows the lock is a `LockFileEx` byte-range lock,
        // and it refuses a handle that was not opened with the `GENERIC_READ` or `GENERIC_WRITE`
        // access right: a handle opened for append alone has `FILE_APPEND_DATA` and nothing else,
        // and the lock then fails with `ERROR_ACCESS_DENIED` — "Access is denied" — rather than
        // with the "somebody else holds it" the caller is asking about.
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        let locked = match <File as fs4::FileExt>::try_lock(&lock_file) {
            Ok(()) => true,
            // `WouldBlock` is the "somebody else holds it" answer the caller asks about; any other
            // error is a real failure to open the lock and is propagated.
            Err(fs4::TryLockError::WouldBlock) => false,
            Err(fs4::TryLockError::Error(error)) => return Err(error),
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut this = Self::from_file(file, path, max_bytes, keep, locked)?;
        this._lock = locked.then_some(lock_file);
        Ok((this, locked))
    }

    /// `may_rotate` is whether this process is the log's only writer. An oversized log is rotated
    /// at open only then: a process that lost the lock must leave the winner's live file alone.
    fn from_file(
        file: File,
        path: PathBuf,
        max_bytes: u64,
        keep: usize,
        may_rotate: bool,
    ) -> io::Result<Self> {
        let written = file.metadata()?.len();
        let mut this = Self {
            path,
            max_bytes,
            keep,
            file,
            written,
            _lock: None,
            may_rotate,
        };
        if may_rotate && this.written >= this.max_bytes {
            this.rotate()?;
        }
        Ok(this)
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Oldest first, so nothing is overwritten before it has moved on.
        let _ = fs::remove_file(self.numbered(self.keep));
        for n in (1..self.keep).rev() {
            let from = self.numbered(n);
            if from.exists() {
                let _ = fs::rename(&from, self.numbered(n + 1));
            }
        }
        if self.keep == 0 {
            let _ = fs::remove_file(&self.path);
        } else {
            let _ = fs::rename(&self.path, self.numbered(1));
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.may_rotate && self.written >= self.max_bytes {
            // A failed rotation must not lose the line or stop logging: keep appending.
            let _ = self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_the_cap_and_keeps_only_the_newest_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut log = RotatingFile::open(path.clone(), 100, 2).unwrap();
        for i in 0..10 {
            log.write_all(format!("{i:0>60}\n").as_bytes()).unwrap();
        }
        let names: Vec<_> = {
            let mut v: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        assert_eq!(names, ["app.log", "app.log.1", "app.log.2"]);
        let total: u64 = names
            .iter()
            .map(|n| fs::metadata(dir.path().join(n)).unwrap().len())
            .sum();
        assert!(total <= 3 * (100 + 61), "bounded: {total}");
        assert!(
            fs::read_to_string(&path).unwrap().contains("000009"),
            "newest line is in app.log"
        );
    }

    #[test]
    fn an_oversized_existing_file_is_rotated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        fs::write(&path, vec![b'x'; 500]).unwrap();
        let _log = RotatingFile::open(path.clone(), 100, 1).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            fs::metadata(dir.path().join("app.log.1")).unwrap().len(),
            500
        );
    }

    /// The regression for the Windows lock: the log is **readable while this process holds it**.
    ///
    /// The shell's own health page (`health::read_recent_problems`) and the diagnostics report
    /// (`read_log_tail`) open the log through a second handle, in the same process, to show the
    /// user what went wrong. `LockFileEx` is a *mandatory* lock on Windows: with the lock on the
    /// log file itself, every one of those reads fails with `ERROR_LOCK_VIOLATION`, and the page
    /// that exists to explain a problem shows nothing. On Unix the lock is advisory and this test
    /// passes either way; it is the Windows run that proves the fix.
    #[test]
    fn the_log_can_be_read_while_this_process_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let (mut log, owns) = RotatingFile::open_locked(path.clone(), 1024, 1).unwrap();
        assert!(owns);
        log.write_all(b"2026-01-01T00:00:00Z  WARN replayed: held\n")
            .unwrap();
        log.flush().unwrap();

        // The two ways the application reads it: a plain read, and a handle that seeks.
        let text = fs::read_to_string(&path).expect("a second handle reads the locked log");
        assert!(text.contains("held"), "{text}");
        let mut again = File::open(&path).unwrap();
        io::Seek::seek(&mut again, io::SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut again, &mut bytes).expect("a seeking reader is not refused");
        assert!(!bytes.is_empty());
    }

    /// A second holder in the **same** process is refused too: the lock is on a handle, not on a
    /// thread, which is what makes the two-launches case testable without a child process.
    #[test]
    fn a_second_open_locked_in_this_process_is_refused_and_does_not_touch_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let (mut first, owns) = RotatingFile::open_locked(path.clone(), 100, 1).unwrap();
        assert!(owns);
        first.write_all(&[b'a'; 60]).unwrap();

        let (_second, second_owns) = RotatingFile::open_locked(path.clone(), 100, 1).unwrap();
        assert!(!second_owns, "the lock is held, so the second is refused");
        assert_eq!(fs::metadata(&path).unwrap().len(), 60);
    }

    /// Rotation at open respects the lock decision. A launch that lost the lock must not rename
    /// the winner's live log out from under it, however large it has grown.
    #[test]
    fn a_loser_does_not_rotate_the_winners_oversized_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let (mut winner, owns) = RotatingFile::open_locked(path.clone(), 100, 2).unwrap();
        assert!(owns);
        // The winner's own rotation is lazy (on the next write), so the file is oversized and
        // live, which is exactly the state a second launch finds after a burst of logging.
        winner.write_all(&[b'w'; 150]).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 150);

        let (_loser, loser_owns) = RotatingFile::open_locked(path.clone(), 100, 2).unwrap();
        assert!(!loser_owns);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            150,
            "the live log was not renamed away by the process that lost the lock"
        );
        assert!(!dir.path().join("app.log.1").exists(), "nothing rotated");
    }

    /// The winner keeps the lock through its own rotation, so a launch that starts mid-rotation
    /// is still refused.
    #[test]
    fn the_lock_survives_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let (mut winner, owns) = RotatingFile::open_locked(path.clone(), 100, 1).unwrap();
        assert!(owns);
        for _ in 0..5 {
            winner.write_all(&[b'x'; 60]).unwrap();
        }
        assert!(dir.path().join("app.log.1").exists(), "it did rotate");
        let (_other, other_owns) = RotatingFile::open_locked(path, 100, 1).unwrap();
        assert!(!other_owns, "and still holds the lock afterwards");
    }

    /// The env var that turns a test run into the "holder" half of the test below.
    const HOLDER_ENV: &str = "REPLAYED_LOG_LOCK_HOLDER";
    const HOLDER_LOG_ENV: &str = "REPLAYED_LOG_LOCK_LOG";
    const HOLDER_READY_ENV: &str = "REPLAYED_LOG_LOCK_READY";

    /// The child half of [`a_second_holder_of_the_log_is_told_so`].
    ///
    /// A byte-range lock is scoped to a *process*, so the holder has to be a real second
    /// process. Whether a second handle inside one process can take a conflicting lock is a
    /// different answer on every platform, and the case that matters is two shells.
    #[test]
    fn the_lock_holder_process() {
        if std::env::var(HOLDER_ENV).is_err() {
            return;
        }
        let path = PathBuf::from(std::env::var(HOLDER_LOG_ENV).unwrap());
        let (_log, owns) = RotatingFile::open_locked(path, 1024, 1).unwrap();
        assert!(owns, "the holder takes the lock");
        fs::write(std::env::var(HOLDER_READY_ENV).unwrap(), b"ready").unwrap();
        // Hold the lock until the parent has tried for it.
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    /// One process writes the shared log; the other one is told so.
    ///
    /// Two shells would otherwise interleave their lines into one file, and the rotation renames
    /// it — which fails on Windows while the other process holds it open. The lock is what keeps
    /// the file whole: the process that cannot take it writes a file of its own.
    #[test]
    fn a_second_holder_of_the_log_is_told_so() {
        if std::env::var(HOLDER_ENV).is_ok() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let ready = dir.path().join("holder-ready");

        let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "log_file::tests::the_lock_holder_process",
                "--nocapture",
            ])
            .env(HOLDER_ENV, "1")
            .env(HOLDER_LOG_ENV, &path)
            .env(HOLDER_READY_ENV, &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the holder process starts");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready.exists() {
            if let Ok(Some(status)) = holder.try_wait() {
                panic!("the holder process exited before it took the lock: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the holder process never took the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let (_second, owns) = RotatingFile::open_locked(path, 1024, 1).unwrap();
        let _ = holder.kill();
        let _ = holder.wait();

        assert!(!owns, "and the second one is refused it");
    }
}
