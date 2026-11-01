//! The recorder's own tests.
//!
//! Unit tests rather than `tests/` integration tests: the end-to-end test needs
//! `EncodeConfig::for_tests_software`, which only exists under `cfg(test)` of this crate.
//!
//! Needs ffmpeg on `PATH` with a working libx264.

use super::*;
use crate::pump::frame_bytes;
use replayed_capture::{AudioBuffer, Frame, PixelFormat};
use replayed_events::EventKind;

/// The stub geometry: small and slow, so four seconds of real-time capture encodes quickly.
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 10;
const SEGMENT_SECONDS: u64 = 1;
const PRE_SECONDS: u64 = 2;
const POST_SECONDS: u64 = 1;

/// The application data directory the end-to-end recordings write into.
///
/// A temp directory by default; setting `REPLAYED_TEST_CLIPS_DIR` keeps the recording there so
/// the clip can be ffprobed by hand:
///
/// ```text
/// REPLAYED_TEST_CLIPS_DIR=$PWD/target/recorder-e2e \
///   cargo test -p replayed-recorder -- --nocapture a_recorder_records
/// ffprobe target/recorder-e2e/clips/clip-*.mp4
/// ```
fn application_data_dir(what: &str) -> (Option<tempfile::TempDir>, PathBuf) {
    match std::env::var_os("REPLAYED_TEST_CLIPS_DIR") {
        // One subdirectory per test: the tests run in parallel and each recorder owns its scratch directory.
        Some(dir) => (None, PathBuf::from(dir).join(what)),
        None => {
            // Under `target/`, not the system temp, so a test run leaves nothing outside the build directory.
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/recorder-tests");
            std::fs::create_dir_all(&base).expect("creating the test scratch root under target/");
            // Canonical: the store refuses to delete through a `..` path.
            let base = base
                .canonicalize()
                .expect("canonicalising the test scratch root");
            let dir = tempfile::Builder::new()
                .prefix(&format!("{what}-"))
                .tempdir_in(&base)
                .expect("a temp dir under target/");
            let path = dir.path().to_path_buf();
            (Some(dir), path)
        }
    }
}

/// A configuration that records from the synthetic sources into a temp directory.
///
/// `dev_software_encoder` is true because this host has no hardware encoder; the encoder is
/// still real ffmpeg (real rawvideo pipe, segment muxer and `-c copy` splice).
fn stub_config(app_data_dir: &Path) -> RecorderConfig {
    let bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    RecorderConfig {
        bin,
        app_data_dir: app_data_dir.to_path_buf(),
        buffer: BufferSection {
            pre_seconds: PRE_SECONDS,
            post_seconds: POST_SECONDS,
            segment_time: SEGMENT_SECONDS,
            scratch_dir: String::new(),
            // Large on purpose: a ring that evicted mid-recording would make these tests about eviction.
            ram_cap_bytes: 1 << 30,
            max_pre_seconds: 0,
            max_post_seconds: 0,
        },
        encode: EncodeSection {
            vendor: "auto".to_string(),
            codec: "h264".to_string(),
            bitrate_kbps: 2_000,
            // Off explicitly: these tests drive the rawvideo path, and the hybrid needs a hardware MFT.
            zero_copy: Some(false),
            fps: FPS,
            // The shipping default; at 64x48 libx264 is far above 10fps, so the probe cannot reduce the rate.
            adapt_fps: true,
            output_size: String::new(),
            // Constant bitrate and the encoder's own preset: what a file that does not mention
            // them records as.
            rate_control: "cbr".to_string(),
            preset: String::new(),
        },
        storage: StorageSection {
            clips_dir: String::new(),
            max_total_bytes: 1 << 30,
            max_age_days: 365,
            sessions: crate::config::SessionStorageRules {
                sessions_dir: String::new(),
                max_total_bytes: 1 << 30,
                max_age_days: 365,
            },
        },
        // No `[audio]` value: resolves to the pipeline's default (192 kbps), pinned by
        // `the_configured_audio_bitrate_reaches_the_encoder_and_unset_gets_the_default`.
        capture: Default::default(),
        audio_bitrate_kbps: None,
        // No `[audio] enabled`: an absent value records the game audio (`RecorderConfig::game_audio`).
        audio_enabled: None,
        sources: Sources::Stub(StubConfig {
            width: WIDTH,
            height: HEIGHT,
            fps: FPS,
        }),
        dev_software_encoder: true,
    }
}

/// Poll `check` until it holds or `budget` elapses, returning the last status.
///
/// The pipeline is paced on the wall clock, so a test can wait for a counter to reach a value
/// but never assert it instantly.
fn wait_for(
    recorder: &Recorder,
    budget: Duration,
    what: &str,
    check: impl Fn(&RecorderStatus) -> bool,
) -> RecorderStatus {
    let deadline = Instant::now() + budget;
    loop {
        let status = recorder.status();
        if check(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "gave up waiting for {what}: {status:?} (error: {:?})",
            status.error
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_recorder_records_produces_a_clip_and_stops_cleanly() {
    let (_tmp, app_data_dir) = application_data_dir("record-clip");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start(cfg).expect("the recorder starts");

    // 1. The status advances unprompted: frames reach the encoder and the ring's media timeline grows.
    //
    //    The bar is the full window (`pre + post`), not "non-empty": no window can be covered by
    //    footage that does not exist yet, so an earlier trigger returns a clip one pre-roll short.
    let wanted_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    let first = wait_for(
        &recorder,
        Duration::from_secs(30),
        "the window to be buffered",
        |s| s.frames > 0 && s.span_ms >= wanted_ms,
    );
    assert!(first.running, "the recorder must report itself as running");
    assert!(
        first.segments > 0,
        "a non-zero span implies a completed segment: {first:?}"
    );
    assert!(
        first.bytes > 0,
        "the ring holds footage, so it has bytes: {first:?}"
    );
    assert_eq!(first.configured_fps, FPS);
    assert_eq!(first.error, None, "nothing has failed");
    eprintln!("after the buffer filled: {first:?}");

    let span_before = first.span_ms;
    let second = wait_for(
        &recorder,
        Duration::from_secs(20),
        "the span to grow",
        |s| s.span_ms > span_before,
    );
    assert!(
        second.frames > first.frames,
        "frames must advance with the span: {} -> {}",
        first.frames,
        second.frames
    );
    eprintln!(
        "two seconds later: span {}ms -> {}ms, frames {} -> {}",
        first.span_ms, second.span_ms, first.frames, second.frames
    );

    // 2. The trigger (media-time path): `clip_now` blocks until the post-roll is on disk, splices
    //    losslessly and indexes the result.
    let clip = recorder
        .clip_now()
        .expect("the trigger must produce a clip");
    eprintln!(
        "clip_now() wrote {} ({}ms, {} bytes, encoder={}, indexed as {:?}, media t={}ms)",
        clip.metadata.path.display(),
        clip.metadata.duration_ms,
        clip.metadata.size_bytes,
        clip.metadata.encoder,
        clip.id,
        clip.started_at_ms
    );
    assert!(clip.metadata.path.is_file(), "the clip must exist on disk");
    assert!(clip.metadata.size_bytes > 0, "and must not be empty");
    assert_eq!(
        clip.metadata.size_bytes,
        std::fs::metadata(&clip.metadata.path).unwrap().len()
    );
    assert_eq!(
        clip.metadata.encoder, "libx264",
        "the encoder that actually ran"
    );
    // The window is `[trigger - pre, trigger + post]` in media terms, spliced from whole segments,
    // so the clip covers the request to within one segment.
    //
    // It can be shorter than the request by one frame interval per segment: with `-fps_mode
    // passthrough` (`replayed_encoder::ffmpeg`) a segment ends at the last frame that arrived
    // (~940ms of media per 1000ms segment at 10fps).
    let requested_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    let frame_ms = 1000 / u64::from(FPS);
    let segments_in_window = requested_ms / (SEGMENT_SECONDS * 1000) + 1;
    let slack_ms = segments_in_window * frame_ms;
    assert!(
        clip.metadata.duration_ms + slack_ms >= requested_ms,
        "the clip must cover the pre+post window within one frame interval per segment \
         ({slack_ms}ms of slack): {}ms for a {requested_ms}ms request",
        clip.metadata.duration_ms
    );
    assert!(
        clip.metadata.duration_ms <= requested_ms + 2 * SEGMENT_SECONDS * 1000,
        "and must not be padded far past it: {}ms for a {requested_ms}ms request",
        clip.metadata.duration_ms
    );

    // Both streams, read back by ffprobe.
    let info = replayed_media::probe::MediaInfo::probe(&bin, &clip.metadata.path)
        .expect("the clip must be a readable media file");
    let video = info.video.as_ref().expect("the clip must carry video");
    let audio = info.audio.as_ref().expect("the clip must carry audio");
    eprintln!(
        "ffprobe: {}ms, {} bytes, video={} {}x{}, audio={} {}Hz {}ch",
        info.duration_ms,
        info.size_bytes,
        video.codec,
        video.width,
        video.height,
        audio.codec,
        audio.sample_rate,
        audio.channels
    );
    assert_eq!(video.codec, "h264");
    assert_eq!((video.width, video.height), (WIDTH, HEIGHT));

    // 3. The clip is in the index with the trigger's values, read through a fresh connection.
    let id = clip.id.expect("the clip was indexed");
    let store = open_clip_index(&db_path).expect("reopen the clip index");
    let rows = store.list_clips().expect("list the index");
    assert_eq!(rows.len(), 1, "one clip, one row");
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.path, clip.metadata.path);
    assert_eq!(row.started_at_ms, clip.started_at_ms);
    assert_eq!(row.duration_ms, clip.metadata.duration_ms);
    assert_eq!(row.size_bytes, clip.metadata.size_bytes);
    assert_eq!(row.codec, "libx264");
    eprintln!("indexed row: {row:?}");

    // 4. Clean shutdown: thread joined, encoder flushed, sources stopped, nothing panicked.
    recorder.stop().expect("stop must succeed");
    let stopped = recorder.status();
    assert!(
        !stopped.running,
        "the status must report the recorder stopped"
    );
    assert!(
        stopped.clips >= 1,
        "the session counted its clip: {stopped:?}"
    );
    assert_eq!(stopped.error, None, "a clean stop has no failure to report");
    eprintln!("after stop: {stopped:?}");

    // 5. Idempotence: a second stop, and a status read after it, must not panic or hang.
    recorder
        .stop()
        .expect("a second stop is a no-op, not an error");
    assert!(!recorder.status().running);

    // 6. A trigger after the stop fails loudly rather than hanging for ever.
    let err = recorder
        .clip_now()
        .expect_err("there is nothing left to clip");
    assert!(
        err.to_string().contains("not running") || err.to_string().contains("stopped"),
        "the error must say the recorder is gone: {err}"
    );
    eprintln!("clip_now() after stop: {err}");
}

#[test]
fn a_clip_records_why_it_was_taken() {
    // A game event goes through the same trigger path as the hotkey, via the real database:
    //   1. a manual clip (the hotkey), recorded as a bookmark event linked to the clip;
    //   2. a game event with a reason, recorded linked to the clip;
    //   3. a marker (round boundary), recorded with no clip.
    let (_tmp, app_data_dir) = application_data_dir("event-clip");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    let ready_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    wait_for(
        &recorder,
        Duration::from_secs(30),
        "enough media for a clip",
        |s| s.span_ms >= ready_ms,
    );

    // 1. Manual: exactly what the CLI does on a hotkey press.
    let manual = recorder
        .clip_now()
        .expect("the manual trigger produces a clip");

    // 2. An event: the derived reason a driver hands over, through the same call.
    let kill = GameEvent::new(
        replayed_events::Source::Lol,
        EventKind::Kill,
        serde_json::json!({ "source": "lol", "event": "ChampionKill", "killer": "Ahri" }),
    );
    let evented = recorder
        .clip_now_with(ClipReason::GameEvent(kill.clone()))
        .expect("the event trigger produces a clip too");

    // 3. A marker: no clip, just the row.
    let marker_id = recorder
        .note_event(GameEvent::bare(
            replayed_events::Source::Gsi,
            EventKind::RoundStart,
        ))
        .expect("the marker is recorded");

    recorder.stop().expect("stop must succeed");

    // Read the result back through a fresh connection, i.e. as a later run would see it.
    let store = open_clip_index(&db_path).expect("reopen the clip index");
    let clips = store.list_clips().expect("list the clips");
    assert_eq!(clips.len(), 2, "both triggers wrote a clip: {clips:?}");

    let events = store.list_events().expect("list the events");
    // Three rows, not two: a hotkey clip records a `bookmark` linked to the clip.
    assert_eq!(
        events.len(),
        3,
        "the manual clip wrote a bookmark, the event trigger wrote a kill, and the marker \
         wrote a round_start: {events:?}"
    );

    // All three belong to the session that was recording; an event with no session is a detached one.
    let session_id = events[0].session_id;
    assert!(
        session_id.is_some(),
        "an event is linked to the session that recorded it: {events:?}"
    );
    assert!(
        events.iter().all(|e| e.session_id == session_id),
        "all three events belong to the one recording session: {events:?}"
    );

    let kill_row = events
        .iter()
        .find(|e| e.kind == "kill")
        .expect("the kill's row");
    assert_eq!(
        kill_row.clip_id, evented.id,
        "linked to the clip it produced"
    );
    // The event's `at` is the trigger instant, and the trigger is inside the clip: at most a
    // pre-roll's worth of media before it, and at least a segment less (the clip starts at the
    // first whole fragment that covers the request, and that fragment can begin before it).
    let into_the_clip = kill_row.at_ms.saturating_sub(evented.started_at_ms);
    assert!(
        into_the_clip <= PRE_SECONDS * 1000
            && into_the_clip + SEGMENT_SECONDS * 1000 >= PRE_SECONDS * 1000,
        "the event's `at` is the trigger instant, which is inside the clip's pre-roll: \
         {into_the_clip}ms into a {PRE_SECONDS}s pre-roll"
    );
    assert!(
        kill_row
            .payload
            .as_deref()
            .unwrap_or("")
            .contains("ChampionKill"),
        "the integration's detail is persisted verbatim: {:?}",
        kill_row.payload
    );
    eprintln!("event row: {kill_row:?}");

    let marker_row = events
        .iter()
        .find(|e| e.id == marker_id)
        .expect("the marker's row");
    assert_eq!(marker_row.kind, "round_start");
    assert_eq!(marker_row.clip_id, None, "a marker has no clip");
    assert_eq!(marker_row.payload, None);
    assert!(
        marker_row.at_ms > kill_row.at_ms,
        "the marker is later on the media timeline: {} vs {}",
        marker_row.at_ms,
        kill_row.at_ms
    );

    // The user's own marker: tagged with the scrubber's vocabulary, linked to the hotkey clip, and
    // with no payload (a bookmark has no source to describe).
    let bookmark = events
        .iter()
        .find(|e| e.kind == BOOKMARK_KIND)
        .expect("the manual clip recorded a bookmark");
    assert_eq!(
        bookmark.clip_id, manual.id,
        "linked to the hotkey's own clip"
    );
    assert!(
        bookmark.payload.is_none(),
        "a bookmark has no source to describe"
    );
    assert_eq!(
        bookmark.session_id, session_id,
        "and it belongs to the same session"
    );

    // The event's clip is a real clip: the reason changes what is recorded about it, not the media path.
    let info = replayed_media::probe::MediaInfo::probe(
        &recorder_bin(&app_data_dir),
        &evented.metadata.path,
    )
    .expect("the event-triggered clip is a readable media file");
    assert!(
        info.video.is_some() && info.audio.is_some(),
        "video and audio, as for a hotkey clip"
    );
    // The clip must carry the whole pre-roll: that footage was already on disk, so a shorter clip
    // would mean the splice dropped footage.
    //
    // It is deliberately not asserted against the full window: the post-roll is whatever the
    // machine could write inside its budget (`post_ms` + `POST_ROLL_MARGIN`), which a loaded CI
    // runner legitimately exhausts.
    assert!(
        evented.metadata.duration_ms >= PRE_SECONDS * 1000,
        "the clip must cover the pre-roll that was already on disk: {}ms for a {PRE_SECONDS}s \
         pre-roll (the full window asked for {ready_ms}ms)",
        evented.metadata.duration_ms
    );

    // The kill is a chapter at the trigger instant; the hotkey clip, whose only event is its
    // bookmark, is left without chapters.
    let bin = recorder_bin(&app_data_dir);
    let chapters = chapter_starts(&evented.metadata.path, &bin);
    let kill_at = chapters
        .iter()
        .find(|(_, title)| title == "Kill")
        .map(|(at, _)| *at)
        .expect("the kill is a chapter of the clip it asked for");
    assert_eq!(
        chapters.first().map(|(_, title)| title.as_str()),
        Some(edit::LEAD_IN_TITLE),
        "the lead-in chapter, because the kill is later than the clip's first frame: {chapters:?}"
    );
    assert!(
        kill_at <= PRE_SECONDS * 1000 && kill_at + SEGMENT_SECONDS * 1000 >= PRE_SECONDS * 1000,
        "the kill's chapter is where the trigger was, within a segment of the pre-roll: {kill_at}ms \
         into the clip"
    );
    assert!(chapter_starts(&manual.metadata.path, &bin).is_empty());
    let row = clips
        .iter()
        .find(|c| Some(c.id) == evented.id)
        .expect("the event clip's row");
    assert_eq!(
        row.size_bytes,
        std::fs::metadata(&evented.metadata.path).unwrap().len(),
        "the index records the size after the chapters were added"
    );
}

/// The marker for the event that caused a clip stays inside that clip's pre-roll.
///
/// The clip's window is cut at whole fragments, so the clip's first frame can be a segment
/// before the requested start, and the trigger, measured from that frame, is where the
/// pre-roll ends. A marker stamped with the engine's own handling instant lands past that:
/// it names a moment the clip's pre-roll does not cover, and it is as far past it as the
/// fragment the window was cut short by.
///
/// The pre-roll here is deliberately not a whole number of segments. With a pre-roll of two
/// one-second segments the requested start lands on a fragment boundary, the overshoot is
/// nothing, and the defect hides behind an alignment that only the recording machine can
/// arrange.
#[test]
fn an_event_triggered_clip_is_stamped_inside_its_pre_roll() {
    const PRE_SECONDS: u64 = 3;
    const SEGMENT_SECONDS: u64 = 2;

    let (_tmp, app_data_dir) = application_data_dir("event-clip-pre-roll");
    let mut cfg = stub_config(&app_data_dir);
    cfg.buffer.pre_seconds = PRE_SECONDS;
    cfg.buffer.segment_time = SEGMENT_SECONDS;
    let db_path = cfg.db_path();

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "enough media for a clip",
        |s| s.span_ms >= (PRE_SECONDS + POST_SECONDS) * 1000,
    );

    let kill = GameEvent::new(
        replayed_events::Source::Lol,
        EventKind::Kill,
        serde_json::json!({ "source": "lol", "event": "ChampionKill", "killer": "Ahri" }),
    );
    let clip = recorder
        .clip_now_with(ClipReason::GameEvent(kill))
        .expect("the event trigger produces a clip");
    recorder.stop().expect("stop must succeed");

    let store = open_clip_index(&db_path).expect("reopen the clip index");
    let events = store.list_events().expect("list the events");
    let kill_row = events
        .iter()
        .find(|e| e.kind == "kill")
        .expect("the kill's row");
    // The clip's first frame, and the pre-roll measured from it: the marker has to sit at or
    // before the end of that, because the event that caused the clip happened before the
    // post-roll did.
    let pre_roll_end = clip.started_at_ms + PRE_SECONDS * 1000;
    assert!(
        kill_row.at_ms <= pre_roll_end,
        "the marker names the moment inside the clip that caused it, so it stays inside the \
         pre-roll: {}ms, and the pre-roll ends at {pre_roll_end}ms (the clip starts at {}ms)",
        kill_row.at_ms,
        clip.started_at_ms
    );
}

/// The event's `at` is the request's own instant, not the moment the engine got round to it.
///
/// The engine reads its clock when it handles the command, and a command that waits in the
/// queue is handled later than it was asked for. The marker would then name a second of the
/// recording the request never happened in, and the gap is as wide as the wait.
///
/// The kill's request is queued behind a clip with a long post-roll, so it waits in the queue
/// while the media moves on, and the two instants are seconds apart.
#[test]
fn an_event_triggered_clip_is_stamped_with_the_request_instant() {
    /// The post-roll of the clip the kill's request waits behind: five seconds of media, so
    /// the engine is busy long enough for the queue to matter.
    const BUSY_POST_SECONDS: u64 = 5;

    let (_tmp, app_data_dir) = application_data_dir("event-clip-request-instant");
    let mut cfg = stub_config(&app_data_dir);
    cfg.buffer.max_post_seconds = BUSY_POST_SECONDS;
    let db_path = cfg.db_path();

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "enough media for a clip",
        |s| s.span_ms >= (PRE_SECONDS + POST_SECONDS) * 1000,
    );

    // The kill's request: taken while the engine is busy with the clip below, so it waits in
    // the queue. The instant it carries is the one the caller asked at.
    let recorder = std::sync::Arc::new(recorder);
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let recorder_for_thread = std::sync::Arc::clone(&recorder);
    let kill_thread = std::thread::spawn(move || {
        // Long enough for the engine to be well inside its post-roll wait, short enough that
        // the wait is still running.
        std::thread::sleep(Duration::from_millis(250));
        let at = recorder_for_thread.media_now_ms();
        asked_tx.send(at).expect("the instant travels");
        recorder_for_thread
            .clip_now_with(ClipReason::GameEvent(GameEvent::bare(
                replayed_events::Source::Lol,
                EventKind::Kill,
            )))
            .expect("the event trigger produces a clip")
    });

    // The clip that occupies the engine while the request waits.
    recorder
        .clip_now_in(ClipReason::Manual, ClipWindow::new(0, BUSY_POST_SECONDS))
        .expect("the busy clip");
    let clip = kill_thread.join().expect("the kill's thread");
    recorder.stop().expect("stop must succeed");

    let asked_at = asked_rx.recv().expect("the instant the request carried");
    let store = open_clip_index(&db_path).expect("reopen the clip index");
    let events = store.list_events().expect("list the events");
    let kill_row = events
        .iter()
        .find(|e| e.kind == "kill")
        .expect("the kill's row");
    assert!(
        kill_row.at_ms <= asked_at + SEGMENT_SECONDS * 1000,
        "the marker names the moment the request was made ({asked_at}ms), not the moment the \
         engine handled it: the row says {}ms, and the clip's own pre-roll ends at {}ms",
        kill_row.at_ms,
        clip.started_at_ms + PRE_SECONDS * 1000
    );
}

/// Each chapter's `(start ms, title)`, read back with ffprobe.
fn chapter_starts(path: &Path, bin: &FfmpegBinaries) -> Vec<(u64, String)> {
    let output = std::process::Command::new(&bin.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_chapters"])
        .arg(path)
        .output()
        .expect("running ffprobe");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("ffprobe json");
    json["chapters"]
        .as_array()
        .expect("a chapters array")
        .iter()
        .map(|c| {
            let start: f64 = c["start_time"].as_str().unwrap().parse().unwrap();
            (
                (start * 1000.0).round() as u64,
                c["tags"]["title"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The ffmpeg binaries the recorder used, discovered the way the test configuration does.
fn recorder_bin(_app_data_dir: &Path) -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH")
}

/// Regression test: **the pacer and the encoder child are given the same rate, and it is the
/// rate the probe measured.**
///
/// Declaring the configured `encode.fps` to both on a machine that cannot
/// encode it would drop the surplus from the encoder's bounded queue (~45% of frames) and run the
/// media timeline slower than real time. Here `encode.fps = 120`, the measurement is 10 and
/// the source offers 120fps, so a consumer that recomputes its rate from `encode.fps` fails:
///
/// * `effective_fps` is the encoder child's own `-framerate` (via `Encoder::input_fps`), and the
///   pacer is built from the same read-back;
/// * `frames` must advance at the pacer's rate, observable because the source offers more;
/// * nothing may be dropped.
#[test]
fn the_pacer_and_the_encoder_are_told_the_same_rate() {
    let (_tmp, app_data_dir) = application_data_dir("rate-agreement");
    let mut cfg = stub_config(&app_data_dir);
    cfg.encode.fps = 120;
    cfg.encode.adapt_fps = true;
    // The source offers the configured rate; the pipeline may only keep what it declared.
    cfg.sources = Sources::Stub(StubConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: 120,
    });

    // A measurement well below the configured rate, as on a 4K machine.
    let measured_fps = 10.0;
    let recorder = Recorder::start_with_measure(cfg, &|encode_cfg: &EncodeConfig| {
        assert_eq!(
            encode_cfg.fps, 120,
            "the probe is handed the configuration that declares the *configured* rate: the \
             number being measured is part of the ffmpeg invocation"
        );
        Ok(ThroughputMeasurement {
            fps: measured_fps,
            frames: 15,
            window: Duration::from_millis(1500),
            source_size: (WIDTH, HEIGHT),
            output_size: (WIDTH, HEIGHT),
        })
    })
    .expect("the recorder starts");

    let status = recorder.status();
    assert_eq!(
        status.configured_fps, 120,
        "what the config asked for is still reported"
    );
    assert_eq!(
        status.effective_fps, 10,
        "the pipeline must run at the measured 10fps: 10.0 floored is 10, and the encoder \
         child's own -framerate is where this value comes from"
    );

    // What the pacer admits: the source offers 120fps, so frames reaching the encoder per second is the pacer's rate.
    let first = wait_for(
        &recorder,
        Duration::from_secs(10),
        "a first rate sample",
        |s| s.frames > 0,
    );
    let t0 = Instant::now();
    let second = wait_for(
        &recorder,
        Duration::from_secs(10),
        "two seconds of capture",
        |_| Instant::now().duration_since(t0) >= Duration::from_millis(2000),
    );
    let wall = Instant::now().duration_since(t0).as_secs_f64();
    let submitted = (second.frames - first.frames) as f64;
    let observed = submitted / wall;
    eprintln!(
        "configured 120fps, measured {measured_fps}fps: {} frames over {wall:.2}s = \
         {observed:.1}/s (declared {}); skipped={} dropped={}",
        second.frames - first.frames,
        second.effective_fps,
        second.skipped,
        second.dropped
    );
    assert!(
        (5.0..=20.0).contains(&observed),
        "the pacer must admit the 10fps it declared, not the configured 120: {observed:.1}/s \
         over {wall:.2}s ({status:?})"
    );
    assert!(
        second.skipped > 0,
        "a source offering 120fps against a 10fps pacer is the case the readback skip exists \
         for: {second:?}"
    );
    assert_eq!(
        second.dropped, 0,
        "pacing to what the encoder was told is what stops the bounded queue from dropping: \
         {second:?}"
    );

    // Media time must track the wall clock (`-f segment` resampling onto a declared grid once ran
    // media at 0.66x).
    //
    // `span_ms` is a staircase that moves in whole-segment steps, so dividing it by a fixed
    // wall-clock window gives an answer that depends on where the window lands. So wait for
    // conditions, not durations: first the first segment on disk, then a defined amount of media
    // (`MEDIA_WINDOW_MS`), and measure how long that took. A warped clock still fails (at 0.66x the
    // ratio is near 0.66).
    const MEDIA_WINDOW_MS: u64 = 3_000;
    let steady = wait_for(
        &recorder,
        Duration::from_secs(15),
        "the first segment on disk",
        |s| s.span_ms > 0,
    );
    let span_first = steady.span_ms;
    let t1 = Instant::now();
    let third = wait_for(
        &recorder,
        Duration::from_secs(30),
        "three more seconds of media",
        |s| s.span_ms.saturating_sub(span_first) >= MEDIA_WINDOW_MS,
    );
    let media = (third.span_ms.saturating_sub(span_first)) as f64;
    let wall = Instant::now().duration_since(t1).as_secs_f64() * 1000.0;
    eprintln!(
        "media time advanced {media:.0}ms over {wall:.0}ms of wall clock (from span \
         {span_first}ms)"
    );
    assert!(
        (0.75..=1.25).contains(&(media / wall)),
        "media time must track the wall clock (issue #2): {media}ms of media over {wall}ms of \
         wall clock"
    );

    recorder.stop().expect("stop must succeed");
}

/// `encode.adapt_fps = false` skips the probe entirely (it costs ~1.5s of startup) and declares
/// the configured rate exactly.
#[test]
fn with_adaptation_off_the_configured_rate_is_declared_and_nothing_is_measured() {
    let (_tmp, app_data_dir) = application_data_dir("rate-no-adaptation");
    let mut cfg = stub_config(&app_data_dir);
    cfg.encode.fps = 60;
    cfg.encode.adapt_fps = false;

    let recorder = Recorder::start_with_measure(cfg, &|_: &EncodeConfig| {
        panic!("no measurement may be taken when encode.adapt_fps = false")
    })
    .expect("the recorder starts without measuring");

    let status = recorder.status();
    assert_eq!(status.configured_fps, 60);
    assert_eq!(
        status.effective_fps, 60,
        "the configured rate is declared as it always was"
    );
    recorder.stop().expect("stop must succeed");
}

#[test]
fn a_stopped_recorder_keeps_reporting_a_stable_status() {
    // The desktop shell polls the status around a stop, so a read after the loop is gone must be a
    // value, not a panic or hang. The counters are the ones the session reached, not zeros.
    let (_tmp, app_data_dir) = application_data_dir("stopped-status");
    let recorder = Recorder::start(stub_config(&app_data_dir)).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(30),
        "the buffer to start",
        |s| s.frames > 0,
    );

    recorder.stop().expect("stop must succeed");
    let first = recorder.status();
    let second = recorder.status();
    assert_eq!(first, second, "two reads of a stopped recorder agree");
    assert!(!first.running);
    assert!(first.frames > 0, "the counters survive the stop: {first:?}");
}

#[test]
fn an_unusable_encoder_configuration_fails_before_any_capture_starts() {
    // The encoder is resolved (and smoke-tested) before the capture backend exists, so a
    // configuration this machine cannot encode with fails at `start` and names the wrong setting.
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut cfg = stub_config(dir.path());
    cfg.encode.codec = "vp9".to_string();

    let err = Recorder::start(cfg)
        .map(|_| ())
        .expect_err("vp9 is not a clip codec");
    assert!(
        err.to_string().contains("unsupported encode.codec"),
        "the error must name the setting: {err}"
    );

    // A nonexistent vendor is rejected while resolving the encoder, before any capture session.
    // (The software-encoder path above skips vendor resolution, so this case clears the flag.)
    let mut cfg = stub_config(dir.path());
    cfg.dev_software_encoder = false;
    cfg.encode.vendor = "intel-arc".to_string();
    let err = Recorder::start(cfg)
        .map(|_| ())
        .expect_err("that is not a vendor");
    assert!(
        err.to_string().contains("unknown encode.vendor"),
        "got: {err}"
    );

    // Observable consequence of the ordering: neither failure reached the scratch directory.
    assert!(
        !dir.path().join("scratch").exists(),
        "the encoder must be resolved before the ring, and the ring before any capture"
    );
}

#[test]
fn a_recorder_reports_itself_as_not_running_before_it_is_started() {
    // What the desktop shell shows when nothing is recording; `configured_fps` is 0 because nothing is configured.
    let idle = RecorderStatus::stopped();
    assert!(!idle.running);
    assert_eq!(idle.configured_fps, 0);
    assert_eq!(idle.frames, 0);
    assert_eq!(idle.error, None);
}

// ---------------------------------------------------------------------------------------
// The mode, the session lifecycle, the microphone and the game watcher
// ---------------------------------------------------------------------------------------

use crate::session::layout_test_segments;
use replayed_events::process::{PresenceChange, WatchedGame};
use replayed_store::SESSION_MODE_SESSION;

/// The options these tests build, so each states only what it changes.
fn options(mode: RecordingMode, mic: bool) -> RecorderOptions {
    RecorderOptions {
        mode,
        save_session: false,
        mic: MicSection {
            enabled: mic,
            device: String::new(),
        },
        games: GamesSection::default(),
        game: None,
        fail_starts: FailStarts::default(),
        audio_backends: AudioBackendOverrides::default(),
        panic_engines: PanicEngines::default(),
        retention_probe: RetentionProbe::default(),
        capture_backend: CaptureOverride::default(),
    }
}

/// [`options`] with a capture backend of the test's own in place of the stub.
fn options_with_capture(mode: RecordingMode, capture: ScriptedCapture) -> RecorderOptions {
    RecorderOptions {
        capture_backend: CaptureOverride::with(Box::new(capture)),
        ..options(mode, false)
    }
}

/// A capture backend written for the test: the stub's frames, plus what a real backend can say
/// that the stub cannot (a window that closed, a device that was lost, a stall).
///
/// The size change is the stub's own ([`StubCapture::resize_at_frame`]); the other two are
/// scripted here because a recording's reaction to them is the engine's, not the backend's.
struct ScriptedCapture {
    inner: StubCapture,
    /// After this many frames have been handed out, `next_frame` fails with this.
    lose: Option<(u64, CaptureLost)>,
    offered: u64,
    /// How long the capture reports having been without a frame, in ms; 0 is "not stalled".
    stalled_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl ScriptedCapture {
    fn new() -> Self {
        Self {
            inner: StubCapture::new(StubConfig {
                width: WIDTH,
                height: HEIGHT,
                fps: FPS,
            }),
            lose: None,
            offered: 0,
            stalled_ms: Arc::default(),
        }
    }

    /// The source changes to `size` as it produces its `frame_index`th frame.
    fn resizing_at(mut self, frame_index: u64, size: (u32, u32)) -> Self {
        self.inner = self.inner.resize_at_frame(frame_index, size);
        self
    }

    /// The capture is lost for good once `frames` frames have been handed out.
    fn lost_after(mut self, frames: u64, lost: CaptureLost) -> Self {
        self.lose = Some((frames, lost));
        self
    }
}

impl CaptureBackend for ScriptedCapture {
    fn start(&mut self) -> Result<()> {
        self.inner.start()
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if let Some((after, lost)) = &self.lose {
            if self.offered >= *after {
                return Err(anyhow::Error::new(lost.clone()));
            }
        }
        let frame = self.inner.next_frame(timeout)?;
        if frame.is_some() {
            self.offered += 1;
        }
        Ok(frame)
    }

    fn discard_pending(&mut self) -> Result<usize> {
        self.inner.discard_pending()
    }

    fn stop(&mut self) -> Result<()> {
        self.inner.stop()
    }

    fn native_size(&self) -> (u32, u32) {
        self.inner.native_size()
    }

    fn take_size_change(&mut self) -> Option<(u32, u32)> {
        self.inner.take_size_change()
    }

    fn stalled_for(&self) -> Option<Duration> {
        match self.stalled_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }
}

/// Decode every frame of `path` into the null muxer, with ffmpeg's log in the form this file's
/// checks can read line by line.
///
/// `repeat+error` rather than `error`: by default ffmpeg folds a message identical to the one
/// before it into a second line, `    Last message repeated N times`, and that line names nothing,
/// so a check that recognises a complaint by its text cannot tell what it repeats. The synthetic
/// source stamps two frames at a segment boundary with the same timestamp, and when *several* frames
/// land on one timestamp the null muxer's complaint about it comes out identical each time, which
/// is when ffmpeg folds it. Whether it does depends on how the child's reads were spread by the
/// machine's load, so a check that filtered the complaint and not its summary passed on an idle host
/// and failed one in ten under load, on a file that was fine. With `repeat` every occurrence is its
/// own line and is judged on its own text.
fn decode_to_null(bin: &FfmpegBinaries, path: &Path) -> std::process::Output {
    std::process::Command::new(&bin.ffmpeg)
        .args(["-v", "repeat+error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .expect("running ffmpeg")
}

/// Asserts that ffmpeg decodes every frame of `path` without a complaint: the proof that a file
/// made of segments of two sizes is one file that plays, which the probe's header does not give.
fn assert_decodes_cleanly(bin: &FfmpegBinaries, path: &Path) {
    let output = decode_to_null(bin, path);
    // The null muxer objects to two frames with the same timestamp ("non monotonically increasing
    // dts"), which the synthetic ten-frames-a-second source produces at a segment boundary in any
    // session, resized or not. That is a property of the test's source, not of what is under test, so
    // it is not counted; every other complaint is (a decode error, a size change, a corrupt packet).
    let stderr = String::from_utf8_lossy(&output.stderr);
    let complaints: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.contains("non monotonically increasing dts"))
        .collect();
    assert!(
        output.status.success() && complaints.is_empty(),
        "{} does not decode cleanly: {complaints:?}",
        path.display(),
    );
}

/// A file that makes the null muxer repeat itself decodes cleanly as far as this file's checks go.
///
/// Three frames to a timestamp is the case [`decode_to_null`] exists for: the complaint about the
/// second frame and the one about the third are the same line, which ffmpeg folds into "Last
/// message repeated 1 times" unless it is asked not to. That used to fail
/// `a_session_follows_the_capture_to_a_new_size_and_ends_as_one_file_that_plays` whenever load made
/// the live source put more than two frames on one timestamp at a segment boundary; this builds the
/// same shape on purpose, so the failure does not wait for a busy machine.
///
/// The test also checks that the fixture does what it is for (the duplicates are there, and
/// identical): a ffmpeg whose null muxer stopped objecting would otherwise leave this passing without
/// proving anything.
#[test]
fn a_decode_check_judges_every_repeat_of_the_null_muxers_timestamp_complaint() {
    let (_tmp, dir) = application_data_dir("decode-repeats");
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    let bin = recorder_bin(&dir);
    let path = dir.join("three-frames-to-a-timestamp.mkv");
    let made = std::process::Command::new(&bin.ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=r=10:d=1:s=64x48",
        ])
        .args([
            "-vf",
            "setpts='floor(N/3)*0.3/TB'",
            "-fps_mode",
            "passthrough",
        ])
        .args(["-c:v", "libx264", "-y"])
        .arg(&path)
        .output()
        .expect("running ffmpeg");
    assert!(made.status.success(), "{made:?}");

    let stderr = String::from_utf8_lossy(&decode_to_null(&bin, &path).stderr).into_owned();
    let dts: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("non monotonically increasing dts"))
        .collect();
    assert!(
        dts.len() >= 3 && dts.windows(2).any(|pair| pair[0] == pair[1]),
        "the fixture must make the muxer repeat an identical complaint: {stderr:?}"
    );
    assert!(
        !stderr.contains("Last message repeated"),
        "every repeat is its own line, so none is a summary the checks cannot read: {stderr:?}"
    );
    assert_decodes_cleanly(&bin, &path);
}

/// The video size of a file, as probed.
fn video_size(bin: &FfmpegBinaries, path: &Path) -> (u32, u32) {
    let info = replayed_media::probe::MediaInfo::probe(bin, path).expect("probing the file");
    let video = info.video.expect("the file has a video stream");
    (video.width, video.height)
}

/// A session follows its capture across a resize and ends as one file that plays.
///
/// The window grows from 64x48 to 80x60 a second and a half in. Without following it the first
/// new frame fails the pump's size guard and the recording is over; following it, the new
/// encoder is declared at 80x60 and scales to the 64x48 the session started at, so the lossless
/// concatenation of all the segments is one file with one set of parameters.
#[test]
fn a_session_follows_the_capture_to_a_new_size_and_ends_as_one_file_that_plays() {
    let (_tmp, app_data_dir) = application_data_dir("session-resize");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let capture = ScriptedCapture::new().resizing_at(15, (80, 60));
    let recorder = Recorder::start_with_options(
        cfg,
        options_with_capture(RecordingMode::FullSession, capture),
    )
    .expect("the session recorder starts");

    // Four more seconds of frames after the resize: the recording is still going.
    let after = wait_for(
        &recorder,
        Duration::from_secs(40),
        "frames to keep arriving after the resize",
        |s| s.frames >= 55 || s.error.is_some() || !s.running,
    );
    assert!(
        after.running,
        "the recording survived the resize: {after:?}"
    );
    assert_eq!(after.error, None, "a resize is not a failure");
    assert_eq!(after.ended, None, "and not an end");
    assert!(after.frames >= 55, "{after:?}");

    recorder.stop().expect("stop must succeed");
    let row = wait_for_finished_session(&bin, &db_path);
    let session_file = PathBuf::from(row.final_path.expect("the session file"));
    assert_eq!(
        video_size(&bin, &session_file),
        (WIDTH, HEIGHT),
        "the output size is held, so every segment carries the same parameters"
    );
    assert_decodes_cleanly(&bin, &session_file);
    let info = replayed_media::probe::MediaInfo::probe(&bin, &session_file).expect("probing it");
    assert!(
        info.duration_ms >= 4_000,
        "the file holds the footage from before the resize and after it: {}ms",
        info.duration_ms
    );
}

/// A replay buffer follows its capture across a resize, and a clip taken afterwards is at the new size.
///
/// The ring holds one stream with one header, so the resize replaces it: the footage from before
/// is gone and the buffer fills again. What matters is that the recording is still alive and
/// that a clip can be taken from the new ring.
#[test]
fn a_replay_buffer_follows_the_capture_to_a_new_size_and_clips_at_it() {
    let (_tmp, app_data_dir) = application_data_dir("buffer-resize");
    let cfg = stub_config(&app_data_dir);
    let bin = cfg.bin.clone();

    let capture = ScriptedCapture::new().resizing_at(15, (80, 60));
    let recorder = Recorder::start_with_options(
        cfg,
        options_with_capture(RecordingMode::ReplayBuffer, capture),
    )
    .expect("the buffer starts");

    let after = wait_for(
        &recorder,
        Duration::from_secs(40),
        "frames to keep arriving after the resize",
        |s| s.frames >= 55 || s.error.is_some() || !s.running,
    );
    assert!(after.running, "the buffer survived the resize: {after:?}");
    assert_eq!(after.error, None, "a resize is not a failure");
    assert!(
        after.dropped <= after.frames,
        "the drop counter is one cumulative number across both encoders: {after:?}"
    );

    // The new ring has to hold the window before a clip can be cut from it.
    let ready_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the new ring to fill",
        |s| s.span_ms >= ready_ms,
    );
    let clip = recorder
        .clip_now()
        .expect("a clip can be taken from the ring that followed the resize");
    assert_eq!(
        video_size(&bin, &clip.metadata.path),
        (80, 60),
        "a buffer's new stream is at the new size"
    );
    assert_eq!(stream_counts(&clip.metadata.path, &bin), (1, 1));
    assert_decodes_cleanly(&bin, &clip.metadata.path);
    recorder.stop().expect("stop must succeed");
}

/// A buffer that is also saving its session cannot follow a resize, and says so rather than
/// writing a second header into the middle of the session file.
#[test]
fn a_replay_buffer_that_saves_its_session_ends_cleanly_at_a_resize() {
    let (_tmp, app_data_dir) = application_data_dir("saved-session-resize");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let capture = ScriptedCapture::new().resizing_at(25, (80, 60));
    let options = RecorderOptions {
        save_session: true,
        ..options_with_capture(RecordingMode::ReplayBuffer, capture)
    };
    let recorder = Recorder::start_with_options(cfg, options).expect("the buffer starts");

    let ended = wait_for(
        &recorder,
        Duration::from_secs(40),
        "the recording to end at the resize",
        |s| !s.running,
    );
    assert_eq!(
        ended.error, None,
        "an end the recorder chose is not a failure"
    );
    let why = ended.ended.expect("the user is told why it ended");
    assert!(
        why.contains("64x48") && why.contains("80x60"),
        "the message names both sizes: {why}"
    );
    recorder.stop().expect("stop reports no failure");

    // What was recorded is a session that plays, at the size it was recorded.
    let row = wait_for_finished_session(&bin, &db_path);
    let file = PathBuf::from(row.final_path.expect("the saved session was finalised"));
    assert_eq!(video_size(&bin, &file), (WIDTH, HEIGHT));
    assert_decodes_cleanly(&bin, &file);
}

/// The captured window closing ends the recording cleanly: a message, no failure, a finalised session.
#[test]
fn a_closed_capture_target_ends_the_recording_cleanly_with_a_finalised_session() {
    let (_tmp, app_data_dir) = application_data_dir("target-closed");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let capture = ScriptedCapture::new().lost_after(25, CaptureLost::TargetClosed);
    let recorder = Recorder::start_with_options(
        cfg,
        options_with_capture(RecordingMode::FullSession, capture),
    )
    .expect("the session recorder starts");

    let ended = wait_for(
        &recorder,
        Duration::from_secs(40),
        "the recording to end when its target closed",
        |s| !s.running,
    );
    assert_eq!(
        ended.error, None,
        "a closed window is the recording doing its job, not a fault: {ended:?}"
    );
    let why = ended.ended.expect("the user is told why it ended");
    assert!(
        why.contains("went away") && why.contains("kept"),
        "the message says what happened and that nothing was lost: {why}"
    );
    recorder
        .stop()
        .expect("stopping a recording that ended cleanly reports no failure");

    let row = wait_for_finished_session(&bin, &db_path);
    let file = PathBuf::from(row.final_path.expect("the session was finalised"));
    assert!(file.is_file());
    assert_decodes_cleanly(&bin, &file);
    assert!(
        replayed_media::probe::MediaInfo::probe(&bin, &file)
            .expect("probing it")
            .duration_ms
            >= 2_000,
        "the footage up to the close was kept"
    );
}

/// A device that is gone for good is a failure, with the backend's own reason in the message, and
/// the session is still finalised on the way out.
#[test]
fn a_lost_graphics_device_fails_the_recording_with_the_reason_and_keeps_the_footage() {
    let (_tmp, app_data_dir) = application_data_dir("device-lost");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let capture = ScriptedCapture::new().lost_after(
        25,
        CaptureLost::DeviceLost {
            attempts: 3,
            reason: "the adapter was removed".to_string(),
        },
    );
    let recorder = Recorder::start_with_options(
        cfg,
        options_with_capture(RecordingMode::FullSession, capture),
    )
    .expect("the session recorder starts");

    let ended = wait_for(
        &recorder,
        Duration::from_secs(40),
        "the recording to fail when its device was lost",
        |s| !s.running,
    );
    let why = ended.error.clone().expect("a lost device is a failure");
    assert!(
        why.contains("3 attempts") && why.contains("the adapter was removed"),
        "the message carries the attempts and the backend's reason: {why}"
    );
    assert_eq!(ended.ended, None, "it did not end cleanly");
    let reported = recorder.stop().expect_err("stop reports the failure");
    assert!(format!("{reported:#}").contains("the adapter was removed"));

    let row = wait_for_finished_session(&bin, &db_path);
    let file = PathBuf::from(row.final_path.expect("the footage was finalised"));
    assert_decodes_cleanly(&bin, &file);
}

/// The status shows how long the capture has been without a frame, and stops showing it when
/// frames are back.
#[test]
fn the_status_shows_a_stalled_capture_and_stops_showing_it_when_frames_return() {
    let (_tmp, app_data_dir) = application_data_dir("capture-stall");
    let cfg = stub_config(&app_data_dir);

    let capture = ScriptedCapture::new();
    let stall = Arc::clone(&capture.stalled_ms);
    let recorder = Recorder::start_with_options(
        cfg,
        options_with_capture(RecordingMode::ReplayBuffer, capture),
    )
    .expect("the buffer starts");

    let healthy = wait_for(
        &recorder,
        Duration::from_secs(30),
        "frames to arrive",
        |s| s.frames > 0,
    );
    assert_eq!(
        healthy.stalled_for_ms, None,
        "frames are arriving: {healthy:?}"
    );

    stall.store(12_000, Ordering::Relaxed);
    let stalled = wait_for(
        &recorder,
        Duration::from_secs(10),
        "the stall to be published",
        |s| s.stalled_for_ms.is_some(),
    );
    assert_eq!(stalled.stalled_for_ms, Some(12_000));
    assert!(
        stalled.running,
        "a stall is a state, not a failure: {stalled:?}"
    );
    assert_eq!(stalled.error, None);

    stall.store(0, Ordering::Relaxed);
    wait_for(
        &recorder,
        Duration::from_secs(10),
        "the stall to clear",
        |s| s.stalled_for_ms.is_none(),
    );
    recorder.stop().expect("stop must succeed");
}

/// The `(video, audio)` stream counts of a file, counted by ffprobe (`MediaInfo` reports only the first stream of each kind).
fn stream_counts(path: &Path, bin: &FfmpegBinaries) -> (usize, usize) {
    let output = std::process::Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("running ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let video = text.lines().filter(|line| line.trim() == "video").count();
    let audio = text.lines().filter(|line| line.trim() == "audio").count();
    (video, audio)
}

/// The session directories and the session files in an application data directory.
fn sessions_on_disk(app_data_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let sessions = app_data_dir.join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "mp4") {
                files.push(path);
            }
        }
    }
    (dirs, files)
}

/// Wait for the session store to hold exactly one finished session, and return its row.
///
/// The engine writes the row from its own connection, so the read is a poll.
fn wait_for_finished_session(bin: &FfmpegBinaries, db: &Path) -> replayed_store::Session {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let store = open_clip_index(db).expect("the index opens");
        let sessions = store.list_sessions().expect("listing sessions");
        if let Some(row) = sessions.iter().find(|row| row.ended_at_ms.is_some()) {
            return row.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no finished session appeared: {sessions:?}"
        );
        let _ = bin;
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_full_session_records_into_its_own_directory_and_keeps_every_segment() {
    // A full session never evicts: it keeps its segments as they age, unlike a rolling buffer.
    let (_tmp, app_data_dir) = application_data_dir("full-session");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");

    let grown = wait_for(
        &recorder,
        Duration::from_secs(40),
        "four seconds of session",
        |s| s.span_ms >= 4_000,
    );
    assert_eq!(grown.mode, RecordingMode::FullSession);
    assert!(
        grown.segments >= 4,
        "four seconds of 1s segments must all still be there — a session evicts nothing: {grown:?}"
    );
    assert!(
        grown.bytes > 4 * 1_024,
        "and the footage is on disk: {grown:?}"
    );
    assert_eq!(grown.error, None, "nothing failed");

    // The segments are in a per-session directory under the sessions area, not in scratch/.
    let (dirs, files) = sessions_on_disk(&app_data_dir);
    assert_eq!(dirs.len(), 1, "one session directory: {dirs:?}");
    assert!(
        files.is_empty(),
        "no session file exists until the recording stops: {files:?}"
    );
    assert!(
        dirs[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("session-"),
        "named from the wall clock: {:?}",
        dirs[0]
    );
    let segments_in_dir = std::fs::read_dir(&dirs[0])
        .unwrap()
        .filter_map(|e| e.ok())
        .count();
    assert!(
        segments_in_dir >= 4,
        "the segments are there: {segments_in_dir}"
    );

    recorder.stop().expect("stop must succeed");

    // One finished session, one file, both streams, and the temporary segments gone.
    let row = wait_for_finished_session(&bin, &db_path);
    assert_eq!(row.mode, SESSION_MODE_SESSION);
    assert_eq!(
        row.game, None,
        "a manually started recording carries no game name"
    );
    let final_path = row
        .final_path
        .clone()
        .expect("the row names the session file");
    let final_path = PathBuf::from(&final_path);
    assert!(final_path.is_file(), "{final_path:?} exists");
    assert_eq!(
        stream_counts(&final_path, &bin),
        (1, 1),
        "one video, one audio track"
    );
    assert_eq!(
        row.size_bytes as u64,
        std::fs::metadata(&final_path).unwrap().len()
    );
    assert!(
        row.size_bytes > 4 * 1_024,
        "the session file is bigger than the cap: {row:?}"
    );
    let info = replayed_media::probe::MediaInfo::probe(&bin, &final_path).expect("probing it");
    assert!(
        info.duration_ms >= 4_000,
        "the session file covers the whole session: {}ms",
        info.duration_ms
    );
    // `duration_ms` is the file's own probed length (the axis a review timeline is drawn against);
    // two measurements of the same file, so the equality holds however loaded the machine was.
    assert_eq!(
        row.duration_ms, info.duration_ms as i64,
        "the session row must carry the recorded media's length, not the window it was \
         recorded in"
    );
    let (dirs, files) = sessions_on_disk(&app_data_dir);
    assert!(
        dirs.is_empty(),
        "the temporary segments are removed: {dirs:?}"
    );
    assert_eq!(
        files,
        vec![final_path],
        "and the session file is the only thing left"
    );
}

#[test]
fn a_full_session_with_a_microphone_concatenates_both_audio_tracks() {
    let (_tmp, app_data_dir) = application_data_dir("session-mic");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    // `Sources::Stub` supplies the microphone via `StubMicrophone`, the only way to cover this without WASAPI.
    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, true))
        .expect("the session recorder starts with a microphone");

    let status = recorder.status();
    assert!(status.mic, "the recording carries a microphone track");
    assert!(
        status.mic_port.is_some(),
        "and the encoder declared a second audio input: {status:?}"
    );
    assert_eq!(
        status.dropped_mic_audio, 0,
        "nothing has been dropped: {status:?}"
    );

    let grown = wait_for(
        &recorder,
        Duration::from_secs(40),
        "three seconds of session",
        |s| s.span_ms >= 3_000,
    );
    assert_eq!(
        grown.dropped_mic_audio, 0,
        "the voice track is being fed: {grown:?}"
    );
    assert_eq!(grown.error, None);

    // A clip taken during the session carries both tracks too: its trigger splices with `-map 0`.
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "enough media for a clip",
        |s| s.span_ms >= (PRE_SECONDS + POST_SECONDS) * 1000,
    );
    let clip = recorder
        .clip_now()
        .expect("the trigger must produce a clip");
    assert_eq!(
        stream_counts(&clip.metadata.path, &bin),
        (1, 2),
        "a clip out of a session with a microphone: {:?}",
        clip.metadata.path
    );

    recorder.stop().expect("stop must succeed");

    let row = wait_for_finished_session(&bin, &db_path);
    let final_path = PathBuf::from(row.final_path.expect("the session file"));
    // `-c copy` through a concat list is only lossless if the second audio track survives (`session::finalise`'s `-map 0`).
    assert_eq!(
        stream_counts(&final_path, &bin),
        (1, 2),
        "one video and **two** audio tracks in {final_path:?}"
    );
}

#[test]
fn a_clip_taken_during_a_full_session_is_the_same_instant_splice() {
    let (_tmp, app_data_dir) = application_data_dir("session-clip");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");
    let ready_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "enough media for a clip",
        |s| s.span_ms >= ready_ms,
    );

    // The hotkey path in session mode: an instant clip out of the footage the session is writing.
    let clip = recorder
        .clip_now()
        .expect("the trigger must produce a clip");
    assert!(clip.metadata.path.is_file());
    assert_eq!(stream_counts(&clip.metadata.path, &bin), (1, 1));
    assert_eq!(clip.metadata.encoder, "libx264");
    assert!(clip.id.is_some(), "the clip is indexed");
    assert!(
        clip.metadata.path.starts_with(app_data_dir.join("clips")),
        "and it went to the clips directory, not the session's: {:?}",
        clip.metadata.path
    );

    recorder.stop().expect("stop must succeed");

    // Both artefacts survive: the clip and the session file.
    let row = wait_for_finished_session(&bin, &db_path);
    let session_file = PathBuf::from(row.final_path.expect("the session file"));
    assert!(session_file.is_file());
    assert!(clip.metadata.path.is_file());
    let store = open_clip_index(&db_path).expect("the index opens");
    assert_eq!(store.list_clips().unwrap().len(), 1, "one clip row");
    assert_eq!(store.list_sessions().unwrap().len(), 1, "one session row");
}

#[test]
fn the_session_row_is_opened_before_capture_and_kept_current_while_it_records() {
    let (_tmp, app_data_dir) = application_data_dir("session-row");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "three seconds of session",
        |s| s.span_ms >= 3_000,
    );

    // Read the row through a second connection, as the desktop shell's session list would.
    let store = open_clip_index(&db_path).expect("the index opens");
    let rows = store.list_sessions().expect("listing sessions");
    assert_eq!(rows.len(), 1, "one session row: {rows:?}");
    let running = &rows[0];
    assert!(running.ended_at_ms.is_none(), "it is still recording");
    assert_eq!(running.mode, SESSION_MODE_SESSION);
    assert!(
        running.size_bytes > 0,
        "a multi-hour recording has to be visible to the retention cap before it stops: {running:?}"
    );
    assert!(
        Path::new(&running.scratch_dir).is_dir(),
        "the row names the directory the segments are in: {}",
        running.scratch_dir
    );
    let id = running.id;
    drop(store);

    recorder.stop().expect("stop must succeed");
    recorder
        .stop()
        .expect("a second stop is a no-op, not an error");

    let store = open_clip_index(&db_path).expect("the index opens");
    let rows = store.list_sessions().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a second stop does not open a second session"
    );
    let finished = &rows[0];
    assert_eq!(finished.id, id);
    assert!(
        finished.ended_at_ms.is_some(),
        "the row is closed: {finished:?}"
    );
    let path = PathBuf::from(finished.final_path.clone().expect("the session file"));
    assert!(path.is_file());
    assert_eq!(
        finished.size_bytes as u64,
        std::fs::metadata(&path).unwrap().len(),
        "and the size is the concatenated file's own"
    );
}

#[test]
fn the_microphone_is_off_by_default_and_leaves_the_encoder_invocation_unchanged() {
    let (_tmp, app_data_dir) = application_data_dir("mic-off");
    let cfg = stub_config(&app_data_dir);

    // With the microphone off the encoder configuration has `mic_audio: None`, whose argument list
    // `replayed-encoder` pins byte for byte.
    let encode_cfg = build_encode_config(
        &cfg,
        &cfg.scratch_dir(),
        replayed_encoder::VideoCodec::H264,
        None,
        (WIDTH, HEIGHT),
    )
    .expect("the encoder configuration");
    assert!(
        encode_cfg.mic_audio.is_none(),
        "no microphone means no second audio input (and no microphone URL in the args)"
    );

    // The engine's default is the microphone off.
    let options = RecorderOptions::default();
    assert!(!options.mic.enabled, "the microphone is opt-in");
    assert_eq!(
        options.mode,
        RecordingMode::ReplayBuffer,
        "and so is full-session mode"
    );
    assert!(
        !options.games.auto_record,
        "and so is game-driven recording"
    );

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    let status = recorder.status();
    assert!(!status.mic);
    assert!(
        status.mic_port.is_none(),
        "the encoder opened no microphone input: {status:?}"
    );
    assert_eq!(status.dropped_mic_audio, 0);
    wait_for(
        &recorder,
        Duration::from_secs(30),
        "the buffer to start",
        |s| s.span_ms > 0,
    );
    assert_eq!(
        recorder.status().dropped_mic_audio,
        0,
        "and stays 0 while recording"
    );
    recorder.stop().expect("stop must succeed");
}

/// A microphone that is on but cannot start fails the recording **loudly**, before anything is
/// captured, never a video with a silent voice track.
///
/// Off Windows `Sources::Platform` has no microphone backend at all, which is the machine this
/// runs on; on Windows the same path reports a machine with no capture endpoint.
#[cfg(not(windows))]
#[test]
fn a_microphone_that_cannot_start_fails_the_recording_before_any_capture() {
    let (_tmp, app_data_dir) = application_data_dir("mic-unavailable");
    let mut cfg = stub_config(&app_data_dir);
    cfg.sources = Sources::Platform;
    let db_path = cfg.db_path();

    let err = Recorder::start_with_options(cfg, options(RecordingMode::ReplayBuffer, true))
        .map(|_| ())
        .expect_err("enabling the microphone on a platform without one must fail");

    let message = format!("{err:#}");
    assert!(
        message.contains("microphone"),
        "the error must name the microphone: {message}"
    );
    assert!(
        !app_data_dir.join("scratch").exists(),
        "nothing was captured: the capture backend is created after the microphone"
    );
    // The index was opened (the first step) but no session was opened for a recording that never started.
    let store = open_clip_index(&db_path).expect("the index opens");
    assert!(store.list_sessions().expect("listing sessions").is_empty());
}

#[test]
fn auto_record_off_starts_no_watcher_and_records_immediately() {
    // With `auto_record` false `GamesSection::start` returns `None`: it starts nothing, not even a thread.
    let (sink, _changes) = mpsc::channel();
    assert!(
        GamesSection::default()
            .start(sink.clone())
            .expect("starting nothing cannot fail")
            .is_none(),
        "auto_record = false must start nothing"
    );
    let off = GamesSection {
        watch: vec![WatchedGame::by_process("Dota 2", "dota2.exe")],
        ..GamesSection::default()
    };
    assert!(
        off.start(sink).expect("still nothing").is_none(),
        "nor with a watch list"
    );

    // And the recorder with default options is recording, not armed: no channel, receiver or supervisor thread exists.
    let (_tmp, app_data_dir) = application_data_dir("games-off");
    let recorder = Recorder::start(stub_config(&app_data_dir)).expect("the recorder starts");
    let status = recorder.status();
    assert!(
        status.running,
        "a recorder with nothing watched records now: {status:?}"
    );
    assert!(!status.watching_games, "and has no watcher: {status:?}");
    assert!(!recorder.is_armed());
    assert_eq!(status.game, None);
    recorder.stop().expect("stop must succeed");
}

#[test]
fn a_watched_game_starting_and_stopping_drives_a_full_session_and_the_retention_pass() {
    let (_tmp, app_data_dir) = application_data_dir("games-on");
    let mut cfg = stub_config(&app_data_dir);
    cfg.encode.fps = 10;
    // A session-start rule of one day: the old finished session must be evicted, the one just written must survive.
    cfg.storage.sessions.max_age_days = 1;
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();
    let sessions_dir = cfg.sessions_dir();
    let _ = &bin;
    std::fs::create_dir_all(&sessions_dir).expect("the sessions area");

    // The old session, with files on disk as a real one leaves them.
    let old_dir = sessions_dir.join("session-1");
    std::fs::create_dir_all(&old_dir).expect("the old session directory");
    std::fs::write(old_dir.join("seg-000000.mp4"), vec![0u8; 512]).expect("an old segment");
    let old_file = sessions_dir.join("session-1.mp4");
    std::fs::write(&old_file, vec![0u8; 512]).expect("an old session file");
    let two_days_ms = 2 * 24 * 60 * 60 * 1_000i64;
    {
        let store = open_clip_index(&db_path).expect("the index opens");
        let id = store
            .start_session(
                Some("Dota 2"),
                SESSION_MODE_SESSION,
                now_ms() - two_days_ms,
                &old_dir.display().to_string(),
                0,
            )
            .expect("opening the old session");
        store
            .end_session(
                id,
                now_ms() - two_days_ms + 1_000,
                Some(&old_file.display().to_string()),
                512,
                0,
            )
            .expect("ending the old session");
    }

    // Armed: nothing is captured until a game starts; the injected presence channel is the only way in.
    let (presence, changes) = mpsc::channel();
    let game = WatchedGame::by_process("Dota 2", "dota2.exe");
    let recorder = Recorder::start_inner(
        cfg,
        options(RecordingMode::FullSession, false),
        // The supervisor builds the shipping probe itself; this panicking closure proves it is not called.
        &|_cfg: &EncodeConfig| {
            panic!("the injected measurement belongs to a recording `start` begins itself")
        },
        Some((changes, None)),
    )
    .expect("the armed recorder starts");

    let armed = recorder.status();
    assert!(
        !armed.running,
        "nothing is recorded before a game starts: {armed:?}"
    );
    assert!(
        armed.watching_games && recorder.is_armed(),
        "but a watcher is running"
    );
    assert_eq!(armed.game, None);
    assert_eq!(armed.error, None, "being armed is not an error");

    // The game starts: the recording begins, with the game's name in the session row.
    presence
        .send(PresenceChange::Started(game.clone()))
        .expect("telling the recorder");
    let recording = wait_for(
        &recorder,
        Duration::from_secs(40),
        "the game's recording to start",
        |s| s.running && s.span_ms >= 3_000,
    );
    assert_eq!(recording.game.as_deref(), Some("Dota 2"), "{recording:?}");
    let store = open_clip_index(&db_path).expect("the index opens");
    let running: Vec<_> = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .filter(|row| row.ended_at_ms.is_none())
        .collect();
    assert_eq!(running.len(), 1, "one running session: {running:?}");
    assert_eq!(
        running[0].game.as_deref(),
        Some("Dota 2"),
        "carrying the detected title"
    );
    drop(store);

    // The game stops: recording closed, session file written, retention pass run (removing the two-day-old session only).
    presence
        .send(PresenceChange::Stopped(game.clone()))
        .expect("telling the recorder");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the recording to stop",
        |s| !s.running,
    );
    let stopped = recorder.status();
    assert_eq!(
        stopped.game, None,
        "no game is being recorded any more: {stopped:?}"
    );
    assert!(stopped.watching_games, "but the watcher is still armed");

    let store = open_clip_index(&db_path).expect("the index opens");
    let sessions = store.list_sessions().expect("listing sessions");
    assert_eq!(
        sessions.len(),
        1,
        "the old session was evicted, the new one kept: {sessions:?}"
    );
    let session = &sessions[0];
    assert_eq!(session.game.as_deref(), Some("Dota 2"));
    let file = PathBuf::from(session.final_path.clone().expect("the new session file"));
    assert!(
        file.is_file(),
        "the session the game produced is on disk: {file:?}"
    );
    assert_eq!(stream_counts(&file, &bin), (1, 1));
    assert!(
        !old_file.exists(),
        "the retention pass removed the old session's file"
    );
    assert!(!old_dir.exists(), "and its scratch directory");
    drop(store);

    // A second game starts and the application shuts down mid-session: the supervisor closes that
    // session exactly as the game stopping did.
    presence
        .send(PresenceChange::Started(game.clone()))
        .expect("telling the recorder");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the second game's recording",
        |s| s.running && s.span_ms >= 3_000,
    );
    recorder
        .stop()
        .expect("stopping the recorder stops the watcher and its recording too");
    assert!(!recorder.is_armed(), "and leaves nothing armed");
    assert!(!recorder.is_running());
    let store = open_clip_index(&db_path).expect("the index opens");
    let sessions = store.list_sessions().expect("listing sessions");
    let other = sessions
        .iter()
        .find(|row| row.id != session.id)
        .expect("the second game's session row: {sessions:?}");
    assert!(
        other.ended_at_ms.is_some(),
        "stopping the application closed it: {other:?}"
    );
    let file = PathBuf::from(other.final_path.clone().expect("with its session file"));
    assert!(file.is_file(), "{file:?} exists");
    assert_eq!(stream_counts(&file, &bin), (1, 1));
}

/// A start that fails must not take the recorder's index connection with it.
///
/// The store is the recorder's only connection to the index, and a recording takes it for the
/// length of the recording. A start that failed *after* taking it used to leave the recorder
/// with none: the next game would fail at once with "this recorder's store is already in a
/// recording", and one transient failure (a probe timeout, a locked database, a capture session
/// another application holds) would end auto-record for the life of the process.
///
/// So: the first game start fails, and the second one must record.
#[test]
fn a_start_that_fails_does_not_take_the_next_one_with_it() {
    let (_tmp, app_data_dir) = application_data_dir("failed-start");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    // The first start fails, after it has taken the store.
    opts.fail_starts = FailStarts::next(1);

    let (presence, changes) = mpsc::channel();
    let recorder = Recorder::start_inner(
        stub_config(&app_data_dir),
        opts,
        // The supervisor builds the shipping probe itself; this panicking closure proves it is
        // not called.
        &|_cfg: &EncodeConfig| {
            panic!("the injected measurement belongs to a recording `start` begins itself")
        },
        Some((changes, None)),
    )
    .expect("the armed recorder starts");

    let game = WatchedGame::by_process("Dota 2", "dota2.exe");
    presence
        .send(PresenceChange::Started(game.clone()))
        .expect("telling the recorder");

    // The first start fails, and says why.
    let failed = wait_for(
        &recorder,
        Duration::from_secs(30),
        "the failed start to be reported",
        |s| s.error.is_some(),
    );
    let why = failed.error.clone().unwrap_or_default();
    assert!(
        why.contains("the test made this start fail"),
        "the failure must be reported as it happened: {why}"
    );
    assert!(!failed.running, "and nothing is being recorded: {failed:?}");

    // The second game starts. It cannot record without the store, so a recording that begins
    // is the proof that the store came back.
    presence
        .send(PresenceChange::Started(game.clone()))
        .expect("telling the recorder");
    let recording = wait_for(
        &recorder,
        Duration::from_secs(40),
        "the second game's recording",
        |s| s.running,
    );
    assert!(
        recording.running,
        "the second game is recorded: {recording:?}"
    );
    assert_eq!(recording.game.as_deref(), Some("Dota 2"), "{recording:?}");

    recorder.stop().expect("stop must succeed");
}

/// A recording thread that dies must not end auto-record, and must not lose the index with it.
///
/// The engine owns the recorder's one index connection, so a panic in its loop took the connection
/// down too: every later game failed with "this recorder's store is already in a recording". The
/// supervisor also acted only on presence changes, so it never noticed the dead thread, said the
/// session "is left for the next start to recover" although nothing would, and left the game
/// unrecorded for as long as it ran.
///
/// So: the first recording thread panics as it starts, while the game is still running. The status
/// must say so, the dead recording's row must be closed in this process, and the game must be
/// recorded again by itself.
#[test]
fn a_recording_thread_that_panics_is_replaced_while_the_game_runs() {
    let (_tmp, app_data_dir) = application_data_dir("engine-panic");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let mut opts = options(RecordingMode::FullSession, false);
    opts.panic_engines = PanicEngines::next(1);

    let (presence, changes) = mpsc::channel();
    let recorder = Recorder::start_inner(
        cfg,
        opts,
        &|_cfg: &EncodeConfig| {
            panic!("the injected measurement belongs to a recording `start` begins itself")
        },
        Some((changes, None)),
    )
    .expect("the armed recorder starts");

    let game = WatchedGame::by_process("Dota 2", "dota2.exe");
    presence
        .send(PresenceChange::Started(game))
        .expect("telling the recorder");

    // The panic is reported as what it was, not as a watcher that is merely idle.
    let failed = wait_for(
        &recorder,
        Duration::from_secs(30),
        "the panic to be reported",
        |s| s.error.as_deref().is_some_and(|e| e.contains("panicked")),
    );
    assert!(!failed.running, "nothing is recording: {failed:?}");

    // The game never stopped, and the recording comes back by itself, which it cannot do without the index.
    let again = wait_for(
        &recorder,
        Duration::from_secs(40),
        "the game to be recorded again",
        |s| s.running && s.span_ms >= 1_000,
    );
    assert_eq!(again.game.as_deref(), Some("Dota 2"), "{again:?}");
    assert_eq!(again.error, None, "the old failure describes nothing now");

    // The dead recording's row was closed here, so exactly one session is still running.
    let store = open_clip_index(&db_path).expect("the index opens");
    let rows = store.list_sessions().expect("listing sessions");
    let running = rows.iter().filter(|r| r.ended_at_ms.is_none()).count();
    assert_eq!(
        running, 1,
        "the panicked recording's row must have been closed: {rows:?}"
    );
    assert_eq!(
        rows.len(),
        2,
        "the dead recording and the new one: {rows:?}"
    );
    drop(store);

    recorder.stop().expect("stop must succeed");
}

/// A second `stop` must not return while the first is still closing the recording.
///
/// `stop` is documented as safe from a shared `Arc<Recorder>`, "a concurrent stop waits for the
/// shutdown rather than returning early". The games-mode supervisor handle was taken under its lock
/// and joined outside it, so the second caller found `None` and went straight on: it returned while
/// the supervisor was still closing the session, and a caller that then exits the process (or reads
/// the session file) cut the first stop off.
#[test]
fn a_second_stop_waits_for_the_first_to_finish_closing_the_recording() {
    let (_tmp, app_data_dir) = application_data_dir("concurrent-stop");
    let (presence, changes) = mpsc::channel();
    let recorder = Arc::new(
        Recorder::start_inner(
            stub_config(&app_data_dir),
            options(RecordingMode::FullSession, false),
            &|_cfg: &EncodeConfig| panic!("the supervisor measures for itself"),
            Some((changes, None)),
        )
        .expect("the armed recorder starts"),
    );
    presence
        .send(PresenceChange::Started(WatchedGame::by_process(
            "Dota 2",
            "dota2.exe",
        )))
        .expect("telling the recorder");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the recording to start",
        |s| s.running && s.span_ms >= 2_000,
    );

    let first = {
        let recorder = Arc::clone(&recorder);
        std::thread::spawn(move || recorder.stop())
    };
    // The first stop is inside the shutdown: the supervisor notices its flag within a tick and then
    // spends real time closing the session.
    std::thread::sleep(Duration::from_millis(20));
    recorder.stop().expect("the second stop");
    assert!(
        !recorder.is_armed() && !recorder.is_running(),
        "the second stop returned while the recording was still being closed: {:?}",
        recorder.status()
    );
    first
        .join()
        .expect("the first stop did not panic")
        .expect("the first stop");
}

/// A recording thread that panicked must not leave the recorder looking as if it could still be told things.
///
/// `stop` returned from the join's error arm before it cleared the command channel, so after the
/// failure the recorder still held a sender whose receiver was gone: a later clip request failed
/// with a message about a recorder that "stopped before it could" answer, not about one that is not
/// running.
#[test]
fn a_stop_after_the_recording_thread_panicked_still_clears_the_commands() {
    let (_tmp, app_data_dir) = application_data_dir("stop-after-panic");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.panic_engines = PanicEngines::next(1);
    let recorder = Recorder::start_with_options(stub_config(&app_data_dir), opts)
        .expect("the recorder starts");

    let err = recorder.stop().expect_err("the recording thread panicked");
    assert!(format!("{err:#}").contains("panicked"), "{err:#}");
    assert!(
        recorder
            .commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none(),
        "the dead recording's command channel must be cleared"
    );
}

/// The retention pass must not run on the pump thread.
///
/// It deletes files and walks directories, and on a slow disk that is time the capture is not
/// pumped: the encoder's queue fills and the recording gets a frozen stretch. It runs on a worker
/// of its own with its own index connection.
#[test]
fn the_retention_pass_does_not_run_on_the_pump_thread() {
    let (_tmp, app_data_dir) = application_data_dir("retention-thread");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.retention_probe.interval = Duration::from_millis(300);
    let probe = opts.retention_probe.ran_on.clone();
    let recorder = Recorder::start_with_options(stub_config(&app_data_dir), opts)
        .expect("the recorder starts");

    let deadline = Instant::now() + Duration::from_secs(30);
    while probe.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no retention pass ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    recorder.stop().expect("stop must succeed");
    let ran_on = probe.lock().unwrap().clone();
    assert!(
        ran_on.iter().all(|name| name == "replayed-retention"),
        "the pass ran on the pump thread: {ran_on:?}"
    );
}

/// A clip's splice must not punch a hole in the recording.
///
/// The splice is ffmpeg's, and ffmpeg takes as long as the clip is long. Run on the pump thread
/// it stops the pump: the capture backend is not drained, the encoder's queue fills, and because
/// the media clock is the wall clock the frames that follow carry a timestamp a whole splice
/// later — a frozen picture in the recording, at every clip, for as long as the splice took.
///
/// The splice is made slow on purpose (a wrapper that waits before the in-memory remux), so
/// the hole it would leave is seconds long and the next clip's own frames show it.
#[cfg(unix)]
#[test]
fn a_clip_does_not_punch_a_hole_in_the_recording() {
    /// How long the wrapped splice takes. Long enough to be unmistakable in the frame
    /// timestamps, short enough to keep the test quick.
    const SPLICE_DELAY: Duration = Duration::from_secs(2);
    /// The longest gap between two frames of a clip that is whole.
    ///
    /// The stub records at 10fps, so the frames are 100ms apart; half a second is five frames,
    /// which is jitter no loaded runner produces and a two-second hole is twenty.
    const WHOLE_CLIP_GAP_MS: u64 = 500;

    let (_tmp, app_data_dir) = application_data_dir("splice-hole");
    let mut cfg = stub_config(&app_data_dir);
    // A pre-roll long enough to cover the hole the slow splice would leave.
    cfg.buffer.pre_seconds = 6;
    let bin = cfg.bin.clone();
    // The recorder's ffmpeg is a wrapper that waits before the in-memory remux.
    cfg.bin.ffmpeg = slow_splice_wrapper(&bin.ffmpeg, SPLICE_DELAY);

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the window to be buffered",
        |s| s.span_ms >= 6_000,
    );

    // The first clip: its splice is the slow one.
    let started = Instant::now();
    let first = recorder.clip_now().expect("the first clip");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= SPLICE_DELAY,
        "the wrapper must have made the splice slow, or the test proves nothing: {elapsed:?}"
    );

    // The footage the splice interrupted is inside a fragment, and a fragment is only
    // ingested once the next one starts: wait for the ring to hold it.
    wait_for(
        &recorder,
        Duration::from_secs(30),
        "the interrupted footage to be indexed",
        |s| s.span_ms >= 10_000,
    );
    // The second clip, whose window covers the moment the first splice ran.
    let second = recorder.clip_now().expect("the second clip");
    recorder.stop().expect("stop must succeed");
    assert!(first.metadata.path.is_file(), "the first clip is on disk");
    assert!(second.metadata.path.is_file(), "the second clip is on disk");

    // The frames of the second clip, and the gaps between them. A recording that kept running
    // under the splice has no gap at all; one that froze for the splice has a hole exactly
    // that long.
    let gaps = frame_gaps(&second.metadata.path, &bin);
    assert!(
        gaps.len() > 10,
        "the clip must carry a run of frames: {} gaps",
        gaps.len()
    );
    let biggest = gaps.iter().copied().max().unwrap_or(0);
    assert!(
        biggest < WHOLE_CLIP_GAP_MS,
        "the recording froze for {biggest}ms while the clip was spliced, so the footage has a \
         hole in it: {gaps:?}"
    );
}

/// An ffmpeg wrapper that waits before the in-memory remux, and passes everything else through.
///
/// The recorder's ffmpeg is the encoder's *and* the splicer's, so the wrapper has to tell them
/// apart: the remux reads a fragmented stream from a pipe and copies it (`-i pipe:0 -c copy`),
/// while the encoder reads raw pixels from a pipe (`-f rawvideo -i pipe:0`) and encodes them.
/// Only the first is the splice this test wants to slow down.
#[cfg(unix)]
fn slow_splice_wrapper(real_ffmpeg: &Path, delay: Duration) -> PathBuf {
    let dir = tempfile::tempdir().expect("a temp dir for the wrapper");
    let path = dir.path().join("slow-splice-ffmpeg");
    let script = format!(
        "#!/bin/sh\n\
         # Waits before the in-memory remux, so a test can prove the recorder keeps pumping\n\
         # while a clip is spliced. Every other invocation goes straight through.\n\
         is_pipe=\n\
         is_copy=\n\
         is_raw=\n\
         for arg in \"$@\"; do\n\
         \x20 [ \"$arg\" = \"pipe:0\" ] && is_pipe=1\n\
         \x20 [ \"$arg\" = \"copy\" ] && is_copy=1\n\
         \x20 [ \"$arg\" = \"rawvideo\" ] && is_raw=1\n\
         done\n\
         if [ -n \"$is_pipe\" ] && [ -n \"$is_copy\" ] && [ -z \"$is_raw\" ]; then\n\
         \x20 sleep {seconds}\n\
         fi\n\
         exec \"{ffmpeg}\" \"$@\"\n",
        seconds = delay.as_secs(),
        ffmpeg = real_ffmpeg.display(),
    );
    std::fs::write(&path, script).expect("writing the wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("making the wrapper executable");
    }
    // The directory has to outlive the path: the recorder runs the wrapper for the whole test.
    std::mem::forget(dir);
    path
}

/// A splice that fails must not leave its half-written file in the clips directory.
///
/// ffmpeg opens the output before it has copied anything, so a splice that dies part-way (a full
/// disk, a killed child, a stream it cannot read) leaves a truncated `clip-*.mp4`. The clip's index
/// row is never written, so nothing would ever list or retain it: a file the clips cap counts as an
/// orphan and the user finds as a clip that will not play.
#[cfg(unix)]
#[test]
fn a_failed_splice_leaves_no_partial_clip() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let wrapper = dir.path().join("failing-ffmpeg");
    // Writes something to the output (the last argument), then fails.
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nfor arg in \"$@\"; do out=\"$arg\"; done\necho partial > \"$out\"\nexit 1\n",
    )
    .expect("writing the wrapper");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("making the wrapper executable");
    }
    let mut bin = FfmpegBinaries::discover(None).expect("ffmpeg on PATH");
    bin.ffmpeg = wrapper;
    let out = dir.path().join("clip-1.mp4");

    let result = splice_clip(
        bin,
        ClipSource::Stream {
            bytes: vec![0; 64],
            audio_tracks: 0,
            start_ms: 0,
            duration_ms: 1_000,
        },
        out.clone(),
        "libx264".to_string(),
        Vec::new(),
        None,
        0,
    );
    assert!(result.is_err(), "the splice must fail: {result:?}");
    assert!(
        !out.exists(),
        "the half-written clip must be removed, not left unindexed in the clips directory"
    );
}

/// The gaps between the video frames of a file, in ms.
///
/// The frames' own timestamps, which is the media timeline: a recording that stopped for a
/// while has a gap here, however seamless the picture looks.
// Only the unix-only splice test calls this, so on Windows it would be dead code.
#[cfg(unix)]
fn frame_gaps(path: &Path, bin: &FfmpegBinaries) -> Vec<u64> {
    let output = std::process::Command::new(&bin.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("running ffprobe");
    let text = String::from_utf8_lossy(&output.stdout);
    let times: Vec<f64> = text
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .collect();
    times
        .windows(2)
        .map(|pair| ((pair[1] - pair[0]) * 1000.0).round() as u64)
        .collect()
}

/// A burst's events are marked at the moment they happened, not when the burst was cut.
///
/// A burst is coalesced before its clip is cut — up to 60% of the pre-roll, which is 18
/// seconds at the default `pre_seconds`. The marker and the chapter must name the moment the
/// event describes, not the moment the request was made: with the request's instant, a kill at
/// the start of a burst is stamped at the end of it, and the chapter lands seconds away from
/// the play it names.
#[test]
fn a_burst_event_is_marked_at_the_moment_it_happened() {
    use crate::event_driver::{act_on_burst, Coalesce, Policy};

    /// How long the burst waits before it settles. Long enough that the two instants — the
    /// arrival and the cut — are far apart on any clock.
    const BURST_WAIT: Duration = Duration::from_secs(3);
    /// How far the mark may sit from the arrival: the media clock moves a fragment at a time
    /// (about a second), so the two samples of it cannot be closer than that.
    const ARRIVAL_SLACK_MS: i64 = 1_500;

    let (_tmp, app_data_dir) = application_data_dir("burst-marker");
    let mut cfg = stub_config(&app_data_dir);
    // A pre-roll long enough to hold the event the burst waited for.
    cfg.buffer.pre_seconds = 5;
    let db_path = cfg.db_path();
    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the window to be buffered",
        |s| s.span_ms >= (5 + POST_SECONDS) * 1000,
    );

    // The event happens now. The burst waits for the rest of it, and the clip is cut at the
    // end of that wait — three seconds later.
    let arrived_at = recorder.media_now_ms();
    let (_tx, rx) = mpsc::channel::<GameEvent>();
    let kill = GameEvent::bare(replayed_events::Source::Lol, EventKind::Kill);
    let policy = Policy {
        coalesce: Coalesce {
            quiet: BURST_WAIT,
            max: BURST_WAIT * 3,
        },
        muted: Vec::new(),
        lol: ClipWindow::default(),
        gsi: ClipWindow::default(),
    };
    let clip = act_on_burst(&recorder, kill, &rx, &policy)
        .expect("the burst is acted on")
        .expect("a highlight takes a clip");
    recorder.stop().expect("stop must succeed");

    // The marker the burst left: the moment the event arrived.
    let store = open_clip_index(&db_path).expect("the index opens");
    let events = store.list_events().expect("the events");
    let marker = events
        .iter()
        .find(|event| event.kind == "kill")
        .expect("the kill's row");
    let drift = marker.at_ms as i64 - arrived_at as i64;
    assert!(
        drift.abs() <= ARRIVAL_SLACK_MS,
        "the marker must be stamped with the moment the event arrived ({arrived_at}ms), not the \
         moment the burst was cut: the row says {}ms",
        marker.at_ms
    );

    // And the chapter in the clip: the moment inside the footage, which is the arrival and not
    // the request. The request is the end of the pre-roll, three seconds later.
    let chapters = chapter_starts(&clip.metadata.path, &recorder_bin(&app_data_dir));
    let kill_at = chapters
        .iter()
        .find(|(_, title)| title == "Kill")
        .map(|(at, _)| *at)
        .expect("the kill is a chapter of the clip it asked for");
    assert!(
        kill_at < 3_500,
        "the chapter must be where the event happened, not at the end of the pre-roll: {chapters:?}"
    );
}

/// An event that arrives while the driver is busy with a clip is marked when it arrived.
///
/// The driver is one thread, and cutting a clip holds it for the post-roll wait and the splice.
/// Events were stamped with the media position only when the thread got back to the channel, so a
/// kill two seconds into a post-roll was stamped at the end of it: the marker and the chapter named a
/// moment seconds after the play, and the clip it started could miss the kill altogether.
#[test]
fn an_event_that_arrives_during_a_clip_is_marked_when_it_arrived() {
    use crate::event_driver::{drive, Coalesce, EventSources, Policy};

    const ARRIVAL_SLACK_MS: i64 = 1_500;

    let (_tmp, app_data_dir) = application_data_dir("queued-event");
    let mut cfg = stub_config(&app_data_dir);
    cfg.buffer.pre_seconds = 5;
    // A post-roll long enough that the driver is held for well over the slack.
    cfg.buffer.post_seconds = 4;
    let db_path = cfg.db_path();
    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the window to be buffered",
        |s| s.span_ms >= 9_000,
    );

    let (tx, events) = mpsc::channel::<GameEvent>();
    let sources = EventSources {
        events,
        _lol: None,
        _gsi: None,
        reports: Vec::new(),
    };
    let policy = Policy {
        coalesce: Coalesce {
            quiet: Duration::from_millis(200),
            max: Duration::from_millis(200),
        },
        muted: Vec::new(),
        lol: ClipWindow::default(),
        gsi: ClipWindow::default(),
    };
    let stop = AtomicBool::new(false);
    let kill = || GameEvent::bare(replayed_events::Source::Lol, EventKind::Kill);

    let arrived_second = std::thread::scope(|scope| {
        let driver = scope.spawn(|| {
            let on_clip: crate::event_driver::ClipHook = Box::new(|_| {});
            drive(&recorder, sources, &stop, &policy, &on_clip)
        });
        tx.send(kill()).expect("the first kill");
        // The driver is now in the first clip's post-roll; the second kill waits in the channel.
        std::thread::sleep(Duration::from_millis(1_000));
        let arrived = recorder.media_now_ms();
        tx.send(kill()).expect("the second kill");
        // Both clips are cut: the second after the first has been answered.
        wait_for(
            &recorder,
            Duration::from_secs(60),
            "both clips to be taken",
            |s| s.clips >= 2,
        );
        stop.store(true, Ordering::Release);
        driver.join().expect("the driver did not panic");
        arrived
    });
    recorder.stop().expect("stop must succeed");

    let store = open_clip_index(&db_path).expect("the index opens");
    let mut kills: Vec<i64> = store
        .list_events()
        .expect("the events")
        .iter()
        .filter(|event| event.kind == "kill")
        .map(|event| event.at_ms as i64)
        .collect();
    kills.sort_unstable();
    assert_eq!(kills.len(), 2, "two kills: {kills:?}");
    let drift = kills[1] - arrived_second as i64;
    assert!(
        drift.abs() <= ARRIVAL_SLACK_MS,
        "the second kill must be stamped when it arrived ({arrived_second}ms), not when the driver \
         got back to it: the row says {}ms",
        kills[1]
    );
}

/// A game's own, shorter `pre_seconds` must not start its clip after the event.
///
/// The clip's window was anchored where the request was *handled*, after the burst had been
/// waited out. With a game window shorter than that wait (3 seconds of burst against a 1 second
/// `pre_seconds`) the clip began two seconds after the kill it was taken for: the event was not
/// in the clip, and its chapter was not either.
#[test]
fn a_game_pre_roll_shorter_than_the_burst_wait_still_covers_the_event() {
    use crate::event_driver::{act_on_burst, Coalesce, Policy};

    const BURST_WAIT: Duration = Duration::from_secs(3);

    let (_tmp, app_data_dir) = application_data_dir("burst-short-pre");
    let mut cfg = stub_config(&app_data_dir);
    // The buffer reaches back further than the game's own window asks for.
    cfg.buffer.pre_seconds = 8;
    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the window to be buffered",
        |s| s.span_ms >= (8 + POST_SECONDS) * 1000,
    );

    let (_tx, rx) = mpsc::channel::<GameEvent>();
    let kill = GameEvent::bare(replayed_events::Source::Lol, EventKind::Kill);
    let policy = Policy {
        coalesce: Coalesce {
            quiet: BURST_WAIT,
            max: BURST_WAIT * 3,
        },
        muted: Vec::new(),
        // One second before the event, one after: shorter than the three seconds the burst waits.
        lol: ClipWindow::new(1, 1),
        gsi: ClipWindow::default(),
    };
    let clip = act_on_burst(&recorder, kill, &rx, &policy)
        .expect("the burst is acted on")
        .expect("a highlight takes a clip");
    recorder.stop().expect("stop must succeed");

    let chapters = chapter_starts(&clip.metadata.path, &recorder_bin(&app_data_dir));
    let kill_at = chapters
        .iter()
        .find(|(_, title)| title == "Kill")
        .map(|(at, _)| *at)
        .unwrap_or_else(|| panic!("the kill the clip was taken for is not in it: {chapters:?}"));
    // About one second of pre-roll before the kill (a segment of slack either way).
    assert!(
        (0..=2_500).contains(&(kill_at as i64)),
        "the kill must sit about one second into the clip: {chapters:?}"
    );
}

/// A `stop` must not wait out a clip's post-roll.
///
/// The post-roll wait ran inside the command handler, on the loop thread, so a `Stop` queued behind
/// it was not read until the whole wait (the post-roll plus the segment lag plus a margin) was over.
/// The wait now happens in the loop's own passes, and a stop ends it: the clip is cut from the
/// footage there is.
#[test]
fn a_stop_does_not_wait_for_a_clips_post_roll() {
    let (_tmp, app_data_dir) = application_data_dir("stop-in-post-roll");
    let mut cfg = stub_config(&app_data_dir);
    cfg.buffer.post_seconds = 8;
    let recorder = Arc::new(Recorder::start(cfg).expect("the recorder starts"));
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the window to be buffered",
        |s| s.span_ms >= 3_000,
    );

    let asker = {
        let recorder = Arc::clone(&recorder);
        std::thread::spawn(move || recorder.clip_now())
    };
    // The command is read and the post-roll wait has begun.
    std::thread::sleep(Duration::from_millis(800));
    let started = Instant::now();
    recorder.stop().expect("stop must succeed");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the stop waited for the post-roll: {took:?}"
    );
    let clip = asker
        .join()
        .expect("the asker did not panic")
        .expect("the clip is cut from the footage there is");
    assert!(clip.metadata.path.is_file(), "{:?}", clip.metadata.path);
}

/// The post-roll wait must survive a ledger whose span lags the wall clock by a whole segment.
///
/// A ledger trusts a segment only once a strictly later one exists (ffmpeg appends to the
/// newest segment), so the span it reports lags the wall clock by up to one segment, and the
/// post-roll cannot arrive before that. A budget of `post_ms + margin` could not be met with a
/// long segment: every clip failed with "timed out waiting for post-roll" and the clip the
/// user asked for was lost.
#[test]
fn a_long_segment_does_not_make_the_post_roll_wait_time_out() {
    const SEGMENT: u64 = 10;

    let (_tmp, app_data_dir) = application_data_dir("long-segment");
    let mut cfg = stub_config(&app_data_dir);
    // A segment ten seconds long: the ledger's span moves in ten-second steps.
    cfg.buffer.segment_time = SEGMENT;
    cfg.buffer.post_seconds = 2;
    // A full session, whose ledger is the one that trusts a segment only once a later one
    // exists (the file ledger's rule: ffmpeg appends to the newest segment).
    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");

    // The first segment is trusted only once a second one exists, ten seconds later.
    wait_for(
        &recorder,
        Duration::from_secs(60),
        "the first segment to be trusted",
        |s| s.span_ms >= 9_000,
    );

    // And the post-roll: the span has to reach the trigger plus the post-roll, which takes
    // another whole segment. A budget of `post + margin` would time out here.
    let clip = recorder
        .clip_now()
        .expect("the post-roll must arrive inside the budget");
    assert!(clip.metadata.path.is_file(), "the clip is on disk");
    recorder.stop().expect("stop must succeed");
}

#[test]
fn a_replay_buffer_can_also_save_the_whole_session_off_the_same_encode() {
    let (_tmp, app_data_dir) = application_data_dir("buffer-plus-session");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.save_session = true;

    let recorder = Recorder::start_with_options(cfg, opts).expect("the recorder starts");
    let wanted_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "the window to be buffered",
        |s| s.span_ms >= wanted_ms + 2_000,
    );

    // The session file grows while recording, in a directory of its own.
    let (dirs, files) = sessions_on_disk(&app_data_dir);
    assert_eq!(dirs.len(), 1, "one session directory: {dirs:?}");
    assert!(files.is_empty(), "no finished file until stop: {files:?}");
    assert!(
        dirs[0]
            .join(crate::memory_ring::SESSION_PART)
            .metadata()
            .unwrap()
            .len()
            > 0,
        "the part file holds footage"
    );

    // And the buffer still serves clips.
    let clip = recorder.clip_now().expect("a clip out of the ring");
    assert!(clip.metadata.path.is_file());
    recorder.stop().expect("stop must succeed");

    let row = wait_for_finished_session(&bin, &db_path);
    assert_eq!(row.mode, SESSION_MODE_SESSION);
    let session = PathBuf::from(
        row.final_path
            .clone()
            .expect("the row names the session file"),
    );
    assert!(session.is_file(), "{session:?} exists");
    assert_eq!(stream_counts(&session, &bin), (1, 1));
    let info = replayed_media::probe::MediaInfo::probe(&bin, &session).expect("probing it");
    assert!(
        info.duration_ms >= wanted_ms,
        "the session file covers the whole recording, not the buffer's window: {}ms",
        info.duration_ms
    );
    assert!(
        info.duration_ms > clip.metadata.duration_ms,
        "and is longer than the clip"
    );
    let (dirs, files) = sessions_on_disk(&app_data_dir);
    assert!(
        dirs.is_empty(),
        "the part file's directory is removed: {dirs:?}"
    );
    assert_eq!(files, vec![session]);
}

#[test]
fn a_session_saved_off_the_buffer_is_recovered_after_a_crash_cut_it_short() {
    let (_tmp, app_data_dir) = application_data_dir("buffer-session-crash");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();
    let sessions_dir = cfg.sessions_dir();
    let session_dir = sessions_dir.join("session-1700000000");
    std::fs::create_dir_all(&session_dir).expect("the crashed session's directory");

    // What the encoder streams: fragmented MP4. Cut mid-fragment, as a killed process leaves it.
    let part = session_dir.join(crate::memory_ring::SESSION_PART);
    let made = std::process::Command::new(&bin.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc=size=64x48:rate=10:duration=3")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
        .args(["-c:v", "libx264", "-g", "10", "-c:a", "aac", "-f", "mp4"])
        .args(["-movflags", "frag_keyframe+empty_moov+default_base_moof"])
        .arg(&part)
        .status()
        .expect("running ffmpeg");
    assert!(made.success());
    let whole = std::fs::metadata(&part).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
    file.set_len(whole - 200).unwrap();
    drop(file);

    let id = {
        let store = open_clip_index(&db_path).expect("the index opens");
        store
            .start_session(
                None,
                SESSION_MODE_SESSION,
                1_700_000_000_000,
                &session_dir.display().to_string(),
                0,
            )
            .expect("the row the crash left behind")
    };

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    recorder.stop().expect("stop must succeed");

    let store = open_clip_index(&db_path).expect("the index opens");
    let row = store.get_session(id).unwrap().expect("the row exists");
    assert!(row.ended_at_ms.is_some(), "closed: {row:?}");
    let out = PathBuf::from(row.final_path.expect("and names the recovered file"));
    assert!(out.is_file());
    assert_eq!(stream_counts(&out, &bin), (1, 1));
    assert!(
        row.duration_ms >= 1_000,
        "most of it survived: {}ms",
        row.duration_ms
    );
    assert!(!session_dir.exists(), "the part file's directory is gone");
}

#[test]
fn a_crashed_session_is_recovered_when_the_next_run_starts() {
    // A killed process leaves a running `sessions` row and segments with no session file; the next start must finish it.
    let (_tmp, app_data_dir) = application_data_dir("crash-recovery");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();
    let sessions_dir = cfg.sessions_dir();
    std::fs::create_dir_all(&sessions_dir).expect("the sessions area");
    let session_dir = sessions_dir.join("session-1700000000");
    let segments = layout_test_segments(&session_dir, 1).expect("the crashed session's segments");
    let bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    let id = {
        let store = open_clip_index(&db_path).expect("the index opens");
        let id = store
            .start_session(
                Some("League of Legends"),
                SESSION_MODE_SESSION,
                1_700_000_000_000,
                &session_dir.display().to_string(),
                0,
            )
            .expect("the row the crash left behind");
        store
            .set_session_size(id, bytes as i64)
            .expect("its last known size");
        id
    };

    // The next run is an ordinary buffer-mode recording: any start recovers what a previous one left.
    let recorder = Recorder::start(cfg).expect("the recorder starts");
    recorder.stop().expect("stop must succeed");

    let store = open_clip_index(&db_path).expect("the index opens");
    let row = store
        .get_session(id)
        .expect("reading the row")
        .expect("the row exists");
    assert!(
        row.ended_at_ms.is_some(),
        "the crashed session was closed: {row:?}"
    );
    let file = PathBuf::from(row.final_path.expect("and it names the recovered file"));
    assert!(file.is_file(), "{file:?} exists");
    assert_eq!(
        stream_counts(&file, &bin),
        (1, 1),
        "with the streams its segments carried"
    );
    assert_eq!(
        row.size_bytes as u64,
        std::fs::metadata(&file).unwrap().len()
    );
    assert!(!session_dir.exists(), "and the temporary segments are gone");
}

#[test]
fn dropping_a_recorder_finalises_and_closes_its_session() {
    // `Drop` is a `stop()`: a front-end that forgets to stop must not leave a running session row or orphan segments.
    let (_tmp, app_data_dir) = application_data_dir("drop-session");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let bin = cfg.bin.clone();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, false))
        .expect("the session recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "three seconds of session",
        |s| s.span_ms >= 3_000,
    );
    drop(recorder);

    let row = wait_for_finished_session(&bin, &db_path);
    assert!(
        row.ended_at_ms.is_some(),
        "the drop closed the session: {row:?}"
    );
    let file = PathBuf::from(row.final_path.clone().expect("the session file"));
    assert!(file.is_file(), "{file:?} exists");
    assert_eq!(stream_counts(&file, &bin), (1, 1));
    assert_eq!(
        row.size_bytes as u64,
        std::fs::metadata(&file).unwrap().len()
    );
    let (dirs, _files) = sessions_on_disk(&app_data_dir);
    assert!(
        dirs.is_empty(),
        "and the temporary segments went with it: {dirs:?}"
    );
}

/// Everything in `dir`, by name. An absent directory counts as empty: the strongest form of
/// "buffering wrote nothing" is that the directory was never created.
fn entries_in(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| {
                e.expect("a directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => panic!("reading {}: {err}", dir.display()),
    }
}

/// Replay-buffer mode writes nothing to disk while it is only buffering.
///
/// Unclipped footage must never reach the SSD. Clips and status would still be correct if a
/// per-second segment file came back, so this checks the disk directly. The clips directory is
/// checked too: saving a clip must leave exactly one file behind.
#[test]
fn buffering_writes_nothing_to_disk_and_only_a_saved_clip_does() {
    let (_tmp, app_data_dir) = application_data_dir("ram-buffer-no-writes");
    let cfg = stub_config(&app_data_dir);
    let scratch = app_data_dir.join("scratch");
    // Where a clip lands, derived the way the recorder derives it.
    let clips = app_data_dir.join("clips");

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    let wanted_ms = (PRE_SECONDS + POST_SECONDS) * 1000;
    let filled = wait_for(
        &recorder,
        Duration::from_secs(30),
        "the window to be buffered",
        |s| s.frames > 0 && s.span_ms >= wanted_ms,
    );
    assert!(filled.running, "the recorder must report itself as running");

    // Not a segment, not a partial file, not even the directory.
    let written = entries_in(&scratch);
    assert!(
        written.is_empty(),
        "buffering wrote {written:?} into {} — unclipped footage must stay in RAM",
        scratch.display()
    );
    assert!(
        entries_in(&clips).is_empty(),
        "no clip has been asked for yet, so the clips directory must be empty"
    );

    // The trigger is the one thing allowed to touch the disk.
    let clip = recorder
        .clip_now()
        .expect("the trigger must produce a clip");
    assert!(clip.metadata.path.is_file(), "the saved clip is on disk");
    assert!(
        entries_in(&scratch).is_empty(),
        "saving a clip left a scratch trail"
    );

    let saved = entries_in(&clips);
    assert_eq!(
        saved.len(),
        1,
        "the clips directory must hold exactly the one saved clip, found {saved:?}"
    );
}

/// An unset `zero_copy` must be survivable on any machine (no NVIDIA/AMD encoder, non-Windows):
/// a default that refuses to start is worse than a slow one. The hardware path is verified on
/// a machine that has the hardware.
#[test]
fn an_unset_zero_copy_survives_a_host_that_cannot_run_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());
    cfg.encode.zero_copy = None;

    match build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (64, 48)) {
        Ok(encode) => assert_eq!(
            encode.video,
            replayed_encoder::VideoInput::RawPixels,
            "with no hardware encoder the unset key resolves to the ordinary path"
        ),
        Err(err) => panic!("an unset zero_copy must not refuse to start: {err:#}"),
    }
}

/// Asking for it explicitly gets the honest answer, the reason it cannot run, not a quiet downgrade.
#[test]
fn an_explicit_zero_copy_is_refused_where_it_cannot_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());
    cfg.encode.zero_copy = Some(true);

    let refused = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (64, 48))
        .expect_err("an explicit zero_copy with no hardware encoder must refuse");
    assert!(
        refused.to_string().contains("hardware encoder"),
        "the refusal must name what is missing: {refused:#}"
    );
}

/// With the hardware present, the reason on a platform that cannot do it is the platform.
#[cfg(not(windows))]
#[test]
fn an_explicit_zero_copy_is_refused_off_windows() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());
    cfg.encode.zero_copy = Some(true);

    let refused = build_encode_config(
        &cfg,
        dir.path(),
        VideoCodec::H264,
        Some(replayed_encoder::Vendor::Nvenc),
        (64, 48),
    )
    .expect_err("an explicit zero_copy off Windows must refuse");
    assert!(
        refused.to_string().contains("Windows"),
        "the refusal must name the platform: {refused:#}"
    );
}

/// `[audio] bitrate_kbps` reaches the encoder's configuration, and a front-end that carries no
/// value (the desktop shell) gets the default 192 kbps rather than zero.
///
/// The non-default half makes the test bite: without the plumbing the encoder keeps the
/// constructor's own default (128 on the test software path).
#[test]
fn the_configured_audio_bitrate_reaches_the_encoder_and_unset_gets_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());

    cfg.audio_bitrate_kbps = Some(96);
    let encode = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect("a 96 kbps configuration");
    assert_eq!(
        encode.audio_bitrate_kbps, 96,
        "the configured value must reach the encoder"
    );

    cfg.audio_bitrate_kbps = None;
    let encode = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect("an unset value");
    assert_eq!(
        encode.audio_bitrate_kbps,
        replayed_encoder::DEFAULT_AUDIO_BITRATE_KBPS,
        "an unset value must be the pipeline's default"
    );
    assert_eq!(
        encode.audio_bitrate_kbps, 192,
        "which is 192 kbps, not zero and not 128"
    );
}

/// The rate-control mode and the preset reach the encoder's configuration.
///
/// The two are the last of the `[encode]` keys, and the ones a settings panel can write, so they
/// have to travel: a panel that could set them and an encoder that ignored them would be a lie.
#[test]
fn the_configured_rate_control_and_preset_reach_the_encoder() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());

    cfg.encode.rate_control = "vbr".to_string();
    cfg.encode.preset = "veryfast".to_string();
    let encode = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect("a vbr configuration");
    assert_eq!(
        encode.rate_control,
        replayed_encoder::RateControl::Vbr,
        "the configured mode must reach the encoder"
    );
    assert_eq!(encode.preset, "veryfast", "and so must the preset");

    // And a file that does not mention them records as it always did.
    cfg.encode.rate_control = "cbr".to_string();
    cfg.encode.preset = String::new();
    let encode = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect("the default configuration");
    assert_eq!(encode.rate_control, replayed_encoder::RateControl::Cbr);
    assert_eq!(encode.preset, "");
}

/// A mode that is not one of the three refuses the start, naming the key.
///
/// A recording at the encoder's own rate, while the file says something else, is the failure this
/// is about: the file would claim a rate the recording does not have.
#[test]
fn an_unknown_rate_control_refuses_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());
    cfg.encode.rate_control = "crf".to_string();

    let err = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect_err("an unknown mode must not be recorded as something else");
    assert!(
        err.to_string().contains("crf"),
        "the refusal names the value: {err:#}"
    );
}

/// `[audio] enabled` reaches the encoder's configuration, and a front-end that carries no value
/// records the game audio.
///
/// The non-default half makes the test bite: without the plumbing the encoder keeps the
/// constructor's `true`, and `false` is what `EncodeConfig::game_audio` must carry into the
/// argument list.
#[test]
fn the_configured_audio_opt_out_reaches_the_encoder_and_unset_records_audio() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = stub_config(dir.path());

    cfg.audio_enabled = Some(false);
    let encode = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect("a video-only configuration");
    assert!(
        !encode.game_audio,
        "the opt-out must reach the encoder, which is the thing that declares the input"
    );

    cfg.audio_enabled = None;
    let encode = build_encode_config(&cfg, dir.path(), VideoCodec::H264, None, (WIDTH, HEIGHT))
        .expect("an unset value");
    assert!(
        encode.game_audio,
        "an unset value must be the pipeline's default, which records the game audio"
    );
}

/// A microphone with the game audio off is refused rather than recorded.
///
/// Audio tracks are named by position and `replayed_media::edit::audio_titles` calls the first
/// `Game Audio`, so a one-track voice recording would be mislabelled. The refusal happens
/// before anything is opened, so no capture or encoder child is needed.
#[test]
fn a_microphone_with_the_game_audio_off_is_refused_at_start() {
    let (_tmp, app_data_dir) = application_data_dir("video-only-mic");
    let mut cfg = stub_config(&app_data_dir);
    cfg.audio_enabled = Some(false);

    let refused = Recorder::start_with_options(cfg, options(RecordingMode::FullSession, true))
        .err()
        .expect("a microphone without the game audio must be refused");
    let text = format!("{refused:#}");
    assert!(
        text.contains("[audio] enabled = false") && text.contains("[mic] enabled = true"),
        "the refusal must name both keys so the reader knows which to change: {text}"
    );
}

#[test]
fn a_clip_window_is_clamped_to_the_buffers_ceiling_and_defaults_to_the_configured_one() {
    // (default pre, default post, max pre, max post), all in ms.
    let resolve = |w: ClipWindow| w.resolve(2_000, 1_000, 4_000, 1_000);
    assert_eq!(
        resolve(ClipWindow::default()),
        (2_000, 1_000),
        "unset is the configured window"
    );
    assert_eq!(resolve(ClipWindow::new(1, 0)), (1_000, 0), "a short clip");
    assert_eq!(
        resolve(ClipWindow::new(4, 1)),
        (4_000, 1_000),
        "up to the ceiling is honoured"
    );
    assert_eq!(
        resolve(ClipWindow::new(60, 30)),
        (4_000, 1_000),
        "beyond it is clamped, not refused"
    );
    assert_eq!(
        resolve(ClipWindow {
            pre_seconds: Some(3),
            post_seconds: None
        }),
        (3_000, 1_000),
        "one half set leaves the other at the configured value"
    );
    assert_eq!(
        resolve(ClipWindow::new(0, 0)),
        (0, 1_000),
        "an empty window still holds a second"
    );
}

#[test]
fn a_short_and_a_long_clip_come_out_of_one_recording_at_their_own_lengths() {
    // The short/long hotkeys and per-game lengths all end at `clip_now_in`. One recording, two
    // clips: the long one reaches back further than the 2s default pre-roll (ceiling 4s), the short one takes only the post-roll.
    const LONG_PRE: u64 = 4;
    let (_tmp, app_data_dir) = application_data_dir("clip-window");
    let mut cfg = stub_config(&app_data_dir);
    cfg.buffer.max_pre_seconds = LONG_PRE;

    let recorder = Recorder::start(cfg).expect("the recorder starts");
    wait_for(
        &recorder,
        Duration::from_secs(40),
        "enough media for a long clip",
        |s| s.span_ms >= (LONG_PRE + POST_SECONDS + 1) * 1000,
    );

    let long = recorder
        .clip_now_in(ClipReason::Manual, ClipWindow::new(LONG_PRE, POST_SECONDS))
        .expect("the long clip");
    let short = recorder
        .clip_now_in(ClipReason::Manual, ClipWindow::new(0, POST_SECONDS))
        .expect("the short clip");
    recorder.stop().expect("stop must succeed");

    assert!(
        long.metadata.duration_ms >= LONG_PRE * 1000,
        "the long clip reaches back past the configured {PRE_SECONDS}s pre-roll: {}ms",
        long.metadata.duration_ms
    );
    assert!(
        short.metadata.duration_ms < long.metadata.duration_ms,
        "the short clip is shorter: {}ms vs {}ms",
        short.metadata.duration_ms,
        long.metadata.duration_ms
    );
    assert!(
        short.metadata.duration_ms < PRE_SECONDS * 1000 + POST_SECONDS * 1000,
        "and shorter than the default window: {}ms",
        short.metadata.duration_ms
    );
}

/// A frame is sized by its geometry, and a frame that is not the size its geometry says is
/// refused.
///
/// The raw video pipe is a flat byte stream with no framing, so a frame that claims 3840x2160
/// and carries 33,177,599 bytes is read as the start of the *next* frame's pixels: the damage
/// appears seconds later as a desync pointing at nothing. The size is therefore computed in
/// `usize` and saturating — a `u32` multiplication of three numbers this large wraps into a
/// *small* number, and a small number is the dangerous one, because a buffer sized for a
/// fraction of the frame compares equal to nothing and the frame is then read as if it were
/// whole.
#[test]
fn a_frame_is_sized_by_its_geometry_and_a_short_frame_is_refused() {
    // The 4K desktop this was reported on: 3840x2160 BGRA.
    let (width, height) = (3840u32, 2160u32);
    let bytes = frame_bytes(width, height);
    assert_eq!(
        bytes, 33_177_600,
        "a 4K BGRA frame is 33.2MB and the size must be exact"
    );
    assert_eq!(
        u64::from(width) * u64::from(height) * 4,
        bytes as u64,
        "the size is the geometry times four bytes a pixel"
    );
    assert!(
        frame_bytes(u32::MAX, u32::MAX) > u32::MAX as usize,
        "a geometry whose u32 product would wrap must saturate, not wrap into a small number \
         that sizes a buffer for a fraction of the frame"
    );

    // A frame one byte short of the frame the pipe was declared for.
    let short = Frame {
        data: vec![0u8; bytes - 1],
        pts: Duration::ZERO,
        width,
        height,
        format: PixelFormat::Bgra8,
        texture: None,
    };
    let err = guard_frame_size(&short, (width, height))
        .expect_err("a frame that is not the size its geometry says must be refused");
    assert!(
        err.to_string().contains("33177600"),
        "the refusal must name the size the frame should have had: {err}"
    );

    // And the frame that is the right size goes through, or the guard would refuse good footage.
    let honest = Frame {
        data: vec![0u8; bytes],
        pts: Duration::ZERO,
        width,
        height,
        format: PixelFormat::Bgra8,
        texture: None,
    };
    guard_frame_size(&honest, (width, height)).expect("an honest frame is admitted");
}

// ---------------------------------------------------------------------------------------
// The audio backends that do not come up
// ---------------------------------------------------------------------------------------

/// An audio backend that records the thread every one of its calls ran on.
///
/// The Windows backends take a COM apartment when they are opened and give it back when they
/// are stopped, and a COM apartment belongs to the thread that initialised it: an object used
/// from another thread is an access violation there, not an error the caller can handle. This
/// records what the recorder does with the threads it is given, so the rule can be checked
/// where the tests run rather than only on the Windows runner.
struct RecordsItsThreads {
    /// The calls, in order, with the thread each ran on.
    calls: Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>>,
    /// How many blocks have been handed over. One, so a pump that drains the backend sees it
    /// empty afterwards: a backend that always answers would keep the pump's drain loop
    /// running for ever.
    delivered: u32,
}

impl RecordsItsThreads {
    fn record(&self, what: &'static str) {
        self.calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((what, std::thread::current().id()));
    }
}

impl AudioBackend for RecordsItsThreads {
    fn start(&mut self) -> anyhow::Result<()> {
        self.record("start");
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        self.record("next_buffer");
        self.delivered += 1;
        Ok((self.delivered == 1).then(a_block))
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.record("stop");
        Ok(())
    }
}

impl Drop for RecordsItsThreads {
    fn drop(&mut self) {
        self.record("drop");
    }
}

/// Every audio backend runs on a thread of its own, and every call it takes stays on it.
///
/// A COM apartment belongs to the thread that initialised it, and the Windows audio backends
/// take theirs when they are opened and give them back when they are stopped. A backend that
/// is opened on one thread and read from another is an access violation on Windows, which is
/// what the Windows CI runner saw: the open ran on a thread of its own and the reads and the
/// release ran on the recorder's.
///
/// The microphone and the game audio are told apart here, because they are two backends and
/// each has an apartment of its own: they must not share a thread either.
#[test]
fn every_audio_backend_runs_on_a_thread_of_its_own() {
    let (_dir, app) = application_data_dir("audio-one-thread");
    let game_calls: Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let mic_calls: Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let mut opts = options(RecordingMode::ReplayBuffer, true);
    opts.audio_backends = opts
        .audio_backends
        .game_audio(Box::new(RecordsItsThreads {
            calls: Arc::clone(&game_calls),
            delivered: 0,
        }))
        .microphone(Box::new(RecordsItsThreads {
            calls: Arc::clone(&mic_calls),
            delivered: 0,
        }));
    let recorder = Recorder::start_with_options(stub_config(&app), opts)
        .expect("the recording starts: both backends open and deliver a block");
    let _ = recorder.stop();

    let game = game_calls.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mic = mic_calls.lock().unwrap_or_else(|p| p.into_inner()).clone();
    for (what, calls) in [("game audio", &game), ("microphone", &mic)] {
        let names: Vec<&str> = calls.iter().map(|(name, _)| *name).collect();
        let trace: Vec<String> = calls
            .iter()
            .map(|(name, id)| format!("{name} on {id:?}"))
            .collect();
        assert!(
            calls.iter().any(|(name, _)| *name == "start"),
            "the {what} backend was opened: {names:?}"
        );
        assert!(
            calls.iter().any(|(name, _)| *name == "next_buffer"),
            "the {what} backend was read from: {names:?}"
        );
        assert!(
            calls.iter().any(|(name, _)| *name == "stop"),
            "the {what} backend was stopped: {names:?}"
        );
        assert!(
            calls.windows(2).all(|pair| pair[0].1 == pair[1].1),
            "every call on the {what} backend ran on one thread: {trace:?}"
        );
    }
    // Neither backend lives on the recorder's own thread: that thread has no apartment of its
    // own, and it is not the one that opened them.
    let recorder_thread = std::thread::current().id();
    assert!(
        game.iter().all(|(_, id)| *id != recorder_thread),
        "the game audio backend never ran on the recorder's thread"
    );
    assert!(
        mic.iter().all(|(_, id)| *id != recorder_thread),
        "the microphone never ran on the recorder's thread"
    );
    // And the two backends do not share a thread: each apartment is the backend's own.
    assert_ne!(
        game[0].1, mic[0].1,
        "the game audio and the microphone run on threads of their own"
    );
}

/// An audio backend whose open never returns.
///
/// The shape of a platform call that waits for ever: the Windows audio service is a separate
/// process, and nothing in the API bounds the call into it.
struct NeverOpens;

impl AudioBackend for NeverOpens {
    fn start(&mut self) -> anyhow::Result<()> {
        // Far longer than the recording's own deadline, so the deadline is what ends the wait.
        std::thread::sleep(Duration::from_secs(30));
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        Ok(None)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An audio backend that opens and then says nothing.
///
/// The shape of an endpoint that is not really there: the open succeeds, and the stream
/// never carries a sample.
struct OpensAndDeliversNothing;

impl AudioBackend for OpensAndDeliversNothing {
    fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        Ok(None)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An audio backend that delivers two blocks, goes quiet, and then fails.
///
/// The shape of a device that goes away mid-run: the track started, and then the endpoint
/// did. The first block is read by the start's own check, the second by the first pump, and
/// the quiet call in between lets that pump finish, so the counters have something to say
/// when the next one fails.
struct FailsAfterTwoBlocks {
    delivered: u32,
}

impl AudioBackend for FailsAfterTwoBlocks {
    fn start(&mut self) -> anyhow::Result<()> {
        self.delivered = 0;
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        self.delivered += 1;
        match self.delivered {
            1 | 2 => Ok(Some(a_block())),
            3 => Ok(None),
            _ => Err(anyhow::anyhow!("the audio endpoint went away")),
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An audio backend that opens and then never answers.
///
/// The shape of a thread stuck inside the platform: the open returns, and the read does not.
struct OpensAndHangs;

impl AudioBackend for OpensAndHangs {
    fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        // Far longer than the recording's own deadline, so the deadline is what ends the wait.
        std::thread::sleep(Duration::from_secs(30));
        Ok(None)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// One 10ms block in the canonical format, which is what a backend hands over.
fn a_block() -> AudioBuffer {
    let format = AudioFormat::default();
    AudioBuffer {
        data: vec![0u8; format.bytes_per_10ms()],
        frames: format.sample_rate as usize / 100,
        pts: Duration::ZERO,
        format,
    }
}

/// A recording whose game-audio backend never comes up is refused, with the reason.
///
/// The failure this guards is the silent one: a run that captures video, produces no file
/// and says nothing about the audio side that was never there. The refusal has to come at
/// the start, because after it the run looks like a healthy one.
#[test]
fn a_game_audio_backend_that_never_opens_is_refused_at_start() {
    let (_dir, app) = application_data_dir("game-audio-never-opens");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.audio_backends = opts.audio_backends.game_audio(Box::new(NeverOpens));
    let err = match Recorder::start_with_options(stub_config(&app), opts) {
        Ok(_) => panic!("a recording whose game audio never comes up must not start"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("did not come up"),
        "the refusal must say the backend never came up; got: {message}"
    );
    assert!(
        message.contains("game audio"),
        "the refusal must name which backend; got: {message}"
    );
}

/// A recording whose game-audio backend opens and then stays silent is refused.
///
/// The open succeeding is not the same as the endpoint being there: a default render
/// endpoint that points at a device which is not present opens and then delivers nothing,
/// and the recording it would produce is a file with no sound in it.
#[test]
fn a_game_audio_backend_that_opens_and_stays_silent_is_refused_at_start() {
    let (_dir, app) = application_data_dir("game-audio-silent");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.audio_backends = opts
        .audio_backends
        .game_audio(Box::new(OpensAndDeliversNothing));
    let err = match Recorder::start_with_options(stub_config(&app), opts) {
        Ok(_) => panic!("a recording whose game audio delivers nothing must not start"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("delivered no audio"),
        "the refusal must say the backend delivered nothing; got: {message}"
    );
}

/// A recording whose game-audio backend opens and then hangs is refused at start.
///
/// The wait for the first block is bounded like the open is: a backend that opens and then
/// never answers is a thread stuck inside the platform's audio service, and a start that waits
/// for it says nothing and captures nothing.
#[test]
fn a_game_audio_backend_that_opens_and_hangs_is_refused_at_start() {
    let (_dir, app) = application_data_dir("game-audio-hangs");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.audio_backends = opts.audio_backends.game_audio(Box::new(OpensAndHangs));
    let err = match Recorder::start_with_options(stub_config(&app), opts) {
        Ok(_) => panic!("a recording whose game audio never answers must not start"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("did not answer"),
        "the refusal must say the backend did not answer; got: {message}"
    );
    assert!(
        message.contains("game audio"),
        "the refusal must name which backend; got: {message}"
    );
}

/// The microphone is waited for the same way the game audio is.
///
/// A microphone that never comes up is a recording with a voice track nothing feeds, which
/// is the same failure as a game-audio track that is not there.
#[test]
fn a_microphone_that_never_opens_is_refused_at_start() {
    let (_dir, app) = application_data_dir("microphone-never-opens");
    let mut opts = options(RecordingMode::ReplayBuffer, true);
    opts.audio_backends = opts.audio_backends.microphone(Box::new(NeverOpens));
    let err = match Recorder::start_with_options(stub_config(&app), opts) {
        Ok(_) => panic!("a recording whose microphone never comes up must not start"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("did not come up"),
        "the refusal must say the backend never came up; got: {message}"
    );
    assert!(
        message.contains("microphone"),
        "the refusal must name which backend; got: {message}"
    );
}

/// The report of a failed run says what the audio side did.
///
/// The encoder's own errors name the child, and a child that never took its audio input
/// is reported as a dead writer thread; the numbers are what say whether the track was
/// ever produced and whether it ever left the queue on the way to the child.
#[test]
fn a_failed_run_reports_what_the_audio_side_handed_over() {
    let (_dir, app) = application_data_dir("audio-fails-mid-run");
    let mut opts = options(RecordingMode::ReplayBuffer, false);
    opts.audio_backends = opts
        .audio_backends
        .game_audio(Box::new(FailsAfterTwoBlocks { delivered: 0 }));
    let recorder = Recorder::start_with_options(stub_config(&app), opts)
        .expect("the recording starts: the backend opens and delivers a block");

    let status = wait_for(
        &recorder,
        Duration::from_secs(10),
        "the run to fail",
        |status| status.error.is_some(),
    );
    let error = status.error.expect("the run failed");
    assert!(
        error.contains("the audio endpoint went away"),
        "the report must carry the failure itself; got: {error}"
    );
    assert!(
        error.contains("1 game-audio blocks"),
        "the report must say what the audio side handed to the encoder; got: {error}"
    );
    recorder.stop().ok();
}

// ---------------------------------------------------------------------------------------
// Production-readiness regressions: ownership, directories, ring-mode rows
// ---------------------------------------------------------------------------------------

/// A second recorder on a data directory that is already recording used to run recovery against the
/// first one's live session row, concatenating segments ffmpeg was still writing and deleting the
/// directory under it. It is refused instead, the first recording is untouched, and the directory
/// is free again once the first one stops.
#[test]
fn a_second_recorder_on_a_live_data_directory_is_refused_and_leaves_the_session_alone() {
    let (_tmp, app_data_dir) = application_data_dir("second-recorder");
    let first = Recorder::start_with_options(
        stub_config(&app_data_dir),
        options(RecordingMode::FullSession, false),
    )
    .expect("the first recorder starts");
    wait_for(&first, Duration::from_secs(40), "a segment on disk", |s| {
        s.segments >= 1
    });
    let db_path = stub_config(&app_data_dir).db_path();
    let row_dir = {
        let store = open_clip_index(&db_path).unwrap();
        let rows = store.list_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        PathBuf::from(&rows[0].scratch_dir)
    };

    let refused = Recorder::start_with_options(
        stub_config(&app_data_dir),
        options(RecordingMode::FullSession, false),
    )
    .err()
    .expect("a second recorder must not start");
    assert!(
        format!("{refused:#}").contains("already recording"),
        "{refused:#}"
    );

    // The live session is exactly as it was: row open, directory and segments in place.
    let store = open_clip_index(&db_path).unwrap();
    let rows = store.list_sessions().unwrap();
    assert_eq!(rows.len(), 1, "no second row, no recovery: {rows:?}");
    assert!(rows[0].ended_at_ms.is_none(), "still recording");
    assert!(row_dir.is_dir(), "the directory was not deleted under it");
    drop(store);

    first.stop().expect("the first recorder stops cleanly");
    let again = Recorder::start_with_options(
        stub_config(&app_data_dir),
        options(RecordingMode::ReplayBuffer, false),
    )
    .expect("the directory is free once the first recorder is gone");
    again.stop().expect("and stops");
}

/// A start that fails after the session directory was made used to leave it behind, empty, for
/// every refused start.
#[test]
fn a_start_that_fails_after_the_session_directory_exists_removes_it() {
    let (_tmp, app_data_dir) = application_data_dir("failed-start-dir");
    let mut cfg = stub_config(&app_data_dir);
    // Refused after `create_session_dir` (see `a_microphone_with_the_game_audio_off_is_refused_at_start`).
    cfg.audio_enabled = Some(false);
    let sessions_dir = cfg.sessions_dir();

    Recorder::start_with_options(cfg, options(RecordingMode::FullSession, true))
        .err()
        .expect("refused");

    let left: Vec<_> = std::fs::read_dir(&sessions_dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "no directory is left behind: {left:?}");
}

/// A replay buffer writes nothing to its scratch directory, and a row that names it invites the
/// retention pass to delete the configured `[buffer] scratch_dir` itself (the user's own folder,
/// when they point it somewhere real) once the session ends. The row records no directory.
#[test]
fn a_buffer_row_never_names_the_scratch_directory() {
    let (_tmp, app_data_dir) = application_data_dir("buffer-row-dir");
    let cfg = stub_config(&app_data_dir);
    let db_path = cfg.db_path();
    let scratch = cfg.scratch_dir();

    let recorder = Recorder::start_with_options(cfg, options(RecordingMode::ReplayBuffer, false))
        .expect("starts");
    recorder.stop().expect("stops");

    let store = open_clip_index(&db_path).unwrap();
    let rows = store.list_sessions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_ne!(
        PathBuf::from(&rows[0].scratch_dir),
        scratch,
        "the shared scratch directory is not a session directory to delete"
    );
    assert_eq!(
        rows[0].scratch_dir, "",
        "a RAM ring has no directory of its own"
    );
}

/// A backend whose stop is a platform call that does not return until released.
struct HangsOnStop {
    release: mpsc::Receiver<()>,
}

impl AudioBackend for HangsOnStop {
    fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn next_buffer(&mut self, _timeout: Duration) -> anyhow::Result<Option<AudioBuffer>> {
        Ok(None)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        let _ = self.release.recv();
        Ok(())
    }
}

/// `WorkerAudio::stop` gave up waiting for the stop's answer after its budget, and then joined the
/// worker thread anyway: a stop that never returns made the shutdown (and a `Drop`) wait for ever on
/// the call the budget was meant to cut off. It must return its timeout error and leave the stuck
/// thread to finish alone.
#[test]
fn a_backend_that_never_stops_cannot_hang_the_shutdown() {
    let (release, hang) = mpsc::channel();
    let (mut worker, opening) =
        WorkerAudio::spawn(Box::new(HangsOnStop { release: hang }), "game audio").unwrap();
    worker.stop_budget = Duration::from_millis(200);
    worker.await_open(&opening, Duration::from_secs(5)).unwrap();

    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = worker.stop();
        let _ = done.send(outcome);
    });
    let outcome = finished
        .recv_timeout(Duration::from_secs(3))
        .expect("stop returned instead of joining a stuck thread");
    let err = outcome.expect_err("the stop timed out");
    assert!(format!("{err:#}").contains("did not stop"), "{err:#}");
    // Let the stuck call return so the test leaves no thread behind.
    let _ = release.send(());
}

/// The microphone's input id is published as it is: an encoder may hand out any id, `0` and `1`
/// included, and "no microphone" is the absence of one, not a number.
#[test]
fn the_published_microphone_input_is_an_option_not_a_number_with_a_magic_value() {
    let status = SharedStatus::new();
    assert_eq!(
        status.mic_port(),
        None,
        "nothing is published before a recording"
    );
    for id in [Some(1), Some(0), Some(49_152), None] {
        status.publish_recording(30, 30, id);
        assert_eq!(status.mic_port(), id, "published as given");
    }
}
