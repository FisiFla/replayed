//! WASAPI microphone capture: the default communications capture endpoint.
//!
//! The sibling of `crate::wasapi`: same `AudioBackend` shape, canonical format
//! ([`MICROPHONE_FORMAT`]), 10ms blocks and [`crate::ConvertedSilenceWatch`]. The conventions match
//! deliberately, because the recorder muxes this track next to the game audio and any difference
//! shows up as drift between the two.
//!
//! # A capture endpoint, not a render one
//!
//! `crate::wasapi::WasapiLoopback` opens the default render endpoint with
//! `AUDCLNT_STREAMFLAGS_LOOPBACK`, which hands a render stream's audio back for reading. Copying
//! that flag here would be wrong: a microphone is already an `eCapture` endpoint and is opened as
//! an ordinary shared-mode capture stream. A compile-time assertion below keeps the flag out.
//!
//! # Which endpoint
//!
//! `GetDefaultAudioEndpoint(eCapture, eCommunications)`. The communications role is what Windows
//! assigns to the device the user talks into (what games and Discord select), so it is "the
//! microphone" even on a machine with a webcam mic, a headset and an audio interface. With no
//! device designated, Windows resolves the role to the default capture device, so there is no
//! second attempt. A machine with no capture endpoint fails on the call itself, naming the role,
//! rather than falling back to an endpoint the user did not choose.
//!
//! # Format negotiation, and no fallback
//!
//! The stream is opened as 48kHz stereo s16le whatever the microphone's mix format is, using
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` (plus `SRC_DEFAULT_QUALITY`) so the engine's matrixer and
//! sample rate converter do the work, as in the loopback backend. The native format is logged next
//! to the request. If the engine will not open the device in the requested format, `start` fails
//! loudly and names both formats: publishing the device's own format under a 48kHz stereo label
//! would silently corrupt the audio timeline.
//!
//! # Always stereo
//!
//! The published format is stereo even for a mono microphone. The pipeline derives the timeline
//! from the byte count, so a mono block would be read as half its duration and the track would
//! slide against the game audio and video. Because the stream is requested with 2 channels, the
//! engine's matrixer up-mixes mono to L=R. Doing it in Rust would mean opening the stream in the
//! device's mono format, losing the engine's rate conversion and the crate's canonical packet
//! format. The log line says `engine_upmixes_mono_to_stereo = true` for a mono device.
//!
//! # Block size
//!
//! Ten milliseconds, as in the loopback backend ([`AudioFormat::bytes_per_10ms`]: 480 frames,
//! 1920 bytes). The stream buffer is the endpoint's default device period; the engine signals the
//! event once per period, so the buffer must be at least that long.
//!
//! # Silence, and a microphone that goes away
//!
//! Muted, hardware-muted and disconnected microphones produce digital silence that samples cannot
//! distinguish from audio, so every block is fed to [`crate::ConvertedSilenceWatch`], which
//! reports a long unbroken run once and never fails or stops capture.
//!
//! A removed endpoint (headset unplugged, microphone disabled, USB device yanked) is different: a
//! live shared-mode capture stream delivers a packet every device period even when nobody is
//! talking, so a quiet stream has died. `AUDCLNT_E_DEVICE_INVALIDATED` and
//! `AUDCLNT_E_RESOURCES_INVALIDATED` mark the stream lost, and a stream that delivered nothing
//! for `DEVICE_STALL` is checked against `IMMDevice::GetState` and marked lost when the endpoint
//! is no longer active. Waiting is bounded by the caller's timeout, so a dead endpoint is not a
//! hot loop.
//!
//! **A lost microphone does not end the recording.** It used to: the error went up through the
//! pump and the engine failed the whole session, video and game audio included, because someone
//! unplugged a headset. The stream is now held by the same `RecoveringSource` as the game audio:
//! the outage is padded with silence on the unbroken timeline, so the voice track stays aligned
//! with the video, and the endpoint is reopened every `audio_recovery::RETRY_INTERVAL` (the
//! default communications capture endpoint again, or the configured one). Only the *first* open
//! is fatal, so a recording that asked for a microphone and never had one is refused at start.
//!
//! The Windows half type-checks for `x86_64-pc-windows-msvc` from macOS (`cargo check` does not
//! link) but has never been executed on Windows: neither endpoint selection, event-callback
//! pacing, nor the engine accepting the 48kHz stereo request for a real microphone has been observed.

use crate::{AudioBackend, AudioBuffer, AudioFormat};
use std::time::{Duration, Instant};

/// The format the microphone backend publishes and asks the engine for: the crate's canonical
/// [`AudioFormat::default`], spelled out so the microphone and system-audio backends can be
/// compared as values (the recorder declares both tracks with `-ar 48000 -ac 2`).
pub const MICROPHONE_FORMAT: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 2,
};

/// Peak sample of the [`StubMicrophone`]'s default waveform: about -12 dBFS, clearly not silence
/// and a plausible voice level.
const DEFAULT_AMPLITUDE: i16 = 8_000;

/// The platform microphone backend, or a clear error on a platform without one.
///
/// `device` is a WASAPI endpoint id (see [`crate::audio_devices`]); empty means the default
/// communications capture endpoint.
///
/// On Windows this is [`WasapiMicrophone`]. Off Windows it returns an error rather than a
/// synthetic stand-in, because a stub selected at runtime would make the recorder look like it is
/// capturing a voice track while recording nothing. [`StubMicrophone`] is the type tests use.
pub fn microphone_backend(device: &str) -> anyhow::Result<Box<dyn AudioBackend>> {
    #[cfg(windows)]
    {
        Ok(Box::new(WasapiMicrophone::new(device)?))
    }
    #[cfg(not(windows))]
    {
        let _ = device;
        Err(no_microphone_on_this_platform())
    }
}

/// The one message every non-Windows microphone entry point returns.
#[cfg(not(windows))]
fn no_microphone_on_this_platform() -> anyhow::Error {
    anyhow::anyhow!(
        "microphone capture is only implemented on Windows (WASAPI): this build has no \
         microphone backend. Record without a microphone, or run on Windows"
    )
}

/// The microphone backend on a host that has no WASAPI.
///
/// Exists so the type has the same name and `new()` shape wherever it compiles. Construction
/// always fails; it never silently succeeds with a stand-in (use [`StubMicrophone`] for that).
#[cfg(not(windows))]
#[derive(Debug)]
pub struct WasapiMicrophone;

#[cfg(not(windows))]
impl WasapiMicrophone {
    /// Always an error off Windows: there is no WASAPI to capture a microphone with.
    pub fn new(_device: &str) -> anyhow::Result<Self> {
        Err(no_microphone_on_this_platform())
    }
}

/// A microphone backend that yields synthetic blocks, for wiring tests off Windows.
///
/// Produces canonical 48kHz stereo s16le blocks on the real-time schedule a device would, so the
/// recorder's pacing, silence handling and muxing see the stream [`WasapiMicrophone`] would deliver.
///
/// The default signal is not silence, or every "audio flowed through the microphone path"
/// assertion would pass vacuously. `with_signal(0)` is a muted microphone, which
/// [`crate::ConvertedSilenceWatch`] exists to report.
#[derive(Debug)]
pub struct StubMicrophone {
    format: AudioFormat,
    /// Block length in milliseconds. The convention is 10; tests that must reach the 30s silence
    /// threshold quickly use whole-second blocks.
    block_ms: u64,
    /// Peak sample of the emitted square wave; 0 means digital silence.
    amplitude: i16,
    /// pts of the next block, measured from `start`: schedule and timestamps from one counter, so they cannot disagree.
    next_pts: Duration,
    /// Frames emitted in the current run, so the waveform is continuous across block boundaries.
    phase: u64,
    /// Blocks handed out in the current run, via either path (`next_buffer` or `drain_for`).
    emitted: u64,
    started_at: Option<Instant>,
}

impl StubMicrophone {
    /// A stub producing `format` in blocks of `block_ms` milliseconds with a deterministic non-silent
    /// waveform. `block_ms` is clamped to at least 1: a zero-length block would make no progress.
    pub fn new(format: AudioFormat, block_ms: u64) -> Self {
        Self {
            format,
            block_ms: block_ms.max(1),
            amplitude: DEFAULT_AMPLITUDE,
            next_pts: Duration::ZERO,
            phase: 0,
            emitted: 0,
            started_at: None,
        }
    }

    /// Emit a waveform whose peak sample is `amplitude` instead of the default signal.
    ///
    /// `with_signal(0)` is digital silence (a muted or disconnected microphone). A negative amplitude
    /// inverts the waveform and is still non-silent.
    pub fn with_signal(mut self, amplitude: i16) -> Self {
        self.amplitude = amplitude;
        self
    }

    /// How many blocks this stub has handed out since the last [`AudioBackend::start`], counted for
    /// both paths. Reset by `start`, because a restart is a new run.
    pub fn buffers_emitted(&self) -> u64 {
        self.emitted
    }

    /// Produce the blocks covering `elapsed` of timeline without sleeping.
    ///
    /// The pacing-free twin of [`AudioBackend::next_buffer`], like `StubAudio::drain_for`, for tests
    /// that need seconds of blocks. The emitted blocks are identical on both paths.
    pub fn drain_for(&mut self, elapsed: Duration) -> Vec<AudioBuffer> {
        let blocks = (elapsed.as_millis() / self.block_ms as u128) as u64;
        (0..blocks).map(|_| self.next_block()).collect()
    }

    /// Frames in one block. The sample rate is floored at 1 so a zero-rate format cannot divide by
    /// zero; the canonical 48kHz 10ms block is 480 frames.
    fn frames_per_block(&self) -> usize {
        ((self.format.sample_rate.max(1) as u128 * self.block_ms as u128) / 1_000) as usize
    }

    /// How long a block lasts, derived from its frame count and the sample rate as the WASAPI
    /// backends do, so `pts` and the byte count agree by construction.
    fn block_duration(&self) -> Duration {
        let frames = self.frames_per_block() as u64;
        Duration::from_micros(frames * 1_000_000 / self.format.sample_rate.max(1) as u64)
    }

    /// One block of the signal: a 1kHz square wave of `amplitude`, phase-continuous, in every channel.
    ///
    /// A square wave because it is deterministic, its peak is exactly `amplitude`, and it is
    /// non-silent from the first sample, which a silence check on a partly-silent waveform would not be.
    fn next_block(&mut self) -> AudioBuffer {
        let frames = self.frames_per_block();
        let channels = self.format.channels as usize;
        // 1kHz: sample_rate/1000 frames per period, forced even so the half-periods are equal, floored at 2.
        let period = ((self.format.sample_rate.max(1) / 1_000).max(2) & !1) as u64;
        let mut data = Vec::with_capacity(frames * channels * 2);
        for _ in 0..frames {
            let sample = if self.phase % period < period / 2 {
                self.amplitude
            } else {
                self.amplitude.wrapping_neg()
            };
            for _ in 0..channels {
                data.extend_from_slice(&sample.to_le_bytes());
            }
            self.phase = self.phase.wrapping_add(1);
        }
        let buffer = AudioBuffer {
            data,
            frames,
            pts: self.next_pts,
            format: self.format,
        };
        self.next_pts += self.block_duration();
        self.emitted += 1;
        buffer
    }
}

impl AudioBackend for StubMicrophone {
    fn start(&mut self) -> anyhow::Result<()> {
        // A restart is a new run: schedule, waveform phase and counter start together.
        self.next_pts = Duration::ZERO;
        self.phase = 0;
        self.emitted = 0;
        self.started_at = Some(Instant::now());
        Ok(())
    }

    /// Real-time paced like `StubAudio::next_buffer`: `Ok(None)` when the next block is not due
    /// within `timeout`, so a zero timeout is a non-blocking "is anything due?" check. `drain_for` is
    /// the pacing-free path.
    fn next_buffer(&mut self, timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        let started = self
            .started_at
            .ok_or_else(|| anyhow::anyhow!("microphone capture not started"))?;
        let Some(sleep) = self.next_pts.checked_sub(started.elapsed()) else {
            return Ok(Some(self.next_block())); // already due
        };
        if sleep > timeout {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        Ok(Some(self.next_block()))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The real microphone backend: WASAPI shared-mode capture from the default communications capture endpoint.
#[cfg(windows)]
mod wasapi_impl {
    use crate::platform::init_com;
    use crate::{clock_base, is_silent_block, AudioBackend, AudioBuffer, ConvertedSilenceWatch};
    // Shared with the loopback backend rather than copied: the same three questions asked of a
    // different endpoint, which keeps the log lines comparable and the published format identical.
    use crate::audio_recovery::{RecoveringSource, SourceGone, SourceOpener};
    use crate::audio_timeline::{qpc_to_shared, AudioTimeline, PacketInfo, PacketSource};
    use crate::wasapi::{
        check_apartment, native_format, qpc_anchor, requested_format, ApartmentOwner,
        MixFormatGuard, TARGET,
    };
    use anyhow::{bail, Context, Result};
    use std::time::{Duration, Instant};
    use windows::core::IUnknown;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Media::Audio::{
        eCapture, eCommunications, IAudioCaptureClient, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
        AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED,
        AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    /// Bytes per captured frame: two channels of 16-bit PCM. Every packet is already stereo whatever
    /// the device is (see the module docs), so the timeline is derivable from the byte count.
    const BYTES_PER_FRAME: usize = TARGET.channels as usize * 2;

    /// 100-nanosecond units per millisecond, for `IAudioClient::Initialize`.
    const HNS_PER_MS: i64 = 10_000;

    /// How long the stream may deliver nothing before the endpoint's state is checked.
    ///
    /// A live shared-mode capture stream delivers a packet every device period (~10ms) whether or not
    /// anyone is talking: a muted microphone is silent packets, not no packets. So two seconds of
    /// nothing means the endpoint has gone away, yet is long enough that a device slow to start
    /// streaming cannot trip it.
    const DEVICE_STALL: Duration = Duration::from_secs(2);

    /// The flags this backend initialises the stream with.
    ///
    /// `EVENTCALLBACK` makes the engine signal per packet; `AUTOCONVERTPCM` + `SRC_DEFAULT_QUALITY`
    /// insert the channel matrixer and sample rate converter so the stream opens as the canonical
    /// format. `AUDCLNT_STREAMFLAGS_LOOPBACK` is deliberately absent (it applies to a render
    /// endpoint); naming the flags here lets the assertion below guard that.
    const STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

    // A microphone is a capture endpoint, so asking for loopback is a category error. This is a
    // compile-time pin because it is the likeliest way to break this backend by copy-paste.
    const _: () = assert!(
        STREAM_FLAGS & AUDCLNT_STREAMFLAGS_LOOPBACK == 0,
        "AUDCLNT_STREAMFLAGS_LOOPBACK is for capturing a render endpoint; the microphone is an \
         eCapture endpoint and must be opened without it"
    );

    /// WASAPI shared-mode microphone capture, opened as 48kHz stereo s16le with the engine
    /// converting (see the module docs).
    pub struct WasapiMicrophone {
        /// The endpoint id to open; empty is the default communications capture endpoint.
        device: String,
        /// The open stream and the timeline it feeds, reopened when the microphone goes away.
        ///
        /// The same arrangement as the game audio: the timeline is never reset, so the outage is
        /// padded with silence like any other gap and the voice track still lines up with the
        /// video and the game audio when the microphone comes back.
        source: Option<RecoveringSource<MicOpener>>,
        /// The stream generation `silence_watch` was armed for.
        watched_generation: u64,
        /// Whether this thread's COM apartment was taken by us and must be given back.
        com_owned: bool,
        /// The thread that took the apartment, once the backend is started.
        ///
        /// The audio objects live in that thread's apartment, so they may only be used there.
        /// The recorder gives every backend a thread of its own; this is the check that says so.
        apartment: Option<ApartmentOwner>,
        /// Diagnostic for a microphone delivering only silence (see [`ConvertedSilenceWatch`]). Armed
        /// when the endpoint needed engine conversion; it only logs.
        silence_watch: ConvertedSilenceWatch,
    }

    /// Opens the microphone stream, for [`RecoveringSource`].
    ///
    /// It never changes its mind about which endpoint is wanted: the microphone is the one the
    /// recording opened, or the default communications endpoint at the moment a reopen is needed
    /// (an empty `device`). Following a change of the default *while a working stream is open*
    /// would swap a microphone the user is talking into for another mid-sentence, and the
    /// microphone never did that before.
    pub(crate) struct MicOpener {
        device: String,
        /// The key of the stream most recently opened, which is what [`SourceOpener::wanted`]
        /// reports, so the open stream is never judged stale.
        last_key: String,
    }

    pub(crate) struct Client {
        audio: IAudioClient,
        capture: IAudioCaptureClient,
        /// Kept because `GetState` tells a stream that stopped delivering from a device that is gone.
        device: IMMDevice,
        /// The endpoint's id, which is the key [`RecoveringSource`] knows this stream by.
        endpoint: String,
        event: EventHandle,
        /// Whether the endpoint's native format needed the engine's converter (was not already 48kHz
        /// stereo s16), so the capture loop can decide whether silence is worth blaming on conversion.
        converting: bool,
        /// One instant read on both the QPC and the shared clock, to move packet timestamps across.
        anchor_hns: u64,
        anchor: Duration,
        /// When a packet last came out of the engine. Persisted across `next_buffer` calls because the
        /// caller polls in short slices, so a per-call timestamp could never reach [`DEVICE_STALL`].
        last_packet_at: Instant,
        /// When the endpoint's state was last checked, so a stalled stream asks Windows at most
        /// once per [`DEVICE_STALL`].
        last_state_check: Instant,
    }

    // SAFETY: the backend is used from one thread at a time: the one that called `start`, which is
    // also the only one allowed to call `stop`, because COM apartments are thread-affine and
    // `CoUninitialize` has to run where `CoInitializeEx` did. `Send` is required by the
    // `AudioBackend` trait and is sound only because nothing shares these objects concurrently:
    // `IMMDevice`, `IAudioClient` and `IAudioCaptureClient` are free-threaded WASAPI objects and the
    // event is a kernel handle. Moving the backend to another thread is safe; sharing it is not,
    // and nothing does.
    unsafe impl Send for Client {}

    /// Owns the auto-reset event the audio engine signals for each new packet.
    struct EventHandle(HANDLE);

    impl Drop for EventHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateEventW and is closed exactly once, here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            // SAFETY: the client is alive for as long as `self` is; a failure here has nowhere left to be reported.
            unsafe {
                let _ = self.audio.Stop();
            }
        }
    }

    impl WasapiMicrophone {
        /// A microphone backend that opens the default communications capture endpoint in [`AudioBackend::start`].
        ///
        /// Construction touches no device (no COM apartment, nothing can fail), which lets the recorder
        /// build its backend selection before deciding to record. Endpoint and format errors therefore
        /// surface from `start`, with the rest of the recording's startup errors.
        pub fn new(device: &str) -> Result<Self> {
            Ok(Self {
                device: device.to_owned(),
                source: None,
                watched_generation: 0,
                com_owned: false,
                // Set by `start`, on the thread that opens the backend.
                apartment: None,
                // Disarmed until `start` learns whether the endpoint needed conversion.
                silence_watch: ConvertedSilenceWatch::new(false),
            })
        }

        fn release_com(&mut self) {
            if !self.com_owned {
                return;
            }
            if let Some(owner) = &self.apartment {
                if !owner.is_here() {
                    // The apartment belongs to the thread that took it, and `CoUninitialize`
                    // has to run there: giving it back here would uninitialise another
                    // thread's apartment, which is the very thing this check exists to catch.
                    // The leak is the lesser fault, and it is named.
                    tracing::error!(
                        opened_on = %owner,
                        released_from = %ApartmentOwner::here(),
                        "the WASAPI microphone's COM apartment is not being given back: it \
                         belongs to the thread that opened it, and a thread cannot give another \
                         thread's apartment back"
                    );
                    self.com_owned = false;
                    return;
                }
            }
            // SAFETY: `start` took the apartment on this thread.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
            self.com_owned = false;
        }
    }

    impl AudioBackend for WasapiMicrophone {
        fn start(&mut self) -> Result<()> {
            if self.source.is_some() {
                bail!("WASAPI microphone capture is already started");
            }
            // The attempt, before it is made.
            //
            // The microphone is opened the same way the game audio is, and it fails the same
            // way: a call into the audio service that never answers leaves nothing in the
            // log but the absence of the line that says it started. This line is written
            // first on purpose, and it names the endpoint being asked for.
            tracing::info!(
                endpoint = if self.device.is_empty() {
                    "the default communications capture endpoint"
                } else {
                    self.device.as_str()
                },
                requested_sample_rate = TARGET.sample_rate,
                requested_channels = TARGET.channels,
                requested_sample_format = "s16",
                "opening the WASAPI microphone backend"
            );
            self.com_owned = init_com()?;
            // The apartment is this thread's from here on, and so are the objects it holds: the
            // backend is opened, read from and stopped on one thread, and this is the record of
            // which one.
            self.apartment = Some(ApartmentOwner::here());
            // Frame 0 is the moment capture began, as for the game audio, so a microphone that
            // opens a little late starts its track alongside the video and the gap is silence.
            let now = clock_base().elapsed();
            let opener = MicOpener {
                device: self.device.clone(),
                last_key: String::new(),
            };
            // Failing to open the first stream is fatal, unlike every later loss: a recording
            // that asked for a microphone and never had one should say so at start.
            match RecoveringSource::start(opener, AudioTimeline::new(TARGET, now), now) {
                Ok(source) => {
                    self.source = Some(source);
                    self.watched_generation = 0;
                    Ok(())
                }
                Err(e) => {
                    // Nothing was captured, so hand the apartment back rather than leave the thread in one nobody owns.
                    self.release_com();
                    Err(e)
                }
            }
        }

        /// Returns the next whole 10ms block, or `Ok(None)` when nothing is due within `timeout`. A zero
        /// timeout is a non-blocking "is anything due?" check, how the recorder drains the microphone
        /// without starving the video.
        fn next_buffer(&mut self, timeout: Duration) -> Result<Option<AudioBuffer>> {
            // The read is a COM call into the audio service, from the thread that owns the
            // apartment the objects live in. A read from anywhere else is an access violation,
            // so it is refused here, where it can be named.
            check_apartment(
                "read from",
                self.apartment.as_ref(),
                "WASAPI microphone capture",
            )?;
            let Self {
                source,
                silence_watch,
                watched_generation,
                ..
            } = self;
            let source = source
                .as_mut()
                .context("WASAPI microphone capture is not started")?;
            let deadline = Instant::now() + timeout;

            loop {
                // Take everything the engine has already handed us, placed by its device timestamps so a
                // glitch that loses samples leaves silence rather than pulling the rest of the track early.
                //
                // A microphone that goes away is not an error here: the stream is reopened on the
                // default communications endpoint every `RETRY_INTERVAL` and the outage is silence on
                // the timeline. Unplugging a headset must not end a recording that is still capturing
                // video and the game's audio. Only an error that is not a lost endpoint comes back.
                source.poll(clock_base().elapsed())?;
                if let Some(client) = source.stream() {
                    if source.generation() != *watched_generation {
                        // Each endpoint gets its own watch, armed only if it needs the engine's converter.
                        *watched_generation = source.generation();
                        *silence_watch = ConvertedSilenceWatch::new(client.converting);
                    }
                }

                if let Some(block) = source.next_block() {
                    let buffer = block.buffer;
                    let block_duration = Duration::from_micros(
                        buffer.frames as u64 * 1_000_000 / u64::from(TARGET.sample_rate),
                    );

                    // Diagnostic only: silence with the converter in the path is either a muted microphone or the
                    // reported AUTOCONVERTPCM-delivers-silence failure, and a user whose clip has no voice track
                    // should hear about it. It latches (one warning per session) and never errors or stops capture.
                    // Padding is skipped: it says nothing about what the microphone hears.
                    if !block.filler
                        && silence_watch.observe(block_duration, is_silent_block(&buffer.data))
                    {
                        tracing::warn!(
                            silent_run_secs = silence_watch.silent_run().as_secs_f64(),
                            capture_secs = silence_watch.elapsed().as_secs_f64(),
                            "the microphone has captured only silence, and the default \
                             communications capture endpoint is not natively 48kHz stereo s16, \
                             so the audio engine's converter (AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM) \
                             is in the path. Either the microphone is muted, muted in software, \
                             or disconnected, or this is the failure mode reported on some \
                             Windows 11 builds where an auto-converted stream delivers silence \
                             instead of audio. This is a suspicion, not a confirmed diagnosis: \
                             capture is still running and has NOT been stopped, but the clip's \
                             microphone track may be silent. Check the microphone's mute switch \
                             and its level in Settings > System > Sound > Input, and if the \
                             device is already 48kHz stereo it is the input rather than the \
                             converter."
                        );
                    }
                    return Ok(Some(buffer));
                }

                // A stream that has gone quiet is checked against the endpoint's state inside the
                // packet source (`Client::pull`), ahead of the deadline: the recorder drains this
                // backend with a zero timeout (`pump_once_counted`), which returns before reaching
                // the wait below, so a check placed there would never run in the real capture loop.
                let now = Instant::now();
                if now >= deadline {
                    // A partial block stays buffered until it is complete.
                    return Ok(None);
                }
                let Some(client) = source.stream() else {
                    // No stream to wait on while the microphone is being reopened; silence is padded meanwhile.
                    std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
                    continue;
                };
                // The engine signals this event once per device period, so waiting on it beats sleeping a fixed slice.
                let wait_ms = (deadline - now).as_millis().clamp(1, 200) as u32;
                // SAFETY: `client.event` is a live auto-reset event owned by `self`.
                let signalled = unsafe { WaitForSingleObject(client.event.0, wait_ms) };
                if signalled != WAIT_OBJECT_0 && signalled != WAIT_TIMEOUT {
                    bail!(
                        "waiting on the WASAPI microphone capture event failed (WAIT_EVENT {})",
                        signalled.0
                    );
                }
            }
        }

        fn stop(&mut self) -> Result<()> {
            // Dropping the client stops the stream and closes the event handle.
            self.source = None;
            self.release_com();
            Ok(())
        }
    }

    impl Drop for WasapiMicrophone {
        fn drop(&mut self) {
            // A backend dropped without `stop` must still release the endpoint and the COM apartment it took.
            self.source = None;
            self.release_com();
        }
    }

    impl SourceOpener for MicOpener {
        type Stream = Client;

        fn open(&mut self) -> Result<(Client, String)> {
            // The attempt, before it is made: an open that hangs or fails is otherwise a silence
            // with nothing in the log, and a reopen is the case where that silence is audible.
            tracing::debug!(
                endpoint = if self.device.is_empty() {
                    "the default communications capture endpoint"
                } else {
                    self.device.as_str()
                },
                "opening the microphone stream"
            );
            let client = open_client(&self.device)?;
            self.last_key.clone_from(&client.endpoint);
            Ok((client, self.last_key.clone()))
        }

        fn wanted(&mut self) -> Result<String> {
            Ok(self.last_key.clone())
        }
    }

    /// Open the default communications capture endpoint in shared mode and start it, asking the
    /// engine for the canonical 48kHz stereo s16le format whatever the device's own mix format is.
    ///
    /// `wanted` is the endpoint id to open; empty is the default communications capture
    /// endpoint.
    fn open_client(wanted: &str) -> Result<Client> {
        // SAFETY: the CLSID is the documented one; `None` means no aggregation.
        let enumerator: IMMDeviceEnumerator = unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator)")?
        };
        // SAFETY: the enumerator is alive. eCapture is the data flow that has microphones (eRender would
        // be a speaker), and eCommunications is the role Windows gives the device the user talks into,
        // the one a game's voice chat selects. No loopback flag: this is the endpoint itself.
        let device = if wanted.is_empty() {
            unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications) }.context(
                "IMMDeviceEnumerator::GetDefaultAudioEndpoint(eCapture, eCommunications): this \
                 machine has no default communications microphone. Plug in or enable a \
                 microphone, or pick one as the default input device in Settings > System > \
                 Sound > Input",
            )?
        } else {
            crate::audio_devices::windows_impl::device_by_id(&enumerator, wanted)?
        };
        // Which device it turned out to be: the log names it, because a machine with several
        // input devices cannot be told which one was captured without it. The id is for the
        // log, so a device whose id cannot be read is still a device: the failure is not
        // the microphone's.
        let endpoint = crate::wasapi::device_id(&device)
            .unwrap_or_else(|e| format!("<the endpoint id could not be read: {e:#}>"));
        // SAFETY: IAudioClient is the capture endpoint's documented client interface.
        let audio: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .context("IMMDevice::Activate(IAudioClient)")?;

        // The device's native format is read only to be logged next to the request; the stream is not
        // opened with it. A failure here is still fatal: it is the call that proves the endpoint is usable.
        // SAFETY: the native format is allocated with CoTaskMemAlloc by the audio engine and freed by
        // `MixFormatGuard` when this function returns; the fields are copied out before then.
        let raw_mix =
            unsafe { audio.GetMixFormat() }.map_err(wasapi_call("IAudioClient::GetMixFormat"))?;
        let raw_mix = MixFormatGuard(raw_mix);
        let native = native_format(raw_mix.0);

        // What the engine is asked to deliver: our format, not the device's (AUTOCONVERTPCM below).
        let requested = requested_format();

        // The event is auto-reset: each wait consumes one signal from the engine.
        // SAFETY: no security attributes and no name are needed for a private event.
        let event = unsafe {
            CreateEventW(None, false, false, None)
                .context("CreateEventW for the WASAPI capture event")?
        };
        let event = EventHandle(event);

        // A shared-mode, event-driven capture stream. The buffer must be at least one device period long
        // for the engine to signal the event at all, so ask for the endpoint's default period.
        let mut default_period = 0i64;
        let mut minimum_period = 0i64;
        // SAFETY: both out-parameters are valid.
        unsafe { audio.GetDevicePeriod(Some(&mut default_period), Some(&mut minimum_period)) }
            .map_err(wasapi_call("IAudioClient::GetDevicePeriod"))?;
        let buffer_duration = if default_period > 0 {
            default_period
        } else {
            10 * HNS_PER_MS
        };

        // Deliberately no fallback to the device's own format if this fails: those bytes would be
        // labelled 48kHz stereo and the byte-count timeline would drift against the game track and the
        // video, so refusing is the fail-closed outcome. The error names both the requested and the
        // reported format, which together are the whole diagnosis.
        //
        // What is about to be asked for is logged first: the engine's own format next to the
        // request, so a refusal names both and a reader can see which conversion was asked for.
        tracing::debug!(
            endpoint = endpoint,
            native_sample_rate = native.sample_rate,
            native_channels = native.channels,
            native_sample_format = ?native.encoding,
            requested_sample_rate = TARGET.sample_rate,
            requested_channels = TARGET.channels,
            "asking the audio engine to open the capture endpoint for the microphone"
        );
        // SAFETY: the stream flags are documented shared-mode flags (see `STREAM_FLAGS`); the
        // periodicity argument must be 0 in shared mode, and `requested` stays alive on this stack
        // frame for the duration of the call.
        unsafe {
            audio
                .Initialize(AUDCLNT_SHAREMODE_SHARED, STREAM_FLAGS, buffer_duration, 0, &requested, None)
                .with_context(|| {
                    format!(
                        "IAudioClient::Initialize(microphone, AUTOCONVERTPCM, requested {}Hz stereo \
                         s16): the audio engine would not open the default communications capture \
                         endpoint in that format (the device reports {}Hz/{}ch/{:?} natively). No \
                         fallback format is used: recording the device's own format and labelling \
                         it 48kHz would desynchronise the microphone track from the game track and \
                         the video",
                        TARGET.sample_rate, native.sample_rate, native.channels, native.encoding
                    )
                })?;
            audio
                .SetEventHandle(event.0)
                .map_err(wasapi_call("IAudioClient::SetEventHandle"))?;
        }
        // SAFETY: registered after Initialize, which is the documented order.
        let capture: IAudioCaptureClient = unsafe { audio.GetService() }
            .map_err(wasapi_call("IAudioClient::GetService(IAudioCaptureClient)"))?;
        // SAFETY: the stream is fully configured at this point.
        unsafe { audio.Start() }.map_err(wasapi_call("IAudioClient::Start"))?;
        let (anchor_hns, anchor) = qpc_anchor()?;

        // Which microphone was opened and whether the engine is converting for it: `converting = true`
        // plus audible 48kHz stereo audio in the microphone track is checkable without a debugger, and
        // `engine_upmixes_mono_to_stereo` says the device was mono and the matrixer duplicated it.
        // The endpoint's id says which device it was.
        tracing::info!(
            endpoint = endpoint,
            endpoint_data_flow = "eCapture",
            // The role only chose the device when none was configured; a configured one is
            // opened by its id.
            chosen_by = if wanted.is_empty() {
                "the default communications role"
            } else {
                "its id in [mic] device"
            },
            native_sample_rate = native.sample_rate,
            native_channels = native.channels,
            native_sample_format = ?native.encoding,
            requested_sample_rate = TARGET.sample_rate,
            requested_channels = TARGET.channels,
            requested_sample_format = "s16",
            converting = native.needs_conversion(),
            engine_upmixes_mono_to_stereo = native.channels == 1,
            "WASAPI microphone capture started"
        );
        Ok(Client {
            audio,
            capture,
            device,
            endpoint,
            event,
            converting: native.needs_conversion(),
            anchor_hns,
            anchor,
            last_packet_at: Instant::now(),
            last_state_check: Instant::now(),
        })
    }

    /// Report a stream whose endpoint has gone away as [`SourceGone`], so it is reopened.
    ///
    /// Consulted only when the stream has already gone quiet. A still-active device is not an error
    /// however quiet the room ([`ConvertedSilenceWatch`] only warns about that); an inactive one can
    /// never deliver another packet on this stream, so the stream is given up and a new one opened
    /// when the endpoint is back.
    fn check_endpoint_alive(client: &Client, since_packet: Duration) -> Result<()> {
        // SAFETY: the device is alive for at least as long as `client`, which outlives this call.
        let state =
            unsafe { client.device.GetState() }.map_err(wasapi_call("IMMDevice::GetState"))?;
        if state.0 & DEVICE_STATE_ACTIVE.0 == 0 {
            return Err(SourceGone(format!(
                "the microphone endpoint has delivered no packet for {:.1}s and is no longer \
                 active (DEVICE_STATE {:#x}): it was unplugged, disabled or removed",
                since_packet.as_secs_f64(),
                state.0
            ))
            .into());
        }
        Ok(())
    }

    /// Whether an HRESULT means the endpoint behind this stream is gone.
    ///
    /// `AUDCLNT_E_DEVICE_INVALIDATED` is the reply for a device removed or disabled while streaming;
    /// `AUDCLNT_E_RESOURCES_INVALIDATED` is its sibling for held resources, and
    /// `AUDCLNT_E_SERVICE_NOT_RUNNING` is the audio service restarting. All are permanent for the
    /// stream and recoverable by opening a new one.
    fn is_endpoint_gone(e: &windows::core::Error) -> bool {
        [
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_RESOURCES_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
        ]
        .contains(&e.code())
    }

    /// Context for a WASAPI call, with the unplugged-microphone case spelled out and marked as
    /// [`SourceGone`] so the stream is reopened instead of the recording failing.
    ///
    /// The same HRESULT comes back for a yanked device and for everything else, and reads as a bare
    /// `0x88890004` in a log; a user with a disconnected headset can act on a sentence, not a number.
    fn wasapi_call(what: &'static str) -> impl FnOnce(windows::core::Error) -> anyhow::Error {
        move |e| {
            if is_endpoint_gone(&e) {
                SourceGone(format!(
                    "{what}: the microphone endpoint was removed, unplugged or disabled \
                     (HRESULT {:#010x})",
                    e.code().0
                ))
                .into()
            } else {
                anyhow::Error::new(e).context(what)
            }
        }
    }

    impl PacketSource for Client {
        /// Pull one packet from the capture client and append it to `out`. A packet can be empty, or
        /// flagged silent (its buffer contents are then undefined; only the frame count matters).
        fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
            // SAFETY: the capture client is alive for as long as `self` is.
            let available = unsafe { self.capture.GetNextPacketSize() }
                .map_err(wasapi_call("IAudioCaptureClient::GetNextPacketSize"))?;
            if available == 0 {
                // Nothing has arrived for a while. A live capture endpoint cannot be silent at the API level (a
                // muted microphone is packets flagged silent, not an absence of packets), so ask Windows whether
                // the device is still there. This is the unplugged-headset path and must be neither an endless
                // wait nor a hot loop: it is throttled to one `GetState` per [`DEVICE_STALL`], so asking on
                // every poll costs nothing. A gone endpoint comes back as `SourceGone` and is reopened.
                let since_packet = self.last_packet_at.elapsed();
                if since_packet >= DEVICE_STALL && self.last_state_check.elapsed() >= DEVICE_STALL {
                    self.last_state_check = Instant::now();
                    check_endpoint_alive(self, since_packet)?;
                }
                return Ok(None);
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            let mut qpc_position: u64 = 0;
            // SAFETY: all four are valid out-parameters; the device position is not needed because the
            // QPC position already says when the first frame was recorded.
            unsafe {
                self.capture
                    .GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        None,
                        Some(&mut qpc_position),
                    )
                    .map_err(wasapi_call("IAudioCaptureClient::GetBuffer"))?
            };

            // The buffer must be released however the copy below turns out.
            let copied = append_packet(data, frames, flags, out);
            // SAFETY: `frames` is exactly the count `GetBuffer` just reported.
            let released = unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(wasapi_call("IAudioCaptureClient::ReleaseBuffer"));
            copied?;
            released?;
            self.last_packet_at = Instant::now();

            let timestamp_ok =
                flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc_position != 0;
            Ok(Some(PacketInfo {
                frames: frames as usize,
                time: timestamp_ok
                    .then(|| qpc_to_shared(qpc_position, self.anchor_hns, self.anchor))
                    .flatten(),
                discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
            }))
        }
    }

    /// Append one packet's samples to `out`.
    ///
    /// The stream was opened as 48kHz stereo s16le, so the engine already produced exactly the bytes
    /// the pipeline wants (a mono device was up-mixed 1->2 by its matrixer): a copy, not a conversion.
    /// If the engine had not done so, the packet byte count would stop matching the 10ms blocks
    /// `next_buffer` slices and the derived timeline, which is why a failed `Initialize` is a loud
    /// error instead of a fallback.
    fn append_packet(data: *mut u8, frames: u32, flags: u32, out: &mut Vec<u8>) -> Result<()> {
        let frames = frames as usize;
        if frames == 0 {
            return Ok(());
        }
        let byte_len = frames
            .checked_mul(BYTES_PER_FRAME)
            .context("the WASAPI microphone packet size overflowed")?;

        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            // A silent packet's buffer is undefined; write the silence it stands for. A muted microphone
            // arrives exactly here, which is what lets the silence watch see it.
            out.resize(out.len() + byte_len, 0);
            return Ok(());
        }
        if data.is_null() {
            bail!("WASAPI returned a {frames}-frame microphone packet with a null data pointer");
        }
        // SAFETY: GetBuffer reports `frames` frames of `BYTES_PER_FRAME` bytes each starting at `data`,
        // and the buffer stays valid until ReleaseBuffer, which happens after this function returns.
        let packet = unsafe { std::slice::from_raw_parts(data, byte_len) };
        out.extend_from_slice(packet);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::Foundation::E_FAIL;

        #[test]
        fn a_removed_microphone_is_a_lost_source_so_the_recording_survives_it() {
            // `RecoveringSource` reopens on `SourceGone` and fails the recording on anything else, so
            // this mapping is what decides whether unplugging a headset ends the session.
            for code in [
                AUDCLNT_E_DEVICE_INVALIDATED,
                AUDCLNT_E_RESOURCES_INVALIDATED,
                AUDCLNT_E_SERVICE_NOT_RUNNING,
            ] {
                let err = wasapi_call("IAudioCaptureClient::GetBuffer")(code.into());
                assert!(
                    err.downcast_ref::<SourceGone>().is_some(),
                    "{code:?} must be a lost source, got: {err:#}"
                );
            }
        }

        #[test]
        fn another_failure_is_still_an_error_that_ends_the_recording() {
            // Only a lost endpoint is recoverable. A failure with no known remedy is not hidden
            // behind a silent track: it comes back as an error.
            let err = wasapi_call("IAudioCaptureClient::GetBuffer")(E_FAIL.into());
            assert!(err.downcast_ref::<SourceGone>().is_none(), "got: {err:#}");
        }
    }
}

#[cfg(windows)]
pub use wasapi_impl::WasapiMicrophone;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{is_silent_block, ConvertedSilenceWatch, CONVERTED_SILENCE_THRESHOLD};

    /// One 10ms block of the canonical format, as a frame count.
    const BLOCK_FRAMES: usize = 480;

    fn mic(block_ms: u64) -> StubMicrophone {
        StubMicrophone::new(AudioFormat::default(), block_ms)
    }

    #[test]
    fn the_stub_microphone_emits_the_canonical_format_in_whole_blocks() {
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        let block = mic
            .next_buffer(Duration::from_secs(1))
            .expect("poll the stub")
            .expect("the block at pts 0 is due immediately");

        // The pipeline declares its audio input as `-ar 48000 -ac 2` s16le and derives the timeline from
        // the byte count, so these three assertions are the whole contract the recorder relies on.
        assert_eq!(block.format, AudioFormat::default(), "48kHz stereo");
        assert_eq!(block.frames, BLOCK_FRAMES, "10ms of 48kHz");
        assert_eq!(
            block.data.len(),
            block.frames * block.format.channels as usize * 2,
            "byte length must be exactly frames x channels x 2"
        );
        assert_eq!(
            block.data.len(),
            AudioFormat::default().bytes_per_10ms(),
            "1920 bytes"
        );
        assert_eq!(
            block.pts,
            Duration::ZERO,
            "the first block is stamped at the run's origin"
        );
    }

    #[test]
    fn with_signal_scales_the_waveform_and_zero_is_silence() {
        let mut loud = mic(10).with_signal(1_234);
        let mut silent = mic(10).with_signal(0);
        loud.start().expect("start the loud stub");
        silent.start().expect("start the silent stub");
        let loud_block = loud
            .next_buffer(Duration::from_secs(1))
            .expect("poll the loud stub")
            .expect("a block is due");
        let silent_block = silent
            .next_buffer(Duration::from_secs(1))
            .expect("poll the silent stub")
            .expect("a block is due");

        let peak = loud_block
            .data
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
            .max()
            .expect("a block has samples");
        assert_eq!(
            peak, 1_234,
            "the waveform's peak is the amplitude that was asked for"
        );
        assert_eq!(
            silent_block.data.len(),
            silent_block.frames * silent_block.format.channels as usize * 2,
            "a silent block keeps the format invariant"
        );
        assert!(
            is_silent_block(&silent_block.data),
            "with_signal(0) is how a test asks for a muted microphone"
        );
    }

    #[test]
    fn the_default_stub_microphone_never_trips_the_silence_watch() {
        // A non-silent stub must keep the watch disarmed even over more than the threshold's worth of blocks.
        let mut mic = mic(10);
        mic.start().expect("start the stub microphone");
        let mut watch = ConvertedSilenceWatch::new(true);
        for buffer in mic.drain_for(CONVERTED_SILENCE_THRESHOLD + Duration::from_secs(1)) {
            assert!(!is_silent_block(&buffer.data));
            assert!(
                !watch.observe(Duration::from_millis(10), is_silent_block(&buffer.data)),
                "real audio must never warn"
            );
        }
        assert!(!watch.has_warned());
    }

    #[test]
    fn next_buffer_before_start_is_an_error() {
        let mut mic = mic(10);
        let err = mic
            .next_buffer(Duration::from_secs(1))
            .expect_err("the stub has no schedule before start");
        assert!(err.to_string().contains("not started"), "got: {err}");
    }

    #[test]
    fn the_microphone_backend_and_the_system_audio_backend_agree_on_the_canonical_format() {
        // Both audio tracks are declared the same way (`-ar 48000 -ac 2`) and sliced by frame index, so a
        // format mismatch between the two backends would slide the voice track against the game track.
        assert_eq!(
            MICROPHONE_FORMAT,
            AudioFormat::default(),
            "the microphone backend must publish the crate's canonical format"
        );

        #[cfg(windows)]
        {
            assert_eq!(
                crate::wasapi::TARGET,
                MICROPHONE_FORMAT,
                "the loopback backend and the microphone backend must target the same format"
            );
        }

        #[cfg(not(windows))]
        {
            // Off Windows the system-audio backend is the stub; this is the one place the two selections can
            // be compared on live objects: same format, frame count and byte count.
            let mut system = crate::platform::default_audio_backend(AudioFormat::default())
                .expect("the system-audio backend selection must succeed on this host");
            system.start().expect("start the system-audio backend");
            let system_block = system
                .next_buffer(Duration::from_secs(1))
                .expect("poll the system-audio backend")
                .expect("a block is due");
            let mut microphone = mic(10);
            microphone.start().expect("start the stub microphone");
            let mic_block = microphone
                .next_buffer(Duration::from_secs(1))
                .expect("poll the stub microphone")
                .expect("a block is due");
            assert_eq!(
                system_block.format, mic_block.format,
                "one canonical format, one track shape"
            );
            assert_eq!(system_block.frames, mic_block.frames);
            assert_eq!(system_block.data.len(), mic_block.data.len());
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn the_microphone_backend_is_unavailable_on_this_platform_with_a_clear_error() {
        // An Err, not an Ok and not a panic: silent success would make the recorder believe it is capturing a voice track.
        let err = match microphone_backend("") {
            Ok(_) => panic!("this host has no microphone backend: an Ok would be a silent success"),
            Err(e) => e,
        };
        let message = err.to_string();
        assert!(
            message.contains("only implemented on Windows"),
            "the error must say the microphone backend is Windows-only; got: {message}"
        );
    }
}
