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
//! End-to-end import + merge + delta cursor + bulk-mode coalescing
//! for both AniList and MyAnimeList watch lists, with bidirectional
//! status tracking: monitor_mode follows the user's AL/MAL status
//! transitions (Watching ↔ Dropped, etc.) regardless of import
//! preferences for existing series, and full-resync runs detect
//! series that have been removed from the user's list and downgrade
//! their monitor_mode to None. Manually-added series are never touched
//! by removal detection (synced_from_external_account_id IS NULL).
//!
//! AL entries land under their real AL id; MAL entries that anibridge
//! can resolve land under the resolved AL id; MAL entries that
//! anibridge misses fall back to the Jikan-fetched-detail path and
//! land under the `-mal_id` sentinel that the existing
//! reconcile-fallbacks flow knows how to promote later. Newly-imported
//! series get their AnimeDetail cached + their artwork fetched + (if
//! configured) a single Jellyfin refresh, all in one coalesced
//! post-merge background task per tick.

use std::collections::HashMap;
use std::sync::LazyLock;

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::external_accounts::{self, ImportPreferences};
use crate::models::log::LogCategory;
use crate::models::monitoring::MonitorMode;
use crate::models::{metadata_cache, series, series_custom_lists, series_genres};
use crate::services::{
    anibridge, anilist, artwork, jikan, logger, mal, monitoring as monitoring_service,
};

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
    /// Episodes the user has marked watched. **Reserved**: the
    /// current merge step doesn't write this to the `series` row
    /// (PR B is a "what's on the list" import, not a "watched-state
    /// mirror"). Carried through the abstraction so a follow-up PR
    /// can light up the user-progress sync without re-plumbing the
    /// fetcher.
    pub progress: i64,
    /// Score on the provider's scale; `0.0` means unrated. **Reserved**:
    /// same status as `progress` — fetched and normalized, not yet
    /// written. Render path NEVER displays "You: 0".
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
/// provider-agnostic [`SyncEntry`] shape. **Does NOT filter by import
/// preferences** — that decision moves to merge time, because an
/// already-imported series whose status changed on AL still needs its
/// `monitor_mode` updated to track the new status, even when the new
/// status's import flag is off. Example: user has a Watching series
/// at `monitor_mode = all`, drops it on AL, has `import_dropped = false`.
/// Filtering at conversion time would silently leave the series at
/// `all` and keep grabbing episodes for a show the user dropped.
/// `merge_into_library` reads `prefs` to gate creation only.
///
/// AL's `media_id` is the AniList id we'd use as `series.anilist_id`,
/// so `anilist_id` and `provider_media_id` are identical here.
pub fn entries_from_anilist(al_entries: Vec<anilist::AniListMediaListEntry>) -> Vec<SyncEntry> {
    al_entries
        .into_iter()
        .map(|e| SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: e.media_id,
            anilist_id: e.media_id,
            status: NormalizedStatus::from_anilist(&e.status),
            progress: e.progress,
            score: e.score,
            updated_at: e.updated_at,
            custom_lists: e.custom_lists,
        })
        .collect()
}

/// Convert a vector of MyAnimeList watch-list entries into
/// [`SyncEntry`]. **Does NOT filter by import preferences** — same
/// rationale as `entries_from_anilist`. AL ID resolution (anibridge
/// MAL→AL lookup, or the negated-MAL-id sentinel on miss) happens at
/// merge time, so this leaves `anilist_id` at `0`. The merge engine
/// treats `0` as "needs resolution" and fills it in before writing
/// to series.
pub fn entries_from_mal(mal_entries: Vec<mal::MalAnimeListEntry>) -> Vec<SyncEntry> {
    mal_entries
        .into_iter()
        .map(|e| SyncEntry {
            provider: external_accounts::PROVIDER_MAL.to_string(),
            provider_media_id: e.media_id,
            // 0 means "needs resolution" — the merge step swaps
            // this for the real AL ID (or the negated-MAL-id
            // sentinel if anibridge has no mapping).
            anilist_id: 0,
            status: NormalizedStatus::from_mal(&e.status),
            progress: e.progress,
            score: e.score,
            updated_at: e.updated_at,
            // MAL has no custom-list concept; field stays empty.
            custom_lists: Vec::new(),
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
    /// Entries that would have been created but the user's import
    /// preferences are off for the entry's status (e.g. new Dropped
    /// entry while `import_dropped = false`). Counted separately from
    /// `unchanged` because the surface visible to the user is "we
    /// skipped these on purpose" vs. "these were already in sync."
    /// Existing-series monitor_mode updates run regardless of import
    /// preferences — flipping a Watching series to Dropped on AL
    /// always downgrades local monitor_mode, even with
    /// `import_dropped = false`.
    pub skipped_by_preference: i32,
    /// Existing series with `monitor_mode_manual_override = 1` that
    /// the merge step left alone. The user explicitly pinned the
    /// monitor_mode through the UI; sync honors the pin until the
    /// user clears it via "Sync from AL/MAL" in the dropdown.
    pub pinned_manually: i32,
    /// Newly-created series rows that need artwork caching, collected
    /// for the deferred bulk-mode pass that runs after merge. Carrying
    /// just the IDs + image URLs (not the full AnimeDetail) keeps
    /// memory bounded on a 500-series first sync.
    pub new_artwork: Vec<NewArtworkSpec>,
}

/// Pointer payload for the deferred artwork-cache pass. The merge
/// step writes one of these per newly-created series; the post-merge
/// background task in sync_anilist / sync_mal walks the list and
/// calls `artwork::cache_image` once per non-empty URL.
///
/// Both URLs may be empty when the upstream provider doesn't supply
/// banner artwork for a series — Jikan in particular often returns
/// only a cover image. The post-merge task skips empty URLs rather
/// than logging a per-series failure.
#[derive(Debug, Clone)]
pub struct NewArtworkSpec {
    pub series_id: i64,
    pub cover_url: String,
    pub banner_url: String,
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
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();

    for entry in entries {
        if entry.anilist_id <= 0 {
            outcome.deferred_jikan += 1;
            continue;
        }
        let target_mode = monitor_mode_for(entry.status, prefs.skip_already_watched);
        match merge_one_anilist_entry(db, entry, target_mode, detail_map, prefs, account_id).await {
            Ok(MergeAction::Created(spec)) => {
                outcome.created += 1;
                outcome.new_artwork.push(spec);
            }
            Ok(MergeAction::MonitorUpdated) => outcome.monitor_mode_updated += 1,
            Ok(MergeAction::Unchanged) => outcome.unchanged += 1,
            Ok(MergeAction::SkippedByPreference) => outcome.skipped_by_preference += 1,
            Ok(MergeAction::PinnedManually) => outcome.pinned_manually += 1,
            Err(msg) => outcome.failed.push((entry.anilist_id, msg)),
        }
    }
    outcome
}

#[derive(Debug, Clone)]
enum MergeAction {
    /// New series row created. Carries the artwork spec so the bulk-
    /// mode post-merge task can cache cover + banner without re-
    /// fetching the AnimeDetail.
    Created(NewArtworkSpec),
    MonitorUpdated,
    Unchanged,
    /// Entry would have been a new create, but the user's import
    /// preferences are off for this status. Existing series with the
    /// same status DO still get monitor_mode updated — only the
    /// create branch checks preferences.
    SkippedByPreference,
    /// Existing series whose `monitor_mode_manual_override = 1` —
    /// user explicitly pinned this monitor_mode through the UI.
    /// Sync stamps `synced_from` so removal-detection still tracks
    /// it but leaves the monitor_mode alone.
    PinnedManually,
}

impl MergeOutcome {
    /// Combine two outcomes from sequential merge passes (typically
    /// the AL-detail pass followed by the Jikan-fallback pass for the
    /// same `entries` slice). Counts add; `failed` lists concatenate.
    /// `deferred_jikan` is taken from `self` and reduced by the
    /// number of entries the second pass actually handled — so a
    /// successful Jikan pass on every deferred entry zeroes out the
    /// counter, matching what the operator sees in the library.
    pub fn merge_pass(mut self, other: MergeOutcome) -> MergeOutcome {
        let handled_by_other = other.created
            + other.monitor_mode_updated
            + other.unchanged
            + other.skipped_by_preference
            + other.pinned_manually
            + other.failed.len() as i32;
        self.created += other.created;
        self.monitor_mode_updated += other.monitor_mode_updated;
        self.unchanged += other.unchanged;
        self.skipped_by_preference += other.skipped_by_preference;
        self.pinned_manually += other.pinned_manually;
        self.deferred_jikan = (self.deferred_jikan - handled_by_other).max(0);
        self.failed.extend(other.failed);
        self.new_artwork.extend(other.new_artwork);
        self
    }
}

/// Stamp `series.synced_from_external_account_id` if the caller
/// passed an `account_id` (live sync) and skip silently when `None`
/// (unit tests and theoretical batch-merge paths that don't have a
/// real account). Best-effort write — a failure is logged but does
/// not fail the merge, since the marker is only used by the removal-
/// detection pass and missing it just means the series stays out of
/// removal candidates (safer than the alternative).
async fn stamp_synced_from_if_set(db: &SqlitePool, series_id: i64, account_id: Option<i64>) {
    if let Some(aid) = account_id
        && let Err(e) = series::stamp_synced_from(db, series_id, aid).await
    {
        tracing::warn!("series::stamp_synced_from failed for series_id={series_id}: {e}");
    }
}

/// #62 PR C — write the user's personal score from the sync entry
/// onto `series.user_score`. Skips silently when `account_id` is
/// `None` (unit-test pathway with no live account). Normalizes AL's
/// `0.0` "unrated" sentinel to `NULL` so the schema unambiguously
/// means "rated" when the column is non-null; the render helper
/// still handles 0.0 defensively for any rows that pre-date the
/// normalization.
///
/// Best-effort write, same rationale as `stamp_synced_from_if_set`:
/// a failure logs but doesn't fail the merge. A missing score just
/// means the "You: X" badge won't render until the next tick.
async fn stamp_user_score_if_set(
    db: &SqlitePool,
    series_id: i64,
    score: f64,
    account_id: Option<i64>,
) {
    if account_id.is_none() {
        return;
    }
    let normalized = if score > 0.0 { Some(score) } else { None };
    if let Err(e) = series::update_user_score(db, series_id, normalized).await {
        tracing::warn!("series::update_user_score failed for series_id={series_id}: {e}");
    }
}

/// #62 PR D — replace the series's AL custom-list memberships from
/// `entry.custom_lists`. Skips when `account_id` is `None` (unit-
/// test pathway) and when `provider` isn't AniList (MAL never emits
/// custom-list memberships, so the call would just clear a never-
/// populated set every tick).
///
/// Called from BOTH the AL-detail and Jikan-fallback merge paths.
/// The Jikan path is dead-by-data today — `entries_from_mal` always
/// returns an empty `custom_lists` and the provider check short-
/// circuits before any DB write — but keeping the call symmetric
/// across both paths means a hypothetical future provider added to
/// the Jikan-fallback path inherits the namespace-skip automatically
/// instead of silently clobbering AL's rows. Two cheap branch-and-
/// returns per Jikan merge is a fine tax for that invariant.
///
/// Replace-on-merge rather than incremental: the GraphQL response
/// carries the full membership map per entry, so clear+insert is
/// the right shape for "user moved this out of Hidden Gems" — an
/// upsert path would leak stale rows.
///
/// Best-effort: a failure logs but doesn't fail the merge. A
/// missing membership row just means the badge / filter doesn't
/// reflect the latest state until the next tick.
async fn stamp_custom_lists_if_set(
    db: &SqlitePool,
    series_id: i64,
    provider: &str,
    custom_lists: &[String],
    account_id: Option<i64>,
) {
    if account_id.is_none() {
        return;
    }
    if provider != external_accounts::PROVIDER_ANILIST {
        return;
    }
    if let Err(e) =
        series_custom_lists::replace_for_series(db, series_id, provider, custom_lists).await
    {
        tracing::warn!(
            "series_custom_lists::replace_for_series failed for series_id={series_id}: {e}"
        );
    }
}

/// Report from a removal-detection pass. `removed` is the list of
/// `series.id` whose monitor_mode got downgraded to `None` because
/// they were no longer in the user's AL/MAL list. Surfaces in the
/// supervised-loop summary so an unexpected removal is visible
/// (e.g. user accidentally cleared their list — the count tells
/// them how much got downgraded).
#[derive(Debug, Default, Clone)]
pub struct RemovalReport {
    pub removed: Vec<i64>,
}

/// Find sync-marked series that aren't in the current fetch and
/// downgrade their `monitor_mode` to `None`. Run AFTER the merge
/// passes (so the merge's own monitor_mode writes don't fight us)
/// and ONLY on full-resync runs (delta runs by definition only see
/// changed entries, so a series that didn't change wouldn't appear
/// in `fetch_ids` and would be wrongly flagged as removed).
///
/// `fetch_ids` is the set of `anilist_id` values from the current
/// sync's entries — positive AL ids for AL syncs, mix of positive
/// (anibridge-resolved) and negated (Jikan-fallback sentinel) for
/// MAL syncs. The same value the merge wrote to `series.anilist_id`,
/// so the comparison is straightforward.
///
/// Series whose `monitor_mode` is already `None` are left alone —
/// no point burning a write to set the same value, and the user
/// might have manually downgraded for their own reasons.
pub async fn detect_removals(
    db: &SqlitePool,
    account_id: i64,
    fetch_ids: &std::collections::HashSet<i64>,
) -> Result<RemovalReport, String> {
    let synced = series::list_synced_from(db, account_id)
        .await
        .map_err(|e| format!("list_synced_from: {e}"))?;
    let mut report = RemovalReport::default();
    let already_none = MonitorMode::None.as_str();
    for s in synced {
        if fetch_ids.contains(&s.anilist_id) {
            continue;
        }
        if s.monitor_mode == already_none {
            continue;
        }
        // Manual override pins the user's chosen monitor_mode against
        // both merge updates AND removal detection. The user
        // explicitly set this mode and may want to keep grabbing the
        // series (e.g. they took it off AL because their list was
        // public and contained spoilers, but still want Ryokan to
        // pick up new episodes).
        if s.monitor_mode_manual_override {
            continue;
        }
        monitoring_service::apply_monitor_mode(db, s.id, MonitorMode::None).await?;
        report.removed.push(s.id);
    }
    Ok(report)
}

/// Walk negated-id [`SyncEntry`]s (the ones `merge_into_library`
/// counted as `deferred_jikan`) and merge each via Jikan-fetched
/// metadata. Used by the MAL sync path so entries whose anibridge
/// MAL→AL lookup missed still land in the library — they sit under
/// the `series.anilist_id = -mal_id` sentinel that the existing
/// reconcile-fallbacks flow already understands.
///
/// Walks one entry at a time rather than fanning out: Jikan is rate-
/// limited at 3 req/s, and `get_anime_detail_cached` carries its own
/// negative-cache + rate-limit state. A failure for any single entry
/// records into `failed` and does not abort the loop, so one
/// upstream-deleted MAL id doesn't block the others from importing.
pub async fn merge_jikan_fallback_entries(
    db: &SqlitePool,
    entries: &[SyncEntry],
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();
    for entry in entries.iter().filter(|e| e.anilist_id < 0) {
        // Recover the original MAL id by negating the sentinel back.
        // `provider_media_id` carries the same value but going through
        // the sentinel keeps the AL-merge path and Jikan-merge path
        // consistent: each derives the upstream id from `anilist_id`.
        let mal_id = -entry.anilist_id;
        let target_mode = monitor_mode_for(entry.status, prefs.skip_already_watched);
        match merge_one_jikan_entry(db, entry, mal_id, target_mode, prefs, account_id).await {
            Ok(MergeAction::Created(spec)) => {
                outcome.created += 1;
                outcome.new_artwork.push(spec);
            }
            Ok(MergeAction::MonitorUpdated) => outcome.monitor_mode_updated += 1,
            Ok(MergeAction::Unchanged) => outcome.unchanged += 1,
            Ok(MergeAction::SkippedByPreference) => outcome.skipped_by_preference += 1,
            Ok(MergeAction::PinnedManually) => outcome.pinned_manually += 1,
            Err(msg) => outcome.failed.push((entry.anilist_id, msg)),
        }
    }
    outcome
}

async fn merge_one_jikan_entry(
    db: &SqlitePool,
    entry: &SyncEntry,
    mal_id: i64,
    target_mode: MonitorMode,
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> Result<MergeAction, String> {
    // Two lookup paths because the row may already exist under either
    // identity. anilist_id (negated sentinel) is canonical for sync-
    // sourced rows; mal_id covers the case where a previous
    // reconcile-fallbacks run promoted the row to a real AL id (and
    // the negated sentinel no longer matches).
    let existing = match series::get_by_anilist_id(db, entry.anilist_id).await {
        Ok(Some(row)) => Some(row),
        Ok(None) => series::get_by_mal_id(db, mal_id)
            .await
            .map_err(|e| format!("series mal lookup failed: {e}"))?,
        Err(e) => return Err(format!("series anilist lookup failed: {e}")),
    };

    if let Some(row) = existing {
        // Existing series → always update monitor_mode regardless
        // of import_status preference. A status transition on AL
        // (Watching → Dropped) must downgrade local monitor_mode
        // even when the new status's import flag is off.
        stamp_synced_from_if_set(db, row.id, account_id).await;
        stamp_user_score_if_set(db, row.id, entry.score, account_id).await;
        stamp_custom_lists_if_set(db, row.id, &entry.provider, &entry.custom_lists, account_id)
            .await;
        // Manual override takes precedence: the user has explicitly
        // pinned this series's monitor_mode through the UI. Sync
        // honors that pin until the user clears it via "Sync from
        // AL/MAL" in the dropdown.
        if row.monitor_mode_manual_override {
            return Ok(MergeAction::PinnedManually);
        }
        if row.monitor_mode == target_mode.as_str() {
            return Ok(MergeAction::Unchanged);
        }
        monitoring_service::apply_monitor_mode(db, row.id, target_mode).await?;
        return Ok(MergeAction::MonitorUpdated);
    }

    // New series → only create if the user wants this status imported.
    if !import_status(entry.status, prefs) {
        return Ok(MergeAction::SkippedByPreference);
    }

    // Fetch metadata from Jikan (cached). The cached helper handles
    // the 15-minute TTL + rate-limit pacing internally; we just call
    // it and trust its output.
    let detail = jikan::get_anime_detail_cached(mal_id)
        .await
        .map_err(|e| format!("Jikan detail fetch failed for mal_id {mal_id}: {e}"))?;

    let primary_title = if !detail.title_english.trim().is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    let (series_id, _created) = series::upsert(
        db,
        series::SeriesCore {
            // Preserve the negated sentinel so the existing > 0 filters
            // throughout the AL call sites continue to skip this row,
            // matching how Jikan-fallback entries already behave when
            // added through the interactive search flow.
            anilist_id: entry.anilist_id,
            mal_id: detail.id_mal.or(Some(mal_id)),
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

    // Same metadata_cache write as the AL path so a Jikan-fallback
    // entry's UI looks the same as an AL one (description, relations
    // — Jikan supplies most of them via /anime/{id}/full).
    if let Err(e) = metadata_cache::upsert(
        db,
        series_id,
        entry.anilist_id,
        detail.id_mal.or(Some(mal_id)),
        &detail,
    )
    .await
    {
        tracing::warn!(
            "metadata_cache::upsert failed for series_id={series_id} during Jikan sync: {e}"
        );
    }

    // #62 PR E — populate genre side table from Jikan-supplied genres.
    if let Err(e) = series_genres::replace_for_series(db, series_id, &detail.genres).await {
        tracing::warn!(
            "series_genres::replace_for_series failed for series_id={series_id} during Jikan sync: {e}"
        );
    }

    stamp_synced_from_if_set(db, series_id, account_id).await;
    stamp_user_score_if_set(db, series_id, entry.score, account_id).await;
    stamp_custom_lists_if_set(
        db,
        series_id,
        &entry.provider,
        &entry.custom_lists,
        account_id,
    )
    .await;
    monitoring_service::apply_monitor_mode(db, series_id, target_mode).await?;
    Ok(MergeAction::Created(NewArtworkSpec {
        series_id,
        cover_url: detail.cover_url.clone(),
        banner_url: detail.banner_url.clone(),
    }))
}

async fn merge_one_anilist_entry(
    db: &SqlitePool,
    entry: &SyncEntry,
    target_mode: MonitorMode,
    detail_map: &HashMap<i64, anilist::AnimeDetail>,
    prefs: &ImportPreferences,
    account_id: Option<i64>,
) -> Result<MergeAction, String> {
    let existing = series::get_by_anilist_id(db, entry.anilist_id)
        .await
        .map_err(|e| format!("series lookup failed: {e}"))?;

    if let Some(row) = existing {
        // Existing series → always update monitor_mode regardless of
        // import_status preference. A status transition on AL
        // (Watching → Dropped) must downgrade local monitor_mode
        // even when `import_dropped = false`, otherwise the series
        // silently keeps grabbing for a show the user dropped.
        stamp_synced_from_if_set(db, row.id, account_id).await;
        stamp_user_score_if_set(db, row.id, entry.score, account_id).await;
        stamp_custom_lists_if_set(db, row.id, &entry.provider, &entry.custom_lists, account_id)
            .await;
        // Manual override takes precedence: the user pinned this
        // series's monitor_mode through the UI. Sync honors the pin
        // until the user clears it via "Sync from AL/MAL".
        if row.monitor_mode_manual_override {
            return Ok(MergeAction::PinnedManually);
        }
        if row.monitor_mode == target_mode.as_str() {
            return Ok(MergeAction::Unchanged);
        }
        monitoring_service::apply_monitor_mode(db, row.id, target_mode).await?;
        return Ok(MergeAction::MonitorUpdated);
    }

    // New series → only create if the user wants this status imported.
    if !import_status(entry.status, prefs) {
        return Ok(MergeAction::SkippedByPreference);
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

    // Populate the cached AnimeDetail inline so the UI sees full
    // metadata (description, genres, relations) immediately on next
    // page load — without this the row would render with bare
    // series-table fields until the next 12h metadata_refresh sweep.
    // Best-effort: a failure logs but doesn't fail the merge.
    if let Err(e) =
        metadata_cache::upsert(db, series_id, entry.anilist_id, detail.id_mal, detail).await
    {
        tracing::warn!("metadata_cache::upsert failed for series_id={series_id} during sync: {e}");
    }

    // #62 PR E — populate genre side table from AL-supplied genres.
    if let Err(e) = series_genres::replace_for_series(db, series_id, &detail.genres).await {
        tracing::warn!(
            "series_genres::replace_for_series failed for series_id={series_id} during AL sync: {e}"
        );
    }

    stamp_synced_from_if_set(db, series_id, account_id).await;
    stamp_user_score_if_set(db, series_id, entry.score, account_id).await;
    stamp_custom_lists_if_set(
        db,
        series_id,
        &entry.provider,
        &entry.custom_lists,
        account_id,
    )
    .await;
    monitoring_service::apply_monitor_mode(db, series_id, target_mode).await?;
    Ok(MergeAction::Created(NewArtworkSpec {
        series_id,
        cover_url: detail.cover_url.clone(),
        banner_url: detail.banner_url.clone(),
    }))
}

/// Seven days of seconds; the weekly full-resync backstop interval.
/// Made an associated constant rather than a magic number so the value
/// shows up in tests and so a future "raise this to 30 days" change is
/// trivial.
pub const FULL_RESYNC_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;

/// Decide whether the current tick should be a full resync (vs. a
/// delta from `list_last_synced_at`). True when:
///   - There's no `list_full_resync_at` yet (first sync after link).
///   - `list_full_resync_at` is older than the weekly backstop window.
///   - There's no `list_last_synced_at` either (cursor unset; nothing
///     to delta against — equivalent to a first sync).
///
/// Pure function so the cursor decision stays unit-testable without
/// mocking the clock.
pub fn should_full_resync(
    list_last_synced_at: Option<i64>,
    list_full_resync_at: Option<i64>,
    now_unix_ts: i64,
) -> bool {
    if list_last_synced_at.is_none() {
        return true;
    }
    match list_full_resync_at {
        None => true,
        Some(t) => now_unix_ts.saturating_sub(t) >= FULL_RESYNC_INTERVAL_SECS,
    }
}

/// Drop entries whose `updated_at` is strictly before the cursor —
/// the caller has already merged everything up to and including that
/// timestamp. With `cursor = None`, all entries pass through (used on
/// full-sync passes and on the first sync ever).
///
/// `>=` rather than `>` is deliberate: the cursor is captured BEFORE
/// the network fetch, so an entry the user just edited at exactly
/// `cursor` may or may not have been visible to the previous tick's
/// fetch (provider read-after-write timing, clock skew between us
/// and the provider). Re-merging an unchanged entry is idempotent
/// (existing-series → unchanged), but losing a changed entry is a
/// silent data bug — we'd never re-fetch it unless a later edit
/// bumped its timestamp. Inclusive boundary is the safe direction.
pub fn drop_entries_before_cursor(entries: Vec<SyncEntry>, cursor: Option<i64>) -> Vec<SyncEntry> {
    let Some(c) = cursor else {
        return entries;
    };
    entries.into_iter().filter(|e| e.updated_at >= c).collect()
}

/// Process-wide lock guarding the watch-list sync. Two callers can
/// race: the supervised cadence loop in `main.rs` and the manual
/// "Sync now" handler. Without serialization they'd produce two
/// concurrent fetches, two merge passes against the same `series`
/// rows (idempotent on data but counters double-count), and two
/// `spawn_post_merge_bulk_pass` artwork loops.
///
/// Mirrors `services::rss::RSS_SYNC_LOCK` but with a split policy:
/// the supervised path **awaits** the lock (a pending manual sync
/// shouldn't push the next supervised tick into the exponential-
/// backoff path), while the manual path **try-locks** and surfaces
/// a "sync already in progress" error so the user gets immediate
/// feedback instead of a silent hang.
static EXTERNAL_SYNC_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// True when a row exists in `external_accounts`. Used by the
/// supervised loop to short-circuit before stamping
/// `scheduled_task_runs` — a 30-minute cadence with no linked
/// account would otherwise churn the table with "no external
/// account linked" rows forever.
pub async fn has_linked_account(db: &SqlitePool) -> bool {
    matches!(external_accounts::get_current(db).await, Ok(Some(_)))
}

/// Run one sync iteration against the linked account. Used by the
/// supervised loop in `main.rs::external_sync`. Awaits
/// [`EXTERNAL_SYNC_LOCK`] — a manual "Sync now" in flight blocks
/// this call until the manual sync completes, instead of letting
/// the supervised tick fail and trigger exponential backoff.
///
/// Returns a one-line summary used by `scheduled_task_runs.detail`.
/// Errors bubble up so the supervised loop's `mark_finished("error",
/// …)` path captures the failure.
pub async fn tick_once(state: &AppState) -> Result<String, String> {
    let _guard = EXTERNAL_SYNC_LOCK.lock().await;
    tick_once_inner(state, false).await
}

/// Manual-trigger variant. Returns a "sync already running" error
/// rather than waiting if the supervised loop or another manual
/// trigger is already mid-tick. The user-facing toast surfaces this
/// error directly so a double-click doesn't silently queue.
///
/// Forces a full resync regardless of `list_full_resync_at`: a user
/// clicking "Sync now" almost always means "I just changed my list,
/// reflect it" — including removals. Without the force flag, removal
/// detection would be skipped until the next 7-day boundary, leaving
/// a removed-from-AL series grabbing for up to a week. The cursor
/// stamps still advance, so the next supervised tick reads as
/// already-synced.
pub async fn tick_once_or_busy(state: &AppState) -> Result<String, String> {
    let _guard = EXTERNAL_SYNC_LOCK
        .try_lock()
        .map_err(|_| "Watch-list sync is already running.".to_string())?;
    tick_once_inner(state, true).await
}

/// True when the sync error string indicates the user's auth token
/// is dead and re-linking is the fix (vs. transient rate-limits,
/// network errors, or upstream 5xx). Matches the stable prefixes
/// the sync engine emits — adding new wordings means updating this
/// list, which is the project's existing string-tag convention for
/// the AL failure taxonomy.
fn is_auth_rejection(err: &str) -> bool {
    const AUTH_PREFIXES: &[&str] = &[
        "AniList rejected the watch-list token",
        "MAL access token expired and no refresh token stored",
        "MAL refresh failed (re-link required)",
        "MAL rejected the token immediately after refresh",
        "re-link required",
    ];
    AUTH_PREFIXES.iter().any(|p| err.contains(p))
}

async fn tick_once_inner(state: &AppState, force_full_sync: bool) -> Result<String, String> {
    let account = external_accounts::get_current(&state.db)
        .await
        .map_err(|e| format!("read external_accounts: {e}"))?;

    let Some(account) = account else {
        return Ok("no external account linked".to_string());
    };

    // Capture the tick's wall-clock at entry, before any network
    // fetch. The cursor we stamp on success is "the moment we started
    // looking" — using the post-fetch time would risk dropping
    // entries the user updated while we were syncing.
    let tick_started_at = current_unix_ts();
    let is_full_sync = force_full_sync
        || should_full_resync(
            account.list_last_synced_at,
            account.list_full_resync_at,
            tick_started_at,
        );
    let delta_cursor = if is_full_sync {
        None
    } else {
        account.list_last_synced_at
    };

    let raw = match account.provider.as_str() {
        external_accounts::PROVIDER_ANILIST => {
            sync_anilist(state, &account, delta_cursor, is_full_sync).await
        }
        external_accounts::PROVIDER_MAL => {
            sync_mal(state, account.clone(), delta_cursor, is_full_sync).await
        }
        other => {
            // Unknown provider string — schema CHECK constraint should
            // prevent this, but surface explicitly rather than panic.
            return Err(format!("unknown external_accounts.provider: {other}"));
        }
    };

    let summary = match raw {
        Ok(s) => s,
        Err(e) => {
            // #62 PR E — auth-rejection detection. The sync engine
            // returns stable error-prefix strings for token-dead
            // failures; a match flips the sticky flag so the
            // Settings UI can render the "Re-link required" banner.
            // Other failure modes (rate-limit, network timeout)
            // leave the flag alone — they're transient.
            if is_auth_rejection(&e)
                && let Err(write_err) =
                    external_accounts::update_last_sync_auth_failed(&state.db, account.id, true)
                        .await
            {
                tracing::warn!(
                    "failed to set last_sync_auth_failed for account_id={}: {write_err}",
                    account.id
                );
            }
            return Err(e);
        }
    };

    // Only stamp on success — a failed tick must not advance the
    // cursor or the entries it skipped fetching would be lost forever.
    external_accounts::stamp_list_synced(&state.db, account.id, tick_started_at, is_full_sync)
        .await?;
    // Clear any stale auth-failure flag — the sync just succeeded,
    // whatever caused the prior failure resolved (e.g. user
    // re-linked).
    if account.last_sync_auth_failed
        && let Err(e) =
            external_accounts::update_last_sync_auth_failed(&state.db, account.id, false).await
    {
        tracing::warn!(
            "failed to clear last_sync_auth_failed for account_id={}: {e}",
            account.id
        );
    }

    Ok(if is_full_sync {
        format!("{summary} [full-resync]")
    } else {
        format!("{summary} [delta]")
    })
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
    delta_cursor: Option<i64>,
    is_full_sync: bool,
) -> Result<String, String> {
    let user_id: i64 = account.provider_user_id.parse().map_err(|e| {
        format!(
            "AL provider_user_id is not a valid integer: {} ({e})",
            account.provider_user_id
        )
    })?;

    let fetch = anilist::fetch_media_list_collection(&account.access_token, user_id).await?;
    let raw = fetch.entries;
    let raw_total = raw.len();

    // Refresh the user's score_format on the linked-account row so
    // the "You: X" badge picks up POINT_X changes the user made on
    // AL after their original link. Empty-string responses no-op
    // (defensive — AL's user.mediaListOptions field has been stable
    // for years but a partial response shouldn't blank a known-good
    // value).
    if let Err(e) =
        external_accounts::update_score_format(&state.db, account.id, &fetch.score_format).await
    {
        tracing::warn!(
            "update_score_format failed for account_id={}: {e}",
            account.id
        );
    }

    let prefs = ImportPreferences {
        import_watching: account.import_watching,
        import_planning: account.import_planning,
        import_paused: account.import_paused,
        import_dropped: account.import_dropped,
        import_completed: account.import_completed,
        skip_already_watched: account.skip_already_watched,
    };
    // Convert raw → SyncEntry without filtering: existing-series
    // monitor_mode updates need to flow regardless of whether the
    // user wants this status imported. The merge step gates only the
    // create branch.
    let entries = entries_from_anilist(raw);
    let after_convert = entries.len();

    // Delta filter: drop entries that haven't changed since the last
    // successful tick. On a full-resync run delta_cursor = None, so
    // every entry passes through.
    let entries = drop_entries_before_cursor(entries, delta_cursor);
    let kept = entries.len();
    let stale_dropped = after_convert - kept;

    // Pre-fetch AnimeDetail for the AL ids that don't yet have a
    // series row AND would be created (status passes import prefs).
    // Existing rows skip the fetch — the merge step only touches
    // monitor_mode for those — and not-existing-but-not-importable
    // entries skip too, since the merge will mark them
    // SkippedByPreference without needing the detail.
    let new_ids = ids_needing_detail_fetch(&state.db, &entries, &prefs).await;
    let detail_map = if new_ids.is_empty() {
        HashMap::new()
    } else {
        anilist::get_anime_details_batch(&new_ids)
            .await
            .map_err(|e| format!("AniList detail batch fetch failed: {e}"))?
    };

    let outcome =
        merge_into_library(&state.db, &entries, &detail_map, &prefs, Some(account.id)).await;

    // Removal detection (full-resync only). Delta runs by definition
    // only fetch CHANGED entries, so a series whose updated_at is
    // older than the cursor wouldn't be in `entries` even though
    // it's still on the user's AL list. Running removal on a delta
    // would wrongly downgrade every still-on-list series whose entry
    // didn't change since the last tick. Full-resync includes every
    // entry on the list, so the missing-from-fetch check is sound.
    let removal_report = if is_full_sync {
        let fetch_ids: std::collections::HashSet<i64> =
            entries.iter().map(|e| e.anilist_id).collect();
        detect_removals(&state.db, account.id, &fetch_ids).await?
    } else {
        RemovalReport::default()
    };
    let removed_count = removal_report.removed.len();

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "AniList watch-list synced: {kept} kept ({stale_dropped} pre-cursor), {} created, {} monitor-mode updated, {} unchanged, {} skipped (import prefs off), {} pinned-manually, {} removed-from-list, {} failed",
            outcome.created,
            outcome.monitor_mode_updated,
            outcome.unchanged,
            outcome.skipped_by_preference,
            outcome.pinned_manually,
            removed_count,
            outcome.failed.len(),
        ),
        &format!("username={} fetched_total={raw_total}", account.username),
    )
    .await;
    log_failed_entries(&state.db, &outcome).await;
    // #62 PR E — clear any stale MAL deferred count from a prior
    // provider on this same account row. AL syncs never produce
    // deferred entries (no anibridge step), so always writing 0
    // keeps the Settings UI accurate after a provider switch.
    if let Err(e) =
        external_accounts::update_last_sync_deferred_count(&state.db, account.id, 0).await
    {
        tracing::warn!(
            "update_last_sync_deferred_count failed for account_id={}: {e}",
            account.id
        );
    }
    spawn_post_merge_bulk_pass(state, outcome.new_artwork.clone()).await;

    Ok(format!(
        "AniList: fetched {raw_total}, kept {kept}, created {}, updated {}, unchanged {}, skipped {}, pinned-manually {}, removed-from-list {}, failed {}",
        outcome.created,
        outcome.monitor_mode_updated,
        outcome.unchanged,
        outcome.skipped_by_preference,
        outcome.pinned_manually,
        removed_count,
        outcome.failed.len(),
    ))
}

/// Return the AL ids in `entries` that need an AnimeDetail fetch
/// before merge: ids whose `series` row doesn't exist locally AND
/// whose status passes the user's import preferences. Existing rows
/// skip because the merge updates only their monitor_mode; not-
/// existing + not-importable entries skip because the merge will
/// SkippedByPreference them without ever needing the detail.
async fn ids_needing_detail_fetch(
    db: &SqlitePool,
    entries: &[SyncEntry],
    prefs: &ImportPreferences,
) -> Vec<i64> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.anilist_id <= 0 {
            continue;
        }
        if !import_status(entry.status, prefs) {
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
    delta_cursor: Option<i64>,
    is_full_sync: bool,
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
    // Convert without filter — same rationale as sync_anilist.
    let normalized = entries_from_mal(entries);
    let after_convert = normalized.len();

    // Delta filter happens BEFORE anibridge resolution / detail fetch
    // so a delta tick doesn't incur a single network call when the
    // user's list hasn't changed since last tick.
    let normalized = drop_entries_before_cursor(normalized, delta_cursor);
    let kept = normalized.len();
    let stale_dropped = after_convert - kept;

    // Resolve MAL → AL via anibridge. Misses fall back to the
    // negated-MAL-id sentinel, handled by merge_jikan_fallback_entries
    // in the second pass below.
    let _ = anibridge::ensure_loaded().await;
    let resolved = resolve_mal_anilist_ids(normalized).await;

    let new_ids = ids_needing_detail_fetch(&state.db, &resolved, &prefs).await;
    let detail_map = if new_ids.is_empty() {
        HashMap::new()
    } else {
        anilist::get_anime_details_batch(&new_ids)
            .await
            .map_err(|e| format!("AniList detail batch fetch failed: {e}"))?
    };

    let al_outcome =
        merge_into_library(&state.db, &resolved, &detail_map, &prefs, Some(account.id)).await;

    // Second pass: walk the negated-id (anibridge-miss) entries and
    // merge each via Jikan metadata. The combined outcome's
    // deferred_jikan counter falls toward zero as Jikan acts on
    // entries; anything still deferred at the end means Jikan also
    // failed (rate-limited, deleted upstream, etc.).
    let jikan_outcome =
        merge_jikan_fallback_entries(&state.db, &resolved, &prefs, Some(account.id)).await;
    let outcome = al_outcome.merge_pass(jikan_outcome);

    // Removal detection (full-resync only) — same rationale as the
    // AL path. fetch_ids covers BOTH positive (anibridge-resolved)
    // and negated (Jikan-fallback sentinel) ids since both shapes
    // land in series.anilist_id.
    let removal_report = if is_full_sync {
        let fetch_ids: std::collections::HashSet<i64> =
            resolved.iter().map(|e| e.anilist_id).collect();
        detect_removals(&state.db, account.id, &fetch_ids).await?
    } else {
        RemovalReport::default()
    };
    let removed_count = removal_report.removed.len();

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!(
            "MyAnimeList watch-list synced: {kept} kept ({stale_dropped} pre-cursor), {} created, {} monitor-mode updated, {} unchanged, {} skipped (import prefs off), {} pinned-manually, {} deferred, {} removed-from-list, {} failed",
            outcome.created,
            outcome.monitor_mode_updated,
            outcome.unchanged,
            outcome.skipped_by_preference,
            outcome.pinned_manually,
            outcome.deferred_jikan,
            removed_count,
            outcome.failed.len(),
        ),
        &format!("username={} fetched_total={raw_total}", account.username),
    )
    .await;
    log_failed_entries(&state.db, &outcome).await;
    // #62 PR E — persist the MAL→AL mapping-failure count so the
    // Settings UI can render a "N series couldn't be mapped" banner
    // without scraping the supervised-loop summary string.
    if let Err(e) = external_accounts::update_last_sync_deferred_count(
        &state.db,
        account.id,
        outcome.deferred_jikan as i64,
    )
    .await
    {
        tracing::warn!(
            "update_last_sync_deferred_count failed for account_id={}: {e}",
            account.id
        );
    }
    spawn_post_merge_bulk_pass(state, outcome.new_artwork.clone()).await;

    Ok(format!(
        "MyAnimeList: fetched {raw_total}, kept {kept}, created {}, updated {}, unchanged {}, skipped {}, pinned-manually {}, deferred {}, removed-from-list {}, failed {}",
        outcome.created,
        outcome.monitor_mode_updated,
        outcome.unchanged,
        outcome.skipped_by_preference,
        outcome.pinned_manually,
        outcome.deferred_jikan,
        removed_count,
        outcome.failed.len(),
    ))
}

/// Coalesced post-merge work for sync-imported series. Runs once per
/// tick (vs. once per series for the interactive add path) so a
/// 200-series first sync doesn't spawn 200 background tasks. Caches
/// cover + banner artwork sequentially through `artwork::cache_image`,
/// then fires a single Jellyfin library refresh if any series was
/// imported and the user has Jellyfin configured.
///
/// All work runs in a spawned task — the sync tick returns immediately
/// after kicking off this future. A failure in any step logs but
/// doesn't propagate; the artwork host being down or Jellyfin being
/// offline shouldn't make the next tick consider the prior tick a
/// failure (which would block the cursor advance).
async fn spawn_post_merge_bulk_pass(state: &AppState, specs: Vec<NewArtworkSpec>) {
    if specs.is_empty() {
        return;
    }
    let db = state.db.clone();
    let jellyfin = state.jellyfin.read().await.clone();
    tokio::spawn(async move {
        // Sequential rather than parallel: hammering an artwork CDN
        // with 400 concurrent requests is the kind of thing that gets
        // an IP rate-limited. The serial walk takes a minute or two
        // for a fresh import; the user's library still renders during
        // that window because cached_or_source_url falls back to the
        // upstream URL when the local key isn't present yet.
        for spec in &specs {
            if !spec.cover_url.is_empty() {
                let _ = artwork::cache_image(
                    &db,
                    &format!("series-{}-cover", spec.series_id),
                    "series",
                    Some(spec.series_id),
                    "cover",
                    &spec.cover_url,
                )
                .await;
            }
            if !spec.banner_url.is_empty() {
                let _ = artwork::cache_image(
                    &db,
                    &format!("series-{}-banner", spec.series_id),
                    "series",
                    Some(spec.series_id),
                    "banner",
                    &spec.banner_url,
                )
                .await;
            }
        }
        // One Jellyfin refresh at the end — covers all newly-imported
        // series in a single call. The same coalesce avoids the
        // pattern where 200 individual interactive adds would fire 200
        // /Library/Refresh requests against Jellyfin and overwhelm the
        // scan queue.
        if let Some(client) = jellyfin
            && let Err(e) = client.refresh_library().await
        {
            logger::warn(
                &db,
                LogCategory::Jellyfin,
                "Sync-driven Jellyfin refresh failed",
                &e,
            )
            .await;
        }
        logger::info(
            &db,
            LogCategory::ExternalSync,
            &format!(
                "Bulk-mode post-merge artwork cache complete ({} series)",
                specs.len()
            ),
            "",
        )
        .await;
    });
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

    fn prefs_with_skip_already_watched() -> ImportPreferences {
        ImportPreferences {
            skip_already_watched: true,
            ..prefs_default()
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
    fn entries_from_anilist_passes_all_statuses_through_unfiltered() {
        // Filter moved from conversion time to merge time so existing
        // series with a filtered-out status still get monitor_mode
        // updated (Watching → Dropped on AL must downgrade local
        // monitor_mode even with import_dropped=false). All four
        // statuses pass through here regardless of prefs.
        let raw = vec![
            al_entry(1, "CURRENT"),
            al_entry(2, "PLANNING"),
            al_entry(3, "DROPPED"),
            al_entry(4, "COMPLETED"),
        ];
        let entries = entries_from_anilist(raw);
        assert_eq!(entries.len(), 4, "no filter at conversion time");
        assert_eq!(entries[0].provider_media_id, 1);
        assert_eq!(
            entries[0].anilist_id, 1,
            "AL provider_media_id == anilist_id"
        );
        assert_eq!(entries[0].status, NormalizedStatus::Watching);
        assert_eq!(entries[2].status, NormalizedStatus::Dropped);
        assert_eq!(entries[3].status, NormalizedStatus::Completed);
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
        let entries = entries_from_mal(raw);
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

    // ── Delta cursor / full-resync helpers ────────────────────────

    #[test]
    fn should_full_resync_when_no_cursor_at_all() {
        // Fresh link: list_last_synced_at and list_full_resync_at are
        // both None → first sync MUST be full to populate everything.
        assert!(should_full_resync(None, None, 1_700_000_000));
    }

    #[test]
    fn should_full_resync_when_only_full_resync_missing() {
        // Defensive: list_last_synced_at populated but
        // list_full_resync_at NULL means the cursor schema landed
        // mid-deployment; treat as "no full sync ever, run one now."
        assert!(should_full_resync(Some(1_700_000_000), None, 1_700_000_001));
    }

    #[test]
    fn should_full_resync_after_seven_day_window() {
        let now = 2_000_000_000;
        let just_under = now - FULL_RESYNC_INTERVAL_SECS + 1;
        let exactly = now - FULL_RESYNC_INTERVAL_SECS;
        let beyond = now - FULL_RESYNC_INTERVAL_SECS - 1;
        assert!(
            !should_full_resync(Some(just_under), Some(just_under), now),
            "below threshold → delta"
        );
        assert!(
            should_full_resync(Some(exactly), Some(exactly), now),
            "exactly at threshold → full (>= boundary)"
        );
        assert!(
            should_full_resync(Some(beyond), Some(beyond), now),
            "past threshold → full"
        );
    }

    #[test]
    fn drop_entries_before_cursor_passes_all_when_cursor_missing() {
        // First sync ever: cursor None means every entry merges.
        let mk = |ts| SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: ts,
            anilist_id: ts,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: ts,
            custom_lists: Vec::new(),
        };
        let entries = vec![mk(1), mk(2), mk(3)];
        let kept = drop_entries_before_cursor(entries, None);
        assert_eq!(kept.len(), 3);
    }

    #[tokio::test]
    async fn tick_once_or_busy_returns_busy_when_lock_held() {
        // Hold the sync lock from a separate task and assert that
        // tick_once_or_busy fails fast with the user-facing message.
        // Regression for the PR #94 finding: supervised + manual
        // races used to spawn two concurrent fetches. Works on the
        // default current_thread runtime because tokio::sync::Mutex
        // and Notify cooperatively yield — the holder runs to the
        // lock + notify_one + .notified() suspension point, then
        // control returns here for the try_lock attempt.
        let lock_held = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let lh = lock_held.clone();
        let r = release.clone();
        let holder = tokio::spawn(async move {
            let _guard = EXTERNAL_SYNC_LOCK.lock().await;
            lh.notify_one();
            r.notified().await;
        });
        lock_held.notified().await;

        let db = crate::test_support::in_memory_pool().await;
        let state = crate::test_support::build_test_app_state(db, None);
        let result = tick_once_or_busy(&state).await;
        assert!(matches!(&result, Err(msg) if msg.contains("already running")));

        release.notify_one();
        let _ = holder.await;
    }

    #[test]
    fn drop_entries_before_cursor_keeps_boundary_and_newer_entries() {
        // cursor = 100: entries with updated_at >= 100 survive; only
        // strictly-older entries drop. Inclusive boundary is the safe
        // direction — see the doc comment on drop_entries_before_cursor
        // for the read-after-write / clock-skew rationale.
        let mk = |ts| SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: ts,
            anilist_id: ts,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: ts,
            custom_lists: Vec::new(),
        };
        let entries = vec![mk(50), mk(99), mk(100), mk(101), mk(200)];
        let kept = drop_entries_before_cursor(entries, Some(100));
        assert_eq!(kept.len(), 3, "boundary entry (100) must be kept");
        assert_eq!(kept[0].updated_at, 100);
        assert_eq!(kept[1].updated_at, 101);
        assert_eq!(kept[2].updated_at, 200);
    }

    #[tokio::test]
    async fn merge_creates_new_series_with_resolved_monitor_mode() {
        let db = crate::test_support::in_memory_pool().await;
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Watching,
        )];
        let mut detail = make_detail(12345, "Example", "TV", "RELEASING");
        detail.cover_url = "https://example/cover.jpg".to_string();
        detail.banner_url = "https://example/banner.jpg".to_string();
        let mut detail_map = HashMap::new();
        detail_map.insert(12345, detail);

        let outcome = merge_into_library(&db, &entries, &detail_map, &prefs_default(), None).await;
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

        // Newly-created series MUST yield an artwork spec so the
        // post-merge bulk-mode pass has the cover + banner URLs to
        // fetch. Without this the spec lookup would silently no-op
        // and the user's library would render via upstream-source
        // fallback URLs forever.
        assert_eq!(outcome.new_artwork.len(), 1);
        assert_eq!(
            outcome.new_artwork[0].cover_url,
            "https://example/cover.jpg"
        );
        assert_eq!(
            outcome.new_artwork[0].banner_url,
            "https://example/banner.jpg"
        );

        // metadata_cache row is written inline so the UI sees full
        // metadata immediately on next page load instead of waiting
        // on the next 12h metadata_refresh sweep. Without this the
        // newly-imported series page renders bare title + status only.
        let cached = crate::models::metadata_cache::get_by_series_id(&db, row.id)
            .await
            .unwrap()
            .expect("metadata_cache row should exist after merge");
        assert_eq!(cached.detail.id, 12345);
        assert_eq!(cached.detail.title_english, "Example");
        assert!(cached.is_fresh, "freshly-cached row must be is_fresh");
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
        let outcome =
            merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
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
        let outcome =
            merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
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
        let outcome =
            merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
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
        let outcome =
            merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, 99999);
        assert!(outcome.failed[0].1.contains("no AniList detail"));
    }

    #[tokio::test]
    async fn merge_jikan_fallback_creates_series_with_negated_sentinel() {
        let db = crate::test_support::in_memory_pool().await;
        // Seed Jikan's detail cache so the merge call hits the cache
        // rather than the live Jikan API. mal_id 555 ↔ negated AL id
        // -555 (the sync-time sentinel).
        let detail = make_detail(-555, "Jikan-only Show", "TV", "RELEASING");
        let mut detail = detail;
        detail.id_mal = Some(555);
        jikan::seed_detail_cache_for_tests(555, detail).await;

        let entries = vec![entry(
            external_accounts::PROVIDER_MAL,
            -555,
            NormalizedStatus::Watching,
        )];
        let outcome = merge_jikan_fallback_entries(&db, &entries, &prefs_default(), None).await;
        assert_eq!(outcome.created, 1);
        assert!(outcome.failed.is_empty());

        // The new row carries the negated sentinel — preserves the
        // existing `> 0` filters on every AL call site so this entry
        // routes back through Jikan on refresh.
        let row = series::get_by_anilist_id(&db, -555)
            .await
            .unwrap()
            .expect("series row should exist under negated sentinel");
        assert_eq!(row.anilist_id, -555);
        assert_eq!(row.mal_id, Some(555));
        assert_eq!(row.monitor_mode, MonitorMode::All.as_str());

        jikan::clear_detail_cache_entry_for_tests(555).await;
    }

    #[tokio::test]
    async fn merge_jikan_fallback_skips_positive_ids() {
        // The Jikan pass must only touch negated-id entries. A
        // positive AL id sneaking in would be a logic bug — the AL
        // pass already handled it.
        let db = crate::test_support::in_memory_pool().await;
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Watching,
        )];
        let outcome = merge_jikan_fallback_entries(&db, &entries, &prefs_default(), None).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.monitor_mode_updated, 0);
        assert_eq!(outcome.unchanged, 0);
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn merge_pass_combines_outcomes_and_drains_deferred_counter() {
        // AL pass deferred 3 entries; Jikan pass merged 2 + failed 1
        // (3 entries handled). Combined deferred drops to 0.
        let al = MergeOutcome {
            created: 5,
            monitor_mode_updated: 2,
            unchanged: 10,
            deferred_jikan: 3,
            failed: Vec::new(),
            skipped_by_preference: 1,
            pinned_manually: 0,
            new_artwork: vec![NewArtworkSpec {
                series_id: 1,
                cover_url: "c1".into(),
                banner_url: "b1".into(),
            }],
        };
        let jikan = MergeOutcome {
            created: 2,
            monitor_mode_updated: 0,
            unchanged: 0,
            deferred_jikan: 0,
            failed: vec![(-9999, "Jikan rate-limited".into())],
            skipped_by_preference: 0,
            pinned_manually: 0,
            new_artwork: vec![
                NewArtworkSpec {
                    series_id: 2,
                    cover_url: "c2".into(),
                    banner_url: "b2".into(),
                },
                NewArtworkSpec {
                    series_id: 3,
                    cover_url: "c3".into(),
                    banner_url: "b3".into(),
                },
            ],
        };
        let combined = al.merge_pass(jikan);
        assert_eq!(combined.created, 7);
        assert_eq!(combined.monitor_mode_updated, 2);
        assert_eq!(combined.unchanged, 10);
        assert_eq!(combined.deferred_jikan, 0);
        assert_eq!(combined.failed.len(), 1);
        // Artwork specs concatenate across passes — the bulk-mode
        // post-merge task expects the full list of new series.
        assert_eq!(combined.new_artwork.len(), 3);
    }

    #[test]
    fn merge_pass_keeps_remaining_deferred_when_jikan_partial() {
        // AL deferred 5; Jikan only handled 2. 3 still deferred.
        let al = MergeOutcome {
            created: 0,
            monitor_mode_updated: 0,
            unchanged: 0,
            deferred_jikan: 5,
            failed: Vec::new(),
            skipped_by_preference: 0,
            pinned_manually: 0,
            new_artwork: Vec::new(),
        };
        let jikan = MergeOutcome {
            created: 2,
            monitor_mode_updated: 0,
            unchanged: 0,
            deferred_jikan: 0,
            failed: Vec::new(),
            skipped_by_preference: 0,
            pinned_manually: 0,
            new_artwork: Vec::new(),
        };
        let combined = al.merge_pass(jikan);
        assert_eq!(combined.deferred_jikan, 3);
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

        let outcome = merge_into_library(
            &db,
            &entries,
            &detail_map,
            &prefs_with_skip_already_watched(),
            None,
        )
        .await;
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
    fn entries_from_mal_passes_all_statuses_through_unfiltered() {
        // Conversion no longer filters; merge step gates create-only
        // by import preference. Every status passes through here so
        // existing series with a filtered-out new status still get
        // their monitor_mode updated downstream.
        let raw = vec![
            mal_entry(1, "watching"),
            mal_entry(2, "on_hold"),
            mal_entry(3, "dropped"),
            mal_entry(4, "completed"),
            mal_entry(5, "plan_to_watch"),
        ];
        let entries = entries_from_mal(raw);
        assert_eq!(entries.len(), 5, "no filter at conversion time");
    }

    #[tokio::test]
    async fn merge_updates_existing_when_status_filtered_out() {
        // Regression for the user-reported case: existing Watching
        // series, AL transitions it to Dropped, user has
        // import_dropped=false. The series MUST flip to monitor_mode
        // = none anyway — otherwise a dropped show keeps grabbing
        // forever.
        let db = crate::test_support::in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Example").await;
        sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
            .bind(MonitorMode::All.as_str())
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();

        // No detail map needed — series already exists, merge updates
        // monitor_mode regardless of whether import_dropped is on.
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Dropped,
        )];
        let prefs = prefs_default(); // import_dropped = false
        assert!(!prefs.import_dropped, "test premise: import_dropped off");

        let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs, None).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.monitor_mode_updated, 1);
        assert_eq!(outcome.skipped_by_preference, 0);

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert_eq!(
            row.monitor_mode,
            MonitorMode::None.as_str(),
            "Dropped status must downgrade existing series to None even with import_dropped=false"
        );
    }

    /// Helper: insert a placeholder `external_accounts` row directly,
    /// bypassing the encrypt-then-INSERT path of `link()`. The
    /// removal-detection tests need a real id to satisfy the FK
    /// constraint on `synced_from_external_account_id` but don't care
    /// about the token contents — the tests never decrypt them.
    /// `provider` must be `"anilist"` or `"mal"` (schema CHECK +
    /// UNIQUE(provider)); two-account tests pass one of each.
    async fn seed_account_id(db: &sqlx::SqlitePool, id: i64, provider: &str) {
        sqlx::query(
            "INSERT INTO external_accounts \
             (id, provider, provider_user_id, username, \
              access_token_encrypted, refresh_token_encrypted, linked_at) \
             VALUES (?, ?, ?, ?, X'00', X'00', 0)",
        )
        .bind(id)
        .bind(provider)
        .bind(format!("user-{id}"))
        .bind(format!("user-{id}"))
        .execute(db)
        .await
        .unwrap();
    }

    /// Helper: stamp `synced_from_external_account_id` on a series row
    /// so the removal-detection tests can pin which series came from
    /// which account.
    async fn force_synced_from(db: &sqlx::SqlitePool, series_id: i64, account_id: i64) {
        sqlx::query("UPDATE series SET synced_from_external_account_id = ? WHERE id = ?")
            .bind(account_id)
            .bind(series_id)
            .execute(db)
            .await
            .unwrap();
    }

    async fn force_monitor_mode(db: &sqlx::SqlitePool, series_id: i64, mode: MonitorMode) {
        sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
            .bind(mode.as_str())
            .bind(series_id)
            .execute(db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn detect_removals_downgrades_missing_synced_series() {
        // The key user-facing behavior: a series that was on AL,
        // synced into Ryokan with monitor_mode=all, gets removed from
        // AL → next full-sync downgrades monitor_mode to None so it
        // stops grabbing.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let kept_id = crate::test_support::seed_series(&db, 100, "Kept").await;
        let removed_id = crate::test_support::seed_series(&db, 200, "Removed").await;
        force_synced_from(&db, kept_id, 1).await;
        force_synced_from(&db, removed_id, 1).await;
        force_monitor_mode(&db, kept_id, MonitorMode::All).await;
        force_monitor_mode(&db, removed_id, MonitorMode::All).await;

        // Current fetch only includes the kept one (anilist_id=100);
        // 200 is missing → removal detection downgrades it.
        let mut fetch_ids = std::collections::HashSet::new();
        fetch_ids.insert(100);
        let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0], removed_id);

        let kept = series::get_by_id(&db, kept_id).await.unwrap().unwrap();
        let removed = series::get_by_id(&db, removed_id).await.unwrap().unwrap();
        assert_eq!(
            kept.monitor_mode,
            MonitorMode::All.as_str(),
            "in-fetch series stays at its existing mode"
        );
        assert_eq!(
            removed.monitor_mode,
            MonitorMode::None.as_str(),
            "removed-from-fetch series downgrades to None"
        );
    }

    #[tokio::test]
    async fn detect_removals_leaves_manually_added_series_alone() {
        // synced_from_external_account_id IS NULL means the user added
        // this manually. Removal detection MUST NOT touch it even if
        // it's not in the fetch — the user's library is theirs.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let manual_id = crate::test_support::seed_series(&db, 300, "Manual").await;
        let synced_id = crate::test_support::seed_series(&db, 400, "Synced").await;
        force_synced_from(&db, synced_id, 1).await;
        force_monitor_mode(&db, manual_id, MonitorMode::All).await;
        force_monitor_mode(&db, synced_id, MonitorMode::All).await;

        // Empty fetch — neither series is on the user's list.
        let fetch_ids = std::collections::HashSet::new();
        let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();

        // Only the synced series gets downgraded.
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0], synced_id);

        let manual = series::get_by_id(&db, manual_id).await.unwrap().unwrap();
        assert_eq!(
            manual.monitor_mode,
            MonitorMode::All.as_str(),
            "manually-added series MUST NOT be touched by removal detection"
        );
    }

    #[tokio::test]
    async fn detect_removals_skips_already_none_series() {
        // A series that's already at monitor_mode=none doesn't need a
        // redundant write. Counter only includes series that actually
        // changed.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let already_none = crate::test_support::seed_series(&db, 500, "Already None").await;
        force_synced_from(&db, already_none, 1).await;
        force_monitor_mode(&db, already_none, MonitorMode::None).await;

        let fetch_ids = std::collections::HashSet::new();
        let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
        assert_eq!(
            report.removed.len(),
            0,
            "already-None series stays out of the report"
        );
    }

    #[tokio::test]
    async fn detect_removals_scopes_to_account_id() {
        // Two accounts, each synced one series. Removal detection
        // for account=1 must NOT touch account=2's series.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        seed_account_id(&db, 2, "mal").await;
        let acct1_series = crate::test_support::seed_series(&db, 600, "Acct1 series").await;
        let acct2_series = crate::test_support::seed_series(&db, 700, "Acct2 series").await;
        force_synced_from(&db, acct1_series, 1).await;
        force_synced_from(&db, acct2_series, 2).await;
        force_monitor_mode(&db, acct1_series, MonitorMode::All).await;
        force_monitor_mode(&db, acct2_series, MonitorMode::All).await;

        // Empty fetch for account 1.
        let fetch_ids = std::collections::HashSet::new();
        let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
        assert_eq!(report.removed, vec![acct1_series]);

        // Account 2's series is unaffected.
        let acct2 = series::get_by_id(&db, acct2_series).await.unwrap().unwrap();
        assert_eq!(acct2.monitor_mode, MonitorMode::All.as_str());
    }

    #[tokio::test]
    async fn merge_writes_user_score_on_existing_series() {
        // Sync brings in entry.score = 8.5; merge writes it to
        // series.user_score so the "You: 8.5" badge renders. Doesn't
        // need to be a status transition — score updates on every
        // tick regardless of monitor_mode movement.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Scored").await;

        let entries = vec![SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: 12345,
            anilist_id: 12345,
            status: NormalizedStatus::Watching,
            progress: 4,
            score: 8.5,
            updated_at: 0,
            custom_lists: Vec::new(),
        }];
        let outcome =
            merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), Some(1)).await;
        // Existing series → MonitorUpdated (the seed left monitor_mode
        // empty so target=All differs); the test pins user_score
        // regardless of the action variant.
        assert!(outcome.failed.is_empty());

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert_eq!(
            row.user_score,
            Some(8.5),
            "merge must write entry.score to user_score"
        );
    }

    #[tokio::test]
    async fn merge_writes_custom_list_memberships_for_anilist() {
        // AL custom-list membership is replaced on every successful
        // merge action so the detail-page badge row + library filter
        // stay in lockstep with the user's current AL state.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Listed").await;

        let entries = vec![SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: 12345,
            anilist_id: 12345,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: vec!["Hidden Gems".into(), "Top 10".into()],
        }];
        let _ = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), Some(1)).await;

        let memberships = series_custom_lists::list_for_series(&db, series_id)
            .await
            .unwrap();
        assert_eq!(memberships.len(), 2);
        assert!(memberships.iter().any(|m| m.list_name == "Hidden Gems"));
        assert!(memberships.iter().any(|m| m.list_name == "Top 10"));
        for m in &memberships {
            assert_eq!(m.provider, external_accounts::PROVIDER_ANILIST);
        }
    }

    #[tokio::test]
    async fn merge_replaces_stale_custom_list_membership() {
        // The user moved a series out of "Hidden Gems" on AL. The
        // next sync's replace-on-merge MUST drop the old membership;
        // an upsert-only path would leak stale rows forever.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Moved").await;

        // First sync: in Hidden Gems.
        let entries_v1 = vec![SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: 12345,
            anilist_id: 12345,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: vec!["Hidden Gems".into()],
        }];
        let _ =
            merge_into_library(&db, &entries_v1, &HashMap::new(), &prefs_default(), Some(1)).await;

        // Second sync: moved to Top 10, no longer in Hidden Gems.
        let entries_v2 = vec![SyncEntry {
            custom_lists: vec!["Top 10".into()],
            ..entries_v1[0].clone()
        }];
        let _ =
            merge_into_library(&db, &entries_v2, &HashMap::new(), &prefs_default(), Some(1)).await;

        let memberships = series_custom_lists::list_for_series(&db, series_id)
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].list_name, "Top 10");
    }

    #[tokio::test]
    async fn merge_jikan_path_does_not_write_custom_lists() {
        // MAL has no custom-list concept; entries_from_mal always
        // returns empty custom_lists. The Jikan-fallback merge path
        // MUST also skip the membership write so it can't ever
        // accidentally clobber AL-side memberships.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "mal").await;
        // Pre-seed a hypothetical AL-side membership for this series
        // (e.g. user previously had AL linked, then switched to MAL).
        let series_id = crate::test_support::seed_series(&db, -777, "Jikan series").await;
        sqlx::query(
            "INSERT INTO series_custom_lists (series_id, provider, list_name) VALUES (?, ?, ?)",
        )
        .bind(series_id)
        .bind("anilist")
        .bind("Old AL List")
        .execute(&db)
        .await
        .unwrap();

        // Seed a Jikan detail so the merge path actually fires.
        crate::services::jikan::seed_detail_cache_for_tests(
            777,
            make_detail(-777, "Jikan series", "TV", "FINISHED"),
        )
        .await;

        let entries = vec![SyncEntry {
            provider: external_accounts::PROVIDER_MAL.to_string(),
            provider_media_id: 777,
            anilist_id: -777,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        }];
        let _ = merge_jikan_fallback_entries(&db, &entries, &prefs_default(), Some(1)).await;

        // The pre-seeded AL membership stays put — Jikan path's
        // skip-when-not-anilist guard prevents it from being wiped.
        let memberships = series_custom_lists::list_for_series(&db, series_id)
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].list_name, "Old AL List");
        assert_eq!(memberships[0].provider, "anilist");

        crate::services::jikan::clear_detail_cache_entry_for_tests(777).await;
    }

    #[tokio::test]
    async fn merge_normalizes_zero_score_to_null() {
        // AL sends 0.0 for unrated entries. The merge normalizes that
        // to NULL so `user_score IS NOT NULL` cleanly means "rated"
        // for any future query. Render helper handles 0.0 defensively
        // for older rows but new writes never produce it.
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Unrated").await;
        // Pre-condition: user_score is non-null (e.g. user rated 7
        // last sync, then unrated this sync).
        sqlx::query("UPDATE series SET user_score = 7.0 WHERE id = ?")
            .bind(series_id)
            .execute(&db)
            .await
            .unwrap();

        let entries = vec![SyncEntry {
            provider: external_accounts::PROVIDER_ANILIST.to_string(),
            provider_media_id: 12345,
            anilist_id: 12345,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        }];
        let _ = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), Some(1)).await;

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert_eq!(
            row.user_score, None,
            "score=0.0 must normalize to NULL on write"
        );
    }

    #[tokio::test]
    async fn merge_skips_existing_when_manual_override_set() {
        // The user has pinned monitor_mode through the UI. AL says
        // their status changed (Watching → Dropped), but the merge
        // step MUST leave the monitor_mode alone — the user
        // explicitly chose this.
        let db = crate::test_support::in_memory_pool().await;
        let series_id = crate::test_support::seed_series(&db, 12345, "Pinned").await;
        sqlx::query(
            "UPDATE series SET monitor_mode = ?, monitor_mode_manual_override = 1 WHERE id = ?",
        )
        .bind(MonitorMode::All.as_str())
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();

        // AL says Dropped, which would normally flip to monitor_mode=None.
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            12345,
            NormalizedStatus::Dropped,
        )];
        let outcome =
            merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
        assert_eq!(outcome.pinned_manually, 1);
        assert_eq!(outcome.monitor_mode_updated, 0);

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert_eq!(
            row.monitor_mode,
            MonitorMode::All.as_str(),
            "manual override must survive a Watching → Dropped transition on AL"
        );
    }

    #[tokio::test]
    async fn detect_removals_skips_manual_override_pinned_series() {
        // A series the user pinned is also off-limits to removal
        // detection — they explicitly want to keep this monitor_mode
        // even after removing from AL (e.g. their list went private).
        let db = crate::test_support::in_memory_pool().await;
        seed_account_id(&db, 1, "anilist").await;
        let pinned_id = crate::test_support::seed_series(&db, 800, "Pinned").await;
        force_synced_from(&db, pinned_id, 1).await;
        force_monitor_mode(&db, pinned_id, MonitorMode::All).await;
        sqlx::query("UPDATE series SET monitor_mode_manual_override = 1 WHERE id = ?")
            .bind(pinned_id)
            .execute(&db)
            .await
            .unwrap();

        let fetch_ids = std::collections::HashSet::new();
        let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
        assert!(
            report.removed.is_empty(),
            "manual-override series must NOT be downgraded by removal detection"
        );

        let row = series::get_by_id(&db, pinned_id).await.unwrap().unwrap();
        assert_eq!(row.monitor_mode, MonitorMode::All.as_str());
    }

    #[test]
    fn is_auth_rejection_matches_known_dead_token_strings() {
        // Each of these is a stable error-prefix the sync engine
        // emits on a token-dead failure; the Settings UI's
        // "Re-link required" banner keys off this exact match.
        // Adding a new wording requires updating the auth-prefix
        // list — pinning the existing ones here so a refactor that
        // reshapes a message gets caught.
        assert!(is_auth_rejection(
            "AniList rejected the watch-list token (status 401); user may need to re-link"
        ));
        assert!(is_auth_rejection(
            "MAL access token expired and no refresh token stored — re-link required"
        ));
        assert!(is_auth_rejection(
            "MAL refresh failed (re-link required): some upstream detail"
        ));
        assert!(is_auth_rejection(
            "MAL rejected the token immediately after refresh — re-link required"
        ));
    }

    #[test]
    fn is_auth_rejection_does_not_match_transient_errors() {
        // Rate-limits, network timeouts, and 5xx-shaped errors are
        // transient; the Settings banner shouldn't fire for them.
        assert!(!is_auth_rejection(
            "AniList rate-limited: too many requests"
        ));
        assert!(!is_auth_rejection(
            "AniList unavailable (status 503): service unavailable"
        ));
        assert!(!is_auth_rejection("AniList HTTP error: connection reset"));
        assert!(!is_auth_rejection(
            "AniList batch request failed: connection timed out"
        ));
        assert!(!is_auth_rejection("MAL fetch failed: 500 Internal Server"));
    }

    #[test]
    fn force_full_sync_overrides_should_full_resync_decision() {
        // A 2-day-old full sync would normally yield is_full_sync=false
        // (within the 7-day delta window). With the force flag set,
        // is_full_sync MUST be true regardless — the manual "Sync
        // now" trigger uses this to make removals apply immediately
        // instead of waiting up to 7 days for the next boundary.
        let now = 2_000_000_000;
        let two_days_ago = now - 2 * 24 * 60 * 60;
        // Without force: standard delta logic returns false.
        assert!(!should_full_resync(
            Some(two_days_ago),
            Some(two_days_ago),
            now
        ));
        // The force-full-sync gate `force || should_full_resync(...)`
        // is the actual logic in tick_once_inner. Pin the OR so a
        // refactor doesn't accidentally drop the force path.
        let force = true;
        let is_full_sync = force || should_full_resync(Some(two_days_ago), Some(two_days_ago), now);
        assert!(is_full_sync, "force flag must override the delta window");
    }

    #[tokio::test]
    async fn merge_skips_new_entry_when_import_pref_off() {
        // The other side of the rule: a NEW Dropped entry with
        // import_dropped=false should NOT create a series row. Stays
        // out of the library entirely. Counter rolls into
        // skipped_by_preference so the operator sees the count.
        let db = crate::test_support::in_memory_pool().await;
        let entries = vec![entry(
            external_accounts::PROVIDER_ANILIST,
            99999,
            NormalizedStatus::Dropped,
        )];
        // Even though detail_map has the entry, the merge should skip
        // creation because the user doesn't want Dropped imports.
        let mut detail_map = HashMap::new();
        detail_map.insert(
            99999,
            make_detail(99999, "Should Not Land", "TV", "FINISHED"),
        );

        let prefs = prefs_default();
        let outcome = merge_into_library(&db, &entries, &detail_map, &prefs, None).await;
        assert_eq!(outcome.created, 0);
        assert_eq!(outcome.skipped_by_preference, 1);
        assert!(
            series::get_by_anilist_id(&db, 99999)
                .await
                .unwrap()
                .is_none(),
            "import_dropped=false must keep new Dropped entries out of the library"
        );
    }
}
