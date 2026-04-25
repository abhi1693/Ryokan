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
use crate::services::{anilist, logger, mal};

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

    match account.provider.as_str() {
        external_accounts::PROVIDER_ANILIST => sync_anilist_dryrun(state, &account).await,
        external_accounts::PROVIDER_MAL => sync_mal_dryrun(state, account).await,
        other => {
            // Unknown provider string — schema CHECK constraint should
            // prevent this, but surface explicitly rather than panic.
            Err(format!("unknown external_accounts.provider: {other}"))
        }
    }
}

/// Fetch the AL watch list and log a count summary. This commit
/// validates the token + network path + GraphQL parser end-to-end
/// without writing to `series` yet — the staging-table merge that
/// turns the fetched entries into library rows lands in a follow-up
/// commit alongside monitor-mode defaults and bulk-mode coalescing.
async fn sync_anilist_dryrun(
    state: &AppState,
    account: &external_accounts::ExternalAccount,
) -> Result<String, String> {
    let user_id: i64 = account.provider_user_id.parse().map_err(|e| {
        format!(
            "AL provider_user_id is not a valid integer: {} ({e})",
            account.provider_user_id
        )
    })?;

    let entries = anilist::fetch_media_list_collection(&account.access_token, user_id).await?;
    let total = entries.len();
    let with_score = entries.iter().filter(|e| e.score > 0.0).count();
    let on_custom_lists = entries
        .iter()
        .filter(|e| !e.custom_lists.is_empty())
        .count();

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "AniList watch-list fetched: {} entries ({} scored, {} on custom lists)",
            total, with_score, on_custom_lists
        ),
        &format!("username={}", account.username),
    )
    .await;

    Ok(format!(
        "AniList: fetched {} entries (merge lands in a follow-up commit)",
        total
    ))
}

/// Fetch the MAL watch list and log a count summary. Same dry-run
/// shape as `sync_anilist_dryrun` — validates the live token, the
/// network path, the parser, and the on-401 refresh-and-retry
/// dance. Doesn't merge into `series` yet.
///
/// Token-refresh happens at this layer rather than inside
/// `services::mal::fetch_animelist` because refresh requires writing
/// the new tokens back to `external_accounts`, which the model
/// layer owns. On 401: refresh, persist, retry the fetch once. A
/// second 401 returns an error rather than looping forever — that
/// shape signals "user must re-link" and the next tick won't fix
/// it.
async fn sync_mal_dryrun(
    state: &AppState,
    account: external_accounts::ExternalAccount,
) -> Result<String, String> {
    let mut access_token = account.access_token.clone();

    let entries = match mal::fetch_animelist(&access_token).await {
        Ok(entries) => entries,
        Err(mal::MalFetchError::Unauthorized) => {
            // Refresh the access token. If THIS fails (refresh token
            // dead or revoked), surface a clear "re-link required"
            // message; the eventual UI banner will read it.
            if account.refresh_token.is_empty() {
                return Err(
                    "MAL access token expired and no refresh token stored — re-link required"
                        .into(),
                );
            }
            let new_tokens = mal::refresh_access_token(&account.refresh_token)
                .await
                .map_err(|e| format!("MAL refresh failed (re-link required): {e}"))?;

            let expires_at = current_unix_ts() + new_tokens.expires_in;
            external_accounts::update_tokens(
                &state.db,
                account.id,
                &new_tokens.access_token,
                &new_tokens.refresh_token,
                Some(expires_at),
            )
            .await
            .map_err(|e| format!("persist refreshed MAL tokens: {e}"))?;

            logger::info(
                &state.db,
                LogCategory::ExternalSync,
                "MAL access token refreshed",
                &format!("account_id={} expires_at={}", account.id, expires_at),
            )
            .await;
            access_token = new_tokens.access_token;

            // Retry the fetch once with the new token. A second 401
            // here is a hard "re-link required" — the refresh
            // succeeded but the new token isn't accepted, which is
            // the failure mode you'd see if MAL revoked the OAuth
            // app or the user revoked their grant.
            mal::fetch_animelist(&access_token)
                .await
                .map_err(|e| match e {
                    mal::MalFetchError::Unauthorized => {
                        "MAL rejected the token immediately after refresh — re-link required".into()
                    }
                    mal::MalFetchError::Other(msg) => format!("MAL fetch failed: {msg}"),
                })?
        }
        Err(mal::MalFetchError::Other(msg)) => return Err(format!("MAL fetch failed: {msg}")),
    };

    let total = entries.len();
    let with_score = entries.iter().filter(|e| e.score > 0.0).count();

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "MyAnimeList watch-list fetched: {} entries ({} scored)",
            total, with_score
        ),
        &format!("username={}", account.username),
    )
    .await;

    Ok(format!(
        "MyAnimeList: fetched {} entries (merge lands in a follow-up commit)",
        total
    ))
}

fn current_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
