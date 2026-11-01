//! Locating and invoking the ffmpeg sidecar binaries.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// Name of the directory holding `ffmpeg`/`ffprobe`, in every layout replayed runs in.
///
/// The same word in a checkout, a `cargo run` target directory and an installed application,
/// because the bundler's resource mapping (`apps/desktop/src-tauri/tauri.conf.json`) is written
/// against it and a desktop-crate test pins the two together. Change it here and the installer
/// puts the sidecars where the lookup no longer reads.
pub const SIDECAR_DIR_NAME: &str = "binaries";

/// The directory ffmpeg/ffprobe are expected in for a packaged build: `binaries/` next to the
/// executable.
///
/// On Windows that is the whole story: the NSIS and MSI installers put resources beside the
/// `.exe`. Inside a macOS `.app` the executable is in `Contents/MacOS` and resources in
/// `Contents/Resources`; that location is [`resource_sidecar_dirs_from`].
pub fn sidecar_dir() -> PathBuf {
    sidecar_dir_from(&exe_dir())
}

/// Testable core of [`sidecar_dir`].
fn sidecar_dir_from(exe_dir: &Path) -> PathBuf {
    exe_dir.join(SIDECAR_DIR_NAME)
}

/// The directories a bundler puts the sidecars in, for the executable at `exe_dir`.
///
/// Tauri's resource directory differs by platform, which decides whether an installed app finds
/// its own ffmpeg:
///
/// - Windows (`nsis`/`msi`): resources land in the installation directory, beside the `.exe`.
///   That is already [`sidecar_dir_from`] (Tauri documents `resource_dir` as the directory
///   containing the main executable on Windows), so nothing is added here.
/// - macOS (`.app`): resources go to `<Foo>.app/Contents/Resources` while the executable is in
///   `<Foo>.app/Contents/MacOS`, so it must be named here; otherwise a packaged app would fall
///   through to `PATH` and refuse to start on a machine with no ffmpeg, defeating the sidecar
///   pipeline.
///
/// The `.app` shape is recognised from directory structure, not `cfg!(target_os)`, so the rule
/// is testable on any host and a bundle built on one platform is found on the one that runs
/// it. Linux is deliberately absent: the target is Windows, and an AppImage layout nothing here
/// builds would be an unverifiable lookup path.
pub fn resource_sidecar_dirs_from(exe_dir: &Path) -> Vec<PathBuf> {
    let in_macos_bundle = exe_dir.file_name() == Some(OsStr::new("MacOS"))
        && exe_dir.parent().and_then(Path::file_name) == Some(OsStr::new("Contents"));
    if !in_macos_bundle {
        return Vec::new();
    }
    match exe_dir.parent() {
        Some(contents) => vec![contents.join("Resources").join(SIDECAR_DIR_NAME)],
        None => Vec::new(),
    }
}

/// Directory holding the running executable, or `.` when it cannot be determined.
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// An extra, development-only place to look for the sidecars.
///
/// `cargo xtask sidecars fetch` writes `<repo>/binaries/`, but a `cargo run` executable lives in
/// `<repo>/target/<profile>/`, so the packaged-build lookup misses it by two directories.
/// Copying ~250 MB into each profile directory would duplicate the sidecars and break when the
/// profile or target directory changed. Instead this walks up from the executable to the first
/// directory holding a `Cargo.toml` (the root of a checkout, not an installation) and uses its
/// `binaries/` if present.
///
/// It is consulted only after both installed locations ([`sidecar_dir_from`] and
/// [`resource_sidecar_dirs_from`]), so a shipped sidecar is never shadowed by a stale checkout.
/// The walk stops at the first `Cargo.toml`, so no ancestor outside a project root can supply
/// binaries. It is not enough for an installed app, which must not depend on a `Cargo.toml`
/// existing above it.
pub fn dev_sidecar_dir() -> Option<PathBuf> {
    dev_sidecar_dir_from(&exe_dir())
}

/// Testable core of [`dev_sidecar_dir`]: `exe_dir` is the directory holding the executable.
pub fn dev_sidecar_dir_from(exe_dir: &Path) -> Option<PathBuf> {
    /// Enough for `target/<profile>/` and `target/<triple>/<profile>/`, and then some.
    const MAX_LEVELS: usize = 6;
    let mut current = Some(exe_dir);
    for _ in 0..MAX_LEVELS {
        let dir = current?;
        if dir.join("Cargo.toml").is_file() {
            let sidecars = dir.join("binaries");
            return sidecars.is_dir().then_some(sidecars);
        }
        current = dir.parent();
    }
    None
}

/// The ordered list of directories [`discover`] searches. Pure, so ordering is asserted without
/// a filesystem: beside the executable, then an installed bundle's resource directory, then the
/// development checkout, then `PATH`.
///
/// The order is the contract. Everything an installer can produce outranks everything a
/// developer's machine can, so an installed app is never served a stale copy from a checkout
/// above it, and `PATH` comes last: replayed cannot vouch for an arbitrary system ffmpeg, as it
/// assumes an LGPL-only, hardware-encoder-only build (see the README, "A note on the `ffmpeg`
/// build").
fn search_candidates(exe_dir: &Path, path_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = vec![sidecar_dir_from(exe_dir)];
    dirs.extend(resource_sidecar_dirs_from(exe_dir));
    dirs.extend(dev_sidecar_dir_from(exe_dir));
    dirs.extend(path_dir);
    dedupe(dirs)
}

/// Drop repeated directories, keeping the first occurrence. On Windows the bundler's resource
/// directory and "next to the executable" are the same path, which would otherwise print twice
/// in the "Searched:" list of a failure message.
fn dedupe(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

/// A `Command` for one of the sidecars, with no console window on Windows.
///
/// Load-bearing on Windows. A GUI process like `replayed.exe` allocates a console for every
/// child it spawns unless the child gets `CREATE_NO_WINDOW`, so each ffmpeg, ffprobe or `reg`
/// call flashed a terminal and the app makes a dozen while drawing a screen. The storm held the
/// main thread, leaving the window unresizable and undraggable although the window config says
/// `resizable: true`.
///
/// Every subprocess in this workspace goes through here. The flag is a no-op off Windows.
pub fn sidecar_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console is allocated for the child. `CREATE_NO_WINDOW`.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegBinaries {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegBinaries {
    /// Resolve binaries: an explicit directory, then beside the executable, then an installed
    /// bundle's resources, then a checkout's `binaries/`, then `PATH`.
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        let candidates = search_candidates(&exe_dir(), which_dir("ffmpeg"));
        Self::discover_from(explicit, &candidates)
    }

    /// Testable core: `candidates` is the list of directories searched. It avoids panics so the
    /// error message can list every location tried.
    pub fn discover_from(explicit: Option<PathBuf>, candidates: &[PathBuf]) -> Result<Self> {
        // An explicit directory is authoritative: if the caller named it, use it.
        if let Some(dir) = explicit {
            return Ok(Self {
                ffmpeg: dir.join(exe("ffmpeg")),
                ffprobe: dir.join(exe("ffprobe")),
            });
        }
        if candidates.is_empty() {
            bail!(
                "ffmpeg not found. Looked next to the executable ({}), in the resources of an \
                 installed app, in a checkout's `binaries/` directory above it (a \
                 development-only fallback), and on PATH. Run `cargo xtask sidecars fetch` \
                 or install ffmpeg.",
                sidecar_dir().display()
            );
        }
        for dir in candidates {
            let ffmpeg = dir.join(exe("ffmpeg"));
            let ffprobe = dir.join(exe("ffprobe"));
            if ffmpeg.is_file() && ffprobe.is_file() {
                return Ok(Self { ffmpeg, ffprobe });
            }
        }
        bail!(
            "ffmpeg not found. Searched, in order: {}. That order is: next to the executable \
             (where the bundled resources of an installed app are placed on Windows), then \
             the resources of an installed macOS `.app` (`Contents/Resources`), then a \
             checkout's `binaries/` directory above the executable (a development-only \
             fallback), then PATH. Run `cargo xtask sidecars fetch`, or install ffmpeg.",
            candidates
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Directory containing `ffmpeg` on `PATH`, if any.
fn which_dir(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join(exe(binary)).is_file())
}

/// How much of a child's stderr is kept: its tail, which is where ffmpeg puts the reason it
/// stopped.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// How long to wait for a pipe's reader to finish once the child has exited or been killed.
///
/// Normally immediate, because the child's end of the pipe closed with it. It is bounded because a
/// grandchild that inherited the pipe would hold it open, and a reader that never ends must not
/// become the hang the deadline exists to prevent.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Read `pipe` to its end on its own thread, delivering the bytes on the returned channel.
///
/// `keep_tail` bounds memory for a chatty stream: only the last that many bytes are kept, but the
/// pipe is still read to its end, since a pipe that is not read is what stalls the child.
fn drain<R: Read + Send + 'static>(mut pipe: R, keep_tail: Option<usize>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&chunk[..n]);
                    // Trimmed at twice the cap, so the copy is amortised rather than per read.
                    if let Some(cap) = keep_tail.filter(|&cap| out.len() > cap * 2) {
                        out.drain(..out.len() - cap);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if let Some(cap) = keep_tail.filter(|&cap| out.len() > cap) {
            out.drain(..out.len() - cap);
        }
        let _ = tx.send(out);
    });
    rx
}

/// Wait for `child` under `timeout` while draining its stdout and stderr, then collect them.
///
/// The pipes have to be read *while* waiting. They are bounded (64KiB on the development host, as
/// little as 4KiB for a Windows anonymous pipe), so a child that writes more than that blocks on
/// the write and never exits: ffmpeg over a damaged input emits a warning per packet, which is far
/// more. Reading them only afterwards, as `wait_with_output` does, turned that into a stall to the
/// full deadline that then reported only "timed out" with the real reason discarded. On a timeout
/// the stderr tail read so far is put in the error for the same reason.
fn finish(mut child: Child, timeout: Duration) -> Result<Output> {
    let stdout = child.stdout.take().map(|pipe| drain(pipe, None));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| drain(pipe, Some(STDERR_TAIL_BYTES)));
    let waited = wait_with_deadline(&mut child, timeout);
    let collect = |rx: Option<Receiver<Vec<u8>>>| {
        rx.and_then(|rx| rx.recv_timeout(DRAIN_GRACE).ok())
            .unwrap_or_default()
    };
    let stdout = collect(stdout);
    let stderr = collect(stderr);
    if let Err(e) = waited {
        let tail = String::from_utf8_lossy(&stderr);
        let tail = tail.trim();
        if tail.is_empty() {
            return Err(e);
        }
        bail!("{e}; it had written: {tail}");
    }
    let status = child.wait().context("collecting ffmpeg output")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Run a child with stdin closed and a hard timeout, capturing stdout/stderr.
///
/// The output is drained while the child runs, so a child that writes more than a pipe holds
/// finishes instead of stalling to the deadline. Only the last 64KiB of stderr is kept.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().context("spawning ffmpeg child")?;
    finish(child, timeout)
}

/// Run a child with `input` written to its stdin, then closed, and a hard timeout.
///
/// [`run_with_timeout`] cannot stand in: it gives the child an immediately-closed stdin, so a
/// probe that must encode something would have ffmpeg read zero frames and fail for an encoder
/// that works. `input` is taken by value because the writer thread must own it.
pub fn run_with_stdin(mut cmd: Command, input: Vec<u8>, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning ffmpeg child")?;
    let mut stdin = child.stdin.take().context("child stdin unavailable")?;

    // The write is on its own thread, not here: a pipe buffer is 64KiB on the development host and
    // as little as 4KiB for a Windows anonymous pipe, so a blocking write from this thread could
    // stall before the deadline loop below runs, the very hang the timeout bounds. The thread ends
    // when the bytes are in or the child is gone (a dead reader fails `write_all` with a broken
    // pipe), so nothing joins it.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // Dropping `stdin` at the end of this closure is the child's EOF.
    });

    finish(child, timeout)
}

/// Poll `child` until it exits, killing it and failing if `timeout` elapses first. Polling is
/// what makes the timeout real: a wedged child would otherwise park the caller forever.
///
/// Public for the encoder's startup throughput probe (`replayed_encoder::throughput`), which
/// feeds frames in a loop for a bounded window, owns its own writing, and needs a deadline that
/// really ends and a child that is killed and reaped rather than leaked.
pub fn wait_with_deadline(child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().context("polling ffmpeg child")? {
            Some(_) => return Ok(()),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("ffmpeg timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn explicit_path_wins() {
        let found = FfmpegBinaries::discover_from(Some(PathBuf::from("/custom/dir")), &[])
            .expect("the configured directory is used");

        // The configured directory decides where the binaries are; the platform decides what an
        // executable is called. So assert the parent and the stem, not one literal path: on Windows
        // the resolved path is `/custom/dir\ffmpeg.exe`, and the `.exe` is a legitimate difference
        // (measured on the Windows test machine).
        assert_eq!(
            found.ffmpeg.parent(),
            Some(std::path::Path::new("/custom/dir"))
        );
        assert_eq!(
            found.ffmpeg.file_stem().and_then(|s| s.to_str()),
            Some("ffmpeg")
        );
        assert_eq!(
            found.ffmpeg.extension().and_then(|s| s.to_str()),
            if cfg!(windows) { Some("exe") } else { None },
            "a Windows executable is named ffmpeg.exe; elsewhere it is named ffmpeg"
        );
        assert_eq!(
            found.ffprobe.parent(),
            found.ffmpeg.parent(),
            "and ffprobe resolves beside it, not from PATH"
        );
        assert_eq!(
            found.ffprobe.file_stem().and_then(|s| s.to_str()),
            Some("ffprobe")
        );
    }

    #[test]
    fn the_error_reports_every_searched_location() {
        let err = FfmpegBinaries::discover_from(None, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sidecar"),
            "error must name the sidecar dir: {msg}"
        );
        assert!(msg.contains("PATH"), "error must name PATH: {msg}");
        assert!(
            msg.contains("binaries"),
            "error must name the checkout's binaries/ fallback: {msg}"
        );
        assert!(
            msg.contains("resources of an installed app"),
            "error must name the installed layout too, or a user of a broken installer has \
             nothing to look at: {msg}"
        );

        let err = FfmpegBinaries::discover_from(
            None,
            &[
                PathBuf::from("/nowhere/next-to-exe"),
                PathBuf::from("/nowhere/checkout"),
            ],
        )
        .expect_err("neither directory has the binaries");
        let msg = err.to_string();
        assert!(msg.contains("/nowhere/next-to-exe"), "{msg}");
        assert!(msg.contains("/nowhere/checkout"), "{msg}");
        assert!(msg.contains("cargo xtask sidecars fetch"), "{msg}");
    }

    /// A checkout laid out like the real one: `Cargo.toml` and `binaries/` at the root, the
    /// running executable two levels down in `target/debug/`.
    fn fake_checkout(root: &Path) -> (PathBuf, PathBuf) {
        let exe_dir = root.join("target/debug");
        std::fs::create_dir_all(&exe_dir).expect("creating target/debug");
        std::fs::create_dir_all(root.join("binaries")).expect("creating binaries/");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("writing Cargo.toml");
        (exe_dir, root.join("binaries"))
    }

    /// Write a complete `ffmpeg`/`ffprobe` pair into `dir`, creating it if needed.
    fn write_sidecar_pair(dir: &Path) {
        std::fs::create_dir_all(dir).expect("creating a sidecar directory");
        std::fs::write(dir.join(exe("ffmpeg")), b"binary").expect("writing ffmpeg");
        std::fs::write(dir.join(exe("ffprobe")), b"binary").expect("writing ffprobe");
    }

    /// The layout `tauri build` produces inside a macOS `.app` for a `bundle.resources` mapping of
    /// `binaries/`: the executable in `<Foo>.app/Contents/MacOS`, the sidecars in
    /// `<Foo>.app/Contents/Resources/binaries`. Returns `(exe_dir, resource_dir)`.
    fn fake_installed_app(root: &Path) -> (PathBuf, PathBuf) {
        let contents = root.join("replayed.app").join("Contents");
        let exe_dir = contents.join("MacOS");
        std::fs::create_dir_all(&exe_dir).expect("creating Contents/MacOS");
        let resources = contents.join("Resources").join(SIDECAR_DIR_NAME);
        write_sidecar_pair(&resources);
        (exe_dir, resources)
    }

    #[test]
    fn dev_fallback_finds_the_checkout_binaries_above_target_debug() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, expected) = fake_checkout(tmp.path());
        assert_eq!(dev_sidecar_dir_from(&exe_dir), Some(expected.clone()));

        // A test binary lives one level further down, in `target/debug/deps`.
        let deps = exe_dir.join("deps");
        std::fs::create_dir_all(&deps).expect("creating deps");
        assert_eq!(dev_sidecar_dir_from(&deps), Some(expected));
    }

    #[test]
    fn dev_fallback_stops_at_the_first_cargo_manifest() {
        // A nested package without its own binaries/ must not reach past itself into an outer
        // checkout's copy: that is how a stale one would shadow the right one.
        let tmp = TempDir::new().expect("temp dir");
        let (_outer_exe, _outer_binaries) = fake_checkout(tmp.path());
        let inner = tmp.path().join("crates/inner");
        let inner_exe = inner.join("target/debug");
        std::fs::create_dir_all(&inner_exe).expect("creating inner target/debug");
        std::fs::write(inner.join("Cargo.toml"), "[package]\n").expect("writing inner Cargo.toml");
        assert_eq!(
            dev_sidecar_dir_from(&inner_exe),
            None,
            "the walk must stop at the inner manifest, not climb into the outer checkout"
        );
    }

    #[test]
    fn dev_fallback_gives_up_without_a_checkout() {
        let tmp = TempDir::new().expect("temp dir");
        let exe_dir = tmp.path().join("Program Files/replayed");
        std::fs::create_dir_all(&exe_dir).expect("creating install dir");
        // No Cargo.toml on the way up: an installed layout must not acquire a fallback.
        assert_eq!(dev_sidecar_dir_from(&exe_dir), None);
    }

    /// The load-bearing test for packaging.
    ///
    /// `Contents/Resources/binaries` is where the bundler's `resources` mapping lands, not "next to
    /// the executable", so a lookup that only knew `exe_dir/binaries` would fall through to `PATH`
    /// in every installed app and refuse to start on a machine with no system ffmpeg.
    #[test]
    fn installed_app_finds_the_sidecars_in_its_own_bundle_resources() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());

        let found = FfmpegBinaries::discover_from(
            None,
            &search_candidates(&exe_dir, Some(PathBuf::from("/usr/bin"))),
        )
        .expect("an installed app must find the sidecars it was bundled with");

        assert_eq!(found.ffmpeg, resources.join(exe("ffmpeg")));
        assert_eq!(found.ffprobe, resources.join(exe("ffprobe")));
        assert!(
            found.ffmpeg.starts_with(tmp.path().join("replayed.app")),
            "the resolved binary must be inside the application bundle, was {}",
            found.ffmpeg.display()
        );
    }

    #[test]
    fn a_bundle_searches_its_resources_before_the_checkout_and_before_path() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());
        // The app sits inside a developer's checkout, so the development fallback is live
        // too: the bundle's resources must still come first.
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").expect("writing Cargo.toml");
        let checkout = tmp.path().join("binaries");
        write_sidecar_pair(&checkout);
        assert_eq!(
            search_candidates(&exe_dir, Some(PathBuf::from("/usr/bin"))),
            vec![
                exe_dir.join(SIDECAR_DIR_NAME),
                resources,
                checkout,
                PathBuf::from("/usr/bin")
            ],
            "order is: next to the executable, then the bundle's resources, then the \
             checkout, then PATH"
        );
    }

    /// Precedence with every source populated at once, resolved rather than merely ordered.
    #[test]
    fn an_installed_sidecar_outranks_the_checkout_and_path() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").expect("writing Cargo.toml");
        let stale_checkout = tmp.path().join("binaries");
        write_sidecar_pair(&stale_checkout);
        let on_path = tmp.path().join("on-path");
        write_sidecar_pair(&on_path);

        let found = FfmpegBinaries::discover_from(
            None,
            &search_candidates(&exe_dir, Some(on_path.clone())),
        )
        .expect("one of the four sources has the pair");

        assert_eq!(
            found.ffmpeg,
            resources.join(exe("ffmpeg")),
            "the shipped sidecar must win over both the checkout and PATH"
        );
        assert_ne!(found.ffmpeg, stale_checkout.join(exe("ffmpeg")));
        assert_ne!(found.ffmpeg, on_path.join(exe("ffmpeg")));
    }

    #[test]
    fn the_checkout_fallback_outranks_path_when_nothing_is_installed() {
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, checkout) = fake_checkout(tmp.path());
        write_sidecar_pair(&checkout);
        let on_path = tmp.path().join("on-path");
        write_sidecar_pair(&on_path);

        let found = FfmpegBinaries::discover_from(
            None,
            &search_candidates(&exe_dir, Some(on_path.clone())),
        )
        .expect("the checkout pair is found");

        assert_eq!(found.ffmpeg, checkout.join(exe("ffmpeg")));
        assert_ne!(found.ffmpeg, on_path.join(exe("ffmpeg")));
    }

    #[test]
    fn a_pair_next_to_the_executable_outranks_the_bundle_resources() {
        // Reachable if something puts a pair in `Contents/MacOS/binaries` (a hand-assembled folder, or
        // Tauri's `externalBin`, which copies into `Contents/MacOS`). Stated as a rule so the order is
        // a decision, not an accident of how the candidate list is built.
        let tmp = TempDir::new().expect("temp dir");
        let (exe_dir, resources) = fake_installed_app(tmp.path());
        let beside_the_exe = exe_dir.join(SIDECAR_DIR_NAME);
        write_sidecar_pair(&beside_the_exe);

        let found = FfmpegBinaries::discover_from(None, &search_candidates(&exe_dir, None))
            .expect("the pair next to the executable is found");

        assert_eq!(found.ffmpeg, beside_the_exe.join(exe("ffmpeg")));
        assert_ne!(found.ffmpeg, resources.join(exe("ffmpeg")));
    }

    #[test]
    fn a_directory_that_is_not_an_app_bundle_has_no_resource_location() {
        // A false positive would put a directory that cannot exist on the search list and print it in
        // every "ffmpeg not found" message.
        let tmp = TempDir::new().expect("temp dir");
        let install = tmp.path().join("Program Files/replayed");
        std::fs::create_dir_all(&install).expect("creating the installation directory");
        assert!(resource_sidecar_dirs_from(&install).is_empty());

        // `MacOS` is not enough on its own: the parent has to be a `Contents` directory.
        let not_a_bundle = tmp.path().join("somewhere/MacOS");
        std::fs::create_dir_all(&not_a_bundle).expect("creating somewhere/MacOS");
        assert!(resource_sidecar_dirs_from(&not_a_bundle).is_empty());
    }

    /// The same lookup pointed at a bundle that was actually built rather than a tree this test
    /// wrote: "the rule is right" versus "the artifact is shaped as the rule expects".
    ///
    /// Opt-in, since it needs a build artifact CI does not produce. Point it at a bundle and it
    /// resolves the executable's own `Contents/MacOS` through the production code path:
    ///
    /// ```text
    /// REPLAYED_APP_BUNDLE=apps/desktop/src-tauri/target/release/bundle/macos/replayed.app \
    ///   cargo test -p replayed-media --lib a_real_app_bundle_beats_path
    /// ```
    ///
    /// `which_dir` is real, so when it finds a system ffmpeg this also shows the bundle's copy wins
    /// over `PATH`.
    #[test]
    fn a_real_app_bundle_beats_path() {
        let Ok(app) = std::env::var("REPLAYED_APP_BUNDLE") else {
            return;
        };
        let app = PathBuf::from(app);
        let exe_dir = app.join("Contents").join("MacOS");
        assert!(
            exe_dir.is_dir(),
            "REPLAYED_APP_BUNDLE must point at a `.app` directory: {} has no Contents/MacOS",
            app.display()
        );

        let candidates = search_candidates(&exe_dir, which_dir("ffmpeg"));
        let found = FfmpegBinaries::discover_from(None, &candidates)
            .unwrap_or_else(|e| panic!("nothing found in the built bundle {}\n{e}", app.display()));

        assert_eq!(
            found.ffmpeg,
            app.join("Contents")
                .join("Resources")
                .join(SIDECAR_DIR_NAME)
                .join(exe("ffmpeg"))
        );
        assert!(found.ffprobe.is_file(), "{:?} exists too", found.ffprobe);
    }

    #[test]
    fn a_directory_holding_only_one_of_the_two_binaries_is_not_used() {
        let tmp = TempDir::new().expect("temp dir");
        let half = tmp.path().join("half/binaries");
        std::fs::create_dir_all(&half).expect("creating a binaries dir");
        std::fs::write(half.join(exe("ffmpeg")), b"binary").expect("writing ffmpeg");
        let err = FfmpegBinaries::discover_from(None, std::slice::from_ref(&half))
            .expect_err("half a pair is not a sidecar directory");
        assert!(err.to_string().contains("half"), "{err}");
    }

    /// A child that writes far more than a pipe holds, to both streams, finishes.
    ///
    /// Before the pipes were drained while waiting, this child blocked on its first full pipe and
    /// was killed at the deadline, reporting only that it had timed out.
    #[cfg(unix)]
    #[test]
    fn a_child_that_writes_more_than_a_pipe_holds_still_finishes() {
        const MIB: usize = 1024 * 1024;
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2; exit 0",
        ]);
        let started = std::time::Instant::now();
        let out = run_with_timeout(cmd, Duration::from_secs(30)).expect("the child finishes");
        assert!(out.status.success());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "it must not have waited out its deadline"
        );
        assert_eq!(out.stdout.len(), MIB, "stdout is kept whole");
        assert_eq!(
            out.stderr.len(),
            STDERR_TAIL_BYTES,
            "stderr is bounded to its tail"
        );
    }

    /// The same for a child fed through stdin, which is the path the in-memory clip save takes.
    #[cfg(unix)]
    #[test]
    fn a_chatty_child_fed_through_stdin_still_finishes() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "cat >/dev/null; head -c 1048576 /dev/zero >&2; exit 0",
        ]);
        let out = run_with_stdin(cmd, vec![0u8; 256 * 1024], Duration::from_secs(30))
            .expect("the child finishes");
        assert!(out.status.success());
        assert_eq!(out.stderr.len(), STDERR_TAIL_BYTES);
    }

    /// A timeout names what the child had said, instead of discarding it.
    #[cfg(unix)]
    #[test]
    fn a_timeout_carries_the_stderr_the_child_wrote() {
        let mut cmd = Command::new("sh");
        // `exec`, so the sleep is the killed process itself and holds no pipe of its own.
        cmd.args(["-c", "echo diagnostic-text >&2; exec sleep 30"]);
        let err = run_with_timeout(cmd, Duration::from_millis(400)).expect_err("it times out");
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "{msg}");
        assert!(msg.contains("diagnostic-text"), "{msg}");
    }
}
