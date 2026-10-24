//! Reconciling the index with the filesystem: the rows that name something which is gone.
//!
//! The index is a cache and the filesystem is the truth. That rule is already stated where the
//! clips list is built (`apps/desktop/src-tauri/src/commands/clips.rs`, `clips_on_disk`), which
//! hides such a row rather than deleting it, because a file can be *temporarily* unavailable — a
//! network directory that is offline, an antivirus quarantine — and quietly rewriting a user's
//! library is not a read's business.
//!
//! This module is the other half: an explicit, user-confirmed reconciliation. Nothing here runs on
//! a read. Two things depend on it being possible at all:
//!
//! * **Retention planning.** Rows whose file is gone still count their recorded bytes
//!   (`crates/recorder/src/index.rs`, `cleanup_pass`), so a library full of phantoms looks full: the
//!   cap is reached by bytes that cannot be reclaimed, and the pass evicts live clips to pay for
//!   empty space. The recorder now plans over live files only; pruning is what stops the dead rows
//!   accumulating forever.
//! * **The library views.** A session row whose concatenated file was deleted in Explorer has no
//!   footage, and listing it is a promise the window cannot keep.
//!
//! # What is never pruned
//!
//! A session that is still recording. Its scratch directory is footage that exists nowhere else
//! yet, and [`Store::delete_session_returning_paths`] refuses it; [`Store::prune_session_row`] does
//! the same, so a hand-built plan cannot get around it. Nothing here decides a process is gone.
//!
//! Files are never unlinked here. A prune removes rows whose file the filesystem has already lost;
//! the one deletion that touches the disk (`remove_cached_thumbnails`) belongs to the shell, which
//! owns the thumbnail cache.

use crate::{Clip, Session, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::path::Path;

/// Whether `clip` names a file that is not on disk.
///
/// Symlinks are followed, as `Path::exists` does: a link to a missing target is a clip that cannot
/// be played, so every surface that shows the clip treats it as missing.
pub fn clip_is_missing(clip: &Clip) -> bool {
    !clip.path.exists()
}

/// Whether `session` names footage that is not on disk.
///
/// A session is missing only when it names a file and that file is gone:
///
/// * **Still recording** (`ended_at` NULL) is never missing. Its footage is the scratch directory,
///   closing the row is the recorder's recovery pass's job rather than a reconciliation's, and the
///   row is immune to retention for the same reason. Pruning it is refused below.
/// * **No `final_path`** is not missing either. A buffer session legitimately has no concatenated
///   file — the ring owns the footage and removes its scratch directory on a clean stop — so "no
///   file" says nothing about whether the row is a phantom. The clips and events it groups are the
///   artefacts, and those are reconciled as clips.
pub fn session_is_missing(session: &Session) -> bool {
    if session.ended_at_ms.is_none() {
        return false;
    }
    match session.final_path.as_deref() {
        Some(path) => !Path::new(path).exists(),
        None => false,
    }
}

/// The rows one prune would remove, decided from the filesystem and nothing else.
///
/// Ids only: the caller does not need the rows, and holding them would go stale before the delete.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrunePlan {
    /// Clip ids whose file is gone, in the order the store listed them.
    pub clips: Vec<i64>,
    /// Session ids whose concatenated file is gone, likewise.
    pub sessions: Vec<i64>,
}

impl PrunePlan {
    /// How many rows the plan removes, both classes.
    pub fn len(&self) -> usize {
        self.clips.len() + self.sessions.len()
    }

    /// Whether there is nothing to reconcile.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Decide what one reconcile removes from the two libraries, touching neither the database nor any
/// file.
pub fn plan_prune(clips: &[Clip], sessions: &[Session]) -> PrunePlan {
    PrunePlan {
        clips: clips
            .iter()
            .filter(|clip| clip_is_missing(clip))
            .map(|clip| clip.id)
            .collect(),
        sessions: sessions
            .iter()
            .filter(|session| session_is_missing(session))
            .map(|session| session.id)
            .collect(),
    }
}

/// What pruning one row did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneRow {
    /// This call removed the row.
    Removed,
    /// No row with that id existed: a concurrent pass, or a manual edit, got there first.
    AlreadyGone,
    /// The row is one this application must keep: a session that is still recording. Nothing was
    /// touched.
    Refused,
}

/// What one prune removed and what it refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PruneOutcome {
    /// Clip ids this call removed, in plan order.
    pub clips_removed: Vec<i64>,
    /// Session ids this call removed, in plan order.
    pub sessions_removed: Vec<i64>,
    /// Session ids the store refused because they are still recording. Must be empty for a plan
    /// [`plan_prune`] built; a non-empty one means a session started recording between the plan and
    /// the delete.
    pub sessions_refused: Vec<i64>,
    /// Plan entries that named a row which no longer existed.
    pub rows_already_gone: usize,
    /// Rows whose delete failed outright. Reported per row rather than ending the pass, as the
    /// retention executor does.
    pub failed: usize,
}

impl PruneOutcome {
    /// How many rows this prune removed.
    pub fn removed(&self) -> usize {
        self.clips_removed.len() + self.sessions_removed.len()
    }
}

/// Apply a prune plan.
///
/// One row at a time, each in its own transaction (the store's methods do that): a row that cannot
/// be deleted is counted in [`PruneOutcome::failed`] and does not stop the rest. Errors are per row
/// by design — the user asked for a reconciliation, and one locked row is not a reason to leave the
/// other nine phantoms in place.
pub fn execute_prune(store: &Store, plan: &PrunePlan) -> Result<PruneOutcome> {
    let mut outcome = PruneOutcome::default();

    for &id in &plan.clips {
        match store.prune_clip_row(id) {
            Ok(PruneRow::Removed) => outcome.clips_removed.push(id),
            Ok(PruneRow::AlreadyGone) => outcome.rows_already_gone += 1,
            Ok(PruneRow::Refused) => {
                // A clip row has nothing to refuse; the store does not return this for one.
                outcome.failed += 1
            }
            Err(err) => {
                tracing::warn!("prune: could not remove the row of clip #{id}: {err:#}");
                outcome.failed += 1;
            }
        }
    }

    for &id in &plan.sessions {
        match store.prune_session_row(id) {
            Ok(PruneRow::Removed) => outcome.sessions_removed.push(id),
            Ok(PruneRow::AlreadyGone) => outcome.rows_already_gone += 1,
            Ok(PruneRow::Refused) => outcome.sessions_refused.push(id),
            Err(err) => {
                tracing::warn!("prune: could not remove the row of session #{id}: {err:#}");
                outcome.failed += 1;
            }
        }
    }

    Ok(outcome)
}

impl Store {
    /// Delete a clip row whose file is gone, together with the tags and events that point at it.
    ///
    /// Returns whether this call removed the row, so the caller reports what it did rather than what
    /// it asked for. The row is removed whatever it names: deciding that its file is gone is the
    /// caller's job ([`plan_prune`], which stats the file), and a store that re-checked would still
    /// race the filesystem between the check and the delete.
    ///
    /// # Why the events go too
    ///
    /// [`Store::delete_clip_returning_path`] *keeps* the events and clears `clip_id`, because a
    /// retention pass must not destroy the history of a clip simply because the clip aged out;
    /// `clip_id` NULL already means "a marker", so the row stays readable. A prune is the opposite
    /// case: the event describes a moment in footage that never existed (or no longer does), and a
    /// prune is the user saying "remove these phantoms". Keeping them would leave every later
    /// timeline showing markers of a clip nobody can open, which is the same disagreement between
    /// index and disk in another table.
    ///
    /// The whole delete is one transaction: a crash cannot leave a tag or an event pointing at a
    /// clip row that is already gone.
    pub fn prune_clip_row(&self, id: i64) -> Result<PruneRow> {
        let exists: Option<i64> = self
            .conn
            .query_row("SELECT id FROM clips WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Ok(PruneRow::AlreadyGone);
        }
        let tx = self.conn.unchecked_transaction()?;
        // Events first: `events.clip_id` is a foreign key, and the clip they name is going.
        tx.execute("DELETE FROM events WHERE clip_id = ?1", params![id])?;
        tx.execute("DELETE FROM clip_tags WHERE clip_id = ?1", params![id])?;
        let removed = tx.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(if removed == 1 {
            PruneRow::Removed
        } else {
            PruneRow::AlreadyGone
        })
    }

    /// Delete a session row whose footage is gone, with the events that point at it.
    ///
    /// The same row-before-file reasoning as [`Store::delete_session_returning_paths`], except that
    /// there is no file to remove: the footage is already gone, which is why the row is a phantom.
    /// Clips extracted from the session survive as first-class artefacts in the clips directory
    /// (their `session_id` is cleared), exactly as a normal session delete leaves them.
    ///
    /// A session that is still recording is [`PruneRow::Refused`] and nothing is touched. The check
    /// is in the same transaction as the delete, so a session that starts or stops between them
    /// cannot slip through.
    ///
    /// The row is removed whatever it names, for the reason [`Store::prune_clip_row`] gives.
    pub fn prune_session_row(&self, id: i64) -> Result<PruneRow> {
        let tx = self.conn.unchecked_transaction()?;
        // One no-op write first, and the reason is the transaction, not the value: it acquires the
        // database's write lock, so from here until the commit nothing else can end, start or
        // delete this row. The rowcount is what the check below reads: 1 means the row exists and
        // is finalised, 0 means it is gone or still recording. A `SELECT` first would not do —
        // reading does not take the lock, and a session that started recording between the read
        // and the event delete would lose its timeline on the way to being refused.
        let finalised = tx.execute(
            "UPDATE sessions SET ended_at = ended_at WHERE id = ?1 AND ended_at IS NOT NULL",
            params![id],
        )?;
        if finalised == 0 {
            let exists: Option<i64> = tx
                .query_row("SELECT id FROM sessions WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            // Dropping the transaction rolls it back; the update changed no value either way.
            return Ok(if exists.is_none() {
                PruneRow::AlreadyGone
            } else {
                PruneRow::Refused
            });
        }
        // The row is finalised and this call holds the write lock: its events go with it, and the
        // clips cut out of it survive as first-class artefacts (their `session_id` cleared, as a
        // normal session delete leaves them).
        tx.execute("DELETE FROM events WHERE session_id = ?1", params![id])?;
        tx.execute(
            "UPDATE clips SET session_id = NULL WHERE session_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(PruneRow::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewClip, NewEvent, SESSION_MODE_BUFFER, SESSION_MODE_SESSION};
    use std::path::PathBuf;

    /// A directory that exists for the length of the test, and the clips inside it.
    struct Temp {
        dir: tempfile::TempDir,
    }

    impl Temp {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("a temporary directory"),
            }
        }

        /// `name` in the temporary directory, written so it exists.
        fn file(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, b"x").unwrap();
            path
        }

        /// `name` in the temporary directory, never written: the phantom's path.
        fn gone(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
    }

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.migrate().unwrap();
        store
    }

    fn clip(path: PathBuf) -> NewClip {
        NewClip {
            path,
            started_at_ms: 0,
            duration_ms: 1_000,
            size_bytes: 4,
            codec: "mpeg4".to_string(),
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_clip_whose_file_is_there_is_not_missing_and_one_whose_file_is_not_is() {
        let temp = Temp::new();
        let store = store();
        let present = store.insert_clip(&clip(temp.file("here.mp4"))).unwrap();
        let gone = store.insert_clip(&clip(temp.gone("gone.mp4"))).unwrap();

        let plan = plan_prune(&store.list_clips().unwrap(), &[]);
        assert_eq!(plan.clips, vec![gone]);
        assert!(!plan.clips.contains(&present));
    }

    #[test]
    fn a_pruned_clip_takes_its_tags_and_its_events_with_it() {
        // The three tables a clip is spread across: the row, its tags, and the events that named it.
        let temp = Temp::new();
        let store = store();
        let id = store.insert_clip(&clip(temp.gone("gone.mp4"))).unwrap();
        store.set_clip_tags(id, &["ace".into()]).unwrap();
        store
            .insert_event(&NewEvent {
                session_id: None,
                kind: "kill".into(),
                at_ms: 5_000,
                payload: None,
                clip_id: Some(id),
            })
            .unwrap();
        // A marker (an event with no clip) is not this clip's and must survive.
        let marker = store
            .insert_event(&NewEvent {
                session_id: None,
                kind: "game_start".into(),
                at_ms: 1,
                payload: None,
                clip_id: None,
            })
            .unwrap();

        assert_eq!(store.prune_clip_row(id).unwrap(), PruneRow::Removed);

        assert_eq!(count(&store, "SELECT COUNT(*) FROM clips"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM clip_tags"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM events"), 1);
        assert_eq!(store.list_events().unwrap()[0].id, marker);
    }

    #[test]
    fn pruning_a_clip_that_is_already_gone_touches_nothing() {
        let store = store();
        assert_eq!(store.prune_clip_row(77).unwrap(), PruneRow::AlreadyGone);
    }

    #[test]
    fn a_session_whose_file_is_gone_is_missing_but_a_buffer_session_with_no_file_is_not() {
        // The distinction the plan rests on: "no file" is a legitimate state for a buffer session
        // and says nothing about whether the row is a phantom.
        let temp = Temp::new();
        let store = store();

        let buffer = store
            .start_session(Some("Dota 2"), SESSION_MODE_BUFFER, 10, "/nowhere", 0)
            .unwrap();
        store.end_session(buffer, 20, None, 0, 0).unwrap();

        let gone = store
            .start_session(Some("Dota 2"), SESSION_MODE_SESSION, 30, "/nowhere", 0)
            .unwrap();
        store
            .end_session(
                gone,
                40,
                Some(&temp.gone("session-1.mp4").to_string_lossy()),
                100,
                1_000,
            )
            .unwrap();

        let sessions = store.list_sessions().unwrap();
        let plan = plan_prune(&[], &sessions);
        assert_eq!(plan.sessions, vec![gone]);
        assert!(!plan.sessions.contains(&buffer));
    }

    #[test]
    fn a_session_whose_file_is_present_is_not_missing() {
        let temp = Temp::new();
        let store = store();
        let path = temp.file("session-2.mp4");
        let id = store
            .start_session(Some("Dota 2"), SESSION_MODE_SESSION, 30, "/nowhere", 0)
            .unwrap();
        store
            .end_session(id, 40, Some(&path.to_string_lossy()), 100, 1_000)
            .unwrap();

        assert!(plan_prune(&[], &store.list_sessions().unwrap()).is_empty());
    }

    #[test]
    fn pruning_a_session_removes_its_events_and_detaches_its_clips() {
        let temp = Temp::new();
        let store = store();
        let id = store
            .start_session(Some("Dota 2"), SESSION_MODE_SESSION, 30, "/nowhere", 0)
            .unwrap();
        store
            .end_session(
                id,
                40,
                Some(&temp.gone("session-3.mp4").to_string_lossy()),
                100,
                1_000,
            )
            .unwrap();
        // A clip cut out of the session: its own file, its own row, and a link back to the session.
        let clip_id = store.insert_clip(&clip(temp.file("cut.mp4"))).unwrap();
        store
            .insert_event(&NewEvent {
                session_id: Some(id),
                kind: "kill".into(),
                at_ms: 5_000,
                payload: None,
                clip_id: Some(clip_id),
            })
            .unwrap();

        assert_eq!(store.prune_session_row(id).unwrap(), PruneRow::Removed);

        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM events"), 0);
        let clip = store.list_clips().unwrap();
        assert_eq!(clip.len(), 1, "the extracted clip survives the session row");
        assert_eq!(clip[0].id, clip_id);
    }

    #[test]
    fn a_session_still_recording_is_refused_and_nothing_is_touched() {
        let store = store();
        let id = store
            .start_session(Some("Dota 2"), SESSION_MODE_SESSION, 30, "/nowhere", 0)
            .unwrap();
        store
            .insert_event(&NewEvent {
                session_id: Some(id),
                kind: "game_start".into(),
                at_ms: 0,
                payload: None,
                clip_id: None,
            })
            .unwrap();

        assert_eq!(store.prune_session_row(id).unwrap(), PruneRow::Refused);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM events"), 1);
        // And no plan would ever select it, which is the other half of the guarantee.
        assert!(plan_prune(&[], &store.list_sessions().unwrap()).is_empty());
    }

    #[test]
    fn execute_prune_removes_every_planned_row_and_reports_what_it_did() {
        let temp = Temp::new();
        let store = store();
        let dead_clip = store.insert_clip(&clip(temp.gone("gone.mp4"))).unwrap();
        let live_clip = store.insert_clip(&clip(temp.file("here.mp4"))).unwrap();
        let dead_session = store
            .start_session(Some("R6"), SESSION_MODE_SESSION, 30, "/nowhere", 0)
            .unwrap();
        store
            .end_session(
                dead_session,
                40,
                Some(&temp.gone("session-4.mp4").to_string_lossy()),
                100,
                1_000,
            )
            .unwrap();

        let plan = plan_prune(
            &store.list_clips().unwrap(),
            &store.list_sessions().unwrap(),
        );
        let outcome = execute_prune(&store, &plan).unwrap();

        assert_eq!(outcome.clips_removed, vec![dead_clip]);
        assert_eq!(outcome.sessions_removed, vec![dead_session]);
        assert_eq!(outcome.removed(), 2);
        assert!(outcome.sessions_refused.is_empty());
        assert_eq!(outcome.failed, 0);
        assert_eq!(store.list_clips().unwrap()[0].id, live_clip);
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn execute_prune_reports_a_row_that_a_concurrent_pass_already_removed() {
        let temp = Temp::new();
        let store = store();
        let id = store.insert_clip(&clip(temp.gone("gone.mp4"))).unwrap();
        let plan = plan_prune(&store.list_clips().unwrap(), &[]);
        // The row goes between the plan and the prune.
        store.prune_clip_row(id).unwrap();

        let outcome = execute_prune(&store, &plan).unwrap();
        assert!(outcome.clips_removed.is_empty());
        assert_eq!(outcome.rows_already_gone, 1);
    }

    #[test]
    fn execute_prune_refuses_a_session_that_started_recording_after_the_plan() {
        // The hazard the refusal exists for: a plan cannot be trusted to still be true when it is
        // applied, and the one row that must never be deleted is a running session's.
        let temp = Temp::new();
        let store = store();
        let id = store
            .start_session(Some("R6"), SESSION_MODE_SESSION, 30, "/nowhere", 0)
            .unwrap();
        store
            .end_session(
                id,
                40,
                Some(&temp.gone("session-5.mp4").to_string_lossy()),
                100,
                1_000,
            )
            .unwrap();
        store
            .insert_event(&NewEvent {
                session_id: Some(id),
                kind: "game_start".into(),
                at_ms: 0,
                payload: None,
                clip_id: None,
            })
            .unwrap();
        let plan = plan_prune(&[], &store.list_sessions().unwrap());
        // It starts recording again (a re-used id, a re-opened row: whatever the cause, the row is
        // now live footage).
        store
            .conn
            .execute("UPDATE sessions SET ended_at = NULL WHERE id = ?1", [id])
            .unwrap();

        let outcome = execute_prune(&store, &plan).unwrap();
        assert!(outcome.sessions_removed.is_empty());
        assert_eq!(outcome.sessions_refused, vec![id]);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM events"),
            1,
            "a refused session keeps its timeline: the refusal is decided under the same write \
             lock as the delete, so nothing of it was touched"
        );
    }
}
