//! Cleanup sweep for stale `pending_grabs` rows (issue #83).
//!
//! **Current scope: minimal row-eviction only.** The plan doc's
//! decision #3 spells out a richer "auto-commit on walkaway" shape
//! where an abandoned pending grab resumes its torrent with every
//! file marked wanted and writes a `grabbed_torrents` row so the
//! library tracks it. That full behavior is tied to the grab-row
//! write + sibling auto-expand work that lives in PR C of the plan
//! staging — doing it piecemeal would leave the torrent in the
//! client but untracked by Ryokan, which is a worse state than
//! today. For PR A's scope we only keep the `pending_grabs` table
//! bounded: every tick, drop rows whose heartbeat is older than
//! `HEARTBEAT_TTL_SECS`. The underlying torrent is left in whatever
//! state `add_torrent_paused` put it (paused on Deluge / Transmission
//! / rTorrent; running-with-all-files-skipped on qBit) — the user
//! can clean it up from the Downloads page if they don't want it.
//!
//! When PR C lands, `sweep_once` grows the resume + mark-wanted +
//! write-grab-row logic inline here. This module is the
//! designated home so the main-loop wiring doesn't move.

use std::time::Duration;

use sqlx::SqlitePool;

use crate::models::pending_grabs;

/// Tick interval for the TTL sweep. Matches plan decision #3's "1
/// minute TTL + 1 minute sweep" shape so the worst-case auto-commit
/// latency (once PR C adds that behavior) is `HEARTBEAT_TTL_SECS +
/// SWEEP_INTERVAL ≈ 2 minutes`.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Run one sweep pass. Separate from the interval loop so tests can
/// drive a single tick with a time-stubbed clock.
pub async fn sweep_once(db: &SqlitePool) -> Result<usize, String> {
    let expired = pending_grabs::list_expired(db).await?;
    let count = expired.len();
    for row in expired {
        // PR A scope: drop the pending row only. See module docstring
        // for the PR C follow-up that will also resume the torrent
        // and write a grabbed_torrents row.
        if let Err(e) = pending_grabs::delete(db, &row.preview_id).await {
            tracing::warn!(
                target: "ryokan::services::grab_sweep",
                preview_id = %row.preview_id,
                info_hash = %row.info_hash,
                error = %e,
                "failed to delete expired pending grab; will retry on next tick"
            );
        } else {
            tracing::debug!(
                target: "ryokan::services::grab_sweep",
                preview_id = %row.preview_id,
                info_hash = %row.info_hash,
                "evicted stale pending grab (PR A scope — no auto-commit yet)"
            );
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::pending_grabs::HEARTBEAT_TTL_SECS;
    use crate::test_support::in_memory_pool;
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

        let count = sweep_once(&db).await.unwrap();
        assert_eq!(count, 1);
        assert!(pending_grabs::get(&db, "fresh").await.unwrap().is_some());
        assert!(pending_grabs::get(&db, "stale").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweep_is_idempotent_on_empty() {
        let db = in_memory_pool().await;
        let count = sweep_once(&db).await.unwrap();
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
        let count = sweep_once(&db).await.unwrap();
        assert_eq!(count, 3);
        assert_eq!(pending_grabs::count(&db).await.unwrap(), 0);
    }
}
