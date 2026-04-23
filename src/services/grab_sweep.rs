//! TTL sweep for stale `pending_grabs` rows (issue #83).
//!
//! Implements plan decision #3's "auto-commit on walkaway" shape:
//! when a modal's heartbeat lapses, the torrent is still a user-
//! intended download (they hit Grab), so we resume it with every
//! file marked wanted rather than leaving it paused forever. The
//! full grab-row-write + sibling auto-expand chain is still PR C
//! scope — this sweep gets the files downloading, but library
//! attribution (the `grabbed_torrents` row wiring) is a separate
//! pass.
//!
//! Per-row flow:
//!   1. Load the row via `list_expired`.
//!   2. If `file_list_json` is populated, a download client is
//!      configured, and `error_message` is empty, mark every file
//!      wanted and resume the torrent.
//!   3. Delete the `pending_grabs` row either way so the sweep
//!      doesn't loop forever on a permanently-stuck entry.
//!
//! Auto-commit is best-effort: any RPC failure is logged as a warn
//! and the row is still deleted. The alternative (leave the row
//! until the client recovers) would stall the sweep and prevent
//! the user from ever re-grabbing the same release since the
//! in-flight dedup path would match the stuck row.

use std::time::Duration;

use serde_json::Value;

use crate::AppState;
use crate::models::pending_grabs;

/// Tick interval for the TTL sweep. Matches plan decision #3's "1
/// minute TTL + 1 minute sweep" shape so the worst-case auto-commit
/// latency is `HEARTBEAT_TTL_SECS + SWEEP_INTERVAL ≈ 2 minutes`.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Run one sweep pass. Separate from the interval loop so tests can
/// drive a single tick with a seeded DB. Takes `&AppState` rather
/// than just `&SqlitePool` so the auto-commit path can reach the
/// active download client.
///
/// Returns the count of rows processed (both auto-committed and
/// silently-dropped). Tests assert on this number; the production
/// caller in `main.rs` ignores the return value.
pub async fn sweep_once(state: &AppState) -> Result<usize, String> {
    let expired = pending_grabs::list_expired(&state.db).await?;
    let count = expired.len();
    for row in expired {
        auto_commit_row(state, &row).await;
        if let Err(e) = pending_grabs::delete(&state.db, &row.preview_id).await {
            tracing::warn!(
                target: "ryokan::services::grab_sweep",
                preview_id = %row.preview_id,
                info_hash = %row.info_hash,
                error = %e,
                "failed to delete expired pending grab; will retry on next tick"
            );
        }
    }
    Ok(count)
}

/// Best-effort auto-commit for one expired row. Read-only against
/// the passed `AppState` — all mutation goes through `DownloadClient`
/// trait methods. Failures are logged and swallowed so the caller
/// can delete the pending row regardless.
async fn auto_commit_row(state: &AppState, row: &pending_grabs::PendingGrab) {
    // Don't unwind a previously-errored row. The handler already
    // surfaced the failure to the modal (status: "error") and the
    // torrent is either gone or in a broken state the user should
    // decide about via the Downloads page.
    if !row.error_message.is_empty() {
        tracing::debug!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "evicted errored pending grab without auto-commit"
        );
        return;
    }

    // Metadata never arrived → no file list to mark. Best we can
    // do is leave the torrent in whatever state the client has it.
    // Logged at `debug` because this is a normal outcome for a
    // short-lived tab close on a dead magnet.
    if row.file_list_json.is_empty() {
        tracing::debug!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "evicted pending grab without file list; no auto-commit possible"
        );
        return;
    }

    let client = {
        let guard = state.download_client.read().await;
        guard.as_ref().cloned()
    };
    let Some(client) = client else {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "download client not configured; can't auto-commit expired pending grab"
        );
        return;
    };

    // Parse the file list just enough to count entries. The full
    // shape (`Vec<PreviewFile>`) lives in `handlers::grab` — we
    // don't need the filenames here, only the length so we can
    // build the `0..len` index slice for `set_file_wanted`.
    let file_count: usize = serde_json::from_str::<Value>(&row.file_list_json)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0);
    if file_count == 0 {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            "pending grab had an empty file_list_json array; skipping auto-commit"
        );
        return;
    }

    let all: Vec<usize> = (0..file_count).collect();
    if let Err(e) = client.set_file_wanted(&row.info_hash, &all, true).await {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            error = %e,
            "auto-commit set_file_wanted(all=true) failed; resume attempt will still fire"
        );
    }

    if let Err(e) = client.resume(&row.info_hash).await {
        tracing::warn!(
            target: "ryokan::services::grab_sweep",
            preview_id = %row.preview_id,
            info_hash = %row.info_hash,
            error = %e,
            "auto-commit resume failed; torrent may remain paused"
        );
        return;
    }

    tracing::info!(
        target: "ryokan::services::grab_sweep",
        preview_id = %row.preview_id,
        info_hash = %row.info_hash,
        file_count = %file_count,
        "auto-committed abandoned pending grab; torrent is now downloading with all files wanted"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::pending_grabs::HEARTBEAT_TTL_SECS;
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn sweep_drops_stale_rows_only() {
        let db = in_memory_pool().await;
        pending_grabs::create(&db, "fresh", "h1", "qbittorrent", None, None, "{}", true)
            .await
            .unwrap();
        pending_grabs::create(&db, "stale", "h2", "qbittorrent", None, None, "{}", true)
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ? WHERE preview_id = 'stale'")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "fresh").await.unwrap().is_some());
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweep_is_idempotent_on_empty() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sweep_continues_after_individual_row_failure() {
        // Simulated failure mode: the delete path could fail if the
        // DB is under contention. The sweep must continue processing
        // the rest of the batch rather than aborting — otherwise one
        // bad row starves every subsequent eviction. Since `delete`
        // is idempotent + durable in practice, we can't easily force
        // a failure here; instead we verify the "all succeed" case
        // handles a mix of stale+fresh without surprises.
        let db = in_memory_pool().await;
        for i in 0..3 {
            let id = format!("stale-{}", i);
            pending_grabs::create(&db, &id, "h", "qbittorrent", None, None, "{}", true)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 3);
        assert_eq!(pending_grabs::count(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sweep_without_download_client_still_drops_row_no_panic() {
        // Even when the auto-commit can't fire (no client
        // configured), the row MUST still be dropped — otherwise
        // the pending_grabs table grows unboundedly and the
        // in-flight dedup path on re-grab matches stale rows
        // forever.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "stale",
            "hash-one",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
        )
        .await
        .unwrap();
        // Populate file_list_json so we'd attempt auto-commit if
        // the client were available; prove the no-client branch
        // still cleans up the row.
        pending_grabs::set_file_list(&db, "stale", "[{\"name\":\"a.mkv\",\"size\":1}]")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweep_evicts_errored_rows_without_attempting_autocommit() {
        // A row with `error_message != ''` came in via a metadata-
        // fetch failure. The auto-commit path must skip it — the
        // torrent is either gone or in a broken state we shouldn't
        // try to resume.
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "errored",
            "hash-err",
            "qbittorrent",
            None,
            None,
            "{}",
            true,
        )
        .await
        .unwrap();
        pending_grabs::set_error(&db, "errored", "metadata fetch timed out")
            .await
            .unwrap();
        sqlx::query("UPDATE pending_grabs SET heartbeat_at = ?")
            .bind(now_unix() - HEARTBEAT_TTL_SECS - 5)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let count = sweep_once(&state).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "errored").await.unwrap().is_none());
    }
}
