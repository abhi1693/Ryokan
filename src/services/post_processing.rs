use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::models::log::LogCategory;
use crate::models::{artwork_cache, config, grabbed_torrents, local_metadata, series};
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
fn do_file_op(mode: &str, src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    match mode {
        "move" => {
            if std::fs::rename(src, dst).is_err() {
                std::fs::copy(src, dst)?;
                let _ = std::fs::remove_file(src);
            }
            Ok(())
        }
        "copy" => {
            std::fs::copy(src, dst)?;
            Ok(())
        }
        _ => {
            // "hardlink" (default): hardlink preferred, copy on failure (cross-fs).
            if std::fs::hard_link(src, dst).is_err() {
                std::fs::copy(src, dst)?;
            }
            Ok(())
        }
    }
}

/// Copy the cached series poster to `dest` (always written as JPEG regardless
/// of original extension — Jellyfin accepts it).
async fn copy_poster(db: &sqlx::SqlitePool, series_id: i64, dest: &Path) {
    let cache_key = format!("series-{}-cover", series_id);
    let entry = match artwork_cache::get(db, &cache_key).await {
        Ok(Some(e)) => e,
        _ => return,
    };
    let _ = std::fs::copy(&entry.local_path, dest);
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

    if series.folder_name.is_empty() {
        return Err(format!(
            "series '{}' has no folder configured",
            series.title
        ));
    }

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

    let series_title = nfo::best_title(&series);
    let season = 1_i32;

    let season_dir = Path::new(&cfg.media_root)
        .join(&series.folder_name)
        .join(format!("Season {:02}", season));

    std::fs::create_dir_all(&season_dir)
        .map_err(|e| format!("create season dir: {}", e))?;

    let mut imported_count = 0_usize;

    for file in &video_files {
        // Full source path on disk (qBit's save_path + relative file name).
        let src: PathBuf = Path::new(torrent_save_path).join(&file.name);

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

        if dest_video.exists() {
            // Already imported (e.g. re-run after partial failure). Skip.
            continue;
        }

        match do_file_op(&cfg.post_processing_mode, &src, &dest_video) {
            Ok(()) => {
                let _ = nfo::write_episode_nfo(&dest_nfo, &series_title, season, ep_num, &ep_title, &aired);
                imported_count += 1;
                logger::info(
                    &state.db,
                    LogCategory::PostProcess,
                    &format!("Imported S{:02}E{:02} of '{}'", season, ep_num, series.title),
                    &format!("mode={} dest={}", cfg.post_processing_mode, dest_video.display()),
                )
                .await;
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

    // Write series-level NFO once.
    let series_nfo = Path::new(&cfg.media_root)
        .join(&series.folder_name)
        .join("tvshow.nfo");
    if !series_nfo.exists() {
        let _ = nfo::write_series_nfo(&series_nfo, &series);
    }

    // Copy poster once.
    let poster_dest = Path::new(&cfg.media_root)
        .join(&series.folder_name)
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

    // Build lookup maps for completed torrents.
    let by_hash: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .filter(|t| is_complete(&t.state))
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();

    let by_name: HashMap<String, &crate::services::qbit::Torrent> = torrents
        .iter()
        .filter(|t| is_complete(&t.state))
        .map(|t| (t.name.to_lowercase(), t))
        .collect();

    let mut any_imported = false;

    for grab in &pending {
        // Match grab to a completed qBit torrent.
        let matched = if !grab.hash.is_empty() {
            by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            // Fuzzy name match as fallback for grabs without a recorded hash.
            by_name.get(&grab.torrent_name.to_lowercase()).copied()
        };

        let Some(torrent) = matched else {
            continue;
        };

        match import_torrent(state, &cfg, grab, &torrent.hash, &torrent.save_path).await {
            Ok(true) => {
                any_imported = true;
                let _ = grabbed_torrents::mark_imported(&state.db, grab.id).await;
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
