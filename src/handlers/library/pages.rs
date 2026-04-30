//! HTML page handlers for the library section.
//!
//! Split out of `handlers::library::mod`: `index` (home library list),
//! `needs_review_page` (cross-library needs-review feed), and
//! `series_detail` (the per-series page) plus their shared builders.
//! `build_episodes` is in here because it's called by both `series_detail`
//! and the `series_episodes_json` polling endpoint in the `episodes`
//! submodule.

use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse},
};
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, local_metadata, monitoring, series};
use crate::services::{
    anilist, artwork, jikan, kitsu, logger, media, monitoring as monitoring_service,
};

use super::reconcile::{
    force_kitsu_fallback_enabled, populate_series_cover_urls, resolve_series_context,
};
use super::{Episode, ErrorTemplate, IndexTemplate, RelationCard, RelationGroup, SeriesTemplate};

#[derive(Default, serde::Deserialize)]
pub struct LibraryIndexQuery {
    /// #62 PR D — `?list=<name>` filter. When present + non-empty,
    /// the index handler keeps only series whose
    /// `series_custom_lists` rows match. Echoed back to the
    /// template so the dropdown's selected-option state persists
    /// across navigations.
    #[serde(default)]
    pub list: Option<String>,
    /// `?search=<text>` library search. Case-insensitive substring
    /// match against `title_english` / `title_romaji` /
    /// `title_native`; composes with `list` (set both → series must
    /// satisfy both predicates).
    #[serde(default)]
    pub search: Option<String>,
    /// #62 PR E — `?sort=<key>` ordering. Currently supports
    /// `recent` (default; SQL `ORDER BY added_at DESC`) and `score`
    /// (user-score descending — only meaningful when an account is
    /// linked, so the dropdown is hidden otherwise; unrated series
    /// sink to the bottom). Anything unrecognized falls through to
    /// `recent`.
    #[serde(default)]
    pub sort: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LibraryIndexQuery>,
) -> Html<String> {
    // Fetch the library list and config concurrently — they're independent
    // and each was previously serialized on the other. `get_all` is the
    // larger query of the two so this shaves the smaller query's RTT off
    // the critical path.
    let (library_res, cfg_res) =
        tokio::join!(series::get_all(&state.db), config::get_config(&state.db),);
    let mut library = library_res.unwrap_or_default();
    let cfg = cfg_res.ok().flatten();

    populate_series_cover_urls(
        &state.db,
        &mut library,
        |item| item.id,
        |item, url| item.cover_url = url,
    )
    .await;

    // #62 PR C — pull the linked account's score_format so library
    // cards can render "You: X" badges per row. Empty string when
    // no account is linked, in which case Series::user_score_display
    // returns None and no badge renders.
    let score_format = crate::models::external_accounts::get_current(&state.db)
        .await
        .ok()
        .flatten()
        .map(|a| a.score_format)
        .unwrap_or_default();

    // #62 PR D — populate the filter dropdown + apply the active
    // filter. Distinct list names are alphabetized; empty result
    // means no memberships synced yet (template hides the dropdown).
    let custom_list_names = crate::models::series_custom_lists::distinct_list_names(&state.db)
        .await
        .unwrap_or_default();
    let custom_list_filter = q.list.unwrap_or_default();
    if !custom_list_filter.is_empty() {
        // In-memory filter against the just-loaded library. Cheaper
        // than a JOIN-based query when the library is already cached
        // — the per-series ids set is small enough that the
        // HashSet lookup on each row is sub-microsecond. A stale or
        // unknown `?list=foo` (e.g. user bookmarked the URL, then
        // synced away the last membership) yields an empty
        // matching_ids set and therefore an empty library — chosen
        // over silently dropping the filter so the dropdown's
        // still-selected value lines up with what the user sees,
        // making the staleness obvious instead of mysterious.
        let matching_ids: std::collections::HashSet<i64> =
            crate::models::series_custom_lists::series_ids_in_list(&state.db, &custom_list_filter)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
        library.retain(|s| matching_ids.contains(&s.id));
    }

    // Library search. Case-insensitive substring match against the
    // three title fields. Composes with the list filter (set both →
    // series must satisfy both predicates).
    let search_query = q.search.unwrap_or_default();
    if !search_query.trim().is_empty() {
        let needle = search_query.trim().to_lowercase();
        library.retain(|s| {
            s.title_english.to_lowercase().contains(&needle)
                || s.title_romaji.to_lowercase().contains(&needle)
                || s.title_native.to_lowercase().contains(&needle)
        });
    }

    // #62 PR E — sort-by-user-score. SQL already returned series
    // ordered by added_at DESC ("recent"); this is an opt-in
    // re-sort applied AFTER filters so the displayed order matches
    // the displayed set. NULL / 0.0 / negative user_score values
    // (unrated, manually-added pre-PR-C, etc.) sort to the bottom
    // so they don't crowd out the rated ones the user is presumably
    // looking at.
    // Sort selector. SQL already returned series ordered by
    // added_at DESC ("recent"); anything else is an opt-in re-sort
    // applied AFTER filters so the displayed order matches the
    // displayed set. Score-based sorts gate on `!score_format.is_empty()`
    // (an external account is linked); title sorts and oldest-first
    // are universal. Unknown keys fall through to "recent".
    let sort_key = q.sort.as_deref().unwrap_or("recent");
    // Tiebreaker for non-title primary sorts: title_english,
    // case-insensitive, with romaji/native fallback so an entry
    // missing English doesn't sort under everything.
    let title_key = |s: &series::Series| -> String {
        let raw = if !s.title_english.is_empty() {
            &s.title_english
        } else if !s.title_romaji.is_empty() {
            &s.title_romaji
        } else {
            &s.title_native
        };
        raw.to_lowercase()
    };
    let sort_value = match sort_key {
        "score" if !score_format.is_empty() => {
            // partial_cmp with NaN-safe ordering: any non-positive
            // or missing score becomes -1.0 so it sinks. Tiebreaker
            // on title keeps the order deterministic across renders
            // for series at the same score.
            library.sort_by(|a, b| {
                let av = a.user_score.filter(|s| *s > 0.0).unwrap_or(-1.0);
                let bv = b.user_score.filter(|s| *s > 0.0).unwrap_or(-1.0);
                bv.partial_cmp(&av)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| title_key(a).cmp(&title_key(b)))
            });
            "score".to_string()
        }
        "score_asc" if !score_format.is_empty() => {
            // Inverse: low → high. Unrated entries still sink — a
            // missing score isn't a 0, conceptually it's "no
            // opinion," and surfacing those above the user's
            // explicit ratings on either end of the range is
            // confusing.
            library.sort_by(|a, b| {
                let av = a.user_score.filter(|s| *s > 0.0).unwrap_or(f64::INFINITY);
                let bv = b.user_score.filter(|s| *s > 0.0).unwrap_or(f64::INFINITY);
                av.partial_cmp(&bv)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| title_key(a).cmp(&title_key(b)))
            });
            "score_asc".to_string()
        }
        "title_asc" => {
            library.sort_by_key(title_key);
            "title_asc".to_string()
        }
        "title_desc" => {
            library.sort_by_key(|s| std::cmp::Reverse(title_key(s)));
            "title_desc".to_string()
        }
        "oldest" => {
            // Sort directly on `added_at` (ISO-8601 from SQLite's
            // CURRENT_TIMESTAMP, lexicographically chronological)
            // rather than reverse-of-SQL-default — the latter would
            // break silently if a future caller's needs reshaped
            // `series::get_all`'s ORDER BY clause.
            library.sort_by(|a, b| {
                a.added_at
                    .cmp(&b.added_at)
                    .then_with(|| title_key(a).cmp(&title_key(b)))
            });
            "oldest".to_string()
        }
        _ => "recent".to_string(),
    };

    let template = IndexTemplate {
        page: "library".to_string(),
        library,
        title_language: cfg
            .map(|c| c.title_language)
            .unwrap_or_else(|| "english".to_string()),
        score_format,
        custom_list_names,
        custom_list_filter,
        search_query,
        sort_value,
    };
    Html(template.render().unwrap_or_default())
}

/// `/library/review` used to render its own page. It's now a System
/// tab (`/system?tab=review`) — redirect there so anything
/// bookmarked, linked, or cached still resolves.
///
/// Phase C / D of the hx-boost rollout: under `hx-boost` an `<a>`
/// click is fetched with `fetch`, which transparently follows 3xx
/// redirects — htmx never sees the redirect, only the final
/// destination's HTML. The pushState'd URL stays at the ORIGINAL
/// click target (`/library/review`) while the rendered content is
/// the destination (`/system?tab=review`), producing an awkward
/// URL/content mismatch in the address bar.
///
/// `htmx_aware_redirect_from_req` solves this: HTMX callers get
/// `200 OK` with `HX-Redirect: /system?tab=review`, which htmx
/// translates into a real `window.location` navigation that updates
/// both URL and content together. Plain (non-HTMX) callers — direct
/// browser nav, bookmarks, third-party links — fall through to the
/// `Redirect::permanent` path so search-engine cache invalidation
/// and deep-linking still work the way 308 promises.
pub async fn needs_review_page(
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    let is_htmx = req
        .headers()
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if is_htmx {
        crate::handlers::responses::htmx_aware_redirect(true, "/system?tab=review")
    } else {
        // Non-HTMX path keeps the 308 (vs the helper's 303) so
        // search engines and HTTP caches treat the redirect as
        // permanent — a 303 from `htmx_aware_redirect` would invite
        // re-fetching `/library/review` indefinitely.
        axum::response::Redirect::permanent("/system?tab=review").into_response()
    }
}

pub async fn series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Html<String> {
    // Fetch config alongside the metadata resolve so both the error
    // and success paths can reuse it. resolve_series_context typically
    // dominates (network round trip to AniList on cold cache), so the
    // cfg fetch overlaps with it for free.
    let (resolve_res, cfg_res) = tokio::join!(
        resolve_series_context(&state.db, request_id),
        config::get_config(&state.db),
    );
    let cfg = cfg_res.ok().flatten();
    let title_language_fallback = || {
        cfg.as_ref()
            .map(|c| c.title_language.clone())
            .unwrap_or_else(|| "english".to_string())
    };
    let (db_series, provider_id, mut detail) = match resolve_res {
        Ok(v) => v,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::AniList,
                &format!("Failed to fetch detail for {}", request_id),
                &e,
            )
            .await;
            let (title, message, tech_detail) = if e.contains("403") {
                (
                    "Metadata Provider Unavailable".to_string(),
                    "The metadata API is temporarily unavailable. This usually resolves itself within a few hours. Try again later.".to_string(),
                    e,
                )
            } else if e.contains("not found") || e.contains("Not Found") {
                (
                    "Series Not Found".to_string(),
                    format!(
                        "Could not find a series with ID {}. It may have been removed from the metadata provider.",
                        request_id
                    ),
                    e,
                )
            } else {
                (
                    "Something Went Wrong".to_string(),
                    "An error occurred while loading this series. Please try again.".to_string(),
                    e,
                )
            };
            let template = ErrorTemplate {
                page: "library".to_string(),
                title,
                message,
                detail: tech_detail,
                title_language: title_language_fallback(),
            };
            return Html(template.render().unwrap_or_default());
        }
    };
    let is_tracked = db_series.is_some();
    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();

    // Ensure monitoring rows first — this writes to DB, and `build_episodes`
    // below reads the monitored set, so these cannot run concurrently
    // without a read-your-writes race. Everything *after* this point is
    // read-only and fans out in parallel.
    let mut monitor_mode = "future".to_string();
    let mut monitor_mode_label = monitoring::MonitorMode::Future.label().to_string();
    let monitor_mode_manual_override = db_series
        .as_ref()
        .map(|s| s.monitor_mode_manual_override)
        .unwrap_or(false);
    if let Some(ref tracked) = db_series {
        if let Ok(summary) =
            monitoring_service::ensure_series_monitoring_rows(&state.db, tracked).await
        {
            monitor_mode = summary.mode.as_str().to_string();
            monitor_mode_label = summary.mode.label().to_string();
        } else {
            monitor_mode = tracked.monitor_mode.clone();
            monitor_mode_label = tracked.monitor_mode_enum().label().to_string();
        }
    }

    // #62 PR B — derive the "Sync from AL/MAL" dropdown option's
    // visibility + label. Only show when both (a) an account is
    // currently linked, and (b) this series row has a non-NULL
    // synced_from_external_account_id pointing at the same account.
    // Rule (b) keeps manually-added series (synced_from = NULL) from
    // showing an option that wouldn't do anything useful; if the
    // user later puts the manual series on their AL list, the next
    // sync stamps synced_from and the option appears.
    let synced_from = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT synced_from_external_account_id FROM series WHERE id = ?",
    )
    .bind(db_id.unwrap_or(0))
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten();
    let linked_account = crate::models::external_accounts::get_current(&state.db)
        .await
        .ok()
        .flatten();
    let can_sync_from_external_account = matches!(
        (linked_account.as_ref(), synced_from),
        (Some(acct), Some(sf)) if acct.id == sf
    );
    let sync_provider_label = match linked_account.as_ref().map(|a| a.provider.as_str()) {
        Some(crate::models::external_accounts::PROVIDER_ANILIST) => "AniList".to_string(),
        Some(crate::models::external_accounts::PROVIDER_MAL) => "MyAnimeList".to_string(),
        _ => String::new(),
    };
    // When the series is sync-tracked and the user hasn't pinned a
    // manual mode, the dropdown shows "Sync from AL/MAL" as selected.
    // Otherwise the option matching the current monitor_mode is
    // selected. Computed here so the template doesn't need a
    // multi-clause condition per option.
    let monitor_mode_select_value =
        if can_sync_from_external_account && !monitor_mode_manual_override {
            "sync".to_string()
        } else {
            monitor_mode.clone()
        };

    // #62 PR C — render the "You: X" badge string per the linked
    // account's score_format. Hidden when no account is linked, the
    // series has no user_score, or the score is the unrated
    // sentinel. Computed here so the template just renders the
    // already-formatted string.
    let user_score_display = match (db_series.as_ref(), linked_account.as_ref()) {
        (Some(row), Some(acct)) => {
            crate::services::user_score::format_user_score(row.user_score, &acct.score_format)
        }
        _ => None,
    };

    // #62 PR D — read AL custom-list memberships for the badge row.
    // Empty when this series isn't on any user-defined list; the
    // template hides the row in that case. Sorted alphabetically by
    // the model layer so the badge order is stable across renders.
    let custom_list_memberships = match db_id {
        Some(id) => crate::models::series_custom_lists::list_for_series(&state.db, id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.list_name)
            .collect(),
        None => Vec::new(),
    };

    // Fan out the four remaining independent read paths. Each one was
    // previously awaited serially — on a cold cache that meant 4+
    // sequential DB round trips + the build_episodes fs-walk + the
    // relation-group artwork lookups all stacked end to end. Running
    // them concurrently collapses the total wait to ~max(...) instead
    // of sum(...). cfg is fetched at the top of the handler alongside
    // resolve_series_context so it's already in scope here.
    let relation_groups_fut = build_relation_groups(&state.db, db_id, &detail);

    let detail_for_episodes = detail.clone();
    let db_for_episodes = state.db.clone();
    let folder_for_episodes = folder_name.clone();
    let episodes_fut = async move {
        // Pull media_root from config inside the task so build_episodes
        // can still run in parallel with the outer config fetch. The
        // extra `get_config` hit here is harmless — the WAL page cache
        // will serve it from memory after the first concurrent fetch.
        let media_root = config::get_config(&db_for_episodes)
            .await
            .ok()
            .flatten()
            .map(|c| c.media_root)
            .unwrap_or_default();
        let out = build_episodes(
            &db_for_episodes,
            &detail_for_episodes,
            db_id,
            &folder_for_episodes,
            &media_root,
        )
        .await;
        (out, media_root)
    };

    let cover_key = db_series.as_ref().map(|s| format!("series-{}-cover", s.id));
    let banner_key = db_series
        .as_ref()
        .map(|s| format!("series-{}-banner", s.id));
    let cover_url_src = detail.cover_url.clone();
    let banner_url_src = detail.banner_url.clone();
    let detail_id = detail.id;
    let detail_mal_id = detail.id_mal;
    let db_for_art = state.db.clone();
    let cover_fut = async move {
        if let Some(key) = cover_key {
            artwork::cached_or_source_url(&db_for_art, &key, &cover_url_src).await
        } else if detail_id != 0 {
            artwork::first_cached_url(
                &db_for_art,
                &[
                    artwork::provider_cover_key(detail_id, detail_mal_id),
                    format!("provider-{}-cover", detail_id),
                ],
                &cover_url_src,
            )
            .await
        } else {
            cover_url_src
        }
    };
    let db_for_banner = state.db.clone();
    let banner_fut = async move {
        if let Some(key) = banner_key {
            artwork::cached_or_source_url(&db_for_banner, &key, &banner_url_src).await
        } else if detail_id != 0 {
            artwork::first_cached_url(
                &db_for_banner,
                &[
                    artwork::provider_banner_key(detail_id, detail_mal_id),
                    format!("provider-{}-banner", detail_id),
                ],
                &banner_url_src,
            )
            .await
        } else {
            banner_url_src
        }
    };

    // #15b — last metadata refresh + the SQL-derived `is_fresh` flag,
    // folded into the existing concurrent fan-out so it doesn't add a
    // sequential round-trip on top. Cheap (indexed provider_id lookup,
    // WAL-cached) but the pattern of the surrounding handler is "every
    // independent read goes in the join!" so stick with that.
    //
    // Issue #106 — `is_fresh` (computed by SQLite at fetch time using
    // the same TTL constant as the periodic refresh task) is the
    // canonical staleness signal. Re-deriving it client-side from
    // `cached_at` would duplicate the SQL `CASE WHEN cached_at >=
    // datetime('now', '-12 hours')` calculation; reuse the value that
    // already came back from the query.
    let db_for_refresh = state.db.clone();
    let refresh_fut = async move {
        crate::models::metadata_cache::get_by_provider_id(&db_for_refresh, provider_id)
            .await
            .ok()
            .flatten()
            .map(|row| (row.cached_at, !row.is_fresh))
            .unwrap_or_default()
    };

    let (relation_groups, episodes_out, cover_url, banner_url, refresh_meta) = tokio::join!(
        relation_groups_fut,
        episodes_fut,
        cover_fut,
        banner_fut,
        refresh_fut,
    );
    let (metadata_refreshed_at, metadata_is_stale) = refresh_meta;
    let ((episodes, on_disk_count, downloaded_count, size_display, monitored_count), media_root) =
        episodes_out;
    detail.cover_url = cover_url;
    detail.banner_url = banner_url;

    let title_language = title_language_fallback();

    let ep_total = detail.effective_episode_count();
    // #15a — render AL and MAL links independently. AL link is hidden
    // for the Jikan-fallback sentinel case (detail.id < 0); MAL link is
    // hidden only when no MAL id is known.
    let anilist_url = if detail.id > 0 {
        format!("https://anilist.co/anime/{}", detail.id)
    } else {
        String::new()
    };
    let mal_url = detail
        .id_mal
        .filter(|id| *id > 0)
        .map(|id| format!("https://myanimelist.net/anime/{}", id))
        .unwrap_or_default();

    let all_monitored = ep_total > 0 && monitored_count >= ep_total;
    let allow_upgrades = db_series.as_ref().map(|s| s.allow_upgrades).unwrap_or(true);
    // PR E — default off (untracked series have no upgrade sweep
    // anyway, so the default is moot for the .unwrap_or() branch).
    let allow_pt_upgrades = db_series
        .as_ref()
        .map(|s| s.allow_pt_upgrades)
        .unwrap_or(false);
    let custom_query_tokens = db_series
        .as_ref()
        .map(|s| s.custom_query_tokens.clone())
        .unwrap_or_default();
    let restrict_to_uploader = db_series
        .as_ref()
        .map(|s| s.restrict_to_uploader.clone())
        .unwrap_or_default();
    let default_custom_query_tokens = cfg
        .as_ref()
        .map(|c| c.default_custom_query_tokens.clone())
        .unwrap_or_default();
    let default_restrict_to_uploader = cfg
        .as_ref()
        .map(|c| c.default_restrict_to_uploader.clone())
        .unwrap_or_default();
    let post_processing_enabled = cfg
        .as_ref()
        .map(|c| c.post_processing_enabled)
        .unwrap_or(false);
    let grab_preview_mode = cfg
        .as_ref()
        .map(|c| c.grab_preview_mode.clone())
        .unwrap_or_else(|| "batches_only".to_string());
    let template = SeriesTemplate {
        page: "library".to_string(),
        route_id: db_id.unwrap_or(provider_id),
        detail,
        is_tracked,
        db_id,
        folder_name,
        media_root,
        episodes,
        ep_total,
        on_disk_count,
        downloaded_count,
        size_display,
        title_language,
        relation_groups,
        anilist_url,
        mal_url,
        metadata_refreshed_at,
        metadata_is_stale,
        monitor_mode,
        monitor_mode_label,
        monitor_mode_manual_override,
        can_sync_from_external_account,
        sync_provider_label,
        monitor_mode_select_value,
        user_score_display,
        custom_list_memberships,
        monitored_count,
        all_monitored,
        allow_upgrades,
        allow_pt_upgrades,
        custom_query_tokens,
        restrict_to_uploader,
        default_custom_query_tokens,
        default_restrict_to_uploader,
        post_processing_enabled,
        grab_preview_mode,
    };
    Html(template.render().unwrap_or_default())
}

/// Maximum number of missing trailing Jikan episodes we'll tolerate before
/// falling back to Kitsu. MAL typically lags AniList's airing schedule by 1-2
/// episodes for long-running series (One Piece being the canonical case).
/// Without this tolerance, every One Piece page load re-runs the Kitsu title
/// search (`best_candidate` hits the Kitsu HTTP API before checking the
/// episode cache) to backfill 1-2 trailing episodes. And for long-running
/// shows Kitsu over-counts anyway — it lists episodes past the actual aired
/// count — so falling back here wouldn't even give us accurate titles.
const JIKAN_MAL_LAG_TOLERANCE: i32 = 10;

fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    let missing = (1..=ep_count)
        .filter(|ep_num| !has_jikan_title(*ep_num))
        .count() as i32;
    missing > JIKAN_MAL_LAG_TOLERANCE
}

/// Build the episode list for a single series (no chain walking).
pub(super) async fn build_episodes(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    db_id: Option<i64>,
    folder_name: &str,
    media_root: &str,
) -> (Vec<Episode>, i32, i32, String, i32) {
    let ep_count = detail.effective_episode_count();
    // Fan out the four independent pre-fetches in parallel:
    //   1. disk file walk (blocking pool)
    //   2. cached episode metadata map (DB, with a fallback path)
    //   3. force_kitsu_fallback config flag (DB)
    //   4. monitored-episode set (DB, only when the series is tracked)
    //   5. per-episode quality tags (DB, only when the series is tracked)
    let disk_files_fut = media::scan_series_folder(media_root, folder_name);

    let detail_id = detail.id;
    let cached_eps_fut = async move {
        if let Some(sid) = db_id {
            let rows = local_metadata::get_episode_map_for_series(db, sid)
                .await
                .unwrap_or_default();
            if rows.is_empty() && detail_id != 0 {
                local_metadata::get_episode_map_for_provider(db, detail_id)
                    .await
                    .unwrap_or_default()
            } else {
                rows
            }
        } else if detail_id != 0 {
            local_metadata::get_episode_map_for_provider(db, detail_id)
                .await
                .unwrap_or_default()
        } else {
            HashMap::new()
        }
    };

    let monitored_fut = async move {
        match db_id {
            Some(id) => monitoring::get_monitored_episode_numbers(db, id)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::HashSet<i32>>(),
            None => std::collections::HashSet::new(),
        }
    };

    let quality_tags_fut = async move {
        match db_id {
            Some(id) => episode_tags::get_for_series(db, id)
                .await
                .unwrap_or_default(),
            None => std::collections::HashMap::new(),
        }
    };

    let (disk_files, cached_eps, force_kitsu_fallback, monitored_lookup, quality_tags) = tokio::join!(
        disk_files_fut,
        cached_eps_fut,
        force_kitsu_fallback_enabled(db),
        monitored_fut,
        quality_tags_fut,
    );
    let cached_matches_force =
        !force_kitsu_fallback || cached_eps.values().any(|ep| ep.source == "kitsu");
    let use_cached_eps = !cached_eps.is_empty() && cached_matches_force;

    let episodic_format = !matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA");
    // Issue #56: airing series whose episode total isn't known yet
    // (typical for MAL-fed currently-airing entries — Jikan reports
    // `episodes: null`) need the Jikan episodes endpoint as the *source*
    // of episode rows, not just titles. Without the `is_airing` arm an
    // ONA-format airing show like JoJo SBR ends up with `episodic_format
    // = false` AND `ep_count == 0`, so Jikan is skipped, the main
    // 1..=ep_count render loop emits nothing, and the page reads as a
    // zero-episode series even though `/anime/{id}/episodes` would have
    // returned the aired list.
    let is_airing_status = matches!(detail.status.as_str(), "RELEASING" | "CURRENTLY_AIRING");
    let should_fetch_jikan = !use_cached_eps
        && detail.id_mal.is_some()
        && (episodic_format || ep_count > 1 || is_airing_status);
    let jikan_eps = if should_fetch_jikan {
        jikan::fetch_episode_titles_for_detail(db, detail).await
    } else {
        HashMap::new()
    };

    // Promote the larger of (fresh Jikan fetch, locally-cached episode
    // map) into ep_count for airing series whose total wasn't known.
    // The downstream render loop (`for ep_num in 1..=ep_count`), the
    // template's `ep_total > 0` section gate, and the monitoring
    // counters all key off ep_count, so without this the fetched
    // episodes stay invisible.
    //
    // Both arms are needed: `jikan_eps` is only populated when
    // `should_fetch_jikan` fires, which requires `!use_cached_eps`. On
    // the cached path Jikan was skipped and `jikan_eps` stays empty, so
    // the promotion from `jikan_eps.len()` alone would be a no-op —
    // leaving an airing series rendered empty on every revisit after
    // the initial sync populated the local episode map.
    let ep_count = ep_count.max(jikan_eps.len() as i32).max(if use_cached_eps {
        cached_eps.len() as i32
    } else {
        0
    });

    let should_try_kitsu = !use_cached_eps
        && ep_count > 1
        && (force_kitsu_fallback
            || episode_needs_kitsu_backfill(ep_count.max(0), |ep_num| {
                jikan_eps
                    .get(&ep_num)
                    .map(|info| !info.title.trim().is_empty())
                    .unwrap_or(false)
            }));
    let kitsu_eps: HashMap<i32, kitsu::EpisodeInfo> = if should_try_kitsu {
        kitsu::fetch_episode_titles_fallback(
            db,
            &[
                detail.title_english.clone(),
                detail.title_romaji.clone(),
                detail.title_native.clone(),
            ],
            detail.season_year,
            detail.episodes,
        )
        .await
    } else {
        HashMap::new()
    };

    let is_tracked = db_id.is_some();

    let mut episodes = Vec::new();
    let mut on_disk_count = 0i32;
    let mut downloaded_count = 0i32;
    let mut total_size: u64 = 0;
    let mut monitored_count = 0i32;

    for ep_num in 1..=ep_count.max(0) {
        let disk_match = disk_files.iter().find(|f| {
            if let Some(s) = f.season_number {
                s == 1 && f.episode_number == ep_num
            } else {
                f.episode_number == ep_num
            }
        });

        let (on_disk, quality, size_display, filename) = match disk_match {
            Some(f) => (
                true,
                f.quality.clone(),
                f.size_display.clone(),
                f.filename.clone(),
            ),
            None => (false, String::new(), String::new(), String::new()),
        };

        if on_disk {
            on_disk_count += 1;
            if let Some(f) = disk_match {
                total_size += f.size_bytes;
            }
        }

        let use_series_fallback = ep_count <= 1;
        let fallback_title = if use_series_fallback {
            preferred_title(
                &detail.title_english,
                &detail.title_romaji,
                &detail.title_native,
            )
        } else {
            String::new()
        };
        let fallback_romaji = if use_series_fallback {
            non_empty_or(&detail.title_romaji, &fallback_title)
        } else {
            String::new()
        };
        let fallback_english = if use_series_fallback {
            non_empty_or(&detail.title_english, &fallback_title)
        } else {
            String::new()
        };
        let fallback_native = if use_series_fallback {
            non_empty_or(&detail.title_native, &fallback_title)
        } else {
            String::new()
        };

        let (ep_title, ep_title_romaji, ep_title_english, ep_title_native, ep_aired) =
            if use_cached_eps {
                if let Some(info) = cached_eps.get(&ep_num) {
                    (
                        non_empty_or(&info.title, &fallback_title),
                        non_empty_or(&info.title_romaji, &fallback_romaji),
                        non_empty_or(&info.title_english, &fallback_english),
                        non_empty_or(&info.title_native, &fallback_native),
                        info.aired.clone(),
                    )
                } else {
                    (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        String::new(),
                    )
                }
            } else if force_kitsu_fallback {
                if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                    let t = if !kitsu_info.title.trim().is_empty() {
                        kitsu_info.title.clone()
                    } else {
                        fallback_title.clone()
                    };
                    (t.clone(), t.clone(), t.clone(), t, kitsu_info.aired.clone())
                } else {
                    match jikan_eps.get(&ep_num) {
                        Some(info) if !info.title.trim().is_empty() => (
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.aired.clone(),
                        ),
                        Some(info) => (
                            fallback_title.clone(),
                            fallback_romaji.clone(),
                            fallback_english.clone(),
                            fallback_native.clone(),
                            info.aired.clone(),
                        ),
                        None => (
                            fallback_title,
                            fallback_romaji,
                            fallback_english,
                            fallback_native,
                            String::new(),
                        ),
                    }
                }
            } else {
                match jikan_eps.get(&ep_num) {
                    Some(info) if !info.title.trim().is_empty() => (
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.aired.clone(),
                    ),
                    Some(info) => (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        info.aired.clone(),
                    ),
                    None => {
                        // Try Kitsu fallback for episode title/air date.
                        if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                            let t = if !kitsu_info.title.trim().is_empty() {
                                kitsu_info.title.clone()
                            } else {
                                fallback_title.clone()
                            };
                            (t.clone(), t.clone(), t.clone(), t, kitsu_info.aired.clone())
                        } else {
                            (
                                fallback_title,
                                fallback_romaji,
                                fallback_english,
                                fallback_native,
                                String::new(),
                            )
                        }
                    }
                }
            };

        let monitored = monitored_lookup.contains(&ep_num);
        if monitored {
            monitored_count += 1;
        }

        // Quality display: disk file quality takes precedence; fall back to grab tag.
        let (display_quality, quality_state) = if !quality.is_empty() {
            (quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&ep_num) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };

        let tag = quality_tags.get(&ep_num);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);

        let downloaded = on_disk || quality_state == "completed";
        if downloaded {
            downloaded_count += 1;
        }
        episodes.push(Episode {
            number: ep_num,
            title: ep_title,
            title_romaji: ep_title_romaji,
            title_english: ep_title_english,
            title_native: ep_title_native,
            aired: ep_aired,
            on_disk,
            downloaded,
            quality: display_quality,
            quality_state,
            size_display,
            filename,
            can_auto_search: is_tracked,
            monitored,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
        });
    }

    // Surface episodes the main 1..=ep_count loop didn't render. Two
    // cases:
    //   1. ep_count == 0 — movies or airing shows with no episodes yet;
    //      the main loop emits no rows, so every disk file lands here.
    //   2. ep_count > 0 but a release partitioned the series into more
    //      files than AniList's reported episode count. Canonical case:
    //      the [smol] Owarimonogatari BD splits the 48-min aired ep 1
    //      back into two ~24-min files, so S1 has 13 files on disk vs
    //      AL's 12 eps. Auto-expand backfills a grab-tag row for the
    //      overflow ep at grab time AND routes the file to the parent
    //      folder at post-process time. Both pre-import ("downloading"
    //      row from the grab tag) and post-import ("imported" row from
    //      the disk file) need to render — without this pass, the main
    //      loop only iterated 1..=ep_count and the overflow was
    //      orphaned in either state. See issue #45.
    let mut rendered_eps: std::collections::HashSet<i32> =
        episodes.iter().map(|e| e.number).collect();

    // Pass 1: on-disk files past ep_count. Takes precedence — a file
    // on disk carries size/filename/quality that a bare grab tag
    // doesn't, and we want the "imported" state to win over any stale
    // "grabbed" tag if the user somehow hits this for both sources.
    for f in &disk_files {
        // Match the main loop's season filter on the ep_count > 0 path:
        // only render season 1 / unseasoned files. Specials/ or S02
        // files under a tracked series folder aren't part of the main
        // episode list. The ep_count == 0 path historically rendered
        // every file regardless of season — preserve that behavior to
        // avoid regressions for movies and airing-with-no-episodes shows.
        if ep_count > 0
            && let Some(s) = f.season_number
            && s != 1
        {
            continue;
        }
        if f.episode_number <= 0 {
            continue;
        }
        if rendered_eps.contains(&f.episode_number) {
            continue;
        }

        on_disk_count += 1;
        downloaded_count += 1;
        total_size += f.size_bytes;
        let monitored = monitored_lookup.contains(&f.episode_number);
        if monitored {
            monitored_count += 1;
        }
        let (display_quality, quality_state) = if !f.quality.is_empty() {
            (f.quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&f.episode_number) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };
        let tag = quality_tags.get(&f.episode_number);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);
        rendered_eps.insert(f.episode_number);
        episodes.push(Episode {
            number: f.episode_number,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            aired: String::new(),
            on_disk: true,
            // This branch only runs when the file already exists under
            // media_root (on_disk=true), so `downloaded` is
            // unconditionally true regardless of tag state.
            downloaded: true,
            quality: display_quality,
            quality_state,
            size_display: f.size_display.clone(),
            filename: f.filename.clone(),
            can_auto_search: is_tracked,
            monitored,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
        });
    }

    // Pass 2: grab-tag rows past ep_count with no matching disk file
    // yet. This is what makes the overflow row render as "downloading"
    // immediately after the batch is queued — auto-expand writes the
    // grab tag, the torrent is still downloading so nothing is on disk,
    // and without this pass the row would be invisible until post-
    // processing imports it.
    if ep_count > 0 {
        for (&ep_num, tag) in quality_tags.iter() {
            if ep_num <= ep_count {
                continue;
            }
            if rendered_eps.contains(&ep_num) {
                continue;
            }

            let monitored = monitored_lookup.contains(&ep_num);
            if monitored {
                monitored_count += 1;
            }
            // `downloaded` tracks completed-state episodes; an overflow
            // tag in 'grabbed' state is mid-download so it counts only
            // when the tag has already been flipped to 'completed' by
            // post-processing. Mirrors the main loop's treatment.
            let downloaded = tag.state == "completed";
            if downloaded {
                downloaded_count += 1;
            }
            rendered_eps.insert(ep_num);
            episodes.push(Episode {
                number: ep_num,
                title: String::new(),
                title_romaji: String::new(),
                title_english: String::new(),
                title_native: String::new(),
                aired: String::new(),
                on_disk: false,
                downloaded,
                quality: tag.quality_tag.clone(),
                quality_state: tag.state.clone(),
                size_display: String::new(),
                filename: String::new(),
                can_auto_search: is_tracked,
                monitored,
                class_source: tag.source.clone(),
                class_resolution: tag.resolution.clone(),
                class_is_remux: tag.is_remux,
                class_is_bdmv: tag.is_bdmv,
                class_web_kind: tag.web_kind.clone(),
                manual_override: tag.manual_override,
                needs_review: tag.needs_review,
            });
        }
    }

    episodes.sort_by_key(|e| std::cmp::Reverse(e.number));

    let size_display = format_size(total_size);
    (
        episodes,
        on_disk_count,
        downloaded_count,
        size_display,
        monitored_count,
    )
}

fn relation_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mal_id) = mal_id {
        format!("mal:{mal_id}")
    } else {
        format!("provider:{provider_id}")
    }
}

/// Resolve the best link ID for a relation card.  If the related entry is
/// tracked in the library (by AniList ID or MAL ID), return the DB series ID
/// so the link always navigates to `/series/<db_id>`.  Otherwise fall back to
/// the provider ID (which may be negative for MAL-sourced entries, but the
/// detail resolver in `resolve_series_context` knows how to handle that).
async fn resolve_relation_card_id(db: &SqlitePool, provider_id: i64, mal_id: Option<i64>) -> i64 {
    // Try AniList ID first (positive IDs).
    if provider_id > 0
        && let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await
    {
        return row.id;
    }
    // Try MAL ID.
    if let Some(mid) = mal_id
        && let Ok(Some(row)) = series::get_by_mal_id(db, mid).await
    {
        return row.id;
    }
    // For MAL-sourced entries, the anilist_id column stores -mal_id.
    if provider_id < 0
        && let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await
    {
        return row.id;
    }
    provider_id
}

fn relation_richness(rel: &anilist::RelatedEntry) -> i32 {
    let mut score = 0;
    if !rel.cover_url.trim().is_empty() {
        score += 4;
    }
    if !rel.format.trim().is_empty() && rel.format != "TBA" {
        score += 2;
    }
    if !rel.status.trim().is_empty() && rel.status != "TBA" {
        score += 2;
    }
    if rel.episodes.unwrap_or(0) > 0 {
        score += 1;
    }
    if !preferred_title(&rel.title_english, &rel.title_romaji, &rel.title_native)
        .trim()
        .is_empty()
    {
        score += 1;
    }
    score
}

fn merge_relation_metadata(
    primary: &anilist::RelatedEntry,
    fallback: &anilist::RelatedEntry,
) -> anilist::RelatedEntry {
    let mut merged = primary.clone();

    if merged.title_romaji.trim().is_empty() {
        merged.title_romaji = fallback.title_romaji.clone();
    }
    if merged.title_english.trim().is_empty() {
        merged.title_english = fallback.title_english.clone();
    }
    if merged.title_native.trim().is_empty() {
        merged.title_native = fallback.title_native.clone();
    }
    if merged.cover_url.trim().is_empty() {
        merged.cover_url = fallback.cover_url.clone();
    }
    if merged.format.trim().is_empty() || merged.format == "TBA" {
        merged.format = fallback.format.clone();
    }
    if merged.status.trim().is_empty() || merged.status == "TBA" {
        merged.status = fallback.status.clone();
        merged.status_display = fallback.status_display.clone();
    }
    if merged.episodes.is_none() || merged.episodes == Some(0) {
        merged.episodes = fallback.episodes;
    }
    if merged.season_year.is_none() {
        merged.season_year = fallback.season_year;
    }
    if merged.id_mal.is_none() {
        merged.id_mal = fallback.id_mal;
    }
    if merged.media_type.trim().is_empty() {
        merged.media_type = fallback.media_type.clone();
    }

    merged
}

/// Group the detail's relations by type for display as cards.
async fn build_relation_groups(
    db: &SqlitePool,
    db_id: Option<i64>,
    detail: &anilist::AnimeDetail,
) -> Vec<RelationGroup> {
    let cached_relations = if let Some(series_id) = db_id {
        let rows = local_metadata::get_relations_for_series(db, series_id)
            .await
            .unwrap_or_default();
        if rows.is_empty() && detail.id != 0 {
            local_metadata::get_relations_for_provider(db, detail.id)
                .await
                .unwrap_or_default()
        } else {
            rows
        }
    } else if detail.id != 0 {
        local_metadata::get_relations_for_provider(db, detail.id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Treat the current AniList detail payload as the canonical relation graph whenever it is
    // available. Cached relation rows can be stale from older MAL/Jikan hydration passes, which is
    // how the same title ends up rendered twice under two different relation tags.
    let has_authoritative_relations = !detail.relations.is_empty();
    let mut relations = if has_authoritative_relations {
        detail.relations.clone()
    } else {
        cached_relations.clone()
    };

    if has_authoritative_relations {
        let by_identity: HashMap<String, usize> = relations
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|(idx, r)| (relation_identity_key(r.id, r.id_mal), idx))
            .collect();

        for cached in cached_relations {
            if !matches!(cached.media_type.as_str(), "ANIME" | "MUSIC") {
                continue;
            }
            let key = relation_identity_key(cached.id, cached.id_mal);
            let Some(idx) = by_identity.get(&key).copied() else {
                continue;
            };
            let merged = merge_relation_metadata(&relations[idx], &cached);
            relations[idx] = merged;
        }
    }

    if !has_authoritative_relations && (detail.id != 0 || detail.id_mal.is_some()) {
        let existing_relation_keys: std::collections::HashSet<String> = relations
            .iter()
            .filter(|r| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|r| relation_identity_key(r.id, r.id_mal))
            .collect();
        let incoming =
            local_metadata::get_incoming_relations_for_provider(db, detail.id, detail.id_mal)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| {
                    !existing_relation_keys.contains(&relation_identity_key(r.id, r.id_mal))
                })
                .collect::<Vec<_>>();
        relations.extend(incoming);
    }

    // Build identity key for the current series so we can filter self-references.
    let self_key = relation_identity_key(detail.id, detail.id_mal);

    let mut deduped: Vec<anilist::RelatedEntry> = Vec::new();
    let mut deduped_index: HashMap<(String, String), usize> = HashMap::new();
    for related in relations {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        // Skip self-references: relations that point back to the current series.
        let related_key = relation_identity_key(related.id, related.id_mal);
        if related_key == self_key {
            continue;
        }
        let normalized_type =
            local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let key = (related_key, normalized_type);
        if let Some(idx) = deduped_index.get(&key).copied() {
            if relation_richness(&deduped[idx]) < relation_richness(&related) {
                deduped[idx] = related;
            }
        } else {
            deduped_index.insert(key, deduped.len());
            deduped.push(related);
        }
    }
    let relations = deduped;

    let type_order = [
        "PREQUEL",
        "SEQUEL",
        "SIDE_STORY",
        "ALTERNATIVE",
        "SUMMARY",
        "FULL_STORY",
        "SPIN_OFF",
        "OTHER",
        "CHARACTER",
        "PARENT",
        "ADAPTATION",
    ];

    // Resolve the per-relation card_id + cover_url concurrently.
    let mut join_set: tokio::task::JoinSet<(usize, i64, String)> = tokio::task::JoinSet::new();
    for (idx, related) in relations.iter().enumerate() {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        let db = db.clone();
        let rel_id = related.id;
        let rel_mal = related.id_mal;
        let rel_cover = related.cover_url.clone();
        join_set.spawn(async move {
            let card_id = resolve_relation_card_id(&db, rel_id, rel_mal).await;
            let cover_url = if let Some(series_id) = db_id {
                artwork::first_cached_url(
                    &db,
                    &[
                        artwork::series_relation_cover_key(series_id, rel_id, rel_mal),
                        format!("series-{}-relation-{}-cover", series_id, rel_id),
                        artwork::provider_cover_key(rel_id, rel_mal),
                        format!("provider-{}-cover", rel_id),
                    ],
                    &rel_cover,
                )
                .await
            } else if rel_id != 0 || rel_mal.is_some() {
                artwork::first_cached_url(
                    &db,
                    &[
                        artwork::provider_cover_key(rel_id, rel_mal),
                        format!("provider-{}-cover", rel_id),
                    ],
                    &rel_cover,
                )
                .await
            } else {
                rel_cover
            };
            (idx, card_id, cover_url)
        });
    }

    let mut resolved: HashMap<usize, (i64, String)> = HashMap::new();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((idx, card_id, cover_url)) => {
                resolved.insert(idx, (card_id, cover_url));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "build_relation_groups: relation resolver task failed; skipping one relation card"
                );
            }
        }
    }

    let mut groups: HashMap<String, Vec<RelationCard>> = HashMap::new();

    for (idx, related) in relations.iter().enumerate() {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        let Some((card_id, cover_url)) = resolved.remove(&idx) else {
            continue;
        };

        let normalized_relation_type =
            local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let cards = groups.entry(normalized_relation_type).or_default();

        cards.push(RelationCard {
            id: card_id,
            title: preferred_title(
                &related.title_english,
                &related.title_romaji,
                &related.title_native,
            ),
            title_romaji: related.title_romaji.clone(),
            title_english: related.title_english.clone(),
            title_native: related.title_native.clone(),
            cover_url,
            format: related.format.clone(),
            status: related.status.clone(),
            episodes: related.episodes,
        });
    }

    let mut result: Vec<RelationGroup> = groups
        .into_iter()
        .map(|(rel_type, mut entries)| {
            entries.sort_by(|a, b| {
                let a_title = a.title.to_ascii_lowercase();
                let b_title = b.title.to_ascii_lowercase();
                a_title
                    .cmp(&b_title)
                    .then_with(|| {
                        a.title_romaji
                            .to_ascii_lowercase()
                            .cmp(&b.title_romaji.to_ascii_lowercase())
                    })
                    .then_with(|| a.id.cmp(&b.id))
            });
            let label = format_relation_label(&rel_type);
            RelationGroup {
                relation_type: rel_type,
                label,
                entries,
            }
        })
        .collect();

    result.sort_by_key(|g| {
        type_order
            .iter()
            .position(|t| *t == g.relation_type)
            .unwrap_or(99)
    });
    result
}

fn format_relation_label(rel_type: &str) -> String {
    match rel_type {
        "PREQUEL" => "Prequel".to_string(),
        "SEQUEL" => "Sequel".to_string(),
        "SIDE_STORY" => "Side Story".to_string(),
        "ALTERNATIVE" => "Alternative".to_string(),
        "SUMMARY" => "Summary".to_string(),
        "FULL_STORY" => "Full Story".to_string(),
        "SPIN_OFF" => "Spin Off".to_string(),
        "OTHER" => "Other".to_string(),
        "CHARACTER" => "Character".to_string(),
        "PARENT" => "Parent".to_string(),
        "ADAPTATION" => "Adaptation".to_string(),
        other => other.replace('_', " "),
    }
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    if !value.trim().is_empty() {
        value.to_string()
    } else {
        fallback.to_string()
    }
}

fn preferred_title(english: &str, romaji: &str, native: &str) -> String {
    if !english.is_empty() {
        english.to_string()
    } else if !romaji.is_empty() {
        romaji.to_string()
    } else {
        native.to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.1} GiB", gb)
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::series;

    fn unique_media_root(suffix: &str) -> std::path::PathBuf {
        let nonce = format!(
            "ryokan_pages_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            suffix,
        );
        let root = std::env::temp_dir().join(nonce);
        std::fs::create_dir_all(&root).expect("create media root");
        root
    }

    fn empty_anime_detail(
        id: i64,
        title_english: &str,
        episodes: Option<i32>,
    ) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            duration: Some(24),
            season: String::new(),
            season_year: Some(2015),
            end_year: Some(2015),
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

    /// Issue #45: a BD release can partition a series into more files
    /// than AniList reports (Owarimonogatari S1 — AL says 12 eps, the
    /// [smol] BD has 13 files because it splits the 48-min aired ep 1
    /// back into two halves). Before the fix, `build_episodes` only
    /// looped 1..=ep_count, so file 13 was routed to disk by
    /// auto-expand but never rendered in the UI. The fix surfaces
    /// any on-disk file with ep > ep_count as its own row.
    #[tokio::test]
    async fn build_episodes_surfaces_on_disk_files_beyond_anilist_episode_count() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21320,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("series upsert");

        // Write 13 synthetic episode files — ep 13 exceeds AL's count.
        let media_root = unique_media_root("surface_beyond_count");
        let series_folder = media_root.join("Owarimonogatari");
        std::fs::create_dir_all(&series_folder).expect("create series dir");
        for ep in 1..=13 {
            let fname = format!("Owarimonogatari - S01E{:02} - Episode.mkv", ep);
            std::fs::write(series_folder.join(&fname), b"x").expect("write ep file");
        }

        // AL reports 12 eps (the on-air ep 1 was a 48-min merged episode).
        let detail = empty_anime_detail(21320, "Owarimonogatari", Some(12));

        let (episodes, on_disk_count, downloaded_count, _size, _monitored) = build_episodes(
            &db,
            &detail,
            Some(series_id),
            "Owarimonogatari",
            media_root.to_str().expect("media root str"),
        )
        .await;

        // Sorted desc by number, so ep 13 is first.
        assert_eq!(
            episodes.len(),
            13,
            "expected 13 rows (1..=12 from AL count + 13 from disk overflow), got {}",
            episodes.len()
        );
        let ep13 = episodes
            .iter()
            .find(|e| e.number == 13)
            .expect("ep 13 row present");
        assert!(ep13.on_disk, "ep 13 must render as on_disk");
        assert_eq!(on_disk_count, 13, "on_disk_count must include the overflow");
        assert_eq!(downloaded_count, 13, "downloaded_count same");

        // Cleanup (best effort).
        std::fs::remove_dir_all(&media_root).ok();
    }

    /// Regression guard: when every on-disk file falls within AL's
    /// ep_count, the surface-beyond-count pass must not duplicate rows
    /// the main loop already rendered.
    #[tokio::test]
    async fn build_episodes_does_not_duplicate_rows_when_disk_matches_count() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 999,
                mal_id: None,
                title: "Test Series",
                title_romaji: "Test Series",
                title_english: "Test Series",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2020),
                end_year: Some(2020),
            },
        )
        .await
        .expect("series upsert");

        let media_root = unique_media_root("no_duplicates");
        let series_folder = media_root.join("Test Series");
        std::fs::create_dir_all(&series_folder).expect("create series dir");
        for ep in 1..=12 {
            let fname = format!("Test Series - S01E{:02} - Episode.mkv", ep);
            std::fs::write(series_folder.join(&fname), b"x").expect("write ep file");
        }

        let detail = empty_anime_detail(999, "Test Series", Some(12));

        let (episodes, _, _, _, _) = build_episodes(
            &db,
            &detail,
            Some(series_id),
            "Test Series",
            media_root.to_str().expect("media root str"),
        )
        .await;

        assert_eq!(episodes.len(), 12, "no duplicates: exactly 12 rows");

        std::fs::remove_dir_all(&media_root).ok();
    }

    /// Issue #45 follow-up: during the download the overflow file isn't
    /// on disk yet, but auto-expand has already written a grab-tag row
    /// for it. `build_episodes` must surface that tag as a row (in
    /// 'grabbed' state) so the user sees the extra episode's download
    /// progress immediately — not just after post-processing runs.
    #[tokio::test]
    async fn build_episodes_surfaces_grab_tags_beyond_ep_count_without_disk_file() {
        use crate::services::source::ClassificationResult;

        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21262,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("series upsert");

        // Write a grab tag for ep 13 (AL-overflow) — simulates what
        // auto_expand::expand_from_files does when it backfills a tag
        // for a parent file whose parsed ep exceeds AL's count.
        crate::models::episode_tags::record_grab(
            &db,
            series_id,
            13,
            &ClassificationResult::unknown(),
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            "smol",
            0,
            true,
        )
        .await
        .expect("record_grab for ep 13");

        // Empty media root — torrent is still downloading, nothing
        // has landed in the library folder yet.
        let media_root = unique_media_root("surfaces_grab_tag_no_disk");
        let series_folder = media_root.join("Owarimonogatari");
        std::fs::create_dir_all(&series_folder).expect("create series dir");

        let detail = empty_anime_detail(21262, "Owarimonogatari", Some(12));

        let (episodes, on_disk_count, downloaded_count, _size, _monitored) = build_episodes(
            &db,
            &detail,
            Some(series_id),
            "Owarimonogatari",
            media_root.to_str().expect("media root str"),
        )
        .await;

        assert_eq!(
            episodes.len(),
            13,
            "expected 13 rows (1..=12 from AL + overflow E13 from grab tag), got {}",
            episodes.len()
        );
        let ep13 = episodes
            .iter()
            .find(|e| e.number == 13)
            .expect("ep 13 row present from grab tag");
        assert!(!ep13.on_disk, "no disk file yet, so on_disk must be false");
        assert!(!ep13.downloaded, "tag state is 'grabbed', not 'completed'");
        assert_eq!(ep13.quality_state, "grabbed");
        assert_eq!(on_disk_count, 0, "nothing on disk yet");
        assert_eq!(downloaded_count, 0, "nothing completed yet");

        std::fs::remove_dir_all(&media_root).ok();
    }

    // ── Pure-helper coverage ──────────────────────────────────────────
    //
    // The async/DB-bound `build_episodes` tests above pin the heaviest
    // user-visible flows. The helpers in this section are the small
    // pure functions the page renderers fan out to — they're the
    // load-bearing invariants every relation card / episode list /
    // size-display string passes through. None had unit tests before
    // this commit.

    /// Zero-init `RelatedEntry`. Tests mutate only the fields that
    /// matter to the case so the assertion focus is on what's being
    /// pinned, not on a wall of empty positional args.
    fn default_relation() -> anilist::RelatedEntry {
        anilist::RelatedEntry {
            id: 1,
            id_mal: None,
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: String::new(),
            status: String::new(),
            status_display: String::new(),
            episodes: None,
            relation_type: String::new(),
            season_year: None,
            media_type: String::new(),
        }
    }

    // ── format_relation_label ────────────────────────────────────────

    #[test]
    fn format_relation_label_known_types_get_friendly_names() {
        assert_eq!(format_relation_label("PREQUEL"), "Prequel");
        assert_eq!(format_relation_label("SEQUEL"), "Sequel");
        assert_eq!(format_relation_label("SIDE_STORY"), "Side Story");
        assert_eq!(format_relation_label("SPIN_OFF"), "Spin Off");
    }

    #[test]
    fn format_relation_label_unknown_type_replaces_underscores() {
        // AL adds new relation_type variants periodically (most
        // recently `CONTAINS`). The fallback turns underscores into
        // spaces so the new variant renders readably without a code
        // change.
        assert_eq!(format_relation_label("BRAND_NEW_TYPE"), "BRAND NEW TYPE");
        assert_eq!(format_relation_label(""), "");
    }

    // ── non_empty_or ──────────────────────────────────────────────────

    #[test]
    fn non_empty_or_uses_value_when_non_empty() {
        assert_eq!(non_empty_or("real", "fallback"), "real");
    }

    #[test]
    fn non_empty_or_falls_back_on_whitespace() {
        // Trim before the empty-check — `"   "` is a fallback case,
        // not a value the user intended to display.
        assert_eq!(non_empty_or("", "fallback"), "fallback");
        assert_eq!(non_empty_or("   ", "fallback"), "fallback");
        assert_eq!(non_empty_or("\t\n", "fallback"), "fallback");
    }

    // ── preferred_title ──────────────────────────────────────────────

    #[test]
    fn preferred_title_prefers_english_then_romaji_then_native() {
        // Order is fixed: english > romaji > native. A regression in
        // the priority would break every "Show: <title>" label across
        // the UI.
        assert_eq!(preferred_title("Eng", "Rom", "Nat"), "Eng");
        assert_eq!(preferred_title("", "Rom", "Nat"), "Rom");
        assert_eq!(preferred_title("", "", "Nat"), "Nat");
        assert_eq!(preferred_title("", "", ""), "");
    }

    // ── format_size ──────────────────────────────────────────────────

    #[test]
    fn format_size_zero_returns_empty_string() {
        // The "size unknown / not yet measured" sentinel renders blank
        // rather than `"0 MiB"` which would clutter every ungrabbed
        // episode row.
        assert_eq!(format_size(0), "");
    }

    #[test]
    fn format_size_uses_mib_under_1_gib() {
        // 100 MiB → "100 MiB"; integer-rounded.
        assert_eq!(format_size(100 * 1024 * 1024), "100 MiB");
        // 500 MiB.
        assert_eq!(format_size(500 * 1024 * 1024), "500 MiB");
    }

    #[test]
    fn format_size_uses_gib_at_or_above_1_gib() {
        // Boundary: exactly 1 GiB → "1.0 GiB". One decimal precision.
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
        // Typical BD episode (~1.5 GiB).
        assert_eq!(
            format_size((1.5_f64 * 1024.0 * 1024.0 * 1024.0) as u64),
            "1.5 GiB"
        );
        // Full season pack.
        assert_eq!(format_size(20 * 1024 * 1024 * 1024), "20.0 GiB");
    }

    // ── relation_identity_key ────────────────────────────────────────

    #[test]
    fn relation_identity_key_prefers_mal_over_provider() {
        // MAL ID is more stable across provider rebinds — when both
        // are available, key on MAL so a re-link doesn't re-key the
        // relation cache.
        assert_eq!(relation_identity_key(42, Some(99)), "mal:99");
    }

    #[test]
    fn relation_identity_key_falls_back_to_provider() {
        assert_eq!(relation_identity_key(42, None), "provider:42");
    }

    #[test]
    fn relation_identity_key_handles_negative_provider_id() {
        // Negative-provider sentinel for MAL-fallback rows should
        // still produce a stable key, not panic on formatting.
        assert_eq!(relation_identity_key(-99, None), "provider:-99");
    }

    // ── relation_richness ────────────────────────────────────────────

    #[test]
    fn relation_richness_zero_for_empty_relation() {
        assert_eq!(relation_richness(&default_relation()), 0);
    }

    #[test]
    fn relation_richness_scores_each_field() {
        // The formula awards: cover=4, format=2, status=2, episodes=1,
        // title=1. Pin each via a relation that has only that field.
        let mut cover_only = default_relation();
        cover_only.cover_url = "url".to_string();
        assert_eq!(relation_richness(&cover_only), 4);

        let mut format_only = default_relation();
        format_only.format = "TV".to_string();
        assert_eq!(relation_richness(&format_only), 2);

        let mut status_only = default_relation();
        status_only.status = "FINISHED".to_string();
        assert_eq!(relation_richness(&status_only), 2);

        let mut episodes_only = default_relation();
        episodes_only.episodes = Some(12);
        assert_eq!(relation_richness(&episodes_only), 1);

        let mut title_only = default_relation();
        title_only.title_english = "Eng".to_string();
        assert_eq!(relation_richness(&title_only), 1);
    }

    #[test]
    fn relation_richness_treats_tba_as_missing() {
        // AL emits "TBA" for unscheduled metadata; the richness
        // function discounts it as "no signal" rather than counting
        // it as present.
        let mut tba = default_relation();
        tba.title_english = "Eng".to_string();
        tba.format = "TBA".to_string();
        tba.status = "TBA".to_string();
        // Only the title contributes (1).
        assert_eq!(relation_richness(&tba), 1);
    }

    #[test]
    fn relation_richness_zero_episodes_does_not_count() {
        // `episodes: Some(0)` (e.g., an unscheduled cour) contributes
        // nothing — the gate is `> 0`.
        let mut zero_eps = default_relation();
        zero_eps.title_english = "Eng".to_string();
        zero_eps.episodes = Some(0);
        assert_eq!(relation_richness(&zero_eps), 1);
    }

    // ── merge_relation_metadata ──────────────────────────────────────

    #[test]
    fn merge_relation_metadata_keeps_primary_when_complete() {
        // Primary has every field — fallback's data is never read.
        let mut primary = default_relation();
        primary.title_english = "Eng".to_string();
        primary.title_romaji = "Rom".to_string();
        primary.cover_url = "url".to_string();
        primary.format = "TV".to_string();
        primary.status = "FINISHED".to_string();
        primary.episodes = Some(12);
        primary.season_year = Some(2024);
        primary.id_mal = Some(99);
        primary.media_type = "ANIME".to_string();

        let mut fallback = default_relation();
        fallback.title_english = "Other".to_string();
        fallback.format = "OVA".to_string();
        fallback.episodes = Some(1);
        fallback.id_mal = Some(1);

        let merged = merge_relation_metadata(&primary, &fallback);
        assert_eq!(merged.title_english, "Eng");
        assert_eq!(merged.format, "TV");
        assert_eq!(merged.episodes, Some(12));
        assert_eq!(merged.id_mal, Some(99));
    }

    #[test]
    fn merge_relation_metadata_fills_empty_fields_from_fallback() {
        // Primary missing every field — every fallback value lands.
        let primary = default_relation();
        let mut fallback = default_relation();
        fallback.title_english = "Eng".to_string();
        fallback.title_romaji = "Rom".to_string();
        fallback.cover_url = "url".to_string();
        fallback.format = "TV".to_string();
        fallback.status = "FINISHED".to_string();
        fallback.episodes = Some(12);
        fallback.season_year = Some(2024);
        fallback.id_mal = Some(99);
        fallback.media_type = "ANIME".to_string();

        let merged = merge_relation_metadata(&primary, &fallback);
        assert_eq!(merged.title_english, "Eng");
        assert_eq!(merged.title_romaji, "Rom");
        assert_eq!(merged.cover_url, "url");
        assert_eq!(merged.format, "TV");
        assert_eq!(merged.status, "FINISHED");
        assert_eq!(merged.episodes, Some(12));
        assert_eq!(merged.season_year, Some(2024));
        assert_eq!(merged.id_mal, Some(99));
        assert_eq!(merged.media_type, "ANIME");
    }

    #[test]
    fn merge_relation_metadata_treats_tba_as_replaceable() {
        // TBA is a placeholder, not data. Both the format and status
        // arms have a `|| field == "TBA"` clause so a fallback with
        // real metadata wins over a TBA primary.
        let mut primary = default_relation();
        primary.title_english = "Eng".to_string();
        primary.format = "TBA".to_string();
        primary.status = "TBA".to_string();

        let mut fallback = default_relation();
        fallback.format = "TV".to_string();
        fallback.status = "FINISHED".to_string();

        let merged = merge_relation_metadata(&primary, &fallback);
        assert_eq!(merged.format, "TV");
        assert_eq!(merged.status, "FINISHED");
    }

    #[test]
    fn merge_relation_metadata_replaces_zero_episodes_with_fallback() {
        // `episodes == Some(0)` is treated as "unknown" for merge
        // purposes — same as `None`. A fallback with real data wins.
        let mut primary = default_relation();
        primary.title_english = "Eng".to_string();
        primary.format = "TV".to_string();
        primary.episodes = Some(0);

        let mut fallback = default_relation();
        fallback.episodes = Some(12);

        let merged = merge_relation_metadata(&primary, &fallback);
        assert_eq!(merged.episodes, Some(12));
    }

    // ── episode_needs_kitsu_backfill ─────────────────────────────────

    #[test]
    fn episode_needs_kitsu_backfill_short_series_never_backfills() {
        // 1-episode series (movies / OVAs) never trigger the Kitsu
        // round-trip, even if Jikan returned nothing — the backfill
        // overhead isn't worth it for one missing title.
        for ep_count in [0, 1] {
            assert!(!episode_needs_kitsu_backfill(ep_count, |_| false));
        }
    }

    #[test]
    fn episode_needs_kitsu_backfill_under_tolerance_skips() {
        // 12-episode series, 5 missing — under the 10-ep tolerance.
        // Skip the backfill: Jikan/MAL is allowed to lag a handful of
        // recent episodes on a still-airing show without forcing the
        // Kitsu HTTP round-trip.
        let missing_eps: std::collections::HashSet<i32> = (1..=5).collect();
        assert!(!episode_needs_kitsu_backfill(12, |ep| {
            !missing_eps.contains(&ep)
        }));
    }

    #[test]
    fn episode_needs_kitsu_backfill_over_tolerance_triggers() {
        // 24-episode series, 11 missing — over the 10-ep tolerance.
        // Backfill fires.
        let missing_eps: std::collections::HashSet<i32> = (1..=11).collect();
        assert!(episode_needs_kitsu_backfill(24, |ep| {
            !missing_eps.contains(&ep)
        }));
    }

    #[test]
    fn episode_needs_kitsu_backfill_complete_jikan_skips() {
        // All titles present — no backfill needed.
        assert!(!episode_needs_kitsu_backfill(24, |_| true));
    }

    // ── should_persist_detail_cache (handlers/library/reconcile) ─────
    //
    // Tested here rather than in reconcile.rs because the helper is
    // private — keeping the test next to other library-handler
    // helpers means the suite has one obvious home.

    #[test]
    fn should_persist_detail_cache_sentinel_anilist_id_always_persists() {
        // Negative anilist_id is the Jikan-fallback sentinel
        // (-mal_id). For these rows the AL detail can never match
        // (the row has no AL identity yet); the cache write is the
        // only way the metadata-cache → relations chain ever
        // populates. Always persist.
        let detail = empty_anime_detail(123, "Show", None);
        assert!(super::super::reconcile::should_persist_detail_cache_for_test(-999, &detail));
        // Zero anilist_id (theoretically impossible after the
        // negative-ID-sentinel sweep, but defensive) also persists.
        assert!(super::super::reconcile::should_persist_detail_cache_for_test(0, &detail));
    }

    #[test]
    fn should_persist_detail_cache_real_anilist_id_requires_match() {
        let detail = empty_anime_detail(42, "Show", None);
        // Match → persist.
        assert!(super::super::reconcile::should_persist_detail_cache_for_test(42, &detail));
        // Mismatch → don't persist (the detail is for a different AL entry).
        assert!(
            !super::super::reconcile::should_persist_detail_cache_for_test(
                42,
                &empty_anime_detail(99, "Show", None)
            )
        );
        // Detail.id = 0 → don't persist (defensive, matches the
        // `id > 0` guard the AL parse path enforces).
        assert!(
            !super::super::reconcile::should_persist_detail_cache_for_test(
                42,
                &empty_anime_detail(0, "Show", None)
            )
        );
    }

    // ── normalize_system_tab (handlers/system) ───────────────────────

    #[test]
    fn normalize_system_tab_known_tabs_pass_through() {
        for tab in ["scoring", "debug", "rss", "tasks", "review", "credits"] {
            assert_eq!(
                crate::handlers::system::normalize_system_tab_for_test(Some(tab.into())),
                tab
            );
        }
    }

    #[test]
    fn normalize_system_tab_help_alias_resolves_to_scoring() {
        // Legacy alias from when scoring rules used to live on a
        // dedicated /system?tab=help page. Pinning the aliasing so
        // the redirect-via-tab convention can't drift away from
        // existing bookmarks.
        assert_eq!(
            crate::handlers::system::normalize_system_tab_for_test(Some("help".into())),
            "scoring"
        );
    }

    #[test]
    fn normalize_system_tab_unknown_or_missing_defaults_to_logs() {
        // Logs is the safest landing — the user can always see what's
        // going on from the logs tab.
        assert_eq!(
            crate::handlers::system::normalize_system_tab_for_test(None),
            "logs"
        );
        assert_eq!(
            crate::handlers::system::normalize_system_tab_for_test(Some("garbage".into())),
            "logs"
        );
        assert_eq!(
            crate::handlers::system::normalize_system_tab_for_test(Some("".into())),
            "logs"
        );
    }
}
