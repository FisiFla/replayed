//! Shared fixtures for the per-module test suites (a real store, real ffmpeg output).

use super::*;
use replayed_media::binaries::run_with_timeout;
use replayed_store::NewClip;
use std::time::Duration;

/// The wall clock the session fixtures start at: deliberately enormous, so a timeline offset
/// computed against the wall start instead of the media epoch is unmistakably wrong.
pub(crate) const WALL_START_MS: i64 = 1_700_000_000_000;

/// A cap nothing reaches and an age nothing exceeds: the policy is a no-op, so a test not about
/// it cannot trip over it.
pub(crate) const GENEROUS: StorageConfigView = StorageConfigView {
    max_total_bytes: u64::MAX,
    max_age_days: 3_650,
};

pub(crate) struct Fixture {
    /// Held so the temporary directory outlives the store and the clips in it.
    pub(crate) _dir: tempfile::TempDir,
    pub(crate) store: Store,
    pub(crate) paths: AppPaths,
    pub(crate) storage: StorageConfigView,
    pub(crate) bins: FfmpegBinaries,
    pub(crate) warnings: Vec<String>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::with_storage(GENEROUS)
    }

    pub(crate) fn with_storage(storage: StorageConfigView) -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let bins = FfmpegBinaries::discover(None)
            .expect("ffmpeg and ffprobe must be on PATH to run these tests");
        let paths = AppPaths::resolve(dir.path(), "");
        std::fs::create_dir_all(&paths.clips_dir).unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        store.migrate().unwrap();
        Self {
            _dir: dir,
            store,
            paths,
            storage,
            bins,
            warnings: Vec::new(),
        }
    }

    pub(crate) fn deps(&self) -> Deps<'_> {
        Deps {
            store: &self.store,
            bins: Some(&self.bins),
            paths: &self.paths,
            storage: &self.storage,
            warnings: &self.warnings,
        }
    }

    /// The same dependencies with no ffmpeg found — what an install without the
    /// sidecars sees.
    pub(crate) fn deps_without_ffmpeg(&self) -> Deps<'_> {
        Deps {
            bins: None,
            ..self.deps()
        }
    }

    /// Index a clip whose file exists on disk but is never opened or decoded: an empty stand-in,
    /// enough for the list, favourite, policy and validation tests. A test about a missing file
    /// removes it afterwards.
    pub(crate) fn add_row(
        &self,
        name: &str,
        started_at_ms: u64,
        duration_ms: u64,
        size_bytes: u64,
    ) -> i64 {
        let path = self.paths.clips_dir.join(name);
        std::fs::write(&path, b"").unwrap();
        self.store
            .insert_clip(&NewClip {
                path,
                started_at_ms,
                duration_ms,
                size_bytes,
                codec: "mpeg4".to_string(),
            })
            .unwrap()
    }

    /// Write a real, decodable clip into the clips directory and index it.
    pub(crate) fn add_real_clip(&self, name: &str, seconds: u32) -> (i64, PathBuf) {
        let path = self.paths.clips_dir.join(name);
        write_test_video(&self.bins, &path, seconds);
        let size_bytes = std::fs::metadata(&path).unwrap().len();
        let id = self
            .store
            .insert_clip(&NewClip {
                path: path.clone(),
                started_at_ms: 0,
                duration_ms: u64::from(seconds) * 1_000,
                size_bytes,
                codec: "mpeg4".to_string(),
            })
            .unwrap();
        (id, path)
    }

    /// A finished session whose recorded file is a real, decodable video.
    ///
    /// The wall start and media epoch are deliberately different and far apart, which is the reason
    /// the column exists; equal values could not tell the two clocks apart.
    ///
    /// The segments and the recorded file live in different directories, as in the real layout (the
    /// recorder's `session_file_path` writes the concatenation into a sessions output directory).
    pub(crate) fn add_finished_session(&self, seconds: u32, media_epoch_ms: i64) -> (i64, PathBuf) {
        // Inside the same temporary root the fixture owns; `AppPaths` does not publish the app-data
        // directory itself.
        let root = self
            .paths
            .clips_dir
            .parent()
            .expect("the clips directory has a parent")
            .to_path_buf();
        let scratch_dir = root.join("sessions").join("session-1");
        let out_dir = root.join("sessions-out");
        std::fs::create_dir_all(&scratch_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        let final_path = out_dir.join("session-1.mp4");
        write_test_video(&self.bins, &final_path, seconds);

        let id = self
            .store
            .start_session(
                Some("Dota 2"),
                replayed_store::SESSION_MODE_SESSION,
                WALL_START_MS,
                &scratch_dir.to_string_lossy(),
                media_epoch_ms,
            )
            .unwrap();
        self.store
            .end_session(
                id,
                WALL_START_MS + i64::from(seconds) * 1_000,
                Some(&final_path.to_string_lossy()),
                std::fs::metadata(&final_path).unwrap().len() as i64,
                // The fixture encodes `seconds` of video, so that IS its media length.
                i64::from(seconds) * 1_000,
            )
            .unwrap();
        (id, final_path)
    }

    /// A session row that is still running, so it has no concatenated file. `mode` is which engine
    /// opened it; a buffer run and a full-session run look alike until one stops.
    pub(crate) fn add_running_session(&self, mode: &str) -> (i64, PathBuf) {
        let dir = self
            .paths
            .clips_dir
            .parent()
            .expect("the clips directory has a parent")
            .join("sessions")
            .join(mode);
        std::fs::create_dir_all(&dir).unwrap();
        let id = self
            .store
            .start_session(None, mode, WALL_START_MS, &dir.to_string_lossy(), 0)
            .unwrap();
        (id, dir)
    }
}

/// Encode `seconds` of `testsrc2` into `path` with ffmpeg, as a clip would be. `-g 10` at 10 fps
/// puts a keyframe every second, so a stream-copy cut has keyframe structure to work with.
pub(crate) fn write_test_video(bins: &FfmpegBinaries, path: &Path, seconds: u32) {
    write_test_video_sized(bins, path, seconds, 320, 180);
}

/// The same, at an explicit frame size: the tests that need two clips whose stream layouts
/// disagree (a combine must refuse them) cannot use the default size for both.
pub(crate) fn write_test_video_sized(
    bins: &FfmpegBinaries,
    path: &Path,
    seconds: u32,
    width: u32,
    height: u32,
) {
    let mut cmd = replayed_media::sidecar_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc2=size={width}x{height}:rate=10:duration={seconds}"
        ))
        .args([
            "-c:v", "mpeg4", "-q:v", "5", "-g", "10", "-pix_fmt", "yuv420p",
        ])
        .arg(path);
    let out = run_with_timeout(cmd, Duration::from_secs(60)).expect("spawning ffmpeg");
    assert!(
        out.status.success(),
        "the test fixture could not be encoded: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(path.is_file(), "ffmpeg reported success but wrote no file");
}

/// Hash the video packets ffmpeg reads from `src` at `seek_ms`, for `dur_ms` if given.
///
/// `-f md5` hashes packet payloads, so equal hashes mean byte-identical streams. Nothing is
/// decoded or encoded on either side, which makes it evidence about a stream copy rather than a
/// proxy for one.
pub(crate) fn stream_md5(
    bins: &FfmpegBinaries,
    src: &Path,
    seek_ms: u64,
    dur_ms: Option<u64>,
) -> String {
    let mut cmd = replayed_media::sidecar_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-y", "-ss"])
        .arg(format!("{:.3}", seek_ms as f64 / 1000.0))
        .arg("-i")
        .arg(src);
    if let Some(ms) = dur_ms {
        cmd.args(["-t"]).arg(format!("{:.3}", ms as f64 / 1000.0));
    }
    cmd.args(["-c", "copy", "-f", "md5", "-"]);
    let out = run_with_timeout(cmd, Duration::from_secs(60)).expect("spawning ffmpeg");
    assert!(
        out.status.success(),
        "the md5 run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
