use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::models::log::LogCategory;
use crate::models::{artwork_cache, config, episode_tags, grabbed_torrents, local_metadata, metadata_cache, series};
use crate::services::source::{self, SeriesContext};
use crate::services::{logger, media, nfo};
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
fn grab_is_stale(grabbed_at: &str, max_age_secs: i64) -> bool {
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
                if std::fs::rename(&src, &dst).is_err() {
                    std::fs::copy(&src, &dst)?;
                    let _ = std::fs::remove_file(&src);
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

/// Copy the cached series poster to `dest` (always written as JPEG regardless
/// of original extension — Jellyfin accepts it). Runs the copy under
/// `spawn_blocking` so a slow/network-backed media root can't stall the
/// runtime while moving a multi-MB poster. A failure here is non-fatal
/// (Jellyfin can still scrape its own poster), but silently swallowing
/// the error left operators with an invisible "no poster in Jellyfin"
/// symptom and nothing in the logs to point at — so every failure path
/// now warns with enough detail to debug it.
async fn copy_poster(db: &sqlx::SqlitePool, series_id: i64, dest: &Path) {
    let cache_key = format!("series-{}-cover", series_id);
    let entry = match artwork_cache::get(db, &cache_key).await {
        Ok(Some(e)) => e,
        _ => return,
    };
    let src = std::path::PathBuf::from(&entry.local_path);
    let dst = dest.to_path_buf();
    let src_display = src.display().to_string();
    let dst_display = dst.display().to_string();
    match tokio::task::spawn_blocking(move || std::fs::copy(src, dst)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!("Failed to copy series poster for series_id={}", series_id),
                &format!("src={}, dst={}, error={}", src_display, dst_display, err),
            )
            .await;
        }
        Err(join_err) => {
            logger::warn(
                db,
                LogCategory::PostProcess,
                &format!("Poster copy task panicked for series_id={}", series_id),
                &format!("src={}, dst={}, error={}", src_display, dst_display, join_err),
            )
            .await;
        }
    }
}

/// Process a single completed torrent. Returns `true` if at least one file was
/// imported, `false` if there was nothing to do yet.
async fn import_torrent(
    state: &AppState,
    cfg: &config::Config,
    grab: &grabbed_torrents::GrabbedTorrent,
    torrent_hash: &str,
    torrent_save_path: &str,
) -> Result<bool, String> {
    let series = series::get_by_id(&state.db, grab.series_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("series {} not found", grab.series_id))?;

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

    let video_files: Vec<_> = files
        .iter()
        .filter(|f| f.progress >= 1.0 && is_video_file(&f.name))
        .collect();

    if video_files.is_empty() {
        return Ok(false);
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

    let series_title = nfo::best_title(&series);
    let season = 1_i32;

    let season_dir = Path::new(&cfg.media_root)
        .join(&folder_name)
        .join(format!("Season {:02}", season));

    {
        let season_dir = season_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&season_dir))
            .await
            .map_err(|e| format!("create season dir join: {}", e))?
            .map_err(|e| format!("create season dir: {}", e))?;
    }

    let mut imported_count = 0_usize;

    // Determine the source base path. If qbit_download_path is configured, use
    // that instead of qBit's internal save_path — this handles Docker path
    // mapping where qBit sees /downloads/ but Ryokan sees a different mount.
    let source_base = if !cfg.qbit_download_path.is_empty() {
        cfg.qbit_download_path.clone()
    } else {
        torrent_save_path.to_string()
    };

    for file in &video_files {
        let src: PathBuf = Path::new(&source_base).join(&file.name);

        let filename_only = Path::new(&file.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&file.name);

        // Parse episode number from the filename.
        let ep_num = media::parse_episode_number(&filename_only.to_lowercase())
            .map(|(_, ep)| ep)
            .or_else(|| {
                // Fall back to the first episode number recorded at grab time.
                grab.episode_numbers.first().copied()
            });

        let Some(ep_num) = ep_num else {
            logger::warn(
                &state.db,
                LogCategory::PostProcess,
                &format!("Could not parse episode number from '{}'", filename_only),
                &format!("series={}", series.title),
            )
            .await;
            continue;
        };

        let ep_title = ep_meta
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

        let aired = ep_meta
            .get(&ep_num)
            .map(|m| m.aired.clone())
            .unwrap_or_default();

        let ext = Path::new(filename_only)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mkv");

        let dest_stem = if ep_title.is_empty() {
            format!(
                "{} - S{:02}E{:02}",
                sanitize_filename(&series_title),
                season,
                ep_num
            )
        } else {
            format!(
                "{} - S{:02}E{:02} - {}",
                sanitize_filename(&series_title),
                season,
                ep_num,
                sanitize_filename(&ep_title)
            )
        };

        let dest_video = season_dir.join(format!("{}.{}", dest_stem, ext));
        let dest_nfo = season_dir.join(format!("{}.nfo", dest_stem));

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
            let season_dir = season_dir.clone();
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
            let old_grabs = grabbed_torrents::find_imported_for_episode(
                &state.db,
                grab.series_id,
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
                    let _ = tokio::fs::remove_file(season_dir.join(format!("{}.nfo", stem))).await;
                }
            }

            logger::info(
                &state.db,
                LogCategory::PostProcess,
                &format!("Replacing S{:02}E{:02} of '{}' with upgraded release", season, ep_num, series.title),
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
                    &series_title,
                    season,
                    ep_num,
                    &ep_title,
                    &aired,
                    runtime_minutes,
                );
                imported_count += 1;
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Imported S{:02}E{:02} of '{}'", season, ep_num, series.title),
                    &format!("mode={} dest={}", cfg.post_processing_mode, dest_video.display()),
                )
                .await;

                // Post-download re-classification (Layers 5 + 6). Runs ffprobe
                // on the landed file and walks the series directory for BD
                // artifacts, then updates episode_quality_tags in place.
                // Rows with manual_override = 1 are left alone by the DB
                // helper so user tags stick.
                let series_root = Path::new(&cfg.media_root).join(&folder_name);
                let pre_source = episode_tags::get_for_series(&state.db, series.id)
                    .await
                    .ok()
                    .and_then(|m| m.get(&ep_num).map(|t| t.source.clone()))
                    .unwrap_or_default();
                let post = source::classify_post_download(
                    &state.db,
                    &dest_video,
                    Some(&series_root),
                    &grab.torrent_name,
                    Some(SeriesContext {
                        status: &series.status,
                        season_year: series.season_year,
                    }),
                )
                .await;
                if let Err(e) = episode_tags::update_classification(
                    &state.db,
                    series.id,
                    ep_num,
                    &post,
                )
                .await
                {
                    logger::warn(
                        &state.db,
                        LogCategory::PostProcess,
                        &format!(
                            "Post-download tag update failed for S{:02}E{:02}",
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
                        &format!("pre={}, post={}", pre_source, post.source.as_str()),
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
                                series.title, season, ep_num
                            ),
                            &format!(
                                "post-download classification {} flagged for review",
                                post.label()
                            ),
                        )
                        .await;
                    }
                }
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

    // Always (re)write series-level NFO so Jellyfin picks up refreshed
    // metadata (status flips from RELEASING to FINISHED, plot updates,
    // newly indexed genres). The previous "write once if missing" behavior
    // meant any NFO written before metadata enrichment shipped never got
    // upgraded. The file is small and the write is local; rewriting on
    // every import run is cheap.
    let series_nfo = Path::new(&cfg.media_root)
        .join(&folder_name)
        .join("tvshow.nfo");
    let _ = nfo::write_series_nfo(&series_nfo, &series, cached_detail.as_ref());

    // Copy poster once.
    let poster_dest = Path::new(&cfg.media_root)
        .join(&folder_name)
        .join("poster.jpg");
    if !poster_dest.exists() {
        copy_poster(&state.db, series.id, &poster_dest).await;
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

    if !cfg.post_processing_enabled || cfg.media_root.is_empty() {
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
            // (> 5 minutes), the user likely deleted it — mark as removed.
            if grab_is_stale(&grab.grabbed_at, 300) {
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

        match import_torrent(state, &cfg, grab, &torrent.hash, &torrent.save_path).await {
            Ok(true) => {
                any_imported = true;
                let _ = grabbed_torrents::mark_imported(&state.db, grab.id).await;
                // Update episode quality tags from "grabbed" to "completed" so the UI
                // shows the quality label instead of a stale progress bar on revisit.
                let _ = episode_tags::mark_completed(
                    &state.db,
                    grab.series_id,
                    &grab.episode_numbers,
                ).await;
            }
            Ok(false) => {
                // Torrent complete but no video files yet — leave as pending.
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

    if any_imported {
        if let Some(jellyfin) = state.jellyfin.read().await.as_ref() {
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
    series_root: std::path::PathBuf,
    file_path: std::path::PathBuf,
    episode_number: i32,
    title: String,
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

            let series_root = Path::new(&cfg.media_root).join(&row.folder_name);

            for file in &disk_files {
                report.files_scanned += 1;

                // Decide whether to (re-)classify this file.
                //
                // Skip when:
                //  - a tag exists with a non-empty source that isn't
                //    "unknown" — already confidently classified.
                //  - the tag is flagged `needs_review` — user should
                //    resolve it manually via the Needs Review queue;
                //    we don't want to silently overwrite their pending
                //    decision with another low-confidence result.
                //  - the tag is `manual_override` — the user explicitly
                //    pinned this classification.
                //
                // Pick up when:
                //  - no tag exists yet (externally-imported file).
                //  - the source column is empty (pre-Phase-1b rows).
                //  - the source is literally "unknown" (case-insensitive)
                //    — the classifier couldn't decide at grab time but
                //    the file exists now, so retry with the full
                //    ffprobe/dir-walk pipeline. This is the background
                //    self-healing path for rows that started as Unknown.
                let tag = existing.get(&file.episode_number);
                if let Some(t) = tag {
                    if t.manual_override || t.needs_review {
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

                // Use the filename itself as the title — we don't have the
                // original torrent name for externally-imported files.
                let title = Path::new(&file.filename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file.filename)
                    .to_string();

                pending.push(PendingClassification {
                    series_id: row.id,
                    series_status: row.status.clone(),
                    series_season_year: row.season_year,
                    series_root: series_root.clone(),
                    file_path,
                    episode_number: file.episode_number,
                    title,
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
        let result = source::classify_post_download(
            &state.db,
            &item.file_path,
            Some(&item.series_root),
            &item.title,
            Some(SeriesContext {
                status: &item.series_status,
                season_year: item.series_season_year,
            }),
        )
        .await;

        // The row may not exist yet for externally-imported files, so
        // we can't rely on `update_classification` alone (it's an
        // UPDATE, not an UPSERT). Use `record_grab` with synthetic
        // release metadata to insert-or-upsert, then flip state to
        // 'completed' since the file is already on disk.
        if !item.row_exists {
            let _ = episode_tags::record_grab(
                &state.db,
                item.series_id,
                item.episode_number,
                &result,
                &item.title,
                "",
            )
            .await;
            let _ = episode_tags::mark_completed(
                &state.db,
                item.series_id,
                &[item.episode_number],
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
