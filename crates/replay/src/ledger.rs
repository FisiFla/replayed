//! The scratch ring index: an in-memory list of the completed segments on the scratch volume, with
//! eviction by byte cap. Media itself is plain appended segment files, deliberately not
//! memory-mapped.
//!
//! **Crash recovery does not depend on this type being persisted.** What survives a crash is the
//! segment *files* in the scratch directory: the recorder rebuilds its view of them by scanning that
//! directory (`replayed_recorder::session::segments_on_disk`, with sequence numbers read from the
//! file names by [`crate::scanner::segment_seq`]), and numbers a new encoder's segments from what it
//! finds there. [`SegmentLedger::save_atomic`], [`SegmentLedger::load`] and the TOML round trip
//! below are public API that no production caller uses: nothing writes a `ledger.toml` and nothing
//! reads one, so they are tested but not part of any recovery path, and hardening them (an `fsync`
//! before the rename, say) would change nothing a user can observe. They are kept as the shape a
//! persisted index would take, not as a promise that one exists. Do not rely on them without first
//! wiring them in and adding that `fsync`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One completed segment on the scratch volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub seq: u64,
    pub file: PathBuf,
    /// Offset from capture start, in milliseconds.
    pub start_ms: u64,
    pub duration_ms: u64,
    pub bytes: u64,
}

impl Segment {
    /// Where the segment ends on the timeline.
    ///
    /// Saturating: the numbers come from a configuration and a file, and a segment that
    /// overflowed the timeline would end before it starts.
    pub fn end_ms(&self) -> u64 {
        self.start_ms.saturating_add(self.duration_ms)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentLedger {
    segments: Vec<Segment>,
    total_bytes: u64,
}

impl SegmentLedger {
    pub fn push(&mut self, seg: Segment) {
        self.total_bytes += seg.bytes;
        self.segments.push(seg);
        // Segments are observed out of order in tests and on rescan.
        self.segments.sort_by_key(|s| s.seq);
        self.segments.dedup_by_key(|s| s.seq);
        self.recompute_bytes();
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn seqs(&self) -> Vec<u64> {
        self.segments.iter().map(|s| s.seq).collect()
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Newest segment's end offset, i.e. how much capture timeline is on disk.
    pub fn span_ms(&self) -> u64 {
        self.segments.last().map(Segment::end_ms).unwrap_or(0)
    }

    /// Drop oldest segments until `total_bytes <= cap`. Retains at least one segment: an empty ring
    /// can never satisfy a later trigger, so slightly over budget beats unable to clip.
    pub fn evict_to_cap(&mut self, cap: u64) -> Vec<Segment> {
        let mut evicted = Vec::new();
        while self.total_bytes > cap && self.segments.len() > 1 {
            let oldest = self.segments.remove(0);
            self.total_bytes = self.total_bytes.saturating_sub(oldest.bytes);
            evicted.push(oldest);
        }
        evicted
    }

    fn recompute_bytes(&mut self) {
        self.total_bytes = self.segments.iter().map(|s| s.bytes).sum();
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).context("serialising ledger")
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let mut l: Self = toml::from_str(text).context("parsing ledger")?;
        l.recompute_bytes();
        l.segments.sort_by_key(|s| s.seq);
        Ok(l)
    }

    /// Persist atomically via temp file + rename, so a crash mid-write cannot leave a truncated
    /// ledger.
    ///
    /// Not called outside tests (see the module docs): recovery rescans the scratch directory
    /// instead. It does not `fsync` before the rename, so a power loss can still leave an empty
    /// file under the final name; a caller wiring this in must add that.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        let text = self.to_toml()?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }

    /// Read a ledger written by [`SegmentLedger::save_atomic`]. Not called outside tests.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seq: u64, bytes: u64) -> Segment {
        Segment {
            seq,
            file: format!("seg-{seq:06}.mp4").into(),
            start_ms: seq * 1000,
            duration_ms: 1000,
            bytes,
        }
    }

    #[test]
    fn evicts_oldest_first_until_under_the_cap() {
        let mut l = SegmentLedger::default();
        for i in 0..5 {
            l.push(seg(i, 10));
        }
        assert_eq!(l.total_bytes(), 50);

        let evicted = l.evict_to_cap(25);

        assert_eq!(
            evicted.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(l.total_bytes(), 20);
    }

    #[test]
    fn never_evicts_below_the_retained_minimum_even_when_over_cap() {
        let mut l = SegmentLedger::default();
        for i in 0..4 {
            l.push(seg(i, 100));
        }
        // Cap is unsatisfiable; a partial clip is worse than being over budget.
        let evicted = l.evict_to_cap(1);
        assert_eq!(
            l.len(),
            1,
            "keeps the newest segment so a clip is still possible"
        );
        assert_eq!(evicted.len(), 3);
    }

    #[test]
    fn keeps_segments_ordered_by_sequence() {
        let mut l = SegmentLedger::default();
        l.push(seg(2, 10));
        l.push(seg(0, 10));
        l.push(seg(1, 10));
        assert_eq!(l.seqs(), vec![0, 1, 2]);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut l = SegmentLedger::default();
        l.push(seg(7, 42));
        let text = l.to_toml().unwrap();
        let back = SegmentLedger::from_toml(&text).unwrap();
        assert_eq!(back.seqs(), vec![7]);
        assert_eq!(back.total_bytes(), 42);
    }
}
