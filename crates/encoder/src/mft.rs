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
    choose_output_type, classify_output_failure, type_change_refusal, OutputFailure,
    OutputTypeSpec, RateControl, VideoCodec,
};
use anyhow::{bail, Context, Result};
use replayed_capture::GpuTexture;
use std::time::{Duration, Instant};
use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_PeakConstrainedVBR,
    eAVEncH264VLevel5_1, eAVEncH264VLevel5_2, eAVEncH264VProfile_High, eAVEncH265VLevel4_1,
    eAVEncH265VLevel5, eAVEncH265VLevel5_1, eAVEncH265VProfile_Main_420_10,
    eAVEncH265VProfile_Main_420_8, CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFShutdown,
    MFStartup, MFTEnumEx, MFT_ENUM_HARDWARE_URL_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_HEVC_ES,
    MFVideoFormat_NV12, MFVideoFormat_P010, MFVideoFormat_RGB32, MFVideoFormat_YUY2,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_CATEGORY_VIDEO_PROCESSOR, MFT_ENUM_FLAG_HARDWARE, MFT_INPUT_STATUS_ACCEPT_DATA,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STATUS_SAMPLE_READY,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_LEVEL,
    MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};

/// What an encoder asked for, when its events were drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// `METransformNeedInput`: it will take a frame now.
    Input,
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
    /// Anything else (a drain completing, an error event): not this exchange's business.
    Other,
}
use windows::Win32::System::Com::CoTaskMemFree;

/// One hardware video encoder this machine offers, and what it said when asked for the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareEncoder {
    /// The MFT's own name for itself (`MFT_FRIENDLY_NAME_Attribute`).
    pub name: String,
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

/// Every hardware encoder for `codec` on this machine, asked whether it takes a D3D11 device manager.
///
/// Errors only when the question could not be asked (Media Foundation would not start, the
/// enumeration failed). No hardware encoder is an empty list, an answer rather than a failure.
pub fn probe_hardware_encoders(codec: VideoCodec) -> Result<Vec<HardwareEncoder>> {
    // SAFETY: `MFStartup` is the documented first call before any other MF entry point, and it is
    // paired with `MFShutdown` on every path out of this function.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
    let probed = probe_started_encoders(codec);
    // SAFETY: balances the successful `MFStartup` above. A failure here means Media Foundation did
    // not shut down cleanly, which is not a reason to discard an answer already measured.
    unsafe {
        let _ = MFShutdown();
    }
    probed
}

fn probe_started_encoders(codec: VideoCodec) -> Result<Vec<HardwareEncoder>> {
    // No type filter, deliberately. Filtering by output subtype can hide an encoder whose
    // registration does not match its name, and `MFT_ENUM_FLAG_SORTANDFILTER` filters against a
    // preference list that means nothing without a type to prefer for. Asking for HEVC returned the
    // two H.264 encoders and one Quick Sync encoder, while the unfiltered list held "NVIDIA HEVC
    // Encoder MFT" and "Intel(R) Hardware H265 Encoder MFT". The negotiation below is what decides, so
    // the enumeration is complete and each encoder says for itself what it will do.
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;

    // The type to match is what the encoder must produce: which hardware encoders can make this codec.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE,
            None,
            None,
            &mut activates,
            &mut count,
        )
    }
    .context("MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE)")?;

    if activates.is_null() || count == 0 {
        // The array is COM-allocated even when empty, so a non-null empty one still has to be freed;
        // "no hardware encoder" is an answer, not an error.
        if !activates.is_null() {
            // SAFETY: `activates` came from `MFTEnumEx`, which allocates with the COM task
            // allocator, and this is the matching free.
            unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
        }
        return Ok(Vec::new());
    }

    // One device and one manager shared by every candidate, as the encoder will do; a device per
    // candidate would answer a different question.
    let device = create_capture_kind_device()?;
    let manager = create_device_manager(&device)?;

    // SAFETY: `activates` points at `count` initialised `Option<IMFActivate>`s, which is what
    // `MFTEnumEx` just wrote there.
    let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let mut probed = Vec::with_capacity(candidates.len());
    for candidate in candidates.iter().flatten() {
        probed.push(ask_one(candidate, &manager, codec));
    }

    // SAFETY: as above — the array is released here.
    unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
    Ok(probed)
}

/// Ask one enumerated MFT for the handshake, and describe it either way.
fn ask_one(
    activate: &IMFActivate,
    manager: &IMFDXGIDeviceManager,
    codec: VideoCodec,
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
            hardware_url,
            asynchronous,
            accepts_d3d11: true,
            input_subtypes,
            accepts,
            refusal: negotiation,
        },
        Err(err) => HardwareEncoder {
            name,
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

/// The formats worth asking about, in the order worth asking.
///
/// `RGB32` and `ARGB32` first because WGC delivers BGRA8 and either name means the texture goes
/// straight in. `NV12` last: the format a hardware encoder most often wants and the one this
/// pipeline cannot produce without a GPU Video Processor MFT.
/// The input subtype to offer an encoder first, given the texture it will be handed.
///
/// The order is a correctness requirement. `accepts_subtype` asks whether the MFT will take a
/// media type, not the texture that arrives later, so an NV12 texture can be offered "RGB32",
/// accepted, and refused only at `ProcessInput` with nothing to say why. The HEVC encoder refuses
/// an ARGB32 texture outright (`0xC00D6D76`) where the H.264 one accepts it, so which format
/// capture hands over decides whether those encoders work.
///
/// The texture's own format is the honest answer to what to offer first; the rest are fallbacks.
fn preferred_input(format: DXGI_FORMAT) -> Option<&'static windows::core::GUID> {
    match format {
        // WGC hands over BGRA, which Media Foundation calls RGB32: same byte order, different name,
        // which is why both are candidates.
        DXGI_FORMAT_B8G8R8A8_UNORM => Some(&MFVideoFormat_RGB32),
        DXGI_FORMAT_NV12 => Some(&MFVideoFormat_NV12),
        _ => None,
    }
}

/// [`CANDIDATE_INPUTS`] with the wanted one moved to the front.
fn candidate_inputs(
    preferred: Option<DXGI_FORMAT>,
) -> Vec<(&'static windows::core::GUID, &'static str)> {
    let mut candidates = CANDIDATE_INPUTS.to_vec();
    if let Some(wanted) = preferred.and_then(preferred_input) {
        if let Some(index) = candidates
            .iter()
            .position(|(subtype, _)| *subtype == wanted)
        {
            let first = candidates.remove(index);
            candidates.insert(0, first);
        }
    }
    candidates
}

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
    // The buffer (one second of the rate) and the ceiling complete the CBR contract. Low-latency mode
    // stays on: turning it off moved the rate by less than run-to-run noise and produced no B-frames
    // (the B-frame count itself is refused, `E_INVALIDARG`), so there was nothing to keep it for (§24).
    let bits = bitrate_kbps.saturating_mul(1_000);
    set_codec_api_value(
        &api,
        &CODECAPI_AVEncCommonMaxBitRate,
        bits,
        "maximum bitrate (bit/s)",
    );
    set_codec_api_value(
        &api,
        &CODECAPI_AVEncCommonBufferSize,
        bits,
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
    }
    .context("MFTEnumEx for a category")?;
    if activates.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: `activates` points at `count` initialised `Option<IMFActivate>`s written by the call
    // above, and the array is released with the matching free below.
    let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let names = candidates
        .iter()
        .flatten()
        .map(|candidate| {
            attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string())
        })
        .collect();
    // SAFETY: as above.
    unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
    Ok(names)
}

/// A D3D11 device of the kind the capture backend creates.
///
/// `BGRA_SUPPORT | VIDEO_SUPPORT`, and the second is not optional for HEVC. BGRA is what WGC
/// requires; VIDEO_SUPPORT is what a Media Foundation encoder asks for when handed a D3D11
/// texture it has to convert. Without it the HEVC encoder answers `0xC00D6D76` ("the input type
/// is not supported for D3D device") at any size, while the H.264 encoder is content either way.
///
/// Public because the probe needs one; in the real pipeline the device is capture's, which is why
/// [`MftEncoder::open`] takes one instead of making its own.
pub fn create_capture_kind_device() -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: `device` is the out-parameter; the adapter and software parameters are null by
    // design (the default hardware adapter), and the feature-level array is empty, which asks for
    // the highest level the adapter supports.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .context("D3D11CreateDevice(HARDWARE, BGRA_SUPPORT | VIDEO_SUPPORT)")?;
    device.context("D3D11CreateDevice returned no device")
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
    /// cannot be handed the captured texture at all.
    ///
    /// `params` is the codec, the rate and the mode the encoder is opened with (see
    /// [`MftEncoderParams`]).
    pub fn open(
        device: &ID3D11Device,
        size: (u32, u32),
        preferred: Option<DXGI_FORMAT>,
        params: MftEncoderParams,
    ) -> Result<Self> {
        // SAFETY: `MFStartup` is refcounted and its pair is in `Drop`, so opening several encoders
        // in one process is a supported sequence.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
        match Self::open_started(device, size, preferred, params) {
            Ok(encoder) => Ok(encoder),
            Err(err) => {
                // SAFETY: balances the `MFStartup` above, which succeeded.
                unsafe {
                    let _ = MFShutdown();
                }
                Err(err)
            }
        }
    }

    fn open_started(
        device: &ID3D11Device,
        size: (u32, u32),
        preferred: Option<DXGI_FORMAT>,
        params: MftEncoderParams,
    ) -> Result<Self> {
        let manager = create_device_manager(device)?;
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // Unfiltered, for the reason at `probe_hardware_encoders`: filtering by an H.264 output subtype
        // hid the HEVC encoder from the recorder as well as the probe. The negotiation below decides.
        //
        // SAFETY: `activates`/`count` are the out-parameters this call fills.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                None,
                None,
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, hardware)")?;
        if activates.is_null() || count == 0 {
            if !activates.is_null() {
                // SAFETY: COM-allocated by `MFTEnumEx`; the matching free.
                unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
            }
            bail!(
                "no hardware encoder MFT on this machine will take {:?} at this size",
                params.codec
            )
        }
        // SAFETY: `activates` points at `count` initialised entries written by the call above.
        let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };

        let mut refusals = Vec::new();
        let mut chosen = None;
        for candidate in candidates.iter().flatten() {
            let name = attribute_string(candidate, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|| "<unnamed MFT>".to_string());
            match configure_encoder(candidate, &manager, size, preferred, params) {
                Ok((transform, input_format, input_subtype)) => {
                    chosen = Some((transform, name, input_format, input_subtype));
                    break;
                }
                // Not fatal: the hardware flag enumerates encoders this machine cannot instantiate.
                Err(err) => refusals.push(format!("{name}: {err:#}")),
            }
        }
        // SAFETY: as above.
        unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };

        let (transform, encoder_name, input_format, input_subtype) = chosen.ok_or_else(|| {
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
            device: device.clone(),
            size,
            params,
            encoder_name,
            input_format,
            input_subtype,
            renegotiated: 0,
            pairing: FramePairing::default(),
            origin: None,
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
            if self.pump_events(&mut produced)? == Asked::Input {
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
        let _ = self.pump_events(&mut produced);
        Ok(produced)
    }

    /// Tell the encoder no more input is coming, and take everything it still has. Without a drain
    /// the last few frames sit inside the encoder and never reach the container.
    pub fn finish(&mut self, deadline: Duration) -> Result<Vec<EncodedFrame>> {
        // SAFETY: a message to the encoder's own transform, with no parameter.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("MFT_MESSAGE_COMMAND_DRAIN")?;
        let mut produced = Vec::new();
        let started = Instant::now();
        loop {
            self.pump_events(&mut produced)?;
            if started.elapsed() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // SAFETY: as above — the stream is over.
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
        Ok(produced)
    }

    /// Drain whatever the encoder has to say, collecting its output as it goes.
    fn pump_events(&mut self, produced: &mut Vec<EncodedFrame>) -> Result<Asked> {
        // The scratch the sample's bytes are copied into, before they are moved into the frame.
        // Reused across the frames of one exchange: the allocation is the size of a frame.
        let mut scratch: Vec<u8> = Vec::new();
        loop {
            match self.next_event()? {
                Some(EncoderEvent::NeedInput) => return Ok(Asked::Input),
                Some(EncoderEvent::HaveOutput) => {
                    scratch.clear();
                    if let Some(frame) = self.take_output(&mut scratch)? {
                        produced.push(frame);
                    }
                }
                // Anything else (a drain completing, an error event) is not this exchange's business.
                Some(EncoderEvent::Other) => {}
                // Nothing queued. Not a failure: this is how a poll ends.
                None => return Ok(Asked::Quiet),
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
        let Ok(event) = (unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) }) else {
            return Ok(None);
        };
        // SAFETY: reading this event's own type.
        let kind = unsafe { event.GetType() }.context("IMFMediaEvent::GetType")?;
        Ok(Some(if kind == METransformNeedInput.0 as u32 {
            EncoderEvent::NeedInput
        } else if kind == METransformHaveOutput.0 as u32 {
            EncoderEvent::HaveOutput
        } else {
            EncoderEvent::Other
        }))
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
                    OutputFailure::Failed => {
                        return Err(err).context("IMFTransform::ProcessOutput")
                    }
                },
            }
            // SAFETY: taken exactly once, which is the contract for a `ManuallyDrop` field the callee
            // filled in.
            let Some(sample) = (unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) })
            else {
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
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("IMFTransform::ProcessInput")
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
        // Detach the device manager before the transform goes, as Media Foundation requires; the
        // transform's own reference then drops with this struct.
        // SAFETY: the transform is live, and the parameter is the documented detach value.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
        // SAFETY: refcounted, balancing the `MFStartup` in `open`.
        unsafe {
            let _ = MFShutdown();
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

    // Negotiated, not assumed: this MFT takes `ARGB32` at 640x480 and refuses it at 3840x2160 with
    // `MF_E_INVALIDMEDIATYPE`, so what it eats depends on size as well as format. Candidates are
    // tried in order, the direct one first (`ARGB32` means the texture goes straight in).
    let mut refusals = Vec::new();
    for (subtype, name) in candidate_inputs(preferred) {
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
        let handle = std::thread::Builder::new()
            .name("replayed-mft".to_string())
            .spawn(move || encode_loop(rx, out, counter, params, stop))
            .expect("spawning the hardware encoder thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
            dropped,
            stopping,
        }
    }

    /// Hand a frame to the encoder. Never blocks and never queues deeper than [`QUEUE_DEPTH`]: a
    /// full queue drops and counts the frame, as the video queue does when ffmpeg is behind.
    ///
    /// `captured_at` is when the frame was captured. It travels with the texture because it is
    /// what the finished frame is given back with, and the finished frame's time is what the
    /// child's timeline is built from.
    pub fn submit(&self, texture: GpuTexture, captured_at: Duration) {
        let Some(tx) = self.tx.as_ref() else { return };
        match tx.try_send((texture, captured_at)) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // The thread is gone, which its own log line explains; the frame is simply not encoded.
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
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

/// The encoder thread's body.
fn encode_loop(
    rx: std::sync::mpsc::Receiver<(GpuTexture, Duration)>,
    out: std::sync::mpsc::SyncSender<VideoChunk>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    params: MftEncoderParams,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // Opened on the first texture, because the device and size come from it; opening here also keeps
    // the open off the capture path.
    let mut encoder: Option<MftEncoder> = None;
    let fps = params.fps.max(1);
    // Frames handed to the encoder so far; the keyframe cadence counts from it.
    let mut frames: u64 = 0;
    // Frames encoded and then not queued, counted rather than logged one by one: a stalled queue drops every frame behind it.
    let mut unqueued: u64 = 0;

    while let Ok((texture, captured_at)) = rx.recv() {
        if encoder.is_none() {
            match MftEncoder::open_for_texture(texture.as_raw(), params) {
                Ok(mut opened) => {
                    if let Err(err) = opened.start() {
                        tracing::error!("could not start the hardware encoder: {err:#}");
                        break;
                    }
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
                            &out,
                            VideoChunk {
                                bytes: header,
                                // The header is not a frame: it has to be in front of the first
                                // frame, so it is written as soon as it can be.
                                captured_at: None,
                            },
                            &stopping,
                        ) {
                            Handover::Queued => {}
                            Handover::Dropped => {
                                tracing::warn!(
                                    "the sequence header did not fit the video queue, so the raw \
                                     bitstream has nothing to probe with until the first keyframe"
                                );
                            }
                            Handover::Closed => break,
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
                    encoder = Some(opened);
                }
                Err(err) => {
                    // Counted as a drop so reporting stays honest about frames that reached this stage and did not become video.
                    tracing::error!("could not open the hardware encoder: {err:#}");
                    dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            Ok(units) if units.is_empty() => {}
            Ok(units) => {
                for unit in units {
                    let chunk = VideoChunk {
                        bytes: unit.bytes,
                        captured_at: unit.captured_at,
                    };
                    match hand_over(&out, chunk, &stopping) {
                        Handover::Queued => {}
                        Handover::Dropped => unqueued += 1,
                        Handover::Closed => break,
                    }
                }
            }
            Err(err) => {
                tracing::warn!("the hardware encoder refused a frame: {err:#}");
                break;
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
    if let Some(mut mft) = encoder {
        match mft.finish(MFT_TIMEOUT) {
            Ok(tail) if !tail.is_empty() => {
                for unit in tail {
                    let chunk = VideoChunk {
                        bytes: unit.bytes,
                        captured_at: unit.captured_at,
                    };
                    if let Err(err) = out.try_send(chunk) {
                        tracing::warn!(
                            "the hardware encoder's last frames did not fit the queue: {err}"
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("draining the hardware encoder failed: {err:#}"),
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
    let end = description
        .Description
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(description.Description.len());
    String::from_utf16_lossy(&description.Description[..end])
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
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: the out-parameters this call fills, and the matching free below.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                None,
                None,
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx for the available types")?;
        if activates.is_null() || count == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `activates` points at `count` initialised entries written by the call above.
        let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let mut rows = Vec::new();
        for candidate in candidates.iter().flatten() {
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
        // SAFETY: COM-allocated by `MFTEnumEx`; the matching free.
        unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
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
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: the out-parameters this call fills, and the matching free below.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                None,
                None,
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx for the ordering test")?;
        if activates.is_null() || count == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `activates` points at `count` initialised entries written by the call above.
        let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let mut rows = Vec::new();
        for candidate in candidates.iter().flatten() {
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
        // SAFETY: COM-allocated by `MFTEnumEx`; the matching free.
        unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
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
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: the out-parameters this call fills, and the matching free below.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                None,
                None,
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx for the size ladder")?;
        if activates.is_null() || count == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `activates` points at `count` initialised entries written by the call above.
        let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let mut rows = Vec::new();
        for candidate in candidates.iter().flatten() {
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
        // SAFETY: COM-allocated by `MFTEnumEx`; the matching free.
        unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
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
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: the out-parameters this call fills, and the matching free below.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                None,
                None,
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx for the output-type matrix")?;
        if activates.is_null() || count == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `activates` points at `count` initialised entries written by the call above.
        let candidates = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let mut rows = Vec::new();
        for candidate in candidates.iter().flatten() {
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
        // SAFETY: COM-allocated by `MFTEnumEx`; this is the matching free.
        unsafe { CoTaskMemFree(Some(activates as *const std::ffi::c_void)) };
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
/// The device is the kind capture creates, because the encoder takes its device from the captured
/// texture and this asks the question the recording will ask. It costs one device and one
/// negotiation, and only while the key is unset.
pub fn can_encode_with_textures(
    codec: VideoCodec,
    size: (u32, u32),
    bitrate_kbps: u32,
    fps: u32,
) -> bool {
    let Ok(device) = create_capture_kind_device() else {
        return false;
    };
    MftEncoder::open(
        &device,
        size,
        None,
        MftEncoderParams {
            codec,
            bitrate_kbps,
            fps,
            // The probe asks whether the encoder will open at all, not how it will rate-control,
            // so it uses the mode every build used: constant bitrate.
            mode: RateControl::Cbr,
        },
    )
    .is_ok()
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
/// The names are the ones [`MftEncoder`] reports: `RGB32` is BGRA in memory (what WGC
/// delivers), `ARGB32` is the same bytes in the other order, and `NV12` is the planar one a
/// GPU video processor would have to produce.
fn dxgi_for_input(input_format: &str) -> DXGI_FORMAT {
    if input_format.starts_with("ARGB32") {
        DXGI_FORMAT_R8G8B8A8_UNORM
    } else if input_format.starts_with("NV12") {
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
    budget: Duration,
) -> Result<MftThroughput> {
    let device = create_capture_kind_device()?;
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
            Some(EncoderEvent::Other) => {}
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
        let encoders = probe_hardware_encoders(VideoCodec::H264).expect("the machine can be asked");
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
        let encoders = probe_hardware_encoders(VideoCodec::H264).expect("the machine can be asked");
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
        let encoders = probe_hardware_encoders(VideoCodec::H264).expect("the machine can be asked");
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
        produced += encoder
            .finish(Duration::from_secs(2))
            .expect("the drain")
            .len();
        assert!(
            produced > 0,
            "the encoder produced no bitstream after the renegotiation, so it did not survive it"
        );
    }
}
