//! Backend selection, so every `#[cfg(windows)]` line lives inside this crate.
//!
//! `cargo check --target x86_64-pc-windows-msvc` type-checks this crate from a non-Windows host
//! (it does not link), the only Windows gate without Windows hardware. Selecting here rather than
//! in the CLI makes that gate cover the choice as well as the backends.
//!
//! # No silent stub fallback on Windows
//!
//! On Windows these functions return a real backend or fail. A synthetic stand-in would make the
//! application look like it is buffering while capturing nothing, surfacing only later as an
//! empty clip. The stubs are selected only off Windows.

use crate::stub::StubConfig;
use crate::target::CaptureTarget;
use crate::{AudioBackend, AudioFormat, CaptureBackend, GameAudioSource, GameFinder};
use anyhow::Result;

/// The video backend for this platform.
///
/// `stub` describes the synthetic source used off Windows. On Windows the real backend reports
/// the monitor's native size through [`CaptureBackend::native_size`], so `stub` is not consulted.
pub fn default_video_backend(
    stub: StubConfig,
    target: &CaptureTarget,
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(windows)]
    {
        // The stub's size is irrelevant: WGC reports the monitor it will capture and the caller reads it
        // back via `native_size`. A configured `encode.output_size` that differs is scaled, so no warning.
        let _ = stub;
        Ok(Box::new(crate::wgc::WgcCapture::new(target)?))
    }
    #[cfg(not(windows))]
    {
        if !target.is_default() {
            tracing::warn!(
                ?target,
                "monitor/window capture needs Windows Graphics Capture; the synthetic stub \
                 has no screen, so the target is ignored"
            );
        }
        Ok(Box::new(crate::stub::StubCapture::new(stub)))
    }
}

/// The audio backend for this platform.
///
/// `format` is what the synthetic source produces off Windows. On Windows the WASAPI backend
/// always produces `AudioFormat::default()` (48kHz stereo s16le), so a different request is
/// reported rather than silently ignored.
pub fn default_audio_backend(format: AudioFormat) -> Result<Box<dyn AudioBackend>> {
    #[cfg(windows)]
    {
        if format != AudioFormat::default() {
            tracing::warn!(
                "audio was requested as {:?} but the Windows loopback backend only \
                 produces {:?}",
                format,
                AudioFormat::default()
            );
        }
        Ok(Box::new(crate::wasapi::WasapiLoopback::new()?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(crate::stub::StubAudio::new(format)))
    }
}

/// The game-audio backend: like [`default_audio_backend`], but under [`GameAudioSource::App`] it
/// captures only the process `find_game` reports, where Windows supports that. `output_device` is the
/// output endpoint id whole-output capture listens to (empty: the Windows default).
pub fn game_audio_backend(
    format: AudioFormat,
    source: GameAudioSource,
    find_game: GameFinder,
    output_device: &str,
) -> Result<Box<dyn AudioBackend>> {
    #[cfg(windows)]
    {
        if format != AudioFormat::default() {
            tracing::warn!(
                "audio was requested as {:?} but the Windows loopback backend only \
                 produces {:?}",
                format,
                AudioFormat::default()
            );
        }
        Ok(Box::new(crate::wasapi::WasapiLoopback::with_source(
            source,
            Some(find_game),
            output_device,
        )?))
    }
    #[cfg(not(windows))]
    {
        let _ = (source, find_game, output_device);
        Ok(Box::new(crate::stub::StubAudio::new(format)))
    }
}

/// `CoInitializeEx(COINIT_MULTITHREADED)` for the calling thread, shared by both Windows backends.
///
/// Returns whether this call took a reference on the apartment, i.e. when the caller owes a
/// matching `CoUninitialize` *on this thread*. That is `S_OK` (the thread was not initialised) and
/// also `S_FALSE` (it already was, as MTA): `S_FALSE` is a success that **still increments the
/// thread's initialisation count**, so a caller that treated it as "not mine" leaked one reference
/// per call. Only `RPC_E_CHANGED_MODE`, the one failure that leaves the count alone, reports
/// `false`: COM is usable there and we must not uninitialise it.
///
/// Also makes sure the process's multithreaded apartment is never torn down by the last
/// `CoUninitialize` of those references; see [`keep_mta_alive`] for why that matters.
#[cfg(windows)]
pub(crate) fn init_com() -> Result<bool> {
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    keep_mta_alive();
    // SAFETY: COM is being initialised for the current thread; a null reserved pointer is the documented value.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr == S_OK {
        return Ok(true);
    }
    if hr == S_FALSE {
        tracing::debug!("COM was already initialised on this thread; taking one more reference");
        return Ok(true);
    }
    if hr == RPC_E_CHANGED_MODE {
        tracing::warn!(
            "this thread is already in a single-threaded COM apartment; audio and video \
             capture are expected to work but that is not a tested configuration"
        );
        return Ok(false);
    }
    anyhow::bail!("CoInitializeEx(COINIT_MULTITHREADED) failed: {hr:?}");
}

/// Whether this process holds a reference on the multithreaded apartment that is never given back.
#[cfg(windows)]
static MTA_KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Hold the process's multithreaded apartment open for the rest of its life.
///
/// # Why
///
/// Each `CoInitializeEx(COINIT_MULTITHREADED)` / `CoUninitialize` pair on a thread is a reference
/// on one apartment *shared by the whole process*, and when the last reference goes the apartment
/// is torn down, and COM then unloads every in-process server DLL it had loaded for it. WGC's
/// `GraphicsCapture.dll` is one of those. It keeps worker threads of its own, and they wind down
/// asynchronously after `GraphicsCaptureSession.Close` / `Direct3D11CaptureFramePool.Close` have
/// returned. A backend that is stopped on the thread that started it ends with the last
/// `CoUninitialize` in the process, so the DLL can be unmapped under a thread still executing it:
/// the process dies with `STATUS_ACCESS_VIOLATION` on an instruction fetch at an address inside a
/// module the dump lists as unloaded. That is the crash this closes. In the `wgc_frame` test
/// binary, whose four tests each start and stop a capture on a fresh thread, it was about one run
/// in three, and at a different place every time (a few tests after the stop that set it off).
///
/// A reference we never release means the apartment, and the DLLs it loaded, live as long as the
/// process does. The cost is a few DLLs that stay mapped, which is what the recorder wants anyway:
/// it creates a capture backend again for every recording.
///
/// This does not replace the per-thread `CoInitializeEx`: a thread still needs its own to be in
/// the apartment, and giving that one back is still the owner thread's job.
///
/// A failure is logged and not returned: capture works without it, as it did before, and the
/// crash it prevents is a race and not a certainty. It is retried on the next call.
#[cfg(windows)]
fn keep_mta_alive() {
    use std::sync::atomic::Ordering;
    use windows::Win32::System::Com::CoIncrementMTAUsage;

    if MTA_KEPT.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: no arguments; the cookie that comes back is deliberately never passed to
    // `CoDecrementMTAUsage`, which is the point of the call.
    match unsafe { CoIncrementMTAUsage() } {
        Ok(_cookie) => MTA_KEPT.store(true, Ordering::Release),
        Err(e) => tracing::warn!(
            "could not keep the COM apartment open ({e}); a stopped capture may unload \
             GraphicsCapture.dll under its own worker threads"
        ),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::init_com;
    use windows::Win32::System::Com::{
        CoGetApartmentType, CoUninitialize, APTTYPE, APTTYPEQUALIFIER, APTTYPE_MTA,
    };

    /// The apartment a thread that never initialised COM finds itself in, if it is in one.
    ///
    /// A thread with no `CoInitializeEx` of its own is in the process's *implicit* multithreaded
    /// apartment when one exists and in none when it does not (`CO_E_NOTINITIALIZED`), which makes
    /// this a probe for whether the apartment is alive, from a thread that cannot be keeping it so.
    fn apartment_seen_by_a_thread_that_never_initialised_com() -> windows::core::Result<APTTYPE> {
        std::thread::spawn(|| {
            let mut kind = APTTYPE::default();
            let mut qualifier = APTTYPEQUALIFIER::default();
            // SAFETY: both are valid out-parameters for the calling thread.
            unsafe { CoGetApartmentType(&mut kind, &mut qualifier) }.map(|()| kind)
        })
        .join()
        .expect("the probe thread does not panic")
    }

    /// The regression test for the `GraphicsCapture.dll` unload crash (see [`super::keep_mta_alive`]).
    ///
    /// A capture backend that is stopped on the thread that started it ends with the process's last
    /// `CoUninitialize`. That used to tear the multithreaded apartment down, and COM unloads the
    /// in-process servers of a torn-down apartment, `GraphicsCapture.dll` among them, under worker
    /// threads of its own that were still winding down. The apartment has to outlive its last user:
    /// this takes it and gives it back from a thread of its own, as the backend does, and asks
    /// from another thread whether it is still there. Without the fix the answer is
    /// `CO_E_NOTINITIALIZED`.
    ///
    /// Needs no desktop and no device: it is COM bookkeeping, so it runs on every Windows host, and
    /// it is the part of the crash that can be tested without the interactive session the
    /// `wgc_frame` tests need.
    #[test]
    fn the_com_apartment_outlives_the_last_backend_that_took_it() {
        std::thread::spawn(|| {
            let owned = init_com().expect("COM can be initialised on a thread of its own");
            if owned {
                // SAFETY: `init_com` took a reference on this thread, which this gives back.
                unsafe { CoUninitialize() };
            }
        })
        .join()
        .expect("the owner thread does not panic");

        let kind = apartment_seen_by_a_thread_that_never_initialised_com().expect(
            "the process's multithreaded apartment was torn down when its last user gave it \
             back, which unloads GraphicsCapture.dll under its own worker threads",
        );
        assert_eq!(kind, APTTYPE_MTA);
    }
}
