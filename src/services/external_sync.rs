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
//! End-to-end fetch + merge for AniList watch lists, and AniList-
//! resolvable MyAnimeList watch lists (via `anibridge::lookup_anilist_by_mal`).
//! MAL entries that anibridge can't resolve are counted as
//! `deferred_jikan` and skipped — the Jikan-fallback path that writes
//! them under the negated-MAL-id sentinel (`-mal_id`) lands in a
//! follow-up commit. Still pending:
//!   1. Jikan-fallback merge for `deferred_jikan` entries.
//!   2. Bulk-mode coalescing for sync-originated adds (defer Jellyfin
//!      refresh + RSS sync until the first-sync drain completes).
//!   3. Delta cursor (`list_last_synced_at`) so a tick only refetches
//!      entries with `updatedAt > cursor`, plus a weekly full-resync
//!      backstop.
//!   4. Manual "Sync now" button + ProgressRegistry hook for the
//!      sticky-toast first-sync UI.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::external_accounts::{self, ImportPreferences};
use crate::models::log::LogCategory;
use crate::models::monitoring::MonitorMode;
use crate::models::series;
use crate::services::{anibridge, anilist, logger, mal, monitoring as monitoring_service};

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

/// Fill in `anilist_id` for MAL-sourced entries via anibridge lookup.
/// Entries that already carry a non-zero `anilist_id` (i.e. AL-sourced)
/// pass through unchanged.
///
/// On a successful MAL→AL lookup, sets `anilist_id` to the matching
/// AniList ID — this is the value the merge step writes to
/// `series.anilist_id`, which means SeaDex / AL-keyed scoring then
/// works for the entry the same way it would for a manually-added AL
/// series.
///
/// On a miss, falls back to the negated-MAL-id sentinel
/// (`anilist_id = -provider_media_id`) so the entry still lands in the
/// library and the existing reconcile-fallbacks flow can promote it
/// to a real AL ID later if anibridge gains a mapping.
///
/// **Caller is responsible for ensuring the anibridge cache is loaded
/// first** (typically via `anibridge::ensure_loaded().await`). This
/// function only reads — it never triggers a download. Splitting the
/// load and the lookup keeps tests deterministic: they can seed the
/// cache directly without racing the real network fetch.
pub async fn resolve_mal_anilist_ids(mut entries: Vec<SyncEntry>) -> Vec<SyncEntry> {
    for entry in &mut entries {
        if entry.anilist_id != 0 {
            // AL-sourced: provider_media_id IS the AL id. Already set.
            continue;
        }
        match anibridge::lookup_anilist_by_mal(entry.provider_media_id).await {
            Some(al_id) => entry.anilist_id = al_id,
            // Negated-MAL-id sentinel — matches the existing
            // services::jikan fallback convention so reconcile and
            // every AL-id-filtered query keeps its `> 0` guard.
            None => entry.anilist_id = -entry.provider_media_id,
        }
    }
    entries
}

// ── Series merge ──────────────────────────────────────────────────

/// Aggregate result from a `merge_into_library` call. Each counter
/// tracks one outcome category so the supervised-loop summary line +
/// future "Sync now" UI both have a single number to render per
/// bucket. `failed` holds the per-entry errors so the operator can
/// see specifically which AL ids didn't merge (most often: the
/// detail-fetch returned no payload for that id, e.g. an AL deletion
/// the user's list still references).
#[derive(Debug, Default, Clone)]
pub struct MergeOutcome {
    /// Series rows freshly inserted by this merge run.
    pub created: i32,
    /// Series rows that already existed and whose stored monitor_mode
    /// differed from the target — bumped to the new mode.
    pub monitor_mode_updated: i32,
    /// Series rows that already existed and whose monitor_mode already
    /// matched the target — left untouched.
    pub unchanged: i32,
    /// MAL-sourced entries whose anibridge lookup missed; merging them
    /// requires the Jikan-fallback path (negated-id sentinel + Jikan
    /// metadata fetch). Counted here for visibility; the actual Jikan
    /// merge lands in a follow-up commit.
    pub deferred_jikan: i32,
    /// Per-entry failures: `(anilist_id, error message)`. The merge
    /// keeps going on a single-row failure rather than aborting; one
    /// AL id deleted upstream shouldn't block the other 199 entries
    /// from importing.
    pub failed: Vec<(i64, String)>,
}

/// Merge a batch of [`SyncEntry`] into the local `series` table.
///
/// Caller is responsible for fetching `detail_map` (typically via
/// `anilist::get_anime_details_batch`) for every NEW positive AL id
/// in `entries`. Existing series don't need a detail entry — the
/// merge only updates `monitor_mode`, leaving cached metadata alone.
///
/// Decision flow per entry:
///   1. anilist_id <= 0  → deferred_jikan += 1, skip (Jikan path TBD).
///   2. series exists and monitor_mode == target → unchanged.
///   3. series exists and monitor_mode != target → apply_monitor_mode,
///      monitor_mode_updated += 1.
///   4. series doesn't exist + detail_map has it → upsert with full
///      core, then apply target monitor_mode. created += 1.
///   5. series doesn't exist + no detail in map → failed entry.
///
/// `apply_monitor_mode` runs `recompute_series_monitoring` as a side
/// effect, so monitoring rows get rebuilt for both new and changed
/// entries. The metadata-cache hydration + per-series classify scan
/// that the interactive add path triggers are intentionally NOT
/// triggered here — bulk-mode coalescing in a follow-up commit will
/// batch them so a 200-series first sync doesn't fan out 200 spawned
/// background tasks.
pub async fn merge_into_library(
    db: &SqlitePool,
    entries: &[SyncEntry],
    detail_map: &HashMap<i64, anilist::AnimeDetail>,
    skip_already_watched: bool,
) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();

    for entry in entries {
        if entry.anilist_id <= 0 {
            outcome.deferred_jikan += 1;
            continue;
        }
        let target_mode = monitor_mode_for(entry.status, skip_already_watched);
        match merge_one_anilist_entry(db, entry, target_mode, detail_map).await {
            Ok(MergeAction::Created) => outcome.created += 1,
            Ok(MergeAction::MonitorUpdated) => outcome.monitor_mode_updated += 1,
            Ok(MergeAction::Unchanged) => outcome.unchanged += 1,
            Err(msg) => outcome.failed.push((entry.anilist_id, msg)),
        }
    }
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeAction {
    Created,
    MonitorUpdated,
    Unchanged,
}

async fn merge_one_anilist_entry(
    db: &SqlitePool,
    entry: &SyncEntry,
    target_mode: MonitorMode,
    detail_map: &HashMap<i64, anilist::AnimeDetail>,
) -> Result<MergeAction, String> {
    let existing = series::get_by_anilist_id(db, entry.anilist_id)
        .await
        .map_err(|e| format!("series lookup failed: {e}"))?;

    if let Some(row) = existing {
        if row.monitor_mode == target_mode.as_str() {
            return Ok(MergeAction::Unchanged);
        }
        monitoring_service::apply_monitor_mode(db, row.id, target_mode).await?;
        return Ok(MergeAction::MonitorUpdated);
    }

    let detail = detail_map.get(&entry.anilist_id).ok_or_else(|| {
        // Most common cause: AL deleted/merged the entry but the
        // user's list still references it. Surface explicitly so the
        // operator knows it isn't a DB error.
        "no AniList detail returned for this id (deleted upstream?)".to_string()
    })?;

    let primary_title = if !detail.title_english.trim().is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    let (series_id, _created) = series::upsert(
        db,
        series::SeriesCore {
            anilist_id: entry.anilist_id,
            mal_id: detail.id_mal,
            title: primary_title,
            title_romaji: &detail.title_romaji,
            title_english: &detail.title_english,
            title_native: &detail.title_native,
            cover_url: &detail.cover_url,
            format: &detail.format,
            status: &detail.status,
            episodes: detail.episodes,
            season_year: detail.season_year,
            end_year: detail.end_year,
        },
    )
    .await
    .map_err(|e| format!("series upsert failed: {e}"))?;

    monitoring_service::apply_monitor_mode(db, series_id, target_mode).await?;
    Ok(MergeAction::Created)
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
        external_accounts::PROVIDER_ANILIST => sync_anilist(state, &account).await,
        external_accounts::PROVIDER_MAL => sync_mal(state, account).await,
        other => {
            // Unknown provider string — schema CHECK constraint should
            // prevent this, but surface explicitly rather than panic.
            Err(format!("unknown external_accounts.provider: {other}"))
        }
    }
}

/// Fetch the AL watch list and merge entries into the library. AL
/// is the simpler path: every entry's `media_id` is already the AL
/// ID we'd write to `series.anilist_id`, so no anibridge resolution
/// step is needed. Bulk-mode coalescing for the metadata-cache
/// hydration + classify-scan side effects lands in a follow-up
/// commit; for now the merge step does the upsert + monitor_mode
/// write only.
async fn sync_anilist(
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

    // Pre-fetch AnimeDetail for the AL ids that aren't already in the
    // local series table. Existing rows skip the fetch — the merge
    // step only touches monitor_mode for those.
    let new_ids = ids_needing_detail_fetch(&state.db, &entries).await;
    let detail_map = if new_ids.is_empty() {
        HashMap::new()
    } else {
        anilist::get_anime_details_batch(&new_ids)
            .await
            .map_err(|e| format!("AniList detail batch fetch failed: {e}"))?
    };

    let outcome =
        merge_into_library(&state.db, &entries, &detail_map, prefs.skip_already_watched).await;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "AniList watch-list synced: {kept} kept ({dropped} skipped), {} created, {} monitor-mode updated, {} unchanged, {} failed",
            outcome.created,
            outcome.monitor_mode_updated,
            outcome.unchanged,
            outcome.failed.len(),
        ),
        &format!(
            "username={} fetched_total={raw_total}",
            account.username
        ),
    )
    .await;
    log_failed_entries(&state.db, &outcome).await;

    Ok(format!(
        "AniList: fetched {raw_total}, kept {kept}, created {}, updated {}, unchanged {}, failed {}",
        outcome.created,
        outcome.monitor_mode_updated,
        outcome.unchanged,
        outcome.failed.len(),
    ))
}

/// Return the AL ids in `entries` that don't yet have a `series` row
/// — the set we need to fetch full AnimeDetail for before merging.
/// Existing rows can skip the network entirely; we only edit their
/// monitor_mode.
async fn ids_needing_detail_fetch(db: &SqlitePool, entries: &[SyncEntry]) -> Vec<i64> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.anilist_id <= 0 {
            continue;
        }
        if matches!(
            series::get_by_anilist_id(db, entry.anilist_id).await,
            Ok(None)
        ) {
            out.push(entry.anilist_id);
        }
    }
    out
}

/// Pump per-entry merge failures into the dedicated AniList log
/// category so the operator can see specifically which ids failed.
/// Capped to the first 10 to keep one bad list from spamming the
/// `logs` table — the count in the summary line still covers the
/// total.
async fn log_failed_entries(db: &SqlitePool, outcome: &MergeOutcome) {
    for (id, msg) in outcome.failed.iter().take(10) {
        logger::warn(
            db,
            LogCategory::ExternalSync,
            &format!("Watch-list merge failed for AL id {id}"),
            msg,
        )
        .await;
    }
}

/// Fetch the MAL watch list, resolve anibridge MAL→AL where possible,
/// and merge the resolved entries. Entries whose anibridge lookup
/// missed are counted in `outcome.deferred_jikan` and skipped — the
/// Jikan-fallback path that writes them under the negated-MAL-id
/// sentinel lands in a follow-up commit.
///
/// Token-refresh happens at this layer rather than inside
/// `services::mal::fetch_animelist` because refresh requires writing
/// the new tokens back to `external_accounts`, which the model
/// layer owns. On 401: refresh, persist, retry the fetch once. A
/// second 401 returns an error rather than looping forever — that
/// shape signals "user must re-link" and the next tick won't fix
/// it.
async fn sync_mal(
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

    // Resolve MAL → AL via anibridge. Misses fall back to the
    // negated-MAL-id sentinel, which the merge step skips for now
    // (counted as deferred_jikan).
    let _ = anibridge::ensure_loaded().await;
    let resolved = resolve_mal_anilist_ids(normalized).await;

    let new_ids = ids_needing_detail_fetch(&state.db, &resolved).await;
    let detail_map = if new_ids.is_empty() {
        HashMap::new()
    } else {
        anilist::get_anime_details_batch(&new_ids)
            .await
            .map_err(|e| format!("AniList detail batch fetch failed: {e}"))?
    };

    let outcome = merge_into_library(
        &state.db,
        &resolved,
        &detail_map,
        prefs.skip_already_watched,
    )
    .await;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "MyAnimeList watch-list synced: {kept} kept ({dropped} skipped), {} created, {} monitor-mode updated, {} unchanged, {} deferred (no anibridge mapping), {} failed",
            outcome.created,
            outcome.monitor_mode_updated,
            outcome.unchanged,
            outcome.deferred_jikan,
            outcome.failed.len(),
        ),
        &format!(
            "username={} fetched_total={raw_total}",
            account.username
        ),
    )
    .await;
    log_failed_entries(&state.db, &outcome).await;

    Ok(format!(
        "MyAnimeList: fetched {raw_total}, kept {kept}, created {}, updated {}, unchanged {}, deferred {}, failed {}",
        outcome.created,
        outcome.monitor_mode_updated,
        outcome.unchanged,
        outcome.deferred_jikan,
        outcome.failed.len(),
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

    /// The anibridge CACHE is process-global, so the three async
    /// resolver tests below have to serialize their seed→lookup→clear
    /// sequences or they race each other. A static Mutex held for the
    /// duration of each test is the simplest reliable guard; using
    /// `tokio::sync::Mutex` (not std) so awaits inside the critical
    /// section don't deadlock on a parking-lot lock.
    static ANIBRIDGE_CACHE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    fn make_detail(
        id: i64,
        title_english: &str,
        format: &str,
        status: &str,
    ) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: title_english.to_string(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: format.to_string(),
            status: status.to_string(),
            status_display: status.to_string(),
            episodes: Some(12),
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn entry(provider: &str, anilist_id: i64, status: NormalizedStatus) -> SyncEntry {
        SyncEntry {
            provider: provider.to_string(),
            provider_media_id: anilist_id.unsigned_abs() as i64,
            anilist_id,
            status,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        }
    }

    #[tokio::test]
    async fn merge_creates_new_series_with_resolved_monitor_mode() {
        let db = crate::test_support::in_memory_pool().await;
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Watching,
        )];
        let mut detail_map = HashMap::new();
        detail_map.insert(12345, make_detail(12345, "Example", "TV", "RELEASING"));

        let outcome = merge_into_library(&db, &entries, &detail_map, false).await;
        assert_eq!(outcome.created, 1);
        assert_eq!(outcome.monitor_mode_updated, 0);
        assert_eq!(outcome.unchanged, 0);
        assert!(outcome.failed.is_empty());

        // Watching + skip_already_watched=false → monitor_mode = "all"
        let row = series::get_by_anilist_id(&db, 12345)
            .await
            .unwrap()
            .expect("series row should exist");
        assert_eq!(row.monitor_mode, MonitorMode::All.as_str());
        assert_eq!(row.title_english, "Example");
    }

    #[tokio::test]
    async fn merge_updates_existing_series_when_monitor_mode_differs() {
        let db = crate::test_support::in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Example").await;
        // Default seed leaves monitor_mode empty; set it to a known
        // starting value so we can prove the merge changed it.
        sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
            .bind(MonitorMode::Future.as_str())
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();

        // No detail map needed — series already exists.
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Watching,
        )];
        let outcome = merge_into_library(&db, &entries, &HashMap::new(), false).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.monitor_mode_updated, 1);
        assert_eq!(outcome.unchanged, 0);

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert_eq!(row.monitor_mode, MonitorMode::All.as_str());
    }

    #[tokio::test]
    async fn merge_leaves_existing_series_alone_when_monitor_mode_matches() {
        let db = crate::test_support::in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Example").await;
        sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
            .bind(MonitorMode::All.as_str())
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();

        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Watching,
        )];
        let outcome = merge_into_library(&db, &entries, &HashMap::new(), false).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.monitor_mode_updated, 0);
        assert_eq!(outcome.unchanged, 1);
    }

    #[tokio::test]
    async fn merge_defers_negated_id_entries_for_jikan_path() {
        let db = crate::test_support::in_memory_pool().await;
        // -7777 means anibridge missed; the Jikan-fallback merge path
        // (next commit) will handle these. For now they're counted
        // and skipped.
        let entries = vec![entry(
            external_accounts::PROVIDER_MAL,
            -7777,
            NormalizedStatus::Watching,
        )];
        let outcome = merge_into_library(&db, &entries, &HashMap::new(), false).await;
        assert_eq!(outcome.deferred_jikan, 1);
        assert_eq!(outcome.created, 0);
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn merge_records_failure_when_detail_missing_for_new_id() {
        let db = crate::test_support::in_memory_pool().await;
        // AL id present in entries but absent from detail_map
        // (AL deleted the entry upstream is the canonical case).
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            99999,
            NormalizedStatus::Watching,
        )];
        let outcome = merge_into_library(&db, &entries, &HashMap::new(), false).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, 99999);
        assert!(outcome.failed[0].1.contains("no AniList detail"));
    }

    #[tokio::test]
    async fn merge_skip_already_watched_lands_existing_for_watching_only() {
        let db = crate::test_support::in_memory_pool().await;
        let entries = vec![
            entry(
                external_accounts::PROVIDER_ANILIST,
                100,
                NormalizedStatus::Watching,
            ),
            entry(
                external_accounts::PROVIDER_ANILIST,
                200,
                NormalizedStatus::Planning,
            ),
        ];
        let mut detail_map = HashMap::new();
        detail_map.insert(100, make_detail(100, "Active", "TV", "RELEASING"));
        detail_map.insert(200, make_detail(200, "PTW", "TV", "FINISHED"));

        let outcome = merge_into_library(&db, &entries, &detail_map, true).await;
        assert_eq!(outcome.created, 2);

        let watching = series::get_by_anilist_id(&db, 100).await.unwrap().unwrap();
        let planning = series::get_by_anilist_id(&db, 200).await.unwrap().unwrap();
        assert_eq!(
            watching.monitor_mode,
            MonitorMode::Existing.as_str(),
            "skip_already_watched flips Watching → existing"
        );
        assert_eq!(
            planning.monitor_mode,
            MonitorMode::Future.as_str(),
            "Planning still maps to future regardless of skip flag"
        );
    }

    #[tokio::test]
    async fn resolve_mal_anilist_ids_uses_anibridge_hit() {
        // Cache-hit path: MAL 1234 → AL 9999 lives in the seeded
        // anibridge cache, so the resolver writes the real AL id back
        // onto the SyncEntry.
        let _guard = ANIBRIDGE_CACHE_GUARD.lock().await;
        anibridge::seed_mal_to_anilist_for_tests(&[(1234, 9999)]).await;

        let entries = vec![SyncEntry {
            provider: external_accounts::PROVIDER_MAL.to_string(),
            provider_media_id: 1234,
            anilist_id: 0,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        }];
        let resolved = resolve_mal_anilist_ids(entries).await;
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].anilist_id, 9999,
            "anibridge hit should set anilist_id to the real AL id"
        );

        anibridge::clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn resolve_mal_anilist_ids_falls_back_to_negated_sentinel_on_miss() {
        // Empty cache → every lookup misses, every MAL entry gets
        // anilist_id = -provider_media_id. This is the reconcile-
        // path-friendly state from the existing Jikan fallback flow.
        let _guard = ANIBRIDGE_CACHE_GUARD.lock().await;
        anibridge::seed_mal_to_anilist_for_tests(&[]).await;

        let entries = vec![
            SyncEntry {
                provider: external_accounts::PROVIDER_MAL.to_string(),
                provider_media_id: 7777,
                anilist_id: 0,
                status: NormalizedStatus::Watching,
                progress: 0,
                score: 0.0,
                updated_at: 0,
                custom_lists: Vec::new(),
            },
            SyncEntry {
                provider: external_accounts::PROVIDER_MAL.to_string(),
                provider_media_id: 8888,
                anilist_id: 0,
                status: NormalizedStatus::Planning,
                progress: 0,
                score: 0.0,
                updated_at: 0,
                custom_lists: Vec::new(),
            },
        ];
        let resolved = resolve_mal_anilist_ids(entries).await;
        assert_eq!(resolved[0].anilist_id, -7777);
        assert_eq!(resolved[1].anilist_id, -8888);

        anibridge::clear_cache_for_tests().await;
    }

    #[tokio::test]
    async fn resolve_mal_anilist_ids_passes_through_anilist_entries_unchanged() {
        // AL entries (anilist_id != 0) MUST NOT be touched even if a
        // MAL ID with the same numeric value happens to live in the
        // cache. Otherwise an AL entry whose AL id collides with some
        // MAL id would be silently rewritten.
        let _guard = ANIBRIDGE_CACHE_GUARD.lock().await;
        anibridge::seed_mal_to_anilist_for_tests(&[(1234, 9999)]).await;

        let entries = vec![SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: 1234,
            anilist_id: 1234,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        }];
        let resolved = resolve_mal_anilist_ids(entries).await;
        assert_eq!(
            resolved[0].anilist_id, 1234,
            "AL pass-through must not be rewritten"
        );

        anibridge::clear_cache_for_tests().await;
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
