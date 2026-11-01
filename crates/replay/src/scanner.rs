//! Deciding which scratch segments are finished being written, and where the next encoder's
//! numbering must continue from.

use std::collections::BTreeSet;

/// Sequence numbers observed on disk that are complete. ffmpeg appends to the newest segment,
/// so a segment is trusted only once a strictly later one exists. Pure: the caller walks the
/// filesystem.
pub fn newly_complete(observed: &[u64], highest_known: Option<u64>) -> Vec<u64> {
    let unique: BTreeSet<u64> = observed.iter().copied().collect();
    let Some(&max) = unique.iter().next_back() else {
        return Vec::new();
    };
    unique
        .into_iter()
        .filter(|seq| *seq < max)
        .filter(|seq| highest_known.is_none_or(|known| *seq > known))
        .collect()
}

/// The sequence number in a `seg-%06d.mp4` filename, if it is one. Anything else in the scratch
/// directory (a ledger, a concat list, a stray file) yields `None`.
pub fn segment_seq(name: &str) -> Option<u64> {
    name.strip_prefix("seg-")?
        .strip_suffix(".mp4")?
        .parse()
        .ok()
}

/// The sequence number a fresh encoder must start writing at, given the seqs in the adopted
/// ledger and the filenames present in the scratch directory.
///
/// ffmpeg's segment muxer numbers from its `-segment_start_number` on every spawn, so without
/// this a second run would write `seg-000000.mp4` over a file the adopted ledger still points
/// at: the ledger would name footage the new run replaced and a clip cut from it would show the
/// wrong picture (measured: a stale segment turned up as black frames). Continuing the
/// numbering makes overwriting impossible.
///
/// Both sources are consulted and the maximum wins:
///
/// * the ledger may hold entries whose files were deleted or evicted, so it is the floor for
///   numbers already used;
/// * the disk may hold a file the ledger never recorded (a crash between ffmpeg writing a
///   segment and the ledger being brought up to date leaves exactly that), so it is the floor for numbers
///   already present.
///
/// A number is never reused, so the encoder only creates new files. Pure: the caller reads the
/// directory and parses names (or uses [`segment_seq`]). An empty ledger and directory give 0.
pub fn next_segment_number<S: AsRef<str>>(ledger_seqs: &[u64], observed_names: &[S]) -> u64 {
    let from_ledger = ledger_seqs.iter().copied().max();
    let from_disk = observed_names
        .iter()
        .filter_map(|n| segment_seq(n.as_ref()))
        .max();
    match from_ledger.max(from_disk) {
        Some(highest) => highest + 1,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_is_complete_only_once_a_later_one_exists() {
        // seq 3 exists but ffmpeg may still be appending to it.
        assert_eq!(newly_complete(&[0, 1, 2, 3], None), vec![0, 1, 2]);
    }

    #[test]
    fn does_not_re_emit_already_known_segments() {
        assert_eq!(newly_complete(&[0, 1, 2, 3], Some(1)), vec![2]);
    }

    #[test]
    fn nothing_is_complete_when_only_one_segment_exists() {
        assert_eq!(newly_complete(&[0], None), Vec::<u64>::new());
    }

    #[test]
    fn tolerates_out_of_order_and_duplicate_observations() {
        assert_eq!(newly_complete(&[2, 0, 2, 1], None), vec![0, 1]);
    }

    #[test]
    fn empty_directory_yields_nothing() {
        assert_eq!(newly_complete(&[], None), Vec::<u64>::new());
    }

    #[test]
    fn a_segment_name_parses_to_its_sequence_number() {
        assert_eq!(segment_seq("seg-000000.mp4"), Some(0));
        assert_eq!(segment_seq("seg-000009.mp4"), Some(9));
        assert_eq!(segment_seq("seg-123456.mp4"), Some(123_456));
    }

    #[test]
    fn things_that_are_not_segment_names_are_not_numbers() {
        // The scratch directory also holds the ledger and, transiently, a concat list.
        assert_eq!(segment_seq("ledger.toml"), None);
        assert_eq!(segment_seq("clip-1.concat.txt"), None);
        assert_eq!(segment_seq("seg-000000.mkv"), None);
        assert_eq!(segment_seq("seg-.mp4"), None);
        assert_eq!(segment_seq("seg-abc.mp4"), None);
    }

    #[test]
    fn a_fresh_scratch_directory_and_an_empty_ledger_start_at_zero() {
        let empty: [&str; 0] = [];
        assert_eq!(next_segment_number(&[], &empty), 0);
    }

    #[test]
    fn numbering_continues_past_the_ledgers_highest_sequence() {
        // A clean previous run: the ledger has segments 0..=5, the disk the same.
        let names = [
            "seg-000000.mp4",
            "seg-000001.mp4",
            "seg-000002.mp4",
            "seg-000003.mp4",
            "seg-000004.mp4",
            "seg-000005.mp4",
        ];
        assert_eq!(next_segment_number(&[0, 1, 2, 3, 4, 5], &names), 6);
    }

    #[test]
    fn numbering_continues_past_a_segment_the_ledger_never_recorded() {
        // The crash case: ffmpeg wrote 6..=9 but the ledger was last saved at 5 (it is saved every
        // 200ms, so a crash loses the tail). Starting at 6 would overwrite seg-000006.mp4, which the
        // adopted ledger is about to index and could splice into a clip, so the disk must be consulted
        // too.
        let names = [
            "seg-000005.mp4",
            "seg-000006.mp4",
            "seg-000007.mp4",
            "seg-000008.mp4",
            "seg-000009.mp4",
        ];
        assert_eq!(next_segment_number(&[0, 1, 2, 3, 4, 5], &names), 10);
    }

    #[test]
    fn a_ledger_entry_with_no_file_still_reserves_its_number() {
        // Nothing on disk, but the ledger claims 7. Reusing 7 would make the ledger's entry name the
        // new file, splicing this run's footage where an old clip was expected; skipping costs seven
        // unused numbers.
        let empty: [&str; 0] = [];
        assert_eq!(next_segment_number(&[3, 7], &empty), 8);
    }

    #[test]
    fn a_non_segment_file_does_not_shift_the_numbering() {
        let names = ["ledger.toml", "seg-000000.mp4", "clip-1.concat.txt"];
        assert_eq!(next_segment_number(&[0], &names), 1);
    }
}
