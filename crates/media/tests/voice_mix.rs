//! A saved clip with a microphone leads with a mix of the game audio and the voice.
//!
//! The microphone is recorded as a second audio track. A player that plays only the first
//! track (the app's own player, a WebView2 `<video>`, and most chat clients a clip is dropped
//! into) never plays the voice. And an export with no `-map` let ffmpeg keep one audio track,
//! the game audio, so the "made to be uploaded" presets dropped the voice too. Seen on the
//! Windows smoke test: the voice was in every file at about -25 dB, and nobody could hear it.
//!
//! The fixture's game audio is a 200 Hz tone and its microphone a 6 kHz tone, so which sound a
//! track carries is read back with a band-pass filter rather than assumed.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use replayed_media::export::{self, ExportPreset};
use replayed_media::{edit, FfmpegBinaries};
use std::path::{Path, PathBuf};
use std::process::Command;

const GAME_HZ: u32 = 200;
const MIC_HZ: u32 = 6000;

/// Louder than this at a tone's frequency: the tone is in the track.
const PRESENT_DB: f64 = -40.0;
/// Quieter than this: it is not. The tones are about -21 dB on their own.
const ABSENT_DB: f64 = -50.0;

fn ffmpeg() -> FfmpegBinaries {
    FfmpegBinaries::discover(None).expect("ffmpeg on PATH (brew install ffmpeg)")
}

/// 2s of video, the game audio and the microphone, tagged as the encoder tags a recording.
/// `fragmented` writes it the way the replay buffer holds it in memory.
fn recording(dir: &Path, name: &str, fragmented: bool) -> PathBuf {
    let out = dir.join(name);
    let bin = ffmpeg();
    let mut cmd = Command::new(&bin.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=320x180:rate=30")
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={GAME_HZ}:sample_rate=48000"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={MIC_HZ}:sample_rate=48000"))
        .args([
            "-t",
            "2",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-c:a",
            "aac",
            "-ac",
            "2",
        ]);
    for (index, title) in edit::audio_titles(2).iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={title}"));
    }
    if fragmented {
        cmd.args(["-movflags", "frag_keyframe+empty_moov", "-f", "mp4"]);
    }
    let status = cmd.arg(&out).status().expect("spawn ffmpeg");
    assert!(
        status.success(),
        "the recording fixture could not be encoded"
    );
    out
}

/// The `name` of each audio track, in order (the MP4 muxer stores `title=` there).
fn audio_names(bin: &FfmpegBinaries, path: &Path) -> Vec<Option<String>> {
    let out = Command::new(&bin.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_streams"])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
    json["streams"]
        .as_array()
        .expect("a streams array")
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .map(|s| s["tags"]["name"].as_str().map(str::to_string))
        .collect()
}

/// The mean level, in dB, of audio track `track` of `path` around `hz`.
fn level_at(bin: &FfmpegBinaries, path: &Path, track: usize, hz: u32) -> f64 {
    let out = Command::new(&bin.ffmpeg)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{track}"), "-af"])
        .arg(format!("bandpass=f={hz}:width_type=q:w=10,volumedetect"))
        .args(["-f", "null", "-"])
        .output()
        .expect("spawn ffmpeg");
    let log = String::from_utf8_lossy(&out.stderr);
    let line = log
        .lines()
        .find(|l| l.contains("mean_volume:"))
        .unwrap_or_else(|| panic!("no level for track {track} of {}: {log}", path.display()));
    line.split("mean_volume:")
        .nth(1)
        .and_then(|v| v.trim().trim_end_matches("dB").trim().parse().ok())
        .unwrap_or_else(|| panic!("an unreadable level: {line}"))
}

fn names(titles: &[&str]) -> Vec<Option<String>> {
    titles.iter().map(|t| Some((*t).to_string())).collect()
}

/// A clip's three tracks: the mix first, then the game audio and the microphone as recorded.
fn assert_mix_leads(bin: &FfmpegBinaries, clip: &Path) {
    assert_eq!(
        audio_names(bin, clip),
        names(&[
            edit::MIX_TITLE,
            edit::GAME_AUDIO_TITLE,
            edit::MICROPHONE_TITLE
        ]),
        "the mix leads, and the two recorded tracks follow under their own names"
    );
    for hz in [GAME_HZ, MIC_HZ] {
        let db = level_at(bin, clip, 0, hz);
        assert!(
            db > PRESENT_DB,
            "the mix must carry the {hz} Hz tone: {db} dB"
        );
    }
    let game_has_voice = level_at(bin, clip, 1, MIC_HZ);
    assert!(
        game_has_voice < ABSENT_DB,
        "the game-audio track is still the game alone: {game_has_voice} dB of the voice"
    );
    let voice_has_game = level_at(bin, clip, 2, GAME_HZ);
    assert!(
        voice_has_game < ABSENT_DB,
        "the microphone track is still the voice alone: {voice_has_game} dB of the game"
    );
}

#[test]
fn a_clip_saved_from_memory_with_a_microphone_leads_with_a_mix_of_both() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let held = recording(dir.path(), "held.mp4", true);
    let clip = dir.path().join("clip.mp4");

    edit::remux_stream_lossless(&bin, std::fs::read(&held).unwrap(), &clip, 2).expect("the save");

    assert_mix_leads(&bin, &clip);
}

#[test]
fn a_clip_cut_from_segments_with_a_microphone_leads_with_a_mix_of_both() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let a = recording(dir.path(), "a.mp4", false);
    let b = recording(dir.path(), "b.mp4", false);
    let list = dir.path().join("concat.txt");
    let total = edit::write_concat_list(&[a, b], &list).unwrap();
    let clip = dir.path().join("clip.mp4");

    edit::concat_clip_sized(&bin, &list, &clip, total, 2).expect("the clip");

    assert_mix_leads(&bin, &clip);
}

/// The full session is the recording itself, not a clip to share: it keeps the two tracks.
#[test]
fn a_session_file_is_not_given_a_mix() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let a = recording(dir.path(), "a.mp4", false);
    let list = dir.path().join("concat.txt");
    let total = edit::write_concat_list(&[a], &list).unwrap();
    let session = dir.path().join("session.mp4");

    edit::concat_lossless_sized(&bin, &list, &session, total, 2).expect("the session");

    assert_eq!(
        audio_names(&bin, &session),
        names(&[edit::GAME_AUDIO_TITLE, edit::MICROPHONE_TITLE])
    );
}

/// A trim copies every track, so the mix and both names survive it.
#[test]
fn a_trim_of_a_mixed_clip_keeps_the_mix_and_the_names() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let held = recording(dir.path(), "held.mp4", true);
    let clip = dir.path().join("clip.mp4");
    edit::remux_stream_lossless(&bin, std::fs::read(&held).unwrap(), &clip, 2).unwrap();
    let trimmed = dir.path().join("trimmed.mp4");

    edit::trim_lossless(&bin, &clip, &trimmed, 0, 1000).expect("the trim");

    assert_mix_leads(&bin, &trimmed);
}

/// The export of a mixed clip, and of a two-track file with no mix (a session, or a clip saved
/// before the mix existed), both carry the voice.
#[test]
fn an_export_carries_the_voice_with_or_without_a_mix_in_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let bin = ffmpeg();
    let two_tracks = recording(dir.path(), "session.mp4", false);
    let held = recording(dir.path(), "held.mp4", true);
    let mixed = dir.path().join("clip.mp4");
    edit::remux_stream_lossless(&bin, std::fs::read(&held).unwrap(), &mixed, 2).unwrap();

    for (src, preset) in [
        (&two_tracks, ExportPreset::Discord),
        (&mixed, ExportPreset::Discord),
        (&two_tracks, ExportPreset::Vertical),
        (&mixed, ExportPreset::Vertical),
    ] {
        let out = dir.path().join(format!(
            "{}-{}.mp4",
            src.file_stem().unwrap().to_string_lossy(),
            preset.as_str()
        ));
        export::export(&bin, src, &out, preset, 2_000).expect("the export");
        assert_eq!(
            audio_names(&bin, &out).len(),
            1,
            "an export for sharing has one audio track: {}",
            out.display()
        );
        for hz in [GAME_HZ, MIC_HZ] {
            let db = level_at(&bin, &out, 0, hz);
            assert!(
                db > PRESENT_DB,
                "{} must carry the {hz} Hz tone: {db} dB",
                out.display()
            );
        }
    }
}

#[test]
fn three_audio_tracks_are_the_mix_the_game_and_the_microphone() {
    assert_eq!(edit::audio_titles(1), vec![edit::GAME_AUDIO_TITLE]);
    assert_eq!(
        edit::audio_titles(2),
        vec![edit::GAME_AUDIO_TITLE, edit::MICROPHONE_TITLE]
    );
    assert_eq!(
        edit::audio_titles(3),
        vec![
            edit::MIX_TITLE,
            edit::GAME_AUDIO_TITLE,
            edit::MICROPHONE_TITLE
        ]
    );
}
