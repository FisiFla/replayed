//! What this machine's hardware video encoders are, and whether they will take a GPU texture.
//!
//! The hybrid path rests on one assumption a development host cannot check: that a hardware
//! encoder MFT exists here and accepts the D3D11 device the capture backend already owns, which
//! avoids reading 33.2 MB back into system memory per frame (~97% of the 4K pipeline's work).
//!
//! So this asks the machine, in the shape of `xtask probe`: it starts Media Foundation,
//! enumerates the hardware encoder MFTs, and runs the handshake the encoder will run
//! (`MFT_MESSAGE_SET_D3D_MANAGER` with a real device manager over a real D3D11 device of the
//! capture path's kind). No window is opened, nothing is captured and no input is synthesised,
//! which matters because the machine runs an anti-cheat.
//!
//! # When the encoder changes its type
//!
//! Media Foundation's encoders may change their output type mid-stream, and they report it by
//! refusing to produce output: `ProcessOutput` answers `MF_E_TRANSFORM_STREAM_CHANGE`
//! (0xC00D6D61, "a stream change has occurred. Output cannot be produced until the streams have
//! been renegotiated"). The caller must then set a new output type — one the encoder itself
//! offers — and set the input type again with it, or the encoder produces nothing for the rest of
//! the stream.
//!
//! Both the recording and the throughput probe take their output through
//! [`MftEncoder::take_output`], so both renegotiate, and there is one implementation of it rather
//! than two that can disagree.

use crate::ffmpeg::{VideoChunk, MFT_TIMEOUT};
use crate::throughput::frame_time;
use crate::timeline::{capture_pts, FramePairing};
use crate::{
    choose_output_type, classify_output_failure, empty_after_renegotiation, type_change_refusal,
    OutputFailure, OutputTypeSpec, RateControl, Vendor, VideoCodec,
};
use anyhow::{bail, Context, Result};
use replayed_capture::GpuTexture;
use std::time::{Duration, Instant};
use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_DESC,
    DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_PeakConstrainedVBR,
    eAVEncH264VLevel5_1, eAVEncH264VLevel5_2, eAVEncH264VProfile_High, eAVEncH265VLevel4_1,
    eAVEncH265VLevel5, eAVEncH265VLevel5_1, eAVEncH265VProfile_Main_420_10,
    eAVEncH265VProfile_Main_420_8, CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFShutdown, IMFTransform, MEError,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample,
    MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx, MFT_ENUM_HARDWARE_URL_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_HEVC_ES, MFVideoFormat_NV12, MFVideoFormat_P010, MFVideoFormat_RGB32,
    MFVideoFormat_YUY2, MFVideoInterlace_Progressive, MFSHUTDOWN_COMPLETED, MFSTARTUP_FULL,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_CATEGORY_VIDEO_PROCESSOR, MFT_ENUM_FLAG_HARDWARE,
    MFT_INPUT_STATUS_ACCEPT_DATA, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STATUS_SAMPLE_READY, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, MFCreateAttributes, MFTEnum2, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_ENUM_ADAPTER_LUID,
};

/// What an encoder asked for, when its events were drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// `METransformNeedInput`: it will take a frame now.
    Input,
    /// `METransformDrainComplete`: everything the encoder owed has been produced.
    Drained,
    /// Nothing queued: what a poll looks like when there is no work.
    Quiet,
}

/// One event from an encoder's queue, taken without waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderEvent {
    /// `METransformNeedInput`: it will take a frame now.
    NeedInput,
    /// `METransformHaveOutput`: it has finished a frame.
    HaveOutput,
    /// `METransformDrainComplete`: the drain [`MftEncoder::finish`] asked for is over, and every
    /// frame it owed has been announced (each `METransformHaveOutput` precedes it).
    DrainComplete,
    /// Anything else the encoder announces (a marker, a type-change notice): not this exchange's
    /// business. An *error* event is not in this bucket: [`MftEncoder::next_event`] turns it into
    /// an `Err`, because an encoder that says it failed has failed.
    Other,
}
use windows::Win32::System::Com::CoTaskMemFree;

/// One hardware video encoder this machine offers, and what it said when asked for the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareEncoder {
    /// The MFT's own name for itself (`MFT_FRIENDLY_NAME_Attribute`).
    pub name: String,
    /// The adapter the handshake was run on: the one this MFT was enumerated for, and the one the
    /// D3D11 device it was handed lives on.
    ///
    /// A hardware encoder can only be handed a device on its own adapter, so a handshake is only a
    /// question about an (encoder, adapter) pair. The probe used to ask every encoder over a
    /// null-adapter device, which is the default adapter, and the Quick Sync encoders on a machine
    /// whose default is NVIDIA answered `E_INVALIDARG` to a device that was never theirs: a
    /// verdict on the pairing that read as a verdict on Quick Sync.
    pub adapter: String,
    /// `MFT_ENUM_HARDWARE_URL_Attribute`, which distinguishes a real hardware MFT from a software
    /// one enumerable under the hardware flag.
    pub hardware_url: Option<String>,
    /// `MF_TRANSFORM_ASYNC`. An asynchronous MFT refuses to be configured until unlocked: NVIDIA's
    /// answers a locked handshake with `MF_E_TRANSFORM_ASYNC_LOCKED` (`0xC00D6D77`). [`ask_one`]
    /// unlocks first, so `accepts_d3d11` is about D3D11 and not the async contract.
    pub asynchronous: bool,
    /// The question this module exists for: did `MFT_MESSAGE_SET_D3D_MANAGER` succeed?
    pub accepts_d3d11: bool,
    /// The input subtypes this MFT will accept, named where this build knows the name.
    ///
    /// This decides the shape of the chain. WGC delivers BGRA8; if the encoder takes only NV12,
    /// something must convert, and the place for that is a GPU Video Processor MFT, not a CPU loop.
    /// If it accepts BGRA directly, the texture goes straight in. Empty when the MFT could not be
    /// asked (see `refusal`).
    pub input_subtypes: Vec<String>,
    /// Which of the formats this pipeline might feed it the MFT actually took, by being asked to
    /// accept one.
    ///
    /// This is the answer; `input_subtypes` is only the enumeration attempt. A hardware encoder
    /// refuses to enumerate its types (`GetInputAvailableType` came back empty even after the D3D11
    /// handshake) but accepts a `SetInputType` it likes, the same call the encoder makes. Names come
    /// from [`CANDIDATE_INPUTS`].
    pub accepts: Vec<String>,
    /// Why no full answer could be had: a failed handshake, activation, unlock or type negotiation,
    /// in the driver's own words.
    pub refusal: Option<String>,
}

/// Every hardware encoder for `codec` on this machine, asked whether it takes a D3D11 device manager
/// **over a device on its own adapter**.
///
/// One adapter at a time: the adapter's encoders are enumerated for its LUID (`MFTEnum2` with
/// `MFT_ENUM_ADAPTER_LUID`) and each is handed a device created on that adapter. An encoder is not
/// asked about a device that is somebody else's, because the answer to that question is always
/// "no" and says nothing about the encoder (see [`HardwareEncoder::adapter`]).
///
/// Errors only when the question could not be asked (Media Foundation would not start, the
/// enumeration failed). No hardware encoder is an empty list, an answer rather than a failure. An
/// adapter that cannot make a device at all, or whose encoders cannot be created, contributes the
/// reason as a row instead of silently dropping out.
pub fn probe_hardware_encoders(codec: VideoCodec) -> Result<Vec<HardwareEncoder>> {
    // Started once and shut down when the guard goes, on every path out and after everything the
    // probe created has been released (see [`MfSession`]).
    let _session = MfSession::start()?;
    probe_started_encoders(codec)
}

fn probe_started_encoders(codec: VideoCodec) -> Result<Vec<HardwareEncoder>> {
    // No type filter, deliberately. Filtering by output subtype can hide an encoder whose
    // registration does not match its name, and `MFT_ENUM_FLAG_SORTANDFILTER` filters against a
    // preference list that means nothing without a type to prefer for. Asking for HEVC returned the
    // two H.264 encoders and one Quick Sync encoder, while the unfiltered list held "NVIDIA HEVC
    // Encoder MFT" and "Intel(R) Hardware H265 Encoder MFT". The negotiation below is what decides, so
    // the enumeration is complete and each encoder says for itself what it will do.
    //
    // The device is the capture path's kind (BGRA | VIDEO_SUPPORT) but on each adapter in turn, and
    // one device and one manager serve every encoder of that adapter, as the encoder will do.
    let mut probed = Vec::new();
    for adapter in hardware_adapters()? {
        let device = match create_device_on(
            Some(&adapter.adapter),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            "BGRA_SUPPORT | VIDEO_SUPPORT",
        ) {
            Ok(device) => device,
            // Not fatal and not silent: an adapter that makes no device cannot host an encoder, and
            // the machine's answer says so.
            Err(err) => {
                tracing::debug!("no device on {}: {err:#}", adapter.name);
                continue;
            }
        };
        let manager = create_device_manager(&device)?;
        let candidates = enumerate_activates_for(&adapter.luid)
            .with_context(|| format!("MFTEnum2(video encoders) for {}", adapter.name))?;
        for candidate in &candidates {
            probed.push(ask_one(candidate, &manager, codec, &adapter.name));
        }
    }
    // "No hardware encoder" is an answer, not an error: an empty list.
    Ok(probed)
}

/// One adapter DXGI lists that is a real GPU (the software rasterizer is not one).
struct HardwareAdapter {
    adapter: IDXGIAdapter1,
    luid: LUID,
    name: String,
}

/// Every hardware adapter DXGI lists, in its order.
fn hardware_adapters() -> Result<Vec<HardwareAdapter>> {
    // SAFETY: creating the factory has no preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut adapters = Vec::new();
    let mut index = 0u32;
    // `EnumAdapters1` fails rather than returning null once the index runs off the end.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        // SAFETY: `GetDesc1` is this adapter's own method.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        adapters.push(HardwareAdapter {
            luid: desc.AdapterLuid,
            name: wide_name(&desc.Description),
            adapter,
        });
    }
    Ok(adapters)
}

/// Ask one enumerated MFT for the handshake, and describe it either way.
fn ask_one(
    activate: &IMFActivate,
    manager: &IMFDXGIDeviceManager,
    codec: VideoCodec,
    adapter: &str,
) -> HardwareEncoder {
    let name = attribute_string(activate, &MFT_FRIENDLY_NAME_Attribute)
        .unwrap_or_else(|| "<unnamed MFT>".to_string());
    let hardware_url = attribute_string(activate, &MFT_ENUM_HARDWARE_URL_Attribute);
    let asynchronous = attribute_u32(
        activate,
        &windows::Win32::Media::MediaFoundation::MF_TRANSFORM_ASYNC,
    )
    .map(|v| v != 0)
    .unwrap_or(false);

    // SAFETY: `ActivateObject` is this `IMFActivate`'s own method; the transform it returns is
    // shut down at the end of this function.
    let activated: Result<IMFTransform> =
        unsafe { activate.ActivateObject() }.context("IMFActivate::ActivateObject(IMFTransform)");
    let transform = match activated {
        Ok(transform) => transform,
        Err(err) => {
            return HardwareEncoder {
                name,
                adapter: adapter.to_string(),
                hardware_url,
                asynchronous,
                accepts_d3d11: false,
                input_subtypes: Vec::new(),
                accepts: Vec::new(),
                refusal: Some(format!("could not be activated: {err:#}")),
            }
        }
    };

    // An asynchronous MFT will not be configured until the caller says it understands the async
    // contract, and every hardware encoder on the test machine is asynchronous. Without this the
    // handshake comes back `MF_E_TRANSFORM_ASYNC_LOCKED`, which reads like a refusal of D3D11 and is
    // not one: the probe would report "no hardware encoder takes a GPU texture" on a machine that has one.
    if asynchronous {
        // SAFETY: `GetAttributes` is this transform's own method, and the handle it returns
        // borrows the transform, which is alive here.
        let unlocked = match unsafe { transform.GetAttributes() } {
            Ok(attributes) => {
                // SAFETY: setting an attribute on the transform's own attribute store, which is
                // how an async MFT is unlocked.
                unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            }
            Err(err) => Err(err),
        };
        if let Err(err) = unlocked {
            return HardwareEncoder {
                name,
                adapter: adapter.to_string(),
                hardware_url,
                asynchronous,
                accepts_d3d11: false,
                input_subtypes: Vec::new(),
                accepts: Vec::new(),
                refusal: Some(format!("could not be unlocked for asynchronous use: {err}")),
            };
        }
    }

    // The handshake itself. `MFT_MESSAGE_SET_D3D_MANAGER` carries the manager as its `ulParam`, so
    // the raw pointer crosses as a `usize`; that is the documented protocol.
    // SAFETY: `manager` outlives the call, and the parameter is the manager's own interface
    // pointer, which is what the message requires.
    let handshake =
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) };
    // Detach the manager before letting the transform go: the contract is that the MFT is told when
    // it no longer holds one (`ulParam = 0`). The device manager outlives the transform, so leaving
    // the association dangling would leak the association.
    // SAFETY: the transform is live and the parameter is the documented detach value.
    unsafe {
        let _ = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
    }

    let input_subtypes = input_subtypes(&transform);
    // Asked at a modest size: the answer is about the format, and 640x480 says as much as 3840x2160
    // without depending on the display. A negotiation failure is a finding (`refusal`), not a panic.
    let (accepts, negotiation) = match accepted_subtypes(&transform, (640, 480), codec) {
        Ok(accepts) => (accepts, None),
        Err(err) => (Vec::new(), Some(format!("{err:#}"))),
    };
    // The transform is released at the end of this function; the attribute query above is the last
    // thing it is needed for.
    match handshake {
        Ok(()) => HardwareEncoder {
            name,
            adapter: adapter.to_string(),
            hardware_url,
            asynchronous,
            accepts_d3d11: true,
            input_subtypes,
            accepts,
            refusal: negotiation,
        },
        Err(err) => HardwareEncoder {
            name,
            adapter: adapter.to_string(),
            hardware_url,
            asynchronous,
            accepts_d3d11: false,
            input_subtypes,
            accepts,
            refusal: Some(match negotiation {
                Some(note) => format!("refused MFT_MESSAGE_SET_D3D_MANAGER: {err}; and {note}"),
                None => format!("refused MFT_MESSAGE_SET_D3D_MANAGER: {err}"),
            }),
        },
    }
}

/// The frame rate the diagnostic probes negotiate, in both directions. The recording path declares
/// the configured rate instead (see [`h264_level`]).
///
/// One rate for both types: an output type at 30fps and an input type at 60 were refused with
/// `MF_E_INVALIDMEDIATYPE` ("invalid, inconsistent, or not supported"); neither rate was wrong
/// alone.
const FRAME_RATE: (u32, u32) = (30, 1);

/// The H.264 level for `size` at `fps`: 5.1 where it fits, 5.2 above it.
///
/// A rate the declared level cannot carry is refused, and 4K at 60 exceeds 5.1 (983,040
/// macroblocks a second; 4K60 is 1,944,000), so the level follows the rate actually declared.
/// 5.2 (2,073,600) carries 4K60, the most this pipeline is configured for.
fn h264_level(size: (u32, u32), fps: u32) -> u32 {
    let macroblocks = u64::from(size.0.div_ceil(16)) * u64::from(size.1.div_ceil(16));
    if macroblocks * u64::from(fps) > 983_040 {
        eAVEncH264VLevel5_2.0 as u32
    } else {
        eAVEncH264VLevel5_1.0 as u32
    }
}

/// The input subtypes this pipeline may offer an encoder for a texture of `format`.
///
/// **A subtype is only offered when it describes the texture that will arrive.** The encoder is
/// handed the captured texture as it is, and nothing here converts: there is no colour-space step
/// between capture's BGRA surface and the transform. `accepts_subtype` asks whether the MFT will
/// take a *media type*, not the texture that comes later, so an encoder that lists `NV12` was
/// opened on `NV12`, accepted it, and was then fed BGRA at `ProcessInput`, where it either refuses
/// every frame with nothing to say why or reads one plane of a BGRA surface as luma. Offering it
/// only for an NV12 texture (which no capture path produces today, but which the probe can build)
/// turns "an NV12-only MFT" from a recording with garbage or no video into a refusal at open,
/// which `encode.zero_copy = auto` answers by taking the ffmpeg path. The GPU conversion that
/// would make NV12-only encoders usable (a D3D11 video processor between capture and encoder) is
/// not here; `docs/verification-status.md` records which encoders on the test machine need it.
///
/// * `BGRA` (and `None`, which means "what capture delivers"): `RGB32` first, then `ARGB32`. They
///   are the same bytes under two names, and which one an encoder takes differs per vendor (the
///   NVIDIA MFTs refuse `RGB32` and accept `ARGB32`; see `docs/verification-status.md`).
/// * `NV12`: `NV12` only.
/// * Anything else: nothing, so an HDR or 10-bit surface is refused instead of mislabelled.
fn candidate_inputs(
    format: Option<DXGI_FORMAT>,
) -> Vec<(&'static windows::core::GUID, &'static str)> {
    match format.unwrap_or(DXGI_FORMAT_B8G8R8A8_UNORM) {
        DXGI_FORMAT_B8G8R8A8_UNORM => CANDIDATE_INPUTS
            .iter()
            .copied()
            .filter(|(subtype, _)| **subtype != MFVideoFormat_NV12)
            .collect(),
        DXGI_FORMAT_NV12 => CANDIDATE_INPUTS
            .iter()
            .copied()
            .filter(|(subtype, _)| **subtype == MFVideoFormat_NV12)
            .collect(),
        _ => Vec::new(),
    }
}

/// Every input subtype this build knows the name of, in the order a *diagnostic* asks about them.
///
/// This is what [`HardwareEncoder::accepts`] reports, so a person can see which formats an MFT
/// takes at all. It is **not** what the recording offers: see [`candidate_inputs`], which keeps
/// only the ones that describe the texture on the way in. `NV12` is listed because knowing that an
/// encoder takes it is the information a future GPU conversion step needs.
const CANDIDATE_INPUTS: [(&windows::core::GUID, &str); 3] = [
    (
        &MFVideoFormat_RGB32,
        "RGB32 (BGRA byte order — what WGC delivers)",
    ),
    (&MFVideoFormat_ARGB32, "ARGB32"),
    (&MFVideoFormat_NV12, "NV12"),
];

/// Which candidate input formats this transform will actually take.
///
/// Asked with `SetInputType` rather than read from an enumeration, because an asynchronous
/// hardware MFT will not enumerate and accepting a type is the call the encoder will make, with
/// a media type built the same way.
fn accepted_subtypes(
    transform: &IMFTransform,
    size: (u32, u32),
    codec: VideoCodec,
) -> Result<Vec<String>> {
    // The output type first: an encoder will not accept an input type until it knows what it
    // produces (Media Foundation negotiates the output side first), and an input attempt before that
    // is refused for the ordering rather than the format. Asking for the input alone reported "takes
    // no candidate input format" on an encoder that has one.
    set_output_type(transform, size, codec, PROBE_BITRATE_KBPS, FRAME_RATE.0)
        .context("setting the output type, which must come before the input")?;

    let mut accepted = Vec::new();
    let mut refusals = Vec::new();
    for (subtype, name) in CANDIDATE_INPUTS {
        match accepts_subtype(transform, subtype, size) {
            Ok(()) => accepted.push(name.to_string()),
            Err(err) => refusals.push(format!("{name}: {err:#}")),
        }
    }
    if accepted.is_empty() {
        // Every candidate refused: the driver's own words for each distinguish "wrong format" from
        // "wrong ordering" from "wrong size".
        bail!(
            "set an output type but no candidate input type was accepted: {}",
            refusals.join("; ")
        );
    }
    Ok(accepted)
}

/// The media subtype an encoder must be able to produce for this codec.
///
/// [`MFTEnumEx`]'s fourth argument is the type an encoder produces, not consumes. Pinned to
/// `MFVideoFormat_H264` it could never return an HEVC encoder, and the probe reported none while
/// `ffmpeg -encoders` listed `hevc_nvenc` and a 4K60 `hevc_nvenc` encode ran clean.
fn output_subtype(codec: VideoCodec) -> windows::core::GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
    }
}

/// The bitrate the probes negotiate with, in kbit/s.
///
/// The probes ask what formats and sizes an encoder takes, not the rate, so this avoids a
/// parameter through four public functions. It is the example config's number, so a verdict
/// describes the default configuration; the recording path passes the configured value, which
/// decides a clip's size.
const PROBE_BITRATE_KBPS: u32 = 20_000;

/// A size small enough that this encoder will accept it as a first configuration (see
/// [`set_output_type`]; a workaround for measured behaviour).
const WARM_UP_SIZE: (u32, u32) = (640, 480);

/// Configure the encoder's output, warming an HEVC encoder at a small size first.
///
/// The warming is measured. This encoder refuses the pipeline's output type at 3840x2160 as the
/// first thing it is asked (`0xC00D6D76`, `MF_E_UNSUPPORTED_D3D_TYPE`) and accepts it once it
/// has accepted a smaller one:
///
/// ```text
/// 640x480 -> ACCEPTED   720p -> ACCEPTED   1080p -> ACCEPTED
/// 1440p   -> ACCEPTED   1800p -> ACCEPTED   4K -> ACCEPTED, twice
/// ```
///
/// Asking for 4K first in the same process is refused, and the refusal is sticky: an encoder that
/// just refused 4K goes on to refuse 640x480, so a retry does not help and being configured small
/// first does. `probe_size_ceiling` re-checks this. The H.264 encoder needs none of it.
fn set_output_type(
    transform: &IMFTransform,
    size: (u32, u32),
    codec: VideoCodec,
    bitrate_kbps: u32,
    fps: u32,
) -> Result<()> {
    if codec == VideoCodec::Hevc && size != WARM_UP_SIZE {
        // Best effort: the type that matters is the one below; an encoder that needs no warming accepts both.
        let _ = set_output_type_once(transform, WARM_UP_SIZE, codec, bitrate_kbps, fps);
    }
    set_output_type_once(transform, size, codec, bitrate_kbps, fps)
}

/// One attempt at configuring the output, with no warming. This is what makes the encoder
/// willing to talk about its input at all.
fn set_output_type_once(
    transform: &IMFTransform,
    size: (u32, u32),
    codec: VideoCodec,
    bitrate_kbps: u32,
    fps: u32,
) -> Result<()> {
    // SAFETY: `MFCreateMediaType` returns an empty type this function owns.
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    // The one place the codec matters. Profile and level are load-bearing at 4K (see the note below)
    // and per codec: an H.265 profile number handed to an H.264 encoder is a value it does not know.
    //
    // `Main_420_8` because WGC hands over 8-bit RGB, so Main10 would describe a stream nothing here
    // produces. HEVC level 5.1 carries 4K at 60; H.264's level depends on the rate (`h264_level`).
    let (subtype, profile, level) = match codec {
        VideoCodec::H264 => (
            output_subtype(VideoCodec::H264),
            eAVEncH264VProfile_High.0 as u32,
            h264_level(size, fps),
        ),
        VideoCodec::Hevc => (
            output_subtype(VideoCodec::Hevc),
            eAVEncH265VProfile_Main_420_8.0 as u32,
            eAVEncH265VLevel5_1.0 as u32,
        ),
    };

    // SAFETY: every call sets an attribute on a media type this function owns.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        // The rate the frames really arrive at. Declared at 30 while fed 60, the encoder budgeted
        // `bitrate / 30` a frame and wrote twice the configured rate (see `pin_bitrate`).
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // A bitrate is required for a rate-controlled encoder, and it is the configured
        // `encode.bitrate_kbps`: a token 2 Mbit/s at 4K is not enough for the encoder to accept the
        // size at all. It is also the rate the clip is really made at on this path, since ffmpeg only
        // copies what comes out of here.
        media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1_000))?;
        // The profile and level are the load-bearing attributes at 4K. Without them this MFT refuses
        // every input format at 3840x2160, including NV12 which it encodes for ffmpeg every day, because
        // it cannot assume the level needed to carry 4K@60. Measured without them: 0xC00D36B4 for ARGB32
        // and NV12, 0xC00D36BD for RGB32.
        media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
        media_type.SetUINT32(&MF_MT_MPEG2_LEVEL, level)?;
        // The GOP is the MFT's to set, here. `-force_key_frames` cannot apply to a stream ffmpeg only
        // copies, so nothing downstream can impose the segment boundaries the replay path depends on.
        // A keyframe a second (the segment length) gives one fragment per segment, which is what
        // `MemoryRingBuffer` parses and the lossless clip path cuts on. The HEVC MFT measured does not
        // follow this (it keyframes every 2 s), so `encode_loop` also forces one a second.
        media_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps)?;
    }
    // SAFETY: setting the output type on a transform that was just unlocked and given its device
    // manager, with a media type that outlives the call.
    // The bitrate is in the message so a rate refusal can be told from a size refusal.
    unsafe { transform.SetOutputType(0, &media_type, 0) }
        .with_context(|| format!("SetOutputType({codec:?}, {bitrate_kbps} kbps)"))
}

/// Set one `ICodecAPI` value and report it: set, or refused in the encoder's own words.
///
/// Reporting matters because a rate this pipeline asks for and an encoder that declines to be
/// told look identical from outside, except as a clip whose size ignores `encode.bitrate_kbps`.
fn set_codec_api_value(api: &ICodecAPI, key: &windows::core::GUID, value: u32, what: &str) {
    // SAFETY: `SetValue` is the interface's own method on a transform this process owns; the
    // `VT_UI4` variant is created here and outlives the call.
    // `windows` 0.62 dropped the safe `VARIANT` wrapper, so the `VT_UI4` payload is built by
    // hand. A plain integer owns nothing, so there is nothing to `VariantClear`.
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};
    let variant = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    };
    match unsafe { api.SetValue(key, &variant) } {
        Ok(()) => tracing::info!("the encoder accepted {what} = {value}"),
        Err(err) => tracing::warn!(
            "the encoder refused {what} = {value} ({err}); it is rate-controlling from its own \
             default, so a clip's size will not follow encode.bitrate_kbps"
        ),
    }
}

/// How far above its mean a variable-bitrate encode may peak: half as much again.
///
/// A choice, and a small one: enough headroom for the encoder to spend on a busy scene (the point
/// of VBR), little enough that a clip's size still follows the configured rate to within the
/// tolerance the CBR measurements established (§24).
const VBR_PEAK_FACTOR_PERCENT: u64 = 150;

/// The `CODECAPI_AVEncCommonMaxBitRate` for a variable-bitrate encode with this mean, in bit/s.
fn vbr_ceiling(mean_bits: u32) -> u32 {
    (u64::from(mean_bits) * VBR_PEAK_FACTOR_PERCENT / 100).min(u64::from(u32::MAX)) as u32
}

/// Pin the rate through the encoder's `ICodecAPI` instead of only asking on the output type.
///
/// `MF_MT_AVG_BITRATE` is what the stream is described as, and every MFT measured treats it as a
/// target. Asking for 20,000 kbps wrote 39,564,433 bit/s with the H.264 MFT and 35,747,208 with
/// the HEVC one (1.98x and 1.79x) while the types declared 30fps for a 60fps feed; declaring the
/// real rate brought both to within 10% of the target (docs/verification-status.md §24).
/// `CODECAPI_AVEncCommonMeanBitRate` is the number those encoders
/// rate-control from, and `CODECAPI_AVEncCommonRateControlMode` is what "mean" means to them (an
/// average to aim near, roughly VBR, or a rate to hold). The mode is set first, then the rate,
/// the order an encoder expects.
///
/// **The mode is what the user asked for, within what this surface can express.**
/// `CODECAPI_AVEncCommonRateControlMode` has constant bitrate (`CBR`) and the variable-bitrate
/// family; the variable mode here is `PeakConstrainedVBR`, the one that pairs with the maximum
/// bitrate and the buffer size this function also pins. The constant-quality mode the MFT also
/// has needs a QP value this app does not carry, so it is not reachable from here: asking for it
/// falls back to constant bitrate with a warning, rather than a recording that claims a
/// quality it did not get.
///
/// An MFT with no `ICodecAPI` is not a failure (the recording is made at the encoder's default)
/// but it is said out loud rather than inferred from a file's size.
fn pin_bitrate(transform: &IMFTransform, bitrate_kbps: u32, mode: RateControl) {
    let api: ICodecAPI = match transform.cast() {
        Ok(api) => api,
        Err(err) => {
            tracing::debug!(
                "this encoder has no ICodecAPI ({err}): it is told the rate on the output type \
                 and nothing else, so the clip's size follows its own default"
            );
            return;
        }
    };
    let (mft_mode, what) = match mode {
        RateControl::Cbr => (eAVEncCommonRateControlMode_CBR, "rate control mode (CBR)"),
        RateControl::Vbr => (
            eAVEncCommonRateControlMode_PeakConstrainedVBR,
            "rate control mode (peak-constrained VBR)",
        ),
        RateControl::Quality => {
            // Not reachable from this surface: the MFT's constant-quality mode is
            // `CODECAPI_AVEncCommonQuality`, a QP value this app does not carry. Say so, and
            // rate-control as constant bitrate, which is what the MFT was already doing.
            tracing::warn!(
                "the Media Foundation path has no constant-quality mode without a QP value, so \
                 the encoder is rate-controlled as constant bitrate; the constant-quality mode is \
                 available on the ffmpeg child path (encode.zero_copy = false)"
            );
            (eAVEncCommonRateControlMode_CBR, "rate control mode (CBR)")
        }
    };
    set_codec_api_value(
        &api,
        &CODECAPI_AVEncCommonRateControlMode,
        mft_mode.0 as u32,
        what,
    );
    set_codec_api_value(
        &api,
        &CODECAPI_AVEncCommonMeanBitRate,
        bitrate_kbps.saturating_mul(1_000),
        "mean bitrate (bit/s)",
    );
    // The buffer (one second of the rate) and the ceiling complete the contract. Low-latency mode
    // stays on: turning it off moved the rate by less than run-to-run noise and produced no B-frames
    // (the B-frame count itself is refused, `E_INVALIDARG`), so there was nothing to keep it for (§24).
    //
    // The ceiling is what separates the two modes. Pinned to the mean in both, as it was, a
    // "peak-constrained VBR" has no room above its average and behaves as constant bitrate while the
    // setting claims otherwise. For VBR the ceiling is [`VBR_PEAK_FACTOR_PERCENT`] of the mean (the mean
    // is still what the clip's size follows), and the buffer is sized to the ceiling so the
    // encoder has the room to spend it.
    let bits = bitrate_kbps.saturating_mul(1_000);
    let ceiling = match mode {
        RateControl::Vbr => vbr_ceiling(bits),
        RateControl::Cbr | RateControl::Quality => bits,
    };
    set_codec_api_value(
        &api,
        &CODECAPI_AVEncCommonMaxBitRate,
        ceiling,
        "maximum bitrate (bit/s)",
    );
    set_codec_api_value(
        &api,
        &CODECAPI_AVEncCommonBufferSize,
        ceiling,
        "buffer size (bits)",
    );
    set_codec_api_value(&api, &CODECAPI_AVLowLatencyMode, 1, "low latency mode");
}

/// Try to set `subtype` as the transform's input type at `size`.
///
/// The media type carries the attributes every video encoder insists on (major type, subtype,
/// frame size, frame rate, interlace mode, pixel aspect ratio), packed as Media Foundation packs
/// them (two 32-bit halves in a `u64`), because a type missing one is rejected for that rather
/// than for the format under test.
fn accepts_subtype(
    transform: &IMFTransform,
    subtype: &windows::core::GUID,
    size: (u32, u32),
) -> Result<()> {
    // SAFETY: `MFCreateMediaType` is the documented constructor and returns an empty type.
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    // SAFETY: every call below sets an attribute on a media type this function owns.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        // The same rate as the output type, from the same constant, or the probe would measure its own inconsistency.
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(FRAME_RATE.0, FRAME_RATE.1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    // SAFETY: setting the input type on a transform that was just unlocked and given its device
    // manager, with a media type that outlives the call.
    unsafe { transform.SetInputType(0, &media_type, 0) }
        .with_context(|| format!("SetInputType({subtype:?})"))
}

/// The input subtypes an MFT offers, by enumerating until it stops answering.
///
/// Bounded, since an MFT that answered forever would hang the probe and no encoder offers more
/// than a handful. An error is the documented "no more types" signal, which is the break.
fn input_subtypes(transform: &IMFTransform) -> Vec<String> {
    const ENOUGH: u32 = 32;
    let mut found = Vec::new();
    for index in 0..ENOUGH {
        // SAFETY: enumerating the first input stream's available types. The index is walked from
        // zero until the call refuses, which is the documented terminator.
        let Ok(media_type) = (unsafe { transform.GetInputAvailableType(0, index) }) else {
            break;
        };
        // SAFETY: reading the subtype GUID out of a media type this call just returned.
        match unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) } {
            Ok(subtype) => found.push(describe_subtype(&subtype)),
            Err(_) => break,
        }
    }
    found
}

/// A video subtype's name, for the ones this build knows, and its GUID otherwise.
///
/// `RGB32` is included deliberately: it is BGRX in memory, the same byte order WGC delivers as
/// BGRA8, so an encoder that lists it takes a captured texture without a colour conversion.
fn describe_subtype(guid: &windows::core::GUID) -> String {
    let known: [(&windows::core::GUID, &str); 8] = [
        (&MFVideoFormat_NV12, "NV12"),
        (&MFVideoFormat_ARGB32, "ARGB32"),
        (
            &MFVideoFormat_RGB32,
            "RGB32 (BGRA byte order — what WGC delivers)",
        ),
        (&MFVideoFormat_YUY2, "YUY2"),
        (&MFVideoFormat_P010, "P010"),
        (&MFVideoFormat_H264, "H264"),
        (&MFVideoFormat_HEVC, "HEVC"),
        (&MFVideoFormat_HEVC_ES, "HEVC_ES"),
    ];
    for (candidate, name) in known {
        if candidate == guid {
            return name.to_string();
        }
    }
    format!("{guid:?}")
}

/// Whether this machine offers a hardware-accelerated Video Processor MFT.
///
/// Only needed if the encoder refuses BGRA: a BGRA to NV12 conversion belongs on the GPU there.
/// Reported either way.
pub fn probe_video_processors() -> Result<Vec<String>> {
    enumerate_names(MFT_CATEGORY_VIDEO_PROCESSOR)
}

/// The friendly names of the MFTs in one category, hardware-flagged and sorted.
fn enumerate_names(category: windows::core::GUID) -> Result<Vec<String>> {
    let candidates = enumerate_activates(category).context("MFTEnumEx for a category")?;
    let names = candidates
        .iter()
        .map(|candidate| {
            attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string())
        })
        .collect();
    Ok(names)
}

/// A D3D11 device for the **diagnostic** probes: BGRA like capture's, plus `VIDEO_SUPPORT`.
///
/// This is *not* the device the recording runs on. Capture's own (`replayed-capture`, `wgc.rs`
/// `create_device`) is created with `D3D11_CREATE_DEVICE_BGRA_SUPPORT` alone and then marked
/// multithread-protected. This one adds `VIDEO_SUPPORT`, which is what a Media Foundation encoder
/// asks for when handed a D3D11 texture it has to convert: without it the HEVC encoder answers
/// `0xC00D6D76` ("the input type is not supported for D3D device") at any size, while the H.264
/// encoder is content either way. The probes that explore *why* an encoder refuses (`mft_probe`'s
/// matrices) need that more permissive device to see the encoder's own behaviour.
///
/// The questions that decide what the recording does ([`can_encode_with_textures`], the
/// throughput measurement) use [`create_recording_device`] instead, so their answer is about the
/// configuration the recording will actually run on. The previous arrangement answered "zero-copy
/// capable" from this device and then opened the encoder on capture's, which has fewer flags.
///
/// Multithread-protected like capture's, because the encoder's thread and the thread that creates
/// textures on the device are not the same.
///
/// Public because the diagnostics need one; in the real pipeline the device is capture's, which is
/// why [`MftEncoder::open`] takes one instead of making its own.
pub fn create_capture_kind_device() -> Result<ID3D11Device> {
    create_device(
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        "BGRA_SUPPORT | VIDEO_SUPPORT",
    )
}

/// A D3D11 device created exactly as capture creates its own: `BGRA_SUPPORT` only, multithread-protected.
///
/// What a question about the recording's configuration must be asked on. If capture's flags change
/// (`replayed-capture`, `wgc.rs` `create_device`), change them here too: nothing ties the two
/// together but this comment, because the capture crate's constructor is private and the encoder
/// takes the device from the texture it is handed.
pub fn create_recording_device() -> Result<ID3D11Device> {
    create_device(D3D11_CREATE_DEVICE_BGRA_SUPPORT, "BGRA_SUPPORT")
}

fn create_device(
    flags: windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG,
    what: &str,
) -> Result<ID3D11Device> {
    create_device_on(None, flags, what)
}

/// [`create_device`] on a named adapter, or on the default one for `None`.
///
/// `None` is what capture does and what the recording's own questions must do. A named adapter is
/// how a probe or a test makes the device an encoder on that adapter can be handed: `D3D11CreateDevice`
/// takes an explicit adapter only with `D3D_DRIVER_TYPE_UNKNOWN`, and refuses `HARDWARE` with one.
fn create_device_on(
    adapter: Option<&IDXGIAdapter1>,
    flags: windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG,
    what: &str,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let adapter = adapter
        .map(|adapter| adapter.cast::<IDXGIAdapter>())
        .transpose()
        .context("an adapter that is not an IDXGIAdapter")?;
    // SAFETY: `device` is the out-parameter; the adapter is either a live DXGI adapter (with the
    // driver type that goes with one) or null by design (the default hardware adapter), and the
    // feature-level array is empty, which asks for the highest level the adapter supports.
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            },
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .with_context(|| format!("D3D11CreateDevice(HARDWARE, {what})"))?;
    let device = device.context("D3D11CreateDevice returned no device")?;
    // As capture does, and for its reason: this device is used from the thread that makes textures
    // and from the encoder's own thread. A device that does not implement the interface is stepped
    // over, as capture does, rather than failing the question.
    if let Ok(multithread) =
        device.cast::<windows::Win32::Graphics::Direct3D11::ID3D11Multithread>()
    {
        // SAFETY: `multithread` belongs to this device. The return value is the previous setting,
        // which is not information anyone here uses.
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
    }
    Ok(device)
}

/// A Media Foundation device manager holding `device`.
fn create_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    // SAFETY: both are the out-parameters this call fills.
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .context("MFCreateDXGIDeviceManager")?;
    let manager = manager.context("MFCreateDXGIDeviceManager returned no manager")?;
    // SAFETY: `device` is a live D3D11 device and `token` is the one this manager was created
    // with, which is what ties the two together.
    unsafe { manager.ResetDevice(device, token) }.context("IMFDXGIDeviceManager::ResetDevice")?;
    Ok(manager)
}

/// An MFT attribute that holds a string, if it is there.
fn attribute_string(activate: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    // SAFETY: `value`/`length` are the out-parameters; the allocation is freed below on the
    // success path, which is the documented contract for `GetAllocatedString`.
    if unsafe { activate.GetAllocatedString(key, &mut value, &mut length) }.is_err() {
        return None;
    }
    // SAFETY: a non-empty `PWSTR` from `GetAllocatedString` is a NUL-terminated wide string this
    // function owns.
    let text = unsafe { value.to_string() }.ok();
    if !value.is_null() {
        // SAFETY: as above — COM allocated it, so COM frees it.
        unsafe { CoTaskMemFree(Some(value.0 as *const std::ffi::c_void)) };
    }
    text
}

/// An MFT attribute that holds a `u32`, if it is there.
fn attribute_u32(activate: &IMFActivate, key: &windows::core::GUID) -> Option<u32> {
    // SAFETY: `GetUINT32` reads the attribute and returns by value.
    unsafe { activate.GetUINT32(key) }.ok()
}

/// Media Foundation, started for as long as this value lives.
///
/// `MFStartup` is refcounted, and its `MFShutdown` has to come *after* every COM object created
/// under it has been released. The encoder used to call `MFShutdown` from the body of its `Drop`,
/// which runs before the struct's own fields are released, so the transform, the event generator
/// and the device manager were all released into a platform that had already been shut down. As a
/// field declared **last** this is dropped last: Rust drops fields in declaration order, after the
/// `Drop` body.
struct MfSession;

impl MfSession {
    fn start() -> Result<Self> {
        // SAFETY: refcounted; the balancing `MFShutdown` is this value's `Drop`.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
        Ok(Self)
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        // SAFETY: balances the successful `MFStartup` in `start`. A failure to shut down cleanly is
        // not something a destructor can act on.
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// An enumerated `IMFActivate`, which shuts down what it created when it is dropped.
///
/// `MFTEnumEx` hands the caller one reference to every activation object, and the documented
/// partner of `ActivateObject` is `ShutdownObject`: the MFT an activation created is released only
/// when the activation is told to shut it down. Neither was done, so every enumeration leaked its
/// activation objects and the MFTs behind them, and the recorder enumerated on every frame while an
/// encoder could not be opened (see [`encode_loop`]). Holding the activation in this type makes the
/// shutdown a consequence of letting go of it.
struct Activation(IMFActivate);

impl std::ops::Deref for Activation {
    type Target = IMFActivate;

    fn deref(&self) -> &IMFActivate {
        &self.0
    }
}

impl Drop for Activation {
    fn drop(&mut self) {
        // SAFETY: the activation's own method. It is a no-op for one that created nothing, and a
        // failure is not actionable here.
        unsafe {
            let _ = self.0.ShutdownObject();
        }
    }
}

/// Every hardware video MFT in `category`, owned.
///
/// The array `MFTEnumEx` returns is COM-allocated and its entries are references the caller owns:
/// each is moved out (taking over the reference) and the array is then freed without touching
/// them, so nothing is released twice and nothing is left behind.
fn enumerate_activates(category: windows::core::GUID) -> Result<Vec<Activation>> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `activates`/`count` are the out-parameters this call fills.
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_HARDWARE,
            None,
            None,
            &mut activates,
            &mut count,
        )
    }?;
    take_activates(activates, count)
}

/// The hardware video encoders that live on the adapter `luid` names, owned.
///
/// `MFTEnumEx` lists every adapter's encoders as one flat list and says nothing about which GPU
/// each belongs to, so on a machine with two GPUs the first one that opens is whichever sorts
/// first, and a hardware encoder can only be handed a texture on its own adapter. `MFTEnum2` takes
/// `MFT_ENUM_ADAPTER_LUID` and answers for one adapter: on the test machine the Quick Sync encoders
/// come back for the Intel adapter's LUID and the NVENC ones for NVIDIA's, and not the other way
/// round. A failure to enumerate is an error, not a fall back to the flat list: the flat list is
/// the question that picked the wrong GPU.
fn enumerate_activates_for(luid: &LUID) -> Result<Vec<Activation>> {
    let mut attributes: Option<IMFAttributes> = None;
    // SAFETY: `attributes` is the out-parameter this call fills.
    unsafe { MFCreateAttributes(&mut attributes, 1) }.context("MFCreateAttributes")?;
    let attributes = attributes.context("MFCreateAttributes returned no attributes")?;
    // The LUID's own layout, which is what the attribute carries: `LowPart` then `HighPart`.
    let mut blob = [0u8; 8];
    blob[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    blob[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    // SAFETY: setting a blob on the attribute store this function owns.
    unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob) }
        .context("setting MFT_ENUM_ADAPTER_LUID")?;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `activates`/`count` are the out-parameters this call fills.
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE,
            None,
            None,
            &attributes,
            &mut activates,
            &mut count,
        )
    }
    .context("MFTEnum2")?;
    take_activates(activates, count)
}

/// What DXGI says about the adapter a device runs on.
fn device_adapter_desc(device: &ID3D11Device) -> Result<DXGI_ADAPTER_DESC> {
    let dxgi = device
        .cast::<IDXGIDevice>()
        .context("the D3D11 device is not a DXGI device")?;
    // SAFETY: `GetAdapter` is this device's own method; the adapter it returns is a new reference.
    let adapter = unsafe { dxgi.GetAdapter() }.context("IDXGIDevice::GetAdapter")?;
    // SAFETY: `GetDesc` is the adapter's own method.
    unsafe { adapter.GetDesc() }.context("IDXGIAdapter::GetDesc")
}

/// The adapter a device runs on, as the LUID `MFTEnum2` is asked about.
fn device_adapter_luid(device: &ID3D11Device) -> Result<LUID> {
    Ok(device_adapter_desc(device)?.AdapterLuid)
}

/// Every hardware video encoder that lives on the adapter `device` runs on, owned.
///
/// What an encoder candidate list is made of: the encoders that can be handed this device's
/// textures, and no others.
fn enumerate_encoders_for(device: &ID3D11Device) -> Result<Vec<Activation>> {
    enumerate_activates_for(&device_adapter_luid(device)?)
}

/// Take ownership of what `MFTEnumEx`/`MFTEnum2` filled in.
fn take_activates(activates: *mut Option<IMFActivate>, count: u32) -> Result<Vec<Activation>> {
    if activates.is_null() {
        return Ok(Vec::new());
    }
    let owned = (0..count as usize)
        // SAFETY: `activates` points at `count` initialised entries written by the enumeration
        // that produced both. Each is read exactly once, which moves its reference out of the array.
        .filter_map(|index| unsafe { std::ptr::read(activates.add(index)) })
        .map(Activation)
        .collect();
    // SAFETY: COM-allocated by the enumeration, and its entries have all been moved out above, so
    // this frees the storage only.
    unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
    Ok(owned)
}

/// The candidates that are the requested vendor's, or all of them when none was requested.
///
/// An encoder that does not name its vendor is not the requested one: the filter decides which GPU
/// the recording runs on, so "cannot tell" is a no (see [`Vendor::names_mft_vendor`]).
fn encoders_for_vendor(candidates: Vec<Activation>, vendor: Option<Vendor>) -> Vec<Activation> {
    let Some(vendor) = vendor else {
        return candidates;
    };
    candidates
        .into_iter()
        .filter(|candidate| {
            attribute_string(candidate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
                .is_some_and(|id| vendor.names_mft_vendor(&id))
        })
        .collect()
}

/// Why no encoder can be opened on `device`, in terms of the adapter it is on.
///
/// `on_adapter` is how many hardware encoders that adapter has at all, which tells "this GPU has no
/// encoder" from "it has some, none of them the vendor's".
fn no_encoder_here(device: &ID3D11Device, params: MftEncoderParams, on_adapter: usize) -> String {
    let adapter = adapter_description(device);
    match params.vendor {
        Some(vendor) => format!(
            "no {} hardware encoder lives on {adapter}, the adapter the captured texture is on \
             ({on_adapter} other hardware encoder(s) are there). An encoder can only be handed a \
             texture on its own adapter, and frames are never copied between adapters; encode \
             this recording through ffmpeg instead (`encode.zero_copy = false`, or leave it unset \
             to let it decide)",
            vendor.as_str()
        ),
        None => format!(
            "no hardware encoder MFT on {adapter}, the adapter the texture is on, will take \
             {:?} at this size",
            params.codec
        ),
    }
}

/// A hardware encoder MFT, configured for a size and fed one GPU texture at a time.
///
/// [`MftEncoder::push_texture`] moves no pixels through the CPU: the captured texture goes to the
/// encoder as a DXGI surface buffer on the GPU it lives on.
///
/// Three things about opening it were each a confident false negative when done the obvious way:
///
/// * an asynchronous MFT must be unlocked before it can be configured;
/// * the output type must be set before the input, because an encoder will not negotiate an
///   input until it knows what it is producing;
/// * the encoder must be chosen, not assumed: the hardware flag enumerates MFTs that cannot be
///   instantiated on this machine.
pub struct MftEncoder {
    transform: IMFTransform,
    /// The same transform as an event generator, which is how an asynchronous MFT says it wants
    /// input and has output. Cast once at open rather than per frame.
    events: IMFMediaEventGenerator,
    /// The activation that created `transform`, shut down when this is dropped (after the transform
    /// and the event generator above have been released, which is the order fields drop in).
    _activation: Activation,
    /// Held because the manager only borrows it: releasing the device while an MFT holds a manager
    /// over it is the dangling-association case Media Foundation warns about.
    device: ID3D11Device,
    size: (u32, u32),
    /// The codec, the rate and the mode it was opened with.
    ///
    /// Kept because a renegotiation has to put the same ones back: the type an encoder offers
    /// after it changed its own may declare a different rate, and the clip's size follows the
    /// bitrate this pins.
    params: MftEncoderParams,
    /// Which MFT was selected, for the log.
    pub encoder_name: String,
    /// Which input format it actually took, at this size.
    ///
    /// `ARGB32` is what WGC delivers, so taking it means the texture goes straight in; taking only
    /// `NV12` means something must convert, and it has to be on the GPU or the copy is moved, not removed.
    pub input_format: String,
    /// The input subtype it took, so the input type can be set again.
    ///
    /// The input type is negotiated against the output type, and Media Foundation requires it to
    /// be set again when the output type changes. Without this, an encoder that changed its
    /// type would refuse every frame that followed with nothing to say why.
    input_subtype: windows::core::GUID,
    /// How many times it changed its type and was renegotiated, for the log and for the probe's
    /// own failure message.
    renegotiated: u32,
    /// The frames handed over and the capture times they were captured at.
    ///
    /// The encoder returns one frame for each frame it was given, in the same order, so this is
    /// how a finished frame is matched back to the frame it is: the oldest frame handed over is
    /// the next frame finished. The capture time is what the child's timeline is built from, and
    /// it cannot be recovered from the encoder's own timestamps — those are the encoder's, and
    /// they are not the picture's.
    pairing: FramePairing,
    /// The capture time of the first frame handed over, which is the encoder stream's origin.
    origin: Option<Duration>,
    /// The `METransformNeedInput` announcements read and not yet spent on a frame (see
    /// [`crate::InputCredits`] for why this is a count and not a moment).
    input_credits: crate::InputCredits,
    /// Keeps Media Foundation started. **Last on purpose**: fields drop in declaration order, so
    /// the platform is shut down only after every COM object above has been released.
    _session: MfSession,
}

/// One frame the encoder finished: its bytes, and the capture time of the frame it is.
///
/// `captured_at` is `None` when the encoder produced a frame that was never handed over, which is
/// a mismatch rather than a fact about the picture: such a frame is written as soon as it can be,
/// which is what happened before the frames were matched back.
pub struct EncodedFrame {
    /// The encoded bytes: one access unit of the elementary stream.
    pub bytes: Vec<u8>,
    /// When the frame that produced them was captured.
    pub captured_at: Option<Duration>,
}

// SAFETY: the members are COM interfaces the `windows` crate marks `Send`+`Sync` for the ones that
// are (a D3D11 device and an MFT are both usable from one thread at a time), and this type is moved
// to the encoder's own thread and used there, never concurrently. The device is additionally
// expected to be multithread-protected by its owner once capture and this encoder both touch it.
unsafe impl Send for MftEncoder {}

/// How many times `ProcessOutput` may be asked again after the encoder changed its type.
///
/// A renegotiation that leads straight to another type change is a fault to report, not a loop to
/// spin in: an encoder that will not settle is not going to settle on its own.
const RENEGOTIATION_ATTEMPTS: u32 = 3;

/// How many output types an encoder is asked for before the enumeration is given up on.
///
/// The enumeration ends with a refusal and that refusal is the end; the bound is here so that an
/// MFT answering for ever cannot hang the probe.
const MAX_OFFERED_TYPES: u32 = 16;

/// The parts of an output type the choice between types is made in.
///
/// Plain data out of a COM object, so the choice can be made — and tested — off Windows.
fn output_type_spec(media_type: &IMFMediaType) -> OutputTypeSpec {
    // SAFETY: each call reads one attribute out of a type this function does not own.
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
        .map(|guid| describe_subtype(&guid))
        .unwrap_or_else(|_| "<no subtype>".to_string());
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map(|packed| ((packed >> 32) as u32, (packed & 0xffff_ffff) as u32))
        .unwrap_or((0, 0));
    let fps = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }
        .map(|packed| (packed >> 32) as u32)
        .unwrap_or(0);
    OutputTypeSpec { subtype, size, fps }
}

impl MftEncoder {
    /// Open a hardware encoder for `codec` on `device` for frames of `size`.
    ///
    /// `device` is the capture path's, deliberately: an MFT given a manager over a different device
    /// cannot be handed the captured texture at all. The encoder is chosen from the ones on
    /// `device`'s own adapter, and from the vendor's among them when `params` names one: it is never
    /// an encoder on another GPU, and never another vendor's in place of the one asked for.
    ///
    /// `params` is the codec, the rate and the mode the encoder is opened with (see
    /// [`MftEncoderParams`]).
    pub fn open(
        device: &ID3D11Device,
        size: (u32, u32),
        preferred: Option<DXGI_FORMAT>,
        params: MftEncoderParams,
    ) -> Result<Self> {
        // Refcounted, so opening several encoders in one process is a supported sequence. The guard
        // goes into the encoder (as its last field) or is dropped with the failed attempt, which
        // is what balances `MFStartup` on both paths.
        let session = MfSession::start()?;
        Self::open_started(device, size, preferred, params, session)
    }

    fn open_started(
        device: &ID3D11Device,
        size: (u32, u32),
        preferred: Option<DXGI_FORMAT>,
        params: MftEncoderParams,
        session: MfSession,
    ) -> Result<Self> {
        let manager = create_device_manager(device)?;
        // Unfiltered by type, for the reason at `probe_hardware_encoders`: filtering by an H.264
        // output subtype hid the HEVC encoder from the recorder as well as the probe. The negotiation
        // below decides.
        //
        // Filtered by *adapter*, and by vendor when one was asked for. An encoder is only ever
        // offered the device it can use: the ones on the adapter `device` runs on. Enumerating every
        // adapter's encoders and taking the first that opened is how a machine with an NVIDIA GPU and
        // an Intel iGPU, asked for Quick Sync, recorded with NVENC's MFT and said nothing; and how,
        // asked for nothing in particular, it could be handed a texture it cannot see.
        let on_adapter = enumerate_encoders_for(device).context(
            "MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, hardware) for the capture device's adapter",
        )?;
        let available = on_adapter.len();
        let activates = encoders_for_vendor(on_adapter, params.vendor);
        if activates.is_empty() {
            bail!("{}", no_encoder_here(device, params, available))
        }

        let mut refusals = Vec::new();
        let mut chosen = None;
        // By value: the activation that is chosen goes into the encoder (it has to outlive the
        // transform it created), and every other one is dropped, which shuts down whatever it created.
        for candidate in activates {
            let name = attribute_string(&candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            match configure_encoder(&candidate, &manager, size, preferred, params) {
                Ok((transform, input_format, input_subtype)) => {
                    chosen = Some((transform, name, input_format, input_subtype, candidate));
                    break;
                }
                // Not fatal: the hardware flag enumerates encoders this machine cannot instantiate.
                Err(err) => refusals.push(format!("{name}: {err:#}")),
            }
        }

        let (transform, encoder_name, input_format, input_subtype, activation) = chosen
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no hardware encoder here could be configured for {size:?}: {}",
                    refusals.join("; ")
                )
            })?;
        // SAFETY: an asynchronous MFT implements the event generator, and this one has just been
        // configured as asynchronous.
        let events: IMFMediaEventGenerator = transform
            .cast()
            .context("IMFTransform as IMFMediaEventGenerator")?;
        Ok(Self {
            transform,
            events,
            _activation: activation,
            device: device.clone(),
            size,
            params,
            encoder_name,
            input_format,
            input_subtype,
            renegotiated: 0,
            pairing: FramePairing::default(),
            origin: None,
            input_credits: crate::InputCredits::default(),
            _session: session,
        })
    }

    /// Open an encoder for frames shaped like `texture`, taking its device from the texture itself.
    ///
    /// An MFT can only be handed a texture that lives on the device its manager was built over, so
    /// passing a device separately would trust two components to agree which one capture chose, and
    /// disagreement leaves a texture the encoder silently cannot see. The geometry also comes from the
    /// texture: it is the size that will arrive, and `VideoInput::EncodedBitstream` refuses a scaled
    /// output at spawn.
    pub fn open_for_texture(texture: &ID3D11Texture2D, params: MftEncoderParams) -> Result<Self> {
        // SAFETY: `texture` is a live D3D11 texture and `GetDevice` is the documented way to ask
        // which device it belongs to. The device it returns is a new reference, held by the encoder.
        let device = unsafe { texture.GetDevice() }.context("ID3D11Texture2D::GetDevice")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter for this texture.
        unsafe { texture.GetDesc(&mut desc) };
        Self::open(
            &device,
            (desc.Width, desc.Height),
            Some(desc.Format),
            params,
        )
    }

    /// The geometry this encoder was opened for.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The device this encoder was opened on, the capture path's. A texture handed to
    /// [`MftEncoder::push_texture`] must belong to it; the probe uses it to build a synthetic frame.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Start the stream. Media Foundation's asynchronous contract begins here: an async MFT will not
    /// accept input until told streaming has begun.
    pub fn start(&mut self) -> Result<()> {
        // SAFETY: both are messages to the encoder's own transform, with no parameter.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        }
        .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
        // SAFETY: as above.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")
    }

    /// Hand the encoder one captured texture once it asks for one, and return what it produced.
    ///
    /// An async MFT takes input only after `METransformNeedInput`, delivered through the event
    /// generator. `ProcessInput` at any other moment answers `MF_E_NOTACCEPTING`, which is why the
    /// wait is here rather than at the call site.
    ///
    /// `captured_at` is when the frame was captured. It is the frame's own time: it becomes the
    /// encoder's timestamp for the frame, and it comes back with the frame the encoder finished,
    /// so the child's timeline is the capture's own.
    ///
    /// `deadline` bounds the wait: an encoder that never asks is a fault to report, not a hang.
    pub fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        captured_at: Duration,
        deadline: Duration,
    ) -> Result<Vec<EncodedFrame>> {
        let mut produced = Vec::new();
        let started = Instant::now();
        loop {
            self.pump_events(&mut produced)?;
            if self.input_credits.available() {
                break;
            }
            if started.elapsed() > deadline {
                bail!(
                    "the encoder did not ask for input within {deadline:?}, so nothing could be                      handed to it"
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        self.push_texture(texture, captured_at)?;
        // Collect without waiting further: what this frame produced is already queued once it was accepted.
        //
        // `?`, not `let _ =`. A failure here (an `MEError` event, a device that was removed, a
        // renegotiation that did not take) used to be dropped, so the encoder reported success
        // while it had stopped producing, and the first sign was a recording with no video. The
        // only thing that is not an error is an empty queue, which `pump_events` already answers
        // with `Asked::Quiet`.
        self.pump_events(&mut produced)?;
        Ok(produced)
    }

    /// Tell the encoder no more input is coming, and collect everything it still has into `tail`.
    ///
    /// Without a drain the last few frames sit inside the encoder and never reach the container.
    /// The wait ends at `METransformDrainComplete`, the encoder's own statement that it has
    /// announced every frame it owed (each `METransformHaveOutput` precedes it). This used to poll
    /// for the whole deadline whatever the encoder said, so every clip and every shutdown paid the
    /// full two seconds and "drained" and "gave up" looked the same to the caller.
    ///
    /// The frames go into `tail` as they arrive, and stay there when this fails: a drain that
    /// times out has still produced frames, and the caller writes them. An `Err` for the deadline
    /// means the encoder never said it was finished, so the tail may be short.
    pub fn finish(&mut self, deadline: Duration, tail: &mut Vec<EncodedFrame>) -> Result<()> {
        // SAFETY: a message to the encoder's own transform, with no parameter.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("MFT_MESSAGE_COMMAND_DRAIN")?;
        let started = Instant::now();
        let drained = loop {
            if self.pump_events(tail)? == Asked::Drained {
                break true;
            }
            if started.elapsed() > deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        // SAFETY: as above — the stream is over.
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
        if !drained {
            bail!(
                "the encoder did not report the drain complete within {deadline:?}, so the last \
                 frames of the recording may be missing"
            );
        }
        Ok(())
    }

    /// Drain whatever the encoder has to say, collecting its output as it goes.
    ///
    /// Reads the queue until it is empty, counting every `NeedInput` it passes (see
    /// [`crate::InputCredits`]): stopping at the first would leave the rest of an encoder's
    /// announcements in a queue the next drain throws away. [`Asked::Input`] is "there is room for
    /// a frame now", which is whether any credit is unspent, not whether one was read this time.
    fn pump_events(&mut self, produced: &mut Vec<EncodedFrame>) -> Result<Asked> {
        // The scratch the sample's bytes are copied into, before they are moved into the frame.
        // Reused across the frames of one exchange: the allocation is the size of a frame.
        let mut scratch: Vec<u8> = Vec::new();
        loop {
            match self.next_event()? {
                Some(EncoderEvent::NeedInput) => self.input_credits.offered(),
                Some(EncoderEvent::HaveOutput) => {
                    scratch.clear();
                    if let Some(frame) = self.take_output(&mut scratch)? {
                        produced.push(frame);
                    }
                }
                Some(EncoderEvent::DrainComplete) => return Ok(Asked::Drained),
                // A marker or a notice: not this exchange's business. Errors never get here.
                Some(EncoderEvent::Other) => {}
                // Nothing queued. Not a failure: this is how a poll ends.
                None => {
                    return Ok(if self.input_credits.available() {
                        Asked::Input
                    } else {
                        Asked::Quiet
                    })
                }
            }
        }
    }

    /// One event from the encoder's queue, taken without waiting.
    ///
    /// `None` when the queue is empty, which is what a poll looks like when the encoder is
    /// busy with the frame it was given. The caller decides what to do about that; the
    /// throughput probe spins on it and the recording's own thread sleeps on it.
    fn next_event(&mut self) -> Result<Option<EncoderEvent>> {
        // SAFETY: the event generator on this encoder's own transform. `NO_WAIT` makes an empty
        // queue an error rather than a block, which is what a poll needs.
        let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            // The one error that means "nothing queued". Every other failure of the event queue
            // (it was shut down, the device went away) used to be read as "nothing queued" too,
            // which is how a dead encoder looked like an idle one until a timeout said otherwise.
            Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
            Err(err) => return Err(err).context("IMFMediaEventGenerator::GetEvent"),
        };
        // SAFETY: reading this event's own type.
        let kind = unsafe { event.GetType() }.context("IMFMediaEvent::GetType")?;
        if kind == METransformNeedInput.0 as u32 {
            Ok(Some(EncoderEvent::NeedInput))
        } else if kind == METransformHaveOutput.0 as u32 {
            Ok(Some(EncoderEvent::HaveOutput))
        } else if kind == METransformDrainComplete.0 as u32 {
            Ok(Some(EncoderEvent::DrainComplete))
        } else if kind == MEError.0 as u32 {
            // SAFETY: reading the status the event carries.
            let status = unsafe { event.GetStatus() };
            match status {
                Ok(code) => bail!("the encoder raised MEError: {code}"),
                Err(err) => {
                    bail!("the encoder raised MEError and its status was unreadable: {err}")
                }
            }
        } else {
            Ok(Some(EncoderEvent::Other))
        }
    }

    /// Take one finished sample's bytes out of the encoder, and say which captured frame it is.
    ///
    /// A type change is renegotiated and the call is made again. Media Foundation's own words for
    /// `MF_E_TRANSFORM_STREAM_CHANGE` (0xC00D6D61) are "a stream change has occurred. Output
    /// cannot be produced until the streams have been renegotiated", so there is nothing to take
    /// until the new type is set, and asking again once it is set is what produces the frame the
    /// encoder was working on.
    ///
    /// `None` when the encoder had nothing to give, which is not a failure: it answers
    /// `MF_E_TRANSFORM_NEED_MORE_INPUT` until it has a frame.
    fn take_output(&mut self, produced: &mut Vec<u8>) -> Result<Option<EncodedFrame>> {
        let mut attempts = 0u32;
        loop {
            attempts = attempts.saturating_add(1);
            // `ManuallyDrop` because that is how the struct is generated: the sample is the caller's once
            // taken, and leaving the field un-taken must not release it twice.
            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            // SAFETY: one output stream, and the sample is the MFT's to allocate (an encoder that provides
            // samples is the case here, so `dwFlags` is zero and the buffer carries no sample of ours). The
            // slice is exactly one element, which is what this stream has, and `status` is the documented
            // out-parameter for the per-stream flags.
            let taken = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
            };
            // Both fields are the caller's to release once the call returns, whatever it returned.
            // The events collection was never released, so one leaked per output call; and taking the
            // sample here (rather than after the error handling) means a failing call cannot leave
            // one behind either.
            // SAFETY: each field is taken exactly once, which is the contract for a `ManuallyDrop`
            // the callee may have filled in.
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
            // SAFETY: as above.
            drop(unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) });
            // The MFT hands back a sample only when it has one; asking earlier is the documented
            // `MF_E_TRANSFORM_NEED_MORE_INPUT`, not an error to propagate.
            match taken {
                Ok(()) => {}
                Err(err) => match classify_output_failure(err.code().0 as u32) {
                    OutputFailure::NeedsInput => return Ok(None),
                    // The encoder changed its output type. The streams have to be renegotiated
                    // before anything can be produced; see `renegotiate_output_type`.
                    OutputFailure::TypeChanged => {
                        if attempts >= RENEGOTIATION_ATTEMPTS {
                            return Err(err).context(format!(
                                "the encoder changed its output type {attempts} times in a row and \
                                 the streams could not be renegotiated"
                            ));
                        }
                        self.renegotiate_output_type()?;
                        continue;
                    }
                    // Quick Sync's way of saying "that output event was the type change": see
                    // [`empty_after_renegotiation`]. Only ever the retry that follows a renegotiation.
                    OutputFailure::Failed
                        if empty_after_renegotiation(err.code().0 as u32, attempts > 1) =>
                    {
                        return Ok(None)
                    }
                    OutputFailure::Failed => {
                        return Err(err).context("IMFTransform::ProcessOutput")
                    }
                },
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            // SAFETY: this sample is the one the encoder just produced, and the contiguous buffer is a
            // view of it that is locked and unlocked around the copy.
            let bytes = unsafe { sample.ConvertToContiguousBuffer() }
                .context("ConvertToContiguousBuffer")?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            // SAFETY: `data`/`length` are the out-parameters; the buffer is unlocked on every path below.
            unsafe { bytes.Lock(&mut data, None, Some(&mut length)) }
                .context("IMFMediaBuffer::Lock")?;
            if !data.is_null() && length > 0 {
                // SAFETY: `Lock` guarantees `length` readable bytes at `data` until `Unlock`.
                produced.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(data, length as usize)
                });
            }
            // SAFETY: pairs the `Lock` above.
            unsafe {
                let _ = bytes.Unlock();
            };
            // One frame in, one frame out: the count the throughput probe reports is the number of
            // frames the encoder actually finished, not the number of frames it was handed.
            //
            // The frame is the oldest one handed over: the encoder returns them in the order they
            // went in, and the capture time is what the child's timeline is built from.
            let captured_at = self.pairing.finished();
            return Ok(Some(EncodedFrame {
                bytes: std::mem::take(produced),
                captured_at,
            }));
        }
    }

    /// The output types the encoder will now take, with the parts of each that matter here.
    ///
    /// Bounded, because an MFT that answered for ever would hang the probe: the enumeration ends
    /// with a refusal, and that refusal is the end. The refusal is kept, because "the encoder
    /// offers nothing" and "the encoder would not say" are different failures.
    fn available_output_types(
        &self,
    ) -> (
        Vec<(IMFMediaType, OutputTypeSpec)>,
        Option<windows::core::Error>,
    ) {
        let mut offered = Vec::new();
        let mut refused = None;
        for index in 0..MAX_OFFERED_TYPES {
            // SAFETY: the transform's own enumeration; the type it returns is owned by the caller.
            let media_type = match unsafe { self.transform.GetOutputAvailableType(0, index) } {
                Ok(media_type) => media_type,
                Err(err) => {
                    refused = Some(err);
                    break;
                }
            };
            let spec = output_type_spec(&media_type);
            offered.push((media_type, spec));
        }
        (offered, refused)
    }

    /// Set the encoder's output type again, after it said it changed it.
    ///
    /// Media Foundation's contract, and every part of it is load-bearing:
    ///
    /// * the new type comes from the encoder itself (`GetOutputAvailableType`), because the encoder
    ///   is the one that changed its mind; a type built here would be the type it just refused;
    /// * the input type is set again with it, because the input type is negotiated against the
    ///   output type and Media Foundation requires it to be set again when that changes. Without
    ///   this the encoder refuses every frame that follows, with nothing to say why;
    /// * the rate is pinned again, because the clip's size follows `encode.bitrate_kbps` and an
    ///   encoder that changed its type may have gone back to its own default.
    ///
    /// Which type to set is [`choose_output_type`]'s answer, and that is not Windows-only: the
    /// choice is a rule, and a rule that can only run on one platform is a rule only tested there.
    fn renegotiate_output_type(&mut self) -> Result<()> {
        let wanted = OutputTypeSpec {
            subtype: self.params.codec.as_str().to_string(),
            size: self.size,
            fps: self.params.fps,
        };
        let (offered, refused) = self.available_output_types();
        let specs: Vec<OutputTypeSpec> = offered.iter().map(|(_, spec)| spec.clone()).collect();
        let Some(index) = choose_output_type(&specs, &wanted) else {
            // What was on offer, in the driver's own spelling: a refusal that does not say what was
            // on offer is a refusal the reader cannot act on.
            for (media_type, _) in &offered {
                tracing::warn!(
                    "the output type it offers: {}",
                    describe_media_type(media_type)
                );
            }
            let refusal = type_change_refusal(&wanted, &specs);
            match refused {
                Some(err) => {
                    bail!("{refusal}; asking it what it offers failed: {err}")
                }
                None => bail!("{refusal}"),
            }
        };
        let (media_type, spec) = &offered[index];
        // SAFETY: the type is one the transform itself offered, and it outlives the call.
        unsafe { self.transform.SetOutputType(0, media_type, 0) }.with_context(|| {
            format!("SetOutputType after the encoder changed its type (it offered {spec:?})")
        })?;
        // The input type again: it is negotiated against the output type, which has just changed.
        let input = video_type(&self.input_subtype, self.size, self.params.fps)?;
        // SAFETY: setting the input type on the transform's own input stream, with a type that
        // outlives the call.
        unsafe { self.transform.SetInputType(0, &input, 0) }.with_context(|| {
            format!(
                "SetInputType after the encoder changed its output type ({:?})",
                self.input_subtype
            )
        })?;
        pin_bitrate(&self.transform, self.params.bitrate_kbps, self.params.mode);
        self.renegotiated = self.renegotiated.saturating_add(1);
        tracing::info!(
            encoder = %self.encoder_name,
            size = ?self.size,
            fps = self.params.fps,
            subtype = %spec.subtype,
            "the encoder changed its output type and the stream was renegotiated"
        );
        Ok(())
    }

    /// Ask for the next frame to be a keyframe.
    ///
    /// `MF_MT_MAX_KEYFRAME_SPACING` is a maximum, and the HEVC MFT measured keyframes every 2 s at
    /// a spacing of one second's frames, which would make every fragment two segments long. This is
    /// the documented per-frame request, and with it HEVC keyframes once a second (§24). Silent on
    /// refusal, since it is asked once a second: a refusal leaves the encoder's own cadence.
    pub fn force_keyframe(&self) {
        let Ok(api) = self.transform.cast::<ICodecAPI>() else {
            return;
        };
        use windows::Win32::System::Variant::{
            VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4,
        };
        let variant = VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_UI4,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { ulVal: 1 },
                }),
            },
        };
        // SAFETY: as in `set_codec_api_value`: the interface's own method, a `VT_UI4` built here.
        let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant) };
    }

    /// The sequence header the encoder published on its output type, in Annex-B form.
    ///
    /// Media Foundation's encoders add `MF_MT_MPEG_SEQUENCE_HEADER` to the output type once the
    /// type is set ("After the output type is set, the video encoder updates the type by adding
    /// the MF_MT_MPEG_SEQUENCE_HEADER attribute"), and the SDK documents the blob as "concatenated
    /// NAL units in Annex B format, along with their start codes" — the SPS and the PPS for H.264,
    /// the VPS, the SPS and the PPS for HEVC.
    ///
    /// The type is read back through `GetOutputAvailableType`, which is where the encoder
    /// publishes the type it negotiated; on a hardware encoder the attribute is on one of the
    /// offered types rather than on the first, so every one of them is asked. The blob is then
    /// converted to Annex-B, because the bitstream the child's stdin carries is a raw Annex-B
    /// stream, and a length-prefixed parameter set is not one a raw demuxer can read.
    ///
    /// This is what the raw H.264 demuxer probes with. Without it the demuxer waits for the first
    /// keyframe to carry the parameter sets in band, and on a display that barely changes that
    /// keyframe arrives after the audio input's connect deadline (issue #78).
    pub fn sequence_header(&self) -> Result<Vec<u8>> {
        let Some(blob) = self.output_type_blob(&MF_MT_MPEG_SEQUENCE_HEADER) else {
            anyhow::bail!(
                "the encoder's output type carries no MF_MT_MPEG_SEQUENCE_HEADER attribute, so \
                 there is no sequence header to put in front of the bitstream"
            );
        };
        crate::annex_b_sequence_header(&blob).context("converting the encoder's sequence header")
    }

    /// A blob attribute of the encoder's own output type, if it has one.
    ///
    /// **The current type is asked first, then every offered one.** The sequence header is a
    /// property of the type the encoder is *producing*, and Media Foundation's documentation puts
    /// it on the output type the encoder sets; asking only the enumeration (`GetOutputAvailableType`)
    /// asks the list this machine's NVENC driver answers with types that carry no header at all,
    /// which is how the check below failed on the Windows box with the encoder working fine
    /// around it. The enumeration remains as a fallback because which type carries the attribute is
    /// the encoder's own business, and a driver that fills it in only on the offered types must
    /// still be read.
    fn output_type_blob(&self, key: &windows::core::GUID) -> Option<Vec<u8>> {
        // SAFETY: a read of this transform's own current output type; the type it returns is owned
        // here and dropped with the loop's iteration.
        if let Ok(media_type) = unsafe { self.transform.GetOutputCurrentType(0) } {
            if let Some(blob) = blob_of(&media_type, key) {
                return Some(blob);
            }
        }
        for index in 0..MAX_OFFERED_TYPES {
            // SAFETY: the transform's own enumeration; the type it returns is owned here.
            let Ok(media_type) = (unsafe { self.transform.GetOutputAvailableType(0, index) })
            else {
                continue;
            };
            if let Some(blob) = blob_of(&media_type, key) {
                return Some(blob);
            }
        }
        None
    }

    /// Hand one captured texture to the encoder, with no copy of its pixels.
    ///
    /// `captured_at` is when the frame was captured, and the encoder is told so: the sample's
    /// timestamp is the frame's real capture time, measured from the first frame handed over. The
    /// stream the encoder sees is then the capture's own timeline rather than a nominal grid — a
    /// frame captured 17.4 ms after the one before it says so — which is what the rate control
    /// budgets the bits against.
    ///
    /// The frame is also remembered, so the frame the encoder finishes can be matched back to the
    /// frame it is: the encoder returns them in order, and the capture time is what the child's
    /// timeline is built from.
    pub fn push_texture(&mut self, texture: &ID3D11Texture2D, captured_at: Duration) -> Result<()> {
        // The documented way to wrap a DXGI surface as a media buffer. `fbottomupwhenlinear` is false
        // because a captured texture is top-down.
        // SAFETY: `texture` is a live D3D11 texture on this encoder's device, and the IID names the
        // interface it is being wrapped as.
        let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false) }
            .context("MFCreateDXGISurfaceBuffer")?;
        // SAFETY: `MFCreateSample` returns an empty sample this function owns.
        let sample = unsafe { MFCreateSample() }.context("MFCreateSample")?;
        // SAFETY: both calls act on that sample, and the buffer is referenced by it.
        unsafe { sample.AddBuffer(&buffer) }.context("IMFSample::AddBuffer")?;
        // The first frame is the stream's origin, so the encoder's timestamps start at zero and
        // march by the real capture intervals.
        let origin = *self.origin.get_or_insert(captured_at);
        let pts = capture_pts(origin, captured_at);
        self.pairing.handed_over(captured_at);
        // SAFETY: setting the sample's timestamps; 100ns units are what Media Foundation counts in.
        unsafe {
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.params.fps.max(1)))?;
        }
        // SAFETY: the encoder's own input stream, with a sample that outlives the call.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .context("IMFTransform::ProcessInput")?;
        self.input_credits.spent();
        Ok(())
    }
}

/// A blob attribute of one media type, or `None` when it has none or cannot be read.
///
/// One function for both callers ([`MftEncoder::output_type_blob`]'s two paths), so the size-then-read
/// dance — which is a `GetBlobSize` and a `GetBlob` that can disagree — exists once.
fn blob_of(media_type: &IMFMediaType, key: &windows::core::GUID) -> Option<Vec<u8>> {
    // SAFETY: reading one attribute out of a type the caller owns.
    let size = unsafe { media_type.GetBlobSize(key) }.ok()?;
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size as usize];
    let mut written = size;
    // SAFETY: the buffer is exactly the size the attribute reported, and it outlives the call.
    if unsafe { media_type.GetBlob(key, &mut buffer, Some(&mut written)) }.is_err() {
        return None;
    }
    buffer.truncate(written as usize);
    (!buffer.is_empty()).then_some(buffer)
}

impl Drop for MftEncoder {
    fn drop(&mut self) {
        // The documented end of an asynchronous MFT: end streaming, `IMFShutdown::Shutdown`, and
        // treat it as gone once `GetShutdownStatus` says `MFSHUTDOWN_COMPLETED`. That is not
        // enough on its own; see the grace period at the end.
        // SAFETY: messages and calls on the encoder's own live transform, with no parameter.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        shut_down_transform(&self.transform, &self.encoder_name);
        // Detach the device manager before the transform goes, as Media Foundation requires; the
        // transform's own reference then drops with this struct. A shut-down MFT answers
        // `MF_E_SHUTDOWN` here, having let go of it already.
        // SAFETY: the transform is live, and the parameter is the documented detach value.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
        // NVIDIA's asynchronous encoder (`nvEncMFTH264x.dll`) still has a work item on Media
        // Foundation's work queue (`RTWorkQ`) when it is let go of, and says nothing about it:
        // `GetShutdownStatus` answers `MFSHUTDOWN_COMPLETED` within a microsecond. Released then,
        // the object is destroyed under that item, which runs `EnterCriticalSection` on freed
        // memory: an access violation on a work-queue thread. On the test machine (RTX 3090) the
        // MFT suite died that way in about one run in three, and sixty open-encode-close cycles in
        // one process in five runs of five, whichever of the teardown messages above were sent or
        // left out. Holding the last references for `RELEASE_GRACE` first took it to none in ten.
        // There is no event to wait for instead, so this is a pause: an opened encoder is closed on
        // a stop or a resize, where reopening one costs about as much again.
        std::thread::sleep(RELEASE_GRACE);
        // Then the fields go, in declaration order: Media Foundation is shut down by `_session`,
        // the last field, after the transform and everything else here has been released. Calling
        // `MFShutdown` in this body ran it first.
    }
}

/// How long a closed encoder's last references are held before they are released (see `Drop`).
const RELEASE_GRACE: Duration = Duration::from_millis(100);

/// How long a closing encoder is given to finish shutting down before it is let go of anyway.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// Shut an asynchronous MFT down and wait until it says it has, bounded by [`SHUTDOWN_WAIT`].
///
/// `Shutdown` may complete asynchronously, which is what `GetShutdownStatus` reports
/// (`MFSHUTDOWN_INITIATED`, then `MFSHUTDOWN_COMPLETED`). A transform without `IMFShutdown` (a
/// synchronous one) has nothing queued and nothing to wait for. A shutdown that does not complete
/// in time is logged and the encoder is let go of: a hang on close would be worse.
fn shut_down_transform(transform: &IMFTransform, name: &str) {
    let Ok(shutdown) = transform.cast::<IMFShutdown>() else {
        return;
    };
    // SAFETY: the transform's own method, with no parameter.
    if let Err(err) = unsafe { shutdown.Shutdown() } {
        tracing::debug!("{name}: IMFShutdown::Shutdown answered {err}");
        return;
    }
    let started = Instant::now();
    loop {
        // SAFETY: as above; valid once `Shutdown` has been called.
        match unsafe { shutdown.GetShutdownStatus() } {
            Ok(status) if status == MFSHUTDOWN_COMPLETED => return,
            Ok(_) if started.elapsed() < SHUTDOWN_WAIT => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(_) => {
                tracing::warn!("{name} did not finish shutting down within {SHUTDOWN_WAIT:?}");
                return;
            }
            // Not reporting a status is not a shutdown in progress; there is nothing to wait on.
            Err(_) => return,
        }
    }
}

/// What the hardware encoder is opened with: the codec, the rate and the mode.
///
/// One value rather than four arguments, because the encoder is opened from three places (the
/// recording, the probe and the test) and a list of arguments is a list that can be given in the
/// wrong order.
#[derive(Debug, Clone, Copy)]
pub struct MftEncoderParams {
    /// The codec the encoder produces.
    pub codec: VideoCodec,
    /// The rate the stream is made at, in kbit/s. Nothing else sets one: ffmpeg copies the
    /// stream, so this is the rate the clip really has.
    pub bitrate_kbps: u32,
    /// The rate frames are fed at, which the encoder budgets the bitrate by.
    pub fps: u32,
    /// The rate-control mode asked for (see [`pin_bitrate`] for what this surface can express).
    pub mode: RateControl,
    /// The vendor whose encoder was asked for (`encode.vendor`), or `None` for any encoder on the
    /// device's adapter, which is what the diagnostics want.
    ///
    /// **A recording always passes one.** The vendor is a choice the user made, and the texture is
    /// on whichever adapter capture's device was created on: when they are different GPUs (Quick
    /// Sync asked for, capture on NVIDIA) there is no encoder to open, and the answer is a refusal
    /// that sends the recording down the ffmpeg path, not the other GPU's encoder in its place.
    pub vendor: Option<Vendor>,
}

/// Configure one candidate: unlock it, give it the device, set its output type, then its input.
///
/// The order matters; each step was learned from a refusal. An async MFT that is not unlocked
/// will not be configured, and an input type asked for before an output type is refused in a way
/// that reads as "this format is unsupported".
fn configure_encoder(
    activate: &IMFActivate,
    manager: &IMFDXGIDeviceManager,
    size: (u32, u32),
    preferred: Option<DXGI_FORMAT>,
    params: MftEncoderParams,
) -> Result<(IMFTransform, String, windows::core::GUID)> {
    // SAFETY: this `IMFActivate`'s own method; the transform it returns is returned to the caller.
    let transform: IMFTransform = unsafe { activate.ActivateObject() }
        .context("IMFActivate::ActivateObject(IMFTransform)")?;

    prepare_transform(activate, &transform, manager)?;
    set_output_type(
        &transform,
        size,
        params.codec,
        params.bitrate_kbps,
        params.fps,
    )?;
    // The number is set here, not only described: the output type carries `MF_MT_AVG_BITRATE`, which
    // these encoders treat as a target rather than a limit (see `pin_bitrate`).
    pin_bitrate(&transform, params.bitrate_kbps, params.mode);
    check_provides_samples(&transform)?;

    // Negotiated, not assumed: this MFT takes `ARGB32` at 640x480 and refuses it at 3840x2160 with
    // `MF_E_INVALIDMEDIATYPE`, so what it eats depends on size as well as format. Candidates are
    // tried in order, the direct one first (`ARGB32` means the texture goes straight in).
    let mut refusals = Vec::new();
    let candidates = candidate_inputs(preferred);
    if candidates.is_empty() {
        bail!(
            "this pipeline has no input type for a {:?} texture (only BGRA and NV12 are offered, \
             and nothing here converts between formats)",
            preferred
        );
    }
    for (subtype, name) in candidates {
        let input = video_type(subtype, size, params.fps)?;
        // SAFETY: setting the input type on a configured transform, with a media type that outlives
        // the call. A refusal here is expected for some formats at some sizes.
        match unsafe { transform.SetInputType(0, &input, 0) } {
            // The subtype comes back with the name: the input type has to be set again if the
            // output type ever changes, and the name alone cannot be set again.
            Ok(()) => return Ok((transform, name.to_string(), *subtype)),
            Err(err) => refusals.push(format!("{name}: {err}")),
        }
    }
    bail!(
        "no input format it would take at {size:?}, having set an output type first: {}",
        refusals.join("; ")
    )
}

/// Refuse an encoder that does not allocate its own output samples.
///
/// [`MftEncoder::take_output`] calls `ProcessOutput` with no sample of its own, which is the
/// documented contract only for a stream flagged `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` (or
/// `CAN_PROVIDE_SAMPLES`, where passing none asks the MFT to provide it). Every encoder measured
/// sets it, but nothing checked: one that does not would answer `E_INVALIDARG` to every output
/// call, and the recording would be a stream of frames that never come out. Allocating a sample
/// of `cbSize` for such an encoder is the other half of the contract; it is not implemented
/// because no encoder here needs it and it could not be tested, so the honest behaviour is to say
/// at open that this encoder is not supported, which `encode.zero_copy = auto` turns into the
/// ffmpeg path.
fn check_provides_samples(transform: &IMFTransform) -> Result<()> {
    // SAFETY: a query on the transform's first output stream, after its output type is set.
    let info =
        unsafe { transform.GetOutputStreamInfo(0) }.context("IMFTransform::GetOutputStreamInfo")?;
    let provides = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0;
    if info.dwFlags & (provides as u32) == 0 {
        bail!(
            "it does not provide its own output samples (flags 0x{:08X}) and this encoder does not \
             allocate them for it",
            info.dwFlags
        );
    }
    Ok(())
}

/// A video media type for one subtype and size, with the attributes every encoder insists on,
/// packed as Media Foundation packs them (two 32-bit halves in a `u64`); a type missing one is
/// rejected for that rather than for the format under test.
fn video_type(subtype: &windows::core::GUID, size: (u32, u32), fps: u32) -> Result<IMFMediaType> {
    // SAFETY: `MFCreateMediaType` returns an empty type this function owns.
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    // SAFETY: every call sets an attribute on that type.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(media_type)
}

/// How many textures may be waiting for the hardware encoder.
///
/// Two, derived rather than chosen. Capture hands out three handover textures in turn, so the
/// texture given to frame *n* is blitted into again at frame *n + 3*. With a queue of two the
/// encoder is done with a texture by the time capture wants it back.
///
/// Deeper would not buy throughput (the encoder is slower than 60 Hz, so frames drop either way)
/// but would risk a texture blitted into while the encoder is still reading it, a torn picture
/// rather than an error. Half the ring keeps the guarantee true instead of likely.
const QUEUE_DEPTH: usize = 2;

/// How long one encoded frame waits for room in the video queue before it is dropped instead.
///
/// The queue is drained by the thread that writes to ffmpeg, which stops making progress whenever
/// ffmpeg stops reading: a quiet child fills its pipe, the pipe fills the queue, and the queue
/// fills this thread. Waiting without a deadline is how the recorder stopped closing:
/// [`MftFeed::shutdown`] joins this thread, and the step that could unstick the reader (killing
/// ffmpeg) sits below that join. On the Windows test machine two of four runs of the hardware
/// path were inside this send at the join and had to be killed.
///
/// It is [`MFT_TIMEOUT`], the budget the encoder calls on this thread already use, since it
/// answers the same question: how long to wait on the other side of the transform.
const HANDOVER_BUDGET: Duration = MFT_TIMEOUT;

/// How many times in a row the encoder may fail to open before the recording is told it has no video.
///
/// An open is not free: it enumerates every hardware encoder, activates each, and negotiates types.
/// Retried on every frame it did that at the capture rate for as long as the machine could not open
/// an encoder, and leaked the activation objects while it did. A handful of attempts a pause apart
/// covers the transient (another process holding the encoder for a moment, a driver reset in
/// progress); a GPU that cannot open one after that is not going to.
const OPEN_ATTEMPTS: u32 = 3;

/// How long the thread waits between open attempts. Frames that arrive meanwhile queue two deep and
/// the rest are dropped and counted by [`MftFeed::submit`], so waiting costs a gap, not a stall.
const OPEN_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// How many frames in a row the encoder may refuse or stall on before it is called dead.
///
/// The thread used to exit on the first one, for good, with one log line: a game's shader-compile
/// hitch or an alt-tab out of exclusive fullscreen stalled the encoder past [`MFT_TIMEOUT`] once,
/// and the recording went on, silently, with no video. A refused frame is dropped and counted
/// instead, and only a run of them (each stall costs [`MFT_TIMEOUT`], so three is several seconds
/// of an encoder that is not answering) is fatal.
const REFUSAL_LIMIT: u32 = 3;

/// The hardware encoder on its own thread, fed through a queue that never blocks.
///
/// `submit` is called from the capture pump, and the transform's synchronous calls take ~30 ms
/// at 4K. Running it inline made the pump wait, and the pump paces capture: a 56.9fps recording
/// came out at 35.0 with 2288 of 5700 frames dropped. So the encoder has its own thread and the
/// pump only hands over textures (owned, nothing copied or read back).
pub struct MftFeed {
    /// `None` once shut down. Dropping it is the shutdown signal (see [`MftFeed::shutdown`]).
    tx: Option<std::sync::mpsc::SyncSender<(GpuTexture, Duration)>>,
    handle: Option<std::thread::JoinHandle<()>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Set by [`MftFeed::shutdown`] before it joins the thread, so a handover waiting for room stops
    /// waiting: from then on nothing below the join can make room.
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Why the encoder's thread gave up, when it did. Written once by the thread just before it
    /// exits, read by [`MftFeed::submit`] and [`MftFeed::failure`], so the death of the encoder is
    /// something the pump is told rather than something it infers from missing video.
    failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl MftFeed {
    /// Start the encoder's thread, whose output goes into `out`, the video queue the pump writes to,
    /// so depth accounting stays in one place.
    pub fn spawn(params: MftEncoderParams, out: std::sync::mpsc::SyncSender<VideoChunk>) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = std::sync::Arc::clone(&dropped);
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = std::sync::Arc::clone(&stopping);
        let failure = std::sync::Arc::new(std::sync::Mutex::new(None));
        let failed = std::sync::Arc::clone(&failure);
        let handle = std::thread::Builder::new()
            .name("replayed-mft".to_string())
            .spawn(move || encode_loop(rx, out, counter, params, stop, failed))
            .expect("spawning the hardware encoder thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
            dropped,
            stopping,
            failure,
        }
    }

    /// Hand a frame to the encoder. Never blocks and never queues deeper than [`QUEUE_DEPTH`]: a
    /// full queue drops and counts the frame, as the video queue does when ffmpeg is behind.
    ///
    /// `captured_at` is when the frame was captured. It travels with the texture because it is
    /// what the finished frame is given back with, and the finished frame's time is what the
    /// child's timeline is built from.
    ///
    /// An error means the encoder is dead and nothing more will be encoded: the thread gave up (see
    /// [`MftFeed::failure`]) or is gone. It used to be `()`, treating a disconnected queue as "the
    /// frame is simply not encoded", so a recording whose encoder had died carried on, capturing and
    /// submitting into a channel nobody read, with the audio still flowing and the status still
    /// "running". The caller turns this into the recording's failure.
    pub fn submit(&self, texture: GpuTexture, captured_at: Duration) -> Result<()> {
        // A nothing-left-to-do after shutdown is not a failure: the pump is being stopped.
        let Some(tx) = self.tx.as_ref() else {
            return Ok(());
        };
        if let Some(reason) = self.failure() {
            bail!("the hardware encoder stopped: {reason}");
        }
        match tx.try_send((texture, captured_at)) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            // The thread is gone. Its reason, if it left one; a panic or a closed video queue
            // leaves none.
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => match self.failure() {
                Some(reason) => bail!("the hardware encoder stopped: {reason}"),
                None => bail!("the hardware encoder's thread has ended"),
            },
        }
    }

    /// Why the encoder's thread gave up, if it did.
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Frames that did not fit the queue and were never encoded.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Close the queue and wait for the thread to encode what is already in it, then drain.
    ///
    /// Dropping the sender is the shutdown signal rather than a message down the queue, because a
    /// full queue can refuse a message and a shutdown that can be refused is a hang. Closing the
    /// sender ends the `recv` loop after the queued frames are taken, so nothing in flight is lost.
    pub fn shutdown(&mut self) {
        // The flag before the sender, and both before the join: this thread cannot finish a handover
        // waiting on a queue whose reader has stopped draining, and the join cannot return until it
        // ends. Telling it to stop waiting keeps the join bounded (see [`hand_over`]).
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::warn!("the hardware encoder thread panicked");
            }
        }
    }
}

/// What handing one encoded frame to the video queue did.
enum Handover {
    Queued,
    /// The queue stayed full for [`HANDOVER_BUDGET`], or shutdown has been asked for: the frame is
    /// not queued, and the stream goes on without it.
    Dropped,
    /// The reader is gone. There is no queue left to write to.
    Closed,
}

/// Hand one encoded frame to the video queue, waiting for room but not for ever.
///
/// A `send` here is what stopped the recorder closing: the queue is drained by the thread that
/// writes to ffmpeg, which stops when ffmpeg stops reading, and the join in
/// [`MftFeed::shutdown`] (the only thing that could unstick it) cannot run until this returns.
/// So the wait is bounded and a frame that cannot be queued in time is dropped and counted: the
/// timeline is wall-clock, and a gap beats a recording that will not stop.
fn hand_over(
    out: &std::sync::mpsc::SyncSender<VideoChunk>,
    units: VideoChunk,
    stopping: &std::sync::atomic::AtomicBool,
) -> Handover {
    let deadline = Instant::now() + HANDOVER_BUDGET;
    let mut pending = units;
    loop {
        match out.try_send(pending) {
            Ok(()) => return Handover::Queued,
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                pending = back;
                if stopping.load(std::sync::atomic::Ordering::SeqCst) || Instant::now() >= deadline
                {
                    return Handover::Dropped;
                }
                // A millisecond: short enough that a shutdown is not felt, long enough that a stalled queue is not a spin.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return Handover::Closed,
        }
    }
}

/// Open an encoder for `texture`, start it, and put its sequence header in front of the bitstream.
///
/// `Ok(None)` when the video queue's reader is gone, which is the recording ending, not the encoder
/// failing.
fn open_and_start(
    texture: &GpuTexture,
    params: MftEncoderParams,
    out: &std::sync::mpsc::SyncSender<VideoChunk>,
    stopping: &std::sync::atomic::AtomicBool,
) -> Result<Option<MftEncoder>> {
    let mut opened = MftEncoder::open_for_texture(texture.as_raw(), params)?;
    opened.start().context("starting the hardware encoder")?;
    // The sequence header goes in front of the bitstream, before the first frame.
    //
    // The child reads the video as a raw H.264 elementary stream, and a raw
    // demuxer has nothing to probe with until it has seen a keyframe carrying the
    // parameter sets. The encoder's own first keyframe can be a long way off on a
    // display that barely changes (the keyframe interval is counted in frames
    // fed, so at four frames a second it is seconds away), and until the probe
    // completes the child has not opened the audio input either — which is the
    // deadlock of issue #78. With the header in front, the probe completes at
    // once.
    match opened.sequence_header() {
        Ok(header) => match hand_over(
            out,
            VideoChunk {
                bytes: header,
                // The header is not a frame: it has to be in front of the first
                // frame, so it is written as soon as it can be.
                captured_at: None,
            },
            stopping,
        ) {
            Handover::Queued => {}
            Handover::Dropped => {
                tracing::warn!(
                    "the sequence header did not fit the video queue, so the raw \
                     bitstream has nothing to probe with until the first keyframe"
                );
            }
            Handover::Closed => return Ok(None),
        },
        Err(err) => {
            // Said out loud, because the alternative is a recording that dies on a
            // quiet display with nothing in the log to say why.
            tracing::error!(
                "the hardware encoder published no usable sequence header \
                 ({err:#}); the raw bitstream the child reads has nothing to \
                 probe with until the first keyframe carries the parameter sets, \
                 so a still display can still starve the recording (issue #78)"
            );
        }
    }
    tracing::info!(
        encoder = %opened.encoder_name,
        format = %opened.input_format,
        size = ?opened.size(),
        adapter = %adapter_description(opened.device()),
        "the hardware encoder is open on its own thread"
    );
    Ok(Some(opened))
}

/// The encoder thread's body.
///
/// It ends for one of four reasons, and only the second is silent: the queue closed (shutdown), the
/// video queue's reader went away (the recording is ending), the encoder could not be opened
/// [`OPEN_ATTEMPTS`] times running, or it failed [`REFUSAL_LIMIT`] frames running. The last two
/// leave their reason in `failure`, which is what turns a dead encoder into the recording's error
/// (see [`MftFeed::submit`]).
fn encode_loop(
    rx: std::sync::mpsc::Receiver<(GpuTexture, Duration)>,
    out: std::sync::mpsc::SyncSender<VideoChunk>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    params: MftEncoderParams,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) {
    // Opened on the first texture, because the device and size come from it; opening here also keeps
    // the open off the capture path.
    let mut encoder: Option<MftEncoder> = None;
    let fps = params.fps.max(1);
    // Frames handed to the encoder so far; the keyframe cadence counts from it.
    let mut frames: u64 = 0;
    // Frames encoded and then not queued, counted rather than logged one by one: a stalled queue drops every frame behind it.
    let mut unqueued: u64 = 0;
    let mut open_strikes = crate::Strikes::new(OPEN_ATTEMPTS);
    let mut frame_strikes = crate::Strikes::new(REFUSAL_LIMIT);
    let mut fatal = false;
    let give_up = |reason: String| {
        tracing::error!("the hardware encoder is giving up: {reason}");
        *failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    };

    'frames: while let Ok((texture, captured_at)) = rx.recv() {
        if encoder.is_none() {
            match open_and_start(&texture, params, &out, &stopping) {
                Ok(Some(opened)) => {
                    open_strikes.record_success();
                    encoder = Some(opened);
                }
                Ok(None) => break 'frames,
                Err(err) => {
                    // Counted as a drop so reporting stays honest about frames that reached this stage and did not become video.
                    tracing::error!("could not open the hardware encoder: {err:#}");
                    dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if open_strikes.record_failure() {
                        give_up(format!(
                            "it could not be opened {OPEN_ATTEMPTS} times in a row; the last \
                             error was: {err:#}"
                        ));
                        fatal = true;
                        break 'frames;
                    }
                    // Not on every frame: see `OPEN_ATTEMPTS`.
                    std::thread::sleep(OPEN_RETRY_PAUSE);
                    continue;
                }
            }
        }
        let mft = encoder.as_mut().expect("just opened");
        // The frame's own time is the encoder's timestamp for it: the stream the encoder sees is
        // the capture's own timeline, and the finished frame carries that time back to the pipe.
        // The keyframe cadence is a count, not a time: it is the segmenter's business, and it must
        // not move when the capture's rate differs from the configured one.
        frames += 1;
        if frames.is_multiple_of(u64::from(fps)) {
            mft.force_keyframe();
        }
        match mft.encode_texture(texture.as_raw(), captured_at, MFT_TIMEOUT) {
            Ok(units) => {
                frame_strikes.record_success();
                for unit in units {
                    let chunk = VideoChunk {
                        bytes: unit.bytes,
                        captured_at: unit.captured_at,
                    };
                    match hand_over(&out, chunk, &stopping) {
                        Handover::Queued => {}
                        Handover::Dropped => unqueued += 1,
                        // The reader is gone, so the rest of this batch has nowhere to go, and nor
                        // does anything after it: leave the whole loop, not just this batch.
                        Handover::Closed => break 'frames,
                    }
                }
            }
            Err(err) => {
                // This frame is lost, and counted as such; the next one gets its chance.
                tracing::warn!("the hardware encoder refused a frame: {err:#}");
                dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if frame_strikes.record_failure() {
                    give_up(format!(
                        "it failed {REFUSAL_LIMIT} frames in a row; the last error was: {err:#}"
                    ));
                    fatal = true;
                    break 'frames;
                }
            }
        }
    }

    if unqueued > 0 {
        tracing::warn!(
            "{unqueued} encoded frames were dropped instead of queued: the video queue stayed full \
             for {HANDOVER_BUDGET:?}, which is the ffmpeg writer thread not keeping up"
        );
    }

    // The tail: an asynchronous MFT produces output when it decides to, so the last frames only become real when drained.
    // Not for an encoder that was just given up on: it is not answering, so a drain would only wait.
    if let Some(mut mft) = encoder.filter(|_| !fatal) {
        let mut tail = Vec::new();
        // Whatever the drain produced is written even when it timed out: frames in hand are not
        // thrown away because the encoder never said it was done.
        let drained = mft.finish(MFT_TIMEOUT, &mut tail);
        for unit in tail {
            let chunk = VideoChunk {
                bytes: unit.bytes,
                captured_at: unit.captured_at,
            };
            if let Err(err) = out.try_send(chunk) {
                tracing::warn!("the hardware encoder's last frames did not fit the queue: {err}");
            }
        }
        if let Err(err) = drained {
            tracing::warn!("draining the hardware encoder failed: {err:#}");
        }
    }
}

/// The adapter a device is running on, by name.
///
/// With two GPUs, "a hardware encoder" is not an answer: a null-adapter device gets whichever
/// adapter DXGI calls the default. The encoder takes its device from the captured texture, so
/// this names the adapter the capture is really running on.
pub fn adapter_description(device: &ID3D11Device) -> String {
    let Ok(dxgi) = device.cast::<IDXGIDevice>() else {
        return "<not a DXGI device>".to_string();
    };
    let Ok(adapter) = (unsafe { dxgi.GetAdapter() }) else {
        return "<no adapter>".to_string();
    };
    adapter_name(&adapter)
}

fn adapter_name(adapter: &IDXGIAdapter) -> String {
    let Ok(description) = (unsafe { adapter.GetDesc() }) else {
        return "<undescribed adapter>".to_string();
    };
    wide_name(&description.Description)
}

/// A DXGI description: UTF-16, NUL-terminated inside a fixed array.
fn wide_name(description: &[u16]) -> String {
    let end = description
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(description.len());
    String::from_utf16_lossy(&description[..end])
}

/// The output-type variants the HEVC matrix tries, in order. Each is the pipeline's type with
/// exactly one thing changed, so a refusal can be attributed.
const OUTPUT_TYPE_VARIANTS: [&str; 8] = [
    "exactly what the pipeline builds",
    "no profile and no level",
    "level 5 instead of 5.1",
    "level 4.1",
    "no average bitrate",
    "no keyframe spacing",
    "MFVideoFormat_HEVC_ES instead of MFVideoFormat_HEVC",
    "Main_420_10 instead of Main_420_8",
];

/// Build the output media type the pipeline builds, with variant `index` of [`OUTPUT_TYPE_VARIANTS`] applied.
///
/// SAFETY: the returned type is a COM object owned by the caller.
unsafe fn build_output_type(
    codec: VideoCodec,
    size: (u32, u32),
    index: usize,
) -> Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    let pair = |first: u32, second: u32| ((first as u64) << 32) | second as u64;
    let subtype = if index == 6 {
        MFVideoFormat_HEVC_ES
    } else {
        output_subtype(codec)
    };
    let (profile, level) = match index {
        7 => (
            eAVEncH265VProfile_Main_420_10.0 as u32,
            eAVEncH265VLevel5_1.0 as u32,
        ),
        2 => (
            eAVEncH265VProfile_Main_420_8.0 as u32,
            eAVEncH265VLevel5.0 as u32,
        ),
        3 => (
            eAVEncH265VProfile_Main_420_8.0 as u32,
            eAVEncH265VLevel4_1.0 as u32,
        ),
        _ => (
            eAVEncH265VProfile_Main_420_8.0 as u32,
            eAVEncH265VLevel5_1.0 as u32,
        ),
    };
    // SAFETY: every call sets an attribute on a media type this function owns.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(FRAME_RATE.0, FRAME_RATE.1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if index != 4 {
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, 20_000_000)?;
        }
        if index != 1 {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            media_type.SetUINT32(&MF_MT_MPEG2_LEVEL, level)?;
        }
        if index != 5 {
            media_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, FRAME_RATE.0)?;
        }
    }
    Ok(media_type)
}

/// Unlock an asynchronous transform and hand it the device manager, in that order.
///
/// An async MFT refuses the device manager with `0xC00D6D77` ("the caller does not appear to
/// support this transform's asynchronous capabilities") until `MF_TRANSFORM_ASYNC_UNLOCK` is set
/// on its attributes, and accepts no type before it has the manager. A second copy of this
/// sequence once left out the unlock: every candidate refused and ten encoders looked like none.
/// So there is one copy.
fn prepare_transform(
    activate: &IMFActivate,
    transform: &IMFTransform,
    manager: &IMFDXGIDeviceManager,
) -> Result<()> {
    if attribute_u32(
        activate,
        &windows::Win32::Media::MediaFoundation::MF_TRANSFORM_ASYNC,
    )
    .map(|value| value != 0)
    .unwrap_or(false)
    {
        // SAFETY: reading the transform's own attribute store, then setting the unlock in it.
        let attributes =
            unsafe { transform.GetAttributes() }.context("IMFTransform::GetAttributes")?;
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .context("unlocking an asynchronous MFT")?;
    }
    // SAFETY: the manager outlives the call, and its interface pointer is what the message wants.
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
    Ok(())
}

/// The attributes of a media type that matter here, as one line.
fn describe_media_type(media_type: &IMFMediaType) -> String {
    // SAFETY: each call reads one attribute from a type this function does not own and does not free.
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
        .map(|guid| describe_subtype(&guid))
        .unwrap_or_else(|_| "<no subtype>".to_string());
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map(|packed| format!("{}x{}", packed >> 32, packed & 0xffff_ffff))
        .unwrap_or_else(|_| "-".to_string());
    let rate = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }
        .map(|packed| format!("{}fps", packed >> 32))
        .unwrap_or_else(|_| "-".to_string());
    let profile = unsafe { media_type.GetUINT32(&MF_MT_MPEG2_PROFILE) }
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "-".to_string());
    let level = unsafe { media_type.GetUINT32(&MF_MT_MPEG2_LEVEL) }
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "-".to_string());
    let bitrate = unsafe { media_type.GetUINT32(&MF_MT_AVG_BITRATE) }
        .map(|value| format!("{}kbps", value / 1000))
        .unwrap_or_else(|_| "-".to_string());
    format!("{subtype}  {size}  {rate}  profile={profile} level={level} bitrate={bitrate}")
}

/// What an encoder says its own output and input types are.
///
/// The direct question, to ask before guessing at attributes. `0xC00D6D76` is
/// `MF_E_UNSUPPORTED_D3D_TYPE` (the type is not one this GPU device supports), the same error NVENC
/// users hit at `SetOutputType`. `GetOutputAvailableType` returns the types the transform offers,
/// with the attributes it wants.
///
/// An async MFT refuses to enumerate input types (hence `SetInputType` elsewhere), so an input
/// failure here is expected and reported as such. An encoder that refuses this codec's output type
/// is not skipped: the refusal is a row, and its available output types are still enumerated,
/// since they say which type it would have taken.
pub fn probe_available_types(codec: VideoCodec, size: (u32, u32)) -> Result<Vec<(String, String)>> {
    // SAFETY: refcounted, and the matching `MFShutdown` runs on the way out.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let rows = (|| -> Result<Vec<(String, String)>> {
        let device = create_capture_kind_device()?;
        let manager = create_device_manager(&device)?;
        let candidates =
            enumerate_encoders_for(&device).context("MFTEnumEx for the available types")?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        for candidate in candidates.iter() {
            let name = attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            let transform = match unsafe { candidate.ActivateObject::<IMFTransform>() } {
                Ok(transform) => transform,
                Err(err) => {
                    rows.push((format!("{name} :: activation"), format!("{err}")));
                    continue;
                }
            };
            if let Err(err) = prepare_transform(candidate, &transform, &manager) {
                rows.push((format!("{name} :: preparation"), format!("{err:#}")));
                continue;
            }
            let media_type = match unsafe { build_output_type(codec, size, 0) } {
                Ok(media_type) => media_type,
                Err(err) => {
                    rows.push((
                        format!("{name} :: the type to ask with"),
                        format!("{err:#}"),
                    ));
                    continue;
                }
            };
            // A refusal is a row, not a skip: `GetOutputAvailableType` says which type the encoder would
            // take whether or not it took the caller's, and the refusing encoder is the one a reader wants
            // to interrogate. The error is reported with its code at the call that produced it and the
            // enumeration below still runs.
            //
            // SAFETY: the transform is live and the type outlives the call.
            let refused = match unsafe { transform.SetOutputType(0, &media_type, 0) } {
                Ok(()) => None,
                Err(err) => Some(format!("{err}")),
            };
            if let Some(err) = &refused {
                rows.push((
                    format!(
                        "{name} :: refused the {codec:?} type at {}x{}",
                        size.0, size.1
                    ),
                    err.clone(),
                ));
            }
            for output in [true, false] {
                // The input side needs an accepted output type (an async MFT will not discuss its inputs until
                // it knows what it produces), so when the type was refused only the output side can answer.
                if !output && refused.is_some() {
                    continue;
                }
                let label = if output { "output" } else { "input" };
                for index in 0..12u32 {
                    // SAFETY: both are the transform's own enumeration methods.
                    let next = if output {
                        unsafe { transform.GetOutputAvailableType(0, index) }
                    } else {
                        unsafe { transform.GetInputAvailableType(0, index) }
                    };
                    match next {
                        Ok(offered) => rows.push((
                            format!("{name} :: {label} #{index}"),
                            describe_media_type(&offered),
                        )),
                        Err(err) => {
                            // Index 0 failing is the whole answer for that direction.
                            if index == 0 {
                                rows.push((format!("{name} :: {label} types"), format!("{err}")));
                            }
                            break;
                        }
                    }
                }
            }
        }
        Ok(rows)
    })();
    // SAFETY: balances the `MFStartup` above.
    unsafe {
        let _ = MFShutdown();
    }
    rows
}

/// Whether an encoder wants its input type declared before its output type.
///
/// "Output first" was measured on the H.264 encoder, but the error being chased names the input
/// type, and an NVENC report of the same `0xC00D6D76` describes configuring the source, then the
/// transform, then the codec. So the order is a variable: this tries all four combinations of
/// order and device manager and reports each.
pub fn probe_ordering(codec: VideoCodec, size: (u32, u32)) -> Result<Vec<(String, String)>> {
    // SAFETY: refcounted, and the matching `MFShutdown` runs on the way out.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let rows = (|| -> Result<Vec<(String, String)>> {
        let device = create_capture_kind_device()?;
        let manager = create_device_manager(&device)?;
        let candidates =
            enumerate_encoders_for(&device).context("MFTEnumEx for the ordering test")?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        for candidate in candidates.iter() {
            let name = attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            for (use_manager, input_first) in
                [(true, false), (true, true), (false, false), (false, true)]
            {
                let label = format!(
                    "{name} :: {} manager, {} first",
                    if use_manager {
                        "with the D3D"
                    } else {
                        "with no"
                    },
                    if input_first { "input" } else { "output" }
                );
                let Ok(transform) = (unsafe { candidate.ActivateObject::<IMFTransform>() }) else {
                    rows.push((label, "could not be activated".to_string()));
                    continue;
                };
                if use_manager {
                    if let Err(err) = prepare_transform(candidate, &transform, &manager) {
                        rows.push((label, format!("preparation failed: {err}")));
                        continue;
                    }
                } else {
                    // An asynchronous transform must be unlocked whether or not it is given a device; setting the attribute on a synchronous one is harmless.
                    if let Ok(attributes) = unsafe { transform.GetAttributes() } {
                        let _ = unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
                    }
                }
                let verdict = (|| -> Result<()> {
                    if input_first {
                        let input = video_type(&MFVideoFormat_NV12, size, FRAME_RATE.0)?;
                        unsafe { transform.SetInputType(0, &input, 0) }
                            .context("SetInputType(NV12)")?;
                    }
                    let output = unsafe { build_output_type(codec, size, 0)? };
                    unsafe { transform.SetOutputType(0, &output, 0) }.context("SetOutputType")?;
                    if !input_first {
                        let input = video_type(&MFVideoFormat_NV12, size, FRAME_RATE.0)?;
                        unsafe { transform.SetInputType(0, &input, 0) }
                            .context("SetInputType(NV12)")?;
                    }
                    Ok(())
                })();
                rows.push((
                    label,
                    match verdict {
                        Ok(()) => "ACCEPTED".to_string(),
                        Err(err) => format!("{err:#}"),
                    },
                ));
            }
        }
        Ok(rows)
    })();
    // SAFETY: balances the `MFStartup` above.
    unsafe {
        let _ = MFShutdown();
    }
    rows
}

/// The largest size a codec's hardware encoder will accept a D3D texture input for.
///
/// Everything else about the refusal is invariant (`prepare_transform`'s order, the output type's
/// attributes, the input format, the device flags) while the same output type is accepted at
/// 640x480 and refused at 3840x2160. So this walks a ladder of sizes and reports where the answer
/// changes: "yes at 1080p, no at 1440p" is a fact about the encoder's D3D path, "yes everywhere
/// but 4K" a fact about 4K.
pub fn probe_size_ceiling(codec: VideoCodec) -> Result<Vec<(String, String)>> {
    // SAFETY: refcounted, and the matching `MFShutdown` runs on the way out.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let rows = (|| -> Result<Vec<(String, String)>> {
        // 4K twice and 1080p twice, on purpose. If the distinguishing factor is the transform's history,
        // a repeat either changes the answer (a state problem with a retry as fix) or it does not
        // (the cause is elsewhere).
        // The ladder rises, with 4K at the end: one that began at 3840x2160 refused every size, while
        // the same ladder rising from 640x480 accepted all of them, 4K twice in one run. If a small size
        // accepted first is what makes a larger one work, the last rows say so; if 4K is refused again on
        // the second attempt, warm-up is not the mechanism.
        let sizes = [
            (640u32, 480u32),
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3200, 1800),
            (3840, 2160),
            (3840, 2160),
        ];
        let device = create_capture_kind_device()?;
        let manager = create_device_manager(&device)?;
        let candidates =
            enumerate_encoders_for(&device).context("MFTEnumEx for the size ladder")?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        for candidate in candidates.iter() {
            let name = attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            for (width, height) in sizes {
                // A fresh transform per size: an MFT that has refused a type need not be in a state where the next answer means anything.
                let Ok(transform) = (unsafe { candidate.ActivateObject::<IMFTransform>() }) else {
                    break;
                };
                if prepare_transform(candidate, &transform, &manager).is_err() {
                    break;
                }
                let Ok(media_type) = (unsafe { build_output_type(codec, (width, height), 0) })
                else {
                    continue;
                };
                // SAFETY: the transform is live and the type outlives the call.
                let verdict = match unsafe { transform.SetOutputType(0, &media_type, 0) } {
                    Ok(()) => "ACCEPTED".to_string(),
                    Err(err) => format!("{err}"),
                };
                rows.push((format!("{name} :: {width}x{height}"), verdict));
            }
        }
        Ok(rows)
    })();
    // SAFETY: balances the `MFStartup` above.
    unsafe {
        let _ = MFShutdown();
    }
    rows
}

/// Which output-media-type attribute an encoder is objecting to, asked one at a time.
///
/// A diagnostic. The HEVC encoder on the Windows test machine accepts the pipeline's type at
/// 640x480 and refuses it at 3840x2160 with `0xC00D6D76` ("the input type is not supported for
/// D3D device"), which names the device rather than the attribute. An NV12 input, a video-capable
/// device and ffmpeg's own `hevc_mf` (which cannot open an HEVC encoder at any size there) did
/// not move it. So this builds the same type with one thing changed at a time and reports which
/// the encoder takes. The device manager is set first, because that is the context of the refusal.
///
/// Nothing in the pipeline calls this. Returns `(what was tried, what happened)` per candidate.
pub fn probe_output_types(codec: VideoCodec, size: (u32, u32)) -> Result<Vec<(String, String)>> {
    // Only HEVC has variants worth trying; the H.264 type is measured and working.
    if codec != VideoCodec::Hevc {
        return Ok(Vec::new());
    }
    // SAFETY: `MFStartup` is refcounted and the matching `MFShutdown` runs on the way out.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let rows = (|| -> Result<Vec<(String, String)>> {
        let device = create_capture_kind_device()?;
        let manager = create_device_manager(&device)?;
        let candidates =
            enumerate_encoders_for(&device).context("MFTEnumEx for the output-type matrix")?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        for candidate in candidates.iter() {
            let name = attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            // A skip is reported, not swallowed, and the preparation is shared. An earlier version returned
            // an empty matrix on a machine with ten encoders because it re-typed the preparation and left
            // out the async unlock, so every candidate refused its device manager with `0xC00D6D77`.
            // `prepare_transform` is the one place that sequence lives.
            let transform = match unsafe { candidate.ActivateObject::<IMFTransform>() } {
                Ok(transform) => transform,
                Err(err) => {
                    rows.push((format!("{name} :: activation"), format!("{err}")));
                    continue;
                }
            };
            if let Err(err) = prepare_transform(candidate, &transform, &manager) {
                rows.push((format!("{name} :: preparation"), format!("{err:#}")));
                continue;
            }
            for (index, label) in OUTPUT_TYPE_VARIANTS.iter().enumerate() {
                let verdict = match unsafe { build_output_type(codec, size, index) } {
                    // SAFETY: the transform is live and the media type outlives the call.
                    Ok(media_type) => match unsafe { transform.SetOutputType(0, &media_type, 0) } {
                        Ok(()) => "ACCEPTED".to_string(),
                        Err(err) => format!("{err}"),
                    },
                    Err(err) => format!("could not be built: {err:#}"),
                };
                rows.push((format!("{name} :: {label}"), verdict));
            }
        }
        Ok(rows)
    })();
    // SAFETY: balances the `MFStartup` above, which succeeded.
    unsafe {
        let _ = MFShutdown();
    }
    rows
}

/// Whether a hardware encoder for `codec` will actually open at `size`.
///
/// For deciding whether to choose the hardware path rather than insisting on it. `auto` is the
/// default for `encode.zero_copy` ("use the hardware when it can"), which an enumeration cannot
/// answer: the Windows test machine's HEVC encoder enumerates, activates, and rejects the output
/// type with `0xC00D6D76` at 4K while accepting it at 640x480. Deciding from `MFTEnumEx` would
/// produce a recording with no video in it.
///
/// The device is the one capture creates ([`create_recording_device`]), because the encoder takes
/// its device from the captured texture and this asks the question the recording will ask. It costs one device and one
/// negotiation, and only while the key is unset.
///
/// `vendor` is the one the recording will open (`encode.vendor`), and it is part of the question:
/// "some encoder opens" is not "Quick Sync opens", see [`textures_refusal`].
pub fn can_encode_with_textures(
    vendor: Vendor,
    codec: VideoCodec,
    size: (u32, u32),
    bitrate_kbps: u32,
    fps: u32,
) -> bool {
    textures_refusal(vendor, codec, size, bitrate_kbps, fps).is_none()
}

/// Why `vendor`'s encoder cannot take the textures capture creates, or `None` when it can.
///
/// The same question as [`can_encode_with_textures`], with the reason kept, for the caller that
/// has to tell the user. **The usual reason is the adapter, not the encoder**: capture's device is
/// on the default adapter, an encoder can only be handed textures on its own adapter, and on a
/// machine whose default GPU is NVIDIA's and which also has an Intel iGPU, `vendor = "qsv"` has a
/// perfectly good encoder that cannot see a single captured frame. That is not a fault of the
/// machine, and the answer is not another GPU's encoder in its place (that would record on a GPU
/// the user did not choose) nor a copy of every frame across the PCIe bus (that is what zero-copy
/// exists to avoid): it is the ffmpeg path, whose `h264_qsv` receives frames over a pipe and does
/// not care which adapter the capture ran on. The recorder takes that path by itself when
/// `encode.zero_copy` is unset, and refuses with this reason when it was set to `true`.
pub fn textures_refusal(
    vendor: Vendor,
    codec: VideoCodec,
    size: (u32, u32),
    bitrate_kbps: u32,
    fps: u32,
) -> Option<String> {
    let device = match create_recording_device() {
        Ok(device) => device,
        Err(err) => return Some(format!("no D3D11 device for capture's own kind: {err:#}")),
    };
    // BGRA, because that is what capture delivers. `None` used to be passed, which offered NV12 to
    // an encoder that would then be fed BGRA: the question this answers is "will the recording's
    // textures be encoded", and an NV12-only encoder cannot (see [`candidate_inputs`]).
    MftEncoder::open(
        &device,
        size,
        Some(DXGI_FORMAT_B8G8R8A8_UNORM),
        MftEncoderParams {
            codec,
            bitrate_kbps,
            fps,
            // The probe asks whether the encoder will open at all, not how it will rate-control,
            // so it uses the mode every build used: constant bitrate.
            mode: RateControl::Cbr,
            // The vendor the recording will ask for, so "yes" means *that* GPU's encoder opens on
            // the device capture creates, not that some encoder here would.
            vendor: Some(vendor),
        },
    )
    .err()
    .map(|err| format!("{err:#}"))
}

/// A BGRA texture of `size` on `device`, standing in for a captured frame.
///
/// Zero-filled (pixel values do not matter). Created with the bind flags WGC's own frames carry,
/// so a refusal on those grounds cannot be mistaken for a format refusal.
///
/// `format` is the format the encoder's input type names, because the media type and the
/// surface have to agree: a texture in another format is refused at `ProcessInput` with
/// nothing to say why.
///
/// Public because the throughput probe needs one, and because a texture created here is the
/// only kind of input the probe can give: a captured one would need a live capture session, and
/// this machine runs an anti-cheat.
pub fn synthetic_capture_texture(
    device: &ID3D11Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `texture` is the out-parameter and the description outlives the call.
    unsafe { device.CreateTexture2D(&description, None, Some(&mut texture)) }
        .context("ID3D11Device::CreateTexture2D")?;
    texture.context("CreateTexture2D returned no texture")
}

/// The DXGI format that goes with an input type the encoder accepted.
///
/// The names are the ones [`MftEncoder`] reports. `RGB32` (X8R8G8B8) and `ARGB32` (A8R8G8B8) are
/// both little-endian 32-bit words with blue in the lowest byte, so in memory they are the byte
/// order `B8G8R8A8` names: that is what WGC delivers, and the format the live path hands the
/// ARGB32-taking NVIDIA encoder. This used to map `ARGB32` to `R8G8B8A8`, which swaps red and blue
/// and made the throughput probe measure a surface the recording never feeds. `NV12` is the planar
/// one a GPU video processor would have to produce.
fn dxgi_for_input(input_format: &str) -> DXGI_FORMAT {
    if input_format.starts_with("NV12") {
        DXGI_FORMAT_NV12
    } else {
        DXGI_FORMAT_B8G8R8A8_UNORM
    }
}

/// What the hardware encoder was measured to do.
#[derive(Debug, Clone, PartialEq)]
pub struct MftThroughput {
    /// Frames the encoder finished per second, over [`MftThroughput::window`].
    pub fps: f64,
    /// Frames it finished inside the measured window.
    pub frames: u64,
    /// How long that window lasted, from the first frame it finished to the last.
    pub window: Duration,
    /// Which encoder was measured.
    pub encoder_name: String,
    /// Which input format it took. The texture's own format, so the pixels never leave the GPU.
    pub input_format: String,
    /// How many times the encoder changed its output type and was renegotiated.
    ///
    /// Reported because it changes what the number means: a rate measured across a renegotiation
    /// is the encoder's rate on both sides of it, and a reader who knows it happened can trust it
    /// more than one who does not.
    pub renegotiations: u32,
}

/// How many textures the probe rotates through, one per fed frame.
///
/// The recording hands the encoder the texture it just captured, so every frame it encodes is a
/// different resource. The probe used to hand over the *same* texture on every frame: legal (the
/// sample is refcounted and the encoder does not write to it) but not what the recording does, and
/// the one difference between the probe and the recording that is on the probe's side of the
/// boundary. A small rotating pool reproduces "a different surface each frame" without holding a
/// frame buffer: these are GPU surfaces, zero-filled, and the encoder is their only reader. Four
/// is enough that a texture the encoder still references is not the next one handed over.
const PROBE_TEXTURES: usize = 4;

/// How long the encoder is given to settle before the measured window opens.
///
/// The first frames of an encode session are not representative of the rest: the session is
/// opened, the first frame pays for the opening, and the GPU may still be at the clock it
/// idled at. A window that opened on those frames would report the slowest frames of the run.
const WARM_UP: Duration = Duration::from_millis(200);

/// How much longer than the window the probe may take before it is called wedged.
///
/// The window is the encoder's own pace, so it ends when it ends; this bounds the case where
/// it does not end at all.
const PROBE_SLACK: Duration = Duration::from_secs(5);

/// How many pause instructions the probe spins between yields.
///
/// The wait for the encoder's next request for input is a spin, not a sleep: a sleep's
/// granularity (Windows: about a millisecond) would be added to every frame, and the
/// measured rate would then depend on where the encoder's own frame time happened to fall
/// relative to the sleep. Yielding every so often keeps the machine's other threads (the
/// encoder's own, and the game) from waiting behind the spin.
const SPIN_ROUNDS: u32 = 4_096;

/// What to say about the type changes an encoder made, in a failure message.
///
/// Empty when there were none, so a message that has nothing to add does not carry a sentence
/// about nothing. Said at all when there were: a probe that failed after the encoder changed its
/// type failed for a reason the reader cannot guess, and "the encoder changed its type" is the
/// difference between a machine that is too slow and a machine that renegotiated.
fn renegotiation_note(renegotiated: u32) -> String {
    if renegotiated == 0 {
        String::new()
    } else {
        format!(
            " The encoder changed its output type {renegotiated} time(s) and the stream was \
             renegotiated."
        )
    }
}

/// What the encoder says about itself when the probe's exchange fails.
///
/// The COM error names the call that failed, not the reason. An encoder that answers
/// `0x8000FFFF` ("catastrophic failure") to `ProcessInput` has usually already told its event
/// queue what it wanted and been refused, and the probe used to report the error alone and discard
/// the rest. What can still be asked, after a failure, is the state the transform is in:
///
/// * whether it is accepting input (`MFT_INPUT_STATUS_ACCEPT_DATA`), which separates "the encoder
///   refused this texture" from "the encoder was already busy";
/// * whether a sample is ready (`MFT_OUTPUT_STATUS_SAMPLE_READY`), which separates "the exchange
///   stopped reading output" from "the encoder never produced any";
/// * which encoder it was and how many times it renegotiated its type.
///
/// All read-only queries on a live transform, so they are safe to ask on the failure path.
fn probe_failure_note(encoder: &MftEncoder, fed: u64, finished: u64) -> String {
    // SAFETY: both are queries on this transform's own streams; neither changes its state.
    let input = unsafe { encoder.transform.GetInputStatus(0) }
        .map(|flags| {
            if flags & MFT_INPUT_STATUS_ACCEPT_DATA.0 as u32 != 0 {
                "accepting input".to_string()
            } else {
                format!("not accepting input (0x{flags:08X})")
            }
        })
        .unwrap_or_else(|err| format!("unreadable (0x{:08X})", err.code().0));
    // SAFETY: as above, output stream 0 is the encoder's only one.
    let output = unsafe { encoder.transform.GetOutputStatus() }
        .map(|flags| {
            if flags & MFT_OUTPUT_STATUS_SAMPLE_READY.0 as u32 != 0 {
                "a sample is ready".to_string()
            } else {
                format!("no sample ready (0x{flags:08X})")
            }
        })
        .unwrap_or_else(|err| format!("unreadable (0x{:08X})", err.code().0));
    format!(
        "the hardware encoder ({} at {}x{}) failed after {fed} frame(s) handed over and          {finished} finished: it is {input}, and {output}.{}",
        encoder.encoder_name,
        encoder.size.0,
        encoder.size.1,
        renegotiation_note(encoder.renegotiated)
    )
}

/// Measure how many frames per second this machine's hardware encoder encodes at `size`.
///
/// This is the transform the recording uses, fed the kind of frame the recording feeds it: a
/// GPU texture on a device of the kind capture creates. No window is opened, nothing is
/// captured and no input is synthesised, which matters because the machine runs an anti-cheat.
///
/// `budget` is how long the measured window lasts. The window is bounded by the encoder's own
/// pace and opens on the first frame it *finishes* after the warm-up, so the number is the
/// encoder's rate rather than the rate at which this probe managed to hand it frames.
///
/// The measurement is of the encoder alone. It is not a promise about the pipeline: the
/// pipeline also captures, copies and muxes, and the encoder's drop counter remains the report
/// of what the pipeline achieved.
pub fn measure_encode_fps(
    size: (u32, u32),
    codec: VideoCodec,
    bitrate_kbps: u32,
    fps: u32,
    mode: RateControl,
    vendor: Vendor,
    budget: Duration,
) -> Result<MftThroughput> {
    let device = create_recording_device()?;
    // The encoder is opened first, because the texture has to be in the format its input type
    // names, and the input type is what the encoder decides. The preferred format is BGRA,
    // which is what the capture backend hands over.
    let mut encoder = MftEncoder::open(
        &device,
        size,
        Some(DXGI_FORMAT_B8G8R8A8_UNORM),
        MftEncoderParams {
            codec,
            bitrate_kbps,
            fps,
            mode,
            // The rate of the encoder the recording will use: another GPU's encoder has another rate.
            vendor: Some(vendor),
        },
    )?;
    let encoder_name = encoder.encoder_name.clone();
    let input_format = encoder.input_format.clone();
    // One texture per fed frame (see `PROBE_TEXTURES`): the recording hands the encoder a fresh
    // captured surface every frame, and a probe that re-used one was not measuring that.
    let format = dxgi_for_input(&input_format);
    let textures = (0..PROBE_TEXTURES)
        .map(|_| synthetic_capture_texture(&device, size, format))
        .collect::<Result<Vec<_>>>()?;
    encoder.start()?;

    // The exchange, counted. `frames` counts what the encoder *finished*, which is the only
    // count that is not this probe's own doing: a frame handed over is a frame the encoder has
    // not encoded yet.
    let mut frames = 0u64;
    let mut fed = 0u64;
    // The measured window: the moment the encoder finished its first frame after the warm-up,
    // and the moment it finished its last.
    let mut window: Option<(Instant, Instant)> = None;
    let mut scratch: Vec<u8> = Vec::new();
    let warm_until = Instant::now() + WARM_UP;
    let deadline = warm_until + budget + PROBE_SLACK;
    let mut spins = 0u32;

    loop {
        if Instant::now() >= deadline {
            bail!(
                "the hardware encoder did not finish a {budget:?} window: it was handed {fed} \
                 frames and finished {frames}, and the deadline (a {WARM_UP:?} warm-up, the \
                 window and a {PROBE_SLACK:?} slack) passed.{} This is a bound, not a measurement",
                renegotiation_note(encoder.renegotiated)
            );
        }
        let event = encoder
            .next_event()
            .with_context(|| probe_failure_note(&encoder, fed, frames))?;
        match event {
            Some(EncoderEvent::NeedInput) => {
                // The probe has no capture behind it, so it makes up a capture time. The interval
                // is the one the encoder will actually be fed at — a frame per 1/fps — because
                // the timeline is what the rate control budgets against: frames a second apart
                // told the encoder this stream was one frame per second while `fps` said
                // otherwise, which is not the recording's stream either way.
                fed = fed.saturating_add(1);
                let captured_at = frame_time(fed - 1, fps);
                let texture = &textures[(fed as usize - 1) % textures.len()];
                encoder
                    .push_texture(texture, captured_at)
                    .with_context(|| probe_failure_note(&encoder, fed, frames))?;
            }
            Some(EncoderEvent::HaveOutput) => {
                let produced = encoder
                    .take_output(&mut scratch)
                    .with_context(|| probe_failure_note(&encoder, fed, frames))?;
                // The bytes are not the measurement; the count is. Dropped so the probe does
                // not hold a window's worth of encoded video.
                scratch.clear();
                let now = Instant::now();
                if now >= warm_until && produced.is_some() {
                    frames = frames.saturating_add(1);
                    let start = window.map_or(now, |(start, _)| start);
                    window = Some((start, now));
                    if now.duration_since(start) >= budget {
                        break;
                    }
                }
            }
            Some(EncoderEvent::DrainComplete | EncoderEvent::Other) => {}
            // The encoder is busy with the frame it was given. Spin: see `SPIN_ROUNDS`.
            None => {
                spins = spins.saturating_add(1);
                if spins >= SPIN_ROUNDS {
                    spins = 0;
                    std::thread::yield_now();
                } else {
                    std::hint::spin_loop();
                }
            }
        }
    }

    let Some((start, end)) = window else {
        bail!(
            "the hardware encoder finished no frames to measure ({size:?} at {fps}fps): it was \
             handed {fed} frames and none of them came back.{} The encoder is not usable at this \
             size, so no rate can be declared for it",
            renegotiation_note(encoder.renegotiated)
        );
    };
    let window = end.duration_since(start);
    let Some(fps) = crate::throughput::frames_per_second(frames, window) else {
        bail!(
            "the hardware encoder finished {frames} frames in a window too short to divide \
             ({window:?}), which is not a rate"
        );
    };
    Ok(MftThroughput {
        fps,
        frames,
        window,
        encoder_name,
        input_format,
        renegotiations: encoder.renegotiated,
    })
}

/// Every hardware video encoder MFT this machine admits to, with no type filter at all: whether
/// an encoder for a codec exists here independent of what it will consume or produce.
pub fn list_hardware_video_encoders() -> Result<Vec<String>> {
    enumerate_names(MFT_CATEGORY_VIDEO_ENCODER)
}

/// Every adapter DXGI lists, in order (the first is what a null-adapter device gets), so "which GPU" is a list, not a guess.
pub fn describe_adapters() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return names;
    };
    let mut index = 0u32;
    // `EnumAdapters` fails rather than returning null once the index runs off the end.
    while let Ok(adapter) = unsafe { factory.EnumAdapters(index) } {
        names.push(adapter_name(&adapter));
        index += 1;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendor of the adapter a null-adapter device gets, which is the one capture's device is
    /// on and so the one a recording's encoder must be from.
    fn default_adapter_vendor() -> Option<Vendor> {
        let device = create_recording_device().ok()?;
        Vendor::from_pci_vendor_id(device_adapter_desc(&device).ok()?.VendorId)
    }

    /// The probe's answers for the adapter capture's device is on (what the tests that open an
    /// encoder on the default device ask about): the encoders of the other adapters are not
    /// candidates for it, and no longer in its answer.
    fn default_adapter_encoders(codec: VideoCodec) -> Vec<HardwareEncoder> {
        let Ok(device) = create_recording_device() else {
            return Vec::new();
        };
        let name = adapter_description(&device);
        probe_hardware_encoders(codec)
            .expect("the machine can be asked")
            .into_iter()
            .filter(|encoder| encoder.adapter == name)
            .collect()
    }

    /// What this machine offers, and whether it takes the capture path's own D3D11 device.
    ///
    /// Reports, and asserts only platform facts: the enumeration must work and every MFT returned must
    /// have accepted the handshake or said why not. Requiring an encoder to exist would fail on a
    /// machine that legitimately has none.
    /// 4K at 60 needs level 5.2 and an H.264 MFT refuses a type whose level cannot carry it; 4K at
    /// 30 and 1440p at 60 still fit 5.1.
    #[test]
    fn the_h264_level_follows_the_declared_rate() {
        assert_eq!(h264_level((3840, 2160), 60), eAVEncH264VLevel5_2.0 as u32);
        assert_eq!(h264_level((3840, 2160), 30), eAVEncH264VLevel5_1.0 as u32);
        assert_eq!(h264_level((2560, 1440), 60), eAVEncH264VLevel5_1.0 as u32);
        assert_eq!(h264_level((1920, 1080), 60), eAVEncH264VLevel5_1.0 as u32);
    }

    #[test]
    fn the_hardware_encoders_are_enumerable_and_answer_the_d3d11_handshake() {
        let encoders = probe_hardware_encoders(VideoCodec::H264).expect("the machine can be asked");
        eprintln!(
            "hardware H.264 encoder MFTs on this machine: {}",
            encoders.len()
        );
        for encoder in &encoders {
            eprintln!(
                "  {:<44} d3d11={:<5} async={:<5} url={}",
                encoder.name,
                encoder.accepts_d3d11,
                encoder.asynchronous,
                encoder.hardware_url.as_deref().unwrap_or("-"),
            );
            if let Some(why) = &encoder.refusal {
                eprintln!("      refused: {why}");
            }
        }
        for encoder in &encoders {
            assert!(
                encoder.accepts_d3d11 || encoder.refusal.is_some(),
                "{} neither accepted the handshake nor gave a reason, which would make the \
                 answer unusable either way",
                encoder.name
            );
        }
    }

    /// The hardware encoder's own rate, measured the way the recording will use it, and the
    /// same way on every run.
    ///
    /// The stability is the point. The raw-pixel probe this replaced reported 46.3, 59.6, 64.7
    /// and 85.7 fps in four runs of an idle 4K machine, and a number that moves like that
    /// cannot be a pace: it was written into both the capture pacer and the encoder's
    /// `-framerate`.
    ///
    /// At 4K because that is the size the complaint was about, and because the encoder's own
    /// frame is long enough there that the measurement is the encoder's pace and not the
    /// probe's.
    ///
    /// A machine with no hardware encoder that takes a GPU texture has nothing to measure,
    /// and that is a finding rather than a failure: the test says so and stops.
    #[test]
    fn the_hardware_encoders_rate_is_measured_the_same_way_on_every_run() {
        const RUNS: usize = 3;
        let size = (3840u32, 2160u32);
        let encoders = default_adapter_encoders(VideoCodec::H264);
        if !encoders.iter().any(|encoder| encoder.accepts_d3d11) {
            eprintln!(
                "this machine has no hardware encoder that takes a GPU texture: there is \
                       nothing to measure"
            );
            return;
        }

        let mut rates = Vec::new();
        for _ in 0..RUNS {
            let measured = measure_encode_fps(
                size,
                VideoCodec::H264,
                20_000,
                60,
                RateControl::Cbr,
                default_adapter_vendor().expect("the default adapter is one of the three vendors'"),
                Duration::from_millis(500),
            )
            .expect("a hardware encoder that takes a texture can be measured");
            eprintln!(
                "the hardware encoder did {:.1}fps at {size:?} ({} frames over {:?}) with {} \
                 taking {}",
                measured.fps,
                measured.frames,
                measured.window,
                measured.encoder_name,
                measured.input_format
            );
            assert!(
                measured.frames > 0 && measured.fps > 0.0,
                "a measurement with no frames in it is not a measurement: {measured:?}"
            );
            rates.push(measured.fps);
        }
        let lowest = rates.iter().copied().fold(f64::INFINITY, f64::min);
        let highest = rates.iter().copied().fold(0.0, f64::max);
        assert!(
            highest <= lowest * 2.0,
            "the rate must not swing by more than two to one between runs on an idle machine: \
             {lowest:.1}fps to {highest:.1}fps over {RUNS} runs at {size:?}"
        );
    }

    /// The hardware encoder publishes a sequence header, and it is the Annex-B form a raw
    /// demuxer can read.
    ///
    /// This is the question issue #78 turns on: the raw H.264 demuxer the child reads has nothing
    /// to probe with until it has seen a keyframe carrying the parameter sets, so the header has
    /// to come from the encoder's output type (`MF_MT_MPEG_SEQUENCE_HEADER`) and go in front of the
    /// bitstream.
    ///
    /// Whether a given encoder publishes it is that encoder's own business — the SDK says the
    /// video encoder adds the attribute to the output type once the type is set, and that the blob
    /// is "concatenated NAL units in Annex B format, along with their start codes" — so this asks
    /// the machine rather than assuming.
    ///
    /// Windows only, and it needs a machine with a hardware encoder that takes a GPU texture: the
    /// calls are Media Foundation's, and there is nothing to ask without an encoder. On a machine
    /// with no such encoder it says so and stops, as the other probes here do.
    #[test]
    fn the_encoder_publishes_a_sequence_header_a_raw_demuxer_can_read() {
        let encoders = default_adapter_encoders(VideoCodec::H264);
        if !encoders.iter().any(|encoder| encoder.accepts_d3d11) {
            eprintln!(
                "this machine has no hardware encoder that takes a GPU texture: there is nothing \
                 to ask for a sequence header"
            );
            return;
        }
        let device = create_capture_kind_device().expect("a capture-kind device");
        let encoder = MftEncoder::open(
            &device,
            WARM_UP_SIZE,
            Some(DXGI_FORMAT_B8G8R8A8_UNORM),
            MftEncoderParams {
                codec: VideoCodec::H264,
                bitrate_kbps: PROBE_BITRATE_KBPS,
                fps: FRAME_RATE.0,
                mode: RateControl::Cbr,
                vendor: None,
            },
        )
        .expect("a hardware encoder that takes a texture");

        let header = encoder
            .sequence_header()
            .expect("the encoder publishes its sequence header");
        eprintln!(
            "the encoder's sequence header is {} bytes: {}",
            header.len(),
            header
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(
            !header.is_empty(),
            "a sequence header with nothing in it is not one: the raw demuxer would have nothing \
             to probe with"
        );
        assert!(
            crate::is_annex_b(&header),
            "the header has to be Annex-B, because the bitstream the child reads is: {:02x?}",
            header
        );
    }

    /// The codes the crate root classifies are the SDK's own codes.
    ///
    /// The classification lives in the crate root, where the `windows` crate is not available at
    /// all, so the numbers are written out there. This is where they are checked against the SDK:
    /// a code that was written down wrong would classify every error as a failure, which is the
    /// bug this whole change is about.
    #[test]
    fn the_output_failure_codes_are_the_sdk_s_own() {
        use windows::Win32::Media::MediaFoundation::{
            MF_E_TRANSFORM_NEED_MORE_INPUT as SDK_NEED_MORE_INPUT,
            MF_E_TRANSFORM_STREAM_CHANGE as SDK_STREAM_CHANGE,
        };
        assert_eq!(
            crate::MF_STREAM_CHANGE,
            SDK_STREAM_CHANGE.0 as u32,
            "the stream-change code the crate classifies is the SDK's"
        );
        assert_eq!(
            crate::MF_NEED_MORE_INPUT,
            SDK_NEED_MORE_INPUT.0 as u32,
            "the need-more-input code the crate classifies is the SDK's"
        );
    }

    /// The renegotiation is a legal sequence of calls on a live encoder, and the encoder keeps
    /// encoding afterwards.
    ///
    /// This is the path a type change takes: `ProcessOutput` answers
    /// `MF_E_TRANSFORM_STREAM_CHANGE` (0xC00D6D61, "a stream change has occurred. Output cannot
    /// be produced until the streams have been renegotiated"), the output type is set again from
    /// the types the encoder itself offers, the input type is set again with it, and the exchange
    /// carries on. Before this, the type change ended the exchange: the probe could not measure,
    /// and the recording stopped encoding.
    ///
    /// Windows only, and it needs a machine with a hardware encoder that takes a GPU texture: the
    /// calls are Media Foundation's, and there is nothing to renegotiate without an encoder.
    ///
    /// What it cannot prove is that a particular encoder will change its type, or when: that is
    /// the machine's business, and the machine that reported the failure is the one that would
    /// show it. What it does prove is that the renegotiation itself works, on a live transform,
    /// which is the part that had never been run.
    #[test]
    fn an_encoder_can_be_renegotiated_and_keeps_encoding_afterwards() {
        let encoders = default_adapter_encoders(VideoCodec::H264);
        if !encoders.iter().any(|encoder| encoder.accepts_d3d11) {
            eprintln!(
                "this machine has no hardware encoder that takes a GPU texture: there is nothing \
                 to renegotiate"
            );
            return;
        }
        // A size the encoder is known to take: the warm-up size the HEVC path uses.
        let size = WARM_UP_SIZE;
        let device = create_capture_kind_device().expect("a capture-kind device");
        let mut encoder = MftEncoder::open(
            &device,
            size,
            Some(DXGI_FORMAT_B8G8R8A8_UNORM),
            MftEncoderParams {
                codec: VideoCodec::H264,
                bitrate_kbps: PROBE_BITRATE_KBPS,
                fps: FRAME_RATE.0,
                mode: RateControl::Cbr,
                vendor: None,
            },
        )
        .expect("a hardware encoder that takes a texture");

        // The renegotiation itself: the type the encoder offers, then the input type again.
        encoder
            .renegotiate_output_type()
            .expect("the encoder offers a type it can be renegotiated to");
        assert_eq!(
            encoder.renegotiated, 1,
            "the renegotiation is counted, so a probe that failed later can say it happened"
        );

        // And the encoder still encodes afterwards: a renegotiation that broke the encoder would
        // be worse than the type change it was meant to answer. This is the exchange the
        // recording uses, not the probe's.
        let texture =
            synthetic_capture_texture(&device, size, dxgi_for_input(&encoder.input_format))
                .expect("a texture in the format the encoder takes");
        encoder.start().expect("the encoder starts");
        let mut produced = 0usize;
        for index in 0..10u64 {
            // A capture time a frame apart, so the encoder is handed a timeline rather than a
            // count: the frames it is given were captured 33ms apart.
            let captured_at = Duration::from_millis(index * 33);
            let bytes = encoder
                .encode_texture(&texture, captured_at, Duration::from_secs(2))
                .expect("the encoder takes the frame");
            produced += bytes.len();
        }
        let mut tail = Vec::new();
        encoder
            .finish(Duration::from_secs(2), &mut tail)
            .expect("the drain");
        produced += tail.len();
        assert!(
            produced > 0,
            "the encoder produced no bitstream after the renegotiation, so it did not survive it"
        );
    }
    // ---- the fixes from the production review ----------------------------------------

    /// A BGRA texture is never offered an NV12 input type, and an NV12 texture only that.
    ///
    /// Nothing converts between the two, so offering NV12 for a BGRA texture produced an encoder
    /// that accepted the type and was fed the wrong surface.
    #[test]
    fn an_input_type_is_only_offered_for_the_texture_it_describes() {
        let names = |format| {
            candidate_inputs(format)
                .into_iter()
                .map(|(_, name)| name)
                .collect::<Vec<_>>()
        };
        for bgra in [Some(DXGI_FORMAT_B8G8R8A8_UNORM), None] {
            let offered = names(bgra);
            assert_eq!(offered.len(), 2, "RGB32 and ARGB32: {offered:?}");
            assert!(
                !offered.iter().any(|name| name.starts_with("NV12")),
                "NV12 must not be offered for BGRA: {offered:?}"
            );
        }
        assert_eq!(names(Some(DXGI_FORMAT_NV12)), vec!["NV12"]);
        assert!(
            names(Some(
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT
            ))
            .is_empty(),
            "a surface this pipeline cannot describe is refused, not mislabelled"
        );
    }

    /// `ARGB32` and `RGB32` are both BGRA in memory, and the probe's texture must say so.
    #[test]
    fn argb32_is_bgra_in_memory() {
        assert_eq!(dxgi_for_input("ARGB32"), DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(
            dxgi_for_input("RGB32 (BGRA byte order — what WGC delivers)"),
            DXGI_FORMAT_B8G8R8A8_UNORM
        );
        assert_eq!(dxgi_for_input("NV12"), DXGI_FORMAT_NV12);
    }

    /// Variable bitrate has room above its mean; constant does not. Pinned equal, VBR was CBR.
    #[test]
    fn a_vbr_ceiling_is_above_the_mean() {
        assert_eq!(vbr_ceiling(20_000_000), 30_000_000);
        assert!(vbr_ceiling(1) >= 1);
        assert_eq!(
            vbr_ceiling(u32::MAX),
            u32::MAX,
            "it saturates, it does not wrap"
        );
    }

    /// Enumerating and letting go can be done again and again.
    ///
    /// The activation objects used to be leaked on every enumeration, and a failed open
    /// enumerated at the capture rate. Not a proof of no leak (that would be a handle count),
    /// but it is the sequence that crashed when the shutdown was done in the wrong order, and
    /// it runs the whole Drop chain: activation, then session.
    #[test]
    fn enumerating_encoders_repeatedly_releases_them() {
        let _session = MfSession::start().expect("Media Foundation starts");
        for _ in 0..20 {
            let found = enumerate_activates(MFT_CATEGORY_VIDEO_ENCODER).expect("enumeration works");
            for activation in &found {
                let _ = attribute_string(activation, &MFT_FRIENDLY_NAME_Attribute);
            }
        }
    }

    /// A feed whose encoder cannot be opened says so, instead of the recording carrying on.
    ///
    /// The texture is a format this pipeline refuses, so the open fails on any machine, with or
    /// without a hardware encoder. After [`OPEN_ATTEMPTS`] attempts the thread gives up and
    /// `submit` returns the reason: that is what turns a dead encoder into the recording's error.
    #[test]
    fn a_feed_that_cannot_open_an_encoder_fails_the_submit() {
        let Ok(device) = create_recording_device() else {
            eprintln!("no D3D11 device on this machine: nothing to submit a texture on");
            return;
        };
        let texture = synthetic_capture_texture(
            &device,
            WARM_UP_SIZE,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
        )
        .expect("a float texture");
        let (out, _kept) = std::sync::mpsc::sync_channel(4);
        let mut feed = MftFeed::spawn(
            MftEncoderParams {
                codec: VideoCodec::H264,
                bitrate_kbps: PROBE_BITRATE_KBPS,
                fps: FRAME_RATE.0,
                mode: RateControl::Cbr,
                vendor: None,
            },
            out,
        );
        let started = Instant::now();
        let failure = loop {
            match feed.submit(GpuTexture::new(texture.clone()), Duration::ZERO) {
                Ok(()) => {}
                Err(err) => break format!("{err:#}"),
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the feed never reported that its encoder could not be opened"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            failure.contains("hardware encoder stopped"),
            "the error names the encoder: {failure}"
        );
        assert!(
            feed.failure().is_some(),
            "and the reason is kept for finish() to report"
        );
        assert!(
            started.elapsed() >= OPEN_RETRY_PAUSE,
            "it retried (with a pause) before giving up, not on the first failure"
        );
        feed.shutdown();
    }

    /// The drain ends when the encoder says it is done, not when the deadline passes.
    #[test]
    fn finish_returns_at_the_drain_complete_event() {
        let encoders = default_adapter_encoders(VideoCodec::H264);
        if !encoders.iter().any(|encoder| encoder.accepts_d3d11) {
            eprintln!("no hardware encoder that takes a GPU texture: nothing to drain");
            return;
        }
        let device = create_recording_device().expect("a recording device");
        let mut encoder = MftEncoder::open(
            &device,
            WARM_UP_SIZE,
            Some(DXGI_FORMAT_B8G8R8A8_UNORM),
            MftEncoderParams {
                codec: VideoCodec::H264,
                bitrate_kbps: PROBE_BITRATE_KBPS,
                fps: FRAME_RATE.0,
                mode: RateControl::Cbr,
                vendor: None,
            },
        )
        .expect("a hardware encoder that takes a BGRA texture");
        let texture = synthetic_capture_texture(&device, WARM_UP_SIZE, DXGI_FORMAT_B8G8R8A8_UNORM)
            .expect("a texture");
        encoder.start().expect("the encoder starts");
        let mut frames = 0usize;
        for index in 0..10u64 {
            frames += encoder
                .encode_texture(
                    &texture,
                    Duration::from_millis(index * 33),
                    Duration::from_secs(2),
                )
                .expect("the encoder takes the frame")
                .len();
        }
        let mut tail = Vec::new();
        let started = Instant::now();
        encoder
            .finish(Duration::from_secs(2), &mut tail)
            .expect("the drain completes and says so");
        let took = started.elapsed();
        eprintln!(
            "drain: {} frame(s) before, {} in the tail, took {took:?}",
            frames,
            tail.len()
        );
        assert!(
            took < Duration::from_millis(1_500),
            "the drain waited for the deadline ({took:?}) instead of the drain-complete event"
        );
        assert!(
            frames + tail.len() >= 10,
            "every frame handed over comes out: {} + {}",
            frames,
            tail.len()
        );
    }

    /// An encoder can be opened, used and let go of again and again in one process.
    ///
    /// Letting go of an asynchronous encoder used to destroy it while a work item it had queued on
    /// Media Foundation's work queue was still to run: the item then ran inside a freed object
    /// (`EnterCriticalSection` on it, an access violation on a work-queue thread). The MFT suite
    /// died that way in about one run in three, in whichever test happened to close an encoder at
    /// the wrong moment, and the recorder closes encoders over a session too (a resize, a failed
    /// open retried). Half the cycles drain first, as a clean stop does; the other half are let go
    /// of mid-stream, as an error or a resize does.
    #[test]
    fn an_encoder_can_be_closed_and_reopened_many_times_in_one_process() {
        let encoders = default_adapter_encoders(VideoCodec::H264);
        if !encoders.iter().any(|encoder| encoder.accepts_d3d11) {
            eprintln!("no hardware encoder that takes a GPU texture: nothing to close");
            return;
        }
        let device = create_recording_device().expect("a recording device");
        let texture = synthetic_capture_texture(&device, WARM_UP_SIZE, DXGI_FORMAT_B8G8R8A8_UNORM)
            .expect("a texture");
        for cycle in 0..60u64 {
            let mut encoder = MftEncoder::open(
                &device,
                WARM_UP_SIZE,
                Some(DXGI_FORMAT_B8G8R8A8_UNORM),
                MftEncoderParams {
                    codec: VideoCodec::H264,
                    bitrate_kbps: PROBE_BITRATE_KBPS,
                    fps: FRAME_RATE.0,
                    mode: RateControl::Cbr,
                    vendor: None,
                },
            )
            .unwrap_or_else(|err| panic!("cycle {cycle}: the encoder opens again: {err:#}"));
            encoder.start().expect("the encoder starts");
            for index in 0..3u64 {
                encoder
                    .encode_texture(
                        &texture,
                        Duration::from_millis(index * 33),
                        Duration::from_secs(2),
                    )
                    .unwrap_or_else(|err| {
                        panic!("cycle {cycle}: the encoder takes a frame: {err:#}")
                    });
            }
            if cycle % 2 == 0 {
                let mut tail = Vec::new();
                encoder
                    .finish(Duration::from_secs(2), &mut tail)
                    .unwrap_or_else(|err| panic!("cycle {cycle}: the drain: {err:#}"));
            }
            drop(encoder);
        }
    }

    /// The device the recording's questions are asked on is created the way capture creates its own,
    /// and an encoder opens on it. (H.264 does not need `VIDEO_SUPPORT`; the diagnostic device has it.)
    #[test]
    fn an_encoder_opens_on_the_device_capture_creates() {
        let encoders = default_adapter_encoders(VideoCodec::H264);
        if !encoders.iter().any(|encoder| encoder.accepts_d3d11) {
            return;
        }
        assert!(
            can_encode_with_textures(
                default_adapter_vendor().expect("the default adapter is one of the three vendors'"),
                VideoCodec::H264,
                WARM_UP_SIZE,
                PROBE_BITRATE_KBPS,
                30
            ),
            "an H.264 encoder that took the handshake opens on capture's own kind of device with BGRA textures"
        );
    }
    // ---- one adapter at a time: the Quick Sync finding --------------------------------

    /// Whether this process is in session 0, which is where an SSH login or a service lands.
    ///
    /// Intel's Quick Sync MFTs cannot be created there: `ActivateObject` (and a bare `CoCreateInstance`
    /// of the MFT's CLSID) answers `0x80004005`, the same MFTs create and encode from the desktop
    /// session, and NVIDIA's do both from either. It is a property of the session and not of the
    /// product, so the tests that need a Quick Sync encoder skip in session 0 and say why, instead of
    /// reporting a driver limitation that is not there.
    fn in_session_zero() -> bool {
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        let mut session = 0u32;
        // SAFETY: `session` is the out-parameter and the process id is this process's own.
        unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }.is_ok() && session == 0
    }

    /// The first hardware adapter of each vendor this app encodes with. A machine can list one GPU
    /// more than once (virtual display drivers each add an adapter on the same card), and one
    /// representative says as much as three.
    fn adapters_by_vendor() -> Vec<(Vendor, HardwareAdapter)> {
        let mut found: Vec<(Vendor, HardwareAdapter)> = Vec::new();
        for adapter in hardware_adapters().expect("DXGI lists adapters") {
            // SAFETY: `GetDesc1` is this adapter's own method.
            let Ok(desc) = (unsafe { adapter.adapter.GetDesc1() }) else {
                continue;
            };
            let Some(vendor) = Vendor::from_pci_vendor_id(desc.VendorId) else {
                continue;
            };
            if !found.iter().any(|(known, _)| *known == vendor) {
                found.push((vendor, adapter));
            }
        }
        found
    }

    fn params_for(vendor: Option<Vendor>) -> MftEncoderParams {
        MftEncoderParams {
            codec: VideoCodec::H264,
            bitrate_kbps: PROBE_BITRATE_KBPS,
            fps: FRAME_RATE.0,
            mode: RateControl::Cbr,
            vendor,
        }
    }

    /// A BGRA frame with something in it: a gradient that moves, so consecutive frames differ and
    /// an encoder has motion to code rather than a flat field.
    fn moving_gradient_texture(
        device: &ID3D11Device,
        size: (u32, u32),
        index: u32,
    ) -> ID3D11Texture2D {
        let texture = synthetic_capture_texture(device, size, DXGI_FORMAT_B8G8R8A8_UNORM)
            .expect("a BGRA texture");
        let mut pixels = Vec::with_capacity(size.0 as usize * size.1 as usize * 4);
        for y in 0..size.1 {
            for x in 0..size.0 {
                pixels.extend_from_slice(&[
                    (x + 7 * index) as u8,
                    (y * 255 / size.1) as u8,
                    (index * 8) as u8,
                    255,
                ]);
            }
        }
        // SAFETY: the immediate context belongs to `device`; `pixels` is one tightly packed BGRA
        // image of exactly the texture's size and outlives the call.
        unsafe {
            let context = device.GetImmediateContext().expect("an immediate context");
            context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), size.0 * 4, 0);
            context.Flush();
        }
        texture
    }

    /// **A texture is never handed to another GPU's encoder, and one vendor is never answered with
    /// another's.**
    ///
    /// The bug this closes: `MFTEnumEx` lists every adapter's encoders as one list and the encoder
    /// took the first that opened. On a machine with an NVIDIA GPU and an Intel iGPU, `encode.vendor
    /// = "qsv"` passed its ffmpeg smoke test and then recorded with NVENC's MFT, on the NVIDIA
    /// device capture creates, and said nothing. Asked for the other adapter's vendor, the open must
    /// fail, name both the vendor and the adapter the texture is on, and leave the choice of what to
    /// do (the ffmpeg path) to the caller.
    ///
    /// Needs only one GPU to say something (the vendors it does not have are refused), and two to
    /// say it about a real pair. It does not activate a Quick Sync encoder, so it runs in session 0.
    #[test]
    fn an_encoder_is_never_taken_from_another_gpu_than_the_textures() {
        let adapters = adapters_by_vendor();
        if adapters.is_empty() {
            eprintln!("no NVIDIA, Intel or AMD adapter here: nothing to refuse");
            return;
        }
        for (own, adapter) in &adapters {
            let device = create_device_on(
                Some(&adapter.adapter),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                "BGRA_SUPPORT",
            )
            .expect("a device on a hardware adapter");
            for asked in [Vendor::Nvenc, Vendor::Qsv, Vendor::Amf] {
                if asked == *own {
                    continue;
                }
                let refusal = match MftEncoder::open(
                    &device,
                    WARM_UP_SIZE,
                    Some(DXGI_FORMAT_B8G8R8A8_UNORM),
                    params_for(Some(asked)),
                ) {
                    Ok(opened) => panic!(
                        "{} was asked for on {}, and {} was opened instead",
                        asked.as_str(),
                        adapter.name,
                        opened.encoder_name
                    ),
                    Err(err) => format!("{err:#}"),
                };
                eprintln!("{} on {}: {refusal}", asked.as_str(), adapter.name);
                assert!(
                    refusal.contains(asked.as_str()) && refusal.contains(&adapter.name),
                    "the refusal names the vendor asked for and the adapter the texture is on: \
                     {refusal}"
                );
            }
        }
    }

    /// **Each vendor's encoder opens on its own adapter and encodes real frames there.**
    ///
    /// Every adapter whose vendor has an encoder on it gets a device of its own, the encoder is
    /// opened by vendor, and thirty frames of moving content go in as textures on that device and
    /// come out as an Annex-B H.264 stream, one access unit per frame. On the test machine this is
    /// the NVIDIA GPU with NVENC's MFT and the Intel iGPU with Quick Sync's, the latter being the
    /// path `0x80004005` was wrongly blamed on: it was session 0 (see [`in_session_zero`]) and, once
    /// in the desktop session, a device on the wrong adapter, not the driver.
    #[test]
    fn each_vendors_encoder_encodes_real_frames_on_its_own_adapter() {
        const FRAMES: u32 = 30;
        let size = (1280u32, 720u32);
        let adapters = adapters_by_vendor();
        if adapters.is_empty() {
            eprintln!("no NVIDIA, Intel or AMD adapter here: nothing to encode on");
            return;
        }
        for (vendor, adapter) in adapters {
            if vendor == Vendor::Qsv && in_session_zero() {
                eprintln!(
                    "skipping Quick Sync on {}: Intel's encoder MFTs cannot be created in \
                     session 0 (0x80004005); run this from the desktop session",
                    adapter.name
                );
                continue;
            }
            let device = create_device_on(
                Some(&adapter.adapter),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                "BGRA_SUPPORT",
            )
            .expect("a device on a hardware adapter");
            let _session = MfSession::start().expect("Media Foundation starts");
            let has_encoder = enumerate_encoders_for(&device)
                .map(|found| !encoders_for_vendor(found, Some(vendor)).is_empty())
                .unwrap_or(false);
            if !has_encoder {
                eprintln!(
                    "{} has no {} encoder MFT: nothing to encode",
                    adapter.name,
                    vendor.as_str()
                );
                continue;
            }

            let mut encoder = MftEncoder::open(
                &device,
                size,
                Some(DXGI_FORMAT_B8G8R8A8_UNORM),
                params_for(Some(vendor)),
            )
            .unwrap_or_else(|err| panic!("{} on {}: {err:#}", vendor.as_str(), adapter.name));
            assert_eq!(
                adapter_description(encoder.device()),
                adapter.name,
                "the encoder is on the adapter that was asked for"
            );
            encoder.start().expect("the encoder starts");
            let mut frames = Vec::new();
            for index in 0..FRAMES {
                let texture = moving_gradient_texture(&device, size, index);
                frames.extend(
                    encoder
                        .encode_texture(
                            &texture,
                            Duration::from_millis(u64::from(index) * 33),
                            Duration::from_secs(5),
                        )
                        .unwrap_or_else(|err| {
                            panic!(
                                "{} on {}: frame {index} of {FRAMES} ({} out so far): {err:#}",
                                vendor.as_str(),
                                adapter.name,
                                frames.len()
                            )
                        }),
                );
            }
            encoder
                .finish(Duration::from_secs(5), &mut frames)
                .expect("the drain");

            let bytes: usize = frames.iter().map(|frame| frame.bytes.len()).sum();
            eprintln!(
                "{} on {}: {} frames in, {} out, {bytes} bytes, taking {}",
                encoder.encoder_name,
                adapter.name,
                FRAMES,
                frames.len(),
                encoder.input_format
            );
            assert_eq!(
                frames.len(),
                FRAMES as usize,
                "one access unit per frame handed over"
            );
            assert!(
                frames.iter().all(|frame| crate::is_annex_b(&frame.bytes)),
                "every access unit is Annex-B, which is what ffmpeg's raw demuxer reads"
            );
            assert!(bytes > 0, "the stream has content");
        }
    }

    /// **The candidate list for a device holds only encoders that can take its textures.**
    ///
    /// For each GPU, a device is made on it and the encoders offered for that device are read back:
    /// every one of them must say, in its own vendor attribute, that it is that GPU's vendor. On
    /// the test machine that is three NVIDIA adapter listings and one Intel one: the Intel device's
    /// list holds Quick Sync and its siblings and no NVENC, and each NVIDIA device's the reverse.
    /// The flat list `MFTEnumEx` returns held all of them for every device, and the first one that
    /// opened won.
    #[test]
    fn the_candidates_for_a_device_are_its_own_adapters_encoders() {
        let _session = MfSession::start().expect("Media Foundation starts");
        for (vendor, adapter) in adapters_by_vendor() {
            let device = create_device_on(
                Some(&adapter.adapter),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                "BGRA_SUPPORT",
            )
            .expect("a device on a hardware adapter");
            let candidates = enumerate_encoders_for(&device).expect("the adapter can be asked");
            for candidate in &candidates {
                let name =
                    attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
                let owner = attribute_string(candidate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
                    .unwrap_or_default();
                assert!(
                    vendor.names_mft_vendor(&owner),
                    "{name} ({owner}) is offered for a device on {} ({}), and is not that GPU's",
                    adapter.name,
                    vendor.as_str()
                );
            }
            eprintln!(
                "{}: {} candidate(s), all {}'s",
                adapter.name,
                candidates.len(),
                vendor.as_str()
            );
        }
    }

    /// **An encoder on another GPU than capture's is refused with a reason, and never opened.**
    ///
    /// The product-level question the recorder asks before choosing zero-copy
    /// ([`can_encode_with_textures`]): asked for every vendor, the answer is `false` for each
    /// vendor except the one whose GPU capture's device is on, and the reason a refusal carries
    /// names the vendor and says what to do. Runs in session 0, because a refusal never activates
    /// an encoder.
    #[test]
    fn a_vendor_that_is_not_on_the_capture_gpu_is_refused_with_its_reason() {
        let Some(own) = default_adapter_vendor() else {
            eprintln!("capture's device is on no NVIDIA, Intel or AMD GPU: nothing to compare");
            return;
        };
        for asked in [Vendor::Nvenc, Vendor::Qsv, Vendor::Amf] {
            if asked == own {
                continue;
            }
            let reason = textures_refusal(asked, VideoCodec::H264, WARM_UP_SIZE, 20_000, 30)
                .unwrap_or_else(|| {
                    panic!(
                        "{} was said to take textures on a {} device",
                        asked.as_str(),
                        own.as_str()
                    )
                });
            eprintln!("{} on a {} device: {reason}", asked.as_str(), own.as_str());
            assert!(
                reason.contains(asked.as_str()) && reason.contains("ffmpeg"),
                "the reason names the vendor and the path that works: {reason}"
            );
            assert!(
                !can_encode_with_textures(asked, VideoCodec::H264, WARM_UP_SIZE, 20_000, 30),
                "the boolean and the reason cannot disagree"
            );
        }
    }
}
