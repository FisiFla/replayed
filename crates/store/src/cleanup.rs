//! The storage manager's policy as pure, testable logic, plus the executor that applies a plan.
//!
//! [`plan_cleanup`] decides: it reads only the clip rows and clock it is given, so both rules and
//! their interactions are unit-testable with no database or filesystem. [`execute_cleanup`] acts:
//! it deletes rows and unlinks files, and carries the non-negotiable property that the row delete
//! is committed before the file is unlinked, and no file is unlinked unless a committed row
//! delete handed back its path.

use crate::retention::{record_cleanup, settle};
use crate::roots::Target;
use crate::{Clip, Eviction, LibraryRoots, Store};
use anyhow::Result;
use std::path::{Component, Path, PathBuf};

/// Milliseconds in a day; the age rule's unit conversion.
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1_000;

/// The `[storage]` policy knobs as the storage manager sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupPolicy {
    /// Total bytes the clips directory may occupy, favourites included. Must be non-zero (see
    /// [`CleanupPolicy::validate`]).
    pub max_total_bytes: u64,
    /// A non-favourited clip whose `created_at` is more than this many days old is deleted. Must be
    /// non-zero (see [`CleanupPolicy::validate`]); `1` is the shortest retention there is.
    pub max_age_days: u64,
}

impl CleanupPolicy {
    /// Refuse a policy with a zero limit.
    ///
    /// A `0` used to be a literal limit: `max_age_days = 0` made every clip "older than now" and
    /// `max_total_bytes = 0` evicted every non-favourite, at the next startup and without a prompt,
    /// from a config value most people read as "unlimited" or "off". Nothing in the config example
    /// or the guide said otherwise, and the loss is irreversible. So zero is not a limit any more:
    /// [`plan_cleanup`] plans **nothing** for such a policy (failing closed, with a warning), and a
    /// caller that wants to say why, or to refuse to start, asks here. "Delete everything" is not a
    /// retention policy; a user who wants it deletes the clips.
    pub fn validate(&self) -> Result<()> {
        if self.max_age_days == 0 {
            anyhow::bail!(
                "max_age_days = 0 is not a limit: it would delete every clip that is not a \
                 favourite. Use at least 1 day"
            );
        }
        if self.max_total_bytes == 0 {
            anyhow::bail!(
                "max_total_bytes = 0 is not a limit: it would delete every clip that is not a \
                 favourite. Use a positive size"
            );
        }
        Ok(())
    }
}

/// Why one clip is in a plan. Both flags can be set (the age and size rules can each select it),
/// but the clip appears in the plan exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeletionReasons {
    /// Older than [`CleanupPolicy::max_age_days`].
    pub too_old: bool,
    /// Evicted to bring the total size at or under [`CleanupPolicy::max_total_bytes`].
    pub over_cap: bool,
}

/// One clip a plan deletes, with the bytes its row accounts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedDeletion {
    pub id: i64,
    pub size_bytes: u64,
    pub reasons: DeletionReasons,
}

/// What one pass would delete, and what that leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPlan {
    /// Clips to delete, oldest-created first (see [`plan_cleanup`]), each exactly once.
    pub deletions: Vec<PlannedDeletion>,
    /// The cap the plan was made against, carried so the executor checks the post-condition against
    /// the live store rather than this forecast.
    pub cap_bytes: u64,
    /// Total size of the library (favourites included) if every deletion here succeeds.
    pub bytes_after: u64,
    /// The part of the overshoot that no cleanup pass may ever remove: how far the favourited clips
    /// alone exceed the cap.
    ///
    /// Non-zero means the cap is unsatisfiable. Evicting every non-favourited clip would still leave
    /// the favourites, so the size rule selects nothing (the manager never deletes a clip to satisfy
    /// a cap it cannot meet) and the caller warns with this number. `bytes_after` may then exceed the
    /// cap by more than this, since it still holds the non-favourited clips the rule declined to evict.
    pub over_cap_by_bytes: u64,
}

impl CleanupPlan {
    /// Whether applying this plan brings the library at or under the cap.
    ///
    /// Equal to `bytes_after <= cap_bytes`: while the favourites are within the cap the size rule
    /// always reaches it, and once they are not, nothing can.
    pub fn cap_met(&self) -> bool {
        self.over_cap_by_bytes == 0
    }

    /// The ids to delete, in plan order.
    pub fn ids(&self) -> Vec<i64> {
        self.deletions.iter().map(|d| d.id).collect()
    }
}

/// Decide what one pass over `clips` deletes, touching neither the filesystem nor the database.
///
/// 1. Age: delete every non-favourited clip older than `max_age_days`.
/// 2. Size: evict non-favourited clips least-recently-used first until the total is at or under
///    `max_total_bytes`. If the favourites alone exceed the cap the rule is not applied at all.
/// 3. Favourites are exempt from both rules.
///
/// A clip both rules select appears once, flagged with both reasons.
///
/// A policy with a zero limit is refused: the plan is empty (see [`CleanupPolicy::validate`]).
///
/// # When the cap cannot be met
///
/// The reachable floor is the favourites. When that floor is above the cap no deletion can satisfy
/// it, so the manager warns and stops rather than delete clips for a cap it cannot meet. The size
/// rule selects nothing in that state, the age rule still applies (a retention rule independent
/// of the cap), and the unsatisfiable part is reported in [`CleanupPlan::over_cap_by_bytes`].
/// Deleting every non-favourited clip would destroy material the user did not protect and still
/// not satisfy the cap.
///
/// # What "least recently used" means here
///
/// A clip row is immutable and nothing reads a clip back, so there is no access time to order by.
/// The candidates are `created_at` (wall clock, stamped by [`Store::insert_clip`]) and
/// `started_at` (media time, which restarts with the scratch directory and so is not comparable
/// across captures). The size rule therefore orders by `created_at`, oldest first. Ties (a burst
/// indexed in the same millisecond) are broken by id, so a plan is deterministic for a given
/// library and `now_ms`.
pub fn plan_cleanup(clips: &[Clip], policy: &CleanupPolicy, now_ms: i64) -> CleanupPlan {
    if let Err(reason) = policy.validate() {
        tracing::warn!("cleanup: the policy is invalid and nothing will be deleted: {reason:#}");
        return CleanupPlan {
            deletions: Vec::new(),
            cap_bytes: policy.max_total_bytes,
            bytes_after: clips
                .iter()
                .fold(0, |total, c| total.saturating_add(c.size_bytes)),
            over_cap_by_bytes: 0,
        };
    }
    let max_age_ms = (policy.max_age_days as i64).saturating_mul(MS_PER_DAY);

    // Oldest-created first, deterministic under ties. Both rules walk this one order.
    let mut ordered: Vec<&Clip> = clips.iter().collect();
    ordered.sort_by_key(|c| (c.created_at_ms, c.id));

    // One slot per clip, in `ordered` order: the age rule marks a clip here instead of pushing it
    // onto a list, so the size rule cannot put the same clip in the plan twice.
    let mut marked: Vec<Option<DeletionReasons>> = vec![None; ordered.len()];

    // The age rule, and the running total the size rule works from. `saturating_sub` keeps a clip
    // with a future `created_at` (a clock that stepped backwards) from reading as ancient, and the
    // total from underflowing. The sum saturates for the same reason: a library that claims more
    // bytes than a `u64` holds must over-count, not panic in the middle of a cleanup pass.
    let mut remaining: u64 = clips
        .iter()
        .fold(0, |total, c| total.saturating_add(c.size_bytes));
    for (i, clip) in ordered.iter().enumerate() {
        if !clip.favourite && now_ms.saturating_sub(clip.created_at_ms) > max_age_ms {
            marked[i] = Some(DeletionReasons {
                too_old: true,
                over_cap: false,
            });
            remaining = remaining.saturating_sub(clip.size_bytes);
        }
    }

    // Is the cap reachable at all? Only while the favourite bytes are within it.
    let favourite_bytes: u64 = clips
        .iter()
        .filter(|c| c.favourite)
        .fold(0, |total, c| total.saturating_add(c.size_bytes));
    let over_cap_by_bytes = favourite_bytes.saturating_sub(policy.max_total_bytes);

    if over_cap_by_bytes == 0 {
        // The size rule stops once the library is at or under the cap (exactly at the cap is not over)
        // and skips favourites. A clip already selected for age is not deleted twice (its bytes already
        // left `remaining`) but is flagged as one the size rule would also have evicted, only while the
        // library is still over the cap, so the flag means what it says.
        for (i, clip) in ordered.iter().enumerate() {
            if remaining <= policy.max_total_bytes {
                break;
            }
            if clip.favourite {
                continue;
            }
            match &mut marked[i] {
                Some(reasons) => reasons.over_cap = true,
                None => {
                    marked[i] = Some(DeletionReasons {
                        too_old: false,
                        over_cap: true,
                    });
                    remaining = remaining.saturating_sub(clip.size_bytes);
                }
            }
        }
    }

    let deletions = ordered
        .iter()
        .zip(&marked)
        .filter_map(|(clip, reasons)| {
            reasons.map(|reasons| PlannedDeletion {
                id: clip.id,
                size_bytes: clip.size_bytes,
                reasons,
            })
        })
        .collect();

    CleanupPlan {
        deletions,
        cap_bytes: policy.max_total_bytes,
        bytes_after: remaining,
        over_cap_by_bytes,
    }
}

/// What one pass actually did to the store and to the disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupOutcome {
    /// Rows this pass deleted (and committed).
    pub deleted: usize,
    /// Planned ids whose row delete failed; each is logged where it happens. Counted, not
    /// propagated, so one unreadable row does not abandon the pass.
    pub failed: usize,
    /// Bytes of file that a successful unlink removed from the disk.
    pub bytes_reclaimed: u64,
    /// Paths a deleted row named whose `remove_file` failed: on disk with no row naming them, for a later sweep or the user.
    pub orphaned: Vec<PathBuf>,
    /// Paths a deleted row named that were already absent. The end state is right, but a row pointing at nothing is worth reporting.
    pub already_missing: Vec<PathBuf>,
    /// Paths a deleted row named that were deliberately not removed: outside the clips directory,
    /// relative, or re-adopted by a live row (see [`LibraryRoots`]). The row is gone, the file is
    /// exactly where it was, and the caller is told.
    pub refused: Vec<PathBuf>,
    /// Planned deletions skipped because the clip became a favourite after the plan was made. The
    /// clip and its file are untouched.
    pub protected: usize,
    /// Size of the planned population after the pass: the plan's forecast plus the bytes of every
    /// planned deletion that did not happen (failed, or protected since planning).
    ///
    /// Not a re-read of every row in the database: the plan is made over the rows whose file exists
    /// (a row whose file is gone holds no bytes to reclaim), and a re-read that counted those rows
    /// again made `cap_met` false for a library the pass had brought under its cap.
    pub bytes_after: u64,
    /// Whether the planned population is at or under the plan's cap after the pass: measured
    /// against what the pass actually did, not forecast.
    pub cap_met: bool,
}

/// Whether a path read out of the index names one place regardless of the working directory:
/// absolute, with no `..` component. The index is user-writable, so a relative path in a row
/// would otherwise be unlinked relative to wherever the process happens to be.
pub(crate) fn is_absolute_and_clean(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir))
}

/// Apply a plan: for each id, evict the row and commit, then remove the path that committed
/// eviction handed back.
///
/// The ordering is the safety property. Deleting the row first means the index never names a
/// file that is gone, the failure a user cannot recover from since the file is the only copy.
/// The reverse failure (a file whose row is gone) is recoverable, invisible and wastes disk, so
/// it is reported at WARN with its path, **and retried**: the eviction queues the path in
/// `pending_deletes` in the same transaction as the row delete, and a path that could not be
/// removed stays queued until a later pass removes it. Every pass starts by working through that
/// queue, whatever its own plan holds.
///
/// Three guards stand between a plan and a `remove_file`:
///
/// * **Favourites.** The row is evicted by [`Store::evict_clip`], which refuses a favourite under
///   the same write lock it deletes under, so a clip starred after the plan was made survives
///   (counted in [`CleanupOutcome::protected`]).
/// * **Containment.** The path must be inside `roots`' clips directory, resolved through symlinks
///   (see [`LibraryRoots`]). A row naming anything else costs the user the row, never the file, and
///   the path is reported in [`CleanupOutcome::refused`].
/// * **Provenance.** The only path ever removed is one a committed eviction queued, so a plan id
///   whose row already disappeared deletes nothing.
///
/// Errors are per-row: a failed row is counted in [`CleanupOutcome::failed`] and logged instead
/// of ending the pass. The returned `Err` is reserved for failures this function cannot attribute to a row.
pub fn execute_cleanup(
    store: &Store,
    plan: &CleanupPlan,
    roots: &LibraryRoots,
) -> Result<CleanupOutcome> {
    let mut outcome = CleanupOutcome::default();

    match store.pending_deletes() {
        Ok(pending) => {
            for tombstone in pending.iter().filter(|t| t.target == Target::ClipFile) {
                let removal = settle(store, roots, tombstone);
                record_cleanup(&mut outcome, &tombstone.path, removal, None);
            }
        }
        Err(err) => tracing::warn!("cleanup: could not read the queued removals: {err:#}"),
    }

    // The planned bytes that are still in the library at the end: failed and protected deletions.
    let mut unreclaimed: u64 = 0;
    for entry in &plan.deletions {
        match store.evict_clip(entry.id) {
            Ok(Eviction::Evicted(tombstone)) => {
                // Committed before this line, together with the queued path.
                outcome.deleted += 1;
                let removal = settle(store, roots, &tombstone);
                record_cleanup(
                    &mut outcome,
                    &tombstone.path,
                    removal,
                    Some(entry.size_bytes),
                );
            }
            Ok(Eviction::Protected) => {
                // The user favourited it after the plan was made.
                tracing::info!(
                    "cleanup: clip #{} became a favourite after the plan was made; it was kept",
                    entry.id
                );
                outcome.protected += 1;
                unreclaimed = unreclaimed.saturating_add(entry.size_bytes);
            }
            Ok(Eviction::Gone) => {
                // No committed row delete, so nothing names a file: leave it exactly where it is.
                tracing::debug!(
                    "cleanup: clip #{} had no row left to delete; nothing was unlinked",
                    entry.id
                );
            }
            Err(err) => {
                tracing::warn!(
                    "cleanup: could not delete the row for clip #{}: {err:#}",
                    entry.id
                );
                outcome.failed += 1;
                unreclaimed = unreclaimed.saturating_add(entry.size_bytes);
            }
        }
    }

    outcome.bytes_after = plan.bytes_after.saturating_add(unreclaimed);
    outcome.cap_met = outcome.bytes_after <= plan.cap_bytes;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = MS_PER_DAY;

    /// A clip row as `Store::list_clips` would hand it to `plan_cleanup`. `started_at_ms` is derived
    /// from nothing meaningful: the policy must not read it (see the "least recently used" note on [`plan_cleanup`]).
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

    fn policy(max_total_bytes: u64, max_age_days: u64) -> CleanupPolicy {
        CleanupPolicy {
            max_total_bytes,
            max_age_days,
        }
    }

    /// A cap nothing reaches and an age nothing exceeds: the plan must be a no-op.
    fn generous() -> CleanupPolicy {
        policy(u64::MAX, 3_650)
    }

    #[test]
    fn a_library_within_both_rules_plans_nothing() {
        let now = 10 * DAY;
        let clips = vec![
            clip(1, now - 2 * DAY, 1_000, false),
            clip(2, now - DAY, 500, true),
        ];

        let plan = plan_cleanup(&clips, &generous(), now);

        assert!(plan.deletions.is_empty(), "nothing is over age or over cap");
        assert!(plan.cap_met());
        assert_eq!(plan.over_cap_by_bytes, 0);
        assert_eq!(
            plan.bytes_after, 1_500,
            "an empty plan leaves the library as it was"
        );
    }

    /// A zero limit used to be literal: `max_age_days = 0` made every clip "older than now" and
    /// `max_total_bytes = 0` evicted every non-favourite, at the next startup, from a config value
    /// most people read as "off". Now a zero limit plans nothing, and says why on request.
    #[test]
    fn a_zero_limit_fails_closed_and_plans_nothing() {
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - 400 * DAY, 100, false),
            clip(2, now - 300 * DAY, 100, false),
        ];
        for zero in [policy(0, 30), policy(1_000, 0), policy(0, 0)] {
            assert!(zero.validate().is_err(), "{zero:?} is rejected by validate");
            let plan = plan_cleanup(&clips, &zero, now);
            assert!(
                plan.deletions.is_empty(),
                "{zero:?} must not delete anything: {:?}",
                plan.deletions
            );
            assert_eq!(plan.bytes_after, 200, "the library is left as it was");
        }
        assert!(policy(1, 1).validate().is_ok());
    }

    #[test]
    fn an_empty_library_plans_nothing() {
        let plan = plan_cleanup(&[], &policy(1, 1), 5 * DAY);

        assert!(plan.deletions.is_empty());
        assert!(plan.cap_met(), "0 bytes is under any cap");
        assert_eq!(plan.bytes_after, 0);
    }

    #[test]
    fn favourites_are_never_selected_by_either_rule() {
        let now = 100 * DAY;
        // Two favourites: one ancient, the pair alone big enough to blow the cap.
        let clips = vec![
            clip(1, now - 90 * DAY, 4_000, true),
            clip(2, now - 90 * DAY, 6_000, true),
            clip(3, now - DAY, 100, false), // recent, small: nothing wrong with it
        ];

        // 1 day of age, 10 bytes of cap: every rule is violated as hard as it can be.
        let plan = plan_cleanup(&clips, &policy(10, 1), now);

        assert!(
            plan.deletions.is_empty(),
            "no non-favourite is over age, and the size rule is suspended: got {:?}",
            plan.ids()
        );
        assert!(!plan.cap_met());
        assert_eq!(
            plan.over_cap_by_bytes, 9_990,
            "the favourites alone exceed the cap by the whole shortfall (10_000 - 10)"
        );
        assert_eq!(plan.bytes_after, 10_100, "nothing was deleted");
    }

    #[test]
    fn the_age_rule_selects_exactly_the_non_favourited_clips_past_the_limit() {
        let now = 100 * DAY;
        let clips = vec![
            clip(1, now - 8 * DAY, 10, false),     // over the limit -> age
            clip(2, now - 7 * DAY - 1, 20, false), // just over -> age
            clip(3, now - 7 * DAY, 40, false),     // exactly at the limit -> kept
            clip(4, now - 6 * DAY, 80, false),     // under -> kept
            clip(5, now - 30 * DAY, 160, true),    // ancient favourite -> kept
        ];

        let plan = plan_cleanup(&clips, &policy(u64::MAX, 7), now);

        assert_eq!(
            plan.ids(),
            vec![1, 2],
            "only the non-favourites older than 7 days"
        );
        assert!(plan
            .deletions
            .iter()
            .all(|d| d.reasons.too_old && !d.reasons.over_cap));
        assert_eq!(
            plan.bytes_after, 280,
            "310 bytes of library less the 30 deleted"
        );
        assert!(plan.cap_met());
    }

    #[test]
    fn the_age_limit_is_a_strict_threshold() {
        let now = DAY * 3;
        let clips = vec![
            clip(1, now - DAY, 1, false), // age exactly one day, limit one day -> kept
            clip(2, now, 1, false),       // created this very millisecond -> kept
        ];
        assert_eq!(
            plan_cleanup(&clips, &policy(u64::MAX, 1), now).ids(),
            Vec::<i64>::new()
        );

        // One millisecond past the limit, it goes.
        let clips = vec![clip(1, now - DAY - 1, 1, false)];
        let plan = plan_cleanup(&clips, &policy(u64::MAX, 1), now);
        assert_eq!(plan.ids(), vec![1]);
        assert!(plan.deletions[0].reasons.too_old);
    }

    #[test]
    fn a_creation_time_in_the_future_is_never_too_old() {
        // A wall clock that stepped backwards between two runs must not make the newest clip look the oldest.
        let now = 10 * DAY;
        let clips = vec![clip(1, now + 5 * DAY, 1_000, false)];

        let plan = plan_cleanup(&clips, &policy(u64::MAX, 1), now);

        assert!(plan.deletions.is_empty());
        assert_eq!(plan.bytes_after, 1_000);
    }

    #[test]
    fn the_size_rule_evicts_oldest_first_until_under_the_cap() {
        let now = 1_000 * DAY;
        // Created in this order: 1 (oldest) ... 4 (newest). 300 bytes in total.
        let clips = vec![
            clip(1, now - 40 * DAY, 100, false),
            clip(2, now - 30 * DAY, 100, false),
            clip(3, now - 20 * DAY, 50, false),
            clip(4, now - 10 * DAY, 50, false),
        ];

        // 300 -> 200 bytes: evicting the single oldest clip is enough.
        let plan = plan_cleanup(&clips, &policy(200, 3_650), now);
        assert_eq!(plan.ids(), vec![1]);
        assert!(plan.deletions[0].reasons.over_cap && !plan.deletions[0].reasons.too_old);
        assert_eq!(
            plan.bytes_after, 200,
            "200 is at the cap, which is not over it"
        );
        assert!(plan.cap_met());

        // 300 -> 90 bytes: three clips go, oldest first, and it stops at the cap.
        let plan = plan_cleanup(&clips, &policy(90, 3_650), now);
        assert_eq!(plan.ids(), vec![1, 2, 3], "least recently created first");
        assert_eq!(plan.bytes_after, 50);
        assert!(plan.cap_met());

        // A cap smaller than any clip: everything evictable goes.
        let plan = plan_cleanup(&clips, &policy(1, 3_650), now);
        assert_eq!(plan.ids(), vec![1, 2, 3, 4]);
        assert_eq!(plan.bytes_after, 0);
        assert!(
            plan.cap_met(),
            "with no favourites, 0 bytes is under a 1 byte cap"
        );
    }

    #[test]
    fn the_size_rule_skips_favourites_that_are_older_than_the_cap_needs() {
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - 40 * DAY, 60, true), // oldest, and exempt
            clip(2, now - 30 * DAY, 60, false),
            clip(3, now - 20 * DAY, 60, false),
        ];

        // 180 -> 60: with #1 exempt, #2 and #3 are both needed to reach the cap.
        let plan = plan_cleanup(&clips, &policy(60, 3_650), now);

        assert_eq!(
            plan.ids(),
            vec![2, 3],
            "favourites are not eviction candidates"
        );
        assert_eq!(plan.bytes_after, 60);
        assert!(
            plan.cap_met(),
            "the favourites are within the cap, so it is reachable"
        );
    }

    #[test]
    fn an_age_deletion_that_already_meets_the_cap_plans_no_evictions() {
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - 30 * DAY, 100, false), // over age: the age rule alone fixes the cap
            clip(2, now - DAY, 50, false),
        ];

        // 150 bytes over a 100 byte cap, but the aged clip is 100 of them.
        let plan = plan_cleanup(&clips, &policy(100, 7), now);

        assert_eq!(plan.ids(), vec![1]);
        assert!(
            !plan.deletions[0].reasons.over_cap,
            "the size rule had nothing left to do once the aged clip was gone"
        );
        assert_eq!(plan.bytes_after, 50);
        assert!(plan.cap_met());
    }

    #[test]
    fn a_clip_selected_by_both_rules_appears_only_once() {
        let now = 1_000 * DAY;
        let clips = vec![
            // Old enough for the age rule, and still the first candidate for a size rule that has real work
            // to do (the library is over the cap even after the age rule ran).
            clip(1, now - 40 * DAY, 100, false),
            clip(2, now - DAY, 100, false),
            clip(3, now - DAY, 5, true),
        ];

        // After the age rule: 205 - 100 = 105 bytes against a 10 byte cap, favourites well inside it, so
        // the size rule runs and reaches #1 first.
        let plan = plan_cleanup(&clips, &policy(10, 7), now);

        assert_eq!(
            plan.ids(),
            vec![1, 2],
            "one entry per clip, #1 not one per rule"
        );
        assert_eq!(
            plan.deletions[0].reasons,
            DeletionReasons {
                too_old: true,
                over_cap: true
            },
            "#1 was selected by both rules, and the plan records both reasons"
        );
        assert_eq!(
            plan.bytes_after, 5,
            "each clip's bytes are counted once, however many rules selected it"
        );
        assert!(plan.cap_met(), "only the favourite is left, and it fits");
    }

    #[test]
    fn a_cap_the_favourites_alone_exceed_is_reported_and_selects_nothing_extra() {
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - DAY, 400, true),
            clip(2, now - 2 * DAY, 200, true),
            clip(3, now - DAY, 100, false), // recent and small: no rule wants it
        ];

        let plan = plan_cleanup(&clips, &policy(500, 7), now);

        assert!(
            plan.deletions.is_empty(),
            "no deletion can satisfy this cap, so none is planned: got {:?}",
            plan.ids()
        );
        assert!(!plan.cap_met());
        assert_eq!(
            plan.over_cap_by_bytes, 100,
            "600 bytes of favourites against 500"
        );
        assert_eq!(
            plan.bytes_after, 700,
            "the projection still counts the non-favourites"
        );
    }

    #[test]
    fn an_unsatisfiable_cap_does_not_suspend_the_age_rule() {
        let now = 1_000 * DAY;
        let clips = vec![
            clip(1, now - 30 * DAY, 200, true), // ancient favourite: kept regardless
            clip(2, now - 30 * DAY, 400, false), // ancient non-favourite: the age rule's job
            clip(3, now - DAY, 100, false),
        ];

        let plan = plan_cleanup(&clips, &policy(10, 7), now);

        assert_eq!(
            plan.ids(),
            vec![2],
            "the age rule is independent of the cap"
        );
        assert!(plan.deletions[0].reasons.too_old && !plan.deletions[0].reasons.over_cap);
        assert!(!plan.cap_met());
        assert_eq!(
            plan.over_cap_by_bytes, 190,
            "200 bytes of favourites against a 10 byte cap"
        );
        assert_eq!(
            plan.bytes_after, 300,
            "what is left after the aged clip is gone"
        );
    }

    #[test]
    fn the_plan_is_ordered_oldest_first_and_deterministic_under_ties() {
        let now = 1_000 * DAY;
        // All four are over age. #1 and #2 share a creation instant (a burst indexed in the same
        // millisecond); the id breaks the tie, so the plan never depends on `list_clips`'s row order.
        let clips = vec![
            clip(4, now - 10 * DAY, 1, false),
            clip(2, now - 30 * DAY, 1, false),
            clip(3, now - 20 * DAY, 1, false),
            clip(1, now - 30 * DAY, 1, false),
        ];

        let first = plan_cleanup(&clips, &policy(u64::MAX, 7), now);
        assert_eq!(first.ids(), vec![1, 2, 3, 4]);

        let mut shuffled = clips.clone();
        shuffled.reverse();
        assert_eq!(
            plan_cleanup(&shuffled, &policy(u64::MAX, 7), now).ids(),
            first.ids()
        );
    }
}
