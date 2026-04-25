//! Provider-shape conversion + status-to-monitor-mode mapping. Pure
//! functions, no DB and no network — every input comes from a
//! provider response or a config row, every output is a vector or an
//! enum value the merge engine can act on.
//!
//! `resolve_mal_anilist_ids` does touch the anibridge cache, but
//! anibridge::lookup_anilist_by_mal is a process-local in-memory
//! lookup at call time (the network fetch happens in the periodic
//! anibridge_refresh background task, not here).

use crate::models::external_accounts::{self, ImportPreferences};
use crate::models::monitoring::MonitorMode;
use crate::services::{anibridge, anilist, mal};

use super::types::{NormalizedStatus, SyncEntry};

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
