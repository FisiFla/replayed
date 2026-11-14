//! Choosing a hardware encoder, and failing loudly when there isn't one.

use crate::{Vendor, VideoCodec};
use anyhow::{bail, Context, Result};
use replayed_media::{smoke_test_encoder, FfmpegBinaries};
use std::sync::{Mutex, OnceLock};

/// Vendors tried, in order, for `vendor = "auto"`.
const AUTO_ORDER: [Vendor; 3] = [Vendor::Nvenc, Vendor::Qsv, Vendor::Amf];

/// Pick a hardware encoder, or explain precisely why none is usable.
///
/// There is no software fallback: a CPU encode would silently destroy in-game performance.
///
/// A vendor is accepted only once it has encoded a frame ([`smoke_test_cached`]). `ffmpeg
/// -encoders` is what ffmpeg was compiled with and names `h264_amf` on a machine with no AMD
/// driver (an NVIDIA + Intel iGPU machine then died mid-capture with "video writer thread has
/// stopped"). Encoding one frame turns that into a startup failure naming the encoder and reason.
pub fn select_vendor(bin: &FfmpegBinaries, requested: &str, codec: VideoCodec) -> Result<Vendor> {
    // Resolve the request before touching ffmpeg, so a bad config value fails immediately and independent of hardware.
    let candidates = candidates_for(requested)?;

    let advertised = advertised_encoders(bin)?;
    select_usable(&candidates, codec, &advertised, |encoder| {
        smoke_test_cached(bin, encoder)
    })
}

/// The vendors a config value asks for, in the order they are tried.
fn candidates_for(requested: &str) -> Result<Vec<Vendor>> {
    Ok(match requested {
        "auto" => AUTO_ORDER.to_vec(),
        "nvenc" => vec![Vendor::Nvenc],
        "qsv" => vec![Vendor::Qsv],
        "amf" => vec![Vendor::Amf],
        other => bail!("unknown encode.vendor: {other}"),
    })
}

/// The selection loop, with "can this machine actually run it?" injected.
///
/// `usable` is a parameter so the ordering and reporting rules are testable on a host with no
/// hardware encoder. The real probe is [`smoke_test_encoder`], passed in by [`select_vendor`].
///
/// A candidate that is not advertised is never probed, and an advertised one must still encode a
/// frame before it is chosen. The first that passes wins; if none does, the error names every
/// candidate tried and what happened to each.
fn select_usable(
    candidates: &[Vendor],
    codec: VideoCodec,
    advertised: &[String],
    mut usable: impl FnMut(&'static str) -> Result<(), String>,
) -> Result<Vendor> {
    let mut outcome: Vec<String> = Vec::with_capacity(candidates.len());
    for vendor in candidates {
        let encoder = codec.hw_encoder_name(*vendor);
        if !advertised.iter().any(|name| name == encoder) {
            outcome.push(format!("{encoder}: not advertised by this ffmpeg build"));
            continue;
        }
        match usable(encoder) {
            Ok(()) => {
                // Breadcrumb: the vendor chosen is the one that encoded a frame, not the first one listed.
                tracing::debug!("{encoder} passed its smoke test");
                return Ok(*vendor);
            }
            Err(reason) => outcome.push(format!(
                "{encoder}: advertised, but it could not encode a single frame: {reason}"
            )),
        }
    }

    bail!(
        "no usable hardware encoder. Tried in order — {}. \
         An encoder appearing in `ffmpeg -encoders` does not mean this machine can run \
         it: that list names every encoder ffmpeg was built with, including ones whose \
         vendor runtime is not installed. Install the GPU vendor runtime (NVIDIA driver / \
         Intel graphics driver / AMD Adrenalin), or set encode.vendor to a vendor this \
         machine has. replayed will not fall back to CPU encoding because it would cost \
         game performance.",
        outcome.join("; ")
    )
}

/// Encoder names ffmpeg advertises. `-encoders` lines look like:
/// ` V....D h264_nvenc  NVIDIA NVENC H.264 encoder (codec h264)`.
fn advertised_encoders(bin: &FfmpegBinaries) -> Result<Vec<String>> {
    let out = replayed_media::sidecar_command(&bin.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .with_context(|| format!("running {}", bin.ffmpeg.display()))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
        .collect())
}

/// What a smoke test said: whether the encoder encoded a frame, or ffmpeg's reason why not.
type SmokeResult = Result<(), String>;

/// The encoders that have passed a smoke test. A `Mutex`, not a lock-free map: a probe runs once
/// per encoder per process.
type SmokeCache = Mutex<Vec<&'static str>>;

/// Encoders that passed their smoke test in this process, keyed by encoder name.
///
/// **Only successes are kept.** Whether an encoder *can* work is not settled by one failure: a
/// probe fails for reasons that pass (a GPU that is asleep or switching on a hybrid laptop, NVENC
/// sessions held by another application, a driver reset, an ffmpeg child that was killed). The
/// application is a tray program that lives for weeks, so a failure that was remembered was a
/// failure for as long as it ran: the app started at login with the discrete GPU powered down,
/// the probe failed once, and every recording after that was refused or fell to a worse vendor
/// until the user quit and started it again. A success is safe to remember (an encoder that
/// encoded a frame will start again), and a failure is simply probed again by the next recording.
/// One process resolves one ffmpeg binary, so the encoder name alone is the key.
static SMOKE_RESULTS: OnceLock<SmokeCache> = OnceLock::new();

/// [`smoke_test_encoder`], with a success remembered for the process lifetime.
fn smoke_test_cached(bin: &FfmpegBinaries, encoder: &'static str) -> Result<(), String> {
    let results = SMOKE_RESULTS.get_or_init(|| Mutex::new(Vec::new()));
    remember_successes(results, encoder, || smoke_test_encoder(bin, encoder))
}

/// Run `probe` unless `encoder` is already known to work, and remember the result only if it
/// passed.
///
/// The lock is never held across the child (a probe can take seconds). A poisoned lock is
/// recovered rather than panicking on the failure path; the worst case is one probe run twice.
fn remember_successes(
    cache: &SmokeCache,
    encoder: &'static str,
    probe: impl FnOnce() -> SmokeResult,
) -> SmokeResult {
    let known = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&encoder);
    if known {
        return Ok(());
    }

    let result = probe();
    if result.is_ok() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(encoder);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed probe is probed again, because the failure may have been the moment and not the
    /// machine; a passed one is not, because an encoder that encoded a frame will start again.
    #[test]
    fn only_a_passed_smoke_test_is_remembered() {
        use std::cell::Cell;
        let cache: SmokeCache = Mutex::new(Vec::new());
        let runs = Cell::new(0);
        let ask = |outcome: SmokeResult| {
            remember_successes(&cache, "h264_nvenc", || {
                runs.set(runs.get() + 1);
                outcome
            })
        };

        assert!(ask(Err("GPU asleep".into())).is_err());
        assert!(
            ask(Ok(())).is_ok(),
            "the failure was not remembered: the encoder was asked again and now works"
        );
        assert_eq!(runs.get(), 2, "both attempts really ran the probe");

        assert!(
            ask(Err("must not run".into())).is_ok(),
            "a success is remembered"
        );
        assert_eq!(runs.get(), 2, "and the third call did not run the probe");
    }

    #[test]
    fn an_unknown_vendor_name_fails_without_probing_ffmpeg() {
        // A bogus binary path proves validation happens before any spawn.
        let bin = FfmpegBinaries {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
        };
        let err = select_vendor(&bin, "voodoo", VideoCodec::H264).unwrap_err();
        assert!(err.to_string().contains("voodoo"), "got: {err}");
    }

    #[test]
    fn a_named_vendor_is_the_only_candidate_tried() {
        assert_eq!(candidates_for("amf").unwrap(), vec![Vendor::Amf]);
        assert_eq!(candidates_for("qsv").unwrap(), vec![Vendor::Qsv]);
        assert_eq!(candidates_for("nvenc").unwrap(), vec![Vendor::Nvenc]);
        assert_eq!(candidates_for("auto").unwrap(), AUTO_ORDER.to_vec());
    }

    /// The bug the smoke test exists for: ffmpeg advertises an encoder the machine cannot
    /// initialise. Selection must reject it and use the next candidate.
    #[test]
    fn an_advertised_but_unusable_encoder_is_rejected_in_favour_of_the_next_candidate() {
        let advertised = advertised_all_h264();
        let mut probed: Vec<&str> = Vec::new();

        let vendor = select_usable(
            &candidates_for("auto").unwrap(),
            VideoCodec::H264,
            &advertised,
            |encoder| {
                probed.push(encoder);
                match encoder {
                    // The measured Windows shape: the encoder is listed but cannot start because its vendor runtime is not installed.
                    "h264_nvenc" => Err("Cannot load nvcuda.dll".to_string()),
                    _ => Ok(()),
                }
            },
        )
        .expect("h264_qsv passes its smoke test");

        assert_eq!(vendor, Vendor::Qsv, "the first vendor that can encode wins");
        assert_eq!(
            probed,
            ["h264_nvenc", "h264_qsv"],
            "candidates are tried in order and the loop stops at the first pass"
        );
    }

    /// A named vendor must never be quietly substituted: `vendor = "amf"` asks for AMF.
    #[test]
    fn a_requested_vendor_that_fails_is_an_error_not_a_substitution() {
        let err = select_usable(
            &candidates_for("amf").unwrap(),
            VideoCodec::H264,
            &advertised_all_h264(),
            |_| Err("DLL amfrt64.dll failed to open".to_string()),
        )
        .expect_err("amf was requested and amf cannot encode");

        let msg = err.to_string();
        assert!(
            msg.contains("h264_amf"),
            "the failure names the encoder: {msg}"
        );
        assert!(
            msg.contains("DLL amfrt64.dll failed to open"),
            "and ffmpeg's reason: {msg}"
        );
        assert!(
            !msg.contains("h264_nvenc") && !msg.contains("h264_qsv"),
            "no other vendor was even considered: {msg}"
        );
    }

    /// The message is the deliverable: it has to name every candidate and its own reason.
    #[test]
    fn a_failed_selection_names_every_vendor_tried_and_its_concrete_reason() {
        // Only AMF is advertised, so the other two report the other failure mode.
        let advertised = vec!["h264_amf".to_string()];
        let err = select_usable(
            &candidates_for("auto").unwrap(),
            VideoCodec::H264,
            &advertised,
            |encoder| Err(format!("{encoder}: cannot load its vendor library")),
        )
        .expect_err("nothing can encode here");

        let msg = err.to_string();
        for name in ["h264_nvenc", "h264_qsv", "h264_amf"] {
            assert!(msg.contains(name), "{name} must be named: {msg}");
        }
        assert!(
            msg.contains("not advertised"),
            "the absent ones say why: {msg}"
        );
        assert!(
            msg.contains("h264_amf: advertised, but it could not encode a single frame"),
            "the advertised one reports its own failure: {msg}"
        );
        assert!(
            msg.contains("h264_amf: cannot load its vendor library"),
            "and carries ffmpeg's concrete reason: {msg}"
        );
        assert!(
            msg.contains("will not fall back to CPU encoding") && msg.contains("game performance"),
            "the no-CPU-fallback guarantee and its reason must survive: {msg}"
        );
    }

    /// An encoder that is not advertised is not probed at all: the list is the cheap first filter,
    /// and probing an unknown encoder would report "unknown encoder" instead of the real story.
    #[test]
    fn a_candidate_that_is_not_advertised_is_never_probed() {
        let mut probed: Vec<&str> = Vec::new();
        let err = select_usable(
            &candidates_for("auto").unwrap(),
            VideoCodec::H264,
            &[],
            |encoder| {
                probed.push(encoder);
                Ok(())
            },
        )
        .expect_err("an empty advertised list cannot produce a vendor");

        assert!(probed.is_empty(), "nothing was spawned: {probed:?}");
        assert!(err.to_string().contains("h264_nvenc"), "got: {err}");
    }

    /// The point of the smoke test: `h264_amf` is in ffmpeg's list on a machine with no AMD driver,
    /// so advertising is not acceptance.
    #[test]
    fn advertising_alone_does_not_make_a_vendor_usable() {
        let advertised = vec!["h264_amf".to_string()];
        let err = select_usable(
            &candidates_for("amf").unwrap(),
            VideoCodec::H264,
            &advertised,
            |_| Err("DLL amfrt64.dll failed to open".to_string()),
        )
        .expect_err("being listed is not enough; it has to encode a frame");

        assert!(
            err.to_string().contains("could not encode a single frame"),
            "got: {err}"
        );
    }

    fn advertised_all_h264() -> Vec<String> {
        ["h264_nvenc", "h264_qsv", "h264_amf"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}
