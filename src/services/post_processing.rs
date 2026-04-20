use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::models::log::LogCategory;
use crate::models::{artwork_cache, config, episode_tags, grabbed_torrents, local_metadata, metadata_cache, series};
use crate::services::source::{self, SeriesContext};
use crate::services::{artwork, logger, media, nfo};
use crate::AppState;

static POST_PROC_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// States qBittorrent reports for a fully downloaded torrent.
fn is_complete(state: &str) -> bool {
    matches!(
        state,
        "uploading"
            | "stalledUP"
            | "queuedUP"
            | "forcedUP"
            | "pausedUP"
            | "checkingUP"
            | "seeding"
            | "stoppedUP"
    )
}

/// States that indicate a torrent has failed or has errors.
fn is_errored(state: &str) -> bool {
    matches!(state, "error" | "missingFiles")
}

/// Check if a grab is older than `max_age_secs` seconds.
pub fn grab_is_stale(grabbed_at: &str, max_age_secs: i64) -> bool {
    // grabbed_at is SQLite CURRENT_TIMESTAMP format: "YYYY-MM-DD HH:MM:SS"
    let Some(grab_time) = chrono::NaiveDateTime::parse_from_str(grabbed_at, "%Y-%m-%d %H:%M:%S").ok() else {
        return false;
    };
    let now = chrono::Utc::now().naive_utc();
    let elapsed = now.signed_duration_since(grab_time).num_seconds();
    elapsed > max_age_secs
}

fn is_video_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    matches!(
        Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "mkv" | "mp4" | "avi" | "wmv" | "webm" | "m4v" | "ts"
    )
}

/// Replace filesystem-unsafe characters in a filename component.
fn sanitize_filename(s: &str) -> String {
    media::sanitize_folder_name(s)
}

/// Hardlink → copy fallback. For "move" mode: rename → copy+delete fallback.
///
/// Runs the whole operation under `spawn_blocking` because a Blu-ray
/// episode cross-device copy can easily be 1–4 GB and blocks for
/// multiple seconds; doing that on a tokio worker starves the RSS sync,
/// HTTP handlers, and other background tasks sharing the same runtime.
async fn do_file_op(mode: &str, src: &Path, dst: &Path) -> std::io::Result<()> {
    let mode = mode.to_string();
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        match mode.as_str() {
            "move" => {
                // Same-fs rename is atomic and instant — the happy path.
                if std::fs::rename(&src, &dst).is_ok() {
                    return Ok(());
                }
                // Cross-fs fallback: copy to a sibling tmp first then
                // rename onto dst so a partially-copied file can't be
                // observed at dst by a subsequent pass and mistaken for
                // a finished import. Cleans up the tmp on rename failure.
                let mut tmp = dst.as_os_str().to_os_string();
                tmp.push(".ryokan-tmp");
                let tmp = PathBuf::from(tmp);
                std::fs::copy(&src, &tmp)?;
                if let Err(e) = std::fs::rename(&tmp, &dst) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                // Source-remove failure is rare (qBit still holds the
                // file open, source dir is read-only, etc.) and the
                // file is safely at dst either way — but surface a warn
                // so the operator can spot duplicate state in qBit's
                // downloads directory.
                if let Err(e) = std::fs::remove_file(&src) {
                    tracing::warn!(
                        target: "ryokan::post_processing",
                        src = %src.display(),
                        error = %e,
                        "post-copy remove_file failed; file remains at source AND destination",
                    );
                }
                Ok(())
            }
            "copy" => {
                std::fs::copy(&src, &dst)?;
                Ok(())
            }
            _ => {
                // "hardlink" (default): hardlink preferred, copy on failure (cross-fs).
                if std::fs::hard_link(&src, &dst).is_err() {
                    std::fs::copy(&src, &dst)?;
                }
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| std::io::Error::other(format!("join error: {}", e)))?
}

/// Copy a cached artwork blob (poster or banner) to every path in
/// `dests`, self-healing the cache on a miss when `source_url` is
/// available. Returns a `Vec<bool>` aligned to `dests` — `true` at
/// position `i` means the blob landed at `dests[i]`.
///
/// **Self-healing rationale:**
/// `metadata_sync::refresh_series_metadata_inner` is the only path
/// that eagerly caches series artwork, and every `artwork::cache_image`
/// error there is swallowed via `let _ = ...`. A transient fetch
/// failure (AL CDN hiccup, a cancelled rebuild sweep, Jikan-fallback
/// with an empty banner field, …) leaves the blob missing
/// indefinitely. Baking the re-fetch into `copy_artwork` itself
/// (rather than a separate pre-step the caller has to remember to
/// run) makes the self-heal an invariant: any future caller that
/// supplies a source URL automatically inherits it.
///
/// **I/O shape:** the blob bytes are read into memory once and
/// written to every dest under a single `spawn_blocking`, so a
/// network-backed media root pays one open per import rather than
/// `dests.len()` opens. Failures are non-fatal (Jellyfin can still
/// scrape its own artwork) but every failure path warns with enough
/// detail to debug it — silent "no artwork in Jellyfin" was the
/// symptom the caller-side self-heal was designed to fix.
///
/// **Args:**
/// - `cache_key` — e.g. `series-{id}-cover` or `series-{id}-banner`
/// - `image_kind` — "cover" or "banner" (matches
///   [`artwork::cache_image`]'s `image_kind`; also used verbatim in
///   log text)
/// - `source_url` — optional upstream URL to re-cache from on a miss;
///   pass `None` when the caller has nothing to fetch from (e.g. a
///   series that ran on the pure Jikan fallback with no banner field)
async fn copy_artwork(
    db: &sqlx::SqlitePool,
    series_id: i64,
    cache_key: &str,
    image_kind: &str,
    source_url: Option<&str>,
    dests: &[&Path],
) -> Vec<bool> {
    if dests.is_empty() {
        return Vec::new();
    }

    // Step 1: cache miss + non-empty source URL → self-heal by
    // re-fetching. Two SELECTs on the unhappy path, one on the happy
    // path (the re-check after the fetch is necessary because the
    // fetch may have partially succeeded — e.g. blob written but ref
    // upsert failed).
    //
    // `metadata_sync::refresh_series_metadata_inner` is the only
    // upstream caller of `cache_image`, and every error there is
    // swallowed via `let _ = ...`. A transient fetch failure (AL CDN
    // hiccup, a cancelled rebuild sweep, a Jikan-fallback with an
    // empty banner field, …) would otherwise leave the blob missing
    // indefinitely — baking the re-fetch in here makes the self-heal
    // an invariant that any future caller supplying a source URL
    // inherits automatically.
    let mut cached = artwork_cache::get(db, cache_key).await.ok().flatten();
    if cached.is_none()
        && let Some(url) = source_url.filter(|u| !u.trim().is_empty())
    {
        tracing::debug!(
            target: "ryokan::post_processing",
            series_id,
            cache_key,
            image_kind,
            "artwork cache miss; fetching on demand",
        );
        if let Err(err) = artwork::cache_image(
            db,
            cache_key,
            "series",
            Some(series_id),
            image_kind,
            url,
        )
        .await
        {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!(
                    "On-demand artwork fetch failed for {image_kind} on series_id={series_id}"
                ),
                &format!("source_url={url}, error={err}"),
            )
            .await;
        }
        cached = artwork_cache::get(db, cache_key).await.ok().flatten();
    }

    let Some(entry) = cached else {
        tracing::debug!(
            target: "ryokan::post_processing",
            series_id,
            cache_key,
            image_kind,
            "artwork blob missing from cache and no recoverable source; skipping copy",
        );
        return vec![false; dests.len()];
    };

    // Step 2: read the blob bytes once, fan out to each dest. Under
    // one `spawn_blocking` so an NFS-backed media root doesn't
    // serialize file-open round-trips from separate tokio tasks.
    let src = std::path::PathBuf::from(&entry.local_path);
    let owned_dests: Vec<std::path::PathBuf> = dests.iter().map(|p| p.to_path_buf()).collect();
    let src_display = src.display().to_string();
    let copy_result = tokio::task::spawn_blocking(move || -> Vec<Result<(), std::io::Error>> {
        let bytes = match std::fs::read(&src) {
            Ok(b) => b,
            Err(e) => {
                // Fan the source-read error out to every dest so the
                // caller sees `false` for each one; log once at the
                // call site (below) with the source path.
                return owned_dests.iter().map(|_| Err(std::io::Error::new(e.kind(), e.to_string()))).collect();
            }
        };
        owned_dests
            .iter()
            .map(|dst| -> std::io::Result<()> {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(dst, &bytes)?;
                Ok(())
            })
            .collect()
    })
    .await;

    let mut results = Vec::with_capacity(dests.len());
    match copy_result {
        Ok(per_dest) => {
            for (i, r) in per_dest.into_iter().enumerate() {
                match r {
                    Ok(()) => results.push(true),
                    Err(err) => {
                        logger::warn(
                            db,
                            LogCategory::PostProcess,
                            &format!(
                                "Failed to copy series {image_kind} for series_id={series_id}"
                            ),
                            &format!(
                                "src={}, dst={}, error={}",
                                src_display,
                                dests[i].display(),
                                err
                            ),
                        )
                        .await;
                        results.push(false);
                    }
                }
            }
        }
        Err(join_err) => {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!("{image_kind} copy task panicked for series_id={series_id}"),
                &format!("src={}, error={}", src_display, join_err),
            )
            .await;
            results.extend((0..dests.len()).map(|_| false));
        }
    }
    results
}

/// Copy the cached series poster to each of `dests`. The cache is
/// self-healed from `source_url` if the blob is missing. Always
/// written as JPEG regardless of original extension — Jellyfin
/// accepts it.
async fn copy_poster(
    db: &sqlx::SqlitePool,
    series_id: i64,
    source_url: Option<&str>,
    dests: &[&Path],
) -> Vec<bool> {
    let cache_key = format!("series-{}-cover", series_id);
    copy_artwork(db, series_id, &cache_key, "cover", source_url, dests).await
}

/// Copy the cached series banner to each of `dests`. Usually just
/// one dest (series-root `banner.jpg`). Jellyfin reads that slot for
/// the series-level banner; writing it locally prevents the
/// TVDB/TMDB fallback scrape from picking a mismatched banner when
/// the anime doesn't cleanly map 1:1 to an external provider entry.
async fn copy_banner(
    db: &sqlx::SqlitePool,
    series_id: i64,
    source_url: Option<&str>,
    dests: &[&Path],
) -> Vec<bool> {
    let cache_key = format!("series-{}-banner", series_id);
    copy_artwork(db, series_id, &cache_key, "banner", source_url, dests).await
}

/// #30 — Decide what offset to subtract from a parsed filename episode
/// number when the file isn't covered by a Phase 2 auto-expand route
/// row. Used by the legacy single-series path so absolute-numbered
/// releases of sequel cours (`[SubsPlease] Jujutsu Kaisen - 56` for
/// JJK S3 E9 with prior-cour total 47) land on the correct relative
/// episode.
///
/// Rule: if the series has a non-zero `cumulative_prior_episodes` AND
/// the parsed number exceeds that cumulative, treat the filename as
/// absolute-numbered and subtract. Otherwise return 0 (relative
/// numbering, file renames to its parsed number as-is).
///
/// Example with JJK S3 (cumulative = 47, own episodes = 12):
///   - raw = 56 (SubsPlease absolute) → 56 > 47 → offset = 47 → ep = 9. ✓
///   - raw = 9 (Erai-raws relative) → 9 ≤ 47 → offset = 0 → ep = 9. ✓
///   - raw = 25 (stray S2 E1 file that got mis-grabbed) → 25 ≤ 47 →
///     offset = 0, file lands as E25 of S3 (which doesn't exist,
///     subsequent rename fails loudly — correct behavior).
///
/// First-season entries (cumulative = 0) short-circuit to offset = 0
/// so the legacy behavior is preserved everywhere absolute numbering
/// can't apply.
fn fallback_ep_offset(raw_ep_num: i32, cumulative_prior_episodes: i32) -> i32 {
    if cumulative_prior_episodes > 0 && raw_ep_num > cumulative_prior_episodes {
        cumulative_prior_episodes
    } else {
        0
    }
}

/// Per-series derived state used during an import. Built once per target
/// series_id on demand inside [`import_torrent`] so a multi-series batch
/// grab doesn't re-fetch the same rows or re-create the same directories
/// for every file. The single-series case fills exactly one entry; a
/// Phase 2 auto-expanded batch fills one entry per sibling touched.
struct SeriesImportCtx {
    series: series::Series,
    folder_name: String,
    series_title: String,
    season_dir: PathBuf,
    ep_meta: HashMap<i32, local_metadata::CachedEpisodeMetadata>,
    /// Cached AniList detail used to enrich episode + series NFOs with
    /// plot, genres, runtime, etc. `None` when the per-series metadata
    /// cache is empty — the NFO writers fall back to the minimal
    /// series-row-only shape.
    cached_detail: Option<crate::services::anilist::AnimeDetail>,
    runtime_minutes: Option<i32>,
    /// Snapshot of `episode_quality_tags` for this series at import
    /// start. Used by the per-file post-download reclassify path to
    /// decide whether to UPDATE in place vs INSERT a new row, and to
    /// log the prior source for diagnostics. Refreshed once per
    /// series-ctx build — safe because within a single import pass
    /// each episode is written at most once, so later files can't
    /// depend on earlier files' writes landing in this map.
    existing_tags: HashMap<i32, episode_tags::EpisodeQualityTag>,
}

/// Resolve the [`SeriesImportCtx`] for `series_id`: loads the series
/// row, materializes its folder name + season directory, and warms up
/// the episode metadata and AniList detail caches. Split out of
/// [`import_torrent`] so a multi-series routed batch can reuse the same
/// context across files without re-running the expensive preamble.
async fn load_series_import_ctx(
    state: &AppState,
    cfg: &config::Config,
    series_id: i64,
) -> Result<SeriesImportCtx, String> {
    let series = series::get_by_id(&state.db, series_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("series {} not found", series_id))?;

    // Auto-generate folder_name from the best title if it was never set.
    let folder_name = if series.folder_name.is_empty() {
        let generated = media::sanitize_folder_name(&nfo::best_title(&series));
        if generated.is_empty() {
            return Err(format!(
                "series '{}' has no usable title for folder name",
                series.title
            ));
        }
        // Persist it so future imports skip this path.
        let _ = series::update_folder(&state.db, series.id, &generated).await;
        generated
    } else {
        series.folder_name.clone()
    };

    // `series_title` flows into `<showtitle>` in every episode NFO and
    // into the renamed filename stem, so it respects the user's
    // `title_language` preference. `folder_name` above stays on
    // `best_title` because it's a one-time persisted default — later
    // preference changes should not rename folders.
    let series_title = nfo::title_for_preference(&series, &cfg.title_language);
    let season_dir = Path::new(&cfg.media_root)
        .join(&folder_name)
        .join(format!("Season {:02}", 1_i32));

    {
        let season_dir = season_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&season_dir))
            .await
            .map_err(|e| format!("create season dir join: {}", e))?
            .map_err(|e| format!("create season dir: {}", e))?;
    }

    let ep_meta = local_metadata::get_episode_map_for_series(&state.db, series.id)
        .await
        .unwrap_or_default();

    // Cached AniList detail. Used to enrich both series and episode NFOs
    // (plot, year, rating, runtime, real genres) so Jellyfin doesn't have
    // to scrape its own metadata. Optional — falls back to the minimal
    // series-row-only NFO when the cache is empty.
    let cached_detail = metadata_cache::get_by_series_id(&state.db, series.id)
        .await
        .ok()
        .flatten()
        .map(|c| c.detail);
    let runtime_minutes = cached_detail.as_ref().and_then(|d| d.duration);

    // Load the full `episode_quality_tags` snapshot once here instead
    // of inside the per-file import loop. Previously this ran N times
    // per batch (one fetch per file) even though each file only writes
    // its own episode row and no inter-file read dependency exists.
    let existing_tags = episode_tags::get_for_series(&state.db, series.id)
        .await
        .unwrap_or_default();

    Ok(SeriesImportCtx {
        series,
        folder_name,
        series_title,
        season_dir,
        ep_meta,
        cached_detail,
        runtime_minutes,
        existing_tags,
    })
}

/// Process a single completed torrent. Returns `true` if at least one file was
/// imported, `false` if there was nothing to do yet.
///
/// Phase 2: if the grab has routing rows in `grabbed_torrent_series`
/// (written by the auto-expand path when a megapack contained sibling
/// entries), each file is routed to the sibling's own library folder
/// instead of the parent's. Grabs without routes fall through to the
/// legacy single-series behavior where every file targets
/// `grab.series_id`.
async fn import_torrent(
    state: &AppState,
    cfg: &config::Config,
    grab: &grabbed_torrents::GrabbedTorrent,
    torrent_hash: &str,
    torrent_save_path: &str,
) -> Result<bool, String> {
    let qbit = state
        .qbit
        .read()
        .await
        .clone()
        .ok_or("qBittorrent not configured")?;

    let files = qbit
        .get_torrent_files(torrent_hash)
        .await
        .map_err(|e| format!("get torrent files: {}", e))?;

    // Phase 2: look up per-file routing rows written by the auto-expand
    // path. A non-empty result means this grab was an auto-expanded
    // batch and each file is tagged with the sibling series_id it
    // belongs to; an empty result is the legacy path where every file
    // routes to `grab.series_id` (pre-Phase-2 grabs, or Phase-2 grabs
    // where sibling detection returned nothing).
    let mut routes = grabbed_torrents::get_series_routes(&state.db, grab.id)
        .await
        .unwrap_or_default();

    // Grab-time auto-expand can fail when qBit's metadata wait times
    // out on a slow tracker (see the 180s wait in
    // `handlers::library::search::auto_expand_library_from_pack`). By
    // import time the file list is always available — if the grab was
    // a batch and no routes were written, retry sibling detection now
    // so siblings still land in their own folders instead of every
    // file falling back to the parent. Motivating case (#45): the
    // HorribleSubs JoJo P3 48-ep pack, where the grab-time wait timed
    // out and Egypt-hen never got auto-added.
    if routes.is_empty() && grab.is_batch && grab.series_id > 0 {
        match metadata_cache::get_by_series_id(&state.db, grab.series_id).await {
            Ok(Some(cached)) => {
                let filenames: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
                let parent_eps: Vec<i32> = cached
                    .detail
                    .episodes
                    .filter(|n| *n > 0 && *n <= 1000)
                    .map(|n| (1..=n).collect())
                    .unwrap_or_default();
                // Synthetic grab context. The per-episode tag rows
                // `expand_from_files` writes for new siblings get
                // their classifications overwritten by
                // `classify_post_download` further down, so
                // `ClassificationResult::unknown()` is fine here.
                // Release group and size are recoverable via post-
                // download paths too.
                let ctx = crate::services::auto_expand::AutoExpandGrabContext {
                    classification: crate::services::source::ClassificationResult::unknown(),
                    release_group: String::new(),
                    size_bytes: 0,
                };
                let _ = crate::services::auto_expand::expand_from_files(
                    &state.db,
                    &filenames,
                    &cached.detail,
                    grab.series_id,
                    &parent_eps,
                    grab.id,
                    &grab.torrent_name,
                    &ctx,
                )
                .await;
                // Reload regardless of return value: `expand_from_files`
                // writes routes when it detects siblings even if those
                // siblings were already tracked (added=0 but routes
                // written).
                routes = grabbed_torrents::get_series_routes(&state.db, grab.id)
                    .await
                    .unwrap_or_default();
            }
            Ok(None) => {
                // Rare but possible: a grab landed before the metadata
                // sync populated the cache for this series. Log so
                // operators can trace "batch imported but siblings
                // never added" without reading the code.
                logger::debug(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Auto-expand retry skipped for '{}' — no cached AniList detail for parent series_id={}",
                        grab.torrent_name, grab.series_id,
                    ),
                    "",
                )
                .await;
            }
            Err(e) => {
                logger::debug(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Auto-expand retry skipped for '{}' — metadata_cache lookup failed for parent series_id={}",
                        grab.torrent_name, grab.series_id,
                    ),
                    &e.to_string(),
                )
                .await;
            }
        }
    }

    // file_idx → (target series_id, episode_offset), flattened from
    // the routes table. `episode_offset` is subtracted from each
    // file's parsed episode number before the file is renamed /
    // tagged so absolute-numbered batches (e.g. smol Monogatari
    // S07E14 → Owari S2 E01 with offset 13) land under the correct
    // arc-local episode number. Offset is 0 for siblings with arc-
    // local numbering and for all legacy routes (via COALESCE in the
    // model read).
    let routes_by_file: HashMap<usize, (i64, i32)> = routes
        .iter()
        .flat_map(|r| {
            let series_id = r.series_id;
            let offset = r.episode_offset;
            r.file_indices
                .iter()
                .map(move |i| (*i, (series_id, offset)))
        })
        .collect();

    // Preserve the canonical qBit file index alongside each entry so
    // completed files can be correlated back to their route row. qBit
    // returns files in a deterministic order keyed by file index, so
    // `enumerate()` applied to the untouched `files` vec yields the
    // same indices that `detect_sibling_entries_in_pack` recorded at
    // grab time.
    let video_files: Vec<(usize, &crate::services::qbit::TorrentFile)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.progress >= 1.0 && is_video_file(&f.name))
        .collect();

    if video_files.is_empty() {
        // #27 — log this at debug rather than silently looping. qBit
        // reported the torrent state as complete but nothing here looks
        // like a finished video file. Most of the time this is a race
        // where the post-proc tick beat qBit's per-file progress update;
        // the next tick will find the files. Rare pathological case is
        // a samples/.nfo-only torrent that stays Ok(false) forever —
        // those would need the stuck-pending timeout fix (future work,
        // tracked separately from this commit).
        logger::debug(
            &state.db,
            LogCategory::PostProcess,
            &format!(
                "No complete video files yet for '{}' — retrying next tick",
                grab.torrent_name
            ),
            "",
        )
        .await;
        return Ok(false);
    }

    // Determine the source base path. If qbit_download_path is configured, use
    // that instead of qBit's internal save_path — this handles Docker path
    // mapping where qBit sees /downloads/ but Ryokan sees a different mount.
    let source_base = if !cfg.qbit_download_path.is_empty() {
        cfg.qbit_download_path.clone()
    } else {
        torrent_save_path.to_string()
    };

    // Lazily-loaded per-series context cache. The single-series case
    // fills exactly one entry; a multi-series routed batch fills one
    // entry per sibling touched.
    let mut series_ctx_cache: HashMap<i64, SeriesImportCtx> = HashMap::new();
    // Unique series_ids that had at least one file successfully
    // imported — drives the per-series NFO/poster write after the loop.
    let mut touched_series: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    // Per-target-series tuple (episode number, individual file size,
    // post-processed on-disk file name) for every file we successfully
    // landed. Replaces the `grab.episode_numbers`-based mark_completed
    // at the end of the loop: bare batch grabs arrive here with an
    // empty episode list on the parent grab row, but we've already
    // parsed ep_num per file above so we can mark completed with the
    // real list instead.
    //
    // The per-file size feeds `mark_grab_history_completed`'s
    // non-batch-only size-refine path (batch rows retain their whole-
    // torrent total so the episode detail modal can show "from an
    // X GiB batch"). The on-disk file name feeds the same function so
    // each per-episode row carries the Sonarr-style renamed basename
    // (e.g. `Jujutsu Kaisen - S01E06 - Hidden Inventory.mkv`) instead
    // of the batch torrent's release title.
    // BTreeMap for deterministic iteration order downstream — the
    // post-loop `mark_completed` / `mark_grab_history_completed` pass
    // runs in series_id ascending order every run, matching
    // `touched_series`'s BTreeSet so log interleaving is stable and
    // greppable. Functionally equivalent to HashMap; pure log hygiene.
    let mut imported_eps_by_series: std::collections::BTreeMap<i64, Vec<(i32, i64, String)>> =
        std::collections::BTreeMap::new();
    let mut imported_count = 0_usize;

    for (file_idx, file) in &video_files {
        // Route this file: prefer the routes table (Phase 2 batch
        // auto-expansion), fall back to `grab.series_id` for legacy
        // grabs and for any completed video file whose index wasn't
        // covered by a route (e.g. extension mismatch between
        // `auto_search::is_media_filename` and [`is_video_file`]).
        //
        // `ep_offset` is computed below, once `ctx` is loaded and the
        // filename is parsed — the legacy fallback needs
        // `series.cumulative_prior_episodes` (#30) to pick the right
        // offset for absolute-numbered releases like
        // `[SubsPlease] Jujutsu Kaisen - 56` (which must land as S3 E9,
        // not S3 E56).
        let target_series_id = routes_by_file
            .get(file_idx)
            .map(|(sid, _)| *sid)
            .unwrap_or(grab.series_id);

        // Can't use the clean `Entry::or_insert_with_async` pattern
        // because the loader is async and `entry()` borrows the map
        // across the await. Branching on the Entry variant keeps the
        // hot path (cache hit) to a single lookup and only calls the
        // loader on a cold miss.
        let ctx = match series_ctx_cache.entry(target_series_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match load_series_import_ctx(state, cfg, target_series_id).await {
                    Ok(ctx) => entry.insert(ctx),
                    Err(e) => {
                        logger::error(
                            &state.db,
                            LogCategory::PostProcess,
                            &format!(
                                "Failed to load series context for id={}",
                                target_series_id
                            ),
                            &e,
                        )
                        .await;
                        continue;
                    }
                }
            }
        };

        let src: PathBuf = Path::new(&source_base).join(&file.name);

        let filename_only = Path::new(&file.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&file.name);

        // Parse episode number from the filename. The `grab.episode_numbers`
        // fallback only makes sense for the legacy single-series path —
        // in a routed batch the routes rows already carry per-sibling
        // episode numbers and the parent grab's list doesn't apply to
        // sibling files.
        let ep_num = media::parse_episode_number(&filename_only.to_lowercase())
            .map(|(_, ep)| ep)
            .or_else(|| {
                if routes_by_file.is_empty() {
                    grab.episode_numbers.first().copied()
                } else {
                    None
                }
            });

        let Some(raw_ep_num) = ep_num else {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!("Could not parse episode number from '{}'", filename_only),
                &format!("series={}", ctx.series.title),
            )
            .await;
            continue;
        };

        // Decide the episode-number offset to subtract:
        //   1. If a route row covered this file (Phase 2 batch auto-
        //      expansion), use the offset the auto-expand path stored
        //      at grab time — e.g. smol Monogatari S07E14 → Owari S2 E01
        //      with offset 13, NoobSubs JoJo E25 → Egypt-hen E01 with
        //      offset 24.
        //   2. Otherwise (single-series legacy fallback) use the
        //      series's cumulative prior-cour episode count (#30) when
        //      the parsed number is clearly in the absolute-numbering
        //      range (greater than the prior-cour total). Example:
        //      `[SubsPlease] Jujutsu Kaisen - 56` → raw_ep_num=56,
        //      cumulative=47, 56 > 47 → offset=47, 56 - 47 = E9 of S3.
        //      A relative-numbered release from the same series
        //      ("Jujutsu Kaisen - 09", raw=9) correctly falls through
        //      to offset=0 because 9 is not greater than 47.
        //
        // A non-positive result after subtraction means the file
        // landed on a sibling whose offset is larger than the file's
        // own episode number — log and skip rather than write a bogus
        // E0 file.
        let ep_offset = routes_by_file
            .get(file_idx)
            .map(|(_, off)| *off)
            .unwrap_or_else(|| {
                fallback_ep_offset(raw_ep_num, ctx.series.cumulative_prior_episodes)
            });
        let ep_num = raw_ep_num - ep_offset;
        if ep_num <= 0 {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Skipping '{}' — effective ep {} after offset {} is non-positive",
                    filename_only, ep_num, ep_offset
                ),
                &format!(
                    "series={}, raw_ep={}, ep_offset={}",
                    ctx.series.title, raw_ep_num, ep_offset
                ),
            )
            .await;
            continue;
        }

        let ep_title = ctx
            .ep_meta
            .get(&ep_num)
            .map(|m| {
                if !m.title_english.is_empty() {
                    m.title_english.clone()
                } else if !m.title.is_empty() {
                    m.title.clone()
                } else {
                    m.title_romaji.clone()
                }
            })
            .unwrap_or_default();

        let aired = ctx
            .ep_meta
            .get(&ep_num)
            .map(|m| m.aired.clone())
            .unwrap_or_default();

        let ext = Path::new(filename_only)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mkv");

        let season = 1_i32;
        let dest_stem = if ep_title.is_empty() {
            format!(
                "{} - S{:02}E{:02}",
                sanitize_filename(&ctx.series_title),
                season,
                ep_num
            )
        } else {
            format!(
                "{} - S{:02}E{:02} - {}",
                sanitize_filename(&ctx.series_title),
                season,
                ep_num,
                sanitize_filename(&ep_title)
            )
        };

        let dest_video = ctx.season_dir.join(format!("{}.{}", dest_stem, ext));
        let dest_nfo = ctx.season_dir.join(format!("{}.nfo", dest_stem));

        // Check for existing files with the same SxxExx tag (any extension).
        // Matching by episode tag instead of full stem handles cases where the
        // episode title changed in AniList between the original grab and the
        // upgrade (e.g. a translated title was added later).
        let ep_tag = format!("S{:02}E{:02}", season, ep_num);
        // Walk the season directory off the runtime — a big season pack on
        // an NFS mount can make the sync read_dir/stat calls block for
        // hundreds of ms. The filter logic is cheap CPU, so we also move
        // it into the spawned task.
        let existing_for_ep: Vec<PathBuf> = {
            let season_dir = ctx.season_dir.clone();
            let ep_tag = ep_tag.clone();
            tokio::task::spawn_blocking(move || -> Vec<PathBuf> {
                std::fs::read_dir(&season_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .map(|s| s.contains(&ep_tag))
                            .unwrap_or(false)
                            && p.extension()
                                .and_then(|e| e.to_str())
                                .map(|e| e != "nfo")
                                .unwrap_or(false)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        };

        if !existing_for_ep.is_empty() {
            // Check if this is an upgrade replacing a previously imported file.
            // If an older imported grab exists for this episode, this is an
            // upgrade — remove the old file and old torrent, then import the new one.
            //
            // Using `target_series_id` (the routed sibling) rather than
            // `grab.series_id` (the parent) is what makes per-sibling
            // upgrade detection work: `find_imported_for_episode`
            // unions across the legacy grabbed_torrents column and the
            // routes table, so a prior sibling-routed import still
            // surfaces here.
            let old_grabs = grabbed_torrents::find_imported_for_episode(
                &state.db,
                target_series_id,
                ep_num,
            )
            .await
            .unwrap_or_default();

            if old_grabs.is_empty() {
                // No older import record — likely a re-run of the same grab. Skip.
                continue;
            }

            // Remove old file(s) and their NFOs to make way for the upgrade.
            // unlinks are wrapped in tokio::fs so a slow filesystem doesn't
            // stall the runtime during the upgrade path.
            for old_file in &existing_for_ep {
                if let Err(e) = tokio::fs::remove_file(old_file).await {
                    logger::error(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!("Failed to remove old file for upgrade: {}", old_file.display()),
                        &e.to_string(),
                    )
                    .await;
                }
                // Remove corresponding NFO (old stem may differ from new dest_stem
                // if the episode title changed between grabs).
                if let Some(stem) = old_file.file_stem().and_then(|s| s.to_str()) {
                    let _ = tokio::fs::remove_file(ctx.season_dir.join(format!("{}.nfo", stem))).await;
                }
            }

            logger::info(
                &state.db,
                LogCategory::PostProcess,
                &format!(
                    "Replacing S{:02}E{:02} of '{}' with upgraded release",
                    season, ep_num, ctx.series.title
                ),
                &format!("old_grabs={}", old_grabs.len()),
            )
            .await;

            // Clean up old torrents from qBittorrent and mark old grabs as
            // replaced. Reuse the `qbit` binding cloned at the top of this
            // function instead of re-taking `state.qbit.read()` each
            // iteration — under a big upgrade with many old grabs the
            // per-iteration lock acquire was serializing against any other
            // task touching `state.qbit`.
            for old_grab in &old_grabs {
                if !old_grab.hash.is_empty() {
                    let _ = qbit.delete_torrent(&old_grab.hash, true).await;
                }
                let _ = grabbed_torrents::mark_removed(&state.db, old_grab.id).await;
            }
        }

        match do_file_op(&cfg.post_processing_mode, &src, &dest_video).await {
            Ok(()) => {
                let _ = nfo::write_episode_nfo(
                    &dest_nfo,
                    &ctx.series_title,
                    season,
                    ep_num,
                    &ep_title,
                    &aired,
                    ctx.runtime_minutes,
                )
                .await;
                imported_count += 1;
                touched_series.insert(target_series_id);
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!(
                        "Imported S{:02}E{:02} of '{}'",
                        season, ep_num, ctx.series.title
                    ),
                    &format!(
                        "mode={} dest={}",
                        cfg.post_processing_mode,
                        dest_video.display()
                    ),
                )
                .await;

                // Post-download re-classification (Layers 5 + 6). Runs ffprobe
                // on the landed file and walks the series directory for BD
                // artifacts, then upserts episode_quality_tags.
                // Rows with manual_override = 1 are left alone by the DB
                // helpers so user tags stick.
                let series_root = Path::new(&cfg.media_root).join(&ctx.folder_name);
                // Snapshot loaded once per series in `load_series_import_ctx`;
                // see `SeriesImportCtx::existing_tags` for why refreshing
                // per-file is unnecessary.
                let existing_row = ctx.existing_tags.get(&ep_num);
                let pre_source = existing_row.map(|t| t.source.clone()).unwrap_or_default();
                let row_exists = existing_row.is_some();
                let post = source::classify_post_download(
                    &state.db,
                    &dest_video,
                    Some(&series_root),
                    &grab.torrent_name,
                    Some(SeriesContext {
                        status: &ctx.series.status,
                        season_year: ctx.series.season_year,
                        end_year: ctx.series.end_year,
                    }),
                    grab.is_batch,
                )
                .await;
                // Batch grabs often arrive here with no pre-existing tag
                // row: the "Grab batch" dropdown and interactive batch
                // paths skip `episode_tags::record_grab` because they
                // don't know which episodes are in the pack until
                // post-processing parses the filenames. `update_classification`
                // is UPDATE-only, so in that case it would silently
                // affect 0 rows and the episode stays UNKNOWN in the
                // UI despite the classifier correctly identifying it.
                // Branch on row existence: UPSERT via `record_grab`
                // for the no-row case (same pattern
                // `scan_library_for_unclassified` uses for externally
                // imported files), UPDATE in-place via
                // `update_classification` otherwise.
                let persist_result = if row_exists {
                    episode_tags::update_classification(
                        &state.db,
                        target_series_id,
                        ep_num,
                        &post,
                    )
                    .await
                } else {
                    episode_tags::record_grab(
                        &state.db,
                        target_series_id,
                        ep_num,
                        &post,
                        &grab.torrent_name,
                        "",
                        file.size,
                        grab.is_batch,
                    )
                    .await
                    .map(|_| ())
                };
                if let Err(e) = persist_result {
                    logger::warn(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!(
                            "Post-download tag persist failed for S{:02}E{:02}",
                            season, ep_num
                        ),
                        &e.to_string(),
                    )
                    .await;
                } else {
                    logger::debug(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!(
                            "Post-download classify S{:02}E{:02}: {} (conf={:.2})",
                            season,
                            ep_num,
                            post.label(),
                            post.confidence
                        ),
                        &format!(
                            "pre={}, post={}, row_existed={}",
                            pre_source,
                            post.source.as_str(),
                            row_exists
                        ),
                    )
                    .await;
                    // If the post-download classifier flipped into needs_review,
                    // surface at INFO so the user can find it in the review list.
                    if post.needs_review {
                        logger::info(
                            &state.db,
                            LogCategory::PostProcess,
                            &format!(
                                "Needs review: {} S{:02}E{:02}",
                                ctx.series.title, season, ep_num
                            ),
                            &format!(
                                "post-download classification {} flagged for review",
                                post.label()
                            ),
                        )
                        .await;
                    }
                }

                let dest_basename = dest_video
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(filename_only)
                    .to_string();
                imported_eps_by_series
                    .entry(target_series_id)
                    .or_default()
                    .push((ep_num, file.size, dest_basename));
            }
            Err(e) => {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("File op failed for '{}'", filename_only),
                    &e.to_string(),
                )
                .await;
            }
        }
    }

    if imported_count == 0 {
        return Ok(false);
    }

    // Series-level artifacts (tvshow.nfo + poster) run once per unique
    // series actually touched, not once total. A multi-series routed
    // batch now maintains the correct per-sibling artifacts instead of
    // dumping everything into the parent's folder.
    //
    // Always (re)write tvshow.nfo so Jellyfin picks up refreshed
    // metadata (status flips from RELEASING to FINISHED, plot updates,
    // newly indexed genres). The previous "write once if missing"
    // behavior meant any NFO written before metadata enrichment shipped
    // never got upgraded. The file is small and the write is local;
    // rewriting on every import run is cheap.
    for series_id in &touched_series {
        let Some(ctx) = series_ctx_cache.get(series_id) else {
            continue;
        };
        let series_root = Path::new(&cfg.media_root).join(&ctx.folder_name);

        // Artwork copies run before NFO writes so the NFO's `<art>`
        // block can reference only the files that actually landed on
        // disk. A hard-coded `<banner>banner.jpg</banner>` tag in
        // tvshow.nfo is worse than useless when banner.jpg doesn't
        // exist — Jellyfin logs a missing-file error per scan and the
        // external-scrape fallback still fires for the empty slot.
        //
        // Series-level cover also feeds the season-level folder.jpg
        // slot, so we dispatch both dests in one `copy_poster` call;
        // the blob is read into memory once and fanned out to both
        // paths under a single `spawn_blocking` (see `copy_artwork`).
        let poster_dest = series_root.join("poster.jpg");
        let season_poster_dest = ctx.season_dir.join("folder.jpg");
        let banner_dest = series_root.join("banner.jpg");

        let cover_source = ctx.cached_detail.as_ref().map(|d| d.cover_url.as_str());
        let banner_source = ctx.cached_detail.as_ref().map(|d| d.banner_url.as_str());

        let poster_results = copy_poster(
            &state.db,
            ctx.series.id,
            cover_source,
            &[&poster_dest, &season_poster_dest],
        )
        .await;
        let has_poster = poster_results.first().copied().unwrap_or(false);
        let has_folder_poster = poster_results.get(1).copied().unwrap_or(false);

        let banner_results =
            copy_banner(&state.db, ctx.series.id, banner_source, &[&banner_dest]).await;
        let has_banner = banner_results.first().copied().unwrap_or(false);

        // Always (re)write tvshow.nfo + season.nfo so refreshed
        // AniList metadata (status flips, plot updates, new genres)
        // propagates. The `<art>` blocks are gated on what landed
        // above so a missing banner doesn't leave a dangling
        // reference in the NFO.
        let series_nfo = series_root.join("tvshow.nfo");
        let _ = nfo::write_series_nfo(
            &series_nfo,
            &ctx.series,
            ctx.cached_detail.as_ref(),
            &cfg.title_language,
            has_poster,
            has_banner,
        )
        .await;

        let season_nfo = ctx.season_dir.join("season.nfo");
        let _ = nfo::write_season_nfo(
            &season_nfo,
            1,
            &ctx.series,
            ctx.cached_detail.as_ref(),
            &cfg.title_language,
            has_folder_poster,
        )
        .await;
    }

    // Flip episode tag rows from "grabbed" to "completed" per target
    // series. Uses the accumulator populated during the per-file loop
    // rather than `grab.episode_numbers` so three cases are handled
    // uniformly:
    //   - legacy single-episode grabs (ep list populated at grab time),
    //   - bare batch grabs where `grab.episode_numbers` is empty and
    //     the real list only exists on the landed filenames,
    //   - Phase 2 routed batches where files are split across sibling
    //     series (each sibling gets its own call keyed by
    //     `target_series_id`).
    //
    // Two flips happen per episode: the quality-tag row (via
    // `mark_completed`) and the newest 'grabbed' history row (via
    // `mark_grab_history_completed`). The history flip stamps in the
    // per-episode post-processed file name (Sonarr-style renamed
    // basename) and — only for non-batch rows — refines size_bytes to
    // the imported file's real size. Batch rows keep the whole-
    // torrent total so the episode detail modal can report "this
    // episode came from an X GiB batch".
    for (series_id, episodes) in &imported_eps_by_series {
        let ep_nums: Vec<i32> = episodes.iter().map(|(n, _, _)| *n).collect();
        let _ = episode_tags::mark_completed(&state.db, *series_id, &ep_nums).await;
        for (ep_num, file_size, file_name) in episodes {
            let _ = episode_tags::mark_grab_history_completed(
                &state.db,
                *series_id,
                *ep_num,
                file_name,
                *file_size,
            )
            .await;
        }
    }

    Ok(true)
}

/// Run one post-processing cycle. Called by the background task every minute.
pub async fn run_once(state: &AppState) {
    let _guard = match POST_PROC_LOCK.try_lock() {
        Ok(g) => g,
        Err(_) => return, // already running
    };

    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        _ => return,
    };

    // When post-processing is disabled, we still want the UI checkmark to
    // flip as soon as qBit reports the torrent complete — otherwise the
    // row is stuck showing a progress bar forever even though the download
    // finished. Run a lightweight sweep that advances state on
    // episode_quality_tags and grabbed_torrents without moving any files.
    //
    // media_root being empty implies post-processing is unusable even if
    // the toggle is on, so treat it the same as the disabled case.
    if !cfg.post_processing_enabled || cfg.media_root.is_empty() {
        let _ = advance_state_without_import(state).await;
        return;
    }

    let pending = match grabbed_torrents::get_all_pending(&state.db).await {
        Ok(p) => p,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::PostProcess,
                "Failed to query pending grabs",
                &e.to_string(),
            )
            .await;
            return;
        }
    };

    if pending.is_empty() {
        return;
    }

    let qbit = match state.qbit.read().await.clone() {
        Some(c) => c,
        None => return,
    };

    let torrents = match qbit.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::PostProcess,
                "Failed to query qBittorrent",
                &e.to_string(),
            )
            .await;
            return;
        }
    };

    // Build lookup maps by hash and name for all torrents.
    let all_by_hash: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();

    let all_by_name: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .map(|t| (t.name.to_lowercase(), t))
        .collect();

    let mut any_imported = false;

    for grab in &pending {
        // Match grab to a qBit torrent.
        let matched = if !grab.hash.is_empty() {
            all_by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            all_by_name.get(&grab.torrent_name.to_lowercase()).copied()
        };

        let Some(torrent) = matched else {
            // Torrent not found in qBittorrent. If the grab is old enough
            // (> 60 seconds), the user likely deleted it — mark as
            // removed. The grace window used to be 5 minutes to cover
            // qBit restarts, but in practice the `all_torrents` call
            // would fail outright during a restart (we'd not even reach
            // this branch with a valid torrent list), so the long grace
            // window just delayed reconciliation of manual qBit deletes
            // for no safety gain. A minute is enough slack for a slow
            // first-poll after an add-torrent RPC, short enough that
            // "deleted ep 9 in qBit and it still shows pending" becomes
            // "shows cancelled within a minute."
            if grab_is_stale(&grab.grabbed_at, 60) {
                logger::warn(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Torrent removed from qBittorrent: '{}'", grab.torrent_name),
                    "Marking as removed (not found in client)",
                )
                .await;
                let _ = grabbed_torrents::mark_removed(&state.db, grab.id).await;
                let _ = episode_tags::clear_tags_for_removal(
                    &state.db,
                    grab.series_id,
                    &grab.episode_numbers,
                )
                .await;
            }
            continue;
        };

        // Detect failed/error torrents and mark them.
        if is_errored(&torrent.state) {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!("Torrent in error state: '{}'", grab.torrent_name),
                &format!("qbit_state={}", torrent.state),
            )
            .await;
            let _ = grabbed_torrents::mark_failed(&state.db, grab.id).await;
            continue;
        }

        if !is_complete(&torrent.state) {
            continue;
        }

        // Stamp qBit's output path on the grab row before we move/
        // hardlink the file into the library. Done BEFORE import so
        // that even if import errors out mid-way, the UI still has a
        // record of where qBit left the file.
        let qbit_path = if !torrent.content_path.is_empty() {
            torrent.content_path.clone()
        } else {
            torrent.save_path.clone()
        };
        let _ = grabbed_torrents::stamp_qbit_content_path(
            &state.db,
            grab.id,
            &qbit_path,
        )
        .await;

        match import_torrent(state, &cfg, grab, &torrent.hash, &torrent.save_path).await {
            Ok(true) => {
                any_imported = true;
                let _ = grabbed_torrents::mark_imported(&state.db, grab.id).await;
                // #27 — log every successful import so there's a trail
                // from grab → complete in System → Logs. Before this,
                // the only log a successful grab produced was the grab
                // itself and maybe the Jellyfin refresh at the end.
                // Operators who went looking for "did this episode
                // land?" had to check the library row or disk.
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Imported '{}'", grab.torrent_name),
                    &format!("series_id={} episodes={:?}", grab.series_id, grab.episode_numbers),
                )
                .await;
                // Episode tag "grabbed → completed" flips happen inside
                // `import_torrent` itself so a Phase 2 routed batch can
                // mark each sibling's tags under the sibling's own
                // series_id + per-route episode numbers. Legacy grabs
                // still get the same flip as before via the
                // `routes.is_empty()` fallback there.
            }
            Ok(false) => {
                // Torrent complete but no video files yet — leave as pending.
                // The caller (qBit) might still be finalizing the files,
                // or the torrent could be all samples/.nfo (pathological).
                // We intentionally don't escalate here — next post-proc
                // tick retries. A stuck-forever failsafe would need a
                // "pending too long" timer; covered by the plan's
                // future work, not this commit.
            }
            Err(e) => {
                logger::error(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Import failed for '{}'", grab.torrent_name),
                    &e,
                )
                .await;
                let _ = grabbed_torrents::mark_failed(&state.db, grab.id).await;
            }
        }
    }

    if any_imported
        && let Some(jellyfin) = state.jellyfin.read().await.as_ref() {
            if let Err(e) = jellyfin.refresh_library().await {
                logger::warn(
                    &state.db,
                    LogCategory::PostProcess,
                    "Jellyfin refresh failed after import",
                    &e,
                )
                .await;
            } else {
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    "Triggered Jellyfin library refresh",
                    "",
                )
                .await;
            }
        }
}

/// Lightweight variant of `run_once` used when post-processing is
/// disabled (or media_root is unset). Advances a qBit-complete pending
/// grab's state on `grabbed_torrents` and `episode_quality_tags` so the
/// UI checkmark can flip, without moving any files or writing an NFO.
///
/// This exists because the UI otherwise has no way to know a torrent
/// finished downloading when post-processing is off — the checkmark
/// watches `episode_quality_tags.state = 'completed'`, which only gets
/// set by the full import pass. Operators who run Ryokan alongside a
/// separate move/rename tool (or who just leave files in the qBit
/// completed dir) would see every row stuck at "Importing…" forever.
async fn advance_state_without_import(state: &AppState) -> Result<(), ()> {
    let pending = grabbed_torrents::get_all_pending(&state.db)
        .await
        .map_err(|_| ())?;
    if pending.is_empty() {
        return Ok(());
    }

    let qbit = match state.qbit.read().await.clone() {
        Some(c) => c,
        None => return Ok(()),
    };

    let torrents = qbit.get_torrents().await.map_err(|_| ())?;
    let by_hash: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();
    let by_name: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .map(|t| (t.name.to_lowercase(), t))
        .collect();

    for grab in &pending {
        let matched = if !grab.hash.is_empty() {
            by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            by_name.get(&grab.torrent_name.to_lowercase()).copied()
        };
        let Some(torrent) = matched else { continue };

        if !is_complete(&torrent.state) {
            continue;
        }

        // Stamp the qBit-side path for the episode detail modal. Prefer
        // content_path (qBit ≥ 2.6.1 — the actual file or container
        // folder) and fall back to save_path for older qBit builds.
        let qbit_path = if !torrent.content_path.is_empty() {
            torrent.content_path.clone()
        } else {
            torrent.save_path.clone()
        };
        let _ = grabbed_torrents::stamp_qbit_content_path(
            &state.db,
            grab.id,
            &qbit_path,
        )
        .await;

        // Mark the grab row as finalized so we stop polling it and the
        // UI stops treating it as in-flight. Use `mark_completed_no_import`
        // rather than `mark_imported` — we never moved a file, so
        // `imported_at` stays NULL and future reports keyed on that
        // column don't see a false positive for this grab. Then flip
        // the episode tag(s) to 'completed' so the checkmark appears
        // on the next poll. Phase-2 sibling routes get the per-series
        // treatment too.
        let _ = grabbed_torrents::mark_completed_no_import(&state.db, grab.id).await;

        let routes = grabbed_torrents::get_series_routes(&state.db, grab.id)
            .await
            .unwrap_or_default();
        if routes.is_empty() {
            let _ = episode_tags::mark_completed(
                &state.db,
                grab.series_id,
                &grab.episode_numbers,
            )
            .await;
        } else {
            for route in &routes {
                let _ = episode_tags::mark_completed(
                    &state.db,
                    route.series_id,
                    &route.episode_numbers,
                )
                .await;
            }
        }
    }

    Ok(())
}

/// Summary of one run of [`scan_library_for_unclassified`]. Used by the
/// handler to build a user-facing report and by logging to record the
/// effect of the background task.
#[derive(Debug, Default)]
pub struct LibraryClassifyReport {
    pub series_scanned: usize,
    pub files_scanned: usize,
    pub files_classified: usize,
    pub files_needing_review: usize,
}

/// One file queued up by the lock-held enumeration phase, carried over
/// into the unlocked classification phase of `scan_library_for_unclassified`.
struct PendingClassification {
    series_id: i64,
    series_status: String,
    series_season_year: Option<i32>,
    series_end_year: Option<i32>,
    series_root: std::path::PathBuf,
    file_path: std::path::PathBuf,
    episode_number: i32,
    /// Sanitized on-disk filename. Used as a fallback title for L1
    /// classification when `original_torrent_name` is empty (externally
    /// imported files that Ryokan never grabbed).
    title: String,
    /// Original torrent name from `grabbed_torrents`, if a Ryokan grab
    /// exists for this episode. Passed to `classify_post_download` as
    /// the L1 input when non-empty: the post-processing import step
    /// sanitizes release tags out of filenames (e.g. "[SubsPlease] Foo
    /// (1080p) [Batch]" becomes "Foo - S01E05 - Title.mkv"), so
    /// classifying against the on-disk name yields `rule=empty` and
    /// the episode shows as UNKNOWN forever. Looking up the original
    /// grab lets us classify against the unsanitized release name.
    original_torrent_name: String,
    /// True when an `episode_quality_tags` row already exists for this
    /// (series, episode) pair. Determines whether the persist step goes
    /// through `update_classification` (UPDATE) or `record_grab` +
    /// `mark_completed` (INSERT upsert + state flip).
    row_exists: bool,
}

/// Walk every tracked series and (re-)classify on-disk video files that
/// don't yet have a confident classification row. This is the Phase 2
/// "library scan path" — it catches files that were imported outside of
/// Ryokan's own grab pipeline (pre-existing rips, manual drops, migrations
/// from another PVR), AND it self-heals rows that the classifier first
/// saw as `Unknown` (low-confidence filename-only result) now that the
/// file is actually on disk and the full ffprobe/dir-walk pipeline can
/// run against it.
///
/// Skips files whose tag row is:
///  - already classified with a confident non-empty, non-"unknown" source,
///  - flagged `needs_review = 1` (user should resolve via the review
///    queue — we don't want to race against them),
///  - flagged `manual_override = 1` (user pinned the classification).
///
/// **Locking:** the enumeration phase (config read, `scan_series_folder`,
/// existing-tag lookup, disk-existence filter) holds `POST_PROC_LOCK` so
/// the work list is a consistent snapshot that can't be invalidated by a
/// parallel `run_once`. Once the list is built, the lock is released
/// before we shell out to ffprobe via `classify_post_download` — those
/// calls can take hundreds of ms per file and would otherwise block the
/// 1-minute `process_completed_downloads` background loop for the full
/// duration of a large scan. The DB writes in the second phase rely on
/// SQLite's normal write serialization; the worst-case race (a real
/// import landing between our classify and persist steps on the same
/// episode) leaves a single row briefly stale and self-heals on the
/// next scan.
pub async fn scan_library_for_unclassified(state: &AppState) -> LibraryClassifyReport {
    let mut report = LibraryClassifyReport::default();

    let pending: Vec<PendingClassification> = {
        // Lock scope: enumerate only. Dropped before the classify loop.
        let _guard = POST_PROC_LOCK.lock().await;

        let cfg = match config::get_config(&state.db).await.ok().flatten() {
            Some(c) => c,
            None => return report,
        };
        if cfg.media_root.is_empty() {
            return report;
        }

        let tracked = series::get_all(&state.db).await.unwrap_or_default();
        let mut pending = Vec::new();

        for row in &tracked {
            if row.folder_name.is_empty() {
                continue;
            }
            let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name);
            if disk_files.is_empty() {
                continue;
            }
            report.series_scanned += 1;

            let existing = episode_tags::get_for_series(&state.db, row.id)
                .await
                .unwrap_or_default();

            // One bulk fetch of imported grabs for this series, used
            // by every file in the inner loop below to look up the
            // unsanitized torrent name. Replaces a pair of per-file
            // queries (find_imported_for_episode + a fallback
            // most_recent_imported_torrent_name_for_series) that ran
            // inside the held POST_PROC_LOCK — that was ~4800
            // round-trips per pass on a 100-series, 24-ep library.
            let imported_grabs =
                grabbed_torrents::imported_grabs_for_series(&state.db, row.id)
                    .await
                    .unwrap_or_default();
            // Per-episode lookup; first occurrence (DESC by grabbed_at)
            // wins, so each episode maps to its most recent grab. This
            // matches the prior find_imported_for_episode .next()
            // semantics on a DESC-sorted result set.
            let mut grab_name_for_episode: HashMap<i32, String> = HashMap::new();
            for (name, eps) in &imported_grabs {
                for ep in eps {
                    grab_name_for_episode
                        .entry(*ep)
                        .or_insert_with(|| name.clone());
                }
            }
            let most_recent_grab_name: Option<&str> =
                imported_grabs.first().map(|(n, _)| n.as_str());

            let series_root = Path::new(&cfg.media_root).join(&row.folder_name);

            for file in &disk_files {
                report.files_scanned += 1;

                // Decide whether to (re-)classify this file.
                //
                // Skip when:
                //  - `manual_override = 1`: the user explicitly pinned
                //    this classification — never touch.
                //  - a tag exists with a non-empty source that isn't
                //    "unknown". Already confidently classified. If such
                //    a row is *also* flagged `needs_review`, we respect
                //    that pending user decision and leave it alone.
                //
                // Pick up when:
                //  - no tag exists yet (externally-imported file).
                //  - the source column is empty (pre-Phase-1b rows).
                //  - the source is literally "unknown" (case-insensitive)
                //    — the classifier couldn't decide at grab time but
                //    the file exists now, so retry with the full
                //    ffprobe/dir-walk pipeline. This is the background
                //    self-healing path for rows that started as Unknown.
                //
                // Note: unknown rows are retried **even when
                // `needs_review = 1`**. That used to trap stale bad
                // classifications — a row that was classified
                // against the sanitized post-import filename before
                // the original-torrent-name lookup landed produces
                // `rule=empty` and gets flagged for review, and the
                // old guard then refused to retry it even after the
                // bug was fixed. An unknown needs-review row carries
                // no useful user decision to stomp on (the user
                // resolves via `set_manual_override`, which sets
                // `manual_override = 1` and clears `needs_review`),
                // so retrying is safe.
                let tag = existing.get(&file.episode_number);
                if let Some(t) = tag {
                    if t.manual_override {
                        continue;
                    }
                    let src = t.source.trim();
                    let is_unknown = src.is_empty() || src.eq_ignore_ascii_case("unknown");
                    if !is_unknown {
                        continue;
                    }
                }

                // Reconstruct the absolute path so ffprobe can read the file.
                let file_path = series_root.join(&file.filename);
                if !file_path.exists() {
                    continue;
                }

                // Use the sanitized on-disk filename as the fallback
                // title. For Ryokan-grabbed files we'll override this
                // with the original torrent name in the unlocked
                // classify phase below so L1 sees the full release tag
                // set instead of the tags-stripped post-import name.
                let title = Path::new(&file.filename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file.filename)
                    .to_string();

                // Look up the grab that covers this episode so we can
                // classify against the unsanitized torrent name. The
                // pre-built `grab_name_for_episode` map and
                // `most_recent_grab_name` fallback come from the
                // single bulk fetch above — both lookups are now
                // pure-Rust and don't touch the DB inside the held
                // POST_PROC_LOCK. The fallback exists for old batch
                // grabs recorded with `episode_numbers = []` before
                // the grab-time episode-range fix; for a single-series
                // batch the most-recent grab is by definition the one
                // these files came from.
                let original_torrent_name = grab_name_for_episode
                    .get(&file.episode_number)
                    .cloned()
                    .or_else(|| most_recent_grab_name.map(|s| s.to_string()))
                    .unwrap_or_default();

                pending.push(PendingClassification {
                    series_id: row.id,
                    series_status: row.status.clone(),
                    series_season_year: row.season_year,
                    series_end_year: row.end_year,
                    series_root: series_root.clone(),
                    file_path,
                    episode_number: file.episode_number,
                    title,
                    original_torrent_name,
                    row_exists: tag.is_some(),
                });
            }
        }

        pending
    };

    // Unlocked classification + persist phase. ffprobe shell-outs are the
    // slow part; doing them here instead of under the lock lets
    // `process_completed_downloads` keep running in parallel.
    for item in pending {
        // Prefer the original torrent name from `grabbed_torrents`
        // (carries full release tags like "[SubsPlease] Foo (01-12)
        // [BD 1080p]"), fall back to the sanitized on-disk filename
        // for externally-imported files that Ryokan never grabbed.
        // This is the difference between L1 producing useful
        // evidence and producing `rule=empty` on a release that
        // landed via post-processing's rename step.
        let classify_title: &str = if !item.original_torrent_name.is_empty() {
            &item.original_torrent_name
        } else {
            &item.title
        };
        let is_batch = crate::models::grabbed_torrents::get_is_batch_by_name(
            &state.db,
            item.series_id,
            classify_title,
        )
        .await
        .unwrap_or(false);
        let result = source::classify_post_download(
            &state.db,
            &item.file_path,
            Some(&item.series_root),
            classify_title,
            Some(SeriesContext {
                status: &item.series_status,
                season_year: item.series_season_year,
                end_year: item.series_end_year,
            }),
            is_batch,
        )
        .await;

        // The row may not exist yet for externally-imported files, so
        // we can't rely on `update_classification` alone (it's an
        // UPDATE, not an UPSERT). Use `record_grab` with synthetic
        // release metadata to insert-or-upsert, then flip state to
        // 'completed' since the file is already on disk.
        if !item.row_exists {
            // Best-effort single stat — failure just leaves size at 0.
            let file_size = tokio::fs::metadata(&item.file_path)
                .await
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            // Externally-imported file — we're creating both the quality
            // tag and the grab history row from thin air. `is_batch` is
            // looked up above from the optional matching grabbed_torrents
            // row; falls back to false for truly external files.
            let _ = episode_tags::record_grab(
                &state.db,
                item.series_id,
                item.episode_number,
                &result,
                classify_title,
                "",
                file_size,
                is_batch,
            )
            .await;
            let _ = episode_tags::mark_completed(
                &state.db,
                item.series_id,
                &[item.episode_number],
            )
            .await;
            // Flip the fresh grab history row to 'completed' and stamp
            // in the on-disk file basename for the episode detail modal.
            let imported_basename = item
                .file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(classify_title)
                .to_string();
            let _ = episode_tags::mark_grab_history_completed(
                &state.db,
                item.series_id,
                item.episode_number,
                &imported_basename,
                file_size,
            )
            .await;
        } else {
            let _ = episode_tags::update_classification(
                &state.db,
                item.series_id,
                item.episode_number,
                &result,
            )
            .await;
        }

        report.files_classified += 1;
        if result.needs_review {
            report.files_needing_review += 1;
        }
    }

    logger::info(
        &state.db,
        LogCategory::PostProcess,
        "Library classify scan finished",
        &format!(
            "series={}, files_scanned={}, classified={}, needs_review={}",
            report.series_scanned,
            report.files_scanned,
            report.files_classified,
            report.files_needing_review
        ),
    )
    .await;

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #30 fallback offset selection ─────────────────────────────────

    #[test]
    fn fallback_offset_absolute_jjk_s3_subsplease() {
        // Motivating case: [SubsPlease] Jujutsu Kaisen - 56 → S3 E9.
        // S1 (24) + S2 (23) = 47 prior-cour episodes. Parsed 56 > 47,
        // so the filename is absolute-numbered and offset = 47.
        assert_eq!(fallback_ep_offset(56, 47), 47);
    }

    #[test]
    fn fallback_offset_relative_release_within_cour() {
        // Erai-raws / cour-specific releases number from 1: raw 9 is
        // relative, 9 ≤ 47, offset = 0. Subtracting zero leaves E9.
        assert_eq!(fallback_ep_offset(9, 47), 0);
    }

    #[test]
    fn fallback_offset_zero_for_first_season_entry() {
        // First-season entry has no prior cours (cumulative = 0), so
        // the legacy behavior is preserved regardless of parsed number.
        assert_eq!(fallback_ep_offset(56, 0), 0);
        assert_eq!(fallback_ep_offset(1, 0), 0);
    }

    #[test]
    fn fallback_offset_mis_grabbed_prior_cour_number() {
        // Pathological: parsed 25 (S2 E1) somehow downloaded into the
        // S3 series folder. 25 ≤ 47 → offset = 0, file lands as E25 of
        // S3. S3 doesn't have E25 so the subsequent rename step fails
        // — the right loud failure rather than silently translating to
        // a plausible-looking E-22.
        assert_eq!(fallback_ep_offset(25, 47), 0);
    }

    #[test]
    fn fallback_offset_equal_to_cumulative() {
        // Boundary: raw exactly equals cumulative. That value would be
        // ambiguous (last episode of the prior cour, or legitimate
        // relative E47 of a show with 47+ episodes). Without more
        // signal we keep it as relative (offset 0) — the alternative
        // (offset = cumulative) would silently map legitimate E47
        // releases of a 48-episode show to E0.
        assert_eq!(fallback_ep_offset(47, 47), 0);
    }
}
