//! The encoder probe: which hardware encoders this machine has and, the part that matters,
//! which of them can actually encode a frame.
//!
//! Shared by `xtask probe` (prints it) and `xtask verify` (embeds it in the report and picks a
//! vendor that must fail).

use anyhow::{Context, Result};
use replayed_media::FfmpegBinaries;
use std::path::PathBuf;
use std::process::Command;

/// Every hardware-encoder id the app knows about, in the order `vendor = "auto"` tries them.
pub const CANDIDATES: [&str; 6] = [
    "h264_nvenc",
    "hevc_nvenc",
    "h264_qsv",
    "hevc_qsv",
    "h264_amf",
    "hevc_amf",
];

/// The vendors a config may name, paired with their H.264 encoder id.
pub const VENDORS: [(&str, &str); 3] = [
    ("nvenc", "h264_nvenc"),
    ("qsv", "h264_qsv"),
    ("amf", "h264_amf"),
];

/// What the probe found about one encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// ffmpeg lists it *and* this machine encoded one 320x240 frame with it.
    Works,
    /// ffmpeg lists it; this machine could not open a session. Carries ffmpeg's reason.
    Fails(String),
    /// ffmpeg was not built with it.
    NotAdvertised,
}

impl State {
    /// The one-line form used in the report.
    pub fn describe(&self) -> String {
        match self {
            State::Works => "advertised, WORKS".to_string(),
            State::Fails(reason) => format!("advertised, FAILS: {reason}"),
            State::NotAdvertised => "not advertised".to_string(),
        }
    }
}

/// The probe's result: one row per candidate, plus the ffmpeg that answered.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub ffmpeg: PathBuf,
    pub rows: Vec<(String, State)>,
}

impl ProbeReport {
    /// Whether a given encoder id passed its smoke test.
    pub fn works(&self, encoder: &str) -> bool {
        self.rows
            .iter()
            .any(|(name, state)| name == encoder && *state == State::Works)
    }

    /// A vendor whose H.264 encoder this machine cannot use, to test the unusable-vendor failure.
    ///
    /// Prefers an `advertised, FAILS` vendor, then `not advertised`. `None` when every vendor
    /// works here, a legitimate state: the check is then reported as not performed, not faked.
    pub fn unusable_h264_vendor(&self) -> Option<(&'static str, &'static str)> {
        let unusable = VENDORS.iter().filter(|(_, encoder)| !self.works(encoder));
        let advertised_fails = unusable.clone().find(|(_, encoder)| {
            self.state_of(encoder)
                .is_some_and(|s| matches!(s, State::Fails(_)))
        });
        advertised_fails
            .or_else(|| unusable.into_iter().next())
            .copied()
    }

    fn state_of(&self, encoder: &str) -> Option<&State> {
        self.rows
            .iter()
            .find(|(name, _)| name == encoder)
            .map(|(_, state)| state)
    }

    /// The text `xtask probe` prints (and the report embeds verbatim).
    pub fn render(&self) -> String {
        let mut text = format!("{}\n", self.ffmpeg.display());
        for (name, state) in &self.rows {
            text.push_str(&format!("{name:<12} {}\n", state.describe()));
        }
        text.push_str(
            "\nWORKS means ffmpeg listed it AND encoded one 320x240 frame with it. A vendor value \
             only works when it lands on WORKS. This is a synthetic frame: it does not capture \
             the screen and does not inject input.",
        );
        if self.rows.iter().all(|(_, s)| *s != State::Works) {
            text.push_str(
                "\nNo hardware encoder on this machine can encode a frame; the capture app will \
                 refuse to start rather than fall back to CPU encoding.",
            );
        }
        text
    }
}

/// The text of `ffmpeg -encoders`, or the reason it is not one.
///
/// An ffmpeg that is corrupt, blocked by antivirus or missing a DLL starts, exits non-zero and
/// prints nothing on stdout. Read as a listing, that is an empty one, and every candidate comes
/// out as "not advertised" followed by "no hardware encoder on this machine can encode a frame":
/// a misdiagnosis of the machine, copied into a verify report, with the real cause (ffmpeg
/// itself is broken) never shown. A failed run, or an empty listing, is an error instead.
fn encoder_listing(status: &str, success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String> {
    if !success {
        let first = String::from_utf8_lossy(stderr)
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("(nothing on stderr)")
            .trim()
            .to_string();
        anyhow::bail!("ffmpeg itself failed ({status}): {first}");
    }
    let text = String::from_utf8_lossy(stdout).into_owned();
    if text.trim().is_empty() {
        anyhow::bail!("ffmpeg succeeded but listed no encoders at all");
    }
    Ok(text)
}

/// Run the probe: `ffmpeg -encoders` for what the build carries, then a one-frame smoke
/// test for each advertised candidate.
pub fn probe(bin: &FfmpegBinaries) -> Result<ProbeReport> {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .with_context(|| format!("running {}", bin.ffmpeg.display()))?;
    let text = encoder_listing(
        &out.status.to_string(),
        out.status.success(),
        &out.stdout,
        &out.stderr,
    )
    .with_context(|| format!("{} -encoders", bin.ffmpeg.display()))?;

    let mut rows = Vec::with_capacity(CANDIDATES.len());
    for name in CANDIDATES {
        // Two questions, and only the second is the answer. `-encoders` says the build carries the
        // encoder; the smoke test says this machine can run it, which is what a config `vendor`
        // depends on. A vendor can advertise an encoder it cannot initialise (on a machine with no AMD
        // hardware, ffmpeg listed h264_amf and died with `DLL amfrt64.dll failed to open`), so "listed"
        // is not enough to trust.
        let state = if !text.lines().any(|l| l.contains(name)) {
            State::NotAdvertised
        } else {
            match replayed_media::smoke_test_encoder(bin, name) {
                Ok(()) => State::Works,
                Err(reason) => State::Fails(reason),
            }
        };
        rows.push((name.to_string(), state));
    }
    Ok(ProbeReport {
        ffmpeg: bin.ffmpeg.clone(),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_ffmpeg_is_reported_as_ffmpeg_failing_not_as_no_hardware_encoder() {
        // Non-zero exit, nothing on stdout: a corrupt or blocked sidecar. It used to read as an
        // empty listing, so every vendor printed "not advertised".
        let err = encoder_listing(
            "exit code: 0xc0000135",
            false,
            b"",
            b"\nThe code execution cannot proceed because avcodec-61.dll was not found.\n",
        )
        .expect_err("a failed ffmpeg is not a listing")
        .to_string();
        assert!(err.contains("ffmpeg itself failed"), "got: {err}");
        assert!(err.contains("0xc0000135"), "the status is shown: {err}");
        assert!(
            err.contains("avcodec-61.dll"),
            "the first stderr line is shown: {err}"
        );
    }

    #[test]
    fn a_successful_ffmpeg_with_no_listing_is_an_error_too() {
        let err = encoder_listing("exit status: 0", true, b" \n", b"")
            .expect_err("an empty listing is not an answer")
            .to_string();
        assert!(err.contains("no encoders"), "got: {err}");
    }

    #[test]
    fn a_real_listing_passes_through() {
        let text = encoder_listing("exit status: 0", true, b" V....D h264_nvenc\n", b"").unwrap();
        assert!(text.contains("h264_nvenc"));
    }
}
