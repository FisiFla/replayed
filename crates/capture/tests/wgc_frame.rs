//! One frame from the real Windows capture backend, written as a PNG.
//!
//! **Ignored by default**, because it needs two things a hosted runner does not have: an
//! interactive desktop session (Windows Graphics Capture captures a session's desktop, and a
//! service or a plain SSH shell has none) and ffmpeg on `PATH`. Run it on the box from a desktop
//! session:
//!
//! ```text
//! cargo test -p replayed-capture --test wgc_frame -- --ignored --nocapture
//! ```
//!
//! This is the evidence the screenshot feature needs. Everything around the capture — the PNG
//! writer, the index row, the window — is tested on every host with the synthetic stub; this is
//! the one test that proves the pixels come from the screen, on the machine that has one.
//!
//! It opens a capture session, takes exactly one frame, and closes it. Nothing is synthesised: no
//! keystrokes, no mouse, so it is safe on a machine that runs kernel-level anti-cheat.
//! The still image it compares against is the desktop itself.
//!
//! # The window tests
//!
//! The tests after it capture a window **of their own**, which they create, resize and destroy
//! themselves with `SetWindowPos` and `DestroyWindow` on the handle they were given. That is the
//! only way to make a capture change under the backend on demand, and it is what the resize and
//! closed-target fixes are tested against: no other process's window is looked up, touched, moved
//! or measured, and no input is synthesised. They are run the same way, from a desktop session:
//!
//! ```text
//! cargo test -p replayed-capture --test wgc_frame -- --ignored --nocapture --test-threads=1
//! ```
//!
//! One at a time (`--test-threads=1`): each captures the session's desktop, and two windows of the
//! same family on one desktop is not what they measure.
#![cfg(windows)]

use replayed_capture::platform::default_video_backend;
use replayed_capture::stub::StubConfig;
use replayed_capture::target::CaptureTarget;
use replayed_capture::{CaptureBackend, CaptureLost, Frame};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, SetWindowPos, ShowWindow,
    TranslateMessage, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_MINIMIZE,
    SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

#[test]
#[ignore = "needs an interactive desktop session and ffmpeg on PATH"]
fn one_real_frame_is_captured_and_written_as_a_png() {
    let target = CaptureTarget::from_config(0, "");
    let mut backend = default_video_backend(
        StubConfig {
            width: 1920,
            height: 1080,
            fps: 60,
        },
        &target,
    )
    .expect("a capture backend on this machine");
    // A screenshot needs the pixels on the CPU; the zero-copy path exists for the encoder.
    backend.deliver_textures(false);

    let frame = replayed_capture::grab_frame(backend.as_mut(), Duration::from_secs(5))
        .expect("one frame from the desktop");

    println!(
        "captured {}x{} ({} bytes) at {:?}",
        frame.width,
        frame.height,
        frame.data.len(),
        frame.pts
    );
    assert!(frame.has_pixels(), "the frame must carry pixels");
    assert_eq!(
        frame.data.len(),
        (frame.width as usize) * (frame.height as usize) * 4,
        "a BGRA frame is width*height*4 bytes"
    );

    // A screen is not one colour. A flat frame means the capture returned an empty surface, which
    // is the failure mode this test exists to catch.
    //
    // Sampled across the whole frame, not from its start. The first 5,000 pixels are the top row of
    // the screen and a little of the next, which is one flat colour whenever a maximised window with
    // a plain title bar is on top (a browser on the box does exactly that), so the test failed on a
    // perfectly good capture. The stride is prime so it does not line up with the width and sample
    // one column; at 3840x2160 it still reads some 33,000 pixels.
    let distinct: BTreeSet<[u8; 3]> = frame
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(251)
        .map(|px| [px[0], px[1], px[2]])
        .collect();
    assert!(
        distinct.len() > 1,
        "the captured frame is a single colour, which no desktop is"
    );

    let bin = replayed_media::FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    // By default the PNG goes in a temporary directory and dies with it. A verification run that
    // wants to *look* at the frame — the tray icon, the window, the empty library's checklist —
    // sets `REPLAYED_WGC_FRAME_OUT` to a path and gets to keep it.
    let keep = std::env::var_os("REPLAYED_WGC_FRAME_OUT").map(std::path::PathBuf::from);
    let dir = tempfile::tempdir().expect("a temporary directory");
    let dst = keep
        .clone()
        .unwrap_or_else(|| dir.path().join("screenshot.png"));
    if let Some(parent) = keep.as_ref().and_then(|path| path.parent()) {
        std::fs::create_dir_all(parent).expect("the frame's destination directory");
    }
    replayed_media::edit::screenshot_png(&bin, frame.width, frame.height, &frame.data, &dst)
        .expect("the frame written as a PNG");

    let info = replayed_media::MediaInfo::probe(&bin, &dst).expect("probing the PNG");
    let video = info.video.as_ref().expect("a PNG is a video stream");
    assert_eq!(
        (video.width, video.height),
        (frame.width, frame.height),
        "the PNG is the frame's own size"
    );
    assert_eq!(video.codec, "png");
    assert!(info.audio.is_none(), "a still image has no audio");
    let size = std::fs::metadata(&dst).expect("the PNG on disk").len();
    assert!(size > 1_000, "a {size}-byte PNG of a desktop is suspicious");
    println!(
        "PNG {}x{} written: {} bytes at {}",
        video.width,
        video.height,
        size,
        dst.display()
    );
}

/// A window this test made, destroyed when the guard goes, so a failed assertion does not leave one
/// on the desktop.
struct OwnWindow {
    hwnd: HWND,
    title: String,
    destroyed: bool,
}

impl OwnWindow {
    /// A visible top-level window of a system class, at a size, with a title unique to this
    /// process so the capture target can name it and nothing else.
    fn open(test: &str, width: i32, height: i32) -> Self {
        let title = format!("replayed-capture-{test}-{}", std::process::id());
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `STATIC` is a system window class, so no window procedure of ours is involved;
        // `wide` is NUL-terminated and outlives the call; the window belongs to this thread.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                windows::core::PCWSTR(wide.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                80,
                80,
                width,
                height,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a window of this test's own");
        // SAFETY: our own window.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        let this = Self {
            hwnd,
            title,
            destroyed: false,
        };
        this.settle(Duration::from_millis(500));
        this
    }

    fn target(&self) -> CaptureTarget {
        CaptureTarget::from_config(0, &self.title)
    }

    /// Resize our own window, keeping its position.
    fn resize(&self, width: i32, height: i32) {
        // SAFETY: our own window, on the thread that made it.
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .expect("resizing the test's own window");
    }

    fn minimise(&self) {
        // SAFETY: our own window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_MINIMIZE) };
    }

    fn destroy(&mut self) {
        if !self.destroyed {
            self.destroyed = true;
            // SAFETY: our own window, on the thread that made it, destroyed once.
            unsafe { DestroyWindow(self.hwnd) }.expect("destroying the test's own window");
        }
    }

    /// Let this thread's windows handle their messages for a while, as an application would.
    fn settle(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        while Instant::now() < until {
            pump_messages();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for OwnWindow {
    fn drop(&mut self) {
        if !self.destroyed {
            // SAFETY: as in `destroy`; a failure here is not worth a second panic.
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

/// Handle the messages this thread has waiting (and only this thread's: they are our own window's).
fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: `message` is a valid out-parameter; a null window handle asks for this thread's own queue.
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: a message the OS just gave this thread.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn start_capture_of(window: &OwnWindow) -> Box<dyn CaptureBackend> {
    let mut backend = default_video_backend(
        StubConfig {
            width: 640,
            height: 480,
            fps: 60,
        },
        &window.target(),
    )
    .expect("a capture backend for the test's own window");
    backend.deliver_textures(false);
    backend.start().expect("starting the capture");
    backend
}

/// The next frame the backend returns, with whatever size change came with it, within `within`.
///
/// A change reported on a call that returned no frame (the pool was rebuilt and the first frame of
/// the new size has not arrived yet) is held and handed back with the frame it belongs to, which is
/// the first one the caller may use at the new size.
fn next_with_change(
    backend: &mut dyn CaptureBackend,
    within: Duration,
) -> Option<(Frame, Option<(u32, u32)>)> {
    let until = Instant::now() + within;
    let mut held = None;
    while Instant::now() < until {
        pump_messages();
        let frame = backend
            .next_frame(Duration::from_millis(100))
            .expect("a frame, or none yet");
        // Asked after every call and before the frame is used: the contract the recorder follows.
        let change = backend.take_size_change().or(held.take());
        match frame {
            Some(frame) => return Some((frame, change)),
            None => held = change,
        }
    }
    None
}

/// Whatever frames the backend returns for the next second are of this size, and none reports another change.
///
/// Not "ten frames": a window nobody repaints presents nothing once it has settled, and a capture
/// that waited for ten would be waiting for the window and not testing the backend.
fn assert_every_frame_is(backend: &mut dyn CaptureBackend, size: (u32, u32)) {
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        pump_messages();
        if let Some(frame) = backend
            .next_frame(Duration::from_millis(100))
            .expect("a frame, or none yet")
        {
            assert_eq!((frame.width, frame.height), size);
        }
        assert_eq!(backend.take_size_change(), None, "reported once");
        assert_eq!(backend.native_size(), size);
    }
}

/// Resize the window until the backend reports the change, and return the frame that carried it.
fn resize_and_wait(
    backend: &mut dyn CaptureBackend,
    window: &OwnWindow,
    width: i32,
    height: i32,
) -> (Frame, (u32, u32)) {
    window.resize(width, height);
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        let (frame, change) =
            next_with_change(backend, Duration::from_secs(10)).expect("a frame after the resize");
        if let Some(size) = change {
            return (frame, size);
        }
        // A frame of the old size that was already on its way: still a whole picture of it.
        assert_eq!(
            (frame.width, frame.height),
            backend.native_size(),
            "a frame the backend returns is always the size it reports"
        );
    }
    panic!("the backend never reported the window's new size");
}

#[test]
#[ignore = "needs an interactive desktop session; resizes a window of its own"]
fn a_resized_window_is_reported_and_its_frames_follow_the_new_size() {
    let window = OwnWindow::open("resize", 640, 480);
    let mut backend = start_capture_of(&window);

    let (first, change) = next_with_change(backend.as_mut(), Duration::from_secs(10))
        .expect("a first frame of the window");
    assert_eq!(change, None, "nothing has changed yet");
    let opened = (first.width, first.height);
    assert_eq!(backend.native_size(), opened);
    println!("window captured at {opened:?}");

    // Bigger: the surface would be a crop of the window if the pool were not rebuilt.
    let (frame, size) = resize_and_wait(backend.as_mut(), &window, 960, 640);
    println!("grown to {size:?}");
    assert!(
        size.0 > opened.0 && size.1 > opened.1,
        "{size:?} vs {opened:?}"
    );
    assert_eq!(
        (frame.width, frame.height),
        size,
        "the frame is the new size"
    );
    assert_eq!(backend.native_size(), size);
    assert_eq!(backend.take_size_change(), None, "reported once");
    assert_every_frame_is(backend.as_mut(), size);

    // Smaller: the surface would be padded if the pool were not rebuilt.
    let (frame, smaller) = resize_and_wait(backend.as_mut(), &window, 480, 360);
    println!("shrunk to {smaller:?}");
    assert!(
        smaller.0 < size.0 && smaller.1 < size.1,
        "{smaller:?} vs {size:?}"
    );
    assert_eq!((frame.width, frame.height), smaller);
    assert_eq!(backend.native_size(), smaller);
    assert_every_frame_is(backend.as_mut(), smaller);
    backend.stop().expect("stopping the capture");
}

#[test]
#[ignore = "needs an interactive desktop session; destroys a window of its own"]
fn a_closed_window_is_a_lost_target_and_not_an_endless_wait() {
    let mut window = OwnWindow::open("closed", 640, 480);
    let mut backend = start_capture_of(&window);
    next_with_change(backend.as_mut(), Duration::from_secs(10))
        .expect("a first frame of the window");

    window.destroy();

    let until = Instant::now() + Duration::from_secs(10);
    let lost = loop {
        assert!(
            Instant::now() < until,
            "a destroyed window never became an error: next_frame went on returning no frame"
        );
        pump_messages();
        match backend.next_frame(Duration::from_millis(100)) {
            Ok(_) => continue,
            Err(e) => break e,
        }
    };
    assert!(
        matches!(
            lost.downcast_ref::<CaptureLost>(),
            Some(CaptureLost::TargetClosed)
        ),
        "the error is the typed one the recorder maps, not {lost:#}"
    );
    // Said again on every call, not once: a caller that ignores it still gets it.
    assert!(backend.next_frame(Duration::from_millis(10)).is_err());
    backend
        .stop()
        .expect("stopping a capture whose target is gone");
}

#[test]
#[ignore = "needs an interactive desktop session; minimises a window of its own"]
fn a_minimised_window_is_a_reported_stall_and_not_an_error() {
    let window = OwnWindow::open("minimised", 640, 480);
    let mut backend = start_capture_of(&window);
    next_with_change(backend.as_mut(), Duration::from_secs(10))
        .expect("a first frame of the window");
    assert_eq!(backend.stalled_for(), None, "frames are arriving");

    window.minimise();

    // Longer than the stall threshold. Whatever the platform does with a minimised window, the
    // call neither fails nor invents a frame; and once it has gone quiet for long enough, says so.
    let until = Instant::now() + Duration::from_secs(12);
    let mut stalled = None;
    while Instant::now() < until && stalled.is_none() {
        pump_messages();
        let _ = backend
            .next_frame(Duration::from_millis(100))
            .expect("a minimised window is quiet, not an error");
        let _ = backend.take_size_change();
        stalled = backend.stalled_for();
    }
    assert!(
        stalled.is_some(),
        "twelve quiet seconds were never reported as a stall"
    );
    backend.stop().expect("stopping the capture");
}
