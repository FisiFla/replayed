//! Retention for the two classes of artefact this application keeps: clips and sessions.
//!
//! [`crate::cleanup`] is one flat policy over the clips directory. A full-session recording is a
//! different class (bigger, longer-lived, managed in whole sessions), and one shared
//! `max_total_bytes` would let whichever class grew first evict the other. So this module adds a
//! second policy beside the first:
//!
//! * a session store over its cap never evicts a clip,
//! * a clips store over its cap never evicts a session,
//! * both use the same two rules, the same reason flags and the same deletion ordering.
//!
//! The clips class of [`plan_retention`] is [`plan_cleanup`] called with the clips half of the
//! policy and converted into this module's plan shape, so the two entry points cannot disagree
//! about the same library.
//!
//! # Deliberate duplication
//!
//! * [`RetentionDeletion`] is [`crate::cleanup::PlannedDeletion`] with [`Subject`] in place of a
//!   bare `id`, so a plan naming sessions and clips says which class an id belongs to.
//! * [`ClassOutcome`] is [`crate::cleanup::CleanupOutcome`] plus the one field a class that can
//!   refuse to remove a path needs.
//!
//! Fusing them with their originals by widening the existing types is deferred: both are part of
//! signatures other crates build against. [`crate::cleanup::DeletionReasons`] is reused verbatim,
//! so both plans explain themselves identically.

use crate::cleanup::{plan_cleanup, CleanupPolicy, DeletionReasons};
use crate::{Clip, Session, Store};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Milliseconds in a day; the age rules' unit conversion. A private copy of the constant in
/// [`crate::cleanup`], where it is private.
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1_000;

/// Retention for one class of artefact: the age rule and the size cap, per class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionRules {
    /// Total bytes this class may occupy, favourites included. An unsatisfiable cap is reported,
    /// never paid for out of the class's other artefacts (see [`ClassPlan::over_cap_by_bytes`]).
    pub max_total_bytes: u64,
    /// A member of the class older than this many days is deleted, unless it is a favourite (or, for
    /// a session, still recording). As in [`CleanupPolicy::max_age_days`], `0` means everything older
    /// than `now_ms`, and the comparison is strictly greater.
    pub max_age_days: u32,
}

/// Sessions and clips are evicted independently: a session store at 100 GB must not push clips
/// out, and vice versa. Two independent rules over two libraries, not one shared cap that only
/// the class that grew first could satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub clips: RetentionRules,
    pub sessions: RetentionRules,
}

/// What a planned deletion refers to. Sessions are deleted as units: a session's segments are not independently addressable rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Clip(i64),
    Session(i64),
}

impl Subject {
    /// The row id, whichever class it is.
    pub fn id(self) -> i64 {
        match self {
            Subject::Clip(id) | Subject::Session(id) => id,
        }
    }

    /// `"clip"` or `"session"`, for a log line.
    pub fn class(self) -> &'static str {
        match self {
            Subject::Clip(_) => "clip",
            Subject::Session(_) => "session",
        }
    }
}

/// One artefact a plan deletes, with the bytes its row accounts for and why it is going.
///
/// The reasons are [`DeletionReasons`], shared with the clips policy: an artefact both rules
/// select appears once, flagged with both. `too_old` is a retention decision and `over_cap` a
/// size decision, which are different things to tell a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionDeletion {
    pub subject: Subject,
    pub size_bytes: u64,
    pub reasons: DeletionReasons,
}

/// What a plan does to one class, and where that leaves it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassPlan {
    /// Members to delete, oldest first (see [`plan_retention`]), each exactly once.
    pub deletions: Vec<RetentionDeletion>,
    /// The cap this class was planned against, carried so the executor checks the post-condition
    /// against the live store rather than this forecast.
    pub cap_bytes: u64,
    /// Total size of the class if every deletion here succeeds.
    pub bytes_after: u64,
    /// The part of the overshoot that no pass may ever remove: how far the members immune to
    /// eviction (favourites, and sessions still recording) exceed the cap.
    ///
    /// Non-zero means the cap is unsatisfiable, and the size rule selects nothing for this class:
    /// evicting the evictable could not reach the cap and would destroy material the user did not
    /// protect. The caller warns with this number instead.
    pub over_cap_by_bytes: u64,
}

impl ClassPlan {
    /// Whether applying this plan brings the class at or under its cap. Equal to
    /// `bytes_after <= cap_bytes`, for the reason [`crate::cleanup::CleanupPlan::cap_met`] gives.
    pub fn cap_met(&self) -> bool {
        self.over_cap_by_bytes == 0
    }

    /// The ids to delete, in plan order.
    pub fn ids(&self) -> Vec<i64> {
        self.deletions.iter().map(|d| d.subject.id()).collect()
    }
}

/// What one retention pass would do to both classes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetentionPlan {
    /// The clips half: exactly what [`plan_cleanup`] would decide for the same library under `policy.clips`.
    pub clips: ClassPlan,
    /// The sessions half.
    pub sessions: ClassPlan,
}

impl RetentionPlan {
    /// Whether applying this plan brings both classes at or under their caps.
    ///
    /// Per class, not in total: one class exceeding its cap is that class's problem, and a combined
    /// figure would hide which is over.
    pub fn cap_met(&self) -> bool {
        self.clips.cap_met() && self.sessions.cap_met()
    }

    /// Every deletion across both classes.
    pub fn deletions(&self) -> usize {
        self.clips.deletions.len() + self.sessions.deletions.len()
    }

    /// Whether this pass has nothing to do.
    pub fn is_empty(&self) -> bool {
        self.deletions() == 0
    }
}

/// Decide what one pass deletes from the clips library and the session store, touching neither
/// the filesystem nor the database.
///
/// The rules are the clips policy's, applied to each class independently with its own cap and age limit:
///
/// 1. Age: delete every non-favourite member older than its class's `max_age_days`.
/// 2. Size: evict non-favourite members least-recently-used first until the class is at or under
///    its `max_total_bytes`. If the immune members alone exceed it, the rule is suspended and
///    the shortfall reported.
/// 3. Favourites are exempt from both rules, in both classes.
///
/// # What "least recently used" means, per class
///
/// Rows are immutable and nothing reads an artefact back, so the ordering is the one instant
/// comparable across runs (as for [`plan_cleanup`]).
///
/// * A clip is aged and ordered by `clips.created_at`, stamped by [`Store::insert_clip`]. The
///   clips half is [`plan_cleanup`] itself, not a second implementation.
/// * A session is aged and ordered by `sessions.started_at`, when its footage begins: a session
///   that started eight days ago and ended yesterday is evicted by a seven-day limit. Aging by
///   `ended_at` would make a session immortal while it kept recording.
///
/// Ties are broken by id in both classes, so a plan is deterministic for a given library and `now_ms`.
///
/// # A session that is still recording
///
/// A session whose `ended_at` is NULL is immune to both rules, more strongly than a favourite: its
/// scratch directory is footage that exists nowhere else yet, so deleting it destroys the only copy.
///
/// * the age rule and the size rule do not select it;
/// * its bytes are counted towards the cap (they are on disk) and in
///   [`ClassPlan::over_cap_by_bytes`], so a live recording that alone exceeds the cap is
///   reported as an unsatisfiable cap rather than paid for out of finished sessions.
///
/// The plan is pure and cannot see the process table, so "still recording" is exactly
/// `ended_at IS NULL`, for planner and executor alike. A session left running by a crash stays
/// immune until [`Store::end_session`] ends it explicitly.
///
/// # Session bytes, and the one number that can lag
///
/// The cap uses `sessions.size_bytes`, stamped by [`Store::end_session`] and kept current while
/// running by [`Store::set_session_size`]. A recorder that does not call the latter plans against
/// 0 bytes for its live session until it stops: a bounded lag, not a wrong eviction, since a
/// running session is never a candidate.
pub fn plan_retention(
    clips: &[Clip],
    sessions: &[Session],
    policy: &RetentionPolicy,
    now_ms: i64,
) -> RetentionPlan {
    // The clips half is called, not reimplemented, so it cannot evict different clips than `plan_cleanup`.
    let flat = plan_cleanup(clips, &clip_rules(policy.clips), now_ms);
    let clips_class = ClassPlan {
        deletions: flat
            .deletions
            .iter()
            .map(|d| RetentionDeletion {
                subject: Subject::Clip(d.id),
                size_bytes: d.size_bytes,
                reasons: d.reasons,
            })
            .collect(),
        cap_bytes: flat.cap_bytes,
        bytes_after: flat.bytes_after,
        over_cap_by_bytes: flat.over_cap_by_bytes,
    };

    RetentionPlan {
        clips: clips_class,
        sessions: plan_sessions(sessions, &policy.sessions, now_ms),
    }
}

/// The clips half of a [`RetentionPolicy`] in the shape [`plan_cleanup`] takes. The only place the two policy types meet.
fn clip_rules(rules: RetentionRules) -> CleanupPolicy {
    CleanupPolicy {
        max_total_bytes: rules.max_total_bytes,
        // `CleanupPolicy` counts days as u64; the retention policy uses u32 for a day count.
        max_age_days: rules.max_age_days as u64,
    }
}

/// The sessions class's planner: the same two rules over session rows.
///
/// Structurally parallel to [`plan_cleanup`] (same order, marking and unsatisfiable-cap rule). See
/// [`plan_retention`] for what a session adds: a different LRU instant, whole-session units, and
/// immunity while recording.
fn plan_sessions(sessions: &[Session], rules: &RetentionRules, now_ms: i64) -> ClassPlan {
    let max_age_ms = (rules.max_age_days as i64).saturating_mul(MS_PER_DAY);

    // Oldest-started first, deterministic under ties. Both rules walk this one order.
    let mut ordered: Vec<&Session> = sessions.iter().collect();
    ordered.sort_by_key(|s| (s.started_at_ms, s.id));

    // One slot per session, in `ordered` order: "mark, never push twice" keeps a session both rules select in the plan once.
    let mut marked: Vec<Option<DeletionReasons>> = vec![None; ordered.len()];

    // A negative `size_bytes` (a hand-edited row, an overflowed writer) counts as zero, as `total_bytes` clamps it.
    let bytes = |s: &Session| s.size_bytes.max(0) as u64;

    // The sums saturate: the bytes come from rows the index does not control, and a store that
    // claims more than a `u64` holds must over-count rather than panic in the middle of a pass.
    let mut remaining: u64 = sessions
        .iter()
        .fold(0, |total, s| total.saturating_add(bytes(s)));
    for (i, session) in ordered.iter().enumerate() {
        if !exempt(session) && now_ms.saturating_sub(session.started_at_ms) > max_age_ms {
            marked[i] = Some(DeletionReasons {
                too_old: true,
                over_cap: false,
            });
            remaining = remaining.saturating_sub(bytes(session));
        }
    }

    // Is the cap reachable at all? Only while the bytes nothing may evict are within it.
    let immune_bytes: u64 = sessions
        .iter()
        .filter(|s| exempt(s))
        .fold(0, |total, s| total.saturating_add(bytes(s)));
    let over_cap_by_bytes = immune_bytes.saturating_sub(rules.max_total_bytes);

    if over_cap_by_bytes == 0 {
        for (i, session) in ordered.iter().enumerate() {
            if remaining <= rules.max_total_bytes {
                break;
            }
            if exempt(session) {
                continue;
            }
            match &mut marked[i] {
                Some(reasons) => reasons.over_cap = true,
                None => {
                    marked[i] = Some(DeletionReasons {
                        too_old: false,
                        over_cap: true,
                    });
                    remaining = remaining.saturating_sub(bytes(session));
                }
            }
        }
    }

    let deletions = ordered
        .iter()
        .zip(&marked)
        .filter_map(|(session, reasons)| {
            reasons.map(|reasons| RetentionDeletion {
                subject: Subject::Session(session.id),
                size_bytes: bytes(session),
                reasons,
            })
        })
        .collect();

    ClassPlan {
        deletions,
        cap_bytes: rules.max_total_bytes,
        bytes_after: remaining,
        over_cap_by_bytes,
    }
}

/// Whether a session is out of the retention rules' reach: favourited, or still recording.
fn exempt(session: &Session) -> bool {
    session.favourite || session.ended_at_ms.is_none()
}

/// What one retention pass actually did to one class, and to the disk under it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassOutcome {
    /// Rows this pass deleted and committed.
    pub deleted: usize,
    /// Planned deletions that were refused or failed; each is logged where it happens. Counted, not
    /// propagated, so one unreadable row does not abandon the pass.
    pub failed: usize,
    /// Bytes this pass actually removed from the disk, measured file by file as removed, not
    /// forecast from the rows, so a row that disagrees with its file still reports what went away.
    pub bytes_reclaimed: u64,
    /// Paths a deleted row named whose removal failed: on disk with no row naming them, for a later sweep or the user.
    pub orphaned: Vec<PathBuf>,
    /// Paths a deleted row named that were already absent. The end state is right, but a row pointing at nothing is worth reporting.
    pub already_missing: Vec<PathBuf>,
    /// Paths deliberately not removed because removing them could destroy something outside this
    /// class (a scratch directory that is a symlink, a bare relative name, a filesystem root). The
    /// path stays on disk, the row is already gone, and the caller is told.
    pub refused: Vec<PathBuf>,
    /// Total size of the class after the pass, re-read from the database.
    pub bytes_after: u64,
    /// Whether the class is at or under the plan's cap after the pass, measured rather than forecast.
    pub cap_met: bool,
}

/// What one retention pass did to both classes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionOutcome {
    pub clips: ClassOutcome,
    pub sessions: ClassOutcome,
}

impl RetentionOutcome {
    /// Rows deleted across both classes.
    pub fn deleted(&self) -> usize {
        self.clips.deleted + self.sessions.deleted
    }

    /// Bytes removed from the disk across both classes.
    pub fn bytes_reclaimed(&self) -> u64 {
        self.clips.bytes_reclaimed + self.sessions.bytes_reclaimed
    }

    /// Whether both classes ended at or under their own cap.
    pub fn cap_met(&self) -> bool {
        self.clips.cap_met && self.sessions.cap_met
    }

    /// Every path left behind that a log line should name.
    pub fn leftovers(&self) -> impl Iterator<Item = &PathBuf> {
        self.clips
            .orphaned
            .iter()
            .chain(&self.clips.refused)
            .chain(&self.sessions.orphaned)
            .chain(&self.sessions.refused)
    }
}

/// Apply a retention plan: for every deletion, delete the row and commit, then remove what that
/// committed delete handed back.
///
/// The same safety property as [`crate::cleanup::execute_cleanup`]: delete the row via
/// [`Store::delete_clip_returning_path`], which returns a path only when its own `DELETE` removed
/// a row, then unlink that path and nothing else. A file is removed because a committed row named
/// it, never on a plan's say-so.
///
/// For a session the row delete is [`Store::delete_session_returning_paths`], returning two things:
///
/// * the final file, removed like a clip's, with a failed or already-missing removal reported alike;
/// * the scratch directory, removed as a tree. That is the most destructive operation in this
///   crate, so it is guarded: the path must look like a directory this application could have
///   created (non-empty, with a final component, a real directory rather than a symlink) before
///   anything inside is touched (see [`remove_scratch_dir`]). A refused path is reported in
///   [`ClassOutcome::refused`] and left as is.
///
/// A session still recording cannot reach the removal code: the store refuses its row delete,
/// which counts as a failed deletion and is logged. The planner never selects one, so that means
/// a hand-built plan or one held across the moment the session started; both warrant a warning, not a crash.
///
/// Errors are per-row: a failed row is counted in [`ClassOutcome::failed`] and logged instead of
/// ending the pass. The returned `Err` is reserved for the final re-read of the store.
pub fn execute_retention(store: &Store, plan: &RetentionPlan) -> Result<RetentionOutcome> {
    let mut outcome = RetentionOutcome::default();

    // Clips first, then sessions: the plan's order, which leaves a session's links to its clips detached by the time the clips go.
    for entry in &plan.clips.deletions {
        let Subject::Clip(id) = entry.subject else {
            tracing::warn!(
                "retention: the plan lists {} #{} under clips; skipping it",
                entry.subject.class(),
                entry.subject.id()
            );
            outcome.clips.failed += 1;
            continue;
        };
        match store.delete_clip_returning_path(id) {
            Ok(Some(path)) => {
                // That DELETE ran in autocommit: it is committed before this line.
                outcome.clips.deleted += 1;
                let path = PathBuf::from(path);
                record(&mut outcome.clips, &path, unlink(&path), entry.size_bytes);
            }
            Ok(None) => {
                tracing::debug!(
                    "retention: clip #{id} had no row left to delete; nothing was removed"
                );
            }
            Err(err) => {
                tracing::warn!("retention: could not delete the row for clip #{id}: {err:#}");
                outcome.clips.failed += 1;
            }
        }
    }

    for entry in &plan.sessions.deletions {
        let Subject::Session(id) = entry.subject else {
            tracing::warn!(
                "retention: the plan lists {} #{} under sessions; skipping it",
                entry.subject.class(),
                entry.subject.id()
            );
            outcome.sessions.failed += 1;
            continue;
        };
        let paths = match store.delete_session_returning_paths(id) {
            Ok(Some(paths)) => paths,
            Ok(None) => {
                tracing::debug!(
                    "retention: session #{id} had no row left to delete; nothing was removed"
                );
                continue;
            }
            Err(err) => {
                // The expected reason is the store refusing a session still recording; any other is a real
                // failure. Either way the session is left whole, the safe direction.
                tracing::warn!("retention: session #{id} was not deleted: {err:#}");
                outcome.sessions.failed += 1;
                continue;
            }
        };
        // Committed before this line, as for a clip.
        outcome.sessions.deleted += 1;

        if let Some(final_path) = paths.final_path.as_deref() {
            let final_path = PathBuf::from(final_path);
            record(
                &mut outcome.sessions,
                &final_path,
                unlink(&final_path),
                entry.size_bytes,
            );
        }
        let scratch = PathBuf::from(&paths.scratch_dir);
        record(
            &mut outcome.sessions,
            &scratch,
            remove_scratch_dir(&scratch),
            entry.size_bytes,
        );
    }

    outcome.clips.bytes_after = store.total_bytes()?;
    outcome.clips.cap_met = outcome.clips.bytes_after <= plan.clips.cap_bytes;
    outcome.sessions.bytes_after = store.total_session_bytes()?;
    outcome.sessions.cap_met = outcome.sessions.bytes_after <= plan.sessions.cap_bytes;
    Ok(outcome)
}

/// What one attempt to remove a path did. One type for files and trees, so bookkeeping is written once.
///
/// Not `PartialEq`: `Removal::Failed` carries an `io::Error`, which does not compare. Tests match on the variant.
#[derive(Debug)]
enum Removal {
    /// Removed. The `u64` is what it actually held, measured from the disk first.
    Removed(u64),
    /// Not there in the first place.
    Missing,
    /// Deliberately not removed, for the stated reason. The path is untouched.
    Refused(&'static str),
    Failed(std::io::Error),
}

/// Fold one removal attempt into a class's outcome.
///
/// Every non-removal lands in one of the three path lists, so an outcome is a complete account of
/// what a pass could not remove, not dependent on a log line nobody reads.
///
/// `planned_bytes` only feeds a debug line noticing a file differed from its row; the bytes
/// counted into `bytes_reclaimed` are the measured ones.
fn record(outcome: &mut ClassOutcome, path: &Path, removal: Removal, planned_bytes: u64) {
    match removal {
        Removal::Removed(actual) => {
            outcome.bytes_reclaimed = outcome.bytes_reclaimed.saturating_add(actual);
            if actual != planned_bytes {
                tracing::debug!(
                    "retention: removed {actual} bytes from {} where the row claimed \
                     {planned_bytes}",
                    path.display()
                );
            }
        }
        Removal::Missing => {
            tracing::warn!(
                "retention: a deleted row named {} and it was already gone — the index and the \
                 disk disagreed about it",
                path.display()
            );
            outcome.already_missing.push(path.to_path_buf());
        }
        Removal::Refused(reason) => {
            tracing::warn!(
                "retention: refused to remove {} ({reason}); it is left exactly as it is",
                path.display()
            );
            outcome.refused.push(path.to_path_buf());
        }
        Removal::Failed(err) => {
            tracing::warn!(
                "retention: a deleted row named {} and it could not be removed ({err}); it is \
                 orphaned on disk and needs a sweep",
                path.display()
            );
            outcome.orphaned.push(path.to_path_buf());
        }
    }
}

/// Remove one regular file, measuring what actually went away.
///
/// Measured from the disk before the unlink, not the row, so a row that disagrees with its file
/// does not make the shown number a lie. `NotFound` is reported as such, not as a silent zero.
fn unlink(path: &Path) -> Removal {
    if !crate::cleanup::is_absolute_and_clean(path) {
        return Removal::Refused("it is relative or contains `..`, so it names no fixed place");
    }
    let bytes = match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_file() {
                meta.len()
            } else {
                // A directory (or symlink) where a file was expected: `remove_file` below refuses it, and there are no bytes of this path to count.
                0
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Removal::Missing,
        Err(err) => return Removal::Failed(err),
    };
    match std::fs::remove_file(path) {
        Ok(()) => Removal::Removed(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Removal::Missing,
        Err(err) => Removal::Failed(err),
    }
}

/// Remove a session's scratch directory and everything in it, measuring the tree first.
///
/// The one place this crate removes something it did not write, and the path comes from a TEXT
/// column a bug or hand-edited index could fill with anything. `remove_dir_all` given `/`, `.`,
/// `..` or a bare relative name is a catastrophe, so the path must look like a directory this
/// application could have created before an entry is touched:
///
/// * non-empty with a final path component, which rules out `.`, `..` and filesystem roots;
/// * a real directory, not a symlink (the walk would go wherever it points) and not a plain file.
///
/// Anything else is [`Removal::Refused`] with the reason. A path that is simply gone is
/// [`Removal::Missing`], an ordinary end state (a session-mode recorder may remove its own
/// scratch directory once the final file exists), not a refusal.
///
/// The tree is measured before removal, and a tree that cannot be measured is not removed: an
/// unreadable directory is a question for a human.
fn remove_scratch_dir(path: &Path) -> Removal {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return Removal::Refused(
            "no final path component: it names a filesystem root, '.' or '..'",
        );
    }
    if !crate::cleanup::is_absolute_and_clean(path) {
        return Removal::Refused("it is relative or contains `..`, so it names no fixed place");
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Removal::Missing,
        Err(err) => return Removal::Failed(err),
    };
    if meta.is_symlink() {
        return Removal::Refused("it is a symlink, and removing it would reach past the path");
    }
    if !meta.is_dir() {
        return Removal::Refused("it is not a directory");
    }
    let bytes = match tree_bytes(path) {
        Ok(bytes) => bytes,
        Err(err) => return Removal::Failed(err),
    };
    match std::fs::remove_dir_all(path) {
        Ok(()) => Removal::Removed(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Removal::Missing,
        Err(err) => Removal::Failed(err),
    }
}

/// The bytes a directory tree holds: every regular file in it, to any depth. Symlinks count as
/// zero and are never followed; a symlink is a path elsewhere, not bytes this application wrote.
fn tree_bytes(path: &Path) -> std::io::Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Ok(0);
    }
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        total = total.saturating_add(tree_bytes(&entry?.path())?);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SESSION_MODE_BUFFER, SESSION_MODE_SESSION};
    use std::path::PathBuf;

    const DAY: i64 = MS_PER_DAY;

    fn rules(max_total_bytes: u64, max_age_days: u32) -> RetentionRules {
        RetentionRules {
            max_total_bytes,
            max_age_days,
        }
    }

    /// A policy whose clips and sessions halves are set independently, which every independence test varies.
    fn policy(clips: RetentionRules, sessions: RetentionRules) -> RetentionPolicy {
        RetentionPolicy { clips, sessions }
    }

    /// A cap nothing reaches and an age nothing exceeds: the plan must be a no-op.
    fn generous() -> RetentionRules {
        rules(u64::MAX, 3_650)
    }

    fn clip(id: i64, created_at_ms: i64, size_bytes: u64, favourite: bool) -> Clip {
        Clip {
            id,
            kind: crate::CLIP_KIND_CLIP.to_string(),
            path: PathBuf::from(format!("/clips/{id}.mp4")),
            started_at_ms: 0,
            duration_ms: 1_000,
            size_bytes,
            codec: "h264_nvenc".into(),
            favourite,
            created_at_ms,
            title: None,
            game: None,
            tags: Vec::new(),
        }
    }

    /// A finished session as `Store::list_sessions` would hand it to `plan_retention`.
    fn session(id: i64, started_at_ms: i64, size_bytes: u64, favourite: bool) -> Session {
        Session {
            id,
            game: Some("League of Legends".into()),
            mode: SESSION_MODE_SESSION.into(),
            started_at_ms,
            // These sessions started against an empty scratch directory: the ordinary case and what every pre-v3 row means.
            media_epoch_ms: 0,
            ended_at_ms: Some(started_at_ms + 60_000),
            scratch_dir: format!("/scratch/{id}"),
            final_path: Some(format!("/sessions/{id}.mp4")),
            size_bytes: size_bytes as i64,
            // A minute of media matching the minute between `started_at` and `ended_at`: these sessions'
            // encoder kept up (not generally true, see `Session::duration_ms`, but the case a planner sees).
            duration_ms: 60_000,
            favourite,
        }
    }

    /// The same row, still recording: `ended_at` is NULL and there is no final file yet.
    fn running(id: i64, started_at_ms: i64, size_bytes: u64) -> Session {
        Session {
            ended_at_ms: None,
            final_path: None,
            ..session(id, started_at_ms, size_bytes, false)
        }
    }

    #[test]
    fn a_store_within_both_classes_rules_plans_nothing() {
        let now = 10 * DAY;
        let clips = vec![clip(1, now - 2 * DAY, 1_000, false)];
        let sessions = vec![session(1, now - DAY, 5_000, false)];

        let plan = plan_retention(&clips, &sessions, &policy(generous(), generous()), now);

        assert!(plan.is_empty());
        assert!(plan.cap_met());
        assert_eq!(plan.clips.bytes_after, 1_000);
        assert_eq!(plan.sessions.bytes_after, 5_000);
    }

    #[test]
    fn the_clips_half_is_exactly_what_the_flat_policy_would_decide() {
        // The clips class is `plan_cleanup` itself, converted, so a retention pass and a cleanup pass cannot disagree on the same clips.
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - 40 * DAY, 100, false), // over age, and the first eviction candidate
            clip(2, now - 2 * DAY, 100, false),
            clip(3, now - 2 * DAY, 5, true),
        ];
        let flat = plan_cleanup(&clips, &clip_rules(rules(10, 7)), now);
        let plan = plan_retention(&clips, &[], &policy(rules(10, 7), generous()), now);

        assert_eq!(plan.clips.ids(), flat.ids());
        assert_eq!(plan.clips.bytes_after, flat.bytes_after);
        assert_eq!(plan.clips.over_cap_by_bytes, flat.over_cap_by_bytes);
        assert_eq!(plan.clips.cap_bytes, flat.cap_bytes);
        assert_eq!(
            plan.clips.deletions[0],
            RetentionDeletion {
                subject: Subject::Clip(1),
                size_bytes: 100,
                reasons: DeletionReasons {
                    too_old: true,
                    over_cap: true
                },
            },
            "the reasons travel with the deletion, both of them"
        );
    }

    #[test]
    fn the_two_classes_are_independent_in_both_directions() {
        let now = 1_000 * DAY;
        let clips = vec![clip(1, now - 40 * DAY, 100, false)];
        let sessions = vec![session(7, now - 40 * DAY, 900, false)];

        // A sessions cap of zero must not evict a clip, however small the clips library.
        let plan = plan_retention(&clips, &sessions, &policy(generous(), rules(0, 3_650)), now);
        assert_eq!(plan.sessions.ids(), vec![7]);
        assert!(
            plan.clips.deletions.is_empty(),
            "a session cap is not a clip cap"
        );

        // And a clips cap of zero must not evict a session.
        let plan = plan_retention(&clips, &sessions, &policy(rules(0, 3_650), generous()), now);
        assert_eq!(plan.clips.ids(), vec![1]);
        assert!(
            plan.sessions.deletions.is_empty(),
            "a clip cap is not a session cap"
        );
    }

    #[test]
    fn the_sessions_age_rule_uses_started_at_and_not_ended_at() {
        let now = 100 * DAY;
        // Ended yesterday, but it started eight days ago: the footage is eight days old.
        let sessions = vec![session(1, now - 8 * DAY, 10, false)];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(u64::MAX, 7)), now);

        assert_eq!(plan.sessions.ids(), vec![1]);
        assert!(plan.sessions.deletions[0].reasons.too_old);
        assert!(!plan.sessions.deletions[0].reasons.over_cap);
    }

    #[test]
    fn the_sessions_age_limit_is_a_strict_threshold() {
        let now = 3 * DAY;
        let sessions = vec![
            session(1, now - DAY, 1, false), // exactly one day old, one day limit: kept
            session(2, now - DAY - 1, 1, false), // one ms older: evicted
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(u64::MAX, 1)), now);

        assert_eq!(plan.sessions.ids(), vec![2]);
    }

    #[test]
    fn the_sessions_size_rule_evicts_oldest_started_first() {
        let now = 1_000 * DAY;
        // Handed in a deliberately scrambled order: the plan must not depend on it.
        let sessions = vec![
            session(3, now - 3 * DAY, 100, false),
            session(1, now - 9 * DAY, 100, false),
            session(2, now - 6 * DAY, 100, false),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(150, 3_650)), now);

        assert_eq!(
            plan.sessions.ids(),
            vec![1, 2],
            "oldest started first, until under the cap"
        );
        assert_eq!(
            plan.sessions.bytes_after, 100,
            "100 bytes is at the cap, not over it"
        );
        assert!(plan.cap_met());
        assert!(plan
            .sessions
            .deletions
            .iter()
            .all(|d| d.reasons.over_cap && !d.reasons.too_old));
    }

    #[test]
    fn a_session_both_rules_select_appears_once_with_both_reasons() {
        let now = 1_000 * DAY;
        let sessions = vec![
            session(1, now - 40 * DAY, 100, false), // over age, and the oldest candidate
            session(2, now - DAY, 100, false),
            session(3, now - DAY, 5, true),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(10, 7)), now);

        assert_eq!(plan.sessions.ids(), vec![1, 2], "one entry per session");
        assert_eq!(
            plan.sessions.deletions[0].reasons,
            DeletionReasons {
                too_old: true,
                over_cap: true
            }
        );
        assert_eq!(
            plan.sessions.bytes_after, 5,
            "bytes counted once, however selected"
        );
    }

    #[test]
    fn favourites_are_immune_in_both_classes_and_by_both_rules() {
        let now = 1_000 * DAY;
        // Clips: an ancient favourite and a favourite that alone exceeds the clip cap.
        let clips = vec![
            clip(1, now - 90 * DAY, 4_000, true),
            clip(2, now - 90 * DAY, 6_000, true),
            clip(3, now - DAY, 100, false),
        ];
        // Sessions: the same two situations.
        let sessions = vec![
            session(1, now - 90 * DAY, 4_000, true),
            session(2, now - 90 * DAY, 6_000, true),
            session(3, now - DAY, 100, false),
        ];

        let plan = plan_retention(&clips, &sessions, &policy(rules(10, 1), rules(10, 1)), now);

        assert_eq!(
            plan.clips.ids(),
            Vec::<i64>::new(),
            "no clip is evicted for a cap it cannot meet"
        );
        assert_eq!(
            plan.sessions.ids(),
            Vec::<i64>::new(),
            "and no session either"
        );
        assert_eq!(plan.clips.over_cap_by_bytes, 9_990);
        assert_eq!(plan.sessions.over_cap_by_bytes, 9_990);
        assert!(
            !plan.cap_met(),
            "and both shortfalls are reported rather than paid for"
        );
        assert_eq!(plan.clips.bytes_after, 10_100);
        assert_eq!(plan.sessions.bytes_after, 10_100);
    }

    #[test]
    fn a_session_that_is_still_recording_is_immune_to_both_rules() {
        let now = 1_000 * DAY;
        let sessions = vec![
            running(1, now - 90 * DAY, 100), // ancient, huge, and recording
            session(2, now - 90 * DAY, 10_000, true), // an ancient favourite
            session(3, now - DAY, 100, false), // recent and small: nothing wants it
        ];

        // Every rule violated as hard as it can be: 1 day of age, 10 bytes of cap.
        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(10, 1)), now);

        assert!(
            plan.sessions.deletions.is_empty(),
            "a running session is not evictable, and the cap it breaks is unsatisfiable: got {:?}",
            plan.sessions.ids()
        );
        assert_eq!(
            plan.sessions.over_cap_by_bytes, 10_090,
            "the immutable bytes — the running session plus the favourite — are the shortfall"
        );
        assert!(!plan.cap_met());
        assert_eq!(plan.sessions.bytes_after, 10_200, "nothing was deleted");
    }

    #[test]
    fn an_unsatisfiable_session_cap_does_not_suspend_the_age_rule() {
        let now = 1_000 * DAY;
        let sessions = vec![
            running(1, now - 90 * DAY, 200),        // recording: exempt
            session(2, now - 30 * DAY, 400, false), // ancient, finished: the age rule's job
            session(3, now - DAY, 100, false),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(10, 7)), now);

        assert_eq!(
            plan.sessions.ids(),
            vec![2],
            "the age rule is independent of the cap"
        );
        assert!(plan.sessions.deletions[0].reasons.too_old);
        assert!(!plan.cap_met());
        assert_eq!(
            plan.sessions.over_cap_by_bytes, 190,
            "200 of running session against 10"
        );
        assert_eq!(plan.sessions.bytes_after, 300);
    }

    #[test]
    fn a_negative_size_counts_as_zero_rather_than_underflowing_the_total() {
        let now = 1_000 * DAY;
        let sessions = vec![
            Session {
                size_bytes: -5,
                ..session(1, now - 40 * DAY, 0, false)
            },
            session(2, now - 39 * DAY, 100, false),
        ];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(0, 3_650)), now);

        assert_eq!(plan.sessions.ids(), vec![1, 2]);
        assert_eq!(
            plan.sessions.bytes_after, 0,
            "a corrupt size never subtracts from the total"
        );
    }

    #[test]
    fn a_session_started_in_the_future_is_never_too_old() {
        // A clock that stepped backwards between two runs must not make the newest session look the oldest.
        let now = 10 * DAY;
        let sessions = vec![session(1, now + 5 * DAY, 1_000, false)];

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(u64::MAX, 1)), now);

        assert!(plan.sessions.deletions.is_empty());
        assert_eq!(plan.sessions.bytes_after, 1_000);
    }

    // ------------------------------------------------------------ the executor

    // A store the executor can run against, for plans built by hand rather than by `plan_retention`.
    fn store() -> Store {
        let store = Store::open_in_memory().expect("an in-memory store");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn the_executor_skips_a_plan_entry_that_is_the_wrong_class() {
        // Defensive: `plan_retention` cannot produce this, but an executor that trusted it would delete a session when asked about a clip.
        let store = store();
        let session_id = store
            .start_session(None, SESSION_MODE_BUFFER, 0, "/scratch/keep", 0)
            .expect("open a session");
        store
            .end_session(session_id, 1, None, 0, 0)
            .expect("end it");

        let plan = RetentionPlan {
            clips: ClassPlan {
                deletions: vec![RetentionDeletion {
                    subject: Subject::Session(session_id),
                    size_bytes: 0,
                    reasons: DeletionReasons {
                        too_old: true,
                        over_cap: false,
                    },
                }],
                cap_bytes: 0,
                bytes_after: 0,
                over_cap_by_bytes: 0,
            },
            sessions: ClassPlan::default(),
        };

        let outcome = execute_retention(&store, &plan).expect("the pass completes");

        assert_eq!(outcome.clips.deleted, 0);
        assert_eq!(
            outcome.clips.failed, 1,
            "the mis-classed entry is counted, not obeyed"
        );
        assert!(
            store.get_session(session_id).expect("read").is_some(),
            "the session survives"
        );
    }

    #[test]
    fn the_executor_refuses_a_running_session_even_when_a_plan_names_it() {
        // The planner never selects one. A plan that does (hand-built, or held across the moment the
        // session started) must still not be a way to lose a recording.
        let store = store();
        let running_id = store
            .start_session(
                Some("League of Legends"),
                SESSION_MODE_SESSION,
                1_000,
                "/scratch/live",
                0,
            )
            .expect("open a session");

        let plan = RetentionPlan {
            clips: ClassPlan::default(),
            sessions: ClassPlan {
                deletions: vec![RetentionDeletion {
                    subject: Subject::Session(running_id),
                    size_bytes: 0,
                    reasons: DeletionReasons {
                        too_old: true,
                        over_cap: false,
                    },
                }],
                cap_bytes: u64::MAX,
                bytes_after: 0,
                over_cap_by_bytes: 0,
            },
        };

        let outcome = execute_retention(&store, &plan).expect("the pass completes");

        assert_eq!(outcome.sessions.deleted, 0);
        assert_eq!(
            outcome.sessions.failed, 1,
            "the refusal is reported, not swallowed"
        );
        assert_eq!(
            outcome.sessions.bytes_reclaimed, 0,
            "and nothing was removed"
        );
        assert!(
            store.get_session(running_id).expect("read").is_some(),
            "the recording's row is still there"
        );
    }

    #[test]
    fn a_plan_id_whose_row_is_already_gone_removes_nothing() {
        // The property the deletion ordering exists for: with no committed row delete there is no authority to remove anything.
        let store = store();
        let id = store
            .start_session(None, SESSION_MODE_BUFFER, 0, "/scratch/absent", 0)
            .expect("open a session");
        store.end_session(id, 1, None, 0, 0).expect("end it");
        let plan = RetentionPlan {
            clips: ClassPlan::default(),
            sessions: ClassPlan {
                deletions: vec![RetentionDeletion {
                    subject: Subject::Session(id),
                    size_bytes: 0,
                    reasons: DeletionReasons {
                        too_old: true,
                        over_cap: false,
                    },
                }],
                cap_bytes: u64::MAX,
                bytes_after: 0,
                over_cap_by_bytes: 0,
            },
        };
        assert!(store
            .delete_session_returning_paths(id)
            .expect("delete it")
            .is_some());

        let outcome = execute_retention(&store, &plan).expect("the pass completes");

        assert_eq!(outcome.sessions.deleted, 0);
        assert_eq!(
            outcome.sessions.failed, 0,
            "a row already gone is not a failure"
        );
        assert!(outcome.sessions.already_missing.is_empty());
        assert!(outcome.sessions.orphaned.is_empty());
        assert!(outcome.sessions.refused.is_empty());
    }

    // ------------------------------------------------- removal guards (no temp dirs)

    #[test]
    fn a_scratch_directory_with_no_final_component_is_refused() {
        // These paths make `remove_dir_all` a catastrophe; the guard is why this module can hold a path from a TEXT column at all.
        for path in ["", ".", "..", "/"] {
            assert!(
                matches!(remove_scratch_dir(Path::new(path)), Removal::Refused(_)),
                "{path:?} must be refused, never recursed into"
            );
        }
    }

    #[test]
    fn relative_and_dotdot_paths_are_refused_for_files_and_directories_alike() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let dotdot = dir.path().join("sub").join("..").join("x");
        for path in [
            Path::new("clips/a.mp4"),
            Path::new("a.mp4"),
            dotdot.as_path(),
        ] {
            assert!(
                matches!(unlink(path), Removal::Refused(_)),
                "{path:?} (file)"
            );
            assert!(
                matches!(remove_scratch_dir(path), Removal::Refused(_)),
                "{path:?} (dir)"
            );
        }
        // An absolute, clean path that is merely absent is still just Missing.
        assert!(matches!(
            unlink(&dir.path().join("gone.mp4")),
            Removal::Missing
        ));
    }

    #[test]
    fn a_file_where_a_scratch_directory_should_be_is_refused_not_unlinked() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"bytes").expect("write it");

        match remove_scratch_dir(&file) {
            Removal::Refused(reason) => assert!(reason.contains("not a directory"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(file.is_file(), "and the file it refused is untouched");
    }

    #[test]
    fn a_symlinked_scratch_directory_is_refused_rather_than_followed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create the target");
        std::fs::write(elsewhere.join("keep.bin"), b"not ours").expect("write into it");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&elsewhere, &link).expect("symlink");

        match remove_scratch_dir(&link) {
            Removal::Refused(reason) => assert!(reason.contains("symlink"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            elsewhere.join("keep.bin").is_file(),
            "the tree it pointed at is untouched"
        );
    }

    #[test]
    fn a_scratch_directory_that_is_already_gone_is_missing_not_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let removal = remove_scratch_dir(&dir.path().join("never-existed"));
        assert!(matches!(removal, Removal::Missing), "got {removal:?}");
    }

    #[test]
    fn removing_a_scratch_directory_reports_the_bytes_the_tree_really_held() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let scratch = dir.path().join("scratch-1");
        std::fs::create_dir_all(scratch.join("nested")).expect("create the tree");
        std::fs::write(scratch.join("seg-1.mkv"), vec![0u8; 400]).expect("segment");
        std::fs::write(scratch.join("nested").join("seg-2.mkv"), vec![0u8; 300]).expect("segment");

        // What is really on disk, read back file by file: the measurement is checked against the files written, not a number the code produced.
        let seg_1 = std::fs::metadata(scratch.join("seg-1.mkv"))
            .expect("stat")
            .len();
        let seg_2 = std::fs::metadata(scratch.join("nested").join("seg-2.mkv"))
            .expect("stat")
            .len();
        assert_eq!(
            seg_1 + seg_2,
            700,
            "the fixture wrote 700 bytes into the tree"
        );

        match remove_scratch_dir(&scratch) {
            Removal::Removed(bytes) => {
                assert_eq!(bytes, seg_1 + seg_2, "measured from the tree, not assumed")
            }
            other => panic!("expected a removal, got {other:?}"),
        }
        assert!(!scratch.exists(), "and the tree is gone");
    }

    #[test]
    fn a_missing_file_is_reported_and_a_directory_is_not_unlinked_as_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let missing = unlink(&dir.path().join("gone.mp4"));
        assert!(matches!(missing, Removal::Missing), "got {missing:?}");

        let file = dir.path().join("clip.mp4");
        std::fs::write(&file, vec![0u8; 123]).expect("write the clip");
        let removed = unlink(&file);
        assert!(
            matches!(removed, Removal::Removed(123)),
            "the bytes it really held: {removed:?}"
        );
        assert!(!file.exists());

        // A directory where a file was expected: `remove_file` refuses it, and the caller reports an orphan rather than recursively deleting something it never indexed.
        let as_dir = dir.path().join("surprise.mp4");
        std::fs::create_dir_all(&as_dir).expect("create the directory");
        std::fs::write(as_dir.join("inside.bin"), b"still here").expect("write into it");
        assert!(matches!(unlink(&as_dir), Removal::Failed(_)));
        assert!(
            as_dir.join("inside.bin").is_file(),
            "nothing was reached inside it"
        );
    }
}
