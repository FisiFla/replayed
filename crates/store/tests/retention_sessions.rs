//! Dual retention against a real database and a real filesystem.
//!
//! The rules are unit-tested in `crates/store/src/retention.rs`. This file shows what a pass does
//! to the disk and the index: which files survive, in which order rows go, and how many bytes a
//! pass actually reclaimed, measured against files the fixture wrote rather than numbers the code
//! assumed. Blocks that reach a decision print their totals (visible with `--nocapture`).
//!
//! Two classes, two caps, and the interactions that only exist once sessions are real: a
//! session's scratch tree and final file both go with it, the clips extracted from it do not, and
//! a session still recording is not a candidate.
//!
//! Every path lives in a temp directory under the workspace's `target/`.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use replayed_store::cleanup::{plan_cleanup, CleanupPolicy};
use replayed_store::retention::{
    execute_retention, plan_retention, RetentionPlan, RetentionPolicy, RetentionRules, Subject,
};
use replayed_store::{
    LibraryRoots, NewClip, NewEvent, Store, SESSION_MODE_BUFFER, SESSION_MODE_SESSION,
};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A temp directory under the workspace's `target/`.
///
/// Not the system temp directory: these tests write and delete real files, and keeping them
/// inside the tree the build owns means a failed run cannot leave anything in a user's profile.
/// `CARGO_TARGET_DIR` is honoured when set; otherwise it is `../../target` from this crate.
fn temp_dir() -> tempfile::TempDir {
    let target = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target"),
    };
    std::fs::create_dir_all(&target).expect("create the target directory");
    // Canonicalised so a failure message names `<workspace>/target/...` rather than a `../..` walk.
    let target = std::fs::canonicalize(&target).unwrap_or(target);
    tempfile::Builder::new()
        .prefix("replayed-store-retention-")
        .tempdir_in(&target)
        .expect("a temp dir under target/")
}

/// The bytes a directory or file really holds, read back from the disk. The test's own
/// measurement, not the crate's, so the two have to agree.
fn real_bytes(path: &Path) -> u64 {
    let meta = std::fs::symlink_metadata(path).expect("stat");
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .expect("read the tree")
        .map(|entry| real_bytes(&entry.expect("entry").path()))
        .sum()
}

/// One session: the row and the real files it names.
struct SessionFiles {
    id: i64,
    scratch_dir: PathBuf,
    /// `None` for a session with no concatenated file: a running one, or a finished one never finalised into a single file.
    final_path: Option<PathBuf>,
    bytes_on_disk: u64,
}

impl SessionFiles {
    fn all_exist(&self) -> bool {
        self.scratch_dir.is_dir() && self.final_path.as_ref().is_none_or(|p| p.is_file())
    }

    fn none_exist(&self) -> bool {
        !self.scratch_dir.exists() && self.final_path.as_ref().is_none_or(|p| !p.exists())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db_path: PathBuf,
    store: Store,
    sessions: usize,
}

impl Fixture {
    fn new() -> Self {
        let dir = temp_dir();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("clips")).expect("create the clips dir");
        let db_path = root.join("replayed.db");
        let store = Store::open(&db_path).expect("open the index");
        store.migrate().expect("migrate the index");
        Self {
            _dir: dir,
            root,
            db_path,
            store,
            sessions: 0,
        }
    }

    /// Write a session's bytes to disk and index the row as a session recorder does: segments in a
    /// scratch directory and, when the session produced one, a concatenated final file. `size_bytes`
    /// is set to what is really there, so the cap arithmetic and the disk start out agreeing.
    fn add_session(
        &mut self,
        started_at_ms: i64,
        segments: &[usize],
        final_bytes: usize,
    ) -> SessionFiles {
        let (id, scratch_dir, final_path, bytes_on_disk) =
            self.write_session(started_at_ms, segments, final_bytes);
        let final_path_arg = final_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        self.store
            .end_session(
                id,
                started_at_ms + 60_000,
                final_path_arg.as_deref(),
                bytes_on_disk as i64,
                0,
            )
            .expect("end the session");
        SessionFiles {
            id,
            scratch_dir,
            final_path,
            bytes_on_disk,
        }
    }

    /// A session still recording: no `ended_at`, segments on disk, and the running size a recorder
    /// keeps current (`Store::set_session_size`) so the sessions cap can see its bytes.
    fn add_running_session(&mut self, started_at_ms: i64, segments: &[usize]) -> SessionFiles {
        let (id, scratch_dir, final_path, bytes_on_disk) =
            self.write_session(started_at_ms, segments, 0);
        assert!(
            final_path.is_none(),
            "a running session has no final file yet"
        );
        self.store
            .set_session_size(id, bytes_on_disk as i64)
            .expect("keep the running size current");
        SessionFiles {
            id,
            scratch_dir,
            final_path,
            bytes_on_disk,
        }
    }

    fn write_session(
        &mut self,
        started_at_ms: i64,
        segments: &[usize],
        final_bytes: usize,
    ) -> (i64, PathBuf, Option<PathBuf>, u64) {
        self.sessions += 1;
        let name = format!("session-{}", self.sessions);
        // Where the recorder puts it: a `session-<n>` directory inside the sessions directory, beside
        // the session's own `session-<n>.mp4`.
        let scratch_dir = self.root.join("sessions").join(&name);
        std::fs::create_dir_all(&scratch_dir).expect("create the scratch dir");
        for (i, bytes) in segments.iter().enumerate() {
            std::fs::write(
                scratch_dir.join(format!("seg-{i:03}.mkv")),
                vec![0u8; *bytes],
            )
            .expect("write a segment");
        }

        let final_path =
            (final_bytes > 0).then(|| self.root.join("sessions").join(format!("{name}.mp4")));
        if let Some(path) = &final_path {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the dir");
            std::fs::write(path, vec![0u8; final_bytes]).expect("write the final file");
        }

        let bytes_on_disk =
            real_bytes(&scratch_dir) + final_path.as_ref().map_or(0, |p| real_bytes(p));
        let id = self
            .store
            .start_session(
                Some("League of Legends"),
                SESSION_MODE_SESSION,
                started_at_ms,
                &scratch_dir.to_string_lossy(),
                0,
            )
            .expect("open the session");
        (id, scratch_dir, final_path, bytes_on_disk)
    }

    /// A clip file of `bytes`, indexed the way the trigger path indexes one.
    fn add_clip(&self, name: &str, bytes: usize) -> (i64, PathBuf) {
        let path = self.root.join("clips").join(name);
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

    fn favourite_session(&self, id: i64) {
        self.store
            .set_session_favourite(id, true)
            .expect("favourite the session");
    }

    /// Age a clip as an operator would with `sqlite3`: `clips.created_at` is stamped by the store with
    /// no API to change it. A session needs no equivalent, since `started_at` is an argument of `start_session`.
    fn backdate_clip(&self, id: i64, created_at_ms: i64) {
        let conn = rusqlite::Connection::open(&self.db_path).expect("a second connection");
        conn.execute(
            "UPDATE clips SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![id, created_at_ms],
        )
        .expect("backdate the row");
    }

    /// Write a clip as a session recorder writes one extracted from a session: the file and its row,
    /// linked to the source session. `insert_clip` takes no `session_id`, so the link is the one
    /// write done by hand.
    fn add_extracted_clip(&self, session_id: i64, name: &str, bytes: usize) -> (i64, PathBuf) {
        let (clip, path) = self.add_clip(name, bytes);
        let conn = rusqlite::Connection::open(&self.db_path).expect("a second connection");
        conn.execute(
            "UPDATE clips SET session_id = ?2 WHERE id = ?1",
            rusqlite::params![clip, session_id],
        )
        .expect("link the clip to its session");
        assert_eq!(self.clip_session_id(clip), Some(session_id));
        (clip, path)
    }

    fn clip_session_id(&self, clip_id: i64) -> Option<i64> {
        let conn = rusqlite::Connection::open(&self.db_path).expect("a second connection");
        conn.query_row(
            "SELECT session_id FROM clips WHERE id = ?1",
            rusqlite::params![clip_id],
            |r| r.get(0),
        )
        .expect("the clip row")
    }

    /// The containment roots the application hands the executor: the clips and sessions directories.
    fn roots(&self) -> LibraryRoots {
        LibraryRoots::new(self.root.join("clips"), self.root.join("sessions"))
    }

    fn plan(&self, policy: &RetentionPolicy) -> RetentionPlan {
        plan_retention(
            &self.store.list_clips().expect("the clips"),
            &self.store.list_sessions().expect("the sessions"),
            policy,
            now_ms(),
        )
    }

    /// Clip ids, sorted: `list_clips` orders by `started_at`, which a fixture indexing every clip at
    /// the same instant cannot break ties with.
    fn clip_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .store
            .list_clips()
            .expect("clips")
            .iter()
            .map(|c| c.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn session_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .store
            .list_sessions()
            .expect("sessions")
            .iter()
            .map(|s| s.id)
            .collect();
        ids.sort_unstable();
        ids
    }
}

fn policy(clips: RetentionRules, sessions: RetentionRules) -> RetentionPolicy {
    RetentionPolicy { clips, sessions }
}

/// A cap nothing reaches and an age nothing exceeds, so the class not under study stays out of the way.
fn generous() -> RetentionRules {
    RetentionRules {
        max_total_bytes: u64::MAX,
        max_age_days: 3_650,
    }
}

fn rules(max_total_bytes: u64, max_age_days: u32) -> RetentionRules {
    RetentionRules {
        max_total_bytes,
        max_age_days,
    }
}

#[test]
fn a_capped_session_is_evicted_as_a_unit_with_its_scratch_tree_and_its_final_file() {
    let mut fx = Fixture::new();
    let old = fx.add_session(now_ms() - 3 * DAY_MS, &[400, 300], 500);
    let recent = fx.add_session(now_ms() - DAY_MS, &[200, 200], 0);

    let store_bytes = fx
        .store
        .total_session_bytes()
        .expect("the session store's bytes");
    assert_eq!(
        old.bytes_on_disk, 1_200,
        "400 + 300 in the scratch tree, 500 in the final file"
    );
    assert_eq!(
        recent.bytes_on_disk, 400,
        "200 + 200, and no final file of its own"
    );
    assert_eq!(store_bytes, 1_600);
    println!(
        "retention: the session store indexes {store_bytes} bytes: {} really on disk across \
         {} sessions, including {} bytes of final files; the cap is 500",
        old.bytes_on_disk + recent.bytes_on_disk,
        fx.store.list_sessions().expect("sessions").len(),
        real_bytes(&fx.root.join("sessions")),
    );

    // 1_600 bytes against a 500 byte cap: evicting the oldest session alone reaches it.
    let plan = fx.plan(&policy(generous(), rules(500, 3_650)));
    println!(
        "retention: plan deletes session {:?} ({} bytes) and forecasts {} bytes left",
        plan.sessions.ids(),
        old.bytes_on_disk,
        plan.sessions.bytes_after
    );
    assert_eq!(
        plan.sessions.ids(),
        vec![old.id],
        "the oldest session, and only it"
    );
    assert_eq!(
        plan.sessions.bytes_after, 400,
        "the recent session's bytes are what is left"
    );
    assert!(plan.sessions.cap_met(), "400 is at or under 500");
    assert!(
        plan.clips.deletions.is_empty(),
        "no clip was touched to make room"
    );

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: deleted {} row(s), reclaimed {} bytes, session store now {} bytes \
         (cap_met: {}); leftovers: {:?}",
        outcome.sessions.deleted,
        outcome.sessions.bytes_reclaimed,
        outcome.sessions.bytes_after,
        outcome.sessions.cap_met,
        outcome.leftovers().collect::<Vec<_>>()
    );

    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(outcome.sessions.failed, 0);
    assert_eq!(
        outcome.sessions.bytes_reclaimed, 1_200,
        "the bytes the tree and the final file really held, measured as they went"
    );
    assert_eq!(
        outcome.sessions.bytes_after, 400,
        "re-read from the store after the pass"
    );
    assert!(outcome.sessions.cap_met);
    assert!(
        outcome.sessions.orphaned.is_empty(),
        "both removal targets were where the row said"
    );
    assert!(outcome.sessions.already_missing.is_empty());
    assert!(outcome.sessions.refused.is_empty());

    assert_eq!(fx.session_ids(), vec![recent.id], "the row is gone");
    assert!(
        old.none_exist(),
        "the whole session went: scratch tree and final file"
    );
    assert!(
        recent.all_exist(),
        "the other session's files are untouched"
    );
    assert_eq!(
        real_bytes(&fx.root.join("sessions")),
        recent.bytes_on_disk,
        "nothing of the evicted session is left under the sessions directory"
    );
}

#[test]
fn the_bytes_a_pass_reports_are_measured_from_the_disk_not_forecast_from_the_rows() {
    // A row that disagrees with its files (a truncated write, a re-encode, a hand-edited index). The
    // plan's arithmetic uses the row, all it has; what a pass reports reclaiming is what the disk
    // actually lost. Reporting the forecast is how a storage manager lies to a user.
    let mut fx = Fixture::new();
    let session = fx.add_session(now_ms() - DAY_MS, &[400, 300], 500);
    let conn = rusqlite::Connection::open(&fx.db_path).expect("a second connection");
    conn.execute(
        "UPDATE sessions SET size_bytes = 9999 WHERE id = ?1",
        rusqlite::params![session.id],
    )
    .expect("make the row disagree with its files");

    let plan = fx.plan(&policy(generous(), rules(1, 3_650)));
    assert_eq!(plan.sessions.ids(), vec![session.id]);
    println!(
        "retention: the row claims {} bytes, the disk holds {}; the cap is 0",
        plan.sessions.deletions[0].size_bytes, session.bytes_on_disk
    );

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    println!(
        "retention: reclaimed {} measured bytes (the row had claimed 9999); store now {} bytes",
        outcome.sessions.bytes_reclaimed, outcome.sessions.bytes_after
    );
    assert_eq!(
        outcome.sessions.bytes_reclaimed, session.bytes_on_disk,
        "measured, not the row's number"
    );
    assert_ne!(outcome.sessions.bytes_reclaimed, 9_999);
    assert_eq!(outcome.sessions.bytes_after, 0);
    assert!(outcome.sessions.cap_met);
    assert!(session.none_exist());
}

#[test]
fn the_two_classes_never_evict_each_other() {
    let mut fx = Fixture::new();
    let (clip_a, path_a) = fx.add_clip("a.mp4", 100);
    let (clip_b, path_b) = fx.add_clip("b.mp4", 100);
    let (clip_c, path_c) = fx.add_clip("c.mp4", 100);
    let session = fx.add_session(now_ms() - DAY_MS, &[4_000], 0);

    // A clip store over its cap with a session store that is not: only clips go.
    let plan = fx.plan(&policy(rules(100, 3_650), generous()));
    println!(
        "retention: 300 bytes of clips against a 100 byte clip cap -> clips {:?}, sessions {:?}",
        plan.clips.ids(),
        plan.sessions.ids()
    );
    assert_eq!(plan.clips.ids(), vec![clip_a, clip_b]);
    assert!(
        plan.sessions.deletions.is_empty(),
        "a clip cap is not a session cap"
    );

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    assert_eq!(outcome.clips.deleted, 2);
    assert_eq!(outcome.sessions.deleted, 0);
    assert_eq!(outcome.clips.bytes_reclaimed, 200);
    assert!(!path_a.exists() && !path_b.exists());
    assert!(
        path_c.exists(),
        "the clip the cap did not need is still there"
    );
    assert!(
        session.all_exist(),
        "and the session and its files are untouched"
    );
    assert_eq!(fx.clip_ids(), vec![clip_c]);

    // And the other direction: a sessions cap of 0 with clips still over their own cap.
    let plan = fx.plan(&policy(generous(), rules(1, 3_650)));
    println!(
        "retention: 4_000 bytes of session against a 0 byte session cap -> clips {:?}, \
         sessions {:?}",
        plan.clips.ids(),
        plan.sessions.ids()
    );
    assert!(
        plan.clips.deletions.is_empty(),
        "a session cap is not a clip cap"
    );
    assert_eq!(plan.sessions.ids(), vec![session.id]);

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(outcome.clips.deleted, 0);
    assert_eq!(
        outcome.sessions.bytes_reclaimed, 4_000,
        "one 4_000 byte segment"
    );
    assert!(path_c.exists(), "the surviving clip is still on disk");
    assert!(session.none_exist(), "and the session's tree is gone");
    assert_eq!(fx.clip_ids(), vec![clip_c]);
    assert!(fx.session_ids().is_empty());
}

#[test]
fn a_favourite_session_is_immune_to_both_rules_and_its_shortfall_is_reported() {
    let mut fx = Fixture::new();
    // Ancient, and alone bigger than the cap: both rules want it, both must be refused.
    let favourite = fx.add_session(now_ms() - 90 * DAY_MS, &[600, 400], 500);
    fx.favourite_session(favourite.id);
    let plain = fx.add_session(now_ms() - 60 * DAY_MS, &[100], 0);

    let plan = fx.plan(&policy(generous(), rules(500, 7)));

    println!(
        "retention: a favourited session holds {} bytes against a 500 byte cap and a 7 day age \
         limit; plan deletes {:?}, over_cap_by_bytes {}, bytes_after {}",
        favourite.bytes_on_disk,
        plan.sessions.ids(),
        plan.sessions.over_cap_by_bytes,
        plan.sessions.bytes_after
    );
    assert_eq!(
        plan.sessions.ids(),
        vec![plain.id],
        "the non-favourite that is over age goes; the favourite is not evicted for a cap it \
         breaks"
    );
    assert_eq!(
        plan.sessions.over_cap_by_bytes, 1_000,
        "1_500 bytes of favourite against 500"
    );
    assert!(
        !plan.sessions.cap_met(),
        "and the shortfall is reported, not paid for"
    );

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: deleted {} session(s), reclaimed {} bytes; store now {} bytes, cap met: {}",
        outcome.sessions.deleted,
        outcome.sessions.bytes_reclaimed,
        outcome.sessions.bytes_after,
        outcome.sessions.cap_met
    );
    assert!(
        favourite.all_exist(),
        "a favourited session is never deleted, files included"
    );
    assert!(plain.none_exist(), "the aged non-favourite went");
    assert!(
        !outcome.sessions.cap_met,
        "the outcome reports the cap as unmet too"
    );
    assert_eq!(
        outcome.sessions.bytes_after, 1_500,
        "the favourite's bytes are still indexed"
    );
}

#[test]
fn a_favourite_clip_is_immune_to_both_rules_under_the_new_entry_point_too() {
    // The clips policy's guarantee, checked through the retention entry point so the two cannot diverge: the clips half is `plan_cleanup`.
    let fx = Fixture::new();
    let (ancient_favourite, ancient_path) = fx.add_clip("ancient-favourite.mp4", 400);
    let (big_favourite, big_path) = fx.add_clip("big-favourite.mp4", 600);
    let (plain, plain_path) = fx.add_clip("plain.mp4", 100);
    fx.backdate_clip(ancient_favourite, now_ms() - 90 * DAY_MS);
    fx.backdate_clip(big_favourite, now_ms() - 90 * DAY_MS);
    for id in [ancient_favourite, big_favourite] {
        assert!(fx.store.set_favourite(id, true).expect("favourite it"));
    }

    let clips = fx.store.list_clips().expect("clips");
    let plan = fx.plan(&policy(rules(10, 1), generous()));
    let flat = plan_cleanup(
        &clips,
        &CleanupPolicy {
            max_total_bytes: 10,
            max_age_days: 1,
        },
        now_ms(),
    );

    println!(
        "retention: {} clips holding {} bytes, 1_000 of them favourited, against a 10 byte cap \
         and a 1 day age limit; retention deletes {:?}, cleanup deletes {:?}",
        clips.len(),
        clips.iter().map(|c| c.size_bytes).sum::<u64>(),
        plan.clips.ids(),
        flat.ids()
    );
    assert_eq!(
        plan.clips.ids(),
        Vec::<i64>::new(),
        "nothing can be deleted for this cap"
    );
    assert_eq!(
        plan.clips.ids(),
        flat.ids(),
        "and the two entry points agree exactly"
    );
    assert_eq!(plan.clips.over_cap_by_bytes, 990);
    assert!(!plan.clips.cap_met());

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    assert_eq!(
        outcome.deleted(),
        0,
        "a pass with nothing to do deletes nothing"
    );
    for path in [&ancient_path, &big_path, &plain_path] {
        assert!(path.is_file(), "{} must survive", path.display());
    }
    assert_eq!(fx.clip_ids(), {
        let mut ids = vec![plain, big_favourite, ancient_favourite];
        ids.sort_unstable();
        ids
    });
}

#[test]
fn a_session_that_is_still_recording_is_never_evicted_and_its_bytes_are_counted() {
    let mut fx = Fixture::new();
    let live = fx.add_running_session(now_ms() - 90 * DAY_MS, &[1_800, 1_200]);
    let finished = fx.add_session(now_ms() - 90 * DAY_MS, &[500], 0);

    // Every rule violated as hard as it can be: a 7 day limit, a 500 byte cap, and the recording alone is 3_000 bytes.
    let plan = fx.plan(&policy(generous(), rules(500, 7)));

    println!(
        "retention: a recording holds {} bytes and a finished session {} bytes, against a 500 \
         byte cap and a 7 day limit; plan deletes {:?}, over_cap_by_bytes {}",
        live.bytes_on_disk,
        finished.bytes_on_disk,
        plan.sessions.ids(),
        plan.sessions.over_cap_by_bytes
    );
    assert_eq!(
        plan.sessions.ids(),
        vec![finished.id],
        "the aged finished session goes; the recording is not a candidate"
    );
    assert_eq!(
        plan.sessions.over_cap_by_bytes, 2_500,
        "the recording's 3_000 bytes less the 500 byte cap: reported, not evicted"
    );
    assert!(!plan.sessions.cap_met());

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(
        outcome.sessions.failed, 0,
        "the planner never names a recording, so nothing fails"
    );
    assert!(
        live.all_exist(),
        "the recording's scratch tree is exactly where it was"
    );
    assert_eq!(
        fx.store
            .get_session(live.id)
            .expect("read")
            .expect("still there")
            .ended_at_ms,
        None,
        "and it is still recording"
    );
    assert_eq!(
        outcome.sessions.bytes_after, 3_000,
        "the recording's bytes are still indexed"
    );
    assert!(
        !outcome.sessions.cap_met,
        "reported unmet rather than made true by eviction"
    );
}

#[test]
fn a_session_left_running_by_a_crash_is_evicted_once_it_is_ended_explicitly() {
    // The other half of the in-progress rule: immunity is not permanent. A session whose recording
    // was killed is still `ended_at IS NULL`, and the honest way to make it evictable is to end it
    // with the clock and bytes the caller believes, not to infer from a process table.
    let mut fx = Fixture::new();
    let crashed = fx.add_running_session(now_ms() - 90 * DAY_MS, &[700]);

    let err = fx
        .store
        .delete_session_returning_paths(crashed.id)
        .expect_err("a session that is still recording cannot be deleted");
    assert!(format!("{err:#}").contains("still recording"), "{err:#}");
    assert!(crashed.scratch_dir.is_dir(), "its tree is untouched");

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    assert!(
        plan.sessions.deletions.is_empty(),
        "and the planner will not name it either"
    );

    // The recovery pass, in one call: the session is over, with no final file to name.
    fx.store
        .end_session(crashed.id, now_ms(), None, crashed.bytes_on_disk as i64, 0)
        .expect("end the session a crash left running");

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    println!(
        "retention: a session started {} days ago, ended by the recovery pass; plan {:?} under a \
         7 day limit",
        (now_ms() - (now_ms() - 90 * DAY_MS)) / DAY_MS,
        plan.sessions.ids()
    );
    assert_eq!(
        plan.sessions.ids(),
        vec![crashed.id],
        "now it is an ordinary aged session"
    );

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(outcome.sessions.bytes_reclaimed, 700);
    assert!(
        !crashed.scratch_dir.exists(),
        "the scratch tree went with the row"
    );
    assert!(fx.session_ids().is_empty());
}

#[test]
fn deleting_a_session_leaves_the_clip_extracted_from_it_and_its_events_alone() {
    let mut fx = Fixture::new();
    let session = fx.add_session(now_ms() - 30 * DAY_MS, &[900], 300);
    // The clip extracted from it: a first-class artefact with its own file and its own row.
    let (clip, clip_path) = fx.add_extracted_clip(session.id, "from-session.mp4", 250);
    let event = fx
        .store
        .insert_event(&NewEvent {
            session_id: Some(session.id),
            kind: "kill".into(),
            at_ms: 30_000,
            payload: Some("{\"source\":\"lol\"}".into()),
            clip_id: Some(clip),
        })
        .expect("record the event that produced it");

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    assert_eq!(plan.sessions.ids(), vec![session.id]);
    assert_eq!(
        plan.deletions(),
        1,
        "the session is deleted as a unit, and no clip is planned"
    );

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: deleted session {} (reclaimed {} bytes); clip {} and its 250 byte file survive",
        session.id, outcome.sessions.bytes_reclaimed, clip
    );

    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(
        outcome.clips.deleted, 0,
        "a session's clips are not the session"
    );
    assert!(
        session.none_exist(),
        "the session's own files are gone: 900 + 300 bytes reclaimed"
    );
    assert_eq!(outcome.sessions.bytes_reclaimed, 1_200);
    assert!(clip_path.is_file(), "the extracted clip's file survives");
    let clips = fx.store.list_clips().expect("clips");
    assert_eq!(clips.len(), 1, "and so does its row");
    assert_eq!(clips[0].id, clip);
    assert_eq!(clips[0].size_bytes, 250);
    assert_eq!(
        real_bytes(&clip_path),
        250,
        "the clip's bytes are really still on the disk"
    );
    assert_eq!(
        fx.clip_session_id(clip),
        None,
        "the link to the session that no longer exists is cleared, and nothing else changes"
    );

    let events = fx.store.list_events().expect("events");
    assert_eq!(events.len(), 1, "the event survives as history");
    assert_eq!(events[0].id, event);
    assert_eq!(
        events[0].session_id, None,
        "detached from the session that is gone"
    );
    assert_eq!(
        events[0].clip_id,
        Some(clip),
        "and still naming the clip it produced"
    );
    assert!(fx
        .store
        .events_for_session(session.id)
        .expect("timeline")
        .is_empty());
}

#[test]
fn deletions_run_oldest_first_in_both_classes() {
    let mut fx = Fixture::new();
    // Sessions written in a scrambled order: the plan follows `started_at`, not row creation order.
    // Clips are stamped by the store, so their order is insertion order (middle, newest, oldest).
    let middle = fx.add_session(now_ms() - 20 * DAY_MS, &[500], 0);
    let newest = fx.add_session(now_ms() - 5 * DAY_MS, &[500], 0);
    let oldest = fx.add_session(now_ms() - 40 * DAY_MS, &[500], 0);
    let (clip_first, path_first) = fx.add_clip("first.mp4", 500);
    let (clip_second, path_second) = fx.add_clip("second.mp4", 500);
    let (clip_third, path_third) = fx.add_clip("third.mp4", 500);

    // Both caps hold exactly one 500 byte artefact, so each class evicts its two oldest and keeps its newest, in that order.
    let plan = fx.plan(&policy(rules(500, 3_650), rules(500, 3_650)));
    println!(
        "retention: 3 sessions and 3 clips of 500 bytes each, caps of 500 -> sessions {:?}, \
         clips {:?}",
        plan.sessions.ids(),
        plan.clips.ids()
    );

    assert_eq!(
        plan.sessions.ids(),
        vec![oldest.id, middle.id],
        "oldest started first, and on until the class is at or under its cap"
    );
    assert_eq!(
        plan.clips.ids(),
        vec![clip_first, clip_second],
        "oldest created first"
    );
    assert_eq!(
        plan.sessions.deletions[0].subject,
        Subject::Session(oldest.id),
        "a deletion says which class it belongs to"
    );
    assert!(plan
        .clips
        .deletions
        .iter()
        .all(|d| matches!(d.subject, Subject::Clip(_))));

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: deleted {} sessions ({} bytes) and {} clips ({} bytes); {} bytes of sessions \
         and {} bytes of clips remain",
        outcome.sessions.deleted,
        outcome.sessions.bytes_reclaimed,
        outcome.clips.deleted,
        outcome.clips.bytes_reclaimed,
        outcome.sessions.bytes_after,
        outcome.clips.bytes_after
    );

    assert_eq!(outcome.deleted(), 4);
    assert_eq!(
        fx.session_ids(),
        vec![newest.id],
        "the newest session is the survivor"
    );
    assert!(newest.all_exist());
    assert!(
        oldest.none_exist() && middle.none_exist(),
        "both older sessions went, files too"
    );
    assert!(!path_first.exists(), "the first-created clip went first");
    assert!(!path_second.exists(), "and the second with it");
    assert!(
        path_third.exists(),
        "the third-created clip is the one that survives"
    );
    assert_eq!(fx.clip_ids(), vec![clip_third]);
    assert_eq!(outcome.clips.bytes_after, 500, "one 500 byte clip remains");
    assert_eq!(
        outcome.sessions.bytes_after, 500,
        "one 500 byte session remains"
    );
    assert!(outcome.cap_met(), "both classes ended under their own caps");
}

#[test]
fn a_scratch_directory_that_is_a_symlink_is_refused_and_reported_not_followed() {
    // The one removal in this crate that takes a whole tree, against the adversary it can actually
    // meet: a `scratch_dir` TEXT value naming something other than the directory the application
    // created. The row is deleted; the tree it pointed at is left as is and reported, not skipped.
    let mut fx = Fixture::new();
    let session = fx.add_session(now_ms() - 30 * DAY_MS, &[600], 200);
    let elsewhere = fx.root.join("not-the-scratch-dir");
    std::fs::create_dir_all(&elsewhere).expect("create the directory the row should not own");
    std::fs::write(elsewhere.join("someone-elses.bin"), vec![0u8; 900]).expect("write into it");
    let link = fx.root.join("sessions").join("session-999");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink the scratch path");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&elsewhere, &link).expect("symlink the scratch path");

    let conn = rusqlite::Connection::open(&fx.db_path).expect("a second connection");
    conn.execute(
        "UPDATE sessions SET scratch_dir = ?2 WHERE id = ?1",
        rusqlite::params![session.id, link.to_string_lossy().to_string()],
    )
    .expect("point the row at the symlink");

    let plan = fx.plan(&policy(generous(), rules(1, 3_650)));
    assert_eq!(plan.sessions.ids(), vec![session.id]);

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: session {} deleted; refused {:?}, reclaimed {} bytes (its 200 byte final \
         file only)",
        session.id, outcome.sessions.refused, outcome.sessions.bytes_reclaimed
    );

    assert_eq!(
        outcome.sessions.deleted, 1,
        "the row goes: it is the row's own bookkeeping"
    );
    assert_eq!(
        outcome.sessions.refused,
        vec![link.clone()],
        "and the path that is not a plain directory is reported, not recursed into"
    );
    assert!(
        outcome.sessions.orphaned.is_empty(),
        "a refusal is not an orphan"
    );
    assert!(outcome.sessions.already_missing.is_empty());
    assert_eq!(
        outcome.sessions.bytes_reclaimed, 200,
        "only the final file was really removed; the tree was left alone"
    );
    assert!(link.exists(), "the symlink is still there");
    assert!(
        elsewhere.join("someone-elses.bin").is_file(),
        "and so is everything it pointed at"
    );
    assert!(fx.session_ids().is_empty(), "the row is gone, as planned");
}

#[test]
fn a_final_file_that_is_already_gone_is_reported_and_the_scratch_tree_still_goes() {
    let mut fx = Fixture::new();
    let session = fx.add_session(now_ms() - 30 * DAY_MS, &[250], 150);
    let final_path = session
        .final_path
        .clone()
        .expect("a session with a final file");
    std::fs::remove_file(&final_path).expect("a file that disappeared behind the index");

    let plan = fx.plan(&policy(generous(), rules(1, 3_650)));
    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: session {} deleted; already_missing {:?}, reclaimed {} bytes",
        session.id, outcome.sessions.already_missing, outcome.sessions.bytes_reclaimed
    );

    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(
        outcome.sessions.already_missing,
        vec![final_path],
        "a row pointing at nothing is reported, not swallowed"
    );
    assert!(
        outcome.sessions.orphaned.is_empty(),
        "there was no file to orphan"
    );
    assert!(outcome.sessions.refused.is_empty());
    assert_eq!(
        outcome.sessions.bytes_reclaimed, 250,
        "the scratch tree's bytes, and only those"
    );
    assert!(!session.scratch_dir.exists(), "the tree went");
}

#[test]
fn a_final_path_that_cannot_be_unlinked_is_reported_as_an_orphan() {
    // The other way the filesystem can disagree with the index: something is there that is not a
    // file. A row deleted and a path left behind is recoverable, and reported.
    let mut fx = Fixture::new();
    let mut session = fx.add_session(now_ms() - 30 * DAY_MS, &[100], 100);
    let in_the_way = fx.root.join("sessions").join("a-directory.mp4");
    std::fs::create_dir_all(&in_the_way).expect("create a directory where a file belongs");
    std::fs::write(in_the_way.join("inside.bin"), b"still here").expect("write into it");
    let conn = rusqlite::Connection::open(&fx.db_path).expect("a second connection");
    conn.execute(
        "UPDATE sessions SET final_path = ?2 WHERE id = ?1",
        rusqlite::params![session.id, in_the_way.to_string_lossy().to_string()],
    )
    .expect("point the row at the directory");
    session.final_path = Some(in_the_way.clone());

    let plan = fx.plan(&policy(generous(), rules(1, 3_650)));
    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");
    println!(
        "retention: session {} deleted; orphaned {:?}, reclaimed {} bytes",
        session.id, outcome.sessions.orphaned, outcome.sessions.bytes_reclaimed
    );

    assert_eq!(
        outcome.sessions.deleted, 1,
        "the row goes first, and it stays gone"
    );
    assert_eq!(
        outcome.sessions.orphaned,
        vec![in_the_way.clone()],
        "reported with its path"
    );
    assert!(
        in_the_way.join("inside.bin").is_file(),
        "and nothing reached inside it"
    );
    assert_eq!(
        outcome.sessions.bytes_reclaimed, 100,
        "only the scratch tree was removed"
    );
    assert!(!session.scratch_dir.exists());
}

/// What a replay-buffer run really writes to the index: a row whose `scratch_dir` is the ring's one
/// shared, user-configurable directory, which the row does not own. Every fixture above gives each
/// session a private `session-N` directory, so none of them modelled the production data and the
/// whole-directory wipe below was invisible to them.
fn add_buffer_row(fx: &Fixture, started_at_ms: i64, shared: &Path) -> i64 {
    let id = fx
        .store
        .start_session(
            Some("League of Legends"),
            SESSION_MODE_BUFFER,
            started_at_ms,
            &shared.to_string_lossy(),
            0,
        )
        .expect("open the buffer session");
    fx.store
        .end_session(id, started_at_ms + 60_000, None, 0, 0)
        .expect("end it");
    id
}

#[test]
fn evicting_aged_buffer_rows_never_removes_the_shared_scratch_directory() {
    let fx = Fixture::new();
    let shared = fx.root.join("Replays");
    std::fs::create_dir_all(&shared).expect("the shared directory");
    let precious = shared.join("my-other-files.txt");
    std::fs::write(&precious, b"not the application's to delete").expect("write");
    let first = add_buffer_row(&fx, now_ms() - 30 * DAY_MS, &shared);
    let second = add_buffer_row(&fx, now_ms() - 20 * DAY_MS, &shared);

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    assert_eq!(
        plan.sessions.ids(),
        vec![first, second],
        "both aged rows are planned"
    );
    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    assert_eq!(outcome.sessions.deleted, 2, "the rows go");
    assert!(shared.is_dir(), "the shared directory survives");
    assert!(precious.is_file(), "and so does everything in it");
    assert!(
        outcome.sessions.already_missing.is_empty() && outcome.sessions.refused.is_empty(),
        "a buffer row owns no tree, so there is nothing to report either: {outcome:?}"
    );
}

/// The configuration the finding describes: the user points `[buffer].scratch_dir` at the sessions
/// directory itself, which holds other sessions' footage.
#[test]
fn a_buffer_row_naming_the_sessions_directory_does_not_take_the_sessions_with_it() {
    let mut fx = Fixture::new();
    let keeper = fx.add_session(now_ms() - DAY_MS, &[300], 200);
    let sessions_dir = fx.root.join("sessions");
    let buffer = add_buffer_row(&fx, now_ms() - 30 * DAY_MS, &sessions_dir);

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    assert_eq!(plan.sessions.ids(), vec![buffer]);
    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    assert_eq!(outcome.sessions.deleted, 1);
    assert!(keeper.all_exist(), "another session's footage is untouched");
    assert_eq!(fx.session_ids(), vec![keeper.id]);
}

#[test]
fn a_session_row_naming_something_outside_the_library_loses_the_row_and_never_the_files() {
    let mut fx = Fixture::new();
    let session = fx.add_session(now_ms() - 30 * DAY_MS, &[100], 0);

    // The row now names a directory and a file nobody at this application ever wrote, shaped to
    // pass every syntactic check: absolute, clean, a real directory, a session-looking name.
    let outside = tempfile::tempdir().expect("an unrelated directory");
    let their_dir = outside.path().join("session-7");
    std::fs::create_dir_all(&their_dir).expect("create");
    std::fs::write(their_dir.join("work.bin"), b"theirs").expect("write");
    let their_file = outside.path().join("session-7.mp4");
    std::fs::write(&their_file, b"theirs too").expect("write");
    let conn = rusqlite::Connection::open(&fx.db_path).expect("a second connection");
    conn.execute(
        "UPDATE sessions SET scratch_dir = ?2, final_path = ?3 WHERE id = ?1",
        rusqlite::params![
            session.id,
            their_dir.to_string_lossy().to_string(),
            their_file.to_string_lossy().to_string()
        ],
    )
    .expect("repoint the row");

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    assert_eq!(plan.sessions.ids(), vec![session.id]);
    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    assert_eq!(outcome.sessions.deleted, 1, "the row goes");
    assert_eq!(
        outcome.sessions.refused.len(),
        2,
        "both paths are reported: {outcome:?}"
    );
    assert!(
        their_dir.join("work.bin").is_file(),
        "the directory is untouched"
    );
    assert!(their_file.is_file(), "the file is untouched");
    assert_eq!(outcome.sessions.bytes_reclaimed, 0);
}

#[test]
fn a_scratch_directory_inside_the_sessions_directory_must_still_look_like_a_session() {
    let mut fx = Fixture::new();
    let session = fx.add_session(now_ms() - 30 * DAY_MS, &[100], 0);
    let odd = fx.root.join("sessions").join("my-vacation-videos");
    std::fs::create_dir_all(&odd).expect("create");
    std::fs::write(odd.join("v.mp4"), b"keep").expect("write");
    let conn = rusqlite::Connection::open(&fx.db_path).expect("a second connection");
    conn.execute(
        "UPDATE sessions SET scratch_dir = ?2 WHERE id = ?1",
        rusqlite::params![session.id, odd.to_string_lossy().to_string()],
    )
    .expect("repoint the row");

    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    assert_eq!(outcome.sessions.refused, vec![odd.clone()]);
    assert!(
        odd.join("v.mp4").is_file(),
        "this application did not create it, so it does not remove it"
    );
}

#[test]
fn a_session_favourited_after_the_plan_was_made_survives_the_pass() {
    let mut fx = Fixture::new();
    let late = fx.add_session(now_ms() - 30 * DAY_MS, &[100], 50);
    let plain = fx.add_session(now_ms() - 29 * DAY_MS, &[100], 50);
    let plan = fx.plan(&policy(generous(), rules(u64::MAX, 7)));
    assert_eq!(plan.sessions.ids(), vec![late.id, plain.id]);

    fx.favourite_session(late.id);

    let outcome = execute_retention(&fx.store, &plan, &fx.roots()).expect("execute the pass");

    assert_eq!(outcome.sessions.deleted, 1);
    assert_eq!(outcome.sessions.protected, 1, "the race is reported");
    assert!(late.all_exist(), "the favourited session's files survive");
    assert!(plain.none_exist());
    assert_eq!(fx.session_ids(), vec![late.id]);
    assert_eq!(
        outcome.sessions.bytes_after, late.bytes_on_disk,
        "and it still counts against the cap"
    );
}
