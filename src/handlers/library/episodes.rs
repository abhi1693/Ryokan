//! Episode-file management endpoints.
//!
//! Split out of `handlers::library::mod` for readability — these handlers
//! share the per-episode action surface (delete, cancel, grab history,
//! mark-failed, progress poll, JSON snapshot) and depend only on a small
//! set of resolver + builder helpers in the parent module.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, grabbed_torrents};
use crate::services::{auto_search, logger, media};

use super::pages::build_episodes;
use super::reconcile::{resolve_series_context, resolve_tracked_series};
use super::search::run_auto_search_targets;
use super::{Episode, MarkEpisodeFailedForm};

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/delete-file/{episode_number}",
    tag = "Library",
    summary = "Delete episode file",
    description = "Delete the on-disk media file for a specific episode.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "File deleted", body = serde_json::Value),
        (status = 400, description = "Series not in library or no file found"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn delete_episode_file(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let json_err = |status: axum::http::StatusCode, msg: &str| {
        (
            status,
            Json(serde_json::json!({"ok": false, "message": msg})),
        )
    };

    let (tracked_row, _, _detail) = match resolve_series_context(&state.db, request_id).await {
        Ok(v) => v,
        Err(e) => return json_err(axum::http::StatusCode::BAD_GATEWAY, &e),
    };

    let tracked = match tracked_row {
        Some(t) => t,
        None => return json_err(axum::http::StatusCode::BAD_REQUEST, "Series not in library"),
    };

    let cfg = match config::get_config(&state.db).await.ok().flatten() {
        Some(c) => c,
        None => return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "No config"),
    };

    let files = media::scan_series_folder(&cfg.media_root, &tracked.folder_name).await;
    let target = files.iter().find(|f| f.episode_number == episode_number);

    match target {
        None => json_err(
            axum::http::StatusCode::NOT_FOUND,
            "Episode file not found on disk",
        ),
        Some(file) => {
            let series_dir = std::path::Path::new(&cfg.media_root).join(&tracked.folder_name);
            let full_path = series_dir.join(&file.filename);

            // Canonicalize and verify the resolved path is still inside
            // the configured media root.
            let media_root_canon = match tokio::fs::canonicalize(&cfg.media_root).await {
                Ok(p) => p,
                Err(e) => {
                    return json_err(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("Failed to resolve media root: {}", e),
                    );
                }
            };
            let full_path_canon = match tokio::fs::canonicalize(&full_path).await {
                Ok(p) => p,
                Err(e) => {
                    return json_err(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("Failed to resolve file: {}", e),
                    );
                }
            };
            if !full_path_canon.starts_with(&media_root_canon) {
                logger::warn(
                    &state.db,
                    LogCategory::Library,
                    "Refused to delete file outside media root",
                    &format!(
                        "series_id={}, requested={}, resolved={}, media_root={}",
                        tracked.id,
                        full_path.display(),
                        full_path_canon.display(),
                        media_root_canon.display()
                    ),
                )
                .await;
                return json_err(
                    axum::http::StatusCode::BAD_REQUEST,
                    "File resolves outside media root",
                );
            }

            if let Err(e) = tokio::fs::remove_file(&full_path_canon).await {
                return json_err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to delete file: {}", e),
                );
            }

            let nfo_path = full_path_canon.with_extension("nfo");
            if let Ok(nfo_canon) = tokio::fs::canonicalize(&nfo_path).await
                && nfo_canon.starts_with(&media_root_canon)
            {
                let _ = tokio::fs::remove_file(&nfo_canon).await;
            }

            let _ = episode_tags::clear_episode_tag(&state.db, tracked.id, episode_number).await;

            let imported_grabs =
                grabbed_torrents::find_imported_for_episode(&state.db, tracked.id, episode_number)
                    .await
                    .unwrap_or_default();
            let mut qbit_removed: Vec<String> = Vec::new();
            if !imported_grabs.is_empty() {
                let client = state.download_client.read().await.as_ref().cloned();
                if let Some(client) = client {
                    for grab in &imported_grabs {
                        if grab.is_batch {
                            continue;
                        }
                        if grab.hash.is_empty() {
                            continue;
                        }
                        // Issue #28 PR C — skip the client-side
                        // delete for grabs from a PT indexer with
                        // seed rules in effect; the client owns
                        // when seeding ends. The grab row still
                        // gets `mark_removed` so the upgrade sweep
                        // doesn't re-grab.
                        if grabbed_torrents::respects_seed_rules(&state.db, &grab.hash).await {
                            logger::info(
                                &state.db,
                                LogCategory::QBit,
                                &format!(
                                    "Skipping client delete for {} (respect_seed_rules); client will stop on its own ratio policy",
                                    grab.torrent_name
                                ),
                                &grab.hash,
                            )
                            .await;
                            let _ = grabbed_torrents::mark_removed(&state.db, grab.id).await;
                            continue;
                        }
                        match client.delete(&grab.hash, true).await {
                            Ok(()) => {
                                qbit_removed.push(grab.torrent_name.clone());
                                let _ = grabbed_torrents::mark_removed(&state.db, grab.id).await;
                            }
                            Err(e) => {
                                logger::debug(
                                    &state.db,
                                    LogCategory::QBit,
                                    &format!(
                                        "Download client delete failed for episode {} torrent '{}' — continuing with file delete",
                                        episode_number, grab.torrent_name
                                    ),
                                    &e,
                                )
                                .await;
                            }
                        }
                    }
                }
            }

            logger::info(
                &state.db,
                LogCategory::Library,
                &format!("Deleted episode {} file: {}", episode_number, file.filename),
                &format!(
                    "series_id={}, path={}, qbit_removed={}",
                    tracked.id,
                    full_path_canon.display(),
                    qbit_removed.len()
                ),
            )
            .await;

            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "deleted": file.filename,
                    "qbit_removed": qbit_removed,
                })),
            )
        }
    }
}

/// Cancel an in-flight grab for an episode: remove the torrent from
/// qBittorrent (with its partial/complete data), mark the grab row as
/// 'removed', and clear the episode's quality tag so it returns to the
/// missing state.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/cancel-pending/{episode_number}",
    tag = "Library",
    summary = "Cancel pending episode grab",
    description = "Remove the in-flight torrent from qBittorrent, mark the grab as removed, and clear the episode's quality tag. Does not trigger a re-search.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Pending grab cancelled", body = serde_json::Value),
        (status = 400, description = "Series not in library"),
        (status = 404, description = "No pending grab found for this episode"),
    ),
)]
pub async fn cancel_pending_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let json_err = |status: axum::http::StatusCode, msg: &str| {
        (
            status,
            Json(serde_json::json!({"ok": false, "message": msg})),
        )
    };

    let tracked = match resolve_tracked_series(&state.db, request_id).await {
        Ok(Some(t)) => t,
        Ok(None) => return json_err(axum::http::StatusCode::BAD_REQUEST, "Series not in library"),
        Err(e) => {
            return json_err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            );
        }
    };

    let mut pending =
        match grabbed_torrents::find_pending_for_episode(&state.db, tracked.id, episode_number)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                return json_err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                );
            }
        };

    // Drift case: `grabbed_torrents.state = 'imported'` but the
    // `episode_quality_tags` row for this episode is still 'grabbed'.
    // We fold these in via `find_imported_for_episode` only when the
    // episode's tag actually says 'grabbed'.
    let tag_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
    )
    .bind(tracked.id)
    .bind(episode_number)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let tag_is_grabbed = matches!(tag_state.as_deref(), Some("grabbed"));
    if tag_is_grabbed
        && let Ok(stuck) =
            grabbed_torrents::find_imported_for_episode(&state.db, tracked.id, episode_number).await
    {
        let tag_state_recheck: Option<String> = sqlx::query_scalar(
            "SELECT state FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
        )
        .bind(tracked.id)
        .bind(episode_number)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if matches!(tag_state_recheck.as_deref(), Some("grabbed")) {
            let seen: std::collections::HashSet<i64> = pending.iter().map(|g| g.id).collect();
            for g in stuck {
                if !seen.contains(&g.id) {
                    pending.push(g);
                }
            }
        } else {
            tracing::debug!(
                target: "ryokan::library",
                series_id = tracked.id,
                episode = episode_number,
                tag_state_now = ?tag_state_recheck,
                "cancel_pending_episode: tag flipped away from 'grabbed' mid-handler — skipping drift-repair branch"
            );
        }
    }

    if pending.is_empty() {
        if tag_is_grabbed {
            let _ = episode_tags::clear_tags_for_removal(&state.db, tracked.id, &[episode_number])
                .await;
            return (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "cancelled": 0,
                    "torrent_failures": Vec::<String>::new(),
                    "note": "Tag cleared; no associated torrent was found.",
                })),
            );
        }
        return json_err(
            axum::http::StatusCode::NOT_FOUND,
            "No pending grab found for this episode",
        );
    }

    tracing::debug!(
        target: "ryokan::library",
        series_id = tracked.id,
        episode = episode_number,
        grab_count = pending.len(),
        grab_ids = ?pending.iter().map(|g| g.id).collect::<Vec<_>>(),
        grab_names = ?pending.iter().map(|g| g.torrent_name.clone()).collect::<Vec<_>>(),
        batch_grabs = ?pending.iter().filter(|g| g.episode_numbers.len() > 1).map(|g| g.id).collect::<Vec<_>>(),
        tag_was_stuck_grabbed = tag_is_grabbed,
        "cancel_pending_episode: matching grabs"
    );

    let client = state.download_client.read().await.as_ref().cloned();

    let mut removed_count = 0;
    let mut torrent_failures: Vec<String> = Vec::new();
    for grab in &pending {
        if !grab.hash.is_empty()
            && let Some(ref client) = client
            && let Err(e) = client.delete(&grab.hash, true).await
        {
            torrent_failures.push(format!("{}: {}", grab.torrent_name, e));
            logger::warn(
                &state.db,
                LogCategory::QBit,
                &format!(
                    "Failed to remove pending torrent for S?E{:02} cancel: '{}'",
                    episode_number, grab.torrent_name
                ),
                &e,
            )
            .await;
        }

        if let Err(e) = grabbed_torrents::mark_removed(&state.db, grab.id).await {
            logger::warn(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Failed to mark grab {} as removed during cancel for S?E{:02}",
                    grab.id, episode_number
                ),
                &e.to_string(),
            )
            .await;
        } else {
            removed_count += 1;
        }
    }

    let _ = episode_tags::clear_tags_for_removal(&state.db, tracked.id, &[episode_number]).await;

    logger::info(
        &state.db,
        LogCategory::Library,
        &format!("Cancelled pending grab for episode {}", episode_number),
        &format!(
            "series_id={}, cancelled={}, qbit_failures={}",
            tracked.id,
            removed_count,
            torrent_failures.len()
        ),
    )
    .await;

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "cancelled": removed_count,
            "torrent_failures": torrent_failures,
        })),
    )
}

/// Get grab history for a specific episode.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/grab-history/{episode_number}",
    tag = "Library",
    summary = "Get episode grab history",
    description = "Returns the grab history for a specific episode, including quality tags and release info.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Grab history entries", body = Vec<episode_tags::GrabHistoryEntry>),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn get_episode_grab_history(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<Vec<episode_tags::GrabHistoryEntry>>, (axum::http::StatusCode, String)> {
    let series_id = resolve_tracked_series(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "Series not in library".to_string(),
        ))?
        .id;

    let history = episode_tags::get_grab_history(&state.db, series_id, episode_number)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(history))
}

/// Mark a grab as failed and re-trigger auto-search for the episode.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/mark-failed/{episode_number}",
    tag = "Library",
    summary = "Mark episode grab as failed",
    description = "Mark a grabbed episode as failed and optionally blocklist it, then re-search for a replacement.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = MarkEpisodeFailedForm,
    responses(
        (status = 200, description = "Re-search report", body = auto_search::AutoSearchReport),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn mark_episode_failed(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(form): Json<MarkEpisodeFailedForm>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "Series not in library".to_string(),
        ))?
        .id;

    let (_sid, _ep, release_title) = episode_tags::mark_grab_failed(&state.db, form.history_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if form.blocklist && !release_title.is_empty() {
        let _ = grabbed_torrents::mark_failed_by_name(&state.db, series_id, &release_title).await;
    }

    if let Ok(old_grabs) =
        grabbed_torrents::find_imported_for_episode(&state.db, series_id, episode_number).await
        && !old_grabs.is_empty()
    {
        let client = { state.download_client.read().await.as_ref().cloned() };
        if let Some(client) = client {
            for old in &old_grabs {
                if old.hash.is_empty() {
                    continue;
                }
                // Issue #28 PR C — preserve PT seed rules across
                // episode-replace. The old torrent has already
                // imported successfully and is seeding to its
                // per-tracker ratio/time policy; deleting it
                // mid-seed could ding the user's tracker ratio.
                if grabbed_torrents::respects_seed_rules(&state.db, &old.hash).await {
                    crate::services::logger::info(
                        &state.db,
                        crate::models::log::LogCategory::QBit,
                        &format!(
                            "Skipping client delete for replaced torrent {} (respect_seed_rules)",
                            old.torrent_name
                        ),
                        &old.hash,
                    )
                    .await;
                    continue;
                }
                if let Err(e) = client.delete(&old.hash, true).await {
                    crate::services::logger::warn(
                        &state.db,
                        crate::models::log::LogCategory::QBit,
                        &format!(
                            "Failed to remove old torrent for S?E{:02} replacement: '{}'",
                            episode_number, old.torrent_name
                        ),
                        &e,
                    )
                    .await;
                }
            }
        }
    }

    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        run_auto_search_targets(
            &state_clone,
            request_id,
            vec![target],
            false,
            Some(series_id),
        )
        .await
    });
    let report = handle.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search task failed: {}", e),
        )
    })??;

    Ok(Json(report))
}

/// Returns download progress for episodes of a series that are currently downloading.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/download-progress",
    tag = "Library",
    summary = "Episode download progress",
    description = "Returns download progress for all actively downloading episodes of a series.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Download progress per episode", body = Vec<EpisodeProgress>),
        (status = 400, description = "Series not in library"),
    ),
)]
pub async fn episode_download_progress(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<EpisodeProgress>>, (axum::http::StatusCode, String)> {
    let tracked = resolve_tracked_series(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "Series not in library".to_string(),
        ))?;

    let pending = crate::models::grabbed_torrents::get_all_pending(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if pending.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let grab_ids: Vec<i64> = pending.iter().map(|g| g.id).collect();
    let routes_by_grab =
        crate::models::grabbed_torrents::get_series_routes_for_grabs(&state.db, &grab_ids)
            .await
            .unwrap_or_default();

    let client = {
        let guard = state.download_client.read().await;
        match guard.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(Json(Vec::new())),
        }
    };

    let torrents = match client.list_scoped().await {
        Ok(t) => t,
        Err(err) => {
            return Err((
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!("Download client unavailable: {err}"),
            ));
        }
    };
    let by_hash: HashMap<String, &crate::services::download_client::DownloadItem> = torrents
        .iter()
        .map(|t| (t.hash.to_lowercase(), t))
        .collect();

    let mut results = Vec::new();
    for grab in &pending {
        let routes = routes_by_grab.get(&grab.id);
        let ep_nums: Vec<i32> = match routes {
            Some(routes) if !routes.is_empty() => routes
                .iter()
                .filter(|r| r.series_id == tracked.id)
                .flat_map(|r| r.episode_numbers.iter().copied())
                .collect(),
            _ if grab.series_id == tracked.id => grab.episode_numbers.clone(),
            _ => continue,
        };
        if ep_nums.is_empty() {
            continue;
        }

        let torrent = if !grab.hash.is_empty() {
            by_hash.get(&grab.hash.to_lowercase()).copied()
        } else {
            None
        };

        let Some(t) = torrent else {
            if crate::services::post_processing::grab_is_stale(&grab.grabbed_at, 30) {
                logger::info(
                    &state.db,
                    LogCategory::QBit,
                    &format!(
                        "Torrent removed in download client — reconciling '{}'",
                        grab.torrent_name
                    ),
                    &format!(
                        "series_id={} grab_id={} hash={}",
                        grab.series_id, grab.id, grab.hash
                    ),
                )
                .await;
                let _ = crate::models::grabbed_torrents::mark_removed(&state.db, grab.id).await;
                let _ = crate::models::episode_tags::clear_tags_for_removal(
                    &state.db,
                    grab.series_id,
                    &grab.episode_numbers,
                )
                .await;
            }
            continue;
        };

        for ep in ep_nums {
            results.push(EpisodeProgress {
                episode: ep,
                progress: t.progress,
                speed: t.dlspeed,
                state: t.state.clone(),
                state_kind: t.state_kind,
            });
        }
    }

    Ok(Json(results))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct EpisodeProgress {
    pub episode: i32,
    pub progress: f64,
    pub speed: i64,
    /// Client-native state string (qBit: `stalledUP`, Deluge: `Seeding`,
    /// Transmission: numeric code, rtorrent: computed). Kept for debug
    /// tooling; UI code should drive off `state_kind` for cross-client
    /// consistency.
    pub state: String,
    /// Normalized state slug from [`DownloadItemState`]. See its
    /// rendered form in the Downloads page state badges.
    pub state_kind: crate::services::download_client::DownloadItemState,
}

/// Returns the current episode state for a series as JSON.
///
/// Used by the series page's download-progress poller: when a torrent
/// disappears from the progress response (meaning the download completed and
/// the post-processing tick has moved the file into the library), the client
/// fetches this endpoint and patches the affected row in-place so the user
/// sees the new on-disk file without a full page refresh.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/episodes",
    tag = "Library",
    summary = "Episode state snapshot",
    description = "Returns the current list of episodes for a series, reflecting on-disk state.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Episode state", body = Vec<Episode>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn series_episodes_json(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<Episode>>, (axum::http::StatusCode, String)> {
    let (db_series, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();

    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg
        .as_ref()
        .map(|c| c.media_root.clone())
        .unwrap_or_default();

    let (episodes, _, _, _, _) =
        build_episodes(&state.db, &detail, db_id, &folder_name, &media_root).await;

    Ok(Json(episodes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::download_client::DownloadClient;
    use crate::services::download_client::qbittorrent::QbitClient;
    use crate::services::download_client::test_helpers;
    use crate::test_support;
    use std::sync::Arc;

    /// D2+D3 live integration test: cancelling a pending grab for an
    /// episode must delete the torrent from the active download
    /// client AND clear the grab state in the DB. Covers both the
    /// blocklist (D2) and episode-removal (D3) call paths — they
    /// share this same "delete in-flight grab and clean up state"
    /// trait surface. The blocklist-specific path (`mark_episode_failed`)
    /// additionally kicks off an auto-search re-run which requires
    /// live AniList + Nyaa and is outside the scope of this trait-
    /// boundary test.
    ///
    /// Flow:
    /// 1. Seed DB: series + pending grab_torrents row for episode 1.
    /// 2. Upload synthetic torrent to qBit with matching hash.
    /// 3. Call `cancel_pending_episode(anilist_id, 1)`.
    /// 4. Assert torrent deleted from qBit.
    #[tokio::test]
    #[ignore = "requires live qBit + transmission-create"]
    async fn d2_d3_cancel_pending_deletes_from_client() {
        if std::env::var("RYOKAN_QBIT_E2E").is_err() {
            eprintln!("skipping");
            return;
        }
        let Some((_tmp, torrent)) = test_helpers::build_named_torrent("d2-d3-cancel-pending")
        else {
            return;
        };
        let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
        let base_url = "http://localhost:8080";
        let category = "ryokan-e2e-d2d3";

        let hash =
            test_helpers::upload_torrent_file_qbit(base_url, "admin", &pass, category, &torrent)
                .await;

        let pool = test_support::in_memory_pool().await;
        let qbit: Arc<dyn DownloadClient> =
            Arc::new(QbitClient::new(base_url, "admin", &pass, category));
        let state = test_support::build_test_app_state(pool.clone(), Some(qbit.clone()));

        // Seed: series + pending grab for episode 1. The
        // `seed_grabbed_torrent` helper writes state='pending' and
        // episode_numbers='[1]' by default — matches what the
        // handler looks up.
        let anilist_id: i64 = 54321;
        let series_id = test_support::seed_series(&pool, anilist_id, "D2/D3 Test Series").await;
        test_support::seed_grabbed_torrent(&pool, series_id, &hash, "d2-d3-test.torrent", &[1])
            .await;
        assert_eq!(
            test_support::count_grabs_for_series(&pool, series_id).await,
            1,
            "precondition: 1 grab seeded"
        );

        // Exercise: cancel the pending grab for episode 1.
        let (status, body) = cancel_pending_episode(
            axum::extract::State(state.clone()),
            axum::extract::Path((anilist_id, 1)),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "cancel_pending_episode returned non-OK: {status} body={}",
            body.0
        );

        // Assert: torrent deleted from qBit.
        let check_client = QbitClient::new(base_url, "admin", &pass, category);
        let list = check_client
            .list_scoped()
            .await
            .expect("list_scoped post-cancel");
        assert!(
            !list.iter().any(|t| t.hash.eq_ignore_ascii_case(&hash)),
            "D2/D3: cancelled torrent must be deleted from qBit (still in list: {list:?})"
        );
        eprintln!("D2/D3 integration verified");
    }

    // ─── CI-gated episode-handler coverage (PR 7) ────────────────
    //
    // Directly-called handler tests that don't need a live
    // download client. Complements the env-gated d1/d2/d3 tests
    // above; those prove the client-backed paths, these prove the
    // DB-only paths that run whenever the client isn't involved
    // (404 on unknown series, grab-history pass-through,
    // resolve_tracked_series lookup semantics).
    mod episodes_ci {
        use super::super::*;
        use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::response::Json as AxumJson;

        // ─── get_episode_grab_history ────────────────────────────

        #[tokio::test]
        async fn get_episode_grab_history_returns_empty_for_series_without_grabs() {
            let db = in_memory_pool().await;
            let anilist_id: i64 = 100;
            let _ = seed_series(&db, anilist_id, "New Show").await;
            let state = build_test_app_state(db, None);
            let AxumJson(history) = get_episode_grab_history(State(state), Path((anilist_id, 1)))
                .await
                .expect("empty history should be Ok, not error");
            assert!(history.is_empty());
        }

        #[tokio::test]
        async fn get_episode_grab_history_rejects_untracked_series_with_400() {
            // Series not in library → 400, not a silent empty list.
            // Caller expects a clear "you asked about a series I
            // don't track" signal rather than a success-with-zero-
            // rows that might be mistaken for "no grabs yet."
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let err = get_episode_grab_history(State(state), Path((99_999, 1)))
                .await
                .expect_err("unknown series should 400");
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            assert!(err.1.contains("not in library"));
        }

        #[tokio::test]
        async fn get_episode_grab_history_accepts_internal_id_and_anilist_id_equivalently() {
            // The path parameter can be either an AniList id or an
            // internal series id — resolve_tracked_series handles
            // both. Pin dual-lookup PARITY by seeding a real grab
            // row and asserting that both paths return it. Without
            // the seed, an empty-history-for-both result would only
            // prove the internal-id branch doesn't reject — it
            // wouldn't catch a regression that routed the two ids
            // to different series.
            let db = in_memory_pool().await;
            let anilist_id: i64 = 101;
            let series_id = seed_series(&db, anilist_id, "Show").await;
            // Raw SQL insert — bypasses episode_tags::record_grab's
            // ClassificationResult plumbing since we only care that
            // the row round-trips through the handler's resolver.
            sqlx::query(
                "INSERT INTO episode_grab_history \
                 (series_id, episode_number, quality_tag, release_title, release_group) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(series_id)
            .bind(5_i32)
            .bind("WEBDL-1080p")
            .bind("[Group] Show - 05 [WEB-DL 1080p].mkv")
            .bind("Group")
            .execute(&db)
            .await
            .unwrap();

            let state = build_test_app_state(db, None);
            let AxumJson(via_al) =
                get_episode_grab_history(State(state.clone()), Path((anilist_id, 5)))
                    .await
                    .expect("AL-id lookup should work");
            let AxumJson(via_internal) =
                get_episode_grab_history(State(state), Path((series_id, 5)))
                    .await
                    .expect("internal-id lookup should work");

            assert_eq!(via_al.len(), 1, "AL-id lookup must return the seeded grab");
            assert_eq!(
                via_internal.len(),
                1,
                "internal-id lookup must return the seeded grab"
            );
            // Parity: both paths resolve to the same series, so they
            // return the same row (same release_title + quality_tag).
            assert_eq!(via_al[0].release_title, via_internal[0].release_title);
            assert_eq!(via_al[0].quality_tag, via_internal[0].quality_tag);
            assert_eq!(
                via_al[0].release_title,
                "[Group] Show - 05 [WEB-DL 1080p].mkv"
            );
        }

        // ─── delete_episode_file (no-client path) ─────────────────

        #[tokio::test]
        async fn delete_episode_file_on_unknown_series_returns_error_status() {
            // `delete_episode_file` returns `(StatusCode, Json<Value>)`
            // directly — no Result wrapper — with an `ok: false` body.
            // The specific status depends on the resolve path: in
            // an offline test env, `resolve_series_context` fails
            // before reaching the "series not in library" branch
            // (AniList unreachable → 502). Either way, the handler
            // must emit a 4xx/5xx with a structured JSON body so
            // the UI can show the reason; silently succeeding on an
            // unknown id would delete phantom files.
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let (status, body) = delete_episode_file(State(state), Path((99_999, 1))).await;
            assert!(
                status.is_client_error() || status.is_server_error(),
                "unknown series must be an error status, got {status}"
            );
            assert_eq!(body.0["ok"], false);
        }

        // `series_episodes_json` + `mark_episode_failed` are
        // deliberately not covered here: both call
        // `resolve_series_context` which unconditionally consults
        // AniList + metadata_cache on the first miss, so exercising
        // them from a cold in-memory DB just hits AniList. Covering
        // them requires either wiremock'ing the AniList client
        // (punted with the rest of the HTTP-backed provider
        // tests) or seeding the provider_metadata_cache table,
        // which is a separate plan item.

        #[test]
        fn episode_progress_wire_shape_carries_both_state_fields() {
            // The series-page download-progress poller (series.js)
            // keys off `state_kind` for the "Importing…" transition
            // check (kind starts with "seeding" → torrent finished).
            // The client-native `state` string is kept for debug
            // tooling only. Regression for the PR that added
            // state_kind: a silent `#[serde(skip)]` or rename would
            // leave `isComplete` always false and the progress bar
            // stuck at 100% forever.
            let p = super::super::EpisodeProgress {
                episode: 5,
                progress: 1.0,
                speed: 0,
                state: "stalledUP".to_string(),
                state_kind: crate::services::download_client::DownloadItemState::SeedingStalled,
            };
            let v = serde_json::to_value(&p).unwrap();
            assert_eq!(v["episode"], 5);
            assert_eq!(v["state"], "stalledUP");
            assert_eq!(v["state_kind"], "seeding-stalled");
        }
    }
}
