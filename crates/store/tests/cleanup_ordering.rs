//! The cleanup executor against a real database and real files.
//!
//! One property, stated as a safety requirement: the database row is deleted and committed before
//! the file is unlinked, and no file is deleted unless a committed row names it.
//!
//! The plan is pure and covered in `crates/store/src/cleanup.rs`. Here a real [`Store`], clips
//! directory and unlinks show what the executor does, including the two ways the filesystem can
//! disagree with the index (a missing file, a failing unlink) and the case that must delete
//! nothing (a plan id whose row is already gone).

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use replayed_store::cleanup::{execute_cleanup, plan_cleanup, CleanupPolicy};
use replayed_store::{LibraryRoots, NewClip, Store};
use std::path::PathBuf;

/// A clips directory plus the database that indexes it, in a temp dir that outlives the test body.
struct Fixture {
    _dir: tempfile::TempDir,
    clips_dir: PathBuf,
    db_path: PathBuf,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clips_dir = dir.path().join("clips");
        std::fs::create_dir_all(&clips_dir).expect("create the clips dir");
        let db_path = dir.path().join("replayed.db");
        let store = Store::open(&db_path).expect("open the clip index");
        store.migrate().expect("migrate the clip index");
        Self {
            _dir: dir,
            clips_dir,
            db_path,
            store,
        }
    }

    /// Write a clip file with `bytes` of content, index it, and return its row id and path.
    fn add_clip(&self, name: &str, bytes: usize) -> (i64, PathBuf) {
        let path = self.clips_dir.join(name);
        std::fs::write(&path, vec![0u8; bytes]).expect("write the clip file");
        let id = self
            .store
            .insert_clip(&NewClip {
                path: path.clone(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: bytes as u64,
                codec: "h264_nvenc".into(),
            })
            .expect("index the clip");
        (id, path)
    }

    /// Index a path without writing anything at it.
    fn index_only(&self, path: PathBuf, bytes: u64) -> i64 {
        self.store
            .insert_clip(&NewClip {
                path,
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: bytes,
                codec: "h264_nvenc".into(),
            })
            .expect("index the clip")
    }

    fn ids(&self) -> Vec<i64> {
        self.store
            .list_clips()
            .expect("list")
            .iter()
            .map(|c| c.id)
            .collect()
    }

    /// Backdate a row's creation time, as an operator would with `sqlite3`.
    ///
    /// The store stamps `created_at` itself and has no API to backdate it, so an aged clip is made by
    /// reaching the column directly. The clock is the only thing faked.
    fn backdate(&self, id: i64, created_at_ms: i64) {
        let conn = rusqlite::Connection::open(&self.db_path).expect("a second connection");
        conn.execute(
            "UPDATE clips SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![id, created_at_ms],
        )
        .expect("backdate the row");
    }

    /// The containment roots the application would hand the executor: this clips directory and a
    /// sessions directory beside it.
    fn roots(&self) -> LibraryRoots {
        LibraryRoots::new(&self.clips_dir, self._dir.path().join("sessions"))
    }

    /// Plan against the store's own rows, as the application does.
    fn plan(&self, policy: &CleanupPolicy) -> replayed_store::cleanup::CleanupPlan {
        plan_cleanup(&self.store.list_clips().expect("list"), policy, now_ms())
    }
}

fn policy(max_total_bytes: u64, max_age_days: u64) -> CleanupPolicy {
    CleanupPolicy {
        max_total_bytes,
        max_age_days,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A file nobody indexes, in the clips directory the executor is working in.
fn stray_file(clips_dir: &std::path::Path) -> PathBuf {
    let stray = clips_dir.join("unindexed.bin");
    std::fs::write(&stray, b"not a clip, not indexed").expect("write the stray file");
    stray
}

#[test]
fn rows_and_files_are_deleted_and_a_favourite_is_left_alone() {
    let fx = Fixture::new();
    let (a, a_path) = fx.add_clip("a.mp4", 100); // created first: evicted first
    let (b, b_path) = fx.add_clip("b.mp4", 100);
    let (fav, fav_path) = fx.add_clip("fav.mp4", 300);
    assert!(fx
        .store
        .set_favourite(fav, true)
        .expect("favourite the clip"));
    let stray = stray_file(&fx.clips_dir);
    for path in [&a_path, &b_path, &fav_path, &stray] {
        assert!(path.is_file(), "fixture: {} must exist", path.display());
    }

    // 500 bytes over a 300 byte cap; the favourite alone accounts for the whole cap.
    let plan = fx.plan(&policy(300, 3_650));
    assert_eq!(plan.ids(), vec![a, b], "the two oldest non-favourites");
    assert!(
        plan.cap_met(),
        "the favourites are inside the cap, so it is reachable"
    );
    assert_eq!(plan.bytes_after, 300);

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 2);
    assert_eq!(outcome.failed, 0);
    assert_eq!(outcome.bytes_reclaimed, 200);
    assert!(
        outcome.orphaned.is_empty(),
        "both files were where the rows said"
    );
    assert!(outcome.already_missing.is_empty());
    assert_eq!(
        outcome.bytes_after, 300,
        "re-read from the store after the deletes"
    );
    assert!(outcome.cap_met);

    assert_eq!(fx.ids(), vec![fav], "the rows are gone");
    assert!(!a_path.exists(), "{} must be unlinked", a_path.display());
    assert!(!b_path.exists(), "{} must be unlinked", b_path.display());
    assert!(
        fav_path.exists(),
        "a favourite is never deleted, file included"
    );
    assert!(
        stray.exists(),
        "the executor only ever unlinks paths a deleted row named"
    );
}

#[test]
fn an_aged_clip_is_deleted_and_a_recent_one_is_kept() {
    let fx = Fixture::new();
    let (old, old_path) = fx.add_clip("old.mp4", 100);
    let (recent, recent_path) = fx.add_clip("recent.mp4", 100);
    fx.backdate(old, now_ms() - 30 * 86_400_000);

    // The age rule alone: the cap is not in play at all.
    let plan = fx.plan(&policy(u64::MAX, 7));
    assert_eq!(plan.ids(), vec![old], "only the aged clip");
    assert!(plan.deletions[0].reasons.too_old);

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 1);
    assert_eq!(outcome.bytes_reclaimed, 100);
    assert!(outcome.cap_met);
    assert_eq!(fx.ids(), vec![recent], "the recent clip's row is untouched");
    assert!(
        !old_path.exists(),
        "the aged clip's file is gone with its row"
    );
    assert!(recent_path.exists(), "the recent clip's file is untouched");
}

#[test]
fn a_row_whose_file_is_already_gone_is_deleted_and_reported() {
    let fx = Fixture::new();
    let missing = fx.clips_dir.join("missing.mp4");
    let id = fx.index_only(missing.clone(), 100);
    let stray = stray_file(&fx.clips_dir);
    assert!(!missing.exists());

    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id]);

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(
        outcome.deleted, 1,
        "the row is deleted even though the file is not there"
    );
    assert_eq!(
        outcome.already_missing,
        vec![missing.clone()],
        "a row pointing at nothing is reported, not swallowed"
    );
    assert!(
        outcome.orphaned.is_empty(),
        "there is no file on disk to orphan"
    );
    assert_eq!(
        outcome.bytes_reclaimed, 0,
        "nothing was reclaimed: nothing was removed"
    );
    assert!(fx.ids().is_empty(), "no rows are left");
    assert!(
        stray.exists(),
        "the unlink never moved away from the row's own path"
    );
}

#[test]
fn an_unlink_that_fails_is_reported_as_an_orphan_and_nothing_else_is_touched() {
    let fx = Fixture::new();
    let (fav, fav_path) = fx.add_clip("fav.mp4", 100);
    assert!(fx
        .store
        .set_favourite(fav, true)
        .expect("favourite the clip"));
    let stray = stray_file(&fx.clips_dir);

    // A path a row names that `remove_file` cannot remove: a directory, as one named `*.mp4` would
    // be if a user or half-finished copy put it there.
    let doomed = fx.clips_dir.join("doomed.mp4");
    std::fs::create_dir_all(&doomed).expect("create the directory");
    std::fs::write(doomed.join("inside.bin"), b"still here").expect("write into it");
    let id = fx.index_only(doomed.clone(), 100);

    // 200 bytes over a 100 byte cap, and the favourite alone is exactly the cap, so the size rule is
    // satisfiable and the doomed clip is what it evicts.
    let plan = fx.plan(&policy(100, 3_650));
    assert_eq!(plan.ids(), vec![id]);

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(
        outcome.deleted, 1,
        "the row is deleted first, and it stays deleted"
    );
    assert_eq!(
        outcome.orphaned,
        vec![doomed.clone()],
        "the unlink that failed is reported with its path"
    );
    assert!(outcome.already_missing.is_empty());
    assert_eq!(outcome.bytes_reclaimed, 0);
    assert!(
        doomed.is_dir(),
        "the file the unlink could not remove is still there"
    );
    assert!(
        doomed.join("inside.bin").exists(),
        "and the executor did not reach inside it"
    );
    assert!(
        fav_path.exists(),
        "the favourite is untouched — it was not in the plan"
    );
    assert!(
        stray.exists(),
        "and neither is anything the index does not name"
    );
    assert_eq!(fx.ids(), vec![fav], "only the doomed clip's row is gone");
}

#[test]
fn a_relative_or_dotdot_path_in_a_row_is_never_unlinked() {
    let fx = Fixture::new();
    // A file reachable from the working directory by a relative path, and by a `..` path from an
    // absolute one. Rows naming them lose the row and never the file.
    let cwd = std::env::current_dir().expect("a working directory");
    let probe = tempfile::NamedTempFile::new_in(&cwd).expect("a file beside the cwd");
    let relative = PathBuf::from(probe.path().file_name().expect("a name"));
    let dotdot = fx.clips_dir.join("..").join("..").join("never-created.mp4");
    let a = fx.index_only(relative.clone(), 100);
    let b = fx.index_only(dotdot.clone(), 100);

    let plan = fx.plan(&policy(1, 3_650));
    assert!(plan.ids().contains(&a) && plan.ids().contains(&b));
    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 2, "the rows go");
    assert!(outcome.refused.contains(&relative) && outcome.refused.contains(&dotdot));
    assert!(
        outcome.orphaned.is_empty(),
        "refused is not a failure to retry"
    );
    assert_eq!(outcome.bytes_reclaimed, 0);
    assert!(
        probe.path().exists(),
        "the file the relative path would have hit is untouched"
    );
}

#[test]
fn a_plan_id_whose_row_is_already_gone_deletes_nothing() {
    let fx = Fixture::new();
    let (id, path) = fx.add_clip("keep.mp4", 100);
    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id]);

    // The row disappears between planning and executing (another cleanup pass, a second instance, a
    // hand-edited index). With no committed row delete there is no authority to unlink anything.
    let removed = fx
        .store
        .delete_clip_returning_path(id)
        .expect("delete the row");
    assert_eq!(removed.map(PathBuf::from), Some(path.clone()));

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.bytes_reclaimed, 0);
    assert!(
        path.exists(),
        "the file is still there: no row named it any more"
    );
    assert!(outcome.orphaned.is_empty());
}

/// The race the executor used to lose: the plan is made from a snapshot, a multi-gigabyte unlink
/// per deletion takes seconds, and in that window the user stars a clip that was about to age out.
/// The favourite is the one protection the policy promises, so it must win.
#[test]
fn a_clip_favourited_after_the_plan_was_made_survives_the_pass() {
    let fx = Fixture::new();
    let (id, path) = fx.add_clip("starred-late.mp4", 100);
    let (other, other_path) = fx.add_clip("other.mp4", 100);
    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id, other], "both are planned for eviction");

    // The desktop's connection commits the star between planning and executing.
    assert!(fx.store.set_favourite(id, true).expect("star the clip"));

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 1, "only the clip nobody starred");
    assert_eq!(outcome.protected, 1, "the race is reported, not silent");
    assert_eq!(outcome.failed, 0);
    assert_eq!(fx.ids(), vec![id], "the starred clip's row survives");
    assert!(path.exists(), "and so does its file");
    assert!(!other_path.exists());
    assert_eq!(
        outcome.bytes_after, 100,
        "the protected clip still occupies the library"
    );
    assert!(
        !outcome.cap_met,
        "against a 1 byte cap it is over, honestly"
    );
}

/// A row that names a file outside the library costs the user the row, never the file: the same
/// line the desktop's interactive delete holds, now held by the unattended pass.
#[test]
fn a_row_naming_a_file_outside_the_library_loses_the_row_and_never_the_file() {
    let fx = Fixture::new();
    let elsewhere = tempfile::tempdir().expect("another directory");
    let thesis = elsewhere.path().join("thesis.docx");
    std::fs::write(&thesis, b"the only copy").expect("write the file");
    let id = fx.index_only(thesis.clone(), 100);

    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id]);
    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 1, "the row goes");
    assert_eq!(
        outcome.refused,
        vec![thesis.clone()],
        "and the refusal is reported"
    );
    assert!(outcome.orphaned.is_empty());
    assert_eq!(outcome.bytes_reclaimed, 0);
    assert!(thesis.exists(), "the user's file is untouched");
    assert!(fx.ids().is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_inside_the_library_cannot_lead_a_removal_out_of_it() {
    let fx = Fixture::new();
    let elsewhere = tempfile::tempdir().expect("another directory");
    let victim = elsewhere.path().join("victim.mp4");
    std::fs::write(&victim, b"outside").expect("write the file");
    let link = fx.clips_dir.join("link");
    std::os::unix::fs::symlink(elsewhere.path(), &link).expect("plant the link");
    let id = fx.index_only(link.join("victim.mp4"), 100);

    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id]);
    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 1);
    assert_eq!(outcome.refused.len(), 1, "{outcome:?}");
    assert!(
        victim.exists(),
        "the link was not followed out of the library"
    );
}

/// A removal that failed used to leave the file on disk with no row, for ever: out of the library,
/// out of the cap, never retried. It is now queued with the row delete and retried by the next
/// pass, which has nothing of its own to plan.
#[test]
fn a_removal_that_failed_is_retried_by_the_next_pass() {
    let fx = Fixture::new();
    // A directory where the file should be: `remove_file` cannot remove it, standing in for a file
    // another program holds open.
    let stuck = fx.clips_dir.join("stuck.mp4");
    std::fs::create_dir_all(&stuck).expect("create the obstruction");
    let id = fx.index_only(stuck.clone(), 100);

    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id]);
    let first = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("the first pass");
    assert_eq!(
        first.orphaned,
        vec![stuck.clone()],
        "it failed, and says so"
    );

    // The obstruction clears: whatever held the path now lets go.
    std::fs::remove_dir_all(&stuck).expect("clear the obstruction");
    std::fs::write(&stuck, vec![0u8; 100]).expect("the file is back, unlocked");

    let empty = fx.plan(&policy(u64::MAX, 3_650));
    assert!(empty.deletions.is_empty(), "this pass plans nothing at all");
    let second = execute_cleanup(&fx.store, &empty, &fx.roots()).expect("the second pass");

    assert_eq!(second.bytes_reclaimed, 100, "the queued removal ran");
    assert!(second.orphaned.is_empty());
    assert!(!stuck.exists(), "the file escaped the index no longer");
    let third = execute_cleanup(&fx.store, &empty, &fx.roots()).expect("the third pass");
    assert_eq!(third.bytes_reclaimed, 0, "and the queue is drained");
}

/// The file name was re-used while the old removal kept failing: the path is somebody's footage
/// again, and a queued removal must not take it.
#[test]
fn a_queued_removal_never_takes_a_path_a_live_row_names_again() {
    let fx = Fixture::new();
    let stuck = fx.clips_dir.join("stuck.mp4");
    std::fs::create_dir_all(&stuck).expect("create the obstruction");
    let id = fx.index_only(stuck.clone(), 100);
    let plan = fx.plan(&policy(1, 3_650));
    assert_eq!(plan.ids(), vec![id]);
    execute_cleanup(&fx.store, &plan, &fx.roots()).expect("the first pass");

    std::fs::remove_dir_all(&stuck).expect("clear the obstruction");
    std::fs::write(&stuck, b"new footage").expect("a new clip at the same path");
    fx.index_only(stuck.clone(), 11);

    let empty = fx.plan(&policy(u64::MAX, 3_650));
    let outcome = execute_cleanup(&fx.store, &empty, &fx.roots()).expect("the next pass");

    assert!(stuck.exists(), "the re-indexed clip's file is untouched");
    assert_eq!(outcome.refused, vec![stuck.clone()]);
    let again = execute_cleanup(&fx.store, &empty, &fx.roots()).expect("and the one after");
    assert!(
        again.refused.is_empty(),
        "the queue entry was dropped, not retried for ever"
    );
}

/// The plan is made over the rows whose file exists, so the pass's verdict must be about those
/// rows too: a phantom row's recorded bytes used to make `cap_met` false for a library the pass had
/// brought under its cap, and the caller then blamed the favourites.
#[test]
fn a_row_whose_file_is_gone_does_not_make_a_met_cap_look_unmet() {
    let fx = Fixture::new();
    let (_a, a_path) = fx.add_clip("a.mp4", 100);
    let (_b, _b_path) = fx.add_clip("b.mp4", 100);
    fx.index_only(fx.clips_dir.join("phantom.mp4"), 10_000);

    // Plan over what is on disk, as the recorder does.
    let live: Vec<_> = fx
        .store
        .list_clips()
        .expect("list")
        .into_iter()
        .filter(|c| c.path.exists())
        .collect();
    let plan = plan_cleanup(&live, &policy(100, 3_650), now_ms());
    assert_eq!(plan.deletions.len(), 1);
    assert!(plan.cap_met());

    let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");

    assert_eq!(outcome.deleted, 1);
    assert!(!a_path.exists());
    assert_eq!(
        outcome.bytes_after, 100,
        "the live population, not the phantom's 10 000"
    );
    assert!(outcome.cap_met);
}

#[test]
fn a_zero_limit_policy_deletes_nothing_end_to_end() {
    let fx = Fixture::new();
    let (_id, path) = fx.add_clip("keep.mp4", 100);
    for zero in [policy(0, 3_650), policy(u64::MAX, 0)] {
        let plan = fx.plan(&zero);
        let outcome = execute_cleanup(&fx.store, &plan, &fx.roots()).expect("execute the plan");
        assert_eq!(outcome.deleted, 0, "{zero:?}");
        assert!(
            path.exists(),
            "{zero:?}: a zero limit is not a licence to delete"
        );
    }
}
