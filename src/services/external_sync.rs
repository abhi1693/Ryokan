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
use crate::models::external_accounts::{self, ImportPreferences};
use crate::models::log::LogCategory;
use crate::models::monitoring::MonitorMode;
use crate::services::{anilist, logger, mal};

// ── Provider-agnostic sync entry abstraction ──────────────────────

/// Provider list status normalized across AL and MAL. AL emits SHOUTY
/// (CURRENT, PLANNING, COMPLETED, DROPPED, PAUSED, REPEATING); MAL
/// emits snake_case (watching, completed, on_hold, dropped,
/// plan_to_watch). Mapping converges both onto this enum so the
/// merge engine + monitor-mode default lookup work the same way
/// regardless of which provider produced the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizedStatus {
    /// AL `CURRENT` / MAL `watching`. The active list — anything
    /// the user is mid-watch on.
    Watching,
    /// AL `PLANNING` / MAL `plan_to_watch`. Plan-to-watch.
    Planning,
    /// AL `PAUSED` / MAL `on_hold`. The user explicitly paused this.
    Paused,
    /// AL `DROPPED` / MAL `dropped`. The user gave up.
    Dropped,
    /// AL `COMPLETED` / MAL `completed`. The user finished it.
    Completed,
    /// AL `REPEATING` (re-watch). MAL doesn't have a distinct value
    /// for this so it never appears for MAL syncs; the engine treats
    /// it the same as Watching when mapping to monitor modes.
    Repeating,
}

impl NormalizedStatus {
    /// Parse AL's status string. Unknown values fall through to
    /// `Planning` because that's the safe default — it grabs nothing
    /// from the back catalog and only acts on future episodes once
    /// the user marks the series as Watching.
    pub fn from_anilist(s: &str) -> Self {
        match s {
            "CURRENT" => Self::Watching,
            "PLANNING" => Self::Planning,
            "PAUSED" => Self::Paused,
            "DROPPED" => Self::Dropped,
            "COMPLETED" => Self::Completed,
            "REPEATING" => Self::Repeating,
            _ => Self::Planning,
        }
    }

    /// Parse MAL's status string. Same safe-default fallback as the
    /// AL path.
    pub fn from_mal(s: &str) -> Self {
        match s {
            "watching" => Self::Watching,
            "plan_to_watch" => Self::Planning,
            "on_hold" => Self::Paused,
            "dropped" => Self::Dropped,
            "completed" => Self::Completed,
            _ => Self::Planning,
        }
    }
}

/// Provider-agnostic sync entry. Both AL and MAL adapters produce
/// these so the merge engine doesn't have to dispatch on provider
/// for each row.
#[derive(Debug, Clone)]
pub struct SyncEntry {
    /// Original provider, kept for diagnostic logging and the
    /// negated-AL-id sentinel decision below.
    pub provider: String,
    /// Provider's own media id. AL ID for AniList, MAL ID for
    /// MyAnimeList. The merge engine uses this for re-link
    /// idempotency on subsequent syncs.
    pub provider_media_id: i64,
    /// AniList ID resolved to the value we'd store on
    /// `series.anilist_id`. For AL entries, identical to
    /// `provider_media_id`. For MAL entries, this is `0` at this
    /// commit — resolution to a real AL ID (or the negated-MAL-id
    /// sentinel if no mapping exists) happens in the merge commit
    /// alongside the anibridge lookup.
    pub anilist_id: i64,
    /// Normalized list status across providers.
    pub status: NormalizedStatus,
    /// Episodes the user has marked watched.
    pub progress: i64,
    /// Score on the provider's scale; `0.0` means unrated. Render
    /// path NEVER displays "You: 0".
    pub score: f64,
    /// Unix epoch (seconds) of the entry's most-recent update on
    /// the provider. The merge engine filters by this against
    /// `external_accounts.list_last_synced_at` for delta sync.
    pub updated_at: i64,
    /// Names of provider-side custom lists this entry belongs to.
    /// AL-only — MAL has no custom-list concept (decision #5 cuts
    /// it from MAL scope). Always empty for MAL.
    pub custom_lists: Vec<String>,
}

/// Map a normalized status to the `MonitorMode` Ryokan should write
/// onto a freshly-imported series. Honors plan decisions #6 (PTW
/// defaults to `monitor-future` instead of the issue body's
/// `monitor-none`) and #7 (skip-already-watched is a per-account
/// opt-in that flips Watching → `monitor-existing`).
///
/// Status → monitor mode without skip-already-watched:
///   Watching, Repeating  → `all`     (grab back catalog + future)
///   Planning             → `future`  (decision #6 — let the user
///                                     start the show whenever, and
///                                     have the recent few episodes
///                                     ready)
///   Paused, Completed    → `existing` (don't grab future episodes;
///                                     leave anything already in
///                                     the library alone)
///   Dropped              → `none`    (track it exists, do nothing)
///
/// With skip_already_watched on: Watching → `existing` instead of
/// `all`, so the back catalog isn't re-grabbed for series the user
/// has already watched elsewhere. Other statuses unchanged —
/// skip-already-watched only matters for the active list.
pub fn monitor_mode_for(status: NormalizedStatus, skip_already_watched: bool) -> MonitorMode {
    match status {
        NormalizedStatus::Watching | NormalizedStatus::Repeating => {
            if skip_already_watched {
                MonitorMode::Existing
            } else {
                MonitorMode::All
            }
        }
        NormalizedStatus::Planning => MonitorMode::Future,
        NormalizedStatus::Paused | NormalizedStatus::Completed => MonitorMode::Existing,
        NormalizedStatus::Dropped => MonitorMode::None,
    }
}

/// True when the user's per-list import-preferences include this
/// status's bucket. Used by the AL/MAL adapters to drop entries
/// that the user said they don't want imported BEFORE we even spend
/// a row on them in the staging table.
pub fn import_status(status: NormalizedStatus, prefs: &ImportPreferences) -> bool {
    match status {
        NormalizedStatus::Watching | NormalizedStatus::Repeating => prefs.import_watching,
        NormalizedStatus::Planning => prefs.import_planning,
        NormalizedStatus::Paused => prefs.import_paused,
        NormalizedStatus::Dropped => prefs.import_dropped,
        NormalizedStatus::Completed => prefs.import_completed,
    }
}

/// Convert a vector of AniList watch-list entries into the
/// provider-agnostic [`SyncEntry`] shape. Drops entries whose status
/// isn't on the user's import list. AL's `media_id` is the AniList
/// id we'd use as `series.anilist_id`, so `anilist_id` and
/// `provider_media_id` are identical here.
pub fn entries_from_anilist(
    al_entries: Vec<anilist::AniListMediaListEntry>,
    prefs: &ImportPreferences,
) -> Vec<SyncEntry> {
    al_entries
        .into_iter()
        .filter_map(|e| {
            let status = NormalizedStatus::from_anilist(&e.status);
            if !import_status(status, prefs) {
                return None;
            }
            Some(SyncEntry {
                provider: external_accounts::PROVIDER_ANILIST.to_string(),
                provider_media_id: e.media_id,
                anilist_id: e.media_id,
                status,
                progress: e.progress,
                score: e.score,
                updated_at: e.updated_at,
                custom_lists: e.custom_lists,
            })
        })
        .collect()
}

/// Convert a vector of MyAnimeList watch-list entries into
/// [`SyncEntry`]. AL ID resolution (anibridge MAL→AL lookup, or the
/// negated-MAL-id sentinel on miss) happens at merge time, so this
/// commit leaves `anilist_id` at `0`. The merge engine treats `0`
/// as "needs resolution" and fills it in before writing to series.
pub fn entries_from_mal(
    mal_entries: Vec<mal::MalAnimeListEntry>,
    prefs: &ImportPreferences,
) -> Vec<SyncEntry> {
    mal_entries
        .into_iter()
        .filter_map(|e| {
            let status = NormalizedStatus::from_mal(&e.status);
            if !import_status(status, prefs) {
                return None;
            }
            Some(SyncEntry {
                provider: external_accounts::PROVIDER_MAL.to_string(),
                provider_media_id: e.media_id,
                // 0 means "needs resolution" — the merge step swaps
                // this for the real AL ID (or the negated-MAL-id
                // sentinel if anibridge has no mapping).
                anilist_id: 0,
                status,
                progress: e.progress,
                score: e.score,
                updated_at: e.updated_at,
                // MAL has no custom-list concept; field stays empty.
                custom_lists: Vec::new(),
            })
        })
        .collect()
}

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

    let raw = anilist::fetch_media_list_collection(&account.access_token, user_id).await?;
    let raw_total = raw.len();

    let prefs = ImportPreferences {
        import_watching: account.import_watching,
        import_planning: account.import_planning,
        import_paused: account.import_paused,
        import_dropped: account.import_dropped,
        import_completed: account.import_completed,
        skip_already_watched: account.skip_already_watched,
    };
    let entries = entries_from_anilist(raw, &prefs);
    let kept = entries.len();
    let dropped = raw_total - kept;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "AniList watch-list fetched: {kept} entries kept ({dropped} skipped per import preferences)"
        ),
        &format!("username={}", account.username),
    )
    .await;

    Ok(format!(
        "AniList: fetched {raw_total}, kept {kept} (merge lands in a follow-up commit)"
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

    let raw_total = entries.len();
    let prefs = ImportPreferences {
        import_watching: account.import_watching,
        import_planning: account.import_planning,
        import_paused: account.import_paused,
        import_dropped: account.import_dropped,
        import_completed: account.import_completed,
        skip_already_watched: account.skip_already_watched,
    };
    let normalized = entries_from_mal(entries, &prefs);
    let kept = normalized.len();
    let dropped = raw_total - kept;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "MyAnimeList watch-list fetched: {kept} entries kept ({dropped} skipped per import preferences)"
        ),
        &format!("username={}", account.username),
    )
    .await;

    Ok(format!(
        "MyAnimeList: fetched {raw_total}, kept {kept} (merge lands in a follow-up commit)"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs_default() -> ImportPreferences {
        // Watching + Planning on, the rest off — the plan-doc-decided
        // shape that fresh installs land at.
        ImportPreferences {
            import_watching: true,
            import_planning: true,
            import_paused: false,
            import_dropped: false,
            import_completed: false,
            skip_already_watched: false,
        }
    }

    #[test]
    fn anilist_status_strings_map_to_normalized() {
        assert_eq!(
            NormalizedStatus::from_anilist("CURRENT"),
            NormalizedStatus::Watching
        );
        assert_eq!(
            NormalizedStatus::from_anilist("PLANNING"),
            NormalizedStatus::Planning
        );
        assert_eq!(
            NormalizedStatus::from_anilist("PAUSED"),
            NormalizedStatus::Paused
        );
        assert_eq!(
            NormalizedStatus::from_anilist("DROPPED"),
            NormalizedStatus::Dropped
        );
        assert_eq!(
            NormalizedStatus::from_anilist("COMPLETED"),
            NormalizedStatus::Completed
        );
        assert_eq!(
            NormalizedStatus::from_anilist("REPEATING"),
            NormalizedStatus::Repeating
        );
        // Unknown values fall through to the safe Planning default
        // so a future AL enum addition doesn't accidentally route
        // entries to a destructive monitor mode.
        assert_eq!(
            NormalizedStatus::from_anilist("hypothetical_new_value"),
            NormalizedStatus::Planning
        );
    }

    #[test]
    fn mal_status_strings_map_to_normalized() {
        assert_eq!(
            NormalizedStatus::from_mal("watching"),
            NormalizedStatus::Watching
        );
        assert_eq!(
            NormalizedStatus::from_mal("plan_to_watch"),
            NormalizedStatus::Planning
        );
        assert_eq!(
            NormalizedStatus::from_mal("on_hold"),
            NormalizedStatus::Paused
        );
        assert_eq!(
            NormalizedStatus::from_mal("dropped"),
            NormalizedStatus::Dropped
        );
        assert_eq!(
            NormalizedStatus::from_mal("completed"),
            NormalizedStatus::Completed
        );
        // MAL has no `repeating` value; unknown strings fall through
        // to the safe Planning default.
        assert_eq!(
            NormalizedStatus::from_mal("garbage"),
            NormalizedStatus::Planning
        );
    }

    #[test]
    fn monitor_mode_for_status_matches_plan_decisions() {
        // Plan decisions #6 + #7 baked in. PTW → Future (NOT None,
        // overrides the issue body), Watching → All by default,
        // skip-already-watched flips Watching → Existing only.
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Watching, false),
            MonitorMode::All
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Repeating, false),
            MonitorMode::All
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Planning, false),
            MonitorMode::Future
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Paused, false),
            MonitorMode::Existing
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Completed, false),
            MonitorMode::Existing
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Dropped, false),
            MonitorMode::None
        );
    }

    #[test]
    fn skip_already_watched_flips_only_watching_to_existing() {
        // The skip toggle is meant for migration-from-streaming
        // users — they want NEW episodes only, not the back catalog.
        // It MUST NOT affect Planning (still Future), Paused (still
        // Existing), or any other status, because those bucket
        // semantics would change in user-surprising ways.
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Watching, true),
            MonitorMode::Existing
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Repeating, true),
            MonitorMode::Existing
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Planning, true),
            MonitorMode::Future
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Paused, true),
            MonitorMode::Existing
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Completed, true),
            MonitorMode::Existing
        );
        assert_eq!(
            monitor_mode_for(NormalizedStatus::Dropped, true),
            MonitorMode::None
        );
    }

    #[test]
    fn import_status_filters_by_per_list_preferences() {
        let prefs = prefs_default();
        // Default-on lists pass through.
        assert!(import_status(NormalizedStatus::Watching, &prefs));
        assert!(import_status(NormalizedStatus::Repeating, &prefs));
        assert!(import_status(NormalizedStatus::Planning, &prefs));
        // Default-off lists are dropped.
        assert!(!import_status(NormalizedStatus::Paused, &prefs));
        assert!(!import_status(NormalizedStatus::Dropped, &prefs));
        assert!(!import_status(NormalizedStatus::Completed, &prefs));

        // Flip a few flags and re-check.
        let mut prefs = prefs;
        prefs.import_watching = false;
        prefs.import_completed = true;
        assert!(!import_status(NormalizedStatus::Watching, &prefs));
        assert!(import_status(NormalizedStatus::Completed, &prefs));
        // Repeating tracks Watching's flag — they're the same bucket
        // for import purposes.
        assert!(!import_status(NormalizedStatus::Repeating, &prefs));
    }

    fn al_entry(media_id: i64, status: &str) -> anilist::AniListMediaListEntry {
        anilist::AniListMediaListEntry {
            media_id,
            status: status.to_string(),
            progress: 0,
            score: 0.0,
            updated_at: 0,
            notes: String::new(),
            custom_lists: Vec::new(),
        }
    }

    #[test]
    fn entries_from_anilist_drops_filtered_statuses_and_preserves_id() {
        let raw = vec![
            al_entry(1, "CURRENT"),
            al_entry(2, "PLANNING"),
            al_entry(3, "DROPPED"),   // default-off
            al_entry(4, "COMPLETED"), // default-off
        ];
        let entries = entries_from_anilist(raw, &prefs_default());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].provider_media_id, 1);
        assert_eq!(
            entries[0].anilist_id, 1,
            "AL provider_media_id == anilist_id"
        );
        assert_eq!(entries[0].status, NormalizedStatus::Watching);
        assert_eq!(entries[1].provider_media_id, 2);
        assert_eq!(entries[1].status, NormalizedStatus::Planning);
    }

    fn mal_entry(media_id: i64, status: &str) -> mal::MalAnimeListEntry {
        mal::MalAnimeListEntry {
            media_id,
            status: status.to_string(),
            progress: 0,
            score: 0.0,
            updated_at: 0,
        }
    }

    #[test]
    fn entries_from_mal_leaves_anilist_id_at_zero_for_resolution() {
        // The merge engine resolves MAL → AL via anibridge before
        // writing to series. Until then, anilist_id is the sentinel
        // 0 so a regression that skips the resolution step writes a
        // visibly-broken value rather than a silently-wrong one.
        let raw = vec![mal_entry(101, "watching"), mal_entry(102, "plan_to_watch")];
        let entries = entries_from_mal(raw, &prefs_default());
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(e.anilist_id, 0, "MAL anilist_id is 0 pre-resolution");
            assert_eq!(e.provider, external_accounts::PROVIDER_MAL);
        }
    }

    #[test]
    fn entries_from_mal_drops_filtered_statuses() {
        // dropped + on_hold + completed are default-off; only
        // watching + planning entries survive the filter.
        let raw = vec![
            mal_entry(1, "watching"),
            mal_entry(2, "on_hold"),
            mal_entry(3, "dropped"),
            mal_entry(4, "completed"),
            mal_entry(5, "plan_to_watch"),
        ];
        let entries = entries_from_mal(raw, &prefs_default());
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.provider_media_id == 1));
        assert!(entries.iter().any(|e| e.provider_media_id == 5));
    }
}
