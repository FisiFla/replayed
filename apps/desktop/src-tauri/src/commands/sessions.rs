//! Session commands: timeline, matches, extraction and deletion.

use super::clips::{
    free_output_path, index_new_clip, inherit_metadata, session_artifact_ok, validate_range,
};
use super::*;
use replayed_media::edit::trim_lossless;
use replayed_media::probe::MediaInfo;
use replayed_store::Session;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------
//
// The session recorder writes into one scratch directory and concatenates it at the end; this
// is the window onto that.
//
// * `offset_ms` comes from the store, computed from the session's media epoch. Nothing here
//   recomputes it against the wall clock: that recomputation was a defect (a timeline measured
//   across two clocks) and stays fixed only while the subtraction lives in one place.
// * `extract_clip` refuses a session that is still recording. Its scratch directory is the only
//   copy of the footage, and a cut while the encoder is still writing would be from a moving
//   target.

/// One session row, or a `session_not_found` naming the id.
fn load_session(deps: &Deps<'_>, id: i64) -> Result<Session, CommandError> {
    deps.store
        .get_session(id)
        .map_err(|e| CommandError::store("reading the session", e))?
        .ok_or_else(|| CommandError::session_not_found(id))
}

/// Every session the index knows whose footage is on disk, newest first.
///
/// The order is the store's own (`ORDER BY started_at DESC, id DESC`), not re-sorted here: one
/// definition of "newest", the one the retention rules also use.
///
/// A row whose recorded file was deleted outside the application is left out, by the same rule the
/// clips list applies (`clips_on_disk`) and for the same reason: listing it is a promise the window
/// cannot keep — the session page would have no footage to scrub, cut from or play. The row is kept
/// rather than deleted (a file can be temporarily unavailable) and the storage panel offers the
/// user-confirmed reconciliation ([`prune_missing`]).
pub fn list_sessions(deps: &Deps<'_>) -> Result<Vec<SessionDto>, CommandError> {
    let sessions = deps
        .store
        .list_sessions()
        .map_err(|e| CommandError::store("listing the sessions", e))?;
    let missing = sessions
        .iter()
        .filter(|session| replayed_store::prune::session_is_missing(session))
        .count();
    if missing > 0 {
        tracing::debug!(
            "skipping {missing} session row(s) from the listing: the recorded file is gone"
        );
    }
    Ok(sessions
        .iter()
        .filter(|session| !replayed_store::prune::session_is_missing(session))
        .map(SessionDto::from)
        .collect())
}

/// One session's row, for the detail panel.
pub fn session_detail(deps: &Deps<'_>, session_id: i64) -> Result<SessionDto, CommandError> {
    Ok(SessionDto::from(&load_session(deps, session_id)?))
}

/// A session's timeline, in media-time order.
///
/// An empty vector is a real answer: a session nobody tagged has no markers. A missing session
/// is [`ErrorCode::SessionNotFound`], so the row is checked first rather than letting an unknown
/// id look like an empty timeline.
pub fn session_events(
    deps: &Deps<'_>,
    session_id: i64,
) -> Result<Vec<SessionEventDto>, CommandError> {
    load_session(deps, session_id)?;
    let events = deps
        .store
        .events_for_session(session_id)
        .map_err(|e| CommandError::store("reading the session timeline", e))?;
    Ok(events.iter().map(SessionEventDto::from).collect())
}

/// One match inside a session (Full Match splitting): a range of the session's media timeline,
/// the markers inside it, and for League the end-of-game scoreboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchDto {
    pub index: usize,
    pub start_ms: i64,
    pub end_ms: i64,
    /// A `game_end` marker closed it; `false` means the end is an inferred boundary.
    pub complete: bool,
    /// No `game_start` marker preceded the end; the start is an inferred boundary.
    pub start_inferred: bool,
    pub game_mode: Option<String>,
    pub map: Option<String>,
    /// `"Win"` / `"Lose"` when the game reported it; `None` is not a guess of either.
    pub result: Option<String>,
    /// The end-of-game scoreboard captured from the League Live Client (a GET); empty for
    /// games and matches that have none.
    pub scoreboard: Vec<serde_json::Value>,
    pub events: Vec<SessionEventDto>,
}

impl From<&replayed_store::matches::MatchSegment> for MatchDto {
    fn from(m: &replayed_store::matches::MatchSegment) -> Self {
        let parse = |p: &Option<String>| {
            p.as_deref()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        };
        let start = parse(&m.start_payload);
        let end = parse(&m.end_payload);
        let text = |v: &Option<serde_json::Value>, key: &str| {
            v.as_ref()
                .and_then(|v| v.get(key))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        let scoreboard = end
            .as_ref()
            .and_then(|v| v.get("scoreboard"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Self {
            index: m.index,
            start_ms: m.start_ms,
            end_ms: m.end_ms,
            complete: m.complete,
            start_inferred: m.start_inferred,
            game_mode: text(&start, "game_mode"),
            map: text(&start, "map"),
            result: text(&end, "result"),
            scoreboard,
            events: m.events.iter().map(SessionEventDto::from).collect(),
        }
    }
}

/// A session split into its matches, in timeline order. Empty is a real answer (a session
/// with no game markers); a missing session is `session_not_found`.
pub fn session_matches(deps: &Deps<'_>, session_id: i64) -> Result<Vec<MatchDto>, CommandError> {
    let session = load_session(deps, session_id)?;
    let events = deps
        .store
        .events_for_session(session_id)
        .map_err(|e| CommandError::store("reading the session timeline", e))?;
    let segments = replayed_store::matches::split_matches(&events, session.duration_ms);
    Ok(segments.iter().map(MatchDto::from).collect())
}

/// Cut one match out of a finished session as a clip (a "Full Match"). The range is the match's
/// own, clamped to the file's known length so a match running to the end of the recording does
/// not fail the range check by a rounding millisecond.
pub fn extract_match(
    deps: &Deps<'_>,
    session_id: i64,
    match_index: usize,
) -> Result<ClipDto, CommandError> {
    let matches = session_matches(deps, session_id)?;
    let Some(m) = matches.get(match_index) else {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "session #{session_id} has no match #{match_index} ({} found)",
                matches.len()
            ),
        ));
    };
    let session = load_session(deps, session_id)?;
    let start = m.start_ms.max(0) as u64;
    let mut end = m.end_ms.max(0) as u64;
    if session.duration_ms > 0 {
        end = end.min(session.duration_ms as u64);
    }
    extract_clip(deps, session_id, start, end)
}

/// Mark a session as a favourite, or clear it: the user's only way to protect one from the
/// session retention rules, as for a clip.
pub fn set_session_favourite(
    deps: &Deps<'_>,
    session_id: i64,
    favourite: bool,
) -> Result<SessionDto, CommandError> {
    // Checked first so a bogus id is a `session_not_found` rather than the store's own "nothing was
    // favourited", which carries no code the frontend can switch on.
    load_session(deps, session_id)?;
    deps.store
        .set_session_favourite(session_id, favourite)
        .map_err(|e| CommandError::store("setting the session's favourite flag", e))?;
    session_detail(deps, session_id)
}

/// What deleting a session actually did.
///
/// The row goes first and the bytes after, for the reason [`DeleteOutcome`] exists: the index
/// must never name files that are gone. The bytes are a directory of segments plus usually one
/// concatenated file, so "did it work" is two answers, both reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionOutcome {
    pub id: i64,
    /// False when nothing named that id. A UI acting on a list fetched a moment ago can race a
    /// retention pass, and "already gone" lets it refresh truthfully (as
    /// [`DeleteOutcome::row_deleted`]).
    pub row_deleted: bool,
    /// The scratch directory of segments is gone.
    pub scratch_removed: bool,
    /// The concatenated session file is gone.
    pub final_file_removed: bool,
    /// Bytes freed, measured from what was on disk immediately before removal.
    pub bytes_reclaimed: u64,
    /// Paths the committed row delete named that could not be removed afterwards. Reported rather
    /// than swallowed: an orphan is recoverable, a silent one is not.
    pub orphaned: Vec<String>,
}

/// Delete a session: its row, then its scratch directory and its concatenated file.
///
/// **A session that is still recording is refused** with [`ErrorCode::SessionStillRecording`].
/// The store refuses it too and guarantees it; this check lets the frontend say why without
/// parsing a message.
pub fn delete_session(
    deps: &Deps<'_>,
    session_id: i64,
) -> Result<DeleteSessionOutcome, CommandError> {
    // The row is read first so a running session is refused with a code the frontend can switch
    // on. A missing row is not an error but `row_deleted: false`, as `delete_clip` answers, so a UI
    // racing a retention pass still refreshes truthfully.
    let Some(session) = deps
        .store
        .get_session(session_id)
        .map_err(|e| CommandError::store("reading the session", e))?
    else {
        return Ok(DeleteSessionOutcome {
            id: session_id,
            row_deleted: false,
            scratch_removed: false,
            final_file_removed: false,
            bytes_reclaimed: 0,
            orphaned: Vec::new(),
        });
    };
    if session.ended_at_ms.is_none() {
        return Err(CommandError::new(
            ErrorCode::SessionStillRecording,
            format!(
                "session #{session_id} is still recording: its scratch directory is the \
                 recording, and no ordering makes deleting it safe. Stop the recording first."
            ),
        ));
    }

    // Committed before anything is unlinked, and the paths are the ones a committed row delete
    // named (the store's own contract).
    let paths = deps
        .store
        .delete_session_returning_paths(session_id)
        .map_err(|e| CommandError::store("deleting the session's row", e))?;
    let Some(paths) = paths else {
        // Raced away between the read above and this delete. Nothing is unlinked, because no committed
        // row delete named anything.
        return Ok(DeleteSessionOutcome {
            id: session_id,
            row_deleted: false,
            scratch_removed: false,
            final_file_removed: false,
            bytes_reclaimed: 0,
            orphaned: Vec::new(),
        });
    };

    let mut outcome = DeleteSessionOutcome {
        id: session_id,
        row_deleted: true,
        scratch_removed: false,
        final_file_removed: false,
        bytes_reclaimed: 0,
        orphaned: Vec::new(),
    };

    // The recorded file is removed before the scratch directory. They are separate locations in
    // the normal layout (segments in the session's scratch directory, the concatenated file in the
    // sessions output directory) but nothing enforces it: the store takes both as caller-supplied
    // strings, so a layout placing the file among the segments would otherwise have its removal
    // reported as a failure because `remove_dir_all` had already taken it. File first is correct
    // for the nested case too.
    //
    // A buffer-mode session has no file, which is not a failure.
    if let Some(final_path) = paths.final_path {
        let final_path = PathBuf::from(final_path);
        // The row is user-writable: a recorded path that is not a session artifact costs the user the
        // row and nothing else (see `session_artifact_ok`).
        if let Err(why) = session_artifact_ok(&final_path, true, deps.paths) {
            tracing::warn!(
                "session #{session_id}: refusing to unlink {} ({why}); the index row is \
                 gone, the file is untouched",
                final_path.display()
            );
            outcome
                .orphaned
                .push(final_path.to_string_lossy().into_owned());
        } else {
            let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
            match std::fs::remove_file(&final_path) {
                Ok(()) => {
                    outcome.final_file_removed = true;
                    outcome.bytes_reclaimed += size;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::error!(
                        "session #{session_id} was deleted from the index but its recorded file \
                     could not be removed: {} ({err})",
                        final_path.display()
                    );
                    outcome
                        .orphaned
                        .push(final_path.to_string_lossy().into_owned());
                }
            }
        }
    }

    // The store hands these back as strings (from the database), so they become paths once, here.
    let scratch_dir = PathBuf::from(&paths.scratch_dir);
    if let Err(why) = session_artifact_ok(&scratch_dir, false, deps.paths) {
        tracing::warn!(
            "session #{session_id}: refusing to remove {} ({why}); the index row is gone, the \
             directory is untouched",
            scratch_dir.display()
        );
        outcome
            .orphaned
            .push(scratch_dir.to_string_lossy().into_owned());
        return Ok(outcome);
    }
    let scratch = dir_size(&scratch_dir);
    match std::fs::remove_dir_all(&scratch_dir) {
        Ok(()) => {
            outcome.scratch_removed = true;
            outcome.bytes_reclaimed += scratch;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "session #{session_id} was deleted from the index but its scratch directory \
                 was already gone: {}",
                scratch_dir.display()
            );
        }
        Err(err) => {
            tracing::error!(
                "session #{session_id} was deleted from the index but its scratch directory \
                 could not be removed: {} ({err})",
                scratch_dir.display()
            );
            outcome
                .orphaned
                .push(scratch_dir.to_string_lossy().into_owned());
        }
    }

    Ok(outcome)
}

/// Cut `[start_ms, end_ms)` out of a finished session into a new clip, and index it.
///
/// The lossless path [`trim_clip`] uses (read that for what a stream copy can and cannot cut),
/// with one extra refusal: a session still recording cannot be cut from, because the file the
/// range was measured against is not the file that would be read.
///
/// A buffer-mode session has no concatenated file (it keeps a rolling ring), so the range has
/// nothing to be measured against; that is refused with the reason, and the clip hotkey is what
/// a running buffer is for.
pub fn extract_clip(
    deps: &Deps<'_>,
    session_id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    let session = load_session(deps, session_id)?;
    if session.ended_at_ms.is_none() {
        return Err(CommandError::new(
            ErrorCode::SessionStillRecording,
            format!(
                "session #{session_id} is still recording, so there is no finished file to cut \
                 from. Stop the recording, then extract the clip."
            ),
        ));
    }
    let Some(final_path) = session.final_path.as_deref() else {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "session #{session_id} has no concatenated file: it was recorded in buffer \
                 mode, which keeps a rolling ring rather than one session file. Use the clip \
                 hotkey while it is running."
            ),
        ));
    };
    let src = PathBuf::from(final_path);
    session_artifact_ok(&src, true, deps.paths).map_err(|why| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "session #{session_id} names {} as its recorded file, but {why}, so nothing \
                 will be cut from it",
                src.display()
            ),
        )
    })?;
    if !src.is_file() {
        return Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "session #{session_id} names {} as its recorded file, but that is not on disk, \
                 so there is nothing to cut from",
                src.display()
            ),
        ));
    }

    let bins = deps.binaries()?;
    // The range is validated against the file's own probed duration, not a stored number: the only
    // duration that matters is the one in the file about to be cut.
    let info = MediaInfo::probe(bins, &src)
        .map_err(|e| CommandError::media("reading the session file's duration", e))?;
    validate_range("session", session_id, info.duration_ms, start_ms, end_ms)?;
    let Some(video) = info.video.as_ref() else {
        return Err(CommandError::new(
            ErrorCode::Media,
            format!(
                "{} has no video stream, so there is no picture to cut a clip from",
                src.display()
            ),
        ));
    };

    let dst = free_output_path(trim_output_path(&src, start_ms, end_ms))?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CommandError::io("creating the clips directory", dir, e))?;
    }

    trim_lossless(bins, &src, &dst, start_ms, end_ms)
        .map_err(|e| CommandError::media("extracting the clip from the session", e))?;

    // The new row's `started_at` is the clip's position on the session's media timeline, which the
    // session's markers are measured on, so a clip extracted at a marker sits at that marker. The
    // file's own timeline starts at zero and nothing compares `started_at` across files, so this is
    // the only reading that carries information. An estimate: a stream copy cannot cut at an
    // arbitrary point.
    let parent_started_at_ms = session.media_epoch_ms.max(0) as u64;
    let new = index_new_clip(
        deps,
        &format!("session #{session_id}"),
        parent_started_at_ms,
        &video.codec,
        &dst,
        start_ms,
        end_ms,
    )?;
    Ok(inherit_metadata(deps, new, session.game.as_deref(), &[]))
}

/// Total bytes of every file under `dir`, or 0 when it cannot be walked.
///
/// Used only to report what a deletion freed, so an unreadable directory is 0 rather than an
/// error: the deletion reports its own failure, and refusing to delete because the measurement
/// failed would be backwards.
pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&path),
                _ => std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;

    // -- sessions ------------------------------------------------------------------------

    #[test]
    fn the_session_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these by hand. Without this, a rename on either side is found by a
        // user looking at an empty session list.
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(2, 1_000);

        let dto = serde_json::to_value(session_detail(&f.deps(), session_id).unwrap()).unwrap();
        let mut keys: Vec<&str> = dto
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "duration_ms",
                "ended_at_ms",
                "favourite",
                "final_path",
                "game",
                "id",
                "mode",
                "scratch_dir",
                "size_bytes",
                "started_at_ms"
            ]
        );
        assert!(
            !keys.contains(&"media_epoch_ms"),
            "the epoch stays behind the IPC boundary. Offsets arrive computed, and publishing \
             the anchor as well would invite the frontend to subtract a second time — which is \
             the cross-clock defect the column exists to fix"
        );

        f.store
            .insert_event(&replayed_store::NewEvent {
                session_id: Some(session_id),
                kind: "kill".to_string(),
                at_ms: 1_500,
                payload: Some("{\"source\":\"lol\"}".to_string()),
                clip_id: None,
            })
            .unwrap();
        let events = session_events(&f.deps(), session_id).unwrap();
        let event = serde_json::to_value(&events[0]).unwrap();
        let mut keys: Vec<&str> = event
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, ["clip_id", "id", "kind", "offset_ms", "payload"]);

        let outcome = delete_session(&f.deps(), session_id).unwrap();
        let value = serde_json::to_value(&outcome).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes_reclaimed",
                "final_file_removed",
                "id",
                "orphaned",
                "row_deleted",
                "scratch_removed"
            ]
        );
    }

    #[test]
    fn a_session_timeline_is_measured_from_the_media_epoch_not_the_wall_clock() {
        // Pins, at the IPC boundary, that `offset_ms` is media time relative to the session epoch. With
        // the wall and media clocks 1.7e12 ms apart, subtracting the wrong one is a different number
        // entirely, and this is what the scrubber plotted.
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(4, 1_000);
        for (kind, at_ms) in [("kill", 1_200u64), ("death", 4_000)] {
            f.store
                .insert_event(&replayed_store::NewEvent {
                    session_id: Some(session_id),
                    kind: kind.to_string(),
                    at_ms,
                    payload: None,
                    clip_id: None,
                })
                .unwrap();
        }

        let events = session_events(&f.deps(), session_id).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.kind.as_str(), e.offset_ms))
                .collect::<Vec<_>>(),
            vec![("kill", 200), ("death", 3_000)],
            "offset = at - media_epoch_ms (1000ms), not at - the wall start"
        );
    }

    fn add_event(f: &Fixture, session_id: i64, kind: &str, at_ms: u64, payload: Option<&str>) {
        f.store
            .insert_event(&replayed_store::NewEvent {
                session_id: Some(session_id),
                kind: kind.to_string(),
                at_ms,
                payload: payload.map(str::to_owned),
                clip_id: None,
            })
            .unwrap();
    }

    #[test]
    fn a_session_splits_into_matches_carrying_the_scoreboard_and_result() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(4, 1_000);
        add_event(
            &f,
            session_id,
            "game_start",
            1_500,
            Some("{\"game_mode\":\"CLASSIC\",\"map\":\"Map11\"}"),
        );
        add_event(&f, session_id, "kill", 2_000, None);
        add_event(
            &f,
            session_id,
            "game_end",
            4_000,
            Some("{\"result\":\"Win\",\"scoreboard\":[{\"champion\":\"Ahri\",\"kills\":5,\"is_local\":true}]}"),
        );

        let matches = session_matches(&f.deps(), session_id).unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!((m.start_ms, m.end_ms, m.complete), (500, 3_000, true));
        assert_eq!(m.game_mode.as_deref(), Some("CLASSIC"));
        assert_eq!(m.map.as_deref(), Some("Map11"));
        assert_eq!(m.result.as_deref(), Some("Win"));
        assert_eq!(m.scoreboard.len(), 1);
        assert_eq!(m.scoreboard[0]["champion"], "Ahri");
        assert_eq!(m.events.len(), 3);

        let keys: Vec<String> = {
            let v = serde_json::to_value(m).unwrap();
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(
            keys,
            [
                "complete",
                "end_ms",
                "events",
                "game_mode",
                "index",
                "map",
                "result",
                "scoreboard",
                "start_inferred",
                "start_ms"
            ]
        );
    }

    #[test]
    fn matches_of_a_missing_session_or_a_missing_match_are_refused() {
        let f = Fixture::new();
        assert_eq!(
            session_matches(&f.deps(), 9_999).unwrap_err().code,
            ErrorCode::SessionNotFound
        );
        let (session_id, _) = f.add_finished_session(1, 0);
        assert!(session_matches(&f.deps(), session_id).unwrap().is_empty());
        assert_eq!(
            extract_match(&f.deps(), session_id, 0).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn extracting_a_match_cuts_its_range_into_the_library() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(4, 0);
        add_event(&f, session_id, "game_start", 1_000, None);
        add_event(&f, session_id, "game_end", 3_000, None);
        let clip = extract_match(&f.deps(), session_id, 0).expect("the match is extracted");
        assert!(PathBuf::from(&clip.path).is_file());
        assert!(
            (1_000..=3_000).contains(&clip.duration_ms),
            "got {}ms",
            clip.duration_ms
        );
    }

    #[test]
    fn an_empty_timeline_and_a_missing_session_are_different_answers() {
        // A session nobody tagged has no markers: an empty list. A session that does not exist is an
        // error. Conflating the two would show an empty scrubber for a typo.
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(1, 0);
        assert!(session_events(&f.deps(), session_id).unwrap().is_empty());

        let err = session_events(&f.deps(), 9_999).expect_err("no such session");
        assert_eq!(err.code, ErrorCode::SessionNotFound);
        assert_eq!(
            session_detail(&f.deps(), 9_999).unwrap_err().code,
            ErrorCode::SessionNotFound
        );
        assert_eq!(
            set_session_favourite(&f.deps(), 9_999, true)
                .unwrap_err()
                .code,
            ErrorCode::SessionNotFound
        );
    }

    #[test]
    fn a_session_whose_file_is_gone_is_not_listed_but_its_row_is_kept() {
        // The reported symptom: a session file deleted in Explorer stayed in the sidebar, and
        // selecting it opened a page with no footage to scrub. The row is not deleted by the read
        // (the file may be temporarily unavailable); it is left out, and the storage panel offers
        // the user-confirmed prune.
        let f = Fixture::new();
        let (kept, _) = f.add_finished_session(1, 0);
        // A second row naming a file that does not exist: the fixture always writes
        // `session-1.mp4`, so the phantom is built by hand rather than by deleting the kept
        // session's own file.
        let missing_path = f
            .paths
            .clips_dir
            .parent()
            .unwrap()
            .join("sessions-out")
            .join("session-2.mp4");
        let gone = f
            .store
            .start_session(
                None,
                replayed_store::SESSION_MODE_SESSION,
                WALL_START_MS,
                "/nowhere",
                0,
            )
            .unwrap();
        f.store
            .end_session(
                gone,
                WALL_START_MS + 1_000,
                Some(&missing_path.to_string_lossy()),
                0,
                0,
            )
            .unwrap();

        let listed: Vec<i64> = list_sessions(&f.deps())
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            listed,
            vec![kept],
            "only the session with footage is listed"
        );
        assert!(!listed.contains(&gone));
        assert_eq!(
            f.store.list_sessions().unwrap().len(),
            2,
            "the row itself is kept: reconciling the index is the prune's job, not a read's"
        );
    }

    #[test]
    fn a_buffer_session_with_no_file_is_still_listed() {
        // "No concatenated file" is the normal state of a buffer session, not evidence that its
        // footage is gone: the ring owns it and the clips it produced are the artefacts.
        let f = Fixture::new();
        let (id, _) = f.add_running_session(replayed_store::SESSION_MODE_BUFFER);
        f.store
            .end_session(id, WALL_START_MS + 1_000, None, 0, 0)
            .unwrap();

        let listed: Vec<i64> = list_sessions(&f.deps())
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(listed, vec![id]);
    }

    #[test]
    fn a_favourite_can_be_set_and_cleared_and_is_visible_in_the_list() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(1, 0);
        assert!(!list_sessions(&f.deps()).unwrap()[0].favourite);

        assert!(
            set_session_favourite(&f.deps(), session_id, true)
                .unwrap()
                .favourite
        );
        assert!(
            list_sessions(&f.deps()).unwrap()[0].favourite,
            "the list reflects it"
        );

        assert!(
            !set_session_favourite(&f.deps(), session_id, false)
                .unwrap()
                .favourite
        );
    }

    #[test]
    fn extracting_a_clip_from_a_running_session_is_refused() {
        // Its scratch directory is being written into, so the file a range would be measured against
        // is not the file that would be read.
        let f = Fixture::new();
        let (id, _) = f.add_running_session(replayed_store::SESSION_MODE_SESSION);

        let err = extract_clip(&f.deps(), id, 0, 500).expect_err("a running session has no file");
        assert_eq!(err.code, ErrorCode::SessionStillRecording);
        assert!(err.message.contains("still recording"), "{}", err.message);

        // Deleting it is refused for the same reason, and by the same code.
        let err = delete_session(&f.deps(), id).expect_err("a running session is not deletable");
        assert_eq!(err.code, ErrorCode::SessionStillRecording);
        assert!(
            f.store.get_session(id).unwrap().is_some(),
            "and the row is untouched"
        );
    }

    #[test]
    fn a_buffer_session_has_no_file_to_extract_from() {
        // Buffer mode keeps a rolling ring rather than one session file, so there is nothing for a
        // range to be measured against. The message says which mode it was.
        let f = Fixture::new();
        let (id, dir) = f.add_running_session(replayed_store::SESSION_MODE_BUFFER);
        f.store
            .end_session(id, WALL_START_MS + 1_000, None, 0, 0)
            .unwrap();

        let err = extract_clip(&f.deps(), id, 0, 500).expect_err("no concatenated file");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("buffer"), "{}", err.message);
        assert!(dir.exists(), "the refusal changes nothing on disk");
    }

    #[test]
    fn extracting_a_clip_from_a_finished_session_writes_and_indexes_it() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(4, 1_000);

        let clip = extract_clip(&f.deps(), session_id, 1_000, 2_000).expect("the extract");
        assert!(
            PathBuf::from(&clip.path).is_file(),
            "the clip is on disk: {}",
            clip.path
        );
        assert!(clip.size_bytes > 0);
        assert!(
            (500..=1_500).contains(&clip.duration_ms),
            "a stream copy cuts on keyframes, so the range is approximate, but it must be \
             near the second that was asked for: got {}ms",
            clip.duration_ms
        );
        // The new row is placed on the session's media timeline, so a clip extracted at a marker sits
        // at that marker: epoch 1000 + start 1000.
        assert_eq!(
            clip.started_at_ms, 2_000,
            "positioned on the session's own timeline"
        );
        assert_eq!(
            list_clips(&f.deps())
                .unwrap()
                .iter()
                .filter(|c| c.id == clip.id)
                .count(),
            1,
            "and it is in the library, not merely on disk"
        );
    }

    #[test]
    fn extracting_out_of_range_or_inverted_is_refused_before_ffmpeg_runs() {
        let f = Fixture::new();
        let (session_id, _) = f.add_finished_session(2, 0);

        let err = extract_clip(&f.deps(), session_id, 500, 500).expect_err("an empty range");
        assert_eq!(err.code, ErrorCode::InvalidRange);
        assert!(
            err.message.contains("session #"),
            "the message names what it refused: {}",
            err.message
        );

        let err = extract_clip(&f.deps(), session_id, 1_500, 900_000).expect_err("past the end");
        assert_eq!(err.code, ErrorCode::OutOfRange);
    }

    #[test]
    fn deleting_a_finished_session_removes_its_row_its_segments_and_its_file() {
        let f = Fixture::new();
        let (id, final_path) = f.add_finished_session(1, 0);
        // What the concatenation leaves behind in the scratch directory.
        let scratch = PathBuf::from(&f.store.get_session(id).unwrap().unwrap().scratch_dir);
        std::fs::write(scratch.join("seg-000001.mp4"), b"pretend segment").unwrap();

        let outcome = delete_session(&f.deps(), id).unwrap();
        assert!(outcome.row_deleted);
        assert!(outcome.scratch_removed, "the segments go");
        assert!(outcome.final_file_removed, "and the recorded file");
        assert!(
            outcome.orphaned.is_empty(),
            "nothing was left behind: {:?}",
            outcome.orphaned
        );
        assert!(!scratch.exists());
        assert!(!final_path.exists());
        assert!(
            outcome.bytes_reclaimed > 0,
            "the bytes that were freed are reported"
        );
        assert!(
            f.store.get_session(id).unwrap().is_none(),
            "the row is gone"
        );

        // A second delete is not an error: it says nothing was deleted, so a UI racing a retention
        // pass can refresh truthfully (as `delete_clip`).
        let again = delete_session(&f.deps(), id).unwrap();
        assert!(!again.row_deleted);
        assert_eq!(again.bytes_reclaimed, 0);
    }

    /// A finished session row whose paths are whatever the test says: the index is a user-writable
    /// file, so nothing about them can be trusted.
    fn add_session_row(f: &Fixture, scratch: &Path, final_path: Option<&Path>) -> i64 {
        let id = f
            .store
            .start_session(
                None,
                replayed_store::SESSION_MODE_SESSION,
                WALL_START_MS,
                &scratch.to_string_lossy(),
                0,
            )
            .unwrap();
        f.store
            .end_session(
                id,
                WALL_START_MS + 1_000,
                final_path.map(|p| p.to_string_lossy()).as_deref(),
                0,
                1_000,
            )
            .unwrap();
        id
    }

    #[test]
    fn deleting_a_session_whose_row_points_outside_the_library_costs_the_row_only() {
        let f = Fixture::new();
        let elsewhere = tempfile::tempdir().unwrap();
        // A directory with precious contents that is not shaped like a session, and a file likewise: a
        // tampered index must not be able to make the shell remove either.
        let precious_dir = elsewhere.path().join("Documents");
        std::fs::create_dir_all(&precious_dir).unwrap();
        std::fs::write(precious_dir.join("thesis.txt"), b"x").unwrap();
        let precious_file = elsewhere.path().join("photo.mp4");
        std::fs::write(&precious_file, b"x").unwrap();
        let id = add_session_row(&f, &precious_dir, Some(&precious_file));

        let outcome = delete_session(&f.deps(), id).unwrap();
        assert!(outcome.row_deleted);
        assert!(!outcome.scratch_removed && !outcome.final_file_removed);
        assert_eq!(
            outcome.orphaned.len(),
            2,
            "both are reported, not removed: {:?}",
            outcome.orphaned
        );
        assert!(precious_dir.join("thesis.txt").is_file());
        assert!(precious_file.is_file());
    }

    #[test]
    fn a_session_row_naming_the_library_itself_removes_nothing() {
        let f = Fixture::new();
        let app_data = f.paths.db_path.parent().unwrap().to_path_buf();
        let id = add_session_row(&f, &app_data, Some(&f.paths.db_path));

        let outcome = delete_session(&f.deps(), id).unwrap();
        assert!(!outcome.scratch_removed);
        assert!(
            f.paths.clips_dir.is_dir() && app_data.is_dir(),
            "the library survives"
        );
    }

    #[test]
    fn a_session_shaped_directory_in_a_custom_sessions_dir_is_still_removed() {
        let f = Fixture::new();
        let custom = tempfile::tempdir().unwrap();
        let scratch = custom.path().join("session-1700000000");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("seg-000001.mp4"), b"x").unwrap();
        let id = add_session_row(&f, &scratch, None);

        let outcome = delete_session(&f.deps(), id).unwrap();
        assert!(
            outcome.scratch_removed,
            "a configured sessions_dir keeps working"
        );
        assert!(!scratch.exists());
    }

    #[test]
    fn cutting_from_a_session_row_outside_the_library_is_refused_before_ffmpeg_runs() {
        let f = Fixture::new();
        let elsewhere = tempfile::tempdir().unwrap();
        let stranger = elsewhere.path().join("movie.mp4");
        std::fs::write(&stranger, b"x").unwrap();
        let scratch = f.paths.clips_dir.parent().unwrap().join("sessions");
        std::fs::create_dir_all(&scratch).unwrap();
        let id = add_session_row(&f, &scratch, Some(&stranger));

        let err = extract_clip(&f.deps(), id, 0, 500).expect_err("not a session artifact");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(!elsewhere.path().join("movie.trim-0-500.mp4").exists());
    }
}
