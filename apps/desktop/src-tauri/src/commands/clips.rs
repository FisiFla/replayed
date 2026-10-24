//! Clip commands: list, metadata, trim, thumbnail, delete, and the path helpers they share.

use super::*;
use replayed_media::edit::{thumbnail as ffmpeg_thumbnail, trim_lossless};
use replayed_media::export::ExportPreset;
use replayed_media::probe::MediaInfo;
use replayed_store::cleanup::{plan_cleanup, CleanupPolicy};
use replayed_store::{Clip, NewClip, Store};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------

/// The clips whose files are actually on disk, and the indexed rows that are not.
struct LiveClips {
    /// Newest first, ties broken by id (see `clips_on_disk`).
    clips: Vec<Clip>,
    /// `(id, path)` for each row left out, in the same order — enough for a caller to say
    /// which ones went and where they used to be.
    missing: Vec<(i64, PathBuf)>,
}

/// **The index is a cache and the filesystem is the truth**; this is the single place that says
/// so.
///
/// Every surface that speaks about clips (the listing and the storage panel) goes through here.
/// When the two read the index differently they contradict each other on screen: one said "No
/// clips are indexed yet" while the other said "Indexed clips 12", "In use 1.2 GiB", for twelve
/// rows whose files had been deleted.
///
/// Handing a dead row to the window also costs more than a wrong list: the UI asks the asset
/// protocol for each one (Tauri logs `File does not exist at path`), then a thumbnail of each,
/// and every thumbnail is an ffmpeg process. Thirteen dead clips on a real install meant a burst
/// of ffmpeg children and a window too busy to be dragged.
///
/// Skipped rather than deleted: a file can be temporarily unavailable (a network directory, an
/// antivirus quarantine), and silently rewriting a user's library is not a read's business. The
/// rows stay, and `missing` says how many there are so a caller can tell the user.
///
/// Ordered by `created_at_ms` (the wall clock the store stamps), not by `clips.started_at`,
/// which is what `Store::list_clips` orders by. `started_at` is media time on the ring's own
/// timeline and restarts with the scratch directory, so it is not comparable across runs.
/// `created_at` is also what the storage policy evicts by, so the list the user sees and the
/// order the manager deletes in agree. Ties are broken by id, so the order is total and stable.
fn clips_on_disk(deps: &Deps<'_>) -> Result<LiveClips, CommandError> {
    let mut clips = deps
        .store
        .list_clips()
        .map_err(|e| CommandError::store("listing clips", e))?;
    clips.sort_by_key(|clip| std::cmp::Reverse((clip.created_at_ms, clip.id)));

    let mut missing = Vec::new();
    clips.retain(|clip| {
        let present = clip.path.exists();
        if !present {
            missing.push((clip.id, clip.path.clone()));
        }
        present
    });
    Ok(LiveClips { clips, missing })
}

/// Every clip on disk, newest first.
pub fn list_clips(deps: &Deps<'_>) -> Result<Vec<ClipDto>, CommandError> {
    let LiveClips { clips, missing } = clips_on_disk(deps)?;

    // Reported here rather than in `clips_on_disk`, so a refresh that is not a listing cannot double
    // the log. `debug!` because it repeats on every refresh.
    for (id, path) in &missing {
        tracing::debug!(
            "skipping clip #{id} from the listing: {} is gone",
            path.display()
        );
    }
    if !missing.is_empty() {
        tracing::warn!(
            "{} of {} indexed clips are missing from disk and were left out of the listing",
            missing.len(),
            missing.len() + clips.len()
        );
    }

    Ok(clips.iter().map(ClipDto::from).collect())
}

/// The storage panel: usage, the configured cap and age limit, and the policy's verdict.
pub fn storage_stats(deps: &Deps<'_>) -> Result<StorageStats, CommandError> {
    storage_stats_at(deps, now_ms())
}

/// [`storage_stats`] against an explicit clock, so a test can see the age rule fire without
/// backdating rows through SQL (the store has no API for that) or sleeping for a day.
///
/// Every figure is about the clips that are on disk (`clips_on_disk`), including the bytes and
/// the eviction plan. `Store::total_bytes` sums what the rows recorded, so a library whose files
/// had been deleted reported gigabytes in use with nothing on disk, and a row with no file has
/// no bytes to reclaim: counting one in the cap arithmetic asks for an eviction that frees
/// nothing.
///
/// The recorder's own retention pass (`crates/store/src/cleanup.rs`,
/// `crates/recorder/src/index.rs`) still plans on rows, so a cap inflated by dead rows can make
/// it evict a live clip.
pub fn storage_stats_at(deps: &Deps<'_>, now_ms: i64) -> Result<StorageStats, CommandError> {
    let LiveClips { clips, missing } = clips_on_disk(deps)?;
    let total_bytes: u64 = clips.iter().map(|clip| clip.size_bytes).sum();

    let policy = CleanupPolicy {
        max_total_bytes: deps.storage.max_total_bytes,
        max_age_days: deps.storage.max_age_days,
    };
    let plan = plan_cleanup(&clips, &policy, now_ms);

    let favourites: Vec<&Clip> = clips.iter().filter(|c| c.favourite).collect();

    Ok(StorageStats {
        clip_count: clips.len(),
        total_bytes,
        favourite_count: favourites.len(),
        favourite_bytes: favourites.iter().map(|c| c.size_bytes).sum(),
        missing_count: missing.len(),
        cap_bytes: policy.max_total_bytes,
        max_age_days: policy.max_age_days,
        cap_met: plan.cap_met(),
        over_cap_by_bytes: plan.over_cap_by_bytes,
        planned_deletions: plan.deletions.len(),
        bytes_after: plan.bytes_after,
        // The other library's phantoms, counted so the panel's reconciliation offer can name both:
        // a session row has no entry in any figure above.
        missing_sessions: super::prune::missing_sessions(deps)?,
        clips_dir: deps.paths.clips_dir.to_string_lossy().into_owned(),
        warnings: deps.warnings.to_vec(),
    })
}

/// Mark a clip as a favourite, or clear it. Favourites are exempt from both storage rules, so
/// this is the user's only way to protect a clip from the manager.
pub fn set_favourite(deps: &Deps<'_>, id: i64, favourite: bool) -> Result<ClipDto, CommandError> {
    let updated = deps
        .store
        .set_favourite(id, favourite)
        .map_err(|e| CommandError::store("setting the favourite flag", e))?;
    // The store reports whether a row was updated; `false` means the id does not exist, and the UI
    // must be told rather than shown a clip that is not there.
    if !updated {
        return Err(CommandError::clip_not_found(id));
    }
    let clip = find_clip(deps.store, id)?;
    Ok(ClipDto::from(&clip))
}

/// Rename a clip: set its display title (blank clears it back to the file name). The file on
/// disk is deliberately not renamed — see `replayed_store::V5_CLIP_COLUMNS`.
pub fn set_clip_title(deps: &Deps<'_>, id: i64, title: &str) -> Result<ClipDto, CommandError> {
    let updated = deps.store.set_clip_title(id, title).map_err(|e| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("could not set the title: {e:#}"),
        )
    })?;
    if !updated {
        return Err(CommandError::clip_not_found(id));
    }
    Ok(ClipDto::from(&find_clip(deps.store, id)?))
}

/// Replace a clip's tags. Invalid tags (blank, too long, too many) are refused and nothing
/// changes.
pub fn set_clip_tags(deps: &Deps<'_>, id: i64, tags: &[String]) -> Result<ClipDto, CommandError> {
    let updated = deps.store.set_clip_tags(id, tags).map_err(|e| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("could not set the tags: {e:#}"),
        )
    })?;
    if !updated {
        return Err(CommandError::clip_not_found(id));
    }
    Ok(ClipDto::from(&find_clip(deps.store, id)?))
}

/// The on-disk file of clip `id`, for the actions that hand it to the OS (clipboard, reveal,
/// drag-out). The path comes from the index, a user-writable SQLite file, so it must exist and
/// sit inside the library, the guard `delete_clip` applies before unlinking.
pub fn clip_file(deps: &Deps<'_>, id: i64) -> Result<PathBuf, CommandError> {
    let clip = find_clip(deps.store, id)?;
    if !clip.path.is_file() {
        return Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "clip #{id} is indexed at {} but that file is not on disk",
                clip.path.display()
            ),
        ));
    }
    if !is_inside_library(&clip.path, deps.paths) {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "clip #{id} points outside the clips and application data directories ({}); \
                 refusing to hand it to the system",
                clip.path.display()
            ),
        ));
    }
    Ok(clip.path)
}

/// Give a freshly cut clip its parent's game and tags (a trim of a Valorant ace is still a
/// Valorant ace). The title is not inherited: it names the parent. Best effort; the new file and
/// row exist either way, and the returned DTO is the row as it ends up.
pub(super) fn inherit_metadata(
    deps: &Deps<'_>,
    new: ClipDto,
    game: Option<&str>,
    tags: &[String],
) -> ClipDto {
    if game.is_some() {
        if let Err(err) = deps.store.set_clip_game(new.id, game) {
            tracing::warn!("clip #{}: could not inherit the game: {err:#}", new.id);
        }
    }
    if !tags.is_empty() {
        if let Err(err) = deps.store.set_clip_tags(new.id, tags) {
            tracing::warn!("clip #{}: could not inherit the tags: {err:#}", new.id);
        }
    }
    find_clip(deps.store, new.id)
        .map(|c| ClipDto::from(&c))
        .unwrap_or(new)
}

/// Cut `[start_ms, end_ms)` out of clip `id` into a new file next to the original, and index the
/// result.
///
/// The cut is [`replayed_media::edit::trim_lossless`], a stream copy; nothing here may
/// re-encode an export. What a stream copy can cut is bounded by the stream itself (cuts snap to
/// keyframes) rather than by the requested millisecond, so the returned clip reports the probed
/// duration of the file actually written, not the requested span. The UI shows the two side by
/// side.
pub fn trim_clip(
    deps: &Deps<'_>,
    id: i64,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    let clip = find_clip(deps.store, id)?;
    validate_trim_range(&clip, start_ms, end_ms)?;

    let bins = deps.binaries()?;
    if !clip.path.is_file() {
        return Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "clip #{id} is indexed at {} but that file is not on disk, so there is \
                 nothing to trim",
                clip.path.display()
            ),
        ));
    }

    let dst = free_output_path(trim_output_path(&clip.path, start_ms, end_ms))?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CommandError::io("creating the clips directory", dir, e))?;
    }

    trim_lossless(bins, &clip.path, &dst, start_ms, end_ms)
        .map_err(|e| CommandError::media("trimming the clip", e))?;

    // The file exists from here on. Everything below can fail, and if it does the file stays: it is
    // what the user asked for, and deleting it to report a bookkeeping error would destroy the only
    // copy of their edit.
    let new = index_new_clip(
        deps,
        &format!("clip #{}", clip.id),
        clip.started_at_ms,
        &clip.codec,
        &dst,
        start_ms,
        end_ms,
    )?;
    Ok(inherit_metadata(
        deps,
        new,
        clip.game.as_deref(),
        &clip.tags,
    ))
}

/// Describe a newly written file and add it to the index.
///
/// The parent is passed as what it is (a label for the log, the media position the new clip
/// should be placed at, and a codec to name when the new file has no video stream) rather than
/// as a `&Clip`. A session's extracted clip has no parent `Clip`, and inventing one would put a
/// fake row's id in a log line and a fake `started_at` on a real clip.
pub(super) fn index_new_clip(
    deps: &Deps<'_>,
    parent: &str,
    parent_started_at_ms: u64,
    parent_codec: &str,
    dst: &Path,
    start_ms: u64,
    end_ms: u64,
) -> Result<ClipDto, CommandError> {
    let bins = deps.binaries()?;
    let info = MediaInfo::probe(bins, dst).map_err(|e| {
        CommandError::media(
            &format!(
                "reading back the trimmed clip {} (the file was written and is on disk, \
                 but it could not be indexed)",
                dst.display()
            ),
            e,
        )
    })?;

    // Sized from the file rather than ffprobe's `format.size`, because the storage policy caps bytes
    // on disk.
    let size_bytes = std::fs::metadata(dst)
        .map_err(|e| CommandError::io("sizing the trimmed clip", dst, e))?
        .len();

    let codec = info
        .video
        .as_ref()
        .map(|v| v.codec.clone())
        // A file with no video stream is not a clip this app can show; falling back to the parent's
        // codec would name one the file may not contain, so say what is known.
        .unwrap_or_else(|| parent_codec.to_string());

    // `started_at` is the trimmed clip's position on its parent's media timeline. The file's own
    // timeline starts at zero and nothing compares `started_at` across clips or runs, so this is
    // the only reading that carries information. An estimate: a stream copy cannot cut at an
    // arbitrary point, so the first frame may sit slightly before the requested `start_ms`.
    let new = NewClip {
        path: dst.to_path_buf(),
        started_at_ms: parent_started_at_ms.saturating_add(start_ms),
        duration_ms: info.duration_ms,
        size_bytes,
        codec,
    };

    let new_id = deps.store.insert_clip(&new).map_err(|e| {
        CommandError::new(
            ErrorCode::Store,
            // The file is on disk with no row naming it, so the storage policy will never collect it.
            // Saying so lets a user act instead of an invisible file filling their disk.
            format!(
                "the trimmed clip was written to {} but could not be added to the index, \
                 so it will not appear in the library and the storage policy cannot manage \
                 it: {e:#}",
                dst.display()
            ),
        )
    })?;

    tracing::info!(
        "{parent} [{start_ms}ms, {end_ms}ms) was written as clip #{new_id}: {size_bytes} \
         bytes, probed duration {}ms, {}",
        info.duration_ms,
        dst.display()
    );

    // Re-read the row so the DTO carries the store's own `created_at` stamp.
    Ok(ClipDto::from(&find_clip(deps.store, new_id)?))
}

/// A JPEG thumbnail of clip `id` at `at_ms`, made once and reused afterwards.
///
/// The file lands in the thumbnails cache under a name derived from the clip and timestamp, so a
/// repeat request is a `stat` rather than another ffmpeg process.
///
/// A timestamp at or past `duration_ms` is rejected here rather than handed to ffmpeg. One
/// inside the clip can still yield no frame (the last frame of a 2000ms stream does not start at
/// 1999ms) and surfaces as an [`ErrorCode::Media`] error naming the clip, timestamp and
/// duration, since "ffmpeg had nothing to read" is otherwise indistinguishable from a corrupt
/// file. The UI asks for a frame from the first second for that reason (see `thumbnailAtMs` in
/// `src/lib/clips.ts`).
pub fn thumbnail(deps: &Deps<'_>, id: i64, at_ms: u64) -> Result<ThumbnailRef, CommandError> {
    let clip = find_clip(deps.store, id)?;
    if clip.duration_ms == 0 {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "clip #{id} is indexed with a duration of 0ms, so there is no frame to \
                 read a thumbnail from"
            ),
        ));
    }
    if at_ms >= clip.duration_ms {
        return Err(CommandError::new(
            ErrorCode::OutOfRange,
            format!(
                "a thumbnail was requested at {at_ms}ms but clip #{id} is only {}ms long",
                clip.duration_ms
            ),
        ));
    }

    // **Before anything is spawned.** A dead clip reaching ffmpeg fails, the UI retries, and that is
    // a process per attempt. The row is still here so the message can say which file is missing,
    // but nothing is executed for it.
    if !clip.path.exists() {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "clip #{id} is indexed at {} and that file is gone, so there is nothing to read a \
                 thumbnail from",
                clip.path.display()
            ),
        ));
    }

    let bins = deps.binaries()?;
    let dst = deps
        .paths
        .thumbnails_dir
        .join(thumbnail_file_name(id, at_ms));
    if dst.is_file() {
        return Ok(ThumbnailRef {
            clip_id: id,
            path: dst.to_string_lossy().into_owned(),
            at_ms,
            cached: true,
        });
    }
    std::fs::create_dir_all(&deps.paths.thumbnails_dir).map_err(|e| {
        CommandError::io(
            "creating the thumbnails directory",
            &deps.paths.thumbnails_dir,
            e,
        )
    })?;

    ffmpeg_thumbnail(bins, &clip.path, at_ms, &dst).map_err(|e| {
        CommandError::media(
            &format!(
                "reading a frame from clip #{id} at {at_ms}ms, which is inside the clip's \
                 {}ms but may be past the start of its last frame",
                clip.duration_ms
            ),
            e,
        )
    })?;

    Ok(ThumbnailRef {
        clip_id: id,
        path: dst.to_string_lossy().into_owned(),
        at_ms,
        cached: false,
    })
}

/// Delete a clip: the row first, then the file.
///
/// The order is the safety property: deleting the row first means the index never names a file
/// that is gone, the failure a user cannot recover from because the file is the only copy. The
/// reverse failure, a file whose row is gone, is recoverable and is reported in
/// [`DeleteOutcome::orphaned_path`] rather than swallowed.
///
/// An id with no row is not an error: it is reported as `row_deleted: false` and nothing is
/// unlinked, because no committed row delete named the file. A UI acting on a list fetched a
/// moment ago can race a cleanup pass, and "already gone" lets it refresh truthfully.
pub fn delete_clip(deps: &Deps<'_>, id: i64) -> Result<DeleteOutcome, CommandError> {
    // Committed before the next line runs (the store's own contract), which is why the path handed
    // back here is one a committed row delete removed.
    let path = deps
        .store
        .delete_clip_returning_path(id)
        .map_err(|e| CommandError::store("deleting the clip's row", e))?;

    let Some(path) = path else {
        return Ok(DeleteOutcome {
            id,
            row_deleted: false,
            file_removed: false,
            orphaned_path: None,
            already_missing: false,
            bytes_reclaimed: 0,
            thumbnails_removed: 0,
        });
    };

    let path = PathBuf::from(path);
    let mut outcome = DeleteOutcome {
        id,
        row_deleted: true,
        file_removed: false,
        orphaned_path: None,
        already_missing: false,
        bytes_reclaimed: 0,
        // Best effort, reported rather than fatal: a stale JPEG in the cache is a wasted kilobyte, not a
        // reason to fail a delete whose row is already committed.
        thumbnails_removed: remove_cached_thumbnails(&deps.paths.thumbnails_dir, id),
    };

    // The row is the only authority for the path, and the index is a user-writable SQLite file: a
    // row pointing outside the library must cost the user a row, never a file.
    if path.exists() && !is_inside_library(&path, deps.paths) {
        tracing::warn!(
            "clip #{id}: refusing to unlink {} because it is outside the clips and \
             application data directories; the index row is gone, the file is untouched",
            path.display()
        );
        outcome.orphaned_path = Some(path.to_string_lossy().into_owned());
        return Ok(outcome);
    }

    // Sized before the unlink so the reported figure is what was actually freed.
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            outcome.file_removed = true;
            outcome.bytes_reclaimed = size_bytes;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "clip #{id} was deleted from the index but its file was already gone: {}",
                path.display()
            );
            outcome.already_missing = true;
        }
        Err(err) => {
            tracing::warn!(
                "clip #{id} was deleted from the index but {} could not be unlinked \
                 ({err}); it is orphaned on disk",
                path.display()
            );
            outcome.orphaned_path = Some(path.to_string_lossy().into_owned());
        }
    }

    Ok(outcome)
}

/// Combine whole clips into one file, losslessly: the concatenation of their own packets.
///
/// The refusal rules are the point, not the feature:
///
/// * **At least two, and each exactly once.** One clip is not a combine, and a repeated id would
///   duplicate footage the user did not ask to duplicate.
/// * **Every file must be on disk and inside the library.** The paths come from the index, a
///   user-writable file, so the same guard `delete_clip` applies is applied here before anything
///   is read: a row pointing outside the library must not get its file concatenated.
/// * **One stream layout for all of them.** A stream copy cannot change size or codec; ffmpeg
///   would concatenate disagreeing inputs and exit 0 with a truncated or unplayable result
///   (`replayed_media::edit::check_concat_layout` has the measurement). Mixed clips are a refusal
///   naming the offending one.
///
/// The output is a new clip in the library, indexed like any other: it is real footage with a
/// real file, so the storage policy must see it. Its game and tags are inherited only where every
/// input agrees (a combine of a Valorant ace and a CS2 round has no single game).
pub fn combine_clips(deps: &Deps<'_>, ids: &[i64]) -> Result<ClipDto, CommandError> {
    let mut ordered: Vec<i64> = Vec::with_capacity(ids.len());
    for &id in ids {
        if !ordered.contains(&id) {
            ordered.push(id);
        }
    }
    if ordered.len() < 2 {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "combining needs at least two different clips; {} was selected",
                ordered.len()
            ),
        ));
    }

    let mut clips = Vec::with_capacity(ordered.len());
    for id in &ordered {
        let clip = find_clip(deps.store, *id)?;
        if !clip.path.is_file() {
            return Err(CommandError::new(
                ErrorCode::Io,
                format!(
                    "clip #{id} is indexed at {} but that file is not on disk, so it cannot be \
                     combined",
                    clip.path.display()
                ),
            ));
        }
        if !is_inside_library(&clip.path, deps.paths) {
            return Err(CommandError::new(
                ErrorCode::InvalidInput,
                format!(
                    "clip #{id} points outside the clips and application data directories ({}); \
                     refusing to read it",
                    clip.path.display()
                ),
            ));
        }
        clips.push(clip);
    }

    let paths: Vec<std::path::PathBuf> = clips.iter().map(|clip| clip.path.clone()).collect();
    let dst = free_output_path(
        deps.paths
            .clips_dir
            .join(format!("combine-{}.mp4", now_ms())),
    )?;
    let bins = deps.binaries()?;
    replayed_media::edit::combine_lossless(bins, &paths, &dst).map_err(|e| {
        CommandError::media(
            &format!(
                "combining clips {}",
                ordered
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            e,
        )
    })?;

    // The file exists from here on; a bookkeeping failure must not delete it (it is what the user
    // asked for, and it is the only copy of the combined footage).
    let label = format!(
        "clips {}",
        ordered
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(" + ")
    );
    let total_ms: u64 = clips.iter().map(|clip| clip.duration_ms).sum();
    let new = index_new_clip(deps, &label, 0, &clips[0].codec, &dst, 0, total_ms)?;

    // Inherited only where every input agrees; `inherit_metadata` writes what it is given.
    let game = clips
        .iter()
        .map(|clip| clip.game.clone())
        .reduce(|a, b| if a == b { a } else { None })
        .flatten();
    let mut tags: Vec<String> = clips
        .iter()
        .flat_map(|clip| clip.tags.iter().cloned())
        .collect();
    tags.sort();
    tags.dedup();
    Ok(inherit_metadata(deps, new, game.as_deref(), &tags))
}

/// Re-encode clip `id` with `preset` into a new file beside it, and report where it went.
///
/// The one operation in this module that decodes and encodes on purpose — an export changes the
/// pixels (a smaller frame, a vertical frame, a GIF), which no stream copy can do. The original is
/// never modified and the export is **not** indexed: it is an artefact meant to be uploaded, and
/// indexing it would put a derived copy of the footage under the storage policy's cap.
pub fn export_clip(deps: &Deps<'_>, id: i64, preset: &str) -> Result<ExportOutcome, CommandError> {
    let preset = ExportPreset::parse(preset).ok_or_else(|| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "{preset:?} is not an export preset; this build offers {}",
                ExportPreset::ALL
                    .iter()
                    .map(|preset| preset.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })?;
    let clip = find_clip(deps.store, id)?;
    if !clip.path.is_file() {
        return Err(CommandError::new(
            ErrorCode::Io,
            format!(
                "clip #{id} is indexed at {} but that file is not on disk, so there is nothing \
                 to export",
                clip.path.display()
            ),
        ));
    }
    if !is_inside_library(&clip.path, deps.paths) {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "clip #{id} points outside the clips and application data directories ({}); \
                 refusing to read it",
                clip.path.display()
            ),
        ));
    }

    let stem = clip
        .path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("clip-{id}"));
    let candidate =
        deps.paths
            .clips_dir
            .join(format!("{stem}-{}.{}", preset.as_str(), preset.extension()));
    let dst = free_output_path(candidate)?;
    let bins = deps.binaries()?;
    replayed_media::export::export(bins, &clip.path, &dst, preset, clip.duration_ms).map_err(
        |e| CommandError::media(&format!("exporting clip #{id} as {}", preset.as_str()), e),
    )?;

    // Sized and probed from the file that was written: a re-encode may land a few frames either
    // side of the source's length, and reporting the source's would be a guess.
    let size_bytes = std::fs::metadata(&dst)
        .map_err(|e| CommandError::io("sizing the export", &dst, e))?
        .len();
    let duration_ms = MediaInfo::probe(bins, &dst)
        .map(|info| info.duration_ms)
        .unwrap_or(clip.duration_ms);
    tracing::info!(
        "clip #{id} exported as {} to {}: {size_bytes} bytes, {duration_ms}ms",
        preset.as_str(),
        dst.display()
    );
    Ok(ExportOutcome {
        preset: preset.as_str().to_string(),
        path: dst.to_string_lossy().into_owned(),
        size_bytes,
        duration_ms,
    })
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

/// Whether `path`'s real parent directory is the clips directory or lives under the application
/// data directory. Parents are canonicalised (resolving `..`, symlinks and junctions) but the
/// file itself is not, so a symlink at the path is unlinked as a link, never followed.
pub(super) fn is_inside_library(path: &Path, paths: &AppPaths) -> bool {
    crate::paths::ensure_within(path, &library_roots(paths)).is_ok()
}

/// The directories the shell owns: the clips directory and the application data directory.
pub(super) fn library_roots(paths: &AppPaths) -> Vec<&Path> {
    [Some(paths.clips_dir.as_path()), paths.db_path.parent()]
        .into_iter()
        .flatten()
        .collect()
}

/// Whether a session's recorded file or scratch directory read from the index may be removed or
/// cut from: inside the library, or shaped like what the recorder names its session artifacts (a
/// configured `sessions_dir` can live anywhere), and never something that contains the clips,
/// thumbnails or application data directories.
pub(super) fn session_artifact_ok(
    path: &Path,
    file: bool,
    paths: &AppPaths,
) -> Result<(), crate::paths::PathRefusal> {
    let protected = [
        Some(paths.clips_dir.as_path()),
        Some(paths.thumbnails_dir.as_path()),
        paths.db_path.parent(),
    ];
    let protected: Vec<&Path> = protected.into_iter().flatten().collect();
    crate::paths::ensure_session_artifact(path, file, &library_roots(paths), &protected).map(|_| ())
}

pub(super) fn find_clip(store: &Store, id: i64) -> Result<Clip, CommandError> {
    store
        .list_clips()
        .map_err(|e| CommandError::store("reading the clip index", e))?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| CommandError::clip_not_found(id))
}

/// Reject a trim range before ffmpeg is spawned.
///
/// The first branch is one the UI never hits, because it clamps first; `trim_lossless` would
/// bail too, but with a message naming no clip. The second makes a range past the clip's end an
/// actionable error rather than an ffmpeg decode failure.
fn validate_trim_range(clip: &Clip, start_ms: u64, end_ms: u64) -> Result<(), CommandError> {
    validate_range("clip", clip.id, clip.duration_ms, start_ms, end_ms)
}

/// The same rules for anything a range can be cut out of, a clip or a session recording.
/// `subject` names which, so the message says "clip #7" or "session #3".
pub(super) fn validate_range(
    subject: &str,
    id: i64,
    duration_ms: u64,
    start_ms: u64,
    end_ms: u64,
) -> Result<(), CommandError> {
    if end_ms <= start_ms {
        return Err(CommandError::new(
            ErrorCode::InvalidRange,
            format!(
                "the range for {subject} #{id} is empty or inverted: start={start_ms}ms, \
                 end={end_ms}ms. A range with no length cannot be cut."
            ),
        ));
    }
    if duration_ms == 0 {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!("{subject} #{id} has a duration of 0ms, so it cannot be cut from"),
        ));
    }
    if end_ms > duration_ms {
        return Err(CommandError::new(
            ErrorCode::OutOfRange,
            format!(
                "the range for {subject} #{id} ends at {end_ms}ms but it is only {duration_ms}ms \
                 long"
            ),
        ));
    }
    Ok(())
}

/// Where a trim of `[start_ms, end_ms)` of `src` is written: beside the original, with the range
/// in the name so the file explains itself in a listing.
///
/// The extension is carried over rather than assumed: `trim_lossless` muxes with
/// `-movflags +faststart`, which only an ISO-BMFF muxer accepts.
pub fn trim_output_path(src: &Path, start_ms: u64, end_ms: u64) -> PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_string());
    src.with_file_name(format!("{stem}.trim-{start_ms}-{end_ms}.{ext}"))
}

/// The first name derived from `candidate` that no file on disk already holds.
///
/// Bounded, and reaching the bound is an error rather than an overwrite: clobbering a previous
/// trim would destroy a file the index may name.
pub(super) fn free_output_path(candidate: PathBuf) -> Result<PathBuf, CommandError> {
    if !candidate.exists() {
        return Ok(candidate);
    }
    let stem = candidate
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());
    let ext = candidate
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_string());

    for n in 2..1000u32 {
        let next = candidate.with_file_name(format!("{stem}-{n}.{ext}"));
        if !next.exists() {
            return Ok(next);
        }
    }
    Err(CommandError::new(
        ErrorCode::Io,
        format!(
            "no free name is available for the trimmed clip: {} and 997 numbered copies \
             of it already exist. Move or delete some of them and try again.",
            candidate.display()
        ),
    ))
}

/// The cache file name for one clip's thumbnail at one timestamp.
pub fn thumbnail_file_name(clip_id: i64, at_ms: u64) -> String {
    format!("clip-{clip_id}-{at_ms}.jpg")
}

/// Remove every cached thumbnail belonging to `clip_id`. Returns how many went.
///
/// The prefix match is exact about the id: `clip-1-` must not match `clip-12-0.jpg`, hence the
/// trailing dash.
pub(super) fn remove_cached_thumbnails(dir: &Path, clip_id: i64) -> usize {
    let prefix = format!("clip-{clip_id}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        // No cache directory is the normal case for a library whose clips were never thumbnailed.
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".jpg") {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) => tracing::warn!(
                "could not remove the cached thumbnail {}: {err}",
                entry.path().display()
            ),
        }
    }
    removed
}

/// Milliseconds since the Unix epoch, saturating to 0 for a clock before it. Nothing here
/// measures elapsed time; it only has to be the wall clock the store is on.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;
    use replayed_media::binaries::run_with_timeout;
    use std::time::Duration;

    // -- list_clips ----------------------------------------------------------------------

    #[test]
    fn lists_clips_newest_first_by_creation_time_not_by_media_time() {
        let f = Fixture::new();
        // The older row carries the newer media timestamp. The store's own `list_clips` orders by
        // `started_at` and would put it first; the command must not, because `started_at` restarts with
        // the scratch directory and is not comparable across runs.
        let older_row = f.add_row("older-row.mp4", 9_000, 1_000, 10);
        let newer_row = f.add_row("newer-row.mp4", 1_000, 1_000, 10);

        let clips = list_clips(&f.deps()).unwrap();

        assert_eq!(clips.len(), 2);
        assert_eq!(
            clips[0].id, newer_row,
            "the most recently indexed clip is first"
        );
        assert_eq!(clips[1].id, older_row);
    }

    #[test]
    fn an_empty_index_lists_nothing() {
        assert!(list_clips(&Fixture::new().deps()).unwrap().is_empty());
    }

    #[test]
    fn list_clips_leaves_out_rows_whose_files_are_gone_but_keeps_the_rows() {
        // A cleanup removed files without their rows, and the listing handed every dead row to the
        // window: one asset-protocol error and one ffmpeg thumbnail process per dead clip. The listing
        // must not name a file that is gone.
        let f = Fixture::new();
        let live = f.add_row("here.mp4", 0, 1_000, 10);
        let gone = f.add_row("gone.mp4", 0, 1_000, 10);
        std::fs::remove_file(f.paths.clips_dir.join("gone.mp4")).unwrap();

        let clips = list_clips(&f.deps()).unwrap();

        assert_eq!(clips.len(), 1, "a row whose file is gone is not a clip");
        assert_eq!(clips[0].id, live, "the row whose file is there still lists");
        assert_eq!(
            f.store.list_clips().unwrap().len(),
            2,
            "skipped, not deleted: a file can be temporarily unavailable, and \
             silently rewriting the library is not the listing's business"
        );
        assert!(
            find_clip(&f.store, gone).is_ok(),
            "the gone row is still in the index for when its file comes back"
        );
    }

    #[test]
    fn every_surface_counts_the_clips_that_are_actually_there() {
        // The listing filtered out rows whose files were gone and the storage panel did not, so one
        // panel read "No clips are indexed yet" above "Indexed clips 12, In use 1.2 GiB of 50 GiB":
        // the recorded size summed from rows, with none of it on disk.
        //
        // Both surfaces now read one rule: the storage panel counts exactly what the listing shows
        // and (next test) names what it is not counting instead of quietly showing less.
        let f = Fixture::new();
        f.add_row("here.mp4", 0, 1_000, 4_000);
        f.add_row("also-here.mp4", 0, 1_000, 6_000);
        f.add_row("gone.mp4", 0, 1_000, 999_000);
        std::fs::remove_file(f.paths.clips_dir.join("gone.mp4")).unwrap();

        let clips = list_clips(&f.deps()).unwrap();
        let stats = storage_stats(&f.deps()).unwrap();

        assert_eq!(clips.len(), 2);
        assert_eq!(
            stats.clip_count,
            clips.len(),
            "the two panels agree on the count"
        );
        assert_eq!(
            stats.total_bytes, 10_000,
            "and on the bytes: only what is on disk, not what the rows claim"
        );
        assert_eq!(
            stats.missing_count, 1,
            "the hidden row is named, not silently dropped"
        );
        assert_eq!(
            f.store.list_clips().unwrap().len(),
            3,
            "still skipped rather than deleted — a file can be temporarily unavailable"
        );
    }

    // -- trim_clip -----------------------------------------------------------------------

    #[test]
    fn trim_rejects_an_empty_or_inverted_range() {
        let f = Fixture::new();
        let (id, src) = f.add_real_clip("ranges.mp4", 2);
        let src_size = std::fs::metadata(&src).unwrap().len();

        for (start, end) in [(1_000u64, 1_000u64), (1_500, 1_000), (500, 0)] {
            let err = trim_clip(&f.deps(), id, start, end).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRange, "start={start} end={end}");
            assert!(
                err.message.contains("empty or inverted"),
                "the message must say what is wrong: {}",
                err.message
            );
        }

        // Rejected before ffmpeg ran: no file was written and no row was added.
        assert_eq!(
            list_clips(&f.deps()).unwrap().len(),
            1,
            "nothing was indexed"
        );
        assert_eq!(
            std::fs::read_dir(&f.paths.clips_dir).unwrap().count(),
            1,
            "the clips directory still holds only the parent"
        );
        assert_eq!(std::fs::metadata(&src).unwrap().len(), src_size);
    }

    #[test]
    fn trim_rejects_a_range_that_reaches_past_the_end_of_the_clip() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("short.mp4", 2);

        let err = trim_clip(&f.deps(), id, 1_000, 2_001).unwrap_err();

        assert_eq!(err.code, ErrorCode::OutOfRange);
        assert!(err.message.contains("2001ms"), "got: {}", err.message);
        assert!(
            err.message.contains("2000ms"),
            "the message names the real duration: {}",
            err.message
        );
    }

    #[test]
    fn trim_reports_an_unknown_clip() {
        let f = Fixture::new();
        let err = trim_clip(&f.deps(), 42, 0, 100).unwrap_err();
        assert_eq!(err.code, ErrorCode::ClipNotFound);
    }

    #[test]
    fn trim_writes_a_new_clip_beside_its_parent_and_indexes_it() {
        let f = Fixture::new();
        let (id, src) = f.add_real_clip("game.mp4", 4);
        let src_size = std::fs::metadata(&src).unwrap().len();

        let trimmed = trim_clip(&f.deps(), id, 1_500, 3_000).unwrap();

        let expected = f.paths.clips_dir.join("game.trim-1500-3000.mp4");
        assert_eq!(trimmed.path, expected.to_string_lossy());
        assert_eq!(
            expected.parent(),
            src.parent(),
            "the new clip sits beside the original"
        );
        assert!(expected.is_file(), "the file must really be on disk");
        assert!(
            trimmed.duration_ms > 0,
            "the duration is probed from the file that was written"
        );
        assert_eq!(
            trimmed.size_bytes,
            std::fs::metadata(&expected).unwrap().len()
        );
        assert_eq!(
            trimmed.started_at_ms, 1_500,
            "its position on the parent's timeline"
        );
        assert!(!trimmed.favourite);

        let clips = list_clips(&f.deps()).unwrap();
        assert_eq!(clips.len(), 2, "the trim is a clip of its own");
        assert_eq!(clips[0].id, trimmed.id, "and it is the newest row");
        assert!(
            clips.iter().any(|c| c.id == id),
            "the parent is still indexed"
        );

        // The original is untouched: a trim never modifies the clip it came from.
        assert_eq!(std::fs::metadata(&src).unwrap().len(), src_size);
    }

    // -- library metadata: title, tags, game; files handed to the OS ---------------------

    #[test]
    fn renaming_and_tagging_a_clip_changes_the_row_and_never_the_file() {
        let f = Fixture::new();
        let id = f.add_row("raw-name.mp4", 0, 1_000, 1);

        let renamed = set_clip_title(&f.deps(), id, "  Triple kill  ").unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Triple kill"));
        assert!(
            f.paths.clips_dir.join("raw-name.mp4").exists(),
            "the file keeps its name"
        );

        let tagged =
            set_clip_tags(&f.deps(), id, &["Ace".into(), "ace".into(), "funny".into()]).unwrap();
        assert_eq!(tagged.tags, vec!["ace".to_string(), "funny".to_string()]);
        assert_eq!(
            tagged.title.as_deref(),
            Some("Triple kill"),
            "tags do not disturb the title"
        );

        assert_eq!(
            set_clip_title(&f.deps(), id, "").unwrap().title,
            None,
            "blank clears"
        );
    }

    #[test]
    fn metadata_commands_report_unknown_ids_and_refuse_bad_input() {
        let f = Fixture::new();
        let id = f.add_row("a.mp4", 0, 1_000, 1);
        assert_eq!(
            set_clip_title(&f.deps(), id + 9, "x").unwrap_err().code,
            ErrorCode::ClipNotFound
        );
        assert_eq!(
            set_clip_tags(&f.deps(), id + 9, &[]).unwrap_err().code,
            ErrorCode::ClipNotFound
        );
        assert_eq!(
            set_clip_tags(&f.deps(), id, &["  ".into()])
                .unwrap_err()
                .code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            set_clip_title(&f.deps(), id, &"x".repeat(500))
                .unwrap_err()
                .code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn clip_file_resolves_library_files_and_refuses_missing_or_outside_ones() {
        let f = Fixture::new();
        let id = f.add_row("in.mp4", 0, 1_000, 1);
        assert_eq!(
            clip_file(&f.deps(), id).unwrap(),
            f.paths.clips_dir.join("in.mp4")
        );

        std::fs::remove_file(f.paths.clips_dir.join("in.mp4")).unwrap();
        assert_eq!(clip_file(&f.deps(), id).unwrap_err().code, ErrorCode::Io);

        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("out.mp4");
        std::fs::write(&path, b"").unwrap();
        let rogue = f
            .store
            .insert_clip(&NewClip {
                path,
                started_at_ms: 0,
                duration_ms: 1,
                size_bytes: 0,
                codec: "mpeg4".into(),
            })
            .unwrap();
        assert_eq!(
            clip_file(&f.deps(), rogue).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            clip_file(&f.deps(), rogue + 9).unwrap_err().code,
            ErrorCode::ClipNotFound
        );
    }

    #[test]
    fn a_trim_inherits_the_games_and_tags_but_not_the_title() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("meta.mp4", 4);
        f.store.set_clip_game(id, Some("Valorant")).unwrap();
        f.store.set_clip_tags(id, &["ace".into()]).unwrap();
        f.store.set_clip_title(id, "Parent").unwrap();

        let trimmed = trim_clip(&f.deps(), id, 1_000, 3_000).unwrap();
        assert_eq!(trimmed.game.as_deref(), Some("Valorant"));
        assert_eq!(trimmed.tags, vec!["ace".to_string()]);
        assert_eq!(trimmed.title, None);
    }

    #[test]
    fn a_second_identical_trim_does_not_overwrite_the_first() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("twice.mp4", 4);

        let first = trim_clip(&f.deps(), id, 1_000, 2_000).unwrap();
        let second = trim_clip(&f.deps(), id, 1_000, 2_000).unwrap();

        assert_ne!(
            first.path, second.path,
            "the second trim must not clobber the first"
        );
        assert_eq!(
            second.path,
            f.paths
                .clips_dir
                .join("twice.trim-1000-2000-2.mp4")
                .to_string_lossy()
        );
        assert!(Path::new(&first.path).is_file());
        assert_eq!(list_clips(&f.deps()).unwrap().len(), 3);
    }

    #[test]
    fn trim_is_a_stream_copy_not_a_re_encode() {
        // The evidence: the trimmed file's video packets are byte-identical to the packets ffmpeg reads
        // out of the same span of the parent. A re-encode cannot produce the same bytes, which the
        // negative control at the end proves, so this assertion is known to be able to fail.
        let f = Fixture::new();
        let (id, src) = f.add_real_clip("copy.mp4", 4);

        let trimmed = trim_clip(&f.deps(), id, 1_500, 3_000).unwrap();
        let trimmed_path = PathBuf::from(&trimmed.path);

        // Both sides cover the same span, measured from the file written, so a stream copy that could
        // not cut at exactly 3000ms still compares like for like.
        let from_written_file = stream_md5(&f.bins, &trimmed_path, 0, None);
        let from_parent = stream_md5(&f.bins, &src, 1_500, Some(trimmed.duration_ms));
        assert_eq!(
            from_written_file, from_parent,
            "the trimmed packets must be the parent's own packets"
        );

        // Negative control: the same span, re-encoded, is a different byte stream.
        let re_encoded = f.paths.clips_dir.join("re-encoded.mp4");
        let mut cmd = replayed_media::sidecar_command(&f.bins.ffmpeg);
        cmd.args(["-v", "error", "-y", "-ss", "1.500", "-i"])
            .arg(&src)
            .args(["-t", "1.500", "-c:v", "mpeg4", "-q:v", "5", "-g", "10"])
            .arg(&re_encoded);
        let out = run_with_timeout(cmd, Duration::from_secs(60)).unwrap();
        assert!(out.status.success(), "the control encode failed");
        assert_ne!(
            from_parent,
            stream_md5(&f.bins, &re_encoded, 0, None),
            "a re-encode must not hash the same, or this test proves nothing"
        );
    }

    #[test]
    fn trim_can_cut_the_whole_clip() {
        // The upper bound is inclusive: a scrubber dragged to the end asks for exactly `duration_ms`,
        // a legal range.
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("whole.mp4", 2);

        let trimmed = trim_clip(&f.deps(), id, 0, 2_000).unwrap();

        assert_eq!(
            trimmed.path,
            f.paths
                .clips_dir
                .join("whole.trim-0-2000.mp4")
                .to_string_lossy()
        );
        assert!(trimmed.duration_ms > 0);
    }

    // -- set_favourite -------------------------------------------------------------------

    #[test]
    fn set_favourite_flips_the_flag_and_reports_an_unknown_id() {
        let f = Fixture::new();
        let id = f.add_row("fav.mp4", 0, 1_000, 10);
        assert!(!list_clips(&f.deps()).unwrap()[0].favourite);

        let marked = set_favourite(&f.deps(), id, true).unwrap();
        assert_eq!(marked.id, id);
        assert!(marked.favourite);
        assert!(
            list_clips(&f.deps()).unwrap()[0].favourite,
            "the flag is persisted"
        );

        let cleared = set_favourite(&f.deps(), id, false).unwrap();
        assert!(!cleared.favourite);
        assert!(!list_clips(&f.deps()).unwrap()[0].favourite);

        let err = set_favourite(&f.deps(), id + 1, true).unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::ClipNotFound,
            "a no-op update must not look like success"
        );
    }

    // -- combine_clips and export_clip -----------------------------------------------------

    #[test]
    fn combining_two_clips_writes_one_lossless_clip_into_the_library() {
        let f = Fixture::new();
        let (a, _) = f.add_real_clip("a.mp4", 1);
        let (b, _) = f.add_real_clip("b.mp4", 1);

        let combined = combine_clips(&f.deps(), &[a, b]).expect("combine two clips");

        let path = PathBuf::from(&combined.path);
        assert!(path.is_file(), "the combined clip is on disk");
        assert_eq!(
            path.parent(),
            Some(f.paths.clips_dir.as_path()),
            "it lands in the library, not beside a source"
        );
        // A stream copy cannot change the shape: both sources are the same size and codec.
        let info = MediaInfo::probe(&f.bins, &path).unwrap();
        assert!(
            info.duration_ms >= 1_900,
            "two one-second clips make about two seconds, got {}ms",
            info.duration_ms
        );
        assert_eq!(f.store.list_clips().unwrap().len(), 3);
        assert_eq!(
            f.store
                .list_clips()
                .unwrap()
                .iter()
                .filter(|c| c.id == combined.id)
                .count(),
            1,
            "the new clip is indexed exactly once"
        );
    }

    #[test]
    fn combining_needs_two_different_clips() {
        let f = Fixture::new();
        let (id, _) = f.add_real_clip("a.mp4", 1);

        let none = combine_clips(&f.deps(), &[]).unwrap_err();
        assert_eq!(none.code, ErrorCode::InvalidInput);
        assert!(none.message.contains("at least two"), "{}", none.message);

        // The same id twice is one clip, not two: a repeat is a slip, not a request to duplicate
        // the footage.
        let same = combine_clips(&f.deps(), &[id, id]).unwrap_err();
        assert_eq!(same.code, ErrorCode::InvalidInput);
        assert!(same.message.contains("at least two"), "{}", same.message);
        assert_eq!(f.store.list_clips().unwrap().len(), 1);
    }

    #[test]
    fn combining_a_clip_whose_file_is_gone_is_refused_and_indexes_nothing() {
        let f = Fixture::new();
        let (live, _) = f.add_real_clip("live.mp4", 1);
        let dead = f.add_row("dead.mp4", 1_000, 1_000, 1_000);
        std::fs::remove_file(f.paths.clips_dir.join("dead.mp4")).unwrap();

        let err = combine_clips(&f.deps(), &[live, dead]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(err.message.contains("dead.mp4"), "{}", err.message);
        assert_eq!(f.store.list_clips().unwrap().len(), 2, "no row was added");
    }

    #[test]
    fn combining_clips_of_different_sizes_is_a_media_error_naming_both() {
        // The command maps the media layer's layout refusal onto its own vocabulary, so the
        // window shows it in the banner like any other failure.
        let f = Fixture::new();
        let (small, _) = f.add_real_clip("small.mp4", 1);
        write_test_video_sized(&f.bins, &f.paths.clips_dir.join("large.mp4"), 1, 640, 360);
        let large_path = f.paths.clips_dir.join("large.mp4");
        let large = f
            .store
            .insert_clip(&replayed_store::NewClip {
                size_bytes: std::fs::metadata(&large_path).unwrap().len(),
                path: large_path,
                started_at_ms: 0,
                duration_ms: 1_000,
                codec: "mpeg4".into(),
            })
            .unwrap();

        let err = combine_clips(&f.deps(), &[small, large]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Media);
        assert!(
            err.message.contains("320x180") && err.message.contains("640x360"),
            "the refusal names both shapes: {}",
            err.message
        );
        assert_eq!(f.store.list_clips().unwrap().len(), 2, "no row was added");
    }

    #[test]
    fn exporting_a_clip_writes_a_file_beside_it_and_does_not_index_it() {
        let f = Fixture::new();
        let (id, source_path) = f.add_real_clip("clip.mp4", 1);
        let rows_before = f.store.list_clips().unwrap().len();

        let outcome = export_clip(&f.deps(), id, "discord").expect("export");

        assert_eq!(outcome.preset, "discord");
        let path = PathBuf::from(&outcome.path);
        assert!(path.is_file(), "the export is on disk");
        assert_eq!(
            path.parent(),
            source_path.parent(),
            "an export is written beside the clip"
        );
        assert!(path.to_string_lossy().contains("-discord"), "{:?}", path);
        assert!(outcome.size_bytes > 0);
        assert!(outcome.duration_ms > 0);
        assert_eq!(
            f.store.list_clips().unwrap().len(),
            rows_before,
            "an export is not another entry in the library"
        );
        assert!(
            source_path.is_file(),
            "the original is never modified or replaced"
        );
    }

    #[test]
    fn the_gif_export_writes_a_gif() {
        let f = Fixture::new();
        let (id, _) = f.add_real_clip("clip.mp4", 1);

        let outcome = export_clip(&f.deps(), id, "gif").expect("export a gif");

        assert_eq!(outcome.preset, "gif");
        assert!(outcome.path.ends_with(".gif"), "{}", outcome.path);
        let info = MediaInfo::probe(&f.bins, &PathBuf::from(&outcome.path)).unwrap();
        assert_eq!(info.video.as_ref().unwrap().codec, "gif");
    }

    #[test]
    fn exporting_with_an_unknown_preset_is_refused_and_names_the_ones_that_exist() {
        let f = Fixture::new();
        let (id, source_path) = f.add_real_clip("clip.mp4", 1);

        let err = export_clip(&f.deps(), id, "ultrawide").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        for preset in ["discord", "vertical", "gif"] {
            assert!(err.message.contains(preset), "{}", err.message);
        }
        // Nothing was written: the refusal happens before ffmpeg is spawned.
        let siblings: Vec<_> = std::fs::read_dir(&f.paths.clips_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path != &source_path)
            .collect();
        assert!(siblings.is_empty(), "{siblings:?}");
    }

    #[test]
    fn exporting_a_clip_whose_file_is_gone_is_refused() {
        let f = Fixture::new();
        let id = f.add_row("gone.mp4", 1_000, 1_000, 1_000);
        std::fs::remove_file(f.paths.clips_dir.join("gone.mp4")).unwrap();

        let err = export_clip(&f.deps(), id, "discord").unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(err.message.contains("not on disk"), "{}", err.message);
    }

    // -- storage_stats -------------------------------------------------------------------

    #[test]
    fn storage_stats_reports_usage_the_cap_and_a_met_verdict() {
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: 1_000,
            max_age_days: 3_650,
        });
        f.add_row("a.mp4", 0, 1_000, 400);
        let favourite = f.add_row("b.mp4", 0, 1_000, 300);
        set_favourite(&f.deps(), favourite, true).unwrap();

        let stats = storage_stats(&f.deps()).unwrap();

        assert_eq!(stats.clip_count, 2);
        assert_eq!(stats.total_bytes, 700);
        assert_eq!(stats.favourite_count, 1);
        assert_eq!(stats.favourite_bytes, 300);
        assert_eq!(stats.cap_bytes, 1_000);
        assert_eq!(stats.max_age_days, 3_650);
        assert!(stats.cap_met, "700 bytes is inside a 1000 byte cap");
        assert_eq!(stats.over_cap_by_bytes, 0);
        assert_eq!(stats.planned_deletions, 0);
        assert_eq!(stats.bytes_after, 700);
        assert_eq!(stats.clips_dir, f.paths.clips_dir.to_string_lossy());
        assert!(stats.warnings.is_empty());
    }

    #[test]
    fn storage_stats_says_when_the_favourites_alone_exceed_the_cap() {
        // The manager stops rather than delete a favourite to satisfy a cap it cannot meet. The panel
        // has to be able to say why nothing will ever be freed.
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: 500,
            max_age_days: 3_650,
        });
        let favourite = f.add_row("big-favourite.mp4", 0, 1_000, 800);
        set_favourite(&f.deps(), favourite, true).unwrap();
        f.add_row("small.mp4", 0, 1_000, 100);

        let stats = storage_stats(&f.deps()).unwrap();

        assert!(!stats.cap_met);
        assert_eq!(
            stats.over_cap_by_bytes, 300,
            "800 bytes of favourites against a 500 cap"
        );
        assert_eq!(
            stats.planned_deletions, 0,
            "evicting everything else cannot reach this cap"
        );
    }

    #[test]
    fn storage_stats_plans_the_eviction_when_the_cap_is_reachable() {
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: 1_000,
            max_age_days: 3_650,
        });
        f.add_row("old.mp4", 0, 1_000, 600);
        f.add_row("new.mp4", 0, 1_000, 600);

        let stats = storage_stats(&f.deps()).unwrap();

        assert_eq!(stats.total_bytes, 1_200);
        assert!(
            stats.cap_met,
            "with no favourites in the way the cap is reachable"
        );
        assert_eq!(
            stats.planned_deletions, 1,
            "one eviction brings 1200 bytes under the cap"
        );
        assert_eq!(stats.bytes_after, 600);
    }

    #[test]
    fn storage_stats_applies_the_age_rule_against_the_clock_it_is_given() {
        let f = Fixture::with_storage(StorageConfigView {
            max_total_bytes: u64::MAX,
            max_age_days: 7,
        });
        f.add_row("recent.mp4", 0, 1_000, 100);
        let now = now_ms();

        let fresh = storage_stats_at(&f.deps(), now).unwrap();
        assert_eq!(
            fresh.planned_deletions, 0,
            "a clip indexed just now is not a week old"
        );

        let day = 24 * 60 * 60 * 1_000;
        let later = storage_stats_at(&f.deps(), now + 30 * day).unwrap();
        assert_eq!(
            later.planned_deletions, 1,
            "30 days on, the 7 day limit selects it"
        );
        assert!(
            later.cap_met,
            "the age rule does not make the cap unreachable"
        );
        assert_eq!(later.bytes_after, 0);
    }

    #[test]
    fn storage_stats_carries_the_startup_warnings() {
        let mut f = Fixture::new();
        f.warnings = vec!["ffmpeg was not found".to_string()];

        assert_eq!(
            storage_stats(&f.deps()).unwrap().warnings,
            vec!["ffmpeg was not found".to_string()]
        );
    }

    // -- thumbnail -----------------------------------------------------------------------

    #[test]
    fn thumbnail_writes_a_jpeg_into_the_cache_and_reuses_it() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("framed.mp4", 4);

        let first = thumbnail(&f.deps(), id, 1_000).unwrap();

        assert!(!first.cached, "the first request has to run ffmpeg");
        assert_eq!(first.at_ms, 1_000);
        assert_eq!(first.clip_id, id);
        let path = PathBuf::from(&first.path);
        assert_eq!(
            path.parent().unwrap(),
            f.paths.thumbnails_dir,
            "the cache, not the clips dir"
        );
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("clip-{id}-1000.jpg")
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[..3],
            &[0xFF, 0xD8, 0xFF],
            "a JPEG starts with FFD8FF"
        );
        assert!(
            bytes.len() > 200,
            "and is not an empty file: {} bytes",
            bytes.len()
        );

        let second = thumbnail(&f.deps(), id, 1_000).unwrap();
        assert!(
            second.cached,
            "the second request is answered from the cache, not ffmpeg"
        );
        assert_eq!(first.path, second.path);

        // A different frame is a different cache entry.
        let other = thumbnail(&f.deps(), id, 2_000).unwrap();
        assert!(!other.cached);
        assert_ne!(other.path, first.path);
        assert_eq!(
            std::fs::read_dir(&f.paths.thumbnails_dir).unwrap().count(),
            2
        );
    }

    #[test]
    fn thumbnail_rejects_a_timestamp_past_the_end_of_the_clip() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("short.mp4", 2);

        let err = thumbnail(&f.deps(), id, 2_000).unwrap_err();

        assert_eq!(err.code, ErrorCode::OutOfRange);
        assert!(err.message.contains("2000ms"), "got: {}", err.message);
        // A frame from inside the clip is served. The guarantee is "the timestamp names a position
        // inside the clip", not "ffmpeg will find a frame there": the last frame of a stream starts a
        // frame interval before its nominal duration ends, so a timestamp a few milliseconds short of
        // the end can still fail (see `thumbnail`). The UI asks for a frame in the first second.
        assert!(thumbnail(&f.deps(), id, 1_000).is_ok());
    }

    #[test]
    fn thumbnail_reports_an_unknown_clip() {
        let f = Fixture::new();
        assert_eq!(
            thumbnail(&f.deps(), 7, 0).unwrap_err().code,
            ErrorCode::ClipNotFound
        );
    }

    #[test]
    fn thumbnail_refuses_a_clip_whose_file_is_gone_before_anything_is_spawned() {
        // A dead row must never reach ffmpeg: the UI asks per clip and retries, so one missing file
        // would mean a process per attempt. No-ffmpeg deps prove the guard runs before the binaries are
        // even consulted.
        let f = Fixture::new();
        let id = f.add_row("vanished.mp4", 0, 2_000, 10);
        std::fs::remove_file(f.paths.clips_dir.join("vanished.mp4")).unwrap();

        let err = thumbnail(&f.deps_without_ffmpeg(), id, 100).unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains(&format!("clip #{id}")),
            "the message must name the clip: {}",
            err.message
        );
        assert!(
            err.message.contains("is gone"),
            "the message must say the file is gone: {}",
            err.message
        );
    }

    #[test]
    fn a_thumbnail_ffmpeg_cannot_produce_names_the_clip_and_the_timestamp() {
        // A row pointing at something that is not a video: ffmpeg fails, and the message must say which
        // clip and which frame, since "ffmpeg had nothing to read" otherwise looks like a corrupt
        // library.
        let f = Fixture::new();
        let broken = f.paths.clips_dir.join("truncated.mp4");
        std::fs::write(&broken, b"not a video at all").unwrap();
        let id = f
            .store
            .insert_clip(&NewClip {
                path: broken,
                started_at_ms: 0,
                duration_ms: 4_000,
                size_bytes: 16,
                codec: "mpeg4".to_string(),
            })
            .unwrap();

        let err = thumbnail(&f.deps(), id, 1_500).unwrap_err();

        assert_eq!(err.code, ErrorCode::Media);
        assert!(
            err.message.contains(&format!("clip #{id} at 1500ms")),
            "the message must name the clip and the frame: {}",
            err.message
        );
    }

    #[test]
    fn media_commands_fail_clearly_when_ffmpeg_is_missing() {
        // The shell opens without ffmpeg (the list and storage panel need none), so the two commands
        // that do need it must fail with a code the UI can explain, not panic.
        let f = Fixture::new();
        let id = f.add_row("no-ffmpeg.mp4", 0, 1_000, 10);
        let deps = f.deps_without_ffmpeg();

        assert_eq!(
            trim_clip(&deps, id, 0, 500).unwrap_err().code,
            ErrorCode::FfmpegUnavailable
        );
        assert_eq!(
            thumbnail(&deps, id, 100).unwrap_err().code,
            ErrorCode::FfmpegUnavailable
        );

        assert_eq!(
            list_clips(&deps).unwrap().len(),
            1,
            "listing needs no ffmpeg"
        );
        assert_eq!(storage_stats(&deps).unwrap().clip_count, 1);
        assert!(
            set_favourite(&deps, id, true).is_ok(),
            "nor does favouriting"
        );
    }

    // -- delete_clip ---------------------------------------------------------------------

    #[test]
    fn delete_removes_the_row_and_then_the_file() {
        let f = Fixture::new();
        let (id, path) = f.add_real_clip("gone.mp4", 2);
        let size_bytes = std::fs::metadata(&path).unwrap().len();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert!(outcome.row_deleted);
        assert!(outcome.file_removed);
        assert_eq!(outcome.orphaned_path, None);
        assert!(!outcome.already_missing);
        assert_eq!(outcome.bytes_reclaimed, size_bytes);
        assert!(!path.exists(), "the file is unlinked");
        assert!(
            list_clips(&f.deps()).unwrap().is_empty(),
            "and the row is gone"
        );
    }

    #[test]
    fn delete_reports_an_orphan_when_the_file_cannot_be_unlinked() {
        // A directory where a clip file should be: `remove_file` refuses it on every platform, the only
        // portable way to make an unlink fail on purpose. The state left is the recoverable one the UI
        // must be able to name.
        let f = Fixture::new();
        let not_a_file = f.paths.clips_dir.join("not-a-file.mp4");
        std::fs::create_dir(&not_a_file).unwrap();
        let id = f
            .store
            .insert_clip(&NewClip {
                path: not_a_file.clone(),
                started_at_ms: 0,
                duration_ms: 1_000,
                size_bytes: 10,
                codec: "mpeg4".to_string(),
            })
            .unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        let expected = not_a_file.to_string_lossy().into_owned();
        assert!(
            outcome.row_deleted,
            "the row delete is committed before the unlink is tried"
        );
        assert!(!outcome.file_removed);
        assert_eq!(outcome.orphaned_path.as_deref(), Some(expected.as_str()));
        assert_eq!(outcome.bytes_reclaimed, 0);
        assert!(
            not_a_file.exists(),
            "the orphaned path is reported, not hidden"
        );
        assert!(
            list_clips(&f.deps()).unwrap().is_empty(),
            "the row went first and stayed gone: that ordering is the property"
        );
    }

    #[test]
    fn delete_reports_a_row_whose_file_was_already_gone() {
        let f = Fixture::new();
        let id = f.add_row("missing.mp4", 0, 1_000, 10);
        // The state under test: the file was removed without its row (a cleanup that deleted files but
        // left the index alone), which is what the missing-file guards elsewhere are for.
        std::fs::remove_file(f.paths.clips_dir.join("missing.mp4")).unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert!(outcome.row_deleted);
        assert!(!outcome.file_removed);
        assert!(outcome.already_missing);
        assert_eq!(outcome.orphaned_path, None);
        assert_eq!(outcome.bytes_reclaimed, 0);
    }

    #[test]
    fn delete_never_unlinks_a_file_outside_the_library() {
        // A tampered index row naming a file somewhere else entirely: the row goes, the
        // file stays, and the path is reported as an orphan so the UI can say so.
        let f = Fixture::new();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("precious.mp4");
        std::fs::write(&outside, b"not a clip of ours").unwrap();
        let id = f
            .store
            .insert_clip(&NewClip {
                path: outside.clone(),
                started_at_ms: 0,
                duration_ms: 1_000,
                size_bytes: 18,
                codec: "mpeg4".to_string(),
            })
            .unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert!(outcome.row_deleted);
        assert!(!outcome.file_removed);
        assert_eq!(outcome.bytes_reclaimed, 0);
        assert_eq!(
            outcome.orphaned_path.as_deref(),
            Some(outside.to_string_lossy().as_ref())
        );
        assert!(
            outside.exists(),
            "a file outside the library is never unlinked"
        );
    }

    #[test]
    fn delete_never_follows_dot_dot_out_of_the_library() {
        let f = Fixture::new();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("precious.mp4");
        std::fs::write(&outside, b"x").unwrap();
        // clips/../../<outside>/precious.mp4 resolves to `outside` but looks library-local.
        // Enough `..` to reach the filesystem root from anywhere, then back down.
        let mut sneaky = f.paths.clips_dir.clone();
        for _ in 0..32 {
            sneaky.push("..");
        }
        sneaky.push(outside.strip_prefix("/").unwrap_or(&outside));
        assert_eq!(
            std::fs::canonicalize(&sneaky).unwrap(),
            std::fs::canonicalize(&outside).unwrap()
        );
        let id = f
            .store
            .insert_clip(&NewClip {
                path: sneaky,
                started_at_ms: 0,
                duration_ms: 1_000,
                size_bytes: 1,
                codec: "mpeg4".to_string(),
            })
            .unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert!(!outcome.file_removed);
        assert!(outside.exists());
    }

    #[test]
    fn delete_of_an_unknown_id_unlinks_nothing() {
        // No file is deleted unless a committed database row names it, so an id with no row must leave
        // any file of that name where it is.
        let f = Fixture::new();
        let stray = f.paths.clips_dir.join("stray.mp4");
        std::fs::write(
            &stray,
            b"not indexed, and therefore not this command's to delete",
        )
        .unwrap();

        let outcome = delete_clip(&f.deps(), 999).unwrap();

        assert!(!outcome.row_deleted);
        assert!(!outcome.file_removed);
        assert!(stray.is_file(), "an unindexed file is not deleted");
    }

    #[test]
    fn delete_takes_that_clip_cached_thumbnails_with_it() {
        let f = Fixture::new();
        let (id, _src) = f.add_real_clip("thumbed.mp4", 2);
        let thumb = thumbnail(&f.deps(), id, 1_000).unwrap();
        let thumb_path = PathBuf::from(&thumb.path);
        assert!(thumb_path.is_file());

        // Another clip's cache entry, whose id shares a prefix with this one's.
        let neighbour = f
            .paths
            .thumbnails_dir
            .join(thumbnail_file_name(id + 100, 1_000));
        std::fs::write(&neighbour, b"another clip's frame").unwrap();

        let outcome = delete_clip(&f.deps(), id).unwrap();

        assert_eq!(outcome.thumbnails_removed, 1);
        assert!(
            !thumb_path.exists(),
            "the deleted clip's cache entry goes with it"
        );
        assert!(
            neighbour.is_file(),
            "a neighbouring clip's cache entry stays"
        );
    }

    // -- pure helpers --------------------------------------------------------------------

    #[test]
    fn the_trim_output_name_carries_the_range_it_was_cut_from() {
        assert_eq!(
            trim_output_path(Path::new("/clips/game.mp4"), 1_500, 3_000),
            PathBuf::from("/clips/game.trim-1500-3000.mp4")
        );
        // The extension is carried over, never assumed (the muxer flags `trim_lossless` passes are only
        // accepted by an ISO-BMFF muxer).
        assert_eq!(
            trim_output_path(Path::new("clip.mkv"), 0, 10),
            PathBuf::from("clip.trim-0-10.mkv")
        );
        // A stem containing dots keeps every part but the last extension.
        assert_eq!(
            trim_output_path(Path::new("/clips/2026.09.23 game.mp4"), 5, 6),
            PathBuf::from("/clips/2026.09.23 game.trim-5-6.mp4")
        );
    }

    #[test]
    fn a_free_output_name_steps_around_the_files_already_on_disk() {
        let f = Fixture::new();
        let candidate = f.paths.clips_dir.join("taken.trim-0-1000.mp4");
        std::fs::write(&candidate, b"a previous trim").unwrap();

        let chosen = free_output_path(candidate.clone()).unwrap();
        assert_eq!(chosen, f.paths.clips_dir.join("taken.trim-0-1000-2.mp4"));

        // With that one taken too, the next number is used — never an overwrite.
        std::fs::write(&chosen, b"and another").unwrap();
        assert_eq!(
            free_output_path(candidate.clone()).unwrap(),
            f.paths.clips_dir.join("taken.trim-0-1000-3.mp4")
        );
        assert_eq!(
            std::fs::read(&chosen).unwrap(),
            b"and another",
            "nothing was clobbered"
        );
    }

    #[test]
    fn the_thumbnail_cache_name_is_derived_from_the_clip_and_the_frame() {
        assert_eq!(thumbnail_file_name(1, 0), "clip-1-0.jpg");
        assert_eq!(thumbnail_file_name(12, 1_500), "clip-12-1500.jpg");
    }
}
