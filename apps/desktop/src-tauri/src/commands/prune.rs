//! The user-confirmed reconciliation of the two libraries with the disk.
//!
//! A row whose file is gone is a phantom: the library filters it out of every list, the storage
//! figures leave it out, and retention plans over files on disk only — but the row itself stays,
//! because a file can be *temporarily* unavailable (a network directory that is offline, an
//! antivirus quarantine) and a read is not allowed to rewrite a user's library. Removing it is a
//! decision, and this is where the user makes it.
//!
//! Two things make this safety-critical rather than bookkeeping:
//!
//! * **Reachability.** A clips directory on an unmounted share looks exactly like a library of
//!   deleted files. [`require_reachable`] refuses the whole command in that case, and a session is
//!   only a candidate when the directory its file lived in is still there (a per-session
//!   `sessions_dir` can be anywhere, so it gets its own check).
//! * **A running session is never removed.** Its scratch directory is footage that exists nowhere
//!   else; the store refuses the row delete ([`replayed_store::prune::PruneRow::Refused`]) even if a
//!   plan somehow named it.

use super::clips::remove_cached_thumbnails;
use super::*;
use replayed_store::prune::{execute_prune, plan_prune, PrunePlan};
use replayed_store::{Clip, Session};
use std::path::Path;

/// Remove every index row that names a file which is gone, and report what went.
///
/// The window calls this only after the user confirms it, from the Storage panel, and it removes
/// index rows only: the files are already gone, and nothing here unlinks anything. The one disk
/// write is the thumbnail cache, which belongs to this shell.
///
/// A row is a candidate only when the place its file lived can be read (see the module docs). When
/// the clips directory itself cannot be read the command fails with [`ErrorCode::Io`] and removes
/// nothing; when one session's own directory cannot be read, that session is left alone and the
/// rest of the pass runs.
pub fn prune_missing(deps: &Deps<'_>) -> Result<PruneOutcome, CommandError> {
    require_reachable(&deps.paths.clips_dir)?;

    let clips = deps
        .store
        .list_clips()
        .map_err(|e| CommandError::store("listing clips", e))?;
    let sessions = deps
        .store
        .list_sessions()
        .map_err(|e| CommandError::store("listing the sessions", e))?;

    let plan = only_reachable(plan_prune(&clips, &sessions), &clips, &sessions);

    let outcome = execute_prune(deps.store, &plan)
        .map_err(|e| CommandError::store("removing the stale rows", e))?;

    let thumbnails_removed: usize = outcome
        .clips_removed
        .iter()
        .map(|&id| remove_cached_thumbnails(&deps.paths.thumbnails_dir, id))
        .sum();

    tracing::info!(
        "prune: removed {} clip row(s) and {} session row(s) whose files are gone, and {} cached \
         thumbnail(s); {} row(s) were already gone, {} refused (still recording), {} failed",
        outcome.clips_removed.len(),
        outcome.sessions_removed.len(),
        thumbnails_removed,
        outcome.rows_already_gone,
        outcome.sessions_refused.len(),
        outcome.failed
    );

    Ok(PruneOutcome {
        clips_removed: outcome.clips_removed.len(),
        sessions_removed: outcome.sessions_removed.len(),
        thumbnails_removed,
        sessions_refused: outcome.sessions_refused.len(),
        rows_already_gone: outcome.rows_already_gone,
        failed: outcome.failed,
    })
}

/// How many rows name a file that is gone, for the storage panel's offer.
///
/// A pure read: it stats files and touches nothing, so it may run on every refresh (which it does,
/// as part of [`storage_stats`]). The separate `missing_sessions` field exists because a session
/// row has no entry in the clip figures, so one count could not stand for both.
pub fn missing_sessions(deps: &Deps<'_>) -> Result<usize, CommandError> {
    let sessions = deps
        .store
        .list_sessions()
        .map_err(|e| CommandError::store("listing the sessions", e))?;
    Ok(sessions
        .iter()
        .filter(|s| replayed_store::prune::session_is_missing(s))
        .count())
}

/// Refuse to reconcile when the clips directory itself cannot be read.
///
/// The load-bearing guard of the whole operation: with the directory unreadable (an unmounted
/// share, a stale network handle, a removed drive) every clip in the index fails `exists`, and a
/// prune would empty the library of rows — and the files, on the other machine, would still be
/// there with nothing left naming them.
///
/// A directory that exists but is empty is fine: a clip deleted when it was the only one leaves a
/// readable, empty clips directory, which is exactly a library to reconcile.
fn require_reachable(dir: &Path) -> Result<(), CommandError> {
    match std::fs::read_dir(dir) {
        // The handle is opened and the first entry (if any) read: a listing that fails halfway is
        // an unreadable directory too.
        Ok(mut entries) => match entries.next() {
            Some(Ok(_)) | None => Ok(()),
            Some(Err(err)) => Err(CommandError::new(
                ErrorCode::Io,
                format!(
                    "the clips directory {} cannot be read to the end ({err}); nothing was \
                     removed — a file that is missing because its directory is unavailable is \
                     not a file the user deleted",
                    dir.display()
                ),
            )),
        },
        Err(err) => Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "the clips directory {} cannot be read ({err}); nothing was removed — an \
                 offline or unmounted directory looks exactly like a library whose clips were \
                 all deleted, and reconciling it would drop rows whose files are still there",
                dir.display()
            ),
        )),
    }
}

/// Drop the candidates whose own directory cannot be read.
///
/// [`require_reachable`] covers the clips directory itself, which is where every clip of a default
/// layout lives. That is not quite every row: a changed `[storage] clips_dir` leaves rows pointing
/// into the old one, and a session's concatenated file can live anywhere
/// (`[storage.sessions] sessions_dir`). Each candidate is therefore checked where it is — when the
/// directory holding the file is not a directory, the file being absent is not evidence of a
/// deletion, and the row waits for the volume to come back.
fn only_reachable(plan: PrunePlan, clips: &[Clip], sessions: &[Session]) -> PrunePlan {
    let clip_reachable = |id: &i64| {
        clips
            .iter()
            .find(|clip| clip.id == *id)
            .is_some_and(|clip| directory_reachable(&clip.path))
    };
    let session_reachable = |id: &i64| {
        sessions
            .iter()
            .find(|session| session.id == *id)
            .is_some_and(|session| {
                session
                    .final_path
                    .as_deref()
                    .is_some_and(directory_reachable)
            })
    };
    PrunePlan {
        clips: plan.clips.into_iter().filter(clip_reachable).collect(),
        sessions: plan
            .sessions
            .into_iter()
            .filter(session_reachable)
            .collect(),
    }
}

/// Whether the directory holding `path` can be read, as a directory.
///
/// `Path::parent` of a bare file name is the empty path, and of `/x/y.mp4` is `/x`: the second is
/// the one this asks about, and the first is never a session or clip row the recorder writes.
fn directory_reachable(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty() || parent.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;
    use replayed_store::NewEvent;

    /// A clip row whose file is on disk, deleted afterwards to make the phantom.
    fn add_dead_row(f: &Fixture, name: &str) -> i64 {
        let id = f.add_row(name, 1_000, 2_000, 3_000);
        std::fs::remove_file(f.paths.clips_dir.join(name)).unwrap();
        id
    }

    #[test]
    fn a_prune_removes_only_the_rows_whose_files_are_gone() {
        let f = Fixture::new();
        let live = f.add_row("live.mp4", 1_000, 2_000, 3_000);
        let dead = add_dead_row(&f, "dead.mp4");

        let outcome = prune_missing(&f.deps()).unwrap();

        assert_eq!(outcome.clips_removed, 1);
        assert_eq!(outcome.sessions_removed, 0);
        assert_eq!(outcome.failed, 0);
        let left: Vec<i64> = f.store.list_clips().unwrap().iter().map(|c| c.id).collect();
        assert_eq!(left, vec![live], "the clip on disk is untouched");
        assert!(!left.contains(&dead));
    }

    #[test]
    fn a_prune_reports_nothing_to_do_as_zero_rather_than_failing() {
        let f = Fixture::new();
        f.add_row("live.mp4", 1_000, 2_000, 3_000);

        let outcome = prune_missing(&f.deps()).unwrap();
        assert_eq!(outcome.clips_removed, 0);
        assert_eq!(outcome.sessions_removed, 0);
        assert_eq!(outcome.rows_already_gone, 0);
    }

    #[test]
    fn a_prune_refuses_an_unreachable_clips_directory_and_removes_nothing() {
        // The failure mode the guard exists for: every `exists` fails, so without the check this
        // would delete every row in the library.
        let f = Fixture::new();
        let id = f.add_row("live.mp4", 1_000, 2_000, 3_000);
        // The directory the shell resolved is gone (an unmounted share, a removed drive).
        std::fs::remove_dir_all(&f.paths.clips_dir).unwrap();

        let err = prune_missing(&f.deps()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(
            err.message.contains("cannot be read"),
            "the refusal names the reason: {}",
            err.message
        );
        assert_eq!(
            f.store.list_clips().unwrap()[0].id,
            id,
            "no row was removed"
        );
    }

    #[test]
    fn a_prune_removes_a_session_row_whose_file_is_gone_and_its_events() {
        let f = Fixture::new();
        let (id, path) = f.add_finished_session(1, 0);
        f.store
            .insert_event(&NewEvent {
                session_id: Some(id),
                kind: "kill".into(),
                at_ms: 1_000,
                payload: None,
                clip_id: None,
            })
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        let outcome = prune_missing(&f.deps()).unwrap();

        assert_eq!(outcome.sessions_removed, 1);
        assert!(f.store.list_sessions().unwrap().is_empty());
        assert!(
            f.store.list_events().unwrap().is_empty(),
            "the phantom session's timeline goes with it"
        );
    }

    #[test]
    fn a_prune_leaves_a_session_alone_when_its_directory_cannot_be_read() {
        // A session store on a share that is not mounted: the file is not there, but neither is
        // the place it lived, so the row is not evidence of a deletion.
        let f = Fixture::new();
        let (id, path) = f.add_finished_session(1, 0);
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(&dir).unwrap();

        let outcome = prune_missing(&f.deps()).unwrap();

        assert_eq!(outcome.sessions_removed, 0);
        assert_eq!(
            f.store.list_sessions().unwrap()[0].id,
            id,
            "the row is left for a run when the directory is back"
        );
    }

    #[test]
    fn a_prune_leaves_a_clip_alone_when_the_directory_it_lived_in_cannot_be_read() {
        // A row can outlive its directory: a changed `[storage] clips_dir` leaves rows naming the
        // old place, and a clip in a subdirectory of a share can outlive the share being unmounted.
        // The command's clips-directory guard does not cover that, so the row is checked where it
        // is and left alone.
        let f = Fixture::new();
        let old_dir = f.paths.clips_dir.parent().unwrap().join("old-clips");
        std::fs::create_dir_all(&old_dir).unwrap();
        let path = old_dir.join("clip-old.mp4");
        std::fs::write(&path, b"x").unwrap();
        let id = f
            .store
            .insert_clip(&replayed_store::NewClip {
                path: path.clone(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: 1,
                codec: "mpeg4".into(),
            })
            .unwrap();
        // The volume goes away: the file is not there, and neither is the place it lived.
        std::fs::remove_dir_all(&old_dir).unwrap();

        let outcome = prune_missing(&f.deps()).unwrap();

        assert_eq!(outcome.clips_removed, 0);
        assert_eq!(
            f.store.list_clips().unwrap()[0].id,
            id,
            "the row waits for the directory to come back"
        );
    }

    #[test]
    fn a_prune_removes_the_cached_thumbnails_of_the_clips_it_forgets() {
        let f = Fixture::new();
        let id = add_dead_row(&f, "dead.mp4");
        std::fs::create_dir_all(&f.paths.thumbnails_dir).unwrap();
        let thumb = f.paths.thumbnails_dir.join(thumbnail_file_name(id, 0));
        std::fs::write(&thumb, b"jpeg").unwrap();

        let outcome = prune_missing(&f.deps()).unwrap();

        assert_eq!(outcome.thumbnails_removed, 1);
        assert!(!thumb.exists(), "the stale JPEG goes with the row");
    }
}
