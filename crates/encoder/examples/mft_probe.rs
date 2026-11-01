//! Ask this machine what hardware video encoders it has, and whether they take a GPU texture.
//!
//! The same shape as `xtask probe`: it starts Media Foundation, enumerates the hardware H.264
//! encoder MFTs of each adapter, and runs the zero-copy handshake on each, passing a device manager
//! built over a D3D11 device of the kind Windows Graphics Capture requires, **created on that
//! encoder's own adapter**. Run it from the desktop session: the Quick Sync MFTs cannot even be
//! created from an SSH session (session 0), and answer `0x80004005` there. A machine with no hardware
//! encoder MFT, or one that refuses the handshake, cannot take the zero-copy path at all.
//!
//! ```text
//! cargo run --release -p replayed-encoder --example mft_probe
//! ```
//!
//! The encoder may change its output type mid-stream — Media Foundation says so by refusing to
//! produce output until the streams are renegotiated — and the library renegotiates for this
//! example and for the recording alike, so a type change is a renegotiation rather than a dead
//! encoder.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    // `-- hevc` asks the same questions about HEVC: which MFTs produce it, and whether the ARGB32
    // handshake survives its different profile and level.
    let codec = if std::env::args().any(|arg| arg.eq_ignore_ascii_case("hevc")) {
        replayed_encoder::VideoCodec::Hevc
    } else {
        replayed_encoder::VideoCodec::H264
    };
    // The probes ask about formats and sizes, not rates, so they use the example's own bitrate and
    // frame rate; the recording path passes `encode.bitrate_kbps` and the effective fps instead.
    const BITRATE_KBPS: u32 = 20_000;
    const FPS: u32 = 60;
    // Unfiltered, so "no encoder for this codec" is distinguishable from "the filter is wrong".
    match replayed_encoder::mft::list_hardware_video_encoders() {
        Ok(names) => {
            println!(
                "--- every hardware video encoder MFT here, unfiltered: {} ---",
                names.len()
            );
            for name in names {
                println!("  {name}");
            }
        }
        Err(err) => println!("could not list encoder MFTs: {err:#}"),
    }
    println!("--- asking about {codec:?} ---");
    let encoders = replayed_encoder::mft::probe_hardware_encoders(codec)?;
    println!("hardware {codec:?} encoder MFTs: {}", encoders.len());
    for encoder in &encoders {
        println!(
            "  {:<46} d3d11={:<5} async={:<5}\n      adapter: {}\n      url: {}",
            encoder.name,
            encoder.accepts_d3d11,
            encoder.asynchronous,
            encoder.adapter,
            encoder.hardware_url.as_deref().unwrap_or("-"),
        );
        if encoder.accepts.is_empty() {
            println!("      takes no candidate input format");
        } else {
            for subtype in &encoder.accepts {
                println!("      takes: {subtype}");
            }
        }
        if encoder.input_subtypes.is_empty() {
            println!("      (it enumerates no types, which is what an async MFT does)");
        }
        if let Some(why) = &encoder.refusal {
            println!("      refused: {why}");
        }
    }
    let usable = encoders.iter().filter(|e| e.accepts_d3d11).count();
    println!("\nof those, {usable} accepted MFT_MESSAGE_SET_D3D_MANAGER over a BGRA-capable D3D11 device");

    // If the encoder lists NV12 but not RGB32, a GPU Video Processor MFT belongs in the chain
    // rather than a CPU conversion loop. WGC delivers BGRA8.
    let takes_bgra = encoders.iter().any(|e| {
        e.input_subtypes
            .iter()
            .any(|s| s.starts_with("RGB32") || s.starts_with("ARGB32"))
    });
    println!("any encoder takes BGRA/ARGB directly: {takes_bgra}");
    match replayed_encoder::mft::probe_video_processors() {
        Ok(processors) if processors.is_empty() => {
            println!("video processor MFTs (for a GPU-side colour conversion): none")
        }
        Ok(processors) => println!(
            "video processor MFTs (for a GPU-side colour conversion): {}",
            processors.join(", ")
        ),
        Err(err) => println!("video processor MFTs: could not be asked: {err}"),
    }
    // The encoder core at 4K: hand it a GPU texture and see whether it takes it with no pixel
    // crossing the CPU.
    if usable > 0 {
        println!("\n--- the encoder core, at 3840x2160 ---");
        let size = (3840u32, 2160u32);
        // The device is the probe's own; in the real pipeline it is capture's, which is why
        // `MftEncoder::open` takes one rather than making its own.
        let device = replayed_encoder::mft::create_capture_kind_device()?;

        // For HEVC the format is the question. The H.264 encoder takes the BGRA texture below and
        // converts inside itself; the HEVC one refuses it (`0xC00D6D76`, MF_E_UNSUPPORTED_D3D_TYPE:
        // "the input type is not supported for D3D device"). Ask the encoder which type it wants rather
        // than guessing at attributes; the capture handover must produce NV12 for HEVC.
        if codec == replayed_encoder::VideoCodec::Hevc {
            println!("\n--- what the encoder says it offers ---");
            for (what, value) in replayed_encoder::mft::probe_available_types(codec, size)? {
                println!("  {what}\n     -> {value}");
            }
            println!("\n--- and at what size it stops accepting a D3D texture ---");
            for (what, value) in replayed_encoder::mft::probe_size_ceiling(codec)? {
                println!("  {what}\n     -> {value}");
            }
            println!("\n--- and whether the order of the two types matters ---");
            for (what, value) in replayed_encoder::mft::probe_ordering(codec, size)? {
                println!("  {what}\n     -> {value}");
            }
        }

        // If the encoder still refuses, find out what it objects to, one attribute at a time.
        if codec == replayed_encoder::VideoCodec::Hevc {
            println!("\n--- which attribute of the output type it objects to ---");
            for (what, verdict) in replayed_encoder::mft::probe_output_types(codec, size)? {
                println!("  {what}\n     -> {verdict}");
            }
        }

        println!("\n--- what an NV12 texture does ---");
        let nv12 = fake_frame(
            &device,
            size,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
        );
        match nv12.and_then(|texture| {
            replayed_encoder::mft::MftEncoder::open_for_texture(
                &texture,
                replayed_encoder::mft::MftEncoderParams {
                    codec,
                    bitrate_kbps: BITRATE_KBPS,
                    fps: FPS,
                    mode: replayed_encoder::RateControl::Cbr,
                    vendor: None,
                },
            )
        }) {
            Ok(encoder) => println!(
                "an NV12 texture opens: {} taking {}",
                encoder.encoder_name, encoder.input_format
            ),
            Err(err) => println!("an NV12 texture does not open: {err:#}"),
        }

        println!("\n--- which GPU ---");
        for (index, name) in replayed_encoder::mft::describe_adapters()
            .into_iter()
            .enumerate()
        {
            let note = if index == 0 {
                "   <- what a null-adapter device gets (the default)"
            } else {
                ""
            };
            println!("  [{index}] {name}{note}");
        }
        println!(
            "  capture-kind device: {}",
            replayed_encoder::mft::adapter_description(&device)
        );
        match replayed_encoder::mft::MftEncoder::open(
            &device,
            size,
            Some(windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM),
            replayed_encoder::mft::MftEncoderParams {
                codec,
                bitrate_kbps: BITRATE_KBPS,
                fps: FPS,
                mode: replayed_encoder::RateControl::Cbr,
                vendor: None,
            },
        ) {
            Ok(mut encoder) => {
                println!(
                    "opened {} for {size:?}, taking {} input, on adapter {}",
                    encoder.encoder_name,
                    encoder.input_format,
                    replayed_encoder::mft::adapter_description(encoder.device())
                );
                if !encoder.input_format.starts_with("RGB32")
                    && !encoder.input_format.starts_with("ARGB32")
                {
                    println!(
                        "  NOTE: that is not the format capture delivers, so this chain needs a \
                         GPU-side conversion to reach it"
                    );
                }
                let texture = fake_frame(
                    encoder.device(),
                    size,
                    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                )?;
                encoder.start()?;
                // Feed it a second of frames and collect the output: H.264 bytes must exist, i.e. a captured
                // texture reaches the encoder with no CPU copy and the encoder makes a stream of it.
                let mut total = 0usize;
                let mut frames = 0usize;
                let mut stream: Vec<u8> = Vec::new();
                // Streamed into ffmpeg as produced, not batched: an H.264 elementary stream has no timestamps,
                // so ffmpeg uses arrival times, and a batch write makes every frame arrive at once (30 frames
                // paced over a second once produced a 196ms fragment).
                let mut sink = FfmpegSink::spawn(codec)?;
                // Paced at 30fps for the same reason.
                let pace = std::time::Duration::from_millis(33);
                // Three seconds, so the one-fragment-per-keyframe contract is visible (one keyframe a second).
                let seconds = 3;
                for index in 0..(30 * seconds) {
                    let at = std::time::Instant::now();
                    // The frame's own capture time: the encoder is told when the frame was
                    // captured, so the stream it produces is on the capture's timeline.
                    let captured_at = std::time::Duration::from_millis(index * 33);
                    match encoder.encode_texture(
                        &texture,
                        captured_at,
                        std::time::Duration::from_secs(2),
                    ) {
                        Ok(encoded) => {
                            for frame in encoded {
                                total += frame.bytes.len();
                                stream.extend_from_slice(&frame.bytes);
                                sink.write(&frame.bytes)?;
                            }
                            frames += 1;
                        }
                        Err(err) => {
                            println!("frame {index} rejected: {err:#}");
                            break;
                        }
                    }
                    let spent = at.elapsed();
                    if spent < pace {
                        std::thread::sleep(pace - spent);
                    }
                }
                // The frames the drain produced count even when it reports a problem: they are
                // collected as they arrive and kept on failure.
                let mut tail = Vec::new();
                if let Err(err) = encoder.finish(std::time::Duration::from_secs(2), &mut tail) {
                    println!("drain failed: {err:#}");
                }
                for frame in tail {
                    total += frame.bytes.len();
                }
                println!("{frames} frames submitted, {total} bytes of H.264 out");
                println!("the stream starts {:02x?}", &stream[..stream.len().min(8)]);
                // Whether what ffmpeg makes of the bare bitstream is the fragmented MP4 the ring parses; an
                // elementary stream carries no timestamps, so this is asked rather than assumed.
                println!("\n--- the bitstream into ffmpeg ---");
                match sink.finish() {
                    Ok(fmp4) => {
                        println!("ffmpeg produced {} bytes of fragmented MP4", fmp4.len());
                        match replayed_media::FragmentSplitter::new().push(&fmp4) {
                            Ok(fragments) => println!(
                                "the project's own splitter sees {} fragment(s): {:?}",
                                fragments.len(),
                                fragments
                                    .iter()
                                    .map(|f| (f.seq, f.start_ms, f.duration_ms, f.keyframe))
                                    .collect::<Vec<_>>()
                            ),
                            Err(err) => println!(
                                "the splitter could not read it, which is the finding: {err:#}"
                            ),
                        }
                    }
                    Err(err) => println!("ffmpeg refused it: {err:#}"),
                }
            }
            Err(err) => println!("could not open one for {size:?}: {err:#}"),
        }
    }

    if usable == 0 {
        // A finding, not an error. Exit non-zero so a script notices, but print the reason, not a stack trace.
        anyhow::bail!("no hardware encoder on this machine accepts the D3D11 handshake");
    }
    Ok(())
}

/// An ffmpeg child taking an elementary stream of `codec` on stdin and writing fragmented MP4 to stdout.
///
/// The command line is the project's replay-buffer shape (`empty_moov+frag_keyframe+default_base_moof`).
/// `-use_wallclock_as_timestamps 1` is load-bearing: an elementary stream has no timestamps, and
/// this takes them from arrival instead of a rigid grid, so the bytes must arrive when they were made.
#[cfg(windows)]
struct FfmpegSink {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    reading: std::thread::JoinHandle<Vec<u8>>,
}

#[cfg(windows)]
impl FfmpegSink {
    fn spawn(codec: replayed_encoder::VideoCodec) -> anyhow::Result<Self> {
        use anyhow::Context;
        use std::io::Read;
        use std::process::{Command, Stdio};

        let bin = replayed_media::FfmpegBinaries::discover(None).context("ffmpeg on PATH")?;
        let mut child = Command::new(&bin.ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                // The raw demuxer's name is the codec's: `h264` or `hevc`. It was `h264` for both,
                // so probing HEVC wrote an H.265 stream through the H.264 parser and the output
                // was unplayable, which misled whoever was investigating a hardware problem.
                "-f",
                codec.as_str(),
                "-use_wallclock_as_timestamps",
                "1",
                "-i",
                "pipe:0",
                "-c:v",
                "copy",
                "-f",
                "mp4",
                "-movflags",
                "empty_moov+frag_keyframe+default_base_moof",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg")?;

        // ffmpeg blocks on a full stdout, so it is read on its own thread or the writes below deadlock.
        let mut stdout = child.stdout.take().context("ffmpeg's stdout")?;
        let reading = std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out);
            out
        });
        let stdin = child.stdin.take().context("ffmpeg's stdin")?;
        Ok(Self {
            child,
            stdin,
            reading,
        })
    }

    /// Hand ffmpeg one access unit of bitstream as it is produced.
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::io::Write;
        if bytes.is_empty() {
            return Ok(());
        }
        self.stdin
            .write_all(bytes)
            .context("writing the bitstream")?;
        // Flushed per access unit: buffering would turn the frames back into a batch.
        self.stdin.flush().context("flushing to ffmpeg")
    }

    /// Close the stream and collect what ffmpeg made of it.
    fn finish(self) -> anyhow::Result<Vec<u8>> {
        use anyhow::Context;
        use std::io::Read;

        let Self {
            mut child,
            stdin,
            reading,
        } = self;
        drop(stdin);
        let fmp4 = reading.join().expect("the reader thread");
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let status = child.wait().context("waiting for ffmpeg")?;
        if !status.success() {
            anyhow::bail!("ffmpeg exited {status}: {stderr}");
        }
        Ok(fmp4)
    }
}

/// A BGRA texture of `size` on `device`, standing in for a captured frame.
///
/// Zero-filled (pixel values do not matter). Created with the bind flags WGC's own frames carry,
/// so a refusal on those grounds cannot be mistaken for a format refusal.
#[cfg(windows)]
fn fake_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    size: (u32, u32),
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> anyhow::Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    use anyhow::Context;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

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

#[cfg(not(windows))]
fn main() {
    println!(
        "The hardware encoder path is Windows-only: it is Media Foundation's encoder MFTs. \
         Nothing to probe on this host."
    );
}
