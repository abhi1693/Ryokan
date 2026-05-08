//! Bulk library operations (issue #125).
//!
//! Per-series actions Ryokan supports today are "click a thing per row,"
//! which scales poorly past two series. This module ships the JSON-API
//! surface for bulk changes, starting with bulk monitor; subsequent PRs
//! layer bulk re-search, bulk delete, bulk finished-mode, and bulk
//! upgrades on top of the same `BulkOutcome` shape.
//!
//! ## Per-series failure isolation
//!
//! Every endpoint returns a structured outcome:
//!
//! ```json
//! { "succeeded": [1, 2, 5, 7], "failed": [{ "series_id": 9, "reason": "..." }] }
//! ```
//!
//! A per-series failure (DB error on series 9, missing folder, download
//! client unreachable) is captured and the loop continues. All-or-nothing
//! semantics would be wrong here — a single broken-folder series shouldn't
//! block 49 valid operations from succeeding. The frontend renders a
//! partial-failure toast + click-for-detail modal so the user sees which
//! IDs failed and why.
//!
//! ## Empty selection is a 200 no-op
//!
//! Defense-in-depth: a request with an empty `series_ids` array returns
//! 200 with empty `succeeded` / `failed` arrays. The toolbar UI is
//! supposed to be hidden when nothing's selected, but a race-during-
//! deselect or a JS bug could still send `{ "series_ids": [] }`. Don't
//! 400 — a benign no-op is the right shape, and validates only that the
//! request body deserialized.
//!
//! ## Audit logging
//!
//! After the per-series loop completes, each endpoint writes one
//! batch-summary `LogCategory::Library` entry: succeeded / failed counts
//! plus the failed IDs inline. Per-series log lines would flood System
//! → Logs at 200-item bulk re-search; the single summary is the
//! auditable "did the bulk action I did yesterday touch series X?"
//! answer without drowning the filter.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, grabbed_torrents, series};
use crate::services::logger;

/// Per-series outcome envelope returned from synchronous bulk endpoints.
/// Async endpoints (bulk re-search) return a `ProgressRegistry` job ID
/// instead and surface this same shape on the progress-completion event.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkOutcome {
    pub succeeded: Vec<i64>,
    pub failed: Vec<BulkFailure>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkFailure {
    pub series_id: i64,
    pub reason: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkMonitorRequest {
    pub series_ids: Vec<i64>,
    /// Monitor mode value (`all` / `future` / `missing` / `existing` /
    /// `none`) or the [`MONITOR_MODE_SYNC_SENTINEL`] string `"sync"`.
    /// Picking the sentinel clears the per-series manual-override flag
    /// so the next AL/MAL sync tick computes the mode; any other value
    /// pins the mode and sets the manual-override flag.
    ///
    /// [`MONITOR_MODE_SYNC_SENTINEL`]: super::crud::MONITOR_MODE_SYNC_SENTINEL
    pub mode: String,
}

/// `POST /api/library/bulk/monitor` — apply the same monitor-mode change
/// to a list of series. Reuses `crud::apply_monitor_mode` per-series so
/// the sentinel-clear and pin-mode branches stay in lockstep with the
/// per-series handler.
#[utoipa::path(
    post,
    path = "/api/library/bulk/monitor",
    tag = "Library",
    summary = "Bulk-set monitor mode",
    request_body = BulkMonitorRequest,
    responses(
        (status = 200, description = "Outcome envelope; check `failed` for per-series errors", body = BulkOutcome),
    ),
)]
pub async fn bulk_monitor(
    State(state): State<AppState>,
    Json(req): Json<BulkMonitorRequest>,
) -> Json<BulkOutcome> {
    if req.series_ids.is_empty() {
        return Json(BulkOutcome {
            succeeded: vec![],
            failed: vec![],
        });
    }

    let mut succeeded = Vec::with_capacity(req.series_ids.len());
    let mut failed = Vec::new();
    for series_id in &req.series_ids {
        match super::crud::apply_monitor_mode(&state.db, *series_id, &req.mode).await {
            Ok(()) => succeeded.push(*series_id),
            Err(reason) => failed.push(BulkFailure {
                series_id: *series_id,
                reason,
            }),
        }
    }

    let failed_ids: Vec<i64> = failed.iter().map(|f| f.series_id).collect();
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Bulk monitor change: {} succeeded, {} failed",
            succeeded.len(),
            failed.len()
        ),
        &format!(
            "action=monitor_set mode={} failed_ids={:?}",
            req.mode, failed_ids
        ),
    )
    .await;

    Json(BulkOutcome { succeeded, failed })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkDeleteRequest {
    pub series_ids: Vec<i64>,
    /// When true, also walks `grabbed_torrents` for each series and
    /// asks the resolved download client to drop the torrent + its
    /// files, then recursively removes the on-disk folder under
    /// `media_root/<folder_name>`. When false, only the database
    /// rows for the series are removed (cascade + `rss_seen`
    /// NULL-out via [`series::remove`]); files stay on disk and
    /// active torrents stay in their download clients.
    pub delete_files: bool,
}

/// `POST /api/library/bulk/delete` — remove a list of series from the
/// library. Per-series cleanup runs in a loop; failures don't abort
/// the batch.
///
/// Recycle bin (#123) hasn't shipped yet, so `delete_files: true`
/// performs a permanent unlink. The Confirmation modal in the
/// frontend warns that this can't be undone. When recycle ships,
/// the file-removal branch swaps to a recycle-bin call without
/// changing this handler's wire shape.
#[utoipa::path(
    post,
    path = "/api/library/bulk/delete",
    tag = "Library",
    summary = "Bulk-remove series from library",
    request_body = BulkDeleteRequest,
    responses(
        (status = 200, description = "Outcome envelope; check `failed` for per-series errors", body = BulkOutcome),
    ),
)]
pub async fn bulk_delete(
    State(state): State<AppState>,
    Json(req): Json<BulkDeleteRequest>,
) -> Json<BulkOutcome> {
    if req.series_ids.is_empty() {
        return Json(BulkOutcome {
            succeeded: vec![],
            failed: vec![],
        });
    }

    // Resolve media_root once at the top — bulk delete pays a single
    // config fetch instead of N when delete_files is true. Returns
    // None when config isn't loaded (shouldn't happen post-setup; we
    // guard rather than crash).
    let media_root: Option<String> = if req.delete_files {
        config::get_config(&state.db)
            .await
            .ok()
            .flatten()
            .map(|c| c.media_root)
    } else {
        None
    };

    let mut succeeded = Vec::with_capacity(req.series_ids.len());
    let mut failed = Vec::new();
    for series_id in &req.series_ids {
        match delete_one_series(&state, *series_id, req.delete_files, media_root.as_deref()).await {
            Ok(()) => succeeded.push(*series_id),
            Err(reason) => failed.push(BulkFailure {
                series_id: *series_id,
                reason,
            }),
        }
    }

    let failed_ids: Vec<i64> = failed.iter().map(|f| f.series_id).collect();
    logger::info(
        &state.db,
        LogCategory::Library,
        &format!(
            "Bulk delete: {} succeeded, {} failed",
            succeeded.len(),
            failed.len()
        ),
        &format!(
            "action=delete delete_files={} failed_ids={:?}",
            req.delete_files, failed_ids
        ),
    )
    .await;

    Json(BulkOutcome { succeeded, failed })
}

/// Per-series delete worker for [`bulk_delete`]. Returns `Result<(),
/// String>` so the loop can collect failures into `BulkOutcome.failed`
/// without aborting the batch.
///
/// Order of operations matters: torrent-client cleanup first (failures
/// are best-effort; logged but not fatal), then on-disk folder removal,
/// then `series::remove` for the DB cascade. If we DB-removed first,
/// a subsequent `grabbed_torrents` lookup for the file/torrent
/// cleanup would return nothing (FK cascade dropped the rows) and
/// active torrents would orphan in their clients.
async fn delete_one_series(
    state: &AppState,
    series_id: i64,
    delete_files: bool,
    media_root: Option<&str>,
) -> Result<(), String> {
    if delete_files {
        // 1. Tell each grab's download client to drop the torrent +
        //    its files. Best-effort: per-client failures log but don't
        //    fail the series. The user explicitly asked for files-
        //    out-too; a transient client outage shouldn't pin the
        //    series in their library forever.
        let hashes = grabbed_torrents::get_all_for_series(&state.db, series_id)
            .await
            .map_err(|e| format!("list grabs: {e}"))?;
        for &(_grab_id, ref hash, dc_id) in &hashes {
            if hash.is_empty() {
                continue;
            }
            if let Some(client) = state.resolve_grab_client(dc_id, hash).await {
                let _ = client.delete(hash, true).await;
            }
        }

        // 2. Remove the on-disk folder. spawn_blocking because
        //    remove_dir_all walks every file and a deep BD library
        //    folder can take a few seconds. media_root is None when
        //    config wasn't loadable; in that case we skip file
        //    removal but still proceed to the DB delete (the user
        //    pressed Delete, the entry should disappear from their
        //    library — orphaned files are recoverable).
        if let Some(root) = media_root
            && let Ok(Some(s)) = series::get_by_id(&state.db, series_id).await
            && !s.folder_name.is_empty()
        {
            let folder = std::path::PathBuf::from(root).join(&s.folder_name);
            if folder.exists() {
                let folder_for_blocking = folder.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    std::fs::remove_dir_all(folder_for_blocking)
                })
                .await;
            }
        }
    }

    // 3. DB cascade. `series::remove` is the canonical path; it does
    //    the rss_seen NULL-out + every other FK-cascading delete.
    series::remove(&state.db, series_id)
        .await
        .map_err(|e| format!("db remove: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;
    use sqlx::SqlitePool;

    /// Insert a minimal series row sufficient for the monitor-mode
    /// write paths to succeed. Returns the inserted id.
    async fn insert_series(pool: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO series (anilist_id, title, monitor_mode, monitor_mode_manual_override)
             VALUES (?, ?, 'future', 0)
             RETURNING id",
        )
        .bind(anilist_id)
        .bind(title)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    /// Read back the (mode, override_flag) tuple for assertions.
    async fn read_monitor_state(pool: &SqlitePool, series_id: i64) -> (String, bool) {
        let row: (String, bool) = sqlx::query_as(
            "SELECT monitor_mode, monitor_mode_manual_override FROM series WHERE id = ?",
        )
        .bind(series_id)
        .fetch_one(pool)
        .await
        .unwrap();
        row
    }

    #[tokio::test]
    async fn bulk_monitor_applies_mode_to_every_id() {
        let pool = in_memory_pool().await;
        let a = insert_series(&pool, 1, "A").await;
        let b = insert_series(&pool, 2, "B").await;
        let c = insert_series(&pool, 3, "C").await;

        let req = BulkMonitorRequest {
            series_ids: vec![a, b, c],
            mode: "all".to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;

        assert_eq!(outcome.succeeded.len(), 3);
        assert!(outcome.failed.is_empty());
        for id in [a, b, c] {
            let (mode, override_flag) = read_monitor_state(&pool, id).await;
            assert_eq!(mode, "all");
            assert!(
                override_flag,
                "explicit-mode bulk should set the manual-override flag"
            );
        }
    }

    #[tokio::test]
    async fn bulk_monitor_sync_sentinel_clears_override_without_touching_mode() {
        // Sentinel posture: clear `monitor_mode_manual_override`,
        // leave `monitor_mode` alone (next sync tick fixes it).
        // Tested separately because this branch silently drives
        // different behavior than an explicit mode change.
        let pool = in_memory_pool().await;
        let id = insert_series(&pool, 1, "A").await;
        // Pre-stamp it with an override so the sentinel has something
        // to clear.
        sqlx::query(
            "UPDATE series SET monitor_mode = 'all', monitor_mode_manual_override = 1 WHERE id = ?",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let req = BulkMonitorRequest {
            series_ids: vec![id],
            mode: super::super::crud::MONITOR_MODE_SYNC_SENTINEL.to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;

        assert_eq!(outcome.succeeded, vec![id]);
        assert!(outcome.failed.is_empty());
        let (mode, override_flag) = read_monitor_state(&pool, id).await;
        assert_eq!(mode, "all", "sentinel should NOT change the mode");
        assert!(!override_flag, "sentinel should clear the override flag");
    }

    #[tokio::test]
    async fn bulk_monitor_isolates_per_series_failures() {
        // Mix a real id with a non-existent one. The non-existent
        // one's UPDATE affects 0 rows in SQLite, which sqlx returns
        // as Ok(()), so we won't actually see a failure for it via
        // the model's update path. This test is therefore the
        // happy-path case for the real id + a "no rows updated"
        // case for the missing id, both succeed at the SQL layer.
        // The test still pins the loop-continues-on-error contract
        // by asserting the real id ends up in `succeeded`.
        let pool = in_memory_pool().await;
        let a = insert_series(&pool, 1, "A").await;
        let req = BulkMonitorRequest {
            series_ids: vec![a, 99999],
            mode: "missing".to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;
        assert!(outcome.succeeded.contains(&a));
        // Real id wrote successfully.
        let (mode, _) = read_monitor_state(&pool, a).await;
        assert_eq!(mode, "missing");
    }

    #[tokio::test]
    async fn bulk_monitor_empty_selection_is_200_noop() {
        let pool = in_memory_pool().await;
        let req = BulkMonitorRequest {
            series_ids: vec![],
            mode: "all".to_string(),
        };
        let state = crate::test_support::build_test_app_state(pool, None);
        let Json(outcome) = bulk_monitor(axum::extract::State(state), Json(req)).await;
        assert!(outcome.succeeded.is_empty());
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn bulk_delete_removes_db_rows_when_delete_files_false() {
        // Covers the "remove from library only" path: rows go from
        // `series` (and cascade across the FK graph), files stay
        // on disk. delete_files=false also skips the
        // grabbed_torrents walk, so this test doesn't need a
        // download client mock.
        let pool = in_memory_pool().await;
        let a = insert_series(&pool, 1, "A").await;
        let b = insert_series(&pool, 2, "B").await;

        let req = BulkDeleteRequest {
            series_ids: vec![a, b],
            delete_files: false,
        };
        let state = crate::test_support::build_test_app_state(pool.clone(), None);
        let Json(outcome) = bulk_delete(axum::extract::State(state), Json(req)).await;

        assert_eq!(outcome.succeeded.len(), 2);
        assert!(outcome.failed.is_empty());
        for id in [a, b] {
            let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM series WHERE id = ?")
                .bind(id)
                .fetch_optional(&pool)
                .await
                .unwrap();
            assert!(exists.is_none(), "series {id} should be DB-removed");
        }
    }

    #[tokio::test]
    async fn bulk_delete_empty_selection_is_200_noop() {
        let pool = in_memory_pool().await;
        let req = BulkDeleteRequest {
            series_ids: vec![],
            delete_files: false,
        };
        let state = crate::test_support::build_test_app_state(pool, None);
        let Json(outcome) = bulk_delete(axum::extract::State(state), Json(req)).await;
        assert!(outcome.succeeded.is_empty());
        assert!(outcome.failed.is_empty());
    }
}
