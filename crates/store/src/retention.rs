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

use crate::cleanup::{plan_cleanup, CleanupOutcome, CleanupPolicy, DeletionReasons};
use crate::roots::{Gate, Target};
use crate::{Clip, Eviction, LibraryRoots, Session, Store, Tombstone};
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
    /// Must be non-zero (see [`RetentionRules::validate`]).
    pub max_total_bytes: u64,
    /// A member of the class older than this many days is deleted, unless it is a favourite (or, for
    /// a session, still recording). The comparison is strictly greater. Must be non-zero (see
    /// [`RetentionRules::validate`]).
    pub max_age_days: u32,
}

impl RetentionRules {
    /// Refuse a zero limit, for the reason [`CleanupPolicy::validate`] gives: a `0` that deletes
    /// everything is not a retention rule, and "plan nothing" is the only fail-closed reading.
    pub fn validate(&self) -> Result<()> {
        clip_rules(*self).validate()
    }
}

impl RetentionPolicy {
    /// Refuse a policy in which either class has a zero limit.
    pub fn validate(&self) -> Result<()> {
        self.clips
            .validate()
            .map_err(|err| err.context("the clips rules"))?;
        self.sessions
            .validate()
            .map_err(|err| err.context("the sessions rules"))
    }
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
    if let Err(reason) = rules.validate() {
        tracing::warn!(
            "retention: the sessions rules are invalid and no session will be deleted: {reason:#}"
        );
        return ClassPlan {
            deletions: Vec::new(),
            cap_bytes: rules.max_total_bytes,
            bytes_after: sessions.iter().fold(0, |total, s| {
                total.saturating_add(s.size_bytes.max(0) as u64)
            }),
            over_cap_by_bytes: 0,
        };
    }
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
    /// Planned deletions skipped because the member became a favourite after the plan was made. The
    /// row and its files are untouched.
    pub protected: usize,
    /// Size of the planned population after the pass: the plan's forecast plus the bytes of every
    /// planned deletion that did not happen (failed, or protected since planning).
    ///
    /// Not a re-read of every row in the database: the plan is made over the rows whose file exists,
    /// and a re-read that counted the rest again made `cap_met` false, and the caller's "the cap
    /// cannot be satisfied" warning wrong, for a class the pass had brought under its cap.
    pub bytes_after: u64,
    /// Whether the planned population is at or under the plan's cap after the pass, measured
    /// against what the pass actually did rather than forecast.
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

/// Apply a retention plan: for every deletion, evict the row and commit, then remove what that
/// committed eviction handed back.
///
/// The same safety properties as [`crate::cleanup::execute_cleanup`], which this extends to
/// sessions:
///
/// * **Row first, file second, and the second step is retried.** [`Store::evict_clip`] and
///   [`Store::evict_session`] delete the row and queue its paths in `pending_deletes` in one
///   transaction. A path that cannot be removed (open in another program, an I/O error) stays
///   queued and is retried at the start of the next pass, whatever that pass's plan holds, so a
///   file can no longer escape the index and the cap for good.
/// * **Favourites survive a race.** The eviction refuses a favourite under the lock it deletes
///   under; a member starred after the plan was made is counted in [`ClassOutcome::protected`].
/// * **Only what the library owns is removed.** Every path must be inside `roots` (see
///   [`LibraryRoots`]): a clip's file inside the clips directory, a session's file inside the
///   sessions directory, a session's scratch directory a `session-<digits>[-<digits>]` child of it.
///   Anything else is [`ClassOutcome::refused`]: the row is gone, the path is untouched.
/// * **The shared scratch directory is never touched.** A replay-buffer row names the ring's one
///   shared `[buffer].scratch_dir`, which the row does not own; the eviction queues no scratch
///   removal for it, and containment would refuse it anyway.
///
/// The scratch directory, removed as a tree, is still the most destructive operation in this crate,
/// so on top of containment [`remove_scratch_dir`] requires a real directory rather than a symlink.
///
/// A session still recording cannot reach the removal code: the store refuses its eviction, which
/// counts as a failed deletion and is logged. The planner never selects one, so that means a
/// hand-built plan or one held across the moment the session started.
///
/// Errors are per-row: a failed row is counted in [`ClassOutcome::failed`] and logged instead of
/// ending the pass. The returned `Err` is reserved for failures this function cannot attribute to a row.
pub fn execute_retention(
    store: &Store,
    plan: &RetentionPlan,
    roots: &LibraryRoots,
) -> Result<RetentionOutcome> {
    let mut outcome = RetentionOutcome::default();

    // First, what earlier passes could not finish. The clip class's and the session class's
    // leftovers are reported in their own class.
    match store.pending_deletes() {
        Ok(pending) => {
            for tombstone in &pending {
                let class = match tombstone.target {
                    Target::ClipFile => &mut outcome.clips,
                    Target::SessionFile | Target::SessionDir => &mut outcome.sessions,
                };
                let removal = settle(store, roots, tombstone);
                record(class, &tombstone.path, removal, None);
            }
        }
        Err(err) => tracing::warn!("retention: could not read the queued removals: {err:#}"),
    }

    // The planned bytes still in each class at the end: failed and protected deletions.
    let mut clips_unreclaimed: u64 = 0;
    let mut sessions_unreclaimed: u64 = 0;

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
        match store.evict_clip(id) {
            Ok(Eviction::Evicted(tombstone)) => {
                // Committed before this line, together with the queued path.
                outcome.clips.deleted += 1;
                let removal = settle(store, roots, &tombstone);
                record(
                    &mut outcome.clips,
                    &tombstone.path,
                    removal,
                    Some(entry.size_bytes),
                );
            }
            Ok(Eviction::Protected) => {
                tracing::info!(
                    "retention: clip #{id} became a favourite after the plan was made; it was kept"
                );
                outcome.clips.protected += 1;
                clips_unreclaimed = clips_unreclaimed.saturating_add(entry.size_bytes);
            }
            Ok(Eviction::Gone) => {
                tracing::debug!(
                    "retention: clip #{id} had no row left to delete; nothing was removed"
                );
            }
            Err(err) => {
                tracing::warn!("retention: could not delete the row for clip #{id}: {err:#}");
                outcome.clips.failed += 1;
                clips_unreclaimed = clips_unreclaimed.saturating_add(entry.size_bytes);
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
        match store.evict_session(id) {
            Ok(Eviction::Evicted(tombstones)) => {
                // Committed before this line, together with the queued paths.
                outcome.sessions.deleted += 1;
                for tombstone in &tombstones {
                    let removal = settle(store, roots, tombstone);
                    record(
                        &mut outcome.sessions,
                        &tombstone.path,
                        removal,
                        Some(entry.size_bytes),
                    );
                }
            }
            Ok(Eviction::Protected) => {
                tracing::info!(
                    "retention: session #{id} became a favourite after the plan was made; it was kept"
                );
                outcome.sessions.protected += 1;
                sessions_unreclaimed = sessions_unreclaimed.saturating_add(entry.size_bytes);
            }
            Ok(Eviction::Gone) => {
                tracing::debug!(
                    "retention: session #{id} had no row left to delete; nothing was removed"
                );
            }
            Err(err) => {
                // The expected reason is the store refusing a session still recording; any other is a real
                // failure. Either way the session is left whole, the safe direction.
                tracing::warn!("retention: session #{id} was not deleted: {err:#}");
                outcome.sessions.failed += 1;
                sessions_unreclaimed = sessions_unreclaimed.saturating_add(entry.size_bytes);
            }
        }
    }

    outcome.clips.bytes_after = plan.clips.bytes_after.saturating_add(clips_unreclaimed);
    outcome.clips.cap_met = outcome.clips.bytes_after <= plan.clips.cap_bytes;
    outcome.sessions.bytes_after = plan
        .sessions
        .bytes_after
        .saturating_add(sessions_unreclaimed);
    outcome.sessions.cap_met = outcome.sessions.bytes_after <= plan.sessions.cap_bytes;
    Ok(outcome)
}

/// What one attempt to remove a path did. One type for files and trees, so bookkeeping is written once.
///
/// Not `PartialEq`: `Removal::Failed` carries an `io::Error`, which does not compare. Tests match on the variant.
#[derive(Debug)]
pub(crate) enum Removal {
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
/// `planned_bytes` only feeds a debug line noticing a file differed from its row (and is `None` for
/// a removal retried from the queue, which has no row to compare with); the bytes counted into
/// `bytes_reclaimed` are the measured ones.
fn record(outcome: &mut ClassOutcome, path: &Path, removal: Removal, planned_bytes: Option<u64>) {
    match removal {
        Removal::Removed(actual) => {
            outcome.bytes_reclaimed = outcome.bytes_reclaimed.saturating_add(actual);
            if let Some(planned_bytes) = planned_bytes.filter(|planned| *planned != actual) {
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
                "retention: a deleted row named {} and it could not be removed ({err}); it stays \
                 queued and the next pass retries it",
                path.display()
            );
            outcome.orphaned.push(path.to_path_buf());
        }
    }
}

/// [`record`] for the flat clips executor, whose outcome has no per-class lists to share.
pub(crate) fn record_cleanup(
    outcome: &mut CleanupOutcome,
    path: &Path,
    removal: Removal,
    planned_bytes: Option<u64>,
) {
    let mut class = ClassOutcome::default();
    record(&mut class, path, removal, planned_bytes);
    outcome.bytes_reclaimed = outcome
        .bytes_reclaimed
        .saturating_add(class.bytes_reclaimed);
    outcome.orphaned.extend(class.orphaned);
    outcome.already_missing.extend(class.already_missing);
    outcome.refused.extend(class.refused);
}

/// Remove `path` as what `target` says it is, once `roots` vouch for it.
///
/// The one function every removal passes through: the containment decision comes first, and only a
/// path it admits reaches [`unlink`] or [`remove_scratch_dir`], which keep their own, narrower
/// checks as a second line.
fn remove_admitted(roots: &LibraryRoots, target: Target, path: &Path) -> Removal {
    match roots.admit(target, path) {
        Ok(()) => match target {
            Target::ClipFile | Target::SessionFile => unlink(path),
            Target::SessionDir => remove_scratch_dir(path),
        },
        Err(Gate::Missing) => Removal::Missing,
        Err(Gate::Refused(reason)) => Removal::Refused(reason),
        Err(Gate::Failed(err)) => Removal::Failed(err),
    }
}

/// Carry out one queued removal and, unless it is worth retrying, take it off the queue.
///
/// Removed, already missing and refused all end the entry: the first two are done, and a refusal
/// (a path outside the library, relative, a symlinked directory) will be the same next pass, so
/// retrying it for ever would only repeat the warning. Only a *failure* stays queued.
///
/// A path a live row names again is refused and dequeued without being touched: the same file name
/// re-recorded while the old removal kept failing is somebody's footage once more.
pub(crate) fn settle(store: &Store, roots: &LibraryRoots, tombstone: &Tombstone) -> Removal {
    let removal = match store.path_is_indexed(tombstone.target, &tombstone.path) {
        Ok(true) => Removal::Refused("a live row names it again, so it is footage once more"),
        Ok(false) => remove_admitted(roots, tombstone.target, &tombstone.path),
        Err(err) => Removal::Failed(std::io::Error::other(format!(
            "could not check whether the index still names it: {err:#}"
        ))),
    };
    if !matches!(removal, Removal::Failed(_)) {
        if let Err(err) = store.forget_pending(tombstone.id) {
            tracing::warn!(
                "retention: could not take {} off the queue of removals: {err:#}",
                tombstone.path.display()
            );
        }
    }
    removal
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
/// The tree is measured before removal, best-effort: the measurement is for the report and never
/// decides whether the tree goes (see [`tree_bytes`]).
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
    // Measured for the report only: a size that cannot be read must never be the reason a tree
    // whose row is already gone stays on disk, so an unreadable entry counts as zero.
    let bytes = tree_bytes(path);
    match std::fs::remove_dir_all(path) {
        Ok(()) => Removal::Removed(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Removal::Missing,
        Err(err) => Removal::Failed(err),
    }
}

/// The bytes a directory tree holds: every regular file in it, to any depth. Symlinks count as
/// zero and are never followed; a symlink is a path elsewhere, not bytes this application wrote.
///
/// Best-effort by design: this number only feeds the report, so an entry that cannot be read (it
/// vanished mid-walk, permissions) counts as zero instead of failing the whole measurement, which
/// used to leave a multi-gigabyte tree orphaned after its row was already deleted.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries.flatten().fold(0, |total, entry| {
        total.saturating_add(tree_bytes(&entry.path()))
    })
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

    /// Rules that select a 400-day-old member by age alone: a positive cap nothing reaches, a 30-day limit.
    fn generous_aged() -> RetentionRules {
        rules(u64::MAX, 30)
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
    fn a_zero_limit_in_either_class_plans_nothing_for_that_class() {
        let now = 1_000 * DAY;
        let clips = vec![clip(1, now - 400 * DAY, 100, false)];
        let sessions = vec![session(7, now - 400 * DAY, 900, false)];

        for zero in [rules(0, 30), rules(1_000, 0)] {
            assert!(zero.validate().is_err());
            let plan = plan_retention(&clips, &sessions, &policy(zero, generous_aged()), now);
            assert!(
                plan.clips.deletions.is_empty(),
                "{zero:?} clips: fail closed"
            );
            assert_eq!(
                plan.sessions.ids(),
                vec![7],
                "the other class is unaffected"
            );

            let plan = plan_retention(&clips, &sessions, &policy(generous_aged(), zero), now);
            assert!(
                plan.sessions.deletions.is_empty(),
                "{zero:?} sessions: fail closed"
            );
            assert_eq!(plan.clips.ids(), vec![1]);
            assert_eq!(plan.sessions.bytes_after, 900);
        }
        assert!(policy(rules(1, 1), rules(1, 1)).validate().is_ok());
        assert!(policy(rules(0, 1), rules(1, 1)).validate().is_err());
    }

    #[test]
    fn the_two_classes_are_independent_in_both_directions() {
        let now = 1_000 * DAY;
        let clips = vec![clip(1, now - 40 * DAY, 100, false)];
        let sessions = vec![session(7, now - 40 * DAY, 900, false)];

        // A sessions cap smaller than any session must not evict a clip, however small the clips library.
        let plan = plan_retention(&clips, &sessions, &policy(generous(), rules(1, 3_650)), now);
        assert_eq!(plan.sessions.ids(), vec![7]);
        assert!(
            plan.clips.deletions.is_empty(),
            "a session cap is not a clip cap"
        );

        // And a clips cap of zero must not evict a session.
        let plan = plan_retention(&clips, &sessions, &policy(rules(1, 3_650), generous()), now);
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

        let plan = plan_retention(&[], &sessions, &policy(generous(), rules(1, 3_650)), now);

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

    // Roots that nothing in these in-memory tests reaches: the rows name paths that do not exist.
    fn roots() -> LibraryRoots {
        LibraryRoots::new("/no-such-clips-dir", "/no-such-sessions-dir")
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

        let outcome = execute_retention(&store, &plan, &roots()).expect("the pass completes");

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

        let outcome = execute_retention(&store, &plan, &roots()).expect("the pass completes");

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

        let outcome = execute_retention(&store, &plan, &roots()).expect("the pass completes");

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
