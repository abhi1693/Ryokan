//! Watch-list sync background task (issue #62 PR B).
//!
//! Pulls the user's AniList or MyAnimeList watch list into the
//! Ryokan library on a configurable cadence (default 30 minutes,
//! range 15 minutes .. 7 days per plan decision #5). One linked
//! account at a time; the supervised task no-ops when nothing is
//! linked or the linked account's tokens fail to decrypt.
//!
//! ## Sync strategy (decision #4)
//!
//! - **Delta on every tick**: query the provider for entries with
//!   `updatedAt > list_last_synced_at`. Cheap, catches the 99%
//!   common case (status changes, score updates, additions).
//! - **Full resync once a week**: backstop against provider-side
//!   drift (missed `updatedAt` fires, backdated bulk imports,
//!   schema additions that retroactively populate fields).
//! - **First sync** is always full: `list_last_synced_at` is NULL,
//!   so there's no delta cursor to start from. Uses a staging-table-
//!   then-merge transaction so the library never flickers through
//!   a half-imported state.
//!
//! ## Status (this commit)
//!
//! Skeleton only: the supervised task ticks every minute, reads the
//! configured interval, calls `tick_once` when the cadence is due,
//! and `tick_once` no-ops by reading the linked account and emitting
//! a placeholder log. Subsequent commits fill in:
//!   1. AniList `MediaListCollection` GraphQL fetch.
//!   2. MyAnimeList animelist endpoint + access-token refresh on 401.
//!   3. Sync engine: staging table merge + monitor-mode defaults +
//!      bulk-mode coalescing for sync-originated adds.
//!   4. Manual "Sync now" button + ProgressRegistry hook for the
//!      sticky-toast first-sync UI.

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::external_accounts;
use crate::models::log::LogCategory;
use crate::services::logger;

/// Run one sync iteration against the linked account. Called by the
/// supervised loop in `main.rs::external_sync` once per configured
/// interval.
///
/// Returns a one-line summary used by `scheduled_task_runs.detail`.
/// Errors bubble up so the supervised loop's `mark_finished("error",
/// …)` path captures the failure.
pub async fn tick_once(state: &AppState) -> Result<String, String> {
    let account = external_accounts::get_current(&state.db)
        .await
        .map_err(|e| format!("read external_accounts: {e}"))?;

    let Some(account) = account else {
        return Ok("no external account linked".to_string());
    };

    // Subsequent commits replace this branch with the real provider
    // dispatch (AL `MediaListCollection` query / MAL animelist
    // pagination + token refresh) and the staging-table merge. For
    // now we just confirm the linked-account decrypt succeeded and
    // log the no-op so System → Logs shows the cadence is alive.
    logger::debug(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "watch-list sync tick (placeholder): provider={} username={}",
            account.provider, account.username
        ),
        "",
    )
    .await;

    Ok(format!(
        "no-op (provider={}, sync engine lands in a follow-up commit)",
        account.provider
    ))
}

/// Read the most recent successful tick from `scheduled_task_runs`.
/// The supervised loop seeds its `minutes_since_last` counter from
/// this so a process restart doesn't force an immediate re-run when
/// we last synced under the configured cadence.
pub async fn minutes_since_last_run(db: &SqlitePool) -> i64 {
    crate::models::scheduled_tasks::minutes_since_last_finished(db, "external_sync")
        .await
        .unwrap_or(10_000)
}
