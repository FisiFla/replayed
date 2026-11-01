//! `xtask verify`: runs the acceptance checks against the CLI and writes a Markdown report a
//! human can read and attach to an issue.
//!
//! It starts the buffer with a bounded configuration, drives the clip path through the CLI's
//! verification-only trigger (`buffer --self-test-clip-after`, an in-process call to the same
//! `Recorder::clip_now` the hotkey calls), measures the process, probes the clip and writes
//! everything down. Pressing the hotkey from a script would mean synthesising input, which is
//! unsafe on a machine whose anti-cheat treats it as hostile.
//!
//! # It does not synthesise input
//!
//! **Nothing in this file sends, simulates or injects a keyboard or mouse event, and nothing
//! enumerates windows.** The self-test flag is the whole mechanism, and it is a function call
//! inside the process under test. That property is the reason the harness exists.
//!
//! # What the exit code means
//!
//! A check that *failed* fails the run. A check that could not be performed does not —
//! unless `--strict` is given. That asymmetry is deliberate: the harness exists to be run
//! on machines that cannot perform half of it (a development host with no GPU encoder, a
//! runner with no desktop), and a report that says plainly what was not judged is worth
//! more than a refusal to produce one. `--strict` is for the caller who wants the absence
//! of evidence to be as loud as the evidence of a defect.
//!
//! Either way the report's verdict line carries the count of what was not judged, so a
//! green verdict is never a statement about checks that did not run.
//!
//! # What it cannot do, and says so
//!
//! Every check it cannot perform is listed in the report as *not performed* with the reason,
//! never silently omitted. It cannot verify:
//!
//! * the desktop GUI (no window is opened);
//! * a real keypress (the hotkey listener is installed but not driven);
//! * anything needing a real game (the event sources are off and nothing is contacted);
//! * the live-source clock divergence between the WGC video clock and the WASAPI audio clock;
//! * whatever the machine's own state makes impossible: a machine whose every encoder works has
//!   no unusable vendor to test, and a natively 48 kHz endpoint never exercises the
//!   engine-side conversion.
//!
//! It also cannot tell whether the pixels were the desktop, only that frames flowed: the
//! frame-content check refuses a single-colour frame, but a black screen would pass as one.

use crate::probe::{self, ProbeReport};
use anyhow::{bail, Context, Result};
use replayed_media::{FfmpegBinaries, MediaInfo};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------------------
// The documented, bounded run configuration
// ---------------------------------------------------------------------------------------

/// How long the buffer is asked to accumulate before the self-test clip, by default.
const DEFAULT_SECONDS: u64 = 45;

/// How often the process's CPU time and working set are sampled while it runs.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// The bounded configuration the harness starts the buffer with.
///
/// Stated in the report verbatim, because a measurement without its configuration is not
/// evidence. Not the shipping defaults (`config.example.toml`): chosen so that one run under
/// two minutes exercises the clip path, the RAM cap, eviction and a steady-state window.
struct RunConfig {
    pre_seconds: u64,
    post_seconds: u64,
    segment_time: u64,
    /// The ring's RAM budget: `buffer.ram_cap_bytes`, the only cap the engine has.
    ///
    /// There is no disk cap. The replay buffer holds its footage in RAM and writes nothing to
    /// the disk while it is only buffering, so a "scratch cap" would have had nothing to bound.
    ram_cap_bytes: u64,
    fps: u32,
    bitrate_kbps: u32,
    audio_bitrate_kbps: u32,
}

/// The one configuration this harness runs with. `segment_time = 1` makes cuts snap to a 1 s
/// grid (+-0.5 s tolerance); the 16 MiB RAM budget is small enough that a ~45 s run of ~1.0 MB/s
/// must evict, and large enough to hold several times the 8 s window.
const RUN_CONFIG: RunConfig = RunConfig {
    pre_seconds: 5,
    post_seconds: 3,
    segment_time: 1,
    ram_cap_bytes: 16 * 1024 * 1024,
    fps: 30,
    bitrate_kbps: 8_000,
    audio_bitrate_kbps: 128,
};

impl RunConfig {
    /// The whole config file, as the CLI parses it.
    ///
    /// `vendor` is `auto` for the buffer run; the unusable-vendor check gets a `vendor`-forced
    /// copy. The event sources are off: this run must touch nothing but its own capture.
    fn to_toml(&self, vendor: &str) -> String {
        format!(
            "# Written by `cargo xtask verify`. A bounded verification configuration, not a\n\
             # user's config: see xtask/src/verify.rs.\n\
             [buffer]\n\
             pre_seconds = {pre}\n\
             post_seconds = {post}\n\
             segment_time = {segment}\n\
             ram_cap_bytes = {cap}\n\
             scratch_dir = \"\"\n\
             \n\
             [encode]\n\
             vendor = \"{vendor}\"\n\
             codec = \"h264\"\n\
             bitrate_kbps = {bitrate}\n\
             fps = {fps}\n\
             output_size = \"\"\n\
             \n\
             [audio]\n\
             enabled = true\n\
             source = \"loopback\"\n\
             codec = \"aac\"\n\
             bitrate_kbps = {audio_bitrate}\n\
             \n\
             [storage]\n\
             clips_dir = \"\"\n\
             max_total_bytes = 2147483648\n\
             max_age_days = 7\n\
             \n\
             [hotkeys]\n\
             clip = \"Ctrl+F8\"\n\
             \n\
             [events]\n\
             lol_poll_enabled = false\n\
             gsi_port = 0\n",
            pre = self.pre_seconds,
            post = self.post_seconds,
            segment = self.segment_time,
            cap = self.ram_cap_bytes,
            bitrate = self.bitrate_kbps,
            fps = self.fps,
            audio_bitrate = self.audio_bitrate_kbps,
        )
    }

    /// The pre-roll plus post-roll window, in ms — what a clip must measure.
    fn window_ms(&self) -> u64 {
        (self.pre_seconds + self.post_seconds) * 1000
    }
}

// ---------------------------------------------------------------------------------------
// The sandbox: the one directory this harness deletes
// ---------------------------------------------------------------------------------------

/// The file the harness writes into a work directory it created, and the only evidence it
/// accepts that a non-empty directory is its own to delete.
///
/// `--work-dir` is a path the caller typed, and the run empties it with `remove_dir_all`
/// before it starts (the engine adopts an existing scratch directory, which would corrupt a
/// measurement). Without a rule about *which* directory may be emptied, `--work-dir ~` or
/// `--work-dir .` is a recursive delete of the user's home or of the checkout, reported as a
/// passing check. A marker the harness wrote itself is the rule that cannot be satisfied by
/// accident: nobody creates `.replayed-verify-sandbox` in a directory they care about.
const SANDBOX_MARKER: &str = ".replayed-verify-sandbox";

/// The directories a sandbox may be neither equal to nor contain (it is deleted recursively,
/// so a directory that merely *contains* one of these would delete it too).
fn protected_dirs(repo: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut dirs = vec![("this repository's checkout", repo.to_path_buf())];
    for var in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            dirs.push(("your home directory", PathBuf::from(home)));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(("the current directory", cwd));
    }
    dirs
}

/// What [`vet_sandbox`] found at the work directory.
#[derive(Debug, PartialEq, Eq)]
enum Sandbox {
    /// Nothing there yet: nothing to delete.
    Absent,
    /// A directory the harness may delete (the canonical path, so a symlink is judged by
    /// where it leads and not by what it is called).
    Removable(PathBuf),
}

/// Decide whether `dir` may be deleted, **before** anything is deleted. Fails closed: the
/// answer is "yes" only for a directory that is missing, empty, or carries [`SANDBOX_MARKER`],
/// and that is not a filesystem or drive root and neither is nor contains a protected
/// directory (see [`protected_dirs`]).
fn vet_sandbox(dir: &Path, protected: &[(&'static str, PathBuf)]) -> Result<Sandbox> {
    let canonical = match dir.canonicalize() {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Sandbox::Absent),
        Err(err) => bail!("cannot resolve --work-dir {}: {err}", dir.display()),
    };
    let refuse = |why: &str| -> Result<Sandbox> {
        bail!(
            "refusing to use --work-dir {} ({}): it is deleted recursively on every run, and \
             {why}",
            dir.display(),
            canonical.display()
        )
    };
    // `/`, `C:\`, `\\?\C:\`: no parent means a root.
    if canonical.parent().is_none() {
        return refuse("that is a filesystem root");
    }
    for (what, path) in protected {
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        if path.starts_with(&canonical) {
            return refuse(&format!("it is, or contains, {what} ({})", path.display()));
        }
    }
    if !canonical.is_dir() {
        return refuse("it is not a directory");
    }
    if canonical.join(SANDBOX_MARKER).is_file() {
        return Ok(Sandbox::Removable(canonical));
    }
    let empty = std::fs::read_dir(&canonical)
        .with_context(|| format!("reading {}", canonical.display()))?
        .next()
        .is_none();
    if empty {
        return Ok(Sandbox::Removable(canonical));
    }
    refuse(&format!(
        "it is not empty and has no `{SANDBOX_MARKER}` file, so this harness did not create \
         it. Pick a directory that does not exist, or delete this one yourself"
    ))
}

/// Create the (empty) sandbox and write the marker that lets the next run delete it.
fn mark_sandbox(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join(SANDBOX_MARKER),
        "created by `cargo xtask verify`; the next run deletes this directory\n",
    )
}

// ---------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
struct Options {
    report: PathBuf,
    work_dir: PathBuf,
    seconds: u64,
    dev_software: bool,
    no_build: bool,
    cli: Option<PathBuf>,
    clean_work: bool,
    /// Whether a check that could not be performed counts as a failure. See [`run_failed`].
    strict: bool,
}

fn usage() -> String {
    "\
usage:
  xtask verify [OPTIONS]

Runs the Phase 1 acceptance checks against the replayed CLI and writes one Markdown
report. It starts the buffer with a bounded configuration (stated in the report), drives
the clip through the CLI's `--self-test-clip-after` trigger — which sends no keyboard or
mouse input — probes the clip it produced, and exits non-zero if any check failed.

options:
  --seconds <N>        how much media to buffer before the self-test clip (default 45)
  --report <PATH>      where the report goes (default target/verify/report.md)
  --work-dir <PATH>    where the run's data directory goes (default target/verify/work).
                       It is DELETED and recreated at the start of every run, so the
                       harness only does that to a directory it created itself (it holds a
                       `.replayed-verify-sandbox` marker) or one that is empty, and refuses
                       your home directory, a filesystem root and the repository checkout.
  --cli <PATH>         an already-built replayed-cli to test (skips the release build)
  --no-build           do not build; use target/release/replayed-cli as it is
  --dev-software-encoder
                       build and run with libx264 instead of a hardware encoder, for a
                       host with no GPU encoder (a development host). Checks that need
                       the real capture/encoder path are then reported as not performed.
  --clean-work         remove the work directory after the report is written
  --strict             also exit non-zero when a check could not be performed, not only
                       when one failed. Off by default: the harness exists to run on
                       machines that cannot perform half of it, so a skipped check is
                       reported rather than punished — unless the caller asks otherwise.
"
    .to_string()
}

fn parse_options(args: &[String], repo: &Path) -> Result<Options> {
    let mut opts = Options {
        report: repo.join("target/verify/report.md"),
        work_dir: repo.join("target/verify/work"),
        seconds: DEFAULT_SECONDS,
        dev_software: false,
        no_build: false,
        cli: None,
        clean_work: false,
        strict: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value = |flag: &str, it: &mut std::slice::Iter<'_, String>| -> Result<String> {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--report" => opts.report = PathBuf::from(value("--report", &mut it)?),
            "--work-dir" => opts.work_dir = PathBuf::from(value("--work-dir", &mut it)?),
            "--cli" => opts.cli = Some(PathBuf::from(value("--cli", &mut it)?)),
            "--seconds" => {
                let raw = value("--seconds", &mut it)?;
                let seconds: u64 = raw
                    .parse()
                    .with_context(|| format!("--seconds expects whole seconds, got {raw:?}"))?;
                if seconds < 5 {
                    bail!("--seconds must be at least 5 (a shorter run cannot fill the ring)");
                }
                opts.seconds = seconds;
            }
            "--dev-software-encoder" => opts.dev_software = true,
            "--no-build" => opts.no_build = true,
            "--clean-work" => opts.clean_work = true,
            "--strict" => opts.strict = true,
            other => bail!("unexpected argument {other:?}\n\n{}", usage()),
        }
    }
    Ok(opts)
}

// ---------------------------------------------------------------------------------------
// Checks and report
// ---------------------------------------------------------------------------------------

/// The outcome of one check, as the report renders it.
enum Outcome {
    Pass,
    Fail(String),
    /// The check could not be performed on this run. Never silent: the reason is printed.
    NotPerformed(String),
}

struct Check {
    id: &'static str,
    what: &'static str,
    criterion: &'static str,
    expected: String,
    measured: String,
    outcome: Outcome,
}

struct Session {
    opts: Options,
    repo: PathBuf,
    checks: Vec<Check>,
    sections: Vec<(String, String)>,
    bin: Option<FfmpegBinaries>,
    cli: Option<PathBuf>,
    probe: Option<ProbeReport>,
    /// The tree the buffer run wrote into (config, scratch, clips, index).
    app_data: Option<PathBuf>,
    run: Option<RunOutput>,
    /// Whether checks that need the real capture backend and a hardware encoder should be
    /// judged (see [`Session::judges_hardware_path`]).
    hardware_path: bool,
}

impl Session {
    fn new(opts: Options, repo: PathBuf) -> Self {
        // On Windows the harness runs the shipping path (WGC + WASAPI + a hardware encoder) unless told
        // to use the software encoder; elsewhere it runs the synthetic stubs, which prove nothing about
        // capture. Checks that depend on the real path are reported, not judged, in the latter cases.
        let hardware_path = cfg!(windows) && !opts.dev_software;
        Session {
            opts,
            repo,
            checks: Vec::new(),
            sections: Vec::new(),
            bin: None,
            cli: None,
            probe: None,
            app_data: None,
            run: None,
            hardware_path,
        }
    }

    fn note(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.sections.push((title.into(), body.into()));
    }

    fn push(
        &mut self,
        id: &'static str,
        what: &'static str,
        criterion: &'static str,
        expected: impl Into<String>,
        measured: impl Into<String>,
        outcome: Outcome,
    ) {
        self.checks.push(Check {
            id,
            what,
            criterion,
            expected: expected.into(),
            measured: measured.into(),
            outcome,
        });
    }

    fn pass(
        &mut self,
        id: &'static str,
        what: &'static str,
        criterion: &'static str,
        expected: impl Into<String>,
        measured: impl Into<String>,
    ) {
        self.push(id, what, criterion, expected, measured, Outcome::Pass);
    }

    fn fail(
        &mut self,
        id: &'static str,
        what: &'static str,
        criterion: &'static str,
        expected: impl Into<String>,
        measured: impl Into<String>,
        reason: impl Into<String>,
    ) {
        self.push(
            id,
            what,
            criterion,
            expected,
            measured,
            Outcome::Fail(reason.into()),
        );
    }

    fn not_performed(
        &mut self,
        id: &'static str,
        what: &'static str,
        criterion: &'static str,
        expected: impl Into<String>,
        measured: impl Into<String>,
        reason: impl Into<String>,
    ) {
        self.push(
            id,
            what,
            criterion,
            expected,
            measured,
            Outcome::NotPerformed(reason.into()),
        );
    }

    /// A check that needs the real capture path and a hardware encoder.
    ///
    /// On a stub/software run the numbers are reported but not judged: a `--dev-software` run says
    /// nothing about hardware encoding, and a synthetic source nothing about the capture rate. The
    /// rule is printed in the report so a reader knows why a row is unjudged.
    #[allow(clippy::too_many_arguments)]
    fn hardware_check(
        &mut self,
        id: &'static str,
        what: &'static str,
        criterion: &'static str,
        expected: impl Into<String>,
        measured: impl Into<String>,
        outcome: Outcome,
    ) {
        let expected = expected.into();
        let measured = measured.into();
        if self.hardware_path {
            self.push(id, what, criterion, expected, measured, outcome);
        } else {
            let reason = "this run used the synthetic capture source and/or the development \
                          software encoder, so the check says nothing about the real capture \
                          + hardware-encoder path; the measured value is reported anyway";
            self.push(
                id,
                what,
                criterion,
                expected,
                measured,
                Outcome::NotPerformed(reason.to_string()),
            );
        }
    }

    fn record_step_error(&mut self, step: &'static str, err: anyhow::Error) {
        self.fail(
            step,
            step,
            "—",
            "the step completes",
            "the step failed",
            format!("{err:#}"),
        );
    }
}

// ---------------------------------------------------------------------------------------
// The run of the CLI under test
// ---------------------------------------------------------------------------------------

/// One line of the child's output, with the tracing timestamp when it has one.
#[derive(Debug, Clone)]
struct LogLine {
    at_ms: Option<u64>,
    text: String,
}

/// A sample of the child's resource use, taken while it ran.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Seconds since the child was spawned.
    t_s: f64,
    /// Cumulative CPU time (user + kernel), seconds.
    cpu_s: f64,
    /// Working set, bytes.
    rss_bytes: u64,
}

struct RunOutput {
    command: String,
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: Vec<LogLine>,
    stderr: String,
    wall: Duration,
    samples: Vec<Sample>,
}

impl RunOutput {
    fn stdout_text(&self) -> String {
        self.stdout
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn find(&self, needle: &str) -> Option<&LogLine> {
        self.stdout.iter().find(|l| l.text.contains(needle))
    }

    fn find_all<'a>(&'a self, needle: &'a str) -> impl Iterator<Item = &'a LogLine> + 'a {
        self.stdout.iter().filter(move |l| l.text.contains(needle))
    }

    /// The whole raw output, stdout and stderr, for the report.
    fn raw(&self) -> String {
        let mut text = self.stdout_text();
        if !self.stderr.trim().is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(self.stderr.trim_end());
        }
        text
    }
}

// ---------------------------------------------------------------------------------------
// Top level
// ---------------------------------------------------------------------------------------

/// The `verify` subcommand.
pub fn verify_command(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", usage());
        return Ok(());
    }
    let repo = crate::repo_root();
    let opts = parse_options(args, &repo)?;
    // Before anything runs, so a bad `--work-dir` is a plain refusal and not a report.
    vet_sandbox(&opts.work_dir, &protected_dirs(&repo))?;
    let strict = opts.strict;
    let mut session = Session::new(opts, repo);
    session.execute();
    let (path, tally) = session.write_report()?;
    if run_failed(tally, strict) {
        bail!(
            "{}; the report is {}\n{}",
            failure_reason(tally),
            path.display(),
            session.console_summary()
        );
    }
    println!("{}", session.console_summary());
    Ok(())
}

/// How many checks came out each way.
///
/// Counted once, at the end of the run, so the report, the console summary and the exit
/// code cannot disagree about what the run found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tally {
    passed: usize,
    failed: usize,
    not_performed: usize,
}

/// The word the report leads with.
///
/// A run that could not perform some of its checks is not a plain "PASS", and a reader who
/// reads only the verdict must not over-read it: the count of what was *not* judged is
/// therefore part of the verdict, not a footnote under it.
fn verdict(tally: Tally) -> String {
    match (tally.failed, tally.not_performed) {
        (0, 0) => "PASS (on this machine, this run)".to_string(),
        (0, skipped) => {
            format!("PASS with {skipped} check(s) not performed (on this machine, this run)")
        }
        _ => "FAIL".to_string(),
    }
}

/// Whether a finished run should exit non-zero.
///
/// A check that *failed* always fails the run. A check that could not be performed fails it
/// only under `--strict`, and that asymmetry is deliberate: the harness exists to be run on
/// machines that cannot perform half of it — a development host with no GPU encoder, a
/// runner with no desktop — and a run that refuses to produce a report is worse than a
/// report that says plainly what was not judged. `--strict` is for the caller who wants
/// the absence of evidence to be as loud as the evidence of a defect.
fn run_failed(tally: Tally, strict: bool) -> bool {
    tally.failed > 0 || (strict && tally.not_performed > 0)
}

/// The one line that says why the run failed.
///
/// Only called when [`run_failed`] says so, so there are exactly two reasons to be here:
/// a check failed, or a strict run found a check it could not perform.
fn failure_reason(tally: Tally) -> String {
    if tally.failed > 0 {
        format!("{} check(s) failed", tally.failed)
    } else {
        format!(
            "--strict: {} check(s) could not be performed, and a strict run counts that as a \
             failure",
            tally.not_performed
        )
    }
}

impl Session {
    /// The run's own count of what it found.
    fn tally(&self) -> Tally {
        Tally {
            passed: self
                .checks
                .iter()
                .filter(|c| matches!(c.outcome, Outcome::Pass))
                .count(),
            failed: self
                .checks
                .iter()
                .filter(|c| matches!(c.outcome, Outcome::Fail(_)))
                .count(),
            not_performed: self
                .checks
                .iter()
                .filter(|c| matches!(c.outcome, Outcome::NotPerformed(_)))
                .count(),
        }
    }

    fn console_summary(&self) -> String {
        let tally = self.tally();
        format!(
            "verify: {verdict} — {passed} passed, {failed} failed, {skipped} not performed\n\
             report: {report}",
            verdict = verdict(tally),
            passed = tally.passed,
            failed = tally.failed,
            skipped = tally.not_performed,
            report = self.opts.report.display()
        )
    }

    /// Run every check, recording a failed check rather than aborting wherever possible:
    /// a report that stops at the first problem hides the rest.
    fn execute(&mut self) {
        // Everything below writes into the sandbox: a run that could not establish one stops.
        if !self.reset_sandbox() {
            return;
        }
        if let Err(err) = self.environment() {
            self.record_step_error("environment", err);
        }
        if let Err(err) = self.build_cli() {
            self.record_step_error("build", err);
        }
        if let Err(err) = self.probe_encoders() {
            self.record_step_error("encoder_probe", err);
        }
        if let Err(err) = self.buffer_run() {
            self.record_step_error("buffer_run", err);
        }
        if let Err(err) = self.clip_checks() {
            self.record_step_error("clip_probe", err);
        }
        if let Err(err) = self.criterion_seven() {
            self.record_step_error("criterion7_startup_failure", err);
        }
    }

    /// Empty the sandbox before the run, so the eviction evidence means something.
    ///
    /// The engine adopts an existing scratch directory and continues its segment numbering, which
    /// is right for a user's machine and wrong for a measurement: a second run against a previous
    /// directory would report a non-zero lowest segment without any eviction. A failure to remove
    /// the sandbox is a failed check rather than a silently weakened one.
    ///
    /// Only a directory this harness created is emptied (see [`vet_sandbox`]); anything else is a
    /// refusal. Returns whether a clean, marked sandbox now exists: `false` stops the run, because
    /// the steps after this one write into the directory.
    fn reset_sandbox(&mut self) -> bool {
        const WHAT: &str = "the run started from a clean sandbox";
        const CRITERION: &str = "criterion 2 (what makes the eviction evidence real)";
        const EXPECTED: &str = "the work directory is empty before the run";
        let dir = self.opts.work_dir.clone();
        let measured = match vet_sandbox(&dir, &protected_dirs(&self.repo)) {
            Ok(Sandbox::Absent) => format!("{} did not exist yet", dir.display()),
            Ok(Sandbox::Removable(path)) => match std::fs::remove_dir_all(&path) {
                Ok(()) => format!("removed {} first", dir.display()),
                Err(err) => {
                    self.fail(
                        "sandbox",
                        WHAT,
                        CRITERION,
                        EXPECTED,
                        format!("could not remove {}: {err}", dir.display()),
                        "a run that adopts a previous run's index and clips would report against \
                         them, and the eviction and clip checks would be measuring the wrong run",
                    );
                    return false;
                }
            },
            Err(err) => {
                self.fail(
                    "sandbox",
                    WHAT,
                    CRITERION,
                    EXPECTED,
                    format!("refused to delete {}", dir.display()),
                    format!("{err:#}"),
                );
                return false;
            }
        };
        if let Err(err) = mark_sandbox(&dir) {
            self.fail(
                "sandbox",
                WHAT,
                CRITERION,
                EXPECTED,
                format!("could not create {}: {err}", dir.display()),
                "the run writes its data directory there",
            );
            return false;
        }
        self.pass("sandbox", WHAT, CRITERION, EXPECTED, measured);
        true
    }

    /// Toolchain, ffmpeg discovery, and what is being run where.
    fn environment(&mut self) -> Result<()> {
        let mut text = String::new();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut versions = Vec::new();
        for (label, program, args) in [
            ("host", "rustc", vec!["-vV"]),
            ("cargo", cargo.as_str(), vec!["--version"]),
        ] {
            let out = Command::new(program).args(&args).output();
            match out {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                    text.push_str(&format!("$ {program} {}\n{stdout}\n\n", args.join(" ")));
                    versions.push((label, stdout));
                }
                Ok(out) => {
                    text.push_str(&format!(
                        "$ {program} {}\n[failed: {}]\n{}\n\n",
                        args.join(" "),
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim_end()
                    ));
                    versions.push((label, format!("failed: {}", out.status)));
                }
                Err(err) => {
                    text.push_str(&format!(
                        "$ {program} {}\n[not found: {err}]\n\n",
                        args.join(" ")
                    ));
                    versions.push((label, format!("not found: {err}")));
                }
            }
        }
        let toolchain_ok = versions
            .iter()
            .all(|(_, v)| !v.starts_with("failed") && !v.starts_with("not found"));
        self.push(
            "toolchain",
            "Rust toolchain present and reporting a version",
            "prerequisite",
            "`rustc -vV` and `cargo --version` both answer",
            versions
                .iter()
                .map(|(l, v)| format!("{l}: {}", v.lines().next().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("; "),
            if toolchain_ok {
                Outcome::Pass
            } else {
                Outcome::Fail(
                    "a toolchain that cannot report its version cannot have built this harness"
                        .into(),
                )
            },
        );

        match FfmpegBinaries::discover(None) {
            Ok(bin) => {
                let version = Command::new(&bin.ffmpeg)
                    .arg("-version")
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .next()
                            .unwrap_or("(no output)")
                            .to_string()
                    })
                    .unwrap_or_else(|e| format!("(ffmpeg -version failed: {e})"));
                let sidecar_ffmpeg = self.repo.join("binaries").join(exe("ffmpeg"));
                let sidecar_note = if sidecar_ffmpeg.is_file() {
                    format!("sidecar directory present: {}", sidecar_ffmpeg.display())
                } else {
                    format!(
                        "no sidecar at {} — ffmpeg/ffprobe are being taken from PATH (fine for a \
                         development checkout; a packaged build ships them in `binaries/`)",
                        sidecar_ffmpeg.display()
                    )
                };
                text.push_str(&format!(
                    "ffmpeg : {}\nffprobe: {}\n{}\n{sidecar_note}\n",
                    bin.ffmpeg.display(),
                    bin.ffprobe.display(),
                    version
                ));
                self.pass(
                    "ffmpeg",
                    "ffmpeg/ffprobe located, and ffmpeg reports its version",
                    "prerequisite",
                    "the binaries the CLI will use are runnable",
                    format!("ffmpeg={} ({version})", bin.ffmpeg.display()),
                );
                self.bin = Some(bin);
            }
            Err(err) => {
                text.push_str(&format!("ffmpeg discovery failed: {err:#}\n"));
                self.fail(
                    "ffmpeg",
                    "ffmpeg/ffprobe located, and ffmpeg reports its version",
                    "prerequisite",
                    "the binaries the CLI will use are runnable",
                    "not found",
                    format!("{err:#}"),
                );
            }
        }
        self.note("Environment", text);
        Ok(())
    }

    /// Build the CLI under test (`target/release`), unless one was supplied.
    fn build_cli(&mut self) -> Result<()> {
        if let Some(cli) = &self.opts.cli {
            if !cli.is_file() {
                bail!("--cli {} does not exist", cli.display());
            }
            self.cli = Some(cli.clone());
            self.pass(
                "build",
                "the CLI under test exists",
                "prerequisite",
                "an executable replayed-cli",
                cli.display().to_string(),
            );
            return Ok(());
        }

        let target = self.repo.join("target/release").join(exe("replayed-cli"));
        let mut command = "cargo build --release -p replayed-cli".to_string();
        if self.opts.dev_software {
            command.push_str(" --features replayed-cli/test-encoders");
        }
        if self.opts.no_build {
            let exists = target.is_file();
            self.note(
                "Build",
                format!(
                    "--no-build: not building. `{command}` was skipped; the binary under test is \
                     {}\n[{}]",
                    target.display(),
                    if exists { "present" } else { "MISSING" }
                ),
            );
            if !exists {
                bail!(
                    "--no-build was given and {} does not exist",
                    target.display()
                );
            }
            self.cli = Some(target);
            return Ok(());
        }

        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&self.repo)
            .args(["build", "--release", "-p", "replayed-cli"]);
        if self.opts.dev_software {
            cmd.args(["--features", "replayed-cli/test-encoders"]);
        }
        let out = cmd
            .output()
            .with_context(|| format!("running `{command}` (is cargo on PATH?)"))?;
        let stdout = strip_ansi(&String::from_utf8_lossy(&out.stdout));
        let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
        let tail: Vec<&str> = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|l| !l.trim().is_empty())
            .collect();
        let tail = tail[tail.len().saturating_sub(20)..].join("\n");
        self.note(
            "Build",
            format!("$ {command}\n{tail}\n\nbinary: {}", target.display()),
        );
        if !out.status.success() || !target.is_file() {
            self.fail(
                "build",
                "the CLI under test builds and exists",
                "prerequisite",
                "a release replayed-cli",
                format!("exit status {}", out.status),
                format!("`{command}` failed; see the Build section"),
            );
            bail!("building replayed-cli failed");
        }
        self.pass(
            "build",
            "the CLI under test builds and exists",
            "prerequisite",
            "a release replayed-cli",
            target.display().to_string(),
        );
        self.cli = Some(target);
        Ok(())
    }

    /// Which hardware encoders this machine advertises, and which actually work.
    fn probe_encoders(&mut self) -> Result<()> {
        let bin = self.bin.clone().context("no ffmpeg")?;
        let report = probe::probe(&bin)?;
        let works: Vec<&str> = report
            .rows
            .iter()
            .filter(|(_, s)| *s == probe::State::Works)
            .map(|(n, _)| n.as_str())
            .collect();
        self.note(
            "Encoder probe",
            format!(
                "$ cargo xtask probe\n{}\n\n(probe itself: a synthetic 320x240 frame, one per \
                 advertised encoder; it does not capture the screen and does not inject input)",
                report.render()
            ),
        );
        let measured = if works.is_empty() {
            "no hardware encoder encoded a frame".to_string()
        } else {
            format!("WORKS: {}", works.join(", "))
        };
        if self.hardware_path {
            if works.is_empty() {
                self.fail(
                    "encoder_probe",
                    "a hardware encoder this machine can actually run",
                    "criterion 7 / spec §3.2 (no CPU fallback)",
                    "at least one of h264_nvenc / h264_qsv / h264_amf passes its smoke test",
                    &measured,
                    "the capture app will refuse to start on this machine",
                );
            } else {
                self.pass(
                    "encoder_probe",
                    "a hardware encoder this machine can actually run",
                    "criterion 7 / spec §3.2 (no CPU fallback)",
                    "at least one H.264 hardware encoder passes its smoke test",
                    &measured,
                );
            }
        } else {
            self.not_performed(
                "encoder_probe",
                "a hardware encoder this machine can actually run",
                "criterion 7 / spec §3.2 (no CPU fallback)",
                "at least one H.264 hardware encoder passes its smoke test",
                &measured,
                "this run was told to use the software encoder (development host), so vendor \
                 selection is not exercised",
            );
        }
        self.probe = Some(report);
        Ok(())
    }

    /// Start the buffer with the bounded configuration and drive the self-test clip.
    fn buffer_run(&mut self) -> Result<()> {
        let app_data = self.opts.work_dir.join("appdata");
        let config_dir = app_data.join("replayed");
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        std::fs::write(config_dir.join("config.toml"), RUN_CONFIG.to_toml("auto"))
            .context("writing the harness config.toml")?;

        let mut args = vec![
            "buffer".to_string(),
            "--self-test-clip-after".to_string(),
            self.opts.seconds.to_string(),
        ];
        if self.opts.dev_software {
            args.push("--dev-software-encoder".to_string());
        }
        let budget = Duration::from_secs(self.opts.seconds + 120);
        let run = self.run_cli(&app_data, &args, budget)?;

        let context_note = format!(
            "configuration (written by the harness, not the machine's own config):\n\n```toml\n{}\n```\n\n\
             command: `{}`\n\nRUST_LOG=debug; the application data directory was redirected to \
             `{}` so the run could not touch the real `%LOCALAPPDATA%\\replayed` (and so the \
             RAM and eviction numbers below belong to this run alone).",
            RUN_CONFIG.to_toml("auto"),
            run.command,
            app_data.display()
        );
        self.note("Run configuration and command", context_note);
        self.note(
            "Buffer run — raw output",
            format!(
                "exit code: {:?}{}\nwall clock: {:.1}s\n\n```text\n{}\n```",
                run.exit_code,
                if run.timed_out {
                    " (TIMED OUT and killed)"
                } else {
                    ""
                },
                run.wall.as_secs_f64(),
                run.raw()
            ),
        );

        // --- the process itself -------------------------------------------------------
        if let Some(code) = run.exit_code {
            if code == 0 {
                self.pass(
                    "run_exit",
                    "the buffer run exited cleanly",
                    "prerequisite",
                    "exit code 0",
                    "exit code 0",
                );
            } else {
                self.fail(
                    "run_exit",
                    "the buffer run exited cleanly",
                    "prerequisite",
                    "exit code 0",
                    format!("exit code {code}"),
                    "see the run's raw output",
                );
            }
        } else {
            self.fail(
                "run_exit",
                "the buffer run exited cleanly",
                "prerequisite",
                "exit code 0",
                if run.timed_out {
                    "killed after the timeout".to_string()
                } else {
                    "no exit code".to_string()
                },
                "see the run's raw output",
            );
        }

        // --- startup ------------------------------------------------------------------
        let buffering = run.find("buffering ");
        match buffering {
            Some(line) => self.pass(
                "startup",
                "the engine started and reported its buffer configuration",
                "criterion 1",
                "a `buffering …` line",
                line.text.trim().to_string(),
            ),
            None => self.fail(
                "startup",
                "the engine started and reported its buffer configuration",
                "criterion 1",
                "a `buffering …` line",
                "not logged",
                "the engine did not reach the buffering state; see the raw output",
            ),
        }
        let geometry = run.find("capture geometry");
        if self.hardware_path {
            match (geometry, run.find("WGC capture started")) {
                (Some(g), Some(w)) => {
                    self.pass(
                        "capture_started",
                        "the real capture backend started and reported its geometry",
                        "criterion 1",
                        "`WGC capture started on the primary monitor …` and a `capture geometry` line",
                        format!("{} | {}", g.text.trim(), w.text.trim()),
                    );
                    self.note(
                        "Capture geometry",
                        format!("{}\n{}", w.text.trim(), g.text.trim()),
                    );
                }
                _ => self.fail(
                    "capture_started",
                    "the real capture backend started and reported its geometry",
                    "criterion 1",
                    "`WGC capture started on the primary monitor …` and a `capture geometry` line",
                    format!(
                        "geometry: {}, WGC: {}",
                        geometry.map(|l| l.text.trim()).unwrap_or("missing"),
                        run.find("WGC capture started")
                            .map(|l| l.text.trim())
                            .unwrap_or("missing")
                    ),
                    "the Windows capture backend did not start; a stub geometry would mean the \
                     stub ran, which is a defect on Windows",
                ),
            }
        } else {
            self.not_performed(
                "capture_started",
                "the real capture backend started and reported its geometry",
                "criterion 1",
                "`WGC capture started on the primary monitor …`",
                geometry
                    .map(|l| l.text.trim().to_string())
                    .unwrap_or_else(|| "no geometry line".into()),
                "this is not a Windows run: the synthetic stub backend is what captured",
            );
        }

        // --- the status line, the rate, the ratio -------------------------------------
        let statuses: Vec<&LogLine> = run.find_all("frames=").collect();
        let parsed: Vec<(Option<u64>, Status)> = statuses
            .iter()
            .filter_map(|l| parse_status(&l.text).map(|s| (l.at_ms, s)))
            .collect();
        if let Some((_, last)) = parsed.last() {
            let measured = format!(
                "frames={} segments={} bytes={} span={}ms dropped={} dropped_audio={} \
                 skipped={} fps={:.1}/{} configured={}",
                last.frames,
                last.segments,
                last.bytes,
                last.span_ms,
                last.dropped,
                last.dropped_audio,
                last.skipped,
                last.fps,
                last.declared,
                last.configured
            );
            self.note(
                "Status line — last sample",
                format!(
                    "```\n{measured}\n```\n{} samples parsed from `frames=` lines.",
                    parsed.len()
                ),
            );
            if last.frames > 0 && last.segments > 0 {
                self.pass(
                    "frames_flowing",
                    "frames reached the encoder and segments were written",
                    "criterion 1",
                    "frames > 0 and segments > 0",
                    format!("frames={} segments={}", last.frames, last.segments),
                );
            } else {
                self.fail(
                    "frames_flowing",
                    "frames reached the encoder and segments were written",
                    "criterion 1",
                    "frames > 0 and segments > 0",
                    format!("frames={} segments={}", last.frames, last.segments),
                    "nothing was captured",
                );
            }

            // Achieved frame rate against the rate the pipeline declared. The denominator is the
            // *effective* rate (the status line's `fps=`, given to both the pacer and the encoder child);
            // `configured=` carries what `encode.fps` asked for and is checked on its own below.
            let declared_fps = last.declared as f64;
            let floor = declared_fps * 0.9;
            // Steady state = after the first segment landed (encoder spawned and warm). The final sample is
            // taken during shutdown and reads ~0, so the verdict is the median of the steady samples.
            let steady: Vec<&Status> = parsed
                .iter()
                .map(|(_, s)| s)
                .filter(|s| s.segments >= 1)
                .collect();
            let steady_fps = {
                let mut v: Vec<f64> = steady.iter().map(|s| s.fps).collect();
                v.sort_by(|a, b| a.total_cmp(b));
                v.get(v.len() / 2).copied().unwrap_or(last.fps)
            };
            let measured_fps = format!(
                "achieved {:.1} fps against {} declared (median of {} steady-state samples; \
                 tolerance {:.1}); encode.fps asked for {}{}",
                steady_fps,
                steady.len(),
                last.declared,
                floor,
                last.configured,
                if last.configured == last.declared {
                    ""
                } else {
                    " — the startup probe measured less than the configured rate and the \
                     pipeline adapted to it"
                }
            );
            self.hardware_check(
                "frame_rate",
                "the achieved frame rate reaches the rate the pipeline declared",
                "criterion 1",
                format!("fps >= {floor:.1} ({declared_fps} declared × 0.9)"),
                &measured_fps,
                if steady_fps >= floor {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!(
                        "the pipeline did not sustain the {} fps it declared on this machine: \
                         the encoder's queue is dropping frames and media time then runs slower \
                         than the wall clock (this is the backstop for a load that changed after \
                         the startup measurement)",
                        last.declared
                    ))
                },
            );

            // Was the configured rate achievable at all? Adaptation must not hide a shortfall: a machine
            // that cannot hold `encode.fps` at this resolution fails this check, with both numbers, even
            // though the pipeline then runs honestly at the lower rate.
            {
                let reached = last.declared == last.configured;
                let measured = format!(
                    "encode.fps = {} configured; the pipeline declared {}",
                    last.configured, last.declared
                );
                self.hardware_check(
                    "configured_rate_reached",
                    "the configured frame rate was achievable at this resolution",
                    "criterion 1",
                    "the declared rate equals encode.fps",
                    &measured,
                    if reached {
                        Outcome::Pass
                    } else {
                        Outcome::Fail(format!(
                            "this machine cannot hold {} fps at the captured resolution: the \
                             startup probe measured the encode path and the pipeline adapted to \
                             {} fps, which keeps media time on the wall clock but does not meet \
                             criterion 1 as configured (issue #1). Lower encode.output_size or \
                             encode.fps, and record both numbers in the ledger",
                            last.configured, last.declared
                        ))
                    },
                );
            }

            // Dropped frames: the encoder's own count of discarded payloads. Frames lost before segment 1
            // (encoder still starting) are reported but not held against the steady state.
            let startup_dropped = steady.first().map(|s| s.dropped).unwrap_or(last.dropped);
            self.hardware_check(
                "dropped",
                "the encoder dropped no frames",
                "criterion 1",
                "no frames dropped after startup (dropped and dropped_audio stop growing once the first segment lands)",
                format!(
                    "dropped={} (of which {} during encoder startup) dropped_audio={}",
                    last.dropped, startup_dropped, last.dropped_audio
                ),
                if last.dropped == startup_dropped && last.dropped_audio == 0 {
                    Outcome::Pass
                } else {
                    Outcome::Fail(
                        "the encoder could not keep up: the recording has holes in it (the \
                         `the encoder cannot sustain …` warning, if present, says the same)"
                            .to_string(),
                    )
                },
            );

            // The readback skip is only observable when the source offers more frames than the pacer keeps:
            // on a 60 Hz+ display against 30 fps it must be non-zero; on a source paced at the declared
            // rate there is nothing to skip, and demanding a non-zero count would be wrong.
            if let (Some((t0, first)), Some((t1, _))) = (parsed.first(), parsed.last()) {
                let offered_delta =
                    (last.frames + last.skipped).saturating_sub(first.frames + first.skipped);
                let wall_s = match (t0, t1) {
                    (Some(a), Some(b)) if b > a => (b - a) as f64 / 1000.0,
                    _ => run.wall.as_secs_f64(),
                };
                let offered_rate = offered_delta as f64 / wall_s.max(0.001);
                let source_faster = offered_rate > last.declared as f64 * 1.2;
                let measured = format!(
                    "offered to the pacer: {offered_delta} frames over {wall_s:.1}s = \
                     {offered_rate:.1}/s against {} declared ({} configured); skipped={}",
                    last.declared, last.configured, last.skipped
                );
                self.hardware_check(
                    "skipped_readback_skip",
                    "frames the pacer throws away are skipped without the GPU readback",
                    "criterion 1 (commit dd921b3)",
                    "if the source offers > 1.2× the rate the pacer admits, skipped > 0",
                    &measured,
                    if !source_faster || last.skipped > 0 {
                        Outcome::Pass // nothing to skip, or the surplus was skipped cheaply
                    } else {
                        Outcome::Fail(
                            "the source delivered more frames than the pacer admits and \
                             skipped stayed 0: every surplus frame was read back before being \
                             dropped, which is the measured 50.8%-of-a-core cost"
                                .to_string(),
                        )
                    },
                );
            }

            // Media time against the wall clock, from `span=` between two timestamped status lines, not
            // file mtimes.
            //
            // Deliberately a hardware check: the property must hold when the machine cannot keep up, which
            // only a real soak shows. Frame-rate conversion is off (`-fps_mode passthrough`), so frames
            // carry arrival timestamps and the ratio is ~1.0 by construction whatever the delivered rate
            // (the `frame_rate`/`configured_rate_reached` checks cover that separately).
            if let (Some((Some(t0), first)), Some((Some(t1), last))) =
                (parsed.first(), parsed.last())
            {
                let wall_ms = t1.saturating_sub(*t0) as f64;
                let span_ms = last.span_ms.saturating_sub(first.span_ms) as f64;
                if wall_ms >= 5_000.0 {
                    let ratio = span_ms / wall_ms;
                    let measured = format!(
                        "span grew {span_ms:.0}ms over {wall_ms:.0}ms of wall clock \
                         ({ratio:.3}x media-seconds per wall-second), from the timestamps of the \
                         status lines (never from file mtimes); note that a run this short \
                         averages less jitter than the ledger's minutes-long soak"
                    );
                    self.hardware_check(
                        "media_vs_real_time",
                        "media time tracks the wall clock",
                        "criterion 1 / the ledger's 0.81x-vs-1.11x question, settled by the timeline fix",
                        "0.9 <= span-per-second <= 1.1",
                        &measured,
                        if (0.9..=1.1).contains(&ratio) {
                            Outcome::Pass
                        } else {
                            Outcome::Fail(format!(
                                "media ran at {ratio:.3}x of the wall clock: a clip's \
                                 pre_seconds covers {:.1} real seconds per configured second",
                                1.0 / ratio
                            ))
                        },
                    );
                } else {
                    self.not_performed(
                        "media_vs_real_time",
                        "media time tracks the wall clock",
                        "criterion 1 / the ledger's 0.81x-vs-1.11x question, settled by the timeline fix",
                        "0.9 <= span-per-second <= 1.1",
                        "fewer than 5s of timestamped status lines",
                        "the run is too short to average the ratio; raise --seconds",
                    );
                }
            } else {
                self.not_performed(
                    "media_vs_real_time",
                    "media time tracks the wall clock",
                    "criterion 1 / the ledger's 0.81x-vs-1.11x question, settled by the timeline fix",
                    "0.9 <= span-per-second <= 1.1",
                    "status lines carry no parseable timestamps",
                    "the tracing timestamp format could not be parsed",
                );
            }

            // --- the RAM cap and eviction ----------------------------------------------
            //
            // The replay buffer is held in RAM: it writes nothing to the disk while it is only
            // buffering, so there is no scratch directory to measure and no disk cap to enforce.
            // What the engine has is `buffer.ram_cap_bytes`, and what it reports is the RAM the
            // ring holds (`bytes=`) and how many fragments it has dropped to stay inside it
            // (`evicted=`).
            let cap = RUN_CONFIG.ram_cap_bytes;
            let max_bytes = parsed.iter().map(|(_, s)| s.bytes).max().unwrap_or(0);
            let evicted = parsed.iter().map(|(_, s)| s.evicted).max().unwrap_or(0);
            let violated = run.find("over its RAM budget");
            let measured = format!(
                "max bytes= {max_bytes} against a {cap}-byte RAM cap ({} status samples); \
                 `bytes=` is the RAM the ring holds, and the engine fails the run outright if \
                 the ring ever goes over",
                parsed.len()
            );
            if violated.is_none() && max_bytes <= cap {
                self.pass(
                    "ram_cap",
                    "the ring stayed inside its RAM cap for the whole run",
                    "criterion 2",
                    "bytes= never exceeds the configured RAM cap",
                    &measured,
                );
            } else {
                self.fail(
                    "ram_cap",
                    "the ring stayed inside its RAM cap for the whole run",
                    "criterion 2",
                    "bytes= never exceeds the configured RAM cap",
                    &measured,
                    violated
                        .map(|l| l.text.trim().to_string())
                        .unwrap_or_else(|| {
                            "bytes= exceeded the cap while the process kept running".into()
                        }),
                );
            }

            // The ring's own eviction count is the evidence: it counts the fragments dropped off
            // the front to stay inside the cap. There is no segment directory to look at — the
            // replay buffer writes nothing to the disk while it is only buffering.
            let measured = format!(
                "{evicted} fragments evicted oldest-first, a peak of {max_bytes} bytes against a \
                 {cap}-byte RAM cap ({} status samples)",
                parsed.len()
            );
            if evicted > 0 {
                self.pass(
                    "eviction",
                    "the cap evicted oldest-first (the ring dropped fragments off the front)",
                    "criterion 2",
                    "evicted= > 0 once the ring reached its cap",
                    &measured,
                );
            } else if max_bytes >= cap {
                // The cap *was* reached and nothing was evicted: a real defect.
                self.fail(
                    "eviction",
                    "the cap evicted oldest-first (the ring dropped fragments off the front)",
                    "criterion 2",
                    "evicted= > 0 once the ring reached its cap",
                    &measured,
                    "the ring reached its cap but nothing was evicted: the oldest footage is still \
                     held, so the ring can grow without bound",
                );
            } else {
                // The run was too small to fill the ring: the harness's own configuration, not a
                // defect. A PASS here would be a claim about code that never ran.
                self.not_performed(
                    "eviction",
                    "the cap evicted oldest-first (the ring dropped fragments off the front)",
                    "criterion 2",
                    "evicted= > 0 once the ring reached its cap",
                    &measured,
                    format!(
                        "the ring never reached the {cap}-byte cap (max bytes= {max_bytes}), so no \
                         eviction was possible in this run: raise --seconds, or lower the cap. An \
                         unsoaked ring proves nothing about criterion 2"
                    ),
                );
            }

            // --- WASAPI endpoint and the silence detector -----------------------------
            let wasapi = run.stdout.iter().find_map(|l| parse_wasapi(&l.text));
            match wasapi {
                Some(w) => {
                    let measured = format!(
                        "native {}Hz/{} channels/{} -> requested {}Hz/{} channels/{}, converting={}",
                        w.native_sample_rate,
                        w.native_channels,
                        w.native_format,
                        w.requested_sample_rate,
                        w.requested_channels,
                        w.requested_format,
                        w.converting
                    );
                    if w.converting {
                        self.pass(
                            "wasapi_autoconversion",
                            "the WASAPI endpoint was opened through the engine's sample-rate conversion",
                            "criterion 8 / commit 0848fc7",
                            "a non-48 kHz native endpoint with converting=true, and 48 kHz stereo \
                             audio in the clip",
                            &measured,
                        );
                    } else {
                        self.not_performed(
                            "wasapi_autoconversion",
                            "the WASAPI endpoint was opened through the engine's sample-rate conversion",
                            "criterion 8 / commit 0848fc7",
                            "a non-48 kHz native endpoint with converting=true",
                            &measured,
                            "the default render endpoint is natively 48 kHz stereo s16, so there \
                             was nothing to convert. Set the default playback device to 44.1 kHz \
                             or 96 kHz and run again to exercise the conversion",
                        );
                    }
                }
                None => {
                    if self.hardware_path {
                        self.fail(
                            "wasapi_autoconversion",
                            "the WASAPI endpoint was opened through the engine's sample-rate conversion",
                            "criterion 8 / commit 0848fc7",
                            "a `WASAPI loopback capture started …` line naming the native and \
                             requested formats",
                            "no WASAPI line in the output",
                            "the Windows audio backend did not report what endpoint it opened",
                        );
                    } else {
                        self.not_performed(
                            "wasapi_autoconversion",
                            "the WASAPI endpoint was opened through the engine's sample-rate conversion",
                            "criterion 8 / commit 0848fc7",
                            "a `WASAPI loopback capture started …` line naming the native and \
                             requested formats",
                            "no WASAPI line in the output",
                            "this is not a Windows run: there is no WASAPI backend off Windows",
                        );
                    }
                }
            }

            let silence = run.find("captured only silence");
            match silence {
                Some(line) => self.pass(
                    "silence_detector",
                    "the converted-endpoint silence detector fired",
                    "criterion 8 / commit 2a63949",
                    "the warning appears once, with the run's own numbers",
                    line.text.trim().to_string(),
                ),
                None => self.not_performed(
                    "silence_detector",
                    "the converted-endpoint silence detector fired",
                    "criterion 8 / commit 2a63949",
                    "the warning appears once when an engine-converted endpoint delivers a long \
                     unbroken run of silence",
                    "the warning did not appear",
                    "it only fires on the converted-but-silent path: the default playback device \
                     must be non-48 kHz AND nothing audible must have been played. That is a \
                     machine-setup condition, not a defect — it is recorded here so a run that \
                     was set up for it is not forgotten",
                ),
            }
        } else {
            self.fail(
                "frames_flowing",
                "frames reached the encoder and segments were written",
                "criterion 1",
                "frames > 0 and segments > 0",
                "no parseable status line",
                "the engine's status line never appeared; see the raw output",
            );
            self.not_performed(
                "media_vs_real_time",
                "media time tracks the wall clock",
                "criterion 1 / the ledger's 0.81x-vs-1.11x question, settled by the timeline fix",
                "0.9 <= span-per-second <= 1.1",
                "no status line to read span= from",
                "the engine's status line never appeared",
            );
        }

        // --- the process's own cost ----------------------------------------------------
        self.cpu_and_rss(&run);

        self.app_data = Some(app_data);
        self.run = Some(run);
        Ok(())
    }

    /// CPU as a percentage of one core and RSS in MB, over a steady-state window.
    fn cpu_and_rss(&mut self, run: &RunOutput) {
        let note = |_s: &Session| -> String {
            // Samples relative to the trigger, so "steady state" is explicit: the window ends at the
            // trigger, whose post-roll wait is a different workload.
            let mut text = String::from("| t (s) | CPU time (s) | RSS (MB) |\n|---|---|---|\n");
            for s in &run.samples {
                text.push_str(&format!(
                    "| {:.1} | {:.3} | {:.1} |\n",
                    s.t_s,
                    s.cpu_s,
                    s.rss_bytes as f64 / (1024.0 * 1024.0)
                ));
            }
            if run.samples.is_empty() {
                text.push_str("| — | — | — |\n");
            }
            text
        };
        self.note("Resource samples", note(self));

        // The steady-state window: after startup and before the trigger.
        let window_start = (self.opts.seconds as f64 / 4.0).max(5.0);
        let trigger_t = run
            .find("waiting for post-roll")
            .and_then(|l| l.at_ms)
            .zip(run.stdout.first().and_then(|l| l.at_ms))
            .map(|(trigger, first)| (trigger.saturating_sub(first)) as f64 / 1000.0)
            .unwrap_or_else(|| run.wall.as_secs_f64());
        let window: Vec<&Sample> = run
            .samples
            .iter()
            .filter(|s| s.t_s >= window_start && s.t_s <= trigger_t)
            .collect();

        if run.samples.is_empty() {
            let reason = "the harness could not sample the process: the platform's sampler \
                          returned nothing (Windows: GetProcessTimes + GetProcessMemoryInfo; \
                          macOS: proc_pid_rusage, which a sandboxed shell stubs out with a \
                          zeroed buffer; other hosts: `ps`, which a sandboxed shell can deny). \
                          The run itself is unaffected";
            self.not_performed(
                "cpu",
                "steady-state CPU under 5% of one core on a quiet screen",
                "criterion 6",
                "CPU < 5% of ONE core",
                "no process samples were taken",
                reason,
            );
            self.not_performed(
                "rss",
                "steady-state RSS under 400 MB at the harness's 16 MiB ring",
                "criterion 6",
                "RSS < 400 MB at the harness's 16 MiB ring",
                "no process samples were taken",
                reason,
            );
            return;
        }
        if window.len() < 3 {
            self.not_performed(
                "cpu",
                "steady-state CPU under 5% of one core on a quiet screen",
                "criterion 6",
                "CPU < 5% of ONE core",
                format!(
                    "only {} samples fell inside the steady-state window",
                    window.len()
                ),
                "the window is the first quarter of the run up to the trigger and needs at \
                 least three 2s samples: raise --seconds",
            );
            self.not_performed(
                "rss",
                "steady-state RSS under 400 MB at the harness's 16 MiB ring",
                "criterion 6",
                "RSS < 400 MB at the harness's 16 MiB ring",
                format!(
                    "only {} samples fell inside the steady-state window",
                    window.len()
                ),
                "the window is the first quarter of the run up to the trigger and needs at \
                 least three 2s samples: raise --seconds",
            );
            return;
        }
        let first = window
            .first()
            .expect("the window has at least three samples");
        let last = window
            .last()
            .expect("the window has at least three samples");
        let dt = last.t_s - first.t_s;
        let cpu_percent = if dt > 0.0 {
            (last.cpu_s - first.cpu_s) / dt * 100.0
        } else {
            f64::NAN
        };
        let rss_bytes = window.iter().map(|s| s.rss_bytes).max().unwrap_or(0);
        let rss_mb = rss_bytes as f64 / (1024.0 * 1024.0);
        let window_desc = format!(
            "{:.1}s … {:.1}s ({:.1}s window), sampled every {:.0}s",
            first.t_s,
            last.t_s,
            dt,
            SAMPLE_INTERVAL.as_secs_f64()
        );

        // What the display was doing while that was measured. The cost the pipeline is being
        // judged on is per frame it is *offered*, and Windows Graphics Capture offers a frame
        // whenever the screen changes: a still desktop offers a few per second, a playing video or
        // an animating page offers the display's full rate. The same code therefore reads several
        // times higher on a busy screen, and a number without this context cannot be compared
        // with a run that had a quiet one (the ledger's 3.1–4.0% was measured on a screen offering
        // almost nothing; the box's 2026-10-24 runs offered ~58/s with Chrome animating and read
        // 5.2–7.5%). The target is unchanged — this only says what it was measured against.
        let offered = offered_frame_rate(run);
        let context = match offered {
            Some((rate, skipped)) => format!(
                "; the display offered {rate:.1} frames/s during the run (pacer skipped \
                 {skipped}), so the cost is per offered frame, not per admitted one"
            ),
            None => String::new(),
        };

        let cost_per_fps = offered
            .filter(|(rate, _)| *rate > 0.0)
            .map(|(rate, _)| {
                format!(
                    "; {:.3}% of a core per offered frame/s, and the old fitted envelope \
                     (informational, fitted to the runs on record, never a bound) would read \
                     {:.2}%",
                    cpu_percent / rate,
                    cpu_envelope_percent(rate)
                )
            })
            .unwrap_or_default();
        let outcome = match cpu_verdict(offered.map(|(rate, _)| rate)) {
            CpuVerdict::Judged if cpu_percent <= CPU_TARGET_PERCENT => Outcome::Pass,
            CpuVerdict::Judged => Outcome::Fail(format!(
                "the pipeline costs {cpu_percent:.2}% of a core at steady state on a quiet \
                 screen, against a target of {CPU_TARGET_PERCENT:.1}%{context} (issue #1 \
                 measured 50.8% at 4K; a regression is what this check is for)"
            )),
            CpuVerdict::BusyScreen { offered } => Outcome::NotPerformed(format!(
                "the display offered {offered:.1} frames/s, above the {QUIET_SCREEN_MAX_OFFERED_FPS:.0} \
                 a quiet desktop offers, and the pipeline's cost is per frame the display \
                 presents (issue #93). A bound fitted to a busy run would only describe that \
                 run, so none is applied: close the animating windows and re-run to judge the \
                 {CPU_TARGET_PERCENT:.0}% target"
            )),
            CpuVerdict::UnknownScreen => Outcome::NotPerformed(
                "the run's status lines gave no offered frame rate, so it is not known whether \
                 the screen was quiet enough for the target to apply"
                    .to_string(),
            ),
        };
        self.hardware_check(
            "cpu",
            "steady-state CPU under 5% of one core on a quiet screen",
            "criterion 6",
            format!(
                "CPU <= {CPU_TARGET_PERCENT:.0}% of ONE core, judged only when the display \
                 offered <= {QUIET_SCREEN_MAX_OFFERED_FPS:.0} frames/s (a quiet desktop). The \
                 cost of this pipeline is **per frame the display presents**: Windows Graphics \
                 Capture delivers a frame when the screen changes, so a still desktop offers a \
                 few a second and a 4K60 animating one offers ~60, and the same code reads \
                 several times higher on the second (issue #93). The target is flat and the \
                 scenario is what is conditioned, because a bound fitted to the data it judges \
                 cannot fail on it. Measured under this harness's bounded config \
                 ({fps} fps, {kbps} kbps), not the shipping 60 fps / 20000 kbps. Measured on the \
                 replayed-cli process itself — the same one the runbook's `Get-Process \
                 replayed-cli` reads, so the ffmpeg encoder child's own CPU is not counted; \
                 Task Manager's per-process column is normalised across cores and would read \
                 5/N%",
                fps = RUN_CONFIG.fps,
                kbps = RUN_CONFIG.bitrate_kbps,
            ),
            format!("{cpu_percent:.2}% of one core over {window_desc}{context}{cost_per_fps}"),
            outcome,
        );
        self.hardware_check(
            "rss",
            "steady-state RSS under 400 MB at the harness's 16 MiB ring",
            "criterion 6",
            "RSS < 400 MB (working set), measured with the ring capped at this harness's \
             16 MiB (`ram_cap_bytes`). It says nothing about the shipping config, whose ring \
             may grow to 256 MiB: the working set there is the baseline this run measures \
             plus up to that much footage, and no run here fills it",
            format!("{rss_mb:.1} MB peak over {window_desc}"),
            if rss_mb < 400.0 {
                Outcome::Pass
            } else {
                Outcome::Fail("working set exceeded 400 MB at steady state".to_string())
            },
        );
    }

    /// Probe the clip the run produced: duration, streams, codec, drift, audio level.
    fn clip_checks(&mut self) -> Result<()> {
        let bin = self.bin.clone().context("no ffmpeg")?;
        // Taken out of `self` rather than borrowed, so checks can be recorded while the run's data is
        // still being read. Nothing after this step needs either value.
        let run = self.run.take().context("no buffer run")?;
        let app_data = self
            .app_data
            .take()
            .context("no application data directory")?;
        let clips_dir = app_data.join("replayed").join("clips");

        let Some(clip) = newest_clip(&clips_dir)? else {
            self.fail(
                "clip_written",
                "a clip was written by the self-test trigger",
                "criterion 3",
                "one clip-*.mp4 in the clips directory",
                format!("no clip-*.mp4 under {}", clips_dir.display()),
                "the trigger produced no file",
            );
            return Ok(());
        };

        // The trigger line and the `wrote` line. Owned values, not references into `run`, so checks can
        // be recorded while reading them.
        let trigger: Option<(u64, Trigger)> = run
            .stdout
            .iter()
            .find_map(|l| l.at_ms.zip(parse_trigger(&l.text)));
        let trigger_at_ms = trigger.map(|(at_ms, _)| at_ms);
        let wrote: Option<(Option<u64>, Wrote)> = run
            .stdout
            .iter()
            .find_map(|l| parse_wrote(&l.text).map(|w| (l.at_ms, w)));
        let self_test_line = run
            .find("self-test: the ring holds")
            .map(|l| l.text.trim().to_string());

        if let Some((at_ms, trigger_line)) = trigger {
            self.note(
                "Trigger instant",
                format!(
                    "triggered at media={}ms wall={}ms, drift {}ms (wall minus media), at {} UTC.
\
                     The same numbers a `Ctrl+F8` press produces: this is the recorder's own 
\
                     media-time trigger path, reached by the self-test instead of a keypress.",
                    trigger_line.media_ms,
                    trigger_line.wall_ms,
                    trigger_line.drift_ms,
                    rfc3339_utc(at_ms)
                ),
            );
        }

        match (&wrote, trigger_at_ms) {
            (Some((at_ms, wrote)), Some(trigger_ms)) => {
                let delta_ms = at_ms.map(|w| w.saturating_sub(trigger_ms));
                self.pass(
                    "clip_written",
                    "the self-test trigger wrote a clip",
                    "criterion 3",
                    "a `wrote …` line naming the file, and the file on disk",
                    format!(
                        "{} ({}ms, {} bytes, encoder={}){}",
                        wrote.path,
                        wrote.duration_ms,
                        wrote.size_bytes,
                        wrote.encoder,
                        self_test_line
                            .as_deref()
                            .map(|l| format!("; self-test line: {l}"))
                            .unwrap_or_default()
                    ),
                );
                let budget_ms = RUN_CONFIG.post_seconds * 1000 + 2000;
                match delta_ms {
                    Some(delta) if delta < budget_ms => self.pass(
                        "clip_trigger_to_write",
                        "the clip was written within the post-roll budget",
                        "criterion 3 (budget = post_seconds + 2 s)",
                        format!("< {budget_ms}ms from the trigger line to the `wrote` line"),
                        format!("{delta}ms"),
                    ),
                    Some(delta) => self.fail(
                        "clip_trigger_to_write",
                        "the clip was written within the post-roll budget",
                        "criterion 3 (budget = post_seconds + 2 s)",
                        format!("< {budget_ms}ms from the trigger line to the `wrote` line"),
                        format!("{delta}ms"),
                        "the trigger→write path overran its budget; note this measures the \
                         self-test trigger's own line, which is the instant the hotkey's press \
                         would have been handled",
                    ),
                    None => self.not_performed(
                        "clip_trigger_to_write",
                        "the clip was written within the post-roll budget",
                        "criterion 3 (budget = post_seconds + 2 s)",
                        format!("< {budget_ms}ms"),
                        "one of the two lines carries no parseable timestamp",
                        "the tracing timestamps could not be read",
                    ),
                }
            }
            _ => self.fail(
                "clip_written",
                "the self-test trigger wrote a clip",
                "criterion 3",
                "a `wrote …` line and the file on disk",
                format!(
                    "clip found at {} but no complete trigger/wrote pair in the log",
                    clip.display()
                ),
                "see the run's raw output",
            ),
        }

        // --- ffprobe ------------------------------------------------------------------
        let info = MediaInfo::probe(&bin, &clip).context("ffprobe on the clip")?;
        let stream_counts = ffprobe_stream_counts(&bin, &clip)?;
        let ffprobe_json =
            ffprobe_json(&bin, &clip).unwrap_or_else(|e| format!("(ffprobe failed: {e:#})"));
        let levels = volumedetect(&bin, &clip)?;
        let frame = frame_stats(&bin, &clip)?;

        let mut probe_text = format!(
            "$ ffprobe -v error -show_streams -show_format -of json {}\n{ffprobe_json}\n",
            clip.display()
        );
        probe_text.push_str(&format!(
            "\n$ ffmpeg -i {} -af volumedetect -f null -\nmean_volume={} dB, max_volume={} dB (digital silence is about -91 dB)\n",
            clip.display(),
            levels.as_ref().map(|l| format!("{:.1}", l.mean_db)).unwrap_or_else(|| "?".into()),
            levels.as_ref().map(|l| format!("{:.1}", l.max_db)).unwrap_or_else(|| "?".into()),
        ));
        if let Some(f) = &frame {
            probe_text.push_str(&format!(
                "\nfirst decoded video frame (scaled to 160x90, rgb24): {} distinct colours, \
                 R/G/B means {:.1}/{:.1}/{:.1}\n",
                f.colours, f.mean_r, f.mean_g, f.mean_b
            ));
        }
        self.note("Clip probe", probe_text);

        // Duration against the configured window.
        let window = RUN_CONFIG.window_ms();
        let delta = (info.duration_ms as i64 - window as i64).abs();
        let tolerance = 500 + 2 * RUN_CONFIG.segment_time as i64 * 1000;
        if delta <= tolerance {
            self.pass(
                "clip_duration",
                "the clip is the configured pre+post window",
                "criterion 4",
                format!("{window}ms ± {tolerance}ms (500ms + one {}s segment at each end: the lossless cut snaps outward to segment boundaries)", RUN_CONFIG.segment_time),
                format!("{}ms (off by {delta}ms)", info.duration_ms),
            );
        } else {
            self.fail(
                "clip_duration",
                "the clip is the configured pre+post window",
                "criterion 4",
                format!("{window}ms ± {tolerance}ms"),
                format!("{}ms (off by {delta}ms)", info.duration_ms),
                "one side of the window is missing, or the cut did not land where it should",
            );
        }

        // Exactly one video and one audio stream, with the expected codecs.
        let videos = stream_counts
            .iter()
            .filter(|t| t.as_str() == "video")
            .count();
        let audios = stream_counts
            .iter()
            .filter(|t| t.as_str() == "audio")
            .count();
        let video_codec = info
            .video
            .as_ref()
            .map(|v| v.codec.clone())
            .unwrap_or_else(|| "none".into());
        let resolution = info
            .video
            .as_ref()
            .map(|v| format!("{}x{}", v.width, v.height))
            .unwrap_or_else(|| "?".into());
        let audio_desc = info
            .audio
            .as_ref()
            .map(|a| format!("{} {}Hz {}ch", a.codec, a.sample_rate, a.channels))
            .unwrap_or_else(|| "none".into());
        let measured = format!(
            "video: {video_codec} {resolution}; audio: {audio_desc}; streams: {}",
            stream_counts.join(", ")
        );
        if videos == 1 && audios == 1 {
            self.pass(
                "clip_streams",
                "the clip carries exactly one video and one audio stream",
                "criterion 8",
                "1 video + 1 audio, video h264/hevc per config, audio aac 48 kHz stereo",
                &measured,
            );
        } else {
            self.fail(
                "clip_streams",
                "the clip carries exactly one video and one audio stream",
                "criterion 8",
                "1 video + 1 audio",
                &measured,
                "the container is not the two-stream clip criterion 8 requires",
            );
        }

        // Stream copy: the clip's codec family is the encoder's, and the splice was quick.
        let expected_family = if video_codec.starts_with("hevc") {
            "hevc"
        } else {
            "h264"
        };
        match &wrote {
            Some((at_ms, wrote)) => {
                // The encoder's *family*, not its name: `h264_nvenc` and `libx264` are both H.264, and ffprobe
                // reports the codec, never the implementation. Matching on the name would fail every
                // software-encoder run and a correct `-c copy` splice.
                let encoder_family = encoder_family(&wrote.encoder);
                let splice_ms = at_ms.zip(trigger_at_ms).map(|(w, t)| w.saturating_sub(t));
                let measured = format!(
                    "clip video codec={video_codec}, logged encoder={}, extraction took {}{}",
                    wrote.encoder,
                    splice_ms
                        .map(|m| format!("{m}ms"))
                        .unwrap_or_else(|| "?".into()),
                    splice_ms
                        .map(|_| format!(" against a clip of {}ms of media", info.duration_ms))
                        .unwrap_or_default()
                );
                if expected_family == encoder_family {
                    let fast = splice_ms.is_some_and(|m| m < info.duration_ms.max(500) / 2);
                    if fast {
                        self.pass(
                            "clip_stream_copy",
                            "the clip is a lossless splice, not a re-encode",
                            "criterion 5",
                            "clip codec family = the encoder that produced the segments, and the \
                             splice far faster than the clip's own duration",
                            &measured,
                        );
                    } else {
                        self.not_performed(
                            "clip_stream_copy",
                            "the clip is a lossless splice, not a re-encode",
                            "criterion 5",
                            "clip codec family = the encoder that produced the segments, and the \
                             splice far faster than the clip's own duration",
                            &measured,
                            "the codec matches (the strong evidence) but the extraction took long \
                             enough that the speed evidence is inconclusive on this run",
                        );
                    }
                } else {
                    self.fail(
                        "clip_stream_copy",
                        "the clip is a lossless splice, not a re-encode",
                        "criterion 5",
                        "clip codec family = the encoder that produced the segments",
                        &measured,
                        "the clip's codec does not match the encoder the run logged",
                    );
                }
            }
            None => self.not_performed(
                "clip_stream_copy",
                "the clip is a lossless splice, not a re-encode",
                "criterion 5",
                "clip codec family = the encoder that produced the segments",
                format!("clip video codec={video_codec}"),
                "no `wrote … encoder=` line to compare against",
            ),
        }

        // The hardware-encoder half of the codec check (shipping path only).
        if let Some((_, wrote)) = &wrote {
            let is_hardware = ["nvenc", "qsv", "amf"]
                .iter()
                .any(|v| wrote.encoder.ends_with(v));
            self.hardware_check(
                "clip_hardware_encoder",
                "the clip was produced by a hardware encoder (no CPU fallback)",
                "criterion 5 / spec §3.2",
                "the logged encoder is one of h264_nvenc / h264_qsv / h264_amf (or the hevc forms)",
                format!("encoder={}", wrote.encoder),
                if is_hardware {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!(
                        "the shipping path encoded with {} — a silent CPU fallback violates the \
                         project's non-negotiable principle",
                        wrote.encoder
                    ))
                },
            );
        }

        // The drift line the splice logs, and the same quantity read back by ffprobe.
        let drift_line = run.stdout.iter().find_map(|l| parse_drift_line(&l.text));
        let probed_drift = info.av_drift();
        let drift_context = probed_drift
            .map(|d| {
                format!(
                    "ffprobe: video {}ms, audio {}ms, drift {}ms",
                    d.video_ms, d.audio_ms, d.delta_ms
                )
            })
            .unwrap_or_else(|| "ffprobe reported no per-stream durations".to_string());
        match drift_line {
            Some((video_ms, audio_ms, drift_ms)) => {
                let measured = format!(
                    "log: video {video_ms}ms audio {audio_ms}ms drift {drift_ms}ms; {drift_context}"
                );
                if drift_ms.abs() <= 100 {
                    self.pass(
                        "clip_av_drift",
                        "the clip's two streams line up (A/V drift)",
                        "criterion 8",
                        "|drift| <= 100ms, and not growing with clip length",
                        &measured,
                    );
                } else {
                    self.fail(
                        "clip_av_drift",
                        "the clip's two streams line up (A/V drift)",
                        "criterion 8",
                        "|drift| <= 100ms",
                        &measured,
                        "a multi-hundred-ms offset is the two live sources desyncing, not muxing \
                         quantisation",
                    );
                }
                self.note(
                    "A/V drift",
                    "The splice's own line, plus the same quantity re-derived by ffprobe.\n\n```\n\
                     log:    video {video_ms}ms audio {audio_ms}ms drift {drift_ms}ms\n\
                     ffprobe: {drift_context}\n```\n\n\
                     This measures drift *within the produced clip*. It is not a measurement of \
                     the live WGC-clock vs WASAPI-clock divergence, which is not instrumented in \
                     this build (see the runbook's Known gaps).",
                );
            }
            None => self.not_performed(
                "clip_av_drift",
                "the clip's two streams line up (A/V drift)",
                "criterion 8",
                "|drift| <= 100ms",
                drift_context,
                "the splice logged no drift line to read (see the splice's own note in the run \
                 output)",
            ),
        }

        // Audio level, proving the track is not silent.
        match &levels {
            Some(l) => {
                let measured = format!(
                    "mean_volume={:.1} dB, max_volume={:.1} dB",
                    l.mean_db, l.max_db
                );
                self.note("Audio level", format!("{measured}\n\nThe report's volumedetect output above carries ffmpeg's own text."));
                if self.hardware_path {
                    if l.max_db > -80.0 {
                        self.pass(
                            "clip_audio_not_silent",
                            "the clip's audio is not silent",
                            "criterion 8",
                            "max_volume well above digital silence (~-91 dB)",
                            &measured,
                        );
                    } else {
                        self.fail(
                            "clip_audio_not_silent",
                            "the clip's audio is not silent",
                            "criterion 8",
                            "max_volume well above digital silence (~-91 dB)",
                            &measured,
                            "WASAPI loopback captured nothing: either the endpoint delivered \
                             silence or the engine-side conversion produced it (the silence \
                             detector's warning above says which case it suspects)",
                        );
                    }
                } else {
                    self.not_performed(
                        "clip_audio_not_silent",
                        "the clip's audio is not silent",
                        "criterion 8",
                        "max_volume well above digital silence (~-91 dB)",
                        &measured,
                        "this run's audio came from the synthetic stub, which emits digital \
                         silence by design — it proves the audio stream is muxed, not that real \
                         loopback audio has content",
                    );
                }
            }
            None => self.not_performed(
                "clip_audio_not_silent",
                "the clip's audio is not silent",
                "criterion 8",
                "max_volume well above digital silence (~-91 dB)",
                "volumedetect produced no numbers",
                "ffmpeg's volumedetect output could not be read",
            ),
        }

        // One decoded frame: not a single-colour (black) frame.
        match &frame {
            Some(f) => {
                let f = *f;
                let measured = format!(
                    "{} distinct colours, R/G/B means {:.1}/{:.1}/{:.1} (a uniform frame would be 1)",
                    f.colours, f.mean_r, f.mean_g, f.mean_b
                );
                if f.colours > 1 {
                    self.pass(
                        "clip_not_black",
                        "the clip's video is not a single-colour frame",
                        "the ledger's black-frame regression (WGC copy_out)",
                        "a decoded frame carries more than one colour",
                        &measured,
                    );
                } else {
                    self.fail(
                        "clip_not_black",
                        "the clip's video is not a single-colour frame",
                        "the ledger's black-frame regression (WGC copy_out)",
                        "a decoded frame carries more than one colour",
                        &measured,
                        "the frame is uniform: either the capture produced no content (the \
                         defect class the ledger records) or the screen really was one colour at \
                         that instant — play the clip and look",
                    );
                }
            }
            None => self.not_performed(
                "clip_not_black",
                "the clip's video is not a single-colour frame",
                "the ledger's black-frame regression (WGC copy_out)",
                "a decoded frame carries more than one colour",
                "no frame could be decoded",
                "the frame-content check could not run",
            ),
        }
        Ok(())
    }

    /// An unusable vendor must fail *at startup*, naming the encoder.
    fn criterion_seven(&mut self) -> Result<()> {
        let report = self.probe.as_ref().context("no encoder probe")?;
        let Some((vendor, encoder)) = report.unusable_h264_vendor() else {
            self.not_performed(
                "criterion7_startup_failure",
                "an unusable encoder vendor exits non-zero, naming the encoder and why",
                "criterion 7",
                "exit != 0 with the encoder id and ffmpeg's reason, before any capture",
                "every vendor this machine advertises passed its smoke test",
                "there is no unusable vendor to point the config at on this machine. Force one \
                 by running on a box (or with a build of ffmpeg) that lacks a vendor runtime",
            );
            return Ok(());
        };

        let app_data = self.opts.work_dir.join("appdata-bad-vendor");
        let config_dir = app_data.join("replayed");
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        std::fs::write(config_dir.join("config.toml"), RUN_CONFIG.to_toml(vendor))
            .context("writing the criterion-7 config.toml")?;

        let args = vec!["buffer".to_string()];
        let run = self.run_cli(&app_data, &args, Duration::from_secs(120))?;
        let output = run.raw();
        self.note(
            format!("Criterion 7 — forced vendor \"{vendor}\" ({encoder})"),
            format!(
                "`vendor = \"{vendor}\"` in the config; this run must fail at startup, before \
                 any capture.\n\ncommand: `{}`\nexit code: {:?}\n\n```text\n{}\n```",
                run.command, run.exit_code, output
            ),
        );

        let named = output.contains(encoder);
        let reason = output.contains("not advertised by this ffmpeg build")
            || output.contains("could not encode a single frame");
        let non_zero = run.exit_code.is_some_and(|c| c != 0);
        let no_capture = !output.contains("buffering ") && !output.contains("encoding with ");

        let measured = format!(
            "exit code {:?}; names `{encoder}`: {named}; repeats the reason: {reason}; started \
             capturing: {}",
            run.exit_code, !no_capture
        );
        if non_zero && named && reason && no_capture {
            self.pass(
                "criterion7_startup_failure",
                "an unusable encoder vendor exits non-zero, naming the encoder and why",
                "criterion 7",
                "exit != 0, the error names the encoder id and ffmpeg's reason, and nothing is \
                 captured",
                &measured,
            );
        } else {
            let why = if !non_zero {
                "the process did not exit non-zero — that is a silent CPU fallback or a hang, \
                 both defects"
            } else if !named {
                "the error does not name the encoder that was tried"
            } else if !reason {
                "the error does not repeat why the encoder failed"
            } else {
                "the capture started before the encoder was refused: this criterion is met by \
                 refusing to start at all"
            };
            self.fail(
                "criterion7_startup_failure",
                "an unusable encoder vendor exits non-zero, naming the encoder and why",
                "criterion 7",
                "exit != 0, the error names the encoder id and ffmpeg's reason, and nothing is \
                 captured",
                &measured,
                why,
            );
        }
        Ok(())
    }

    /// Spawn the CLI, read every line it writes, sample its resource use, and wait.
    fn run_cli(
        &self,
        app_data_base: &Path,
        args: &[String],
        budget: Duration,
    ) -> Result<RunOutput> {
        let cli = self.cli.clone().context("no CLI binary to run")?;
        let command = format!(
            "{} {}",
            cli.display(),
            args.iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        );
        std::fs::create_dir_all(app_data_base)
            .with_context(|| format!("creating {}", app_data_base.display()))?;

        let mut cmd = Command::new(&cli);
        cmd.args(args)
            .current_dir(&self.opts.work_dir)
            .env("RUST_LOG", "debug")
            // The application data directory is redirected into the sandbox, so this run cannot touch the
            // machine's real one.
            .env(data_dir_env(), app_data_base)
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("spawning {command}"))?;

        let rx = drain_pipes(&mut child);
        let started = Instant::now();
        let mut stdout = Vec::new();
        let mut stderr = String::new();
        let mut samples = Vec::new();
        let mut exit_code = None;
        let mut timed_out = false;
        let mut next_sample = started + SAMPLE_INTERVAL;

        loop {
            while let Ok((stream, text)) = rx.recv_timeout(Duration::from_millis(20)) {
                match stream {
                    Stream::Stdout => stdout.push(LogLine {
                        at_ms: parse_epoch_ms(&text),
                        text,
                    }),
                    Stream::Stderr => {
                        stderr.push_str(&text);
                        stderr.push('\n');
                    }
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_code = status.code();
                    break;
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = child.kill();
                    return Err(err).context("waiting for the CLI");
                }
            }
            let now = Instant::now();
            if now >= next_sample {
                if let Some((cpu_s, rss_bytes)) = sample_process(&child) {
                    samples.push(Sample {
                        t_s: now.duration_since(started).as_secs_f64(),
                        cpu_s,
                        rss_bytes,
                    });
                }
                next_sample = now + SAMPLE_INTERVAL;
            }
            if now.duration_since(started) > budget {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
        }
        // The readers stop when the pipes close; take whatever is still buffered.
        while let Ok((stream, text)) = rx.recv_timeout(Duration::from_millis(500)) {
            match stream {
                Stream::Stdout => stdout.push(LogLine {
                    at_ms: parse_epoch_ms(&text),
                    text,
                }),
                Stream::Stderr => {
                    stderr.push_str(&text);
                    stderr.push('\n');
                }
            }
        }

        Ok(RunOutput {
            command,
            exit_code,
            timed_out,
            stdout,
            stderr: strip_ansi(&stderr),
            wall: started.elapsed(),
            samples,
        })
    }

    /// Write the report, and hand back what the run found.
    fn write_report(&mut self) -> Result<(PathBuf, Tally)> {
        let tally = self.tally();
        let (passed, failed, not_performed) = (tally.passed, tally.failed, tally.not_performed);
        let verdict = verdict(tally);

        let mut md = String::new();
        md.push_str("# replayed verification report\n\n");
        md.push_str(&format!(
            "- **Generated**: {} UTC\n- **By**: `cargo xtask verify` (see `xtask/src/verify.rs`)\n\
             - **Verdict**: **{verdict}** — {passed} passed, {failed} failed, {not_performed} not performed\n\
             - **Host**: {} {}\n- **Repository**: {} at {}\n\
             - **Windows-path checks judged?**: {}\n- **Strict?**: {}\n",
            rfc3339_utc(now_ms()),
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.repo.display(),
            git_rev(&self.repo).unwrap_or_else(|| "(git revision unavailable)".into()),
            if self.hardware_path {
                "yes — this is a Windows run of the shipping capture/encoder path"
            } else {
                "no — the synthetic stub capture and/or the development software encoder ran; \
                 checks that need the real path are reported but not judged"
            },
            if self.opts.strict {
                "yes — a check that could not be performed counts as a failure, so this run \
                 could only have passed with every check performed"
            } else {
                "no — a check that could not be performed is reported, not failed; the count \
                 above says how many there were"
            }
        ));
        md.push_str(
            "\n> The clip in this report was taken by the CLI's own `--self-test-clip-after` \
             trigger, which calls the same media-time path the `Ctrl+F8` hotkey calls. **No \
             keyboard or mouse input was synthesised, sent or injected, and no window was \
             enumerated** — see the run output below for the line that says so.\n",
        );

        md.push_str("\n## Checks\n\n");
        md.push_str(
            "| # | Check | Criterion | Expected | Measured | Result |\n|---|---|---|---|---|---|\n",
        );
        for (i, c) in self.checks.iter().enumerate() {
            let result = match &c.outcome {
                Outcome::Pass => "PASS".to_string(),
                Outcome::Fail(why) => format!("**FAIL** — {}", escape_table(why)),
                Outcome::NotPerformed(why) => format!("_not performed_ — {}", escape_table(why)),
            };
            md.push_str(&format!(
                "| {} | {} (`{}`) | {} | {} | {} | {} |\n",
                i + 1,
                escape_table(c.what),
                c.id,
                escape_table(c.criterion),
                escape_table(&c.expected),
                escape_table(&c.measured),
                result
            ));
        }

        md.push_str(
            "\n## What this harness does not cover\n\n\
             Stated up front, because a green column is easy to over-read:\n\n\
             - **The desktop GUI.** No window is opened; the shell's recording wiring \
             (`ce81aef`, `bf87be0`) is not exercised. That needs the app launched by a human.\n\
             - **A real keypress.** The hotkey listener is installed, but the harness never \
             drives it: the clip is taken through the same engine call a press reaches. That \
             the global hotkey itself fires is still a manual step (press it and watch for the \
             same `hotkey pressed:` line).\n\
             - **Anything needing a real game.** The event sources are switched off in the \
             harness config and nothing is contacted; the Phase 4 integrations still need a bot \
             game and a CS2 round.\n\
             - **The live-source clock divergence** between the WGC video clock and the WASAPI \
             audio clock: not instrumented in this build, so no run can measure it here.\n\
             - **Frame content beyond one frame.** The harness decodes one frame and refuses a \
             single-colour one, but it cannot tell you the pixels were the desktop you expected.\n\
             - **Checks marked _not performed_ above.** Each names its reason; they are not \
             silent.\n\n\
             What the harness *does* establish is in the table: the encoder probe, the run's own \
             rate and timeline numbers, the RAM cap and eviction, the clip the trigger \
             produced (duration, streams, codec, drift, audio level, one decoded frame), and the \
             criterion-7 startup failure.\n",
        );

        for (title, body) in &self.sections {
            md.push_str(&format!("\n## {title}\n\n{body}\n"));
        }

        // The exact invocation, so the report can be regenerated rather than believed.
        let mut invocation = format!("cargo xtask verify --seconds {}", self.opts.seconds);
        if self.opts.dev_software {
            invocation.push_str(" --dev-software-encoder");
        }
        if self.opts.no_build {
            invocation.push_str(" --no-build");
        }
        if self.opts.strict {
            invocation.push_str(" --strict");
        }
        md.push_str(&format!(
            "\n## How to reproduce\n\n```console\n$ {invocation}\n```\n"
        ));
        md.push_str(&format!(
            "\nRun it in this checkout's root, in an **interactive desktop session** (an SSH \
             session has no desktop, which the capture backend needs — \
             `scripts/verify-in-interactive-session.ps1` exists for exactly that). The full \
             command lines and the config the run used are in the sections above; the sandbox \
             this run wrote into is `{}`{}.\n",
            self.opts.work_dir.display(),
            if self.opts.clean_work {
                " (removed afterwards: `--clean-work`)"
            } else {
                ""
            }
        ));

        let report = self.opts.report.clone();
        if let Some(parent) = report.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&report, md).with_context(|| format!("writing {}", report.display()))?;

        if self.opts.clean_work {
            // The same rule as the start of the run: only a directory this harness marked.
            let removal = vet_sandbox(&self.opts.work_dir, &protected_dirs(&self.repo))
                .map_err(|err| format!("{err:#}"))
                .and_then(|sandbox| match sandbox {
                    Sandbox::Absent => Ok(()),
                    Sandbox::Removable(path) => {
                        std::fs::remove_dir_all(path).map_err(|err| err.to_string())
                    }
                });
            match removal {
                Ok(()) => self.note(
                    "Cleanup",
                    format!("removed {}", self.opts.work_dir.display()),
                ),
                Err(err) => eprintln!(
                    "xtask: could not remove {}: {err}",
                    self.opts.work_dir.display()
                ),
            }
        }
        Ok((report, tally))
    }
}

// ---------------------------------------------------------------------------------------
// Parsers (pure, unit-tested below)
// ---------------------------------------------------------------------------------------

/// `key=value` in a tracing line, where the key is a whole token and the value runs to the
/// next whitespace. Returns the value without any trailing `ms`.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let mut from = 0;
    while let Some(pos) = line[from..].find(&needle) {
        let abs = from + pos;
        let boundary = abs == 0
            || line[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == ';');
        if boundary {
            let rest = &line[abs + needle.len()..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let value = &rest[..end];
            return Some(value.strip_suffix("ms").unwrap_or(value));
        }
        from = abs + needle.len();
    }
    None
}

/// The engine's periodic status line.
///
/// `declared` is the `fps=` denominator: the rate the pipeline runs at (the pacer's rate and
/// the encoder child's `-framerate`). `configured` is the `configured=` field, what
/// `encode.fps` asked for, falling back to `declared` when absent.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Status {
    frames: u64,
    segments: u64,
    bytes: u64,
    span_ms: u64,
    /// Fragments the ring dropped off the front to stay inside its RAM cap.
    evicted: u64,
    dropped: u64,
    dropped_audio: u64,
    skipped: u64,
    fps: f64,
    declared: u32,
    configured: u32,
}

/// The CPU target: 5% of one core, the acceptance criterion's own figure from before any
/// measurement on the box (the ledger's criterion 6). A flat number, judged only on a quiet
/// screen (see [`cpu_verdict`]).
const CPU_TARGET_PERCENT: f64 = 5.0;

/// The most frames a second the display may offer for a run to count as the quiet-desktop
/// scenario the 5% target was set for.
///
/// This defines the *scenario*, not the pass mark: it is a quarter of a 60 Hz display's rate,
/// which is what "a handful of frames a second" (the ledger's §15 quiet screen) means in a
/// number. It was chosen without looking at any CPU figure, so no measurement can move it.
const QUIET_SCREEN_MAX_OFFERED_FPS: f64 = 15.0;

/// What the CPU figure may be judged against, given what the display offered the pacer.
#[derive(Debug, PartialEq)]
enum CpuVerdict {
    /// A quiet screen: the flat [`CPU_TARGET_PERCENT`] applies.
    Judged,
    /// A busy screen: the cost is per frame the display presents, and the only thing the
    /// harness could fit to such a run is the run itself.
    BusyScreen { offered: f64 },
    /// The run's status lines gave no frame rate, so whether the screen was quiet is unknown.
    UnknownScreen,
}

/// Decide whether a CPU figure can be judged at all.
///
/// **History, because the shape of this check was wrong once.** Issue #93: four runs on the
/// 4K60 box read 5.2-7.5% against the flat 5% target, because Windows Graphics Capture delivers a
/// frame when the screen *changes* and that desktop offered ~58 frames/s, where §15's 3.1-4.0%
/// was a quiet screen. The first answer was an envelope, `3.5% + 0.08% x offered fps`, fitted
/// "with margin" to the five measurements on record. That could not fail on the data it was fitted
/// to (6.55% against 8.18% at 58.5/s), allowed 8.3% at 60/s where the target was 5%, and so judged
/// nothing a regression would not also have to be 60% worse to trip.
///
/// A bound that is chosen after seeing the measurements it judges is a description of them. This
/// check therefore keeps the target fixed, **and conditions the verdict on the scenario instead**:
/// the 5% applies to a quiet screen, where the figure it was set for was measured, and a run on
/// a busy screen reports its numbers (and the cost per offered frame) without a verdict.
/// [`cpu_envelope_percent`] survives only as an informational figure printed beside them.
///
/// The cost of that honesty: on a desktop that animates, this check is "not performed", and
/// `--strict` fails the run. Quiet the screen (close the animating windows) to judge it.
fn cpu_verdict(offered_fps: Option<f64>) -> CpuVerdict {
    match offered_fps {
        None => CpuVerdict::UnknownScreen,
        Some(rate) if rate > QUIET_SCREEN_MAX_OFFERED_FPS => {
            CpuVerdict::BusyScreen { offered: rate }
        }
        Some(_) => CpuVerdict::Judged,
    }
}

/// The old fitted envelope, `3.5% + 0.08% x offered fps`: **informational only, never a bound**.
///
/// Fitted to five measurements (the ledger's §15 quiet 3.1-4.0%, §48's four busy runs at 58-60/s)
/// and not derived from anything, so it says "this run looks like the runs on record", which is
/// useful context and not a pass mark. It is printed so a reader can see how the figure compares.
fn cpu_envelope_percent(offered_fps: f64) -> f64 {
    3.5 + 0.08 * offered_fps.max(0.0)
}

/// The rate the display offered the pacer over the run, and the count it skipped.
///
/// Computed from the same `frames=`/`skipped=` status lines the rate checks read: offered is
/// `frames + skipped` between the first and last sample, over the wall time between them. `None`
/// when the run has fewer than two samples to difference (the caller then says nothing rather
/// than naming a rate it does not have).
///
/// It exists because the CPU criterion is a cost *per offered frame*: the same pipeline reads
/// several times higher on a screen that presents 60 frames a second than on one that presents
/// three, and a figure without that context is not comparable with a run that had a quieter
/// screen.
fn offered_frame_rate(run: &RunOutput) -> Option<(f64, u64)> {
    let parsed: Vec<(Option<u64>, Status)> = run
        .find_all("frames=")
        .filter_map(|line| parse_status(&line.text).map(|status| (line.at_ms, status)))
        .collect();
    let (first_t, first) = parsed.first()?;
    let (last_t, last) = parsed.last()?;
    let offered = (last.frames + last.skipped).saturating_sub(first.frames + first.skipped);
    let wall_s = match (first_t, last_t) {
        (Some(a), Some(b)) if b > a => (b - a) as f64 / 1000.0,
        _ => return None,
    };
    (wall_s > 0.0).then(|| (offered as f64 / wall_s, last.skipped))
}

fn parse_status(line: &str) -> Option<Status> {
    let fps_field = field(line, "fps")?;
    let (achieved, declared) = fps_field.split_once('/')?;
    let declared: u32 = declared.parse().ok()?;
    Some(Status {
        frames: field(line, "frames")?.parse().ok()?,
        segments: field(line, "segments")?.parse().ok()?,
        bytes: field(line, "bytes")?.parse().ok()?,
        span_ms: field(line, "span")?.parse().ok()?,
        // Optional: a status line from a build that does not report eviction is not a parse
        // failure, it is a run that cannot say whether anything was evicted.
        evicted: field(line, "evicted")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        dropped: field(line, "dropped")?.parse().ok()?,
        dropped_audio: field(line, "dropped_audio")?.parse().ok()?,
        skipped: field(line, "skipped")?.parse().ok()?,
        fps: achieved.parse().ok()?,
        declared,
        configured: field(line, "configured")
            .and_then(|c| c.parse().ok())
            .unwrap_or(declared),
    })
}

/// The trigger line: `hotkey pressed: media=44000ms wall=44123ms (drift 123ms); …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Trigger {
    media_ms: u64,
    wall_ms: u64,
    drift_ms: i64,
}

fn parse_trigger(line: &str) -> Option<Trigger> {
    if !line.contains("waiting for post-roll") {
        return None;
    }
    let drift = line.split("(drift ").nth(1)?.split(')').next()?.trim();
    Some(Trigger {
        media_ms: field(line, "media")?.parse().ok()?,
        wall_ms: field(line, "wall")?.parse().ok()?,
        drift_ms: drift.strip_suffix("ms").unwrap_or(drift).parse().ok()?,
    })
}

/// The `wrote …` line: `wrote <path> (<ms>ms, <bytes> bytes, encoder=<name>)`.
struct Wrote {
    path: String,
    duration_ms: u64,
    size_bytes: u64,
    encoder: String,
}

fn parse_wrote(line: &str) -> Option<Wrote> {
    let after = line.split("wrote ").nth(1)?;
    let (path, rest) = after.split_once(" (")?;
    let stats = rest.split(')').next()?;
    let mut parts = stats.split(',');
    let duration_ms = parts.next()?.trim().strip_suffix("ms")?.parse().ok()?;
    let size_bytes = parts.next()?.trim().strip_suffix(" bytes")?.parse().ok()?;
    let encoder = parts.next()?.trim().strip_prefix("encoder=")?.to_string();
    Some(Wrote {
        path: path.trim().to_string(),
        duration_ms,
        size_bytes,
        encoder,
    })
}

/// The splice's drift line: `clip clip-1234: video 3007ms audio 3008ms drift -1ms`.
///
/// These are space-separated words, not `key=value` fields, so [`field`] does not apply.
fn parse_drift_line(line: &str) -> Option<(u64, u64, i64)> {
    if !line.contains("clip ") || !line.contains("drift ") {
        return None;
    }
    let value = |key: &str| -> Option<&str> {
        let rest = line.split(&format!("{key} ")).nth(1)?;
        let word = rest.split_whitespace().next()?;
        Some(word.strip_suffix("ms").unwrap_or(word))
    };
    Some((
        value("video")?.parse().ok()?,
        value("audio")?.parse().ok()?,
        value("drift")?.parse().ok()?,
    ))
}

/// The WASAPI backend's startup line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WasapiInfo {
    native_sample_rate: u32,
    native_channels: u16,
    native_format: String,
    requested_sample_rate: u32,
    requested_channels: u16,
    requested_format: String,
    converting: bool,
}

fn parse_wasapi(line: &str) -> Option<WasapiInfo> {
    if !line.contains("WASAPI loopback capture started") {
        return None;
    }
    Some(WasapiInfo {
        native_sample_rate: field(line, "native_sample_rate")?.parse().ok()?,
        native_channels: field(line, "native_channels")?.parse().ok()?,
        native_format: field(line, "native_sample_format")?.to_string(),
        requested_sample_rate: field(line, "requested_sample_rate")?.parse().ok()?,
        requested_channels: field(line, "requested_channels")?.parse().ok()?,
        requested_format: field(line, "requested_sample_format")?
            .trim_matches('"')
            .to_string(),
        converting: field(line, "converting")?.starts_with("true"),
    })
}

/// The epoch milliseconds of a leading tracing timestamp (`2026-09-23T19:20:27.157909Z`).
fn parse_epoch_ms(text: &str) -> Option<u64> {
    let text = text.trim_start();
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[10] != b'T' || bytes[19] != b'.' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: u32 = text.get(5..7)?.parse().ok()?;
    let day: u32 = text.get(8..10)?.parse().ok()?;
    let hour: u64 = text.get(11..13)?.parse().ok()?;
    let minute: u64 = text.get(14..16)?.parse().ok()?;
    let second: u64 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + (hour * 3600 + minute * 60 + second) as i64;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The current wall-clock time, in ms since the epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// An RFC3339 UTC timestamp, for the report header.
fn rfc3339_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Remove ANSI escape sequences, so a piped run's log is still parseable.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip `ESC [ … m` (the only kind tracing's formatter emits).
            if chars.clone().next() == Some('[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// A program name with the platform's executable suffix.
fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The environment variable the CLI reads its application data *base* from.
fn data_dir_env() -> &'static str {
    if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "XDG_DATA_HOME"
    }
}

fn shell_quote(arg: &str) -> String {
    if arg.contains(' ') {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

/// Escape a value for a Markdown table cell.
fn escape_table(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The `clip-*.mp4` files under `dir`, newest name last (the trigger writes one).
fn newest_clip(dir: &Path) -> Result<Option<PathBuf>> {
    let mut clips: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("clip-") && n.ends_with(".mp4"))
        })
        .collect();
    clips.sort();
    Ok(clips.pop())
}

/// The numbers of the `seg-NNNNNN.mp4` files on disk.
/// The codec family an encoder id belongs to, as ffprobe would report it.
///
/// ffprobe prints `h264`, never `h264_nvenc` or `libx264`, so the codec check compares
/// families; this maps hardware and software ids alike.
fn encoder_family(encoder: &str) -> &str {
    if encoder.contains("264") {
        "h264"
    } else if encoder.contains("265") || encoder.contains("hevc") {
        "hevc"
    } else {
        encoder
    }
}

/// `git rev-parse --short HEAD`, best effort.
fn git_rev(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------------------
// Reading the child process
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// One thread per pipe, so a full pipe buffer can never stall the child while the parent
/// is sampling it.
fn drain_pipes(child: &mut Child) -> Receiver<(Stream, String)> {
    let (tx, rx) = mpsc::channel();
    for (stream, pipe) in [
        (
            Stream::Stdout,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            Stream::Stderr,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                match line {
                    Ok(line) => {
                        if tx.send((stream, line)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    rx
}

/// The process's cumulative CPU time and working set.
///
/// Windows: `GetProcessTimes` + `GetProcessMemoryInfo` on the child's handle. Everywhere else
/// (the development host): `proc_pid_rusage` on macOS, `ps -o time=,rss=` otherwise.
#[cfg(windows)]
fn sample_process(child: &Child) -> Option<(f64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    // `cb` is the size of the struct, and the API checks it: set it in the initializer rather
    // than by assignment, so the value and the allocation cannot drift apart.
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: the handle comes from a live `Child` this function is called on, and both
    // callees only write into the structs passed by pointer.
    let (cpu_s, rss_bytes) = unsafe {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return None;
        }
        let to_secs = |ft: &FILETIME| {
            let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
            ticks as f64 / 10_000_000.0 // 100-ns units
        };
        let cpu = to_secs(&kernel) + to_secs(&user);
        let queried = K32GetProcessMemoryInfo(handle, &mut counters, counters.cb) != 0;
        (cpu, queried.then_some(counters.WorkingSetSize as u64))
    };
    // A failed query is "could not measure", not "0 bytes": see `measured_working_set`.
    Some((cpu_s, measured_working_set(rss_bytes)?))
}

/// A working set that was actually measured: `None` for a failed query and for a zero.
///
/// A live process always has a resident set, so zero (or a query that returned an error) means
/// the read failed. Reporting it as `0.0 MB` made `RSS < 400 MB` a vacuous PASS for every
/// sample, where the macOS sampler has always refused with `None` ("a fabricated measurement"),
/// which the caller turns into a *not performed* row. One rule for both, tested here on any host.
#[cfg_attr(not(windows), allow(dead_code))]
fn measured_working_set(queried: Option<u64>) -> Option<u64> {
    queried.filter(|&bytes| bytes > 0)
}

#[cfg(target_os = "macos")]
fn sample_process(child: &Child) -> Option<(f64, u64)> {
    // `proc_pid_rusage` rather than `ps`: no subprocess (a sandboxed shell denies `ps`) and exactly
    // the two numbers wanted. `ri_user_time` + `ri_system_time` are nanoseconds of CPU;
    // `ri_resident_size` is the resident set in bytes, macOS's analogue of the working set.
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: `info` is a live, fully owned `rusage_info_v2`; `proc_pid_rusage` fills it
    // in for a pid that is our own live child. The cast is the signature libc declares
    // (`*mut rusage_info_t` is a `*mut c_void` out-parameter).
    unsafe {
        let mut buffer: libc::rusage_info_t = info.as_mut_ptr().cast();
        let rc =
            libc::proc_pid_rusage(child.id() as libc::c_int, libc::RUSAGE_INFO_V2, &mut buffer);
        if rc != 0 {
            return None;
        }
        let info = info.assume_init();
        // A live process has a non-zero resident set, so zero here means the call did not fill the
        // buffer: in a sandboxed shell `proc_pid_rusage` answers `rc = 0` and leaves the struct as it
        // was. An honest `None` ("could not measure") is the only correct answer; reporting
        // `0.0 MB / 0.00% of a core` would be a fabricated measurement.
        if info.ri_resident_size == 0 {
            return None;
        }
        let cpu_s = (info.ri_user_time + info.ri_system_time) as f64 / 1_000_000_000.0;
        Some((cpu_s, info.ri_resident_size))
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn sample_process(child: &Child) -> Option<(f64, u64)> {
    let out = Command::new("ps")
        .args(["-o", "time=", "-o", "rss=", "-p", &child.id().to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let cpu_s = parse_ps_time(parts.next()?)?;
    let rss_kb: u64 = parts.next()?.parse().ok()?;
    Some((cpu_s, rss_kb * 1024))
}

/// `ps`'s `time=` column: `[[dd-]hh:]mm:ss[.ss]`.
///
/// Compiled on every platform so its unit test runs wherever the harness is developed.
#[allow(dead_code)]
fn parse_ps_time(text: &str) -> Option<f64> {
    let (days, rest) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let seconds = match parts.as_slice() {
        [h, m, s] => {
            h.parse::<f64>().ok()? * 3600.0
                + m.parse::<f64>().ok()? * 60.0
                + s.parse::<f64>().ok()?
        }
        [m, s] => m.parse::<f64>().ok()? * 60.0 + s.parse::<f64>().ok()?,
        [s] => s.parse::<f64>().ok()?,
        _ => return None,
    };
    Some(days * 86_400.0 + seconds)
}

// ---------------------------------------------------------------------------------------
// ffprobe / ffmpeg on the clip
// ---------------------------------------------------------------------------------------

/// The clip's stream types, in ffprobe's own order.
fn ffprobe_stream_counts(bin: &FfmpegBinaries, clip: &Path) -> Result<Vec<String>> {
    let out = Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(clip)
        .output()
        .with_context(|| format!("running {} on {}", bin.ffprobe.display(), clip.display()))?;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn ffprobe_json(bin: &FfmpegBinaries, clip: &Path) -> Result<String> {
    let out = Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(clip)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        bail!("ffprobe failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// ffmpeg's own volume numbers for the clip's audio.
#[derive(Debug, Clone, Copy)]
struct Levels {
    mean_db: f64,
    max_db: f64,
}

fn volumedetect(bin: &FfmpegBinaries, clip: &Path) -> Result<Option<Levels>> {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(clip)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .context("running ffmpeg volumedetect")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut mean = None;
    let mut max = None;
    for line in stderr.lines() {
        if let Some(rest) = line.split("mean_volume:").nth(1) {
            mean = rest.split("dB").next().and_then(|v| v.trim().parse().ok());
        }
        if let Some(rest) = line.split("max_volume:").nth(1) {
            max = rest.split("dB").next().and_then(|v| v.trim().parse().ok());
        }
    }
    Ok(match (mean, max) {
        (Some(mean_db), Some(max_db)) => Some(Levels { mean_db, max_db }),
        _ => None,
    })
}

/// One decoded frame's colour statistics — the black-frame check.
#[derive(Debug, Clone, Copy)]
struct FrameStats {
    colours: usize,
    mean_r: f64,
    mean_g: f64,
    mean_b: f64,
}

fn frame_stats(bin: &FfmpegBinaries, clip: &Path) -> Result<Option<FrameStats>> {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(clip)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=160:90",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .context("decoding one frame")?;
    if !out.status.success() || out.stdout.is_empty() {
        return Ok(None);
    }
    let mut colours = std::collections::HashSet::new();
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    // `as_chunks` is newer than this workspace's oldest supported toolchain.
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    for chunk in out.stdout.chunks_exact(3) {
        colours.insert((chunk[0], chunk[1], chunk[2]));
        r += chunk[0] as u64;
        g += chunk[1] as u64;
        b += chunk[2] as u64;
        n += 1;
    }
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(FrameStats {
        colours: colours.len(),
        mean_r: r as f64 / n as f64,
        mean_g: g as f64 / n as f64,
        mean_b: b as f64 / n as f64,
    }))
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_is_parsed_field_by_field() {
        let line = "2026-09-23T19:20:27.157909Z DEBUG replayed_recorder: frames=1814 \
                    segments=57 bytes=37795772 span=57000ms evicted=3 dropped=1183 \
                    dropped_audio=12 skipped=2048 fps=24.1/24 configured=30";
        let status = parse_status(line).expect("the engine's own line must parse");
        assert_eq!(
            status,
            Status {
                frames: 1814,
                segments: 57,
                bytes: 37795772,
                span_ms: 57000,
                evicted: 3,
                dropped: 1183,
                dropped_audio: 12,
                skipped: 2048,
                fps: 24.1,
                declared: 24,
                configured: 30,
            }
        );
        // A line without `configured=` still parses: the declared rate is the fallback.
        let older = parse_status(
            "frames=1 segments=2 bytes=3 span=4ms evicted=0 \
                                  dropped=0 dropped_audio=0 skipped=0 fps=29.9/30",
        )
        .expect("an older status line still parses");
        assert_eq!((older.declared, older.configured), (30, 30));
        // A line that is missing a field is not a status line, not a half-filled one.
        assert!(parse_status("frames=1 segments=2").is_none());
    }

    #[test]
    fn the_cpu_target_is_flat_and_the_scenario_is_what_is_conditioned() {
        // Issue #93's runs: §15's quiet screen and §48's four busy ones. The verdict depends on the
        // screen, never on the CPU figure, so no measurement can move it.
        assert_eq!(cpu_verdict(Some(4.0)), CpuVerdict::Judged);
        assert_eq!(
            cpu_verdict(Some(QUIET_SCREEN_MAX_OFFERED_FPS)),
            CpuVerdict::Judged
        );
        for busy in [58.0, 58.2, 59.4, 60.0] {
            assert_eq!(
                cpu_verdict(Some(busy)),
                CpuVerdict::BusyScreen { offered: busy },
                "{busy}/s is the animating 4K desktop the target was never set for"
            );
        }
        // No frame data: whether the screen was quiet is unknown, and the old fallback to a flat
        // 5% pretended it was.
        assert_eq!(cpu_verdict(None), CpuVerdict::UnknownScreen);
        assert_eq!(CPU_TARGET_PERCENT, 5.0, "the target is the original one");
    }

    #[test]
    fn the_old_envelope_is_informational_and_no_longer_a_bound() {
        // It is still computed, for context beside the numbers: 6.55% against 8.18% at 58.5/s is
        // the run it was fitted to, which is the reason it cannot be a bound.
        assert!((cpu_envelope_percent(58.5) - 8.18).abs() < 0.01);
        assert_eq!(cpu_envelope_percent(-3.0), 3.5);
        // And a busy run that would have passed the envelope gets no verdict from it.
        assert!(matches!(
            cpu_verdict(Some(58.5)),
            CpuVerdict::BusyScreen { .. }
        ));
    }

    #[test]
    fn the_offered_rate_is_frames_plus_skips_over_the_wall_time() {
        // The two samples the box's run really produced (frames admitted, skips closed), a second
        // apart in `at_ms` terms and 2s apart in wall time: 40 admitted + 76 skipped over 2s.
        let run = RunOutput {
            stdout: vec![
                LogLine {
                    at_ms: Some(1_000),
                    text: "frames=10 segments=1 bytes=0 span=1000ms evicted=0 dropped=0 \
                           dropped_audio=0 skipped=30 fps=10.0/30 configured=30"
                        .to_string(),
                },
                LogLine {
                    at_ms: Some(3_000),
                    text: "frames=50 segments=1 bytes=0 span=5000ms evicted=0 dropped=0 \
                           dropped_audio=0 skipped=106 fps=25.0/30 configured=30"
                        .to_string(),
                },
            ],
            command: "replayed-cli buffer".to_string(),
            exit_code: Some(0),
            timed_out: false,
            stderr: String::new(),
            wall: Duration::from_secs(120),
            samples: Vec::new(),
        };
        let (rate, skipped) = offered_frame_rate(&run).expect("two samples are enough");
        assert!((rate - 58.0).abs() < 0.001, "offered {rate}/s");
        assert_eq!(skipped, 106);
        // One sample cannot be differenced, and guessing would be worse than saying nothing.
        let mut single = run;
        single.stdout.truncate(1);
        assert_eq!(offered_frame_rate(&single), None);
    }

    #[test]
    fn a_field_is_only_matched_at_a_token_boundary() {
        // `dropped=` must not match inside `dropped_audio=`.
        let line = "frames=1 dropped_audio=7 dropped=3";
        assert_eq!(field(line, "dropped"), Some("3"));
        assert_eq!(field(line, "dropped_audio"), Some("7"));
        assert_eq!(field(line, "span"), None);
        // The `ms` suffix is stripped from values that carry it.
        assert_eq!(field("span=57000ms dropped=0", "span"), Some("57000"));
    }

    #[test]
    fn the_trigger_and_wrote_lines_parse_into_their_numbers() {
        let trigger = parse_trigger(
            "2026-09-23T19:21:00.000000Z  INFO replayed_recorder: hotkey pressed: \
             media=44000ms wall=44123ms (drift 123ms); waiting for post-roll",
        )
        .expect("the hotkey's own line");
        assert_eq!(
            (trigger.media_ms, trigger.wall_ms, trigger.drift_ms),
            (44000, 44123, 123)
        );

        let wrote = parse_wrote(
            "2026-09-23T19:21:03.000000Z  INFO replayed_recorder: wrote \
             /tmp/appdata/replayed/clips/clip-1758650463.mp4 (13091ms, 8529609 bytes, \
             encoder=h264_nvenc)",
        )
        .expect("the recorder's own line");
        assert_eq!(wrote.duration_ms, 13091);
        assert_eq!(wrote.size_bytes, 8529609);
        assert_eq!(wrote.encoder, "h264_nvenc");
        assert!(wrote.path.ends_with("clip-1758650463.mp4"));

        // A line that is not one of these must not parse as one.
        assert!(parse_trigger("wrote something (1ms, 2 bytes, encoder=x)").is_none());
    }

    #[test]
    fn the_wasapi_line_carries_the_conversion_pair() {
        let line = "2026-09-23T19:20:00.000000Z  INFO replayed_capture::wasapi: \
                    native_sample_rate=44100 native_channels=2 native_sample_format=F32 \
                    requested_sample_rate=48000 requested_channels=2 \
                    requested_sample_format=\"s16\" converting=true WASAPI loopback capture \
                    started on the default render endpoint";
        let info = parse_wasapi(line).expect("the wasapi line");
        assert_eq!(info.native_sample_rate, 44100);
        assert_eq!(info.requested_sample_rate, 48000);
        assert!(info.converting);
        assert_eq!(info.requested_format, "s16");
        assert!(parse_wasapi("WGC capture started on the primary monitor").is_none());
    }

    #[test]
    fn the_splice_drift_line_parses_with_a_negative_delta() {
        let (video, audio, drift) = parse_drift_line(
            "2026-09-23T19:21:04.000000Z  INFO replayed_replay::splice: clip clip-1758650463: \
             video 13091ms audio 13104ms drift -13ms",
        )
        .expect("the splice's own line");
        assert_eq!((video, audio, drift), (13091, 13104, -13));
        assert!(parse_drift_line("frames=1 segments=2").is_none());
    }

    #[test]
    fn timestamps_round_trip_through_civil_dates() {
        // 2026-09-23T19:20:27.157909Z
        let ms = parse_epoch_ms("2026-09-23T19:20:27.157909Z  INFO whatever").expect("a timestamp");
        assert_eq!(rfc3339_utc(ms), "2026-09-23T19:20:27Z");
        // A leap day, and the epoch itself.
        assert_eq!(
            rfc3339_utc(parse_epoch_ms("2024-02-29T00:00:00.000000Z x").expect("a leap day")),
            "2024-02-29T00:00:00Z"
        );
        assert_eq!(parse_epoch_ms("1970-01-01T00:00:00.000000Z x"), Some(0));
        // Not a timestamp: the `Error: …` line anyhow prints on stderr.
        assert_eq!(parse_epoch_ms("Error: no usable hardware encoder"), None);
    }

    #[test]
    fn ansi_escapes_do_not_survive_a_piped_log() {
        // tracing's fmt layer colours the level even when stdout is a pipe, which would make every
        // parser miss its field.
        let coloured = "\u{1b}[2m2026-09-23T19:20:27.157909Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
                        \u{1b}[2mxtask\u{1b}[0m\u{1b}[2m:\u{1b}[0m frames=10 span=1000ms";
        let clean = strip_ansi(coloured);
        assert!(!clean.contains('\u{1b}'));
        assert_eq!(field(&clean, "frames"), Some("10"));
        assert_eq!(
            parse_epoch_ms(&clean),
            parse_epoch_ms("2026-09-23T19:20:27.157909Z x")
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_macos_sampler_never_reports_a_zero_working_set() {
        // The macOS sampler, checked against a child that is definitely alive and has memory.
        //
        // The invariant is `None` or a real number, never a plausible zero: in a sandboxed shell
        // `proc_pid_rusage` answers `rc = 0` and leaves the buffer untouched, and reporting that as
        // "0.0 MB" would be a fabricated measurement, so the harness marks the check not-performed.
        let mut child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning sleep");
        if let Some((cpu_s, rss_bytes)) = sample_process(&child) {
            assert!(
                rss_bytes > 0,
                "a live process always has a resident set; 0 means the read failed"
            );
            assert!(cpu_s >= 0.0, "CPU time cannot be negative: {cpu_s}");
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The engine's real status format string, with every placeholder filled by a number.
    ///
    /// The harness reads the recorder's periodic status line by field name, and the line is
    /// produced by one `tracing::debug!` format string in `crates/recorder/src/lib.rs`. Nothing
    /// shared ties the two, so a renamed or reordered field left every unit test green (they use
    /// hand-written fixtures) and was found only by a live run on Windows. This reads the producer's
    /// source, lifts that format string out and fills it in, so the fixture *is* the engine's
    /// format. Brittle on purpose: if the producer's macro is reshaped so it cannot be found, the
    /// test fails and says to update the extraction, which is the moment to look at the parser too.
    fn engines_status_line() -> String {
        const ENGINE: &str = include_str!("../../crates/recorder/src/lib.rs");
        let start = ENGINE
            .find("\"frames={} segments={}")
            .expect("the engine's status format string moved: update this extraction");
        let literal = &ENGINE[start + 1..];
        let end = literal
            .find("\",\n")
            .expect("the engine's status format string has no closing quote");
        // Rust's `\<newline><indent>` continuation joins the pieces with no whitespace between.
        let mut joined = String::new();
        let mut lines = literal[..end].split('\n');
        joined.push_str(lines.next().unwrap_or_default().trim_end_matches('\\'));
        for line in lines {
            joined.push_str(line.trim_start().trim_end_matches('\\'));
        }
        joined.replace("{:.1}", "1.5").replace("{}", "7")
    }

    #[test]
    fn the_harness_parses_the_status_line_the_engine_actually_prints() {
        let line = engines_status_line();
        let status = parse_status(&line)
            .unwrap_or_else(|| panic!("the harness cannot parse the engine's status line: {line}"));
        // Every field the harness judges on must be present as a token in the producer, not
        // defaulted by the parser's optional-field fallbacks (`evicted`, `configured`).
        for key in ["evicted", "configured", "skipped", "span", "dropped_audio"] {
            assert!(
                field(&line, key).is_some(),
                "the engine's status line has no `{key}=`: {line}"
            );
        }
        assert_eq!(
            status.configured, 7,
            "`configured=` was read, not defaulted"
        );
        assert_eq!(status.evicted, 7, "`evicted=` was read, not defaulted");
        assert_eq!(status.skipped, 7);
    }

    #[test]
    fn a_failed_or_zero_working_set_is_not_a_measurement() {
        // The Windows sampler used to substitute 0 when `K32GetProcessMemoryInfo` failed, so
        // every sample read 0.0 MB and `RSS < 400 MB` passed without measuring anything.
        assert_eq!(measured_working_set(None), None);
        assert_eq!(measured_working_set(Some(0)), None);
        assert_eq!(measured_working_set(Some(48 << 20)), Some(48 << 20));
    }

    #[test]
    fn the_ps_time_column_is_parsed() {
        assert_eq!(parse_ps_time("0:03.45"), Some(3.45));
        assert_eq!(parse_ps_time("01:03.00"), Some(63.0));
        assert_eq!(parse_ps_time("2-01:00:00"), Some(176_400.0));
        assert_eq!(parse_ps_time("garbage"), None);
    }

    #[test]
    fn options_default_to_the_repository_target_directory() {
        let repo = Path::new("/repo");
        let opts = parse_options(&[], repo).expect("no arguments are all defaults");
        assert_eq!(opts.report, repo.join("target/verify/report.md"));
        assert_eq!(opts.work_dir, repo.join("target/verify/work"));
        assert_eq!(opts.seconds, DEFAULT_SECONDS);
        assert!(!opts.dev_software && !opts.no_build && !opts.clean_work);
        assert!(!opts.strict);

        let opts = parse_options(
            &[
                "--seconds",
                "90",
                "--report",
                "/tmp/r.md",
                "--dev-software-encoder",
                "--clean-work",
                "--strict",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
            repo,
        )
        .expect("the full flag list parses");
        assert_eq!(opts.seconds, 90);
        assert_eq!(opts.report, PathBuf::from("/tmp/r.md"));
        assert!(opts.dev_software && opts.clean_work);
        assert!(opts.strict);
    }

    /// A repository checkout and a home directory side by side in one scratch tree, as
    /// `(scratch, repo, home, protected)`.
    fn sandbox_world() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        Vec<(&'static str, PathBuf)>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("home/me/src/replayed");
        let home = tmp.path().join("home/me");
        std::fs::create_dir_all(&repo).unwrap();
        let protected = vec![
            ("this repository's checkout", repo.clone()),
            ("your home directory", home.clone()),
        ];
        (tmp, repo, home, protected)
    }

    fn refusal(dir: &Path, protected: &[(&'static str, PathBuf)]) -> String {
        vet_sandbox(dir, protected)
            .expect_err("this directory must not be deleted")
            .to_string()
    }

    #[test]
    fn the_home_directory_and_everything_above_it_are_never_a_sandbox() {
        // `--work-dir ~` was a recursive delete of the home directory, reported as a passing
        // check. The home directory is refused even when it is empty of anything but a marker,
        // and so is a directory that merely contains it.
        let (tmp, _repo, home, protected) = sandbox_world();
        // Only the home entry, so the label under test is the one that fires (the checkout
        // lives inside the home directory here, as it usually does).
        let protected = &protected[1..];
        std::fs::write(
            home.join(SANDBOX_MARKER),
            "a marker does not make home removable",
        )
        .unwrap();
        assert!(refusal(&home, protected).contains("your home directory"));
        assert!(refusal(&tmp.path().join("home"), protected).contains("your home directory"));
    }

    #[test]
    fn the_repository_checkout_and_its_parents_are_never_a_sandbox() {
        let (_tmp, repo, home, protected) = sandbox_world();
        assert!(refusal(&repo, &protected).contains("repository"));
        // The parent of the checkout is also the parent of whatever else lives beside it.
        assert!(refusal(&repo.join("../.."), &protected).contains("it is, or contains"));
        assert!(refusal(&home.join("src"), &protected).contains("repository"));
        // `.` resolved against the checkout is the checkout.
        assert!(refusal(&repo.join("."), &protected).contains("repository"));
    }

    #[test]
    fn a_filesystem_root_is_never_a_sandbox() {
        let root = std::env::current_dir()
            .unwrap()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();
        assert!(root.parent().is_none(), "the test needs a real root");
        assert!(refusal(&root, &[]).contains("filesystem root"));
    }

    #[test]
    fn a_non_empty_directory_this_harness_did_not_create_is_refused_and_left_alone() {
        let (tmp, _repo, _home, protected) = sandbox_world();
        let videos = tmp.path().join("Videos");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("holiday.mp4"), b"irreplaceable").unwrap();

        let err = refusal(&videos, &protected);
        assert!(
            err.contains(SANDBOX_MARKER),
            "the error names the rule: {err}"
        );
        assert!(
            videos.join("holiday.mp4").is_file(),
            "vetting deletes nothing"
        );
    }

    #[test]
    fn a_missing_an_empty_and_a_marked_directory_may_be_deleted() {
        let (tmp, _repo, _home, protected) = sandbox_world();
        let missing = tmp.path().join("fresh");
        assert_eq!(vet_sandbox(&missing, &protected).unwrap(), Sandbox::Absent);

        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(
            vet_sandbox(&empty, &protected).unwrap(),
            Sandbox::Removable(_)
        ));

        // What a previous run leaves: a marker and a data directory it filled.
        let marked = tmp.path().join("marked");
        mark_sandbox(&marked).unwrap();
        std::fs::create_dir_all(marked.join("appdata/clips")).unwrap();
        assert!(matches!(
            vet_sandbox(&marked, &protected).unwrap(),
            Sandbox::Removable(_)
        ));
    }

    #[test]
    fn a_marker_that_is_a_directory_is_not_a_marker() {
        let (tmp, _repo, _home, protected) = sandbox_world();
        let dir = tmp.path().join("work");
        std::fs::create_dir_all(dir.join(SANDBOX_MARKER)).unwrap();
        assert!(refusal(&dir, &protected).contains(SANDBOX_MARKER));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_judged_by_where_it_leads() {
        // `--work-dir ./work` where `work` is a link to the home directory is the home directory.
        let (tmp, _repo, home, protected) = sandbox_world();
        let link = tmp.path().join("work");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(refusal(&link, &protected).contains("it is, or contains"));
    }

    #[test]
    fn the_default_work_directory_is_allowed_to_be_created_and_then_re_run() {
        let (_tmp, repo, _home, protected) = sandbox_world();
        let work = repo.join("target/verify/work");
        assert_eq!(vet_sandbox(&work, &protected).unwrap(), Sandbox::Absent);
        mark_sandbox(&work).unwrap();
        assert!(matches!(
            vet_sandbox(&work, &protected).unwrap(),
            Sandbox::Removable(_)
        ));
    }

    fn session_with_work_dir(repo: &Path, work_dir: PathBuf) -> Session {
        Session::new(
            Options {
                report: repo.join("report.md"),
                work_dir,
                seconds: 45,
                dev_software: false,
                no_build: false,
                cli: None,
                clean_work: false,
                strict: false,
            },
            repo.to_path_buf(),
        )
    }

    #[test]
    fn reset_sandbox_refuses_a_directory_it_did_not_create_and_stops_the_run() {
        let (tmp, repo, _home, _protected) = sandbox_world();
        let videos = tmp.path().join("Videos");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("holiday.mp4"), b"irreplaceable").unwrap();

        let mut session = session_with_work_dir(&repo, videos.clone());
        assert!(!session.reset_sandbox(), "the run must not go on");
        assert!(videos.join("holiday.mp4").is_file(), "nothing was deleted");
        assert!(
            !videos.join(SANDBOX_MARKER).exists(),
            "and nothing was claimed"
        );
        assert!(
            matches!(session.checks[0].outcome, Outcome::Fail(_)),
            "the refusal is a failed check, not a pass"
        );
    }

    #[test]
    fn reset_sandbox_empties_its_own_directory_and_marks_the_new_one() {
        let (tmp, repo, _home, _protected) = sandbox_world();
        let work = tmp.path().join("work");
        mark_sandbox(&work).unwrap();
        std::fs::write(work.join("stale-index.db"), b"from the last run").unwrap();

        let mut session = session_with_work_dir(&repo, work.clone());
        assert!(session.reset_sandbox());
        assert!(
            !work.join("stale-index.db").exists(),
            "the last run is gone"
        );
        assert!(
            work.join(SANDBOX_MARKER).is_file(),
            "the next run may repeat this"
        );
        assert!(matches!(session.checks[0].outcome, Outcome::Pass));
    }

    #[test]
    fn a_run_that_is_too_short_is_refused_rather_than_run() {
        let err = parse_options(
            &["--seconds".to_string(), "2".to_string()],
            Path::new("/repo"),
        )
        .expect_err("a two-second run cannot fill the ring")
        .to_string();
        assert!(err.contains("at least 5"), "got: {err}");
        let err = parse_options(&["--nonsense".to_string()], Path::new("/repo"))
            .expect_err("an unknown flag must be refused")
            .to_string();
        assert!(err.contains("--nonsense"), "got: {err}");
    }

    #[test]
    fn the_summary_counts_the_failed_checks_and_not_the_rest() {
        // The exit code follows the FAIL count only: a not-performed check is honesty, not a failure.
        let mut session = Session::new(
            Options {
                report: PathBuf::from("/tmp/report.md"),
                work_dir: PathBuf::from("/tmp/work"),
                seconds: 45,
                dev_software: false,
                no_build: false,
                cli: None,
                clean_work: false,
                strict: false,
            },
            PathBuf::from("/repo"),
        );
        session.pass("a", "a", "—", "x", "y");
        session.not_performed("b", "b", "—", "x", "y", "reason");
        assert!(session
            .console_summary()
            .contains("1 passed, 0 failed, 1 not performed"));
        session.fail("c", "c", "—", "x", "y", "reason");
        assert!(session
            .console_summary()
            .contains("1 passed, 1 failed, 1 not performed"));
    }

    #[test]
    fn a_run_that_skipped_checks_does_not_call_itself_a_plain_pass() {
        // The verdict is the line a reader skims. A run that could not judge half of its
        // checks must not read as a plain PASS, and the count of what was not judged has to
        // be in the verdict itself rather than in a footnote under it.
        assert_eq!(
            verdict(Tally {
                passed: 9,
                failed: 0,
                not_performed: 0
            }),
            "PASS (on this machine, this run)"
        );
        assert_eq!(
            verdict(Tally {
                passed: 9,
                failed: 0,
                not_performed: 7
            }),
            "PASS with 7 check(s) not performed (on this machine, this run)"
        );
        assert_eq!(
            verdict(Tally {
                passed: 9,
                failed: 1,
                not_performed: 7
            }),
            "FAIL"
        );
    }

    #[test]
    fn only_a_strict_run_fails_over_a_check_that_could_not_be_performed() {
        // The default is lenient, and deliberately so: the harness exists to run on machines
        // that cannot perform half of it, and a run that refuses to report is worse than a
        // report that says what was not judged. `--strict` is the caller that wants the
        // absence of evidence to be as loud as the evidence of a defect.
        let skipped = Tally {
            passed: 3,
            failed: 0,
            not_performed: 7,
        };
        assert!(!run_failed(skipped, false));
        assert!(run_failed(skipped, true));

        let failed = Tally {
            passed: 3,
            failed: 1,
            not_performed: 0,
        };
        assert!(run_failed(failed, false));
        assert!(run_failed(failed, true));

        let clean = Tally {
            passed: 3,
            failed: 0,
            not_performed: 0,
        };
        assert!(!run_failed(clean, false));
        assert!(!run_failed(clean, true));
    }
}
