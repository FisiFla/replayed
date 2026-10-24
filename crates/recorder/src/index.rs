//! The clip index: opening it, inserting a just-spliced clip, and applying the storage policy.
//!
//! Synchronous bookkeeping over a [`Store`], costing the capture path nothing; it sits beside the
//! engine because it is the one part of the trigger path testable without ffmpeg or a backend.

use crate::config::StorageSection;
use anyhow::{Context, Result};
use replayed_events::GameEvent;
use replayed_media::edit::Chapter;
use replayed_replay::splice::ClipMetadata;
use replayed_store::prune::{clip_is_missing, session_is_missing};
use replayed_store::retention::{
    execute_retention, plan_retention, RetentionOutcome, RetentionPolicy, RetentionRules,
};
use replayed_store::{Clip, Event, NewClip, NewEvent, Session, Store};
use std::path::{Path, PathBuf};

/// Wall-clock now, in ms since the Unix epoch: the clock `clips.created_at` and the storage age rule use.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Wall-clock now in milliseconds since the Unix epoch; stamps clip file names (`clip-<ts>.mp4`).
///
/// Milliseconds, not seconds: two clips asked for within one second (a double press, or an event
/// right after a hotkey) must not share a file name, or the second overwrites the first.
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Open the clip index in the app data directory and bring its schema up to date.
///
/// A failure is fatal on purpose: an index that cannot be opened records nothing, and unrecorded
/// clips are never cleaned up. Stopping with the path in the log is diagnosable; carrying on is not.
pub fn open_clip_index(db_path: &Path) -> Result<Store> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let store = Store::open(db_path)?;
    store.migrate()?;
    tracing::info!(
        "clip index {} (schema v{}): {} clips, {} bytes",
        db_path.display(),
        store.schema_version()?,
        store.list_clips()?.len(),
        store.total_bytes()?
    );
    Ok(store)
}

/// Insert the `clips` row for a clip that has just been spliced.
///
/// * `started_at_ms` is the clip's first frame on the ring's ledger timeline (media time), the
///   timeline the footage is measured on; the wall-clock instant is `created_at`, stamped by the store.
/// * The codec is `clip.encoder`, the encoder that produced the file (`h264_nvenc`, `libx264`, ...),
///   not the config's abstract `h264`.
/// * `session_id` is left NULL: this function has no session context, and a made-up id would
///   point at a session that does not exist.
///
/// A failed insert is logged at ERROR and returns `None`; it is not fatal. The file exists and the
/// user asked for it, so losing it to bookkeeping would be worse than a missing entry. An unindexed
/// clip is invisible to the list and the storage policy.
pub fn index_clip(store: &Store, clip: &ClipMetadata, started_at_ms: u64) -> Option<i64> {
    let new = NewClip {
        path: clip.path.clone(),
        started_at_ms,
        duration_ms: clip.duration_ms,
        size_bytes: clip.size_bytes,
        codec: clip.encoder.clone(),
    };
    match store.insert_clip(&new) {
        Ok(id) => {
            tracing::info!(
                "indexed clip #{id} (media t={started_at_ms}ms, {}ms, {} bytes, {}): {}",
                clip.duration_ms,
                clip.size_bytes,
                clip.encoder,
                clip.path.display()
            );
            Some(id)
        }
        Err(err) => {
            tracing::error!(
                "could not index the clip that was just written ({}): {err:#}. The file is \
                 kept — it is on disk and it is what was asked for — but it will not appear \
                 in the clip list, and the storage policy cannot manage it.",
                clip.path.display()
            );
            None
        }
    }
}

/// The chapter marks for a clip covering `[started_at_ms, started_at_ms + duration_ms)` on the
/// media timeline: one per game event in that span, at its offset into the clip.
///
/// `trigger` is the game event that asked for the clip, as `(kind, at_ms)`; it is not in the index
/// yet when the chapters are written, so it is passed alongside. Bookmarks are left out: they mark
/// why a clip was taken, not what happened in the game. Repeats of one kind at one instant become
/// one mark.
pub fn clip_chapters(
    events: &[Event],
    trigger: Option<(&str, u64)>,
    started_at_ms: u64,
    duration_ms: u64,
) -> Vec<Chapter> {
    let end_ms = started_at_ms.saturating_add(duration_ms);
    let mut marks: Vec<(u64, &str)> = events
        .iter()
        .map(|e| (e.at_ms, e.kind.as_str()))
        .chain(trigger.map(|(kind, at)| (at, kind)))
        .filter(|&(at, kind)| kind != BOOKMARK_KIND && (started_at_ms..end_ms).contains(&at))
        .collect();
    marks.sort_unstable();
    marks.dedup();
    marks
        .into_iter()
        .map(|(at, kind)| Chapter {
            start_ms: at - started_at_ms,
            title: chapter_title(kind),
        })
        .collect()
}

/// `bomb_planted` becomes `Bomb planted`: the tag is the only name an event has here, and an
/// unknown tag must still produce a title.
fn chapter_title(kind: &str) -> String {
    let spaced = kind.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => spaced,
    }
}

/// The `events.kind` a manual clip records, so the timeline shows the moment the user chose.
///
/// The tag is the vocabulary the scrubber colours by (`replayed-store` passes it through
/// verbatim) and is deliberately not a [`GameEvent`]: only a keypress produced it.
pub const BOOKMARK_KIND: &str = "bookmark";

/// Record the `events` row for a manual clip and return its id. `payload` is `None`: a bookmark
/// has no source to describe.
pub fn index_bookmark(
    store: &Store,
    at_ms: u64,
    clip_id: Option<i64>,
    session_id: Option<i64>,
) -> Result<i64> {
    let id = store.insert_event(&NewEvent {
        session_id,
        kind: BOOKMARK_KIND.to_string(),
        at_ms,
        payload: None,
        clip_id,
    })?;
    tracing::info!("recorded bookmark event #{id} at media t={at_ms}ms against clip #{clip_id:?}");
    Ok(id)
}

/// Insert the `events` row for a derived game event, linked to its clip when it produced one.
///
/// * `at_ms` is the event's position on the ledger's media timeline, the clock of
///   `clips.started_at` and a session's `media_epoch_ms`. For a clip's own event it is the trigger
///   instant (`buffer.pre_seconds` into the clip), because that is when the event happened.
/// * `clip_id` is `None` for a marker (recorded but not clipped, see `EventKind::is_highlight`) and
///   for an event whose clip could not be indexed.
/// * `session_id` is the recording session; NULL means an event detached from its session (what
///   deletion does to it).
///
/// Returns the new row's id or the error: a written clip must not be lost to a bookkeeping failure,
/// but a marker is the only thing the caller asked for, so failing it is worth reporting.
pub fn index_event(
    store: &Store,
    event: &GameEvent,
    at_ms: u64,
    clip_id: Option<i64>,
    session_id: Option<i64>,
) -> Result<i64> {
    let new = NewEvent {
        session_id,
        kind: event.kind.as_tag().to_string(),
        at_ms,
        payload: event.payload.clone(),
        clip_id,
    };
    let id = store.insert_event(&new)?;
    match clip_id {
        Some(clip) => tracing::info!(
            "recorded {} event #{} at media t={at_ms}ms against clip #{clip}",
            event.kind,
            id
        ),
        None => tracing::info!(
            "recorded {} event #{} at media t={at_ms}ms (no clip: a marker, not a highlight)",
            event.kind,
            id
        ),
    }
    Ok(id)
}

/// Bookkeeping across storage-policy passes, so a lasting condition is stated when it appears or
/// its size changes rather than once per pass.
///
/// One field per cap: clips and sessions are planned independently, so each can be unsatisfiable
/// on its own. Public because they are the observable "why was nothing deleted", asserted by tests.
#[derive(Default, Debug)]
pub struct CleanupReport {
    /// The shortfall of the last pass whose clips cap could not be met; `None` while it is met, so a recurrence warns again.
    pub last_clips_shortfall: Option<u64>,
    /// The same for the sessions cap.
    pub last_sessions_shortfall: Option<u64>,
}

/// Apply the storage policy once: plan against the index, execute the plan, report.
///
/// One plan over both libraries (clips and sessions, each with its own cap and age limit) comes from
/// [`plan_retention`] and deletion ordering from [`execute_retention`]; this supplies the policy from
/// the config and the clock. Silent when idle, since it runs at startup and every
/// [`crate::CLEANUP_INTERVAL`]. The returned [`RetentionOutcome`] holds what the pass measured, so
/// callers and tests need not reconstruct it from log lines.
///
/// `clips_dir` is the directory the clips live in, and it is read as well as the index: a clip
/// whose index write failed is on disk and in that directory, and the policy plans over the rows.
/// Without the directory the orphans would be invisible — never counted, never evicted, never
/// reported — and the directory could grow past its cap without bound.
pub fn cleanup_pass(
    store: &Store,
    storage: &StorageSection,
    clips_dir: &Path,
    report: &mut CleanupReport,
) -> RetentionOutcome {
    let policy = RetentionPolicy {
        clips: RetentionRules {
            max_total_bytes: storage.max_total_bytes,
            // Days are `u32` in the retention rules and `u64` in the config; above `u32::MAX` means "never", so clamp rather than wrap.
            max_age_days: storage.max_age_days.min(u64::from(u32::MAX)) as u32,
        },
        sessions: RetentionRules {
            max_total_bytes: storage.sessions.max_total_bytes,
            max_age_days: storage.sessions.max_age_days.min(u64::from(u32::MAX)) as u32,
        },
    };

    let clips = match store.list_clips() {
        Ok(clips) => clips,
        Err(err) => {
            tracing::warn!("storage policy: cannot read the clip index: {err:#}");
            return RetentionOutcome::default();
        }
    };
    let sessions = match store.list_sessions() {
        Ok(sessions) => sessions,
        Err(err) => {
            tracing::warn!("storage policy: cannot read the session store: {err:#}");
            return RetentionOutcome::default();
        }
    };

    // The index is a cache and the filesystem is the truth: a row whose file is gone claims bytes
    // that cannot be reclaimed, so planning over it makes the cap look fuller than the disk is and
    // evicts live clips to pay for empty space. Plan over what is on disk; the dead rows themselves
    // are removed by an explicit, user-confirmed prune (`replayed_store::prune`), never here — a
    // retention read is not allowed to rewrite the library, and a network directory that is merely
    // offline must not look like a library of deletions.
    let live_clips: Vec<Clip> = clips
        .iter()
        .filter(|clip| !clip_is_missing(clip))
        .cloned()
        .collect();
    let live_sessions: Vec<Session> = sessions
        .iter()
        .filter(|session| !session_is_missing(session))
        .cloned()
        .collect();
    let missing_clips = clips.len() - live_clips.len();
    let missing_sessions = sessions.len() - live_sessions.len();
    if missing_clips > 0 || missing_sessions > 0 {
        // Debug rather than warn: a pass runs at startup and on a timer, so the condition would
        // repeat. The startup summary the desktop logs is where a user is told, and the storage
        // panel offers the prune.
        tracing::debug!(
            "storage policy: {missing_clips} clip row(s) and {missing_sessions} session row(s) \
             name a file that is gone; they were left out of the plan because they hold no bytes \
             to reclaim"
        );
    }

    let plan = plan_retention(&live_clips, &live_sessions, &policy, now_ms());
    let outcome = match execute_retention(store, &plan) {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!("storage policy: the pass could not be completed: {err:#}");
            return RetentionOutcome::default();
        }
    };

    if outcome.deleted() > 0 {
        tracing::info!(
            "storage policy: deleted {} clip(s) and {} session(s), reclaimed {} bytes; the \
             clips directory now holds {} bytes against its {} byte cap, and the session \
             store {} bytes against its {} byte cap",
            outcome.clips.deleted,
            outcome.sessions.deleted,
            outcome.bytes_reclaimed(),
            outcome.clips.bytes_after,
            policy.clips.max_total_bytes,
            outcome.sessions.bytes_after,
            policy.sessions.max_total_bytes
        );
    }
    let failed = outcome.clips.failed + outcome.sessions.failed;
    if failed > 0 {
        tracing::warn!(
            "storage policy: {failed} planned deletion(s) could not be applied; the affected \
             library is larger than the policy asked for"
        );
    }
    let leftovers: Vec<&PathBuf> = outcome.leftovers().collect();
    if !leftovers.is_empty() {
        // The row is gone but the path remains: a failed removal, a refused path (a symlinked or suspicious
        // session directory) or an already-missing file. Named, since "within cap" would otherwise be about rows only.
        tracing::warn!(
            "storage policy: {} path(s) named by deleted rows are still on disk: {}",
            leftovers.len(),
            leftovers
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // The files in the clips directory that no row names: a clip whose index write failed is
    // on disk and in the library's directory, but the policy plans over the rows. They are
    // counted against the cap and named, so the directory cannot grow past it unnoticed.
    let orphans = orphan_clips(clips_dir, &clips);
    let orphan_bytes: u64 = orphans.iter().map(|(_, bytes)| bytes).sum();
    if !orphans.is_empty() {
        tracing::warn!(
            "storage policy: {} file(s) in {} are not in the index ({} bytes): the index write \
             failed for them, so the policy cannot manage them. They count against the cap, and \
             only the user can remove them.",
            orphans.len(),
            clips_dir.display(),
            orphan_bytes
        );
    }

    // A cap that immune members alone exceed (favourites, a running session) can never be met, so the
    // shortfall is what the user must act on. Warn only when it appears or changes.
    report_clips_shortfall(
        report,
        policy.clips.max_total_bytes,
        &plan,
        &outcome,
        orphan_bytes,
    );
    report_sessions_shortfall(report, policy.sessions.max_total_bytes, &plan, &outcome);
    outcome
}

/// The clips in the directory that no row names, with their sizes.
///
/// A clip is kept when its index write fails — the user asked for it — but then nothing in the
/// index knows about it: it is not in the library, and the storage policy, which plans over the
/// rows, cannot see it either. These are those files.
fn orphan_clips(clips_dir: &Path, rows: &[Clip]) -> Vec<(PathBuf, u64)> {
    let named: std::collections::HashSet<&Path> =
        rows.iter().map(|row| row.path.as_path()).collect();
    let mut orphans = Vec::new();
    let Ok(entries) = std::fs::read_dir(clips_dir) else {
        // A directory that cannot be read holds nothing this pass can account for; the clip
        // rows are still planned over.
        return orphans;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || named.contains(path.as_path()) {
            continue;
        }
        let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        orphans.push((path, bytes));
    }
    orphans
}

/// The clips cap's unsatisfiable case (see [`cleanup_pass`]).
fn report_clips_shortfall(
    report: &mut CleanupReport,
    cap: u64,
    plan: &replayed_store::retention::RetentionPlan,
    outcome: &RetentionOutcome,
    orphan_bytes: u64,
) {
    // The directory's real size: the rows the policy planned over, plus the files no row
    // names. The cap is about the directory, so the orphans count.
    let held = outcome.clips.bytes_after.saturating_add(orphan_bytes);
    if outcome.clips.cap_met && held <= cap {
        report.last_clips_shortfall = None;
        return;
    }
    let shortfall = plan.clips.over_cap_by_bytes.max(held.saturating_sub(cap));
    if report.last_clips_shortfall != Some(shortfall) {
        tracing::warn!(
            "storage policy: storage.max_total_bytes ({cap}) cannot be satisfied — the clips \
             directory holds {held} bytes, {shortfall} over the cap, and nothing can be deleted \
             for it: favourites are exempt from both rules, and {orphan_bytes} of those bytes \
             are in files the index does not name. Raise storage.max_total_bytes, un-favourite \
             some clips, or remove the unindexed files by hand."
        );
        report.last_clips_shortfall = Some(shortfall);
    }
}

/// The sessions cap's unsatisfiable case: a favourited session, or one still recording (its scratch
/// directory exists nowhere else), both of which may not be deleted.
fn report_sessions_shortfall(
    report: &mut CleanupReport,
    cap: u64,
    plan: &replayed_store::retention::RetentionPlan,
    outcome: &RetentionOutcome,
) {
    if outcome.sessions.cap_met {
        report.last_sessions_shortfall = None;
    } else if report.last_sessions_shortfall != Some(plan.sessions.over_cap_by_bytes) {
        tracing::warn!(
            "storage policy: storage.sessions.max_total_bytes ({cap}) cannot be satisfied — \
             the sessions nothing may delete (favourited ones, and one that is still \
             recording) exceed it by {} bytes. The session store holds {} bytes. Raise \
             storage.sessions.max_total_bytes, un-favourite a session, or finish the \
             recording that is running.",
            plan.sessions.over_cap_by_bytes,
            outcome.sessions.bytes_after
        );
        report.last_sessions_shortfall = Some(plan.sessions.over_cap_by_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn event(kind: &str, at_ms: u64) -> Event {
        Event {
            id: 0,
            session_id: Some(1),
            kind: kind.into(),
            at_ms,
            payload: None,
            clip_id: None,
        }
    }

    #[test]
    fn a_clip_marks_every_event_in_its_span_at_its_offset() {
        let events = [
            event("round_start", 9_999),
            event("round_start", 10_000),
            event("kill", 12_000),
            event("bomb_planted", 14_000),
            event("death", 40_000),
        ];
        let marks = clip_chapters(&events, Some(("kill", 25_000)), 10_000, 30_000);
        let got: Vec<(u64, &str)> = marks
            .iter()
            .map(|c| (c.start_ms, c.title.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (0, "Round start"),
                (2_000, "Kill"),
                (4_000, "Bomb planted"),
                (15_000, "Kill")
            ],
            "the 9.999s round start is before the clip and the 40s death after its end"
        );
    }

    #[test]
    fn a_trigger_already_in_the_index_is_one_mark_not_two() {
        let marks = clip_chapters(&[event("kill", 5_000)], Some(("kill", 5_000)), 0, 10_000);
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn a_clip_with_only_bookmarks_gets_no_chapters() {
        let marks = clip_chapters(&[event(BOOKMARK_KIND, 5_000)], None, 0, 10_000);
        assert!(marks.is_empty(), "{marks:?}");
    }

    /// The clip metadata a ring's `trigger` hands back, without running ffmpeg.
    fn metadata(path: PathBuf, size_bytes: u64) -> ClipMetadata {
        ClipMetadata {
            path,
            duration_ms: 12_000,
            size_bytes,
            encoder: "h264_nvenc".into(),
        }
    }

    /// An open index in a temp directory, plus the directory holding it.
    fn index_dir() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = open_clip_index(&dir.path().join("replayed.db")).expect("open the index");
        (dir, store)
    }

    /// The schema version this build's store writes, read from the store itself: `replayed-store`'s
    /// `SCHEMA_VERSION` is private, and a hard-coded number broke when the store bumped it. A fresh
    /// in-memory database migrates through the same path as a file one.
    fn store_schema_version() -> i32 {
        let fresh = Store::open_in_memory().expect("an in-memory store");
        fresh.migrate().expect("migrating a fresh store");
        fresh.schema_version().expect("reading its version")
    }

    #[test]
    fn the_clip_index_is_created_migrated_and_reopened_in_place() {
        let (dir, store) = index_dir();
        let db = dir.path().join("replayed.db");
        assert!(db.is_file(), "opening the index creates the database file");
        assert_eq!(
            store.schema_version().unwrap(),
            store_schema_version(),
            "opening the index migrates it to the store's own current schema version \
             (`replayed-store` keeps that number private; see `store_schema_version`)"
        );

        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        index_clip(&store, &metadata(path, 500), 1_000).expect("the clip is indexed");
        drop(store);

        // A second start adopts the same index (migration is forward-only and idempotent).
        let reopened = open_clip_index(&db).expect("reopen the index");
        assert_eq!(reopened.schema_version().unwrap(), store_schema_version());
        assert_eq!(
            reopened.list_clips().unwrap().len(),
            1,
            "the clip survived the restart"
        );

        // The session columns the version gate exists for are present, so a session row can be opened and closed.
        let session = reopened
            .start_session(
                None,
                replayed_store::SESSION_MODE_BUFFER,
                1_000,
                "scratch",
                0,
            )
            .expect("the index carries the session columns");
        reopened
            .end_session(session, 2_000, None, 0, 0)
            .expect("and can close a session");
        assert_eq!(reopened.list_sessions().unwrap().len(), 1);
    }

    #[test]
    fn indexing_a_clip_records_the_values_it_was_handed() {
        let (dir, store) = index_dir();
        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let meta = metadata(path.clone(), std::fs::metadata(&path).unwrap().len());
        let before = now_ms();

        let id = index_clip(&store, &meta, 42_000).expect("the clip is indexed");
        let after = now_ms();

        let rows = store.list_clips().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, id);
        assert_eq!(row.path, path);
        assert_eq!(
            row.started_at_ms, 42_000,
            "the ledger position is recorded as given"
        );
        assert_eq!(row.duration_ms, meta.duration_ms);
        assert_eq!(
            row.size_bytes,
            std::fs::metadata(&path).unwrap().len(),
            "the recorded size is the real file's"
        );
        assert_eq!(
            row.codec, "h264_nvenc",
            "the encoder that produced the file, not the config's abstract codec"
        );
        assert!(!row.favourite, "a new clip is not a favourite");
        assert!(
            row.created_at_ms >= before && row.created_at_ms <= after,
            "created_at is stamped with the wall clock at insert ({})",
            row.created_at_ms
        );
        assert_eq!(store.total_bytes().unwrap(), 500);
    }

    #[test]
    fn a_clip_that_cannot_be_indexed_is_reported_and_the_file_is_kept() {
        let (dir, store) = index_dir();
        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let meta = metadata(path.clone(), 500);

        assert!(index_clip(&store, &meta, 1).is_some());
        // `clips.path` is UNIQUE, so a duplicate insert fails; the index write is the one thing that can fail after the clip exists.
        assert_eq!(
            index_clip(&store, &meta, 2),
            None,
            "a duplicate insert fails"
        );
        assert!(
            path.is_file(),
            "a failed index write must never cost the clip the user asked for"
        );
        assert_eq!(
            store.list_clips().unwrap().len(),
            1,
            "and must not corrupt the index"
        );
    }

    #[test]
    fn indexing_an_event_records_the_reason_and_its_clip() {
        let (dir, store) = index_dir();
        let path = dir.path().join("clip-1.mp4");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let clip_id =
            index_clip(&store, &metadata(path, 500), 30_000).expect("the clip is indexed");

        let kill = replayed_events::GameEvent::new(
            replayed_events::Source::Lol,
            replayed_events::EventKind::Kill,
            // One line of JSON, as the payload column holds.
            serde_json::json!({ "source": "lol", "killer": "Ahri" }),
        );
        let id =
            index_event(&store, &kill, 34_000, Some(clip_id), None).expect("the reason is written");

        let events = store.list_events().expect("read the events back");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, id);
        assert_eq!(events[0].kind, "kill", "the tag the vocabulary defines");
        assert_eq!(events[0].at_ms, 34_000, "media time, as given");
        assert_eq!(
            events[0].clip_id,
            Some(clip_id),
            "linked to the clip it produced"
        );
        assert_eq!(events[0].session_id, None);
        assert!(events[0].payload.as_deref().unwrap_or("").contains("Ahri"));

        // A marker: recorded, no clip.
        let marker = replayed_events::GameEvent::bare(
            replayed_events::Source::Gsi,
            replayed_events::EventKind::RoundStart,
        );
        index_event(&store, &marker, 40_000, None, None).expect("a marker is written too");
        let events = store.list_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "round_start");
        assert_eq!(events[1].clip_id, None);
        assert_eq!(
            events[1].payload, None,
            "a bare event has no detail to record"
        );
    }

    #[test]
    fn an_event_for_a_clip_that_does_not_exist_is_reported_rather_than_written() {
        // The foreign key is the store's; this checks the failure comes back as an error rather than being dropped.
        let (_dir, store) = index_dir();
        let event = replayed_events::GameEvent::bare(
            replayed_events::Source::Lol,
            replayed_events::EventKind::Kill,
        );
        assert!(index_event(&store, &event, 1_000, Some(7), None).is_err());
        assert!(store.list_events().unwrap().is_empty());
    }

    /// The storage policy, in the shape the config file gives it.
    fn storage(
        clips_dir: &Path,
        clips_cap: u64,
        sessions_dir: &Path,
        sessions_cap: u64,
    ) -> StorageSection {
        StorageSection {
            clips_dir: clips_dir.display().to_string(),
            max_total_bytes: clips_cap,
            max_age_days: 3_650,
            sessions: crate::config::SessionStorageRules {
                sessions_dir: sessions_dir.display().to_string(),
                max_total_bytes: sessions_cap,
                max_age_days: 3_650,
            },
        }
    }

    #[test]
    fn a_cleanup_pass_applies_the_configured_policy_through_the_index() {
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();

        // Two clips of 1_000 bytes each, indexed exactly as the trigger path indexes them.
        let mut paths = Vec::new();
        for i in 1..=2u64 {
            let path = clips_dir.join(format!("clip-{i}.mp4"));
            std::fs::write(&path, vec![0u8; 1_000]).unwrap();
            index_clip(&store, &metadata(path.clone(), 1_000), i * 1_000).unwrap();
            paths.push(path);
        }

        // Room for one of the two clips, and a session store with nothing in it.
        let storage = storage(&clips_dir, 1_000, &dir.path().join("sessions"), 1 << 30);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);

        let rows = store.list_clips().unwrap();
        assert_eq!(rows.len(), 1, "the cap allows one of the two clips");
        let survivor = rows[0].path.clone();
        assert!(survivor.is_file(), "the survivor's file is untouched");
        let evicted = paths
            .into_iter()
            .find(|p| *p != survivor)
            .expect("one was evicted");
        assert!(
            !evicted.exists(),
            "the evicted clip's row and its file are both gone"
        );
        assert_eq!(store.total_bytes().unwrap(), 1_000);
        assert_eq!(outcome.clips.deleted, 1);
        assert_eq!(
            outcome.clips.bytes_reclaimed, 1_000,
            "measured from the files, not the rows"
        );
        assert!(outcome.sessions.deleted == 0 && outcome.sessions.bytes_reclaimed == 0);
        assert!(outcome.clips.cap_met, "the clips cap is met after the pass");

        // A second pass must leave the survivor alone: this pass runs every `CLEANUP_INTERVAL`, so it must be a no-op once the library fits.
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);
        assert_eq!(store.list_clips().unwrap().len(), 1);
        assert!(survivor.is_file(), "an idle pass deletes nothing");
        assert_eq!(outcome.deleted(), 0);
    }

    #[test]
    fn a_clip_row_whose_file_is_gone_does_not_make_the_cap_evict_a_live_clip() {
        // The stale-index defect, at the layer that caused the loss: retention plans over the ROWS
        // it is given, so a row whose file was deleted in Explorer still claims its bytes. With the
        // live clip indexed first it is the oldest candidate, so a plan built from rows evicts it to
        // pay for empty space and keeps the phantom. A pass plans over what is on disk instead.
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();

        let live_path = clips_dir.join("clip-live.mp4");
        std::fs::write(&live_path, vec![0u8; 1_000]).unwrap();
        let live = index_clip(&store, &metadata(live_path.clone(), 1_000), 1_000).unwrap();

        let dead_path = clips_dir.join("clip-dead.mp4");
        std::fs::write(&dead_path, vec![0u8; 1_000]).unwrap();
        let dead = index_clip(&store, &metadata(dead_path.clone(), 1_000), 2_000).unwrap();
        // The user deleted the file in Explorer: the row is now a phantom claiming 1_000 bytes.
        std::fs::remove_file(&dead_path).unwrap();

        // A cap the live clip exactly fills, and a session store with nothing in it.
        let storage = storage(&clips_dir, 1_000, &dir.path().join("sessions"), 1 << 30);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);

        assert_eq!(
            outcome.deleted(),
            0,
            "nothing is over the cap once only files on disk are counted"
        );
        assert!(
            live_path.is_file(),
            "clip #{live} is on disk and within the cap: it must not be evicted for a row that \
             holds no bytes"
        );
        assert!(
            store.list_clips().unwrap().iter().any(|c| c.id == dead),
            "the dead row is left alone: reconciling the index with the disk is an explicit, \
             user-confirmed prune, not a retention pass's decision"
        );
        assert_eq!(store.list_clips().unwrap().len(), 2);
    }

    #[test]
    fn a_session_row_whose_file_is_gone_does_not_make_the_cap_evict_a_live_session() {
        // The sessions half of the same defect: a finished session whose concatenated file was
        // deleted in Explorer claims `size_bytes` that no longer exists. Planning over it evicts the
        // older live session (the plan's order is oldest-started first) and keeps the phantom.
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&clips_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Recent wall-clock starts: the age rule is not the one under test, and a session that
        // started in 1970 is legitimately too old to keep.
        let now = now_ms();
        let live_path = sessions_dir.join("session-live.mp4");
        std::fs::write(&live_path, vec![0u8; 1_000]).unwrap();
        let live = store
            .start_session(
                Some("R6"),
                replayed_store::SESSION_MODE_SESSION,
                now - 2_000,
                "/nowhere",
                0,
            )
            .unwrap();
        store
            .end_session(
                live,
                now - 1_500,
                Some(&live_path.display().to_string()),
                1_000,
                1_000,
            )
            .unwrap();

        let dead_path = sessions_dir.join("session-dead.mp4");
        std::fs::write(&dead_path, vec![0u8; 1_000]).unwrap();
        let dead = store
            .start_session(
                Some("R6"),
                replayed_store::SESSION_MODE_SESSION,
                now - 1_000,
                "/nowhere",
                0,
            )
            .unwrap();
        store
            .end_session(
                dead,
                now - 500,
                Some(&dead_path.display().to_string()),
                1_000,
                1_000,
            )
            .unwrap();
        std::fs::remove_file(&dead_path).unwrap();

        // A clips cap with nothing to do and a sessions cap the live session exactly fills.
        let storage = storage(&clips_dir, 1 << 30, &sessions_dir, 1_000);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);

        assert_eq!(outcome.sessions.deleted, 0);
        assert!(
            live_path.is_file(),
            "session #{live} is on disk and within the cap: it must not be evicted for a row \
             whose footage is gone"
        );
        assert_eq!(
            store.list_sessions().unwrap().len(),
            2,
            "both rows stay: the dead one is pruned on request, never by a retention pass"
        );
        assert!(store.list_sessions().unwrap().iter().any(|s| s.id == dead));
    }

    /// The two libraries are evicted independently, and a favourite survives in each.
    #[test]
    fn retention_evicts_clips_and_sessions_independently_with_favourites_immune() {
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&clips_dir).unwrap();

        // Two clips, the newer one favourited.
        let mut clip_paths = Vec::new();
        for i in 1..=2u64 {
            let path = clips_dir.join(format!("clip-{i}.mp4"));
            std::fs::write(&path, vec![0u8; 1_000]).unwrap();
            let id = index_clip(&store, &metadata(path.clone(), 1_000), i * 1_000).unwrap();
            if i == 2 {
                store.set_favourite(id, true).unwrap();
            }
            clip_paths.push(path);
        }

        // Two finished sessions with their own directories and files, the newer favourited.
        let mut session_paths = Vec::new();
        for i in 1..=2i64 {
            let scratch = sessions_dir.join(format!("session-{i}"));
            std::fs::create_dir_all(&scratch).unwrap();
            std::fs::write(scratch.join("seg-000000.mp4"), vec![0u8; 400]).unwrap();
            let final_path = sessions_dir.join(format!("session-{i}.mp4"));
            std::fs::write(&final_path, vec![0u8; 600]).unwrap();
            let id = store
                .start_session(
                    Some("Dota 2"),
                    replayed_store::SESSION_MODE_SESSION,
                    i * 1_000,
                    &scratch.display().to_string(),
                    0,
                )
                .unwrap();
            store
                .end_session(
                    id,
                    i * 1_000 + 5,
                    Some(&final_path.display().to_string()),
                    1_000,
                    0,
                )
                .unwrap();
            if i == 2 {
                store.set_session_favourite(id, true).unwrap();
            }
            session_paths.push((final_path, scratch));
        }

        // Each cap allows exactly one (the favourite) and nothing more.
        let storage = storage(&clips_dir, 1_000, &sessions_dir, 1_000);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);

        assert_eq!(store.list_clips().unwrap().len(), 1, "one clip survives");
        assert!(
            store.list_clips().unwrap()[0].favourite,
            "and it is the favourite"
        );
        assert!(clip_paths[1].is_file(), "the favourite's file is untouched");
        assert!(!clip_paths[0].exists(), "the non-favourite clip is gone");

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1, "one session survives");
        assert!(sessions[0].favourite, "and it is the favourite");
        assert!(
            session_paths[1].0.is_file(),
            "the favourite session's file is untouched"
        );
        assert!(
            session_paths[1].1.is_dir(),
            "and so is its scratch directory"
        );
        assert!(
            !session_paths[0].0.exists(),
            "the evicted session's file is gone"
        );
        assert!(
            !session_paths[0].1.exists(),
            "and so is the directory its segments lived in"
        );
        assert_eq!(store.total_session_bytes().unwrap(), 1_000);

        assert_eq!(outcome.clips.deleted, 1);
        assert_eq!(outcome.sessions.deleted, 1, "the pass reports both classes");
        assert_eq!(outcome.clips.bytes_reclaimed, 1_000);
        assert_eq!(outcome.sessions.bytes_reclaimed, 1_000);
        assert!(outcome.cap_met(), "both caps are met after the pass");
    }

    /// A cap nothing may satisfy is reported per class and nothing is deleted to try. Both causes are
    /// real: a favourited session, and a session still recording.
    #[test]
    fn an_unsatisfiable_cap_is_reported_for_each_class() {
        let (dir, store) = index_dir();
        let clips_dir = dir.path().join("clips");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&clips_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // One favourited clip of 1_000 bytes against a 100-byte cap.
        let clip_path = clips_dir.join("clip-1.mp4");
        std::fs::write(&clip_path, vec![0u8; 1_000]).unwrap();
        let clip_id = index_clip(&store, &metadata(clip_path.clone(), 1_000), 1_000).unwrap();
        store.set_favourite(clip_id, true).unwrap();

        // A running session of 2_000 bytes against a 100-byte cap.
        let scratch = sessions_dir.join("session-1");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("seg-000000.mp4"), vec![0u8; 2_000]).unwrap();
        let running = store
            .start_session(
                None,
                replayed_store::SESSION_MODE_SESSION,
                1_000,
                &scratch.display().to_string(),
                0,
            )
            .unwrap();
        store.set_session_size(running, 2_000).unwrap();

        let storage = storage(&clips_dir, 100, &sessions_dir, 100);
        let mut report = CleanupReport::default();
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);

        assert_eq!(
            outcome.deleted(),
            0,
            "nothing may be deleted to satisfy these caps"
        );
        assert!(!outcome.clips.cap_met && !outcome.sessions.cap_met);
        assert_eq!(
            report.last_clips_shortfall,
            Some(900),
            "the favourite's excess is reported"
        );
        assert_eq!(
            report.last_sessions_shortfall,
            Some(1_900),
            "the live session's is too"
        );
        assert!(clip_path.is_file(), "the favourite is untouched");
        assert!(
            scratch.is_dir(),
            "and the live session's footage is not touched"
        );
        assert!(store
            .get_session(running)
            .unwrap()
            .unwrap()
            .ended_at_ms
            .is_none());

        // The report is remembered (an identical pass does not change it); once the immunity is gone the next pass evicts both classes.
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);
        assert_eq!(outcome.deleted(), 0, "still nothing to do: {outcome:?}");
        assert_eq!(report.last_sessions_shortfall, Some(1_900));

        // The recording ends and the clip is un-favourited, so the policy now has something it may delete.
        store.end_session(running, 9_000, None, 2_000, 0).unwrap();
        store.set_favourite(clip_id, false).unwrap();
        let outcome = cleanup_pass(&store, &storage, &clips_dir, &mut report);
        assert_eq!(
            outcome.deleted(),
            2,
            "once nothing is immune, both classes shrink: {outcome:?}"
        );
        assert_eq!(outcome.clips.bytes_reclaimed, 1_000);
        assert_eq!(outcome.sessions.bytes_reclaimed, 2_000);
        assert!(
            outcome.cap_met(),
            "both caps are met after the pass: {outcome:?}"
        );
        assert_eq!(
            report.last_clips_shortfall, None,
            "the unsatisfiable condition is gone"
        );
        assert_eq!(report.last_sessions_shortfall, None);
        assert!(!clip_path.exists(), "the clip's file went with its row");
        assert!(
            !scratch.exists(),
            "and the session's scratch directory with its row"
        );
    }
}
