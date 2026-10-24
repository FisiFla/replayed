//! The data transfer objects the frontend reads (clips, sessions, storage).

use replayed_store::{Clip, Session, SessionEvent};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------
// Data transfer objects
// ---------------------------------------------------------------------------------------

/// One clip as the frontend sees it. Field names are snake_case, matching the Rust field
/// names and the store's columns: the TS interface in `src/lib/types.ts` mirrors this
/// exactly, and a test below pins the JSON keys so the two cannot drift silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipDto {
    pub id: i64,
    pub path: String,
    /// Media-time position of the clip's first frame (see `replayed_store::Clip`). Not
    /// comparable across captures; the list is ordered by `created_at_ms` instead.
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    pub favourite: bool,
    /// Wall-clock instant the row was written, ms since the Unix epoch.
    pub created_at_ms: i64,
    /// User-chosen display name (schema v5); `None` shows the file name. The file is never
    /// renamed.
    pub title: Option<String>,
    /// The game the recording matched; `None` when none was detected.
    pub game: Option<String>,
    /// User tags, normalized (lower-case) and sorted.
    pub tags: Vec<String>,
}

impl From<&Clip> for ClipDto {
    fn from(clip: &Clip) -> Self {
        Self {
            id: clip.id,
            path: clip.path.to_string_lossy().into_owned(),
            started_at_ms: clip.started_at_ms,
            duration_ms: clip.duration_ms,
            size_bytes: clip.size_bytes,
            codec: clip.codec.clone(),
            favourite: clip.favourite,
            created_at_ms: clip.created_at_ms,
            title: clip.title.clone(),
            game: clip.game.clone(),
            tags: clip.tags.clone(),
        }
    }
}

/// One recording session, as the Sessions list sees it.
///
/// Mirrored by hand in `src/lib/types.ts`, and pinned by a test below that asserts the exact
/// JSON keys.
///
/// `media_epoch_ms` is deliberately not here. It is the anchor the store subtracts to turn an
/// event's absolute media time into a timeline offset, and offsets arrive already computed;
/// exposing the epoch would invite the frontend to subtract a second time, the cross-clock
/// defect the column was added to fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDto {
    pub id: i64,
    /// The game the watcher matched. `None` for a session nobody detected, which is the
    /// ordinary case for a manual start.
    pub game: Option<String>,
    /// `"buffer"` or `"session"` (see `replayed_store::SESSION_MODE_*`). A buffer session
    /// has no concatenated file: the row exists to group one run's clips and events.
    pub mode: String,
    /// Wall clock, ms since the Unix epoch. Not comparable with `offset_ms`, which is media time:
    /// two different clocks, on purpose.
    pub started_at_ms: i64,
    /// When it stopped. **`None` means it is still recording.**
    pub ended_at_ms: Option<i64>,
    /// The concatenated file, once finalised. `None` while it runs, and always for a
    /// buffer-mode session.
    pub final_path: Option<String>,
    /// Bytes the session occupies, as the store recorded them.
    pub size_bytes: i64,
    /// Exempt from both session retention rules, exactly as a favourited clip is.
    pub favourite: bool,
    /// Where the segments are, so an operator can find the footage without the database.
    pub scratch_dir: String,
    /// How long the recorded media is, in ms: the axis the review timeline is drawn against.
    ///
    /// `0` means unknown, not "zero length": a session recovered from a crash has no concatenated
    /// file to probe, and rows written before schema v4 predate the column. A front-end must
    /// disable its scrubber on 0.
    ///
    /// Not derivable from `started_at_ms`/`ended_at_ms`, the wall-clock window: a session whose
    /// encoder could not keep up is shorter than the clock says.
    pub duration_ms: i64,
}

impl From<&Session> for SessionDto {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id,
            game: s.game.clone(),
            mode: s.mode.clone(),
            started_at_ms: s.started_at_ms,
            ended_at_ms: s.ended_at_ms,
            final_path: s.final_path.clone(),
            size_bytes: s.size_bytes,
            favourite: s.favourite,
            scratch_dir: s.scratch_dir.clone(),
            duration_ms: s.duration_ms,
        }
    }
}

/// One marker on a session's timeline, as the scrubber plots it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventDto {
    pub id: i64,
    /// The integration's tag: `"kill"`, `"death"`, `"round_start"`, or the `"bookmark"` a hotkey
    /// clip writes. Passed through verbatim: this layer does not know the vocabulary, so a tag it
    /// has never seen is a marker that exists rather than an error. The UI colours unknown tags
    /// neutrally.
    pub kind: String,
    /// The marker's position on the session timeline, in ms of media time from the session's start.
    /// Computed by the store in one SQL expression, so the scrubber, the tests and any consumer
    /// agree on what a position means.
    pub offset_ms: i64,
    /// The integration's own detail, as JSON text, verbatim. `None` for a bookmark, which
    /// has no source to describe.
    pub payload: Option<String>,
    /// The clip this marker produced, when it produced one. `None` for a marker recorded
    /// without clipping.
    pub clip_id: Option<i64>,
}

impl From<&SessionEvent> for SessionEventDto {
    fn from(e: &SessionEvent) -> Self {
        Self {
            id: e.id,
            kind: e.kind.clone(),
            offset_ms: e.offset_ms,
            payload: e.payload.clone(),
            clip_id: e.clip_id,
        }
    }
}

/// The storage panel's data: what is on disk, what the cap is, and what the policy makes of it.
///
/// Every count and byte is about the clips that are on disk, the set the listing shows, from the
/// same function (`clips_on_disk`). `missing_count` is how many indexed rows that leaves out, so
/// the panel can say so rather than show an unexplained smaller number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageStats {
    /// Clips whose file is there. Not the number of rows in the index.
    pub clip_count: usize,
    /// Bytes those clips actually occupy, summed from the clips, not from what the rows recorded
    /// (see `clips_on_disk`).
    pub total_bytes: u64,
    /// Indexed rows whose file is gone: not counted above, and named here rather than
    /// silently dropped.
    pub missing_count: usize,
    /// Session rows whose recorded file is gone. Not counted in any figure above either — a
    /// session has no entry in `clip_count`/`total_bytes` — and named so the panel can offer the
    /// reconciliation for both libraries at once.
    pub missing_sessions: usize,
    pub favourite_count: usize,
    pub favourite_bytes: u64,
    pub cap_bytes: u64,
    pub max_age_days: u64,
    /// Whether the policy's verdict is that the cap can be met. `false` means the favourites alone
    /// exceed it: the manager stops rather than delete a favourite to satisfy a cap it cannot meet.
    pub cap_met: bool,
    /// How far the favourited clips alone exceed the cap. Non-zero is the warning.
    pub over_cap_by_bytes: u64,
    /// How many clips the next cleanup pass would delete.
    pub planned_deletions: usize,
    /// Total bytes the library would hold after that pass.
    pub bytes_after: u64,
    pub clips_dir: String,
    /// Startup problems worth showing (a missing ffmpeg, most likely).
    pub warnings: Vec<String>,
}

/// Where a clip's cached thumbnail is, and whether it had to be made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThumbnailRef {
    pub clip_id: i64,
    pub path: String,
    pub at_ms: u64,
    /// `true` when the file already existed and ffmpeg was not run again.
    pub cached: bool,
}

/// What `delete_clip` did.
///
/// The row goes first and the file second; this reports each half separately, because the
/// interesting states are the mismatches: a row deleted whose file could not be unlinked is an
/// orphan for a later sweep or the user, and must never be reported as a clean delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteOutcome {
    pub id: i64,
    /// Whether this call's own `DELETE` removed a row. `false` means the clip was already gone from
    /// the index, in which case nothing was unlinked because no committed row named the file.
    pub row_deleted: bool,
    pub file_removed: bool,
    /// Set when the row was deleted but the file could not be unlinked.
    pub orphaned_path: Option<String>,
    /// Set when the row was deleted and the file was already absent.
    pub already_missing: bool,
    /// Bytes of file the unlink actually removed.
    pub bytes_reclaimed: u64,
    /// Cached thumbnails for this clip that were removed with it.
    pub thumbnails_removed: usize,
}

/// Where an export went, and what it is.
///
/// No clip id: an export is not in the library. It is a file beside the clip, and the window's job
/// is to say where it landed and offer to reveal it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportOutcome {
    /// The preset that produced it, by its wire name (`discord`, `vertical`, `gif`).
    pub preset: String,
    /// The file written. Outside the index on purpose (see `commands::export_clip`).
    pub path: String,
    /// Bytes on disk.
    pub size_bytes: u64,
    /// The exported file's own probed length, which a re-encode may shift by a frame or two.
    pub duration_ms: u64,
}

/// What removing the index rows whose files are gone did.
///
/// Counts rather than ids: the window reports what happened and refreshes, and a list of ids it
/// would not use is another thing to keep true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneOutcome {
    /// Clip rows removed because their file is gone.
    pub clips_removed: usize,
    /// Session rows removed because their recorded file is gone.
    pub sessions_removed: usize,
    /// Cached thumbnails removed for those clips (best effort; a stale JPEG is not a failure).
    pub thumbnails_removed: usize,
    /// Session rows the store refused because they are still recording. The plan never selects
    /// one, so a non-zero count means a session started between the plan and the delete: nothing
    /// was touched for those ids, and the next prune sees them again.
    pub sessions_refused: usize,
    /// Rows that no longer existed when the prune ran (a concurrent pass got there first).
    pub rows_already_gone: usize,
    /// Rows whose delete failed. The affected library is unchanged for them.
    pub failed: usize,
}

/// The displays this machine has, and the one the capture is pointed at.
///
/// The rate is here so the framerate setting can be made against the display's own: a capture
/// rate above the display's refresh rate buys nothing, because the display has nothing new to
/// show. Empty off Windows, where there is no screen to enumerate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureDisplaysDto {
    /// Every display, in `[capture] monitor` order: index 0 is the primary.
    pub displays: Vec<replayed_capture::DisplayInfo>,
    /// The index `[capture] monitor` selects, or `None` when the settings could not be read —
    /// in which case the capture target is unknown rather than assumed.
    pub selected: Option<usize>,
}

#[cfg(test)]
mod tests {
    use crate::commands::test_support::*;
    use crate::commands::{list_clips, storage_stats};

    #[test]
    fn a_clip_dto_carries_every_field_the_frontend_reads() {
        let f = Fixture::new();
        let id = f.add_row("fields.mp4", 1_234, 5_678, 9_101);

        let clips = list_clips(&f.deps()).unwrap();
        let clip = &clips[0];

        assert_eq!(clip.id, id);
        assert_eq!(
            clip.path,
            f.paths.clips_dir.join("fields.mp4").to_string_lossy()
        );
        assert_eq!(clip.started_at_ms, 1_234);
        assert_eq!(clip.duration_ms, 5_678);
        assert_eq!(clip.size_bytes, 9_101);
        assert_eq!(clip.codec, "mpeg4");
        assert!(!clip.favourite, "clips are not favourites by default");
        assert!(
            clip.created_at_ms > 0,
            "the store stamps a wall-clock creation instant"
        );
    }

    #[test]
    fn the_dto_json_matches_the_typescript_interface() {
        // `src/lib/types.ts` mirrors these structs by hand. Without this, a rename on either side is
        // found by a user looking at an empty list.
        let f = Fixture::new();
        f.add_row("shape.mp4", 1, 2, 3);

        let clip = serde_json::to_value(&list_clips(&f.deps()).unwrap()[0]).unwrap();
        let mut keys: Vec<&str> = clip
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "codec",
                "created_at_ms",
                "duration_ms",
                "favourite",
                "game",
                "id",
                "path",
                "size_bytes",
                "started_at_ms",
                "tags",
                "title"
            ]
        );

        let stats = serde_json::to_value(storage_stats(&f.deps()).unwrap()).unwrap();
        let mut keys: Vec<&str> = stats
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes_after",
                "cap_bytes",
                "cap_met",
                "clip_count",
                "clips_dir",
                "favourite_bytes",
                "favourite_count",
                "max_age_days",
                "missing_count",
                "missing_sessions",
                "over_cap_by_bytes",
                "planned_deletions",
                "total_bytes",
                "warnings"
            ]
        );
    }
}
