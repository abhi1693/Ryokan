//! Settings → Indexers CRUD handlers (issue #28 PR B).
//!
//! Mirrors the shape of the groups + custom-formats settings
//! handlers: form-driven upsert + delete that redirect back to
//! the tab. The "test connection" path lands in a follow-up
//! commit since it needs the full search-pipeline integration to
//! be useful.

use axum::{
    Form, Json,
    extract::State,
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::indexers::{IndexerForm, KIND_NEWZNAB, KIND_TORZNAB, delete, insert, update};
use crate::models::log::LogCategory;
use crate::services::logger;

/// Form for create/update — `id == None` creates, `id == Some(n)`
/// updates row `n`. Mirrors CustomFormatUpsertForm shape.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerUpsertForm {
    pub id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub api_key: String,
    /// Sonarr-convention priority. Range 1-50; out-of-range coerces
    /// to 25. Empty string also coerces to 25.
    pub priority: Option<String>,
    /// HTML form checkboxes only POST when checked, so the field
    /// is `Option<String>` and presence-equivalent to true.
    pub enabled: Option<String>,
    pub is_private_tracker: Option<String>,
    /// Empty string = NULL (use default seed rules).
    pub seed_ratio: Option<String>,
    pub seed_time_minutes: Option<String>,
    pub min_seeders: Option<String>,
    pub request_timeout_secs: Option<String>,
    /// Multi-client routing pin — id of the row in
    /// `download_clients` this indexer routes to. Empty
    /// string = NULL (use the default client at grab time).
    pub download_client_id: Option<String>,
    /// multi-rss PR 1 — opt this indexer into the RSS sync
    /// fan-out. Checkbox; presence-equivalent to true.
    pub rss_enabled: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerDeleteForm {
    pub id: i64,
}

#[utoipa::path(
    post,
    path = "/settings/indexers/upsert",
    tag = "Settings",
    summary = "Create or update an indexer",
    description = "Form-driven upsert for the Settings → Indexers tab. Creates a new row when `id` is omitted; updates the row identified by `id` otherwise. Validates kind ∈ {torznab, newznab}, priority ∈ [1, 50], min_seeders ≥ 0. Out-of-range numerics coerce to safe defaults rather than rejecting the submission. Redirects back to the indexers tab.",
    responses(
        (status = 303, description = "Redirect back to the indexers tab"),
    ),
)]
pub async fn settings_indexers_upsert(
    State(state): State<AppState>,
    Form(form): Form<IndexerUpsertForm>,
) -> Response {
    let name = form.name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=indexers&err=Name+required").into_response();
    }
    let kind = match form.kind.as_str() {
        KIND_TORZNAB | KIND_NEWZNAB => form.kind.as_str(),
        _ => {
            return Redirect::to("/settings?tab=indexers&err=Invalid+kind").into_response();
        }
    };
    let url = form.url.trim();
    if url.is_empty() {
        return Redirect::to("/settings?tab=indexers&err=URL+required").into_response();
    }
    // PR #107 review fix #12: catch typos at save time rather
    // than at the next search. reqwest::Url::parse is what the
    // client uses internally; round-tripping it here surfaces
    // missing scheme / malformed host immediately.
    if reqwest::Url::parse(url).is_err() {
        return Redirect::to("/settings?tab=indexers&err=Invalid+URL+syntax").into_response();
    }
    let priority = parse_priority(&form.priority);
    let min_seeders = parse_optional_i32(&form.min_seeders, 1).max(0);
    let request_timeout_secs = parse_optional_secs(&form.request_timeout_secs);
    let api_key = form.api_key.trim();
    let download_client_id = parse_optional_i64(&form.download_client_id);

    // Protocol guard — torznab indexers route torrent magnets /
    // .torrent URLs; newznab indexers route NZB URLs. Pinning a
    // newznab indexer to a BT client (or vice versa, torznab → SAB)
    // surfaces at grab time as "client rejected URL" with no upfront
    // signal — better to refuse the save with a clear toast. Mirrors
    // Sonarr's per-indexer Protocol enum check.
    //
    // PR 112 review #1 (4th pass) — fail closed on transient DB
    // error. The earlier `if let Ok(Some(row))` shape silently
    // skipped the gate when get_by_id returned Err, which would
    // let a torznab→SAB pin slip through under a hiccup at save
    // time. Match Err explicitly with a "DB error: ...; please
    // retry" toast. Ok(None) still permits (row deleted between
    // page-load and submit is intentional).
    if let Some(client_id) = download_client_id {
        let row = match crate::models::download_clients::get_by_id(&state.db, client_id).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => None, // intentional: client deleted between page-load and submit
            Err(e) => {
                let msg = urlencoding::encode(&format!(
                    "Couldn't verify protocol pin (DB error: {e}); please retry."
                ))
                .into_owned();
                return Redirect::to(&format!("/settings?tab=indexers&err={msg}")).into_response();
            }
        };
        if let Some(row) = row {
            let indexer_proto = crate::services::download_client::protocol_for_indexer_kind(kind);
            let client_proto =
                crate::services::download_client::protocol_for_client_kind(&row.kind);
            if let (Some(ip), Some(cp)) = (indexer_proto, client_proto)
                && ip != cp
            {
                let msg = urlencoding::encode(&format!(
                    "Can't pin a {kind} indexer to a {} client (protocol mismatch — \
                     {kind} returns {ip} URLs, {} accepts {cp})",
                    row.kind, row.kind
                ))
                .into_owned();
                return Redirect::to(&format!("/settings?tab=indexers&err={msg}")).into_response();
            }
        }
    }

    let payload = IndexerForm {
        name,
        kind,
        url,
        api_key,
        priority,
        enabled: form.enabled.is_some(),
        is_private_tracker: form.is_private_tracker.is_some(),
        seed_ratio: parse_optional_f64(&form.seed_ratio),
        seed_time_minutes: parse_optional_i64(&form.seed_time_minutes),
        min_seeders,
        request_timeout_secs,
        download_client_id,
        rss_enabled: form.rss_enabled.is_some(),
    };

    let result = match form.id {
        Some(id) => update(&state.db, id, payload).await.map(|_| id),
        None => insert(&state.db, payload).await,
    };

    match result {
        Ok(id) => {
            let verb = if form.id.is_some() {
                "updated"
            } else {
                "added"
            };
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Indexer {verb}: {name} ({kind})"),
                &format!("id={id}, priority={priority}"),
            )
            .await;
            // PR #107 review fix #4: rebuild the IndexerCache so
            // the next search picks up the new/edited row without
            // a process restart.
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            let msg = urlencoding::encode(&format!("Indexer '{name}' {verb}")).into_owned();
            Redirect::to(&format!("/settings?tab=indexers&msg={msg}")).into_response()
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Indexer upsert failed",
                &e.to_string(),
            )
            .await;
            Redirect::to("/settings?tab=indexers&err=Save+failed").into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/settings/indexers/delete",
    tag = "Settings",
    summary = "Delete an indexer",
    description = "Removes the indexer row by id. Existing grabbed_torrents and pending_grabs rows referencing this indexer have their indexer_id NULLed in the same transaction, so grab history is preserved with the FK cleared. SQLite can't enforce a real ON DELETE SET NULL via ALTER TABLE so the model layer (`models::indexers::delete`) handles it explicitly.",
    responses(
        (status = 303, description = "Redirect back to the indexers tab"),
    ),
)]
pub async fn settings_indexers_delete(
    State(state): State<AppState>,
    Form(form): Form<IndexerDeleteForm>,
) -> Redirect {
    // PR #107 round-3 review fixes #2+#3: the SET-NULL UPDATEs
    // for grabbed_torrents.indexer_id + pending_grabs.indexer_id
    // are now folded into models::indexers::delete as a transaction
    // so all three statements succeed or fail atomically; previously
    // the handler ran them with `let _ = …` and a partial-NULL-out
    // could ride out a transient I/O error silently.
    // Capture the name before delete so the success toast can name
    // the row that was removed. A failed lookup falls back to the
    // numeric id; the delete itself is the source of truth.
    let display_name = crate::models::indexers::get_by_id(&state.db, form.id)
        .await
        .ok()
        .flatten()
        .map(|r| r.name)
        .unwrap_or_else(|| format!("id={}", form.id));
    match delete(&state.db, form.id).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Indexer deleted: {display_name} (id={})", form.id),
                "",
            )
            .await;
            // PR #107 review fix #4: same cache refresh as upsert.
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            let msg =
                urlencoding::encode(&format!("Indexer '{display_name}' deleted")).into_owned();
            Redirect::to(&format!("/settings?tab=indexers&msg={msg}"))
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Indexer delete failed",
                &e.to_string(),
            )
            .await;
            // PR #107 round-4 review fix #3: surface the failure
            // via `&err=` so the user sees an inline banner instead
            // of a quiet success-looking redirect. Mirrors the
            // upsert handler's "Save failed" pattern.
            Redirect::to("/settings?tab=indexers&err=Delete+failed")
        }
    }
}

// ── multi-rss commit G — Test RSS feed endpoint for indexers ────────

/// Body of the indexer-RSS Test request. Mirrors the direct-feed
/// shape — caller passes the indexer row id, handler runs a
/// single empty-`q` `?t=tvsearch` against it and returns the
/// item count + first title.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerRssTestForm {
    pub id: i64,
}

/// JSON envelope for the indexer-RSS Test response. Same shape
/// as the direct-feed Test response so the frontend toast can
/// share the rendering helper.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct IndexerRssTestResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_title: Option<String>,
}

#[utoipa::path(
    post,
    path = "/settings/indexers/test-rss",
    tag = "Settings",
    summary = "Test-fetch an indexer's RSS endpoint",
    description = "Fires a single `?t=tvsearch&cat=5070` (with empty `q`) request against the indexer identified by id and returns a JSON envelope describing the result: item count and first item's title. Used by the Settings → Indexers form's per-row Test RSS button. Indexer protocol kind is already known from the row (torznab/newznab → torrent/usenet) so no protocol detection step is needed here, unlike the direct-feed Test.",
    responses(
        (status = 200, description = "Test result envelope", body = IndexerRssTestResponse),
    ),
)]
pub async fn settings_indexers_test_rss(
    State(state): State<AppState>,
    Json(form): Json<IndexerRssTestForm>,
) -> Json<IndexerRssTestResponse> {
    // Look up the live `Arc<dyn Indexer>` from the in-memory
    // cache so the test fetch reuses the same reqwest client +
    // cooldown state the sync path uses.
    let snapshot = state.indexers.read().await.clone();
    let Some(indexer) = snapshot.iter().find(|i| i.id() == form.id).cloned() else {
        return Json(IndexerRssTestResponse {
            ok: false,
            error: Some(format!(
                "Indexer id={} not in cache (try Save before Test, or check Enabled)",
                form.id
            )),
            item_count: None,
            first_title: None,
        });
    };

    match crate::services::indexers::fetch_indexer_rss(&*indexer).await {
        Ok(items) => {
            let count = items.len() as i32;
            let first_title = items.first().map(|i| i.title.clone());
            Json(IndexerRssTestResponse {
                ok: true,
                error: None,
                item_count: Some(count),
                first_title,
            })
        }
        Err(err) => Json(IndexerRssTestResponse {
            ok: false,
            error: Some(err),
            item_count: None,
            first_title: None,
        }),
    }
}

/// Coerce the priority form field into the Sonarr-convention
/// range. Anything out of [1, 50] (or unparseable) lands at 25 —
/// the default — rather than rejecting the submission. Matches
/// the validate_* helpers in the parent settings module.
pub(crate) fn parse_priority(raw: &Option<String>) -> i32 {
    let parsed = raw
        .as_deref()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(25);
    parsed.clamp(1, 50)
}

fn parse_optional_i32(raw: &Option<String>, default: i32) -> i32 {
    raw.as_deref()
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<i32>().ok()
            }
        })
        .unwrap_or(default)
}

fn parse_optional_i64(raw: &Option<String>) -> Option<i64> {
    raw.as_deref().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<i64>().ok()
        }
    })
}

fn parse_optional_f64(raw: &Option<String>) -> Option<f64> {
    raw.as_deref().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<f64>().ok()
        }
    })
}

/// Per-indexer search timeout. Stored as `Option<i64>` (NULL =
/// use default). Out-of-range values (< 1s or > 600s) coerce to
/// None rather than persist a value that would force every
/// search to immediately timeout or block forever.
fn parse_optional_secs(raw: &Option<String>) -> Option<i64> {
    parse_optional_i64(raw).filter(|n| (1..=600).contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_priority_clamps_into_sonarr_range() {
        assert_eq!(parse_priority(&Some("0".into())), 1);
        assert_eq!(parse_priority(&Some("51".into())), 50);
        assert_eq!(parse_priority(&Some("25".into())), 25);
        assert_eq!(parse_priority(&Some("-100".into())), 1);
    }

    #[test]
    fn parse_priority_falls_back_to_25_on_unparseable() {
        assert_eq!(parse_priority(&None), 25);
        assert_eq!(parse_priority(&Some(String::new())), 25);
        assert_eq!(parse_priority(&Some("garbage".into())), 25);
        assert_eq!(parse_priority(&Some("3.14".into())), 25);
    }

    #[test]
    fn parse_optional_secs_filters_out_of_range_values() {
        // <1 or >600 → None (defensive: prevents a typo persisting
        // a 0s timeout that fails every search instantly, or a
        // 30000s value that blocks the auto-search loop forever).
        assert_eq!(parse_optional_secs(&Some("0".into())), None);
        assert_eq!(parse_optional_secs(&Some("601".into())), None);
        assert_eq!(parse_optional_secs(&Some("30".into())), Some(30));
    }

    #[test]
    fn parse_optional_i64_treats_empty_string_as_none() {
        assert_eq!(parse_optional_i64(&Some(String::new())), None);
        assert_eq!(parse_optional_i64(&Some("   ".into())), None);
        assert_eq!(parse_optional_i64(&Some("42".into())), Some(42));
    }

    #[test]
    fn parse_optional_f64_treats_empty_string_as_none() {
        assert_eq!(parse_optional_f64(&Some(String::new())), None);
        assert_eq!(parse_optional_f64(&Some("2.5".into())), Some(2.5));
    }

    /// PR G follow-up: protocol-mismatch validation on the indexer
    /// upsert path. Pinning a torznab indexer to a SAB client (or a
    /// newznab indexer to a BT client) used to silently save the row
    /// and only fail at grab time when the client rejected the URL.
    /// These tests pin the upfront-rejection shape so a future
    /// refactor can't drop the guard and re-introduce the silent-
    /// fail surface.
    mod protocol_guard {
        use super::super::*;
        use crate::models::download_clients::{DownloadClientForm, insert as insert_dc};
        use crate::test_support::{build_test_app_state, in_memory_pool};
        use axum::extract::{Form, State};

        fn extract_location(resp: axum::response::Response) -> String {
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_default()
        }

        fn upsert_form(kind: &str, dc_id: i64) -> IndexerUpsertForm {
            IndexerUpsertForm {
                id: None,
                name: "Test".into(),
                kind: kind.to_string(),
                url: "https://prowlarr.local/1/api".into(),
                api_key: "k".into(),
                priority: Some("25".into()),
                enabled: Some("on".into()),
                is_private_tracker: None,
                seed_ratio: None,
                seed_time_minutes: None,
                min_seeders: Some("1".into()),
                request_timeout_secs: None,
                download_client_id: Some(dc_id.to_string()),
                rss_enabled: None,
            }
        }

        async fn seed_clients(db: &sqlx::SqlitePool) -> (i64 /* qbit */, i64 /* sab */) {
            let qbit = insert_dc(
                db,
                DownloadClientForm {
                    name: "qBit",
                    kind: "qbittorrent",
                    url: "http://qbit.local",
                    username: "",
                    password: "",
                    label: "",
                    download_path: "",
                    enabled: true,
                    is_default: true,
                },
            )
            .await
            .expect("seed qbit");
            let sab = insert_dc(
                db,
                DownloadClientForm {
                    name: "SAB",
                    kind: "sabnzbd",
                    url: "http://sab.local",
                    username: "",
                    password: "key",
                    label: "tv",
                    download_path: "",
                    enabled: true,
                    is_default: false,
                },
            )
            .await
            .expect("seed sab");
            (qbit, sab)
        }

        #[tokio::test]
        async fn torznab_pinned_to_sab_is_rejected() {
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp =
                settings_indexers_upsert(State(state.clone()), Form(upsert_form("torznab", sab)))
                    .await;
            let location = extract_location(resp);
            assert!(
                location.contains("err=") && location.contains("protocol"),
                "expected protocol-mismatch err redirect, got: {location}"
            );
            // Row must NOT have been inserted.
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert!(
                rows.is_empty(),
                "torznab→SAB save must be rejected, not silently persisted: {rows:?}"
            );
        }

        #[tokio::test]
        async fn newznab_pinned_to_qbit_is_rejected() {
            let db = in_memory_pool().await;
            let (qbit, _sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp =
                settings_indexers_upsert(State(state.clone()), Form(upsert_form("newznab", qbit)))
                    .await;
            let location = extract_location(resp);
            assert!(
                location.contains("err=") && location.contains("protocol"),
                "expected protocol-mismatch err redirect, got: {location}"
            );
            assert!(
                crate::models::indexers::list_all(&state.db)
                    .await
                    .unwrap()
                    .is_empty(),
                "newznab→qBit save must be rejected"
            );
        }

        #[tokio::test]
        async fn torznab_pinned_to_qbit_succeeds() {
            // Positive test — same-protocol pair must save through.
            let db = in_memory_pool().await;
            let (qbit, _sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp =
                settings_indexers_upsert(State(state.clone()), Form(upsert_form("torznab", qbit)))
                    .await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=") && !location.contains("err="),
                "expected success redirect, got: {location}"
            );
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].download_client_id, Some(qbit));
        }

        #[tokio::test]
        async fn newznab_pinned_to_sab_succeeds() {
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp =
                settings_indexers_upsert(State(state.clone()), Form(upsert_form("newznab", sab)))
                    .await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=") && !location.contains("err="),
                "expected success redirect, got: {location}"
            );
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].download_client_id, Some(sab));
        }

        #[tokio::test]
        async fn no_pin_skips_validation() {
            // The "(use default)" path — empty download_client_id —
            // bypasses the protocol guard since there's no client
            // to validate against. Default-routing happens at grab
            // time per the existing pin-resolution chain.
            let db = in_memory_pool().await;
            let _ = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let mut form = upsert_form("torznab", 0);
            form.download_client_id = None;
            let resp = settings_indexers_upsert(State(state.clone()), Form(form)).await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=") && !location.contains("err="),
                "expected success redirect, got: {location}"
            );
        }

        #[tokio::test]
        async fn db_error_during_pin_lookup_fails_closed() {
            // PR 112 review #1 (4th pass) — a transient DB error on
            // the protocol-pin lookup must NOT silently skip the
            // gate (the prior `if let Ok(Some(row))` shape did this).
            // Provoke the error by closing the pool, then confirm
            // upsert returns a "DB error" toast and refuses the save.
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            db.close().await;
            let resp =
                settings_indexers_upsert(State(state.clone()), Form(upsert_form("torznab", sab)))
                    .await;
            let location = extract_location(resp);
            assert!(
                location.contains("err=")
                    && (location.contains("DB%20error") || location.contains("DB+error")),
                "expected fail-closed err redirect mentioning DB error, got: {location}"
            );
        }
    }

    /// Toast wording is user-facing — `?msg=Saved` was the
    /// pre-PR-108 default and didn't tell the user what
    /// happened. The current handler emits
    /// `Indexer '<name>' added` / `... updated` / `... deleted`
    /// so the toast reads naturally. These tests pin that
    /// surface in case a future refactor shortens or reformats
    /// the message.
    mod toast_format {
        use super::super::*;
        use crate::models::indexers::{IndexerForm, KIND_TORZNAB, insert};
        use crate::test_support::{build_test_app_state, in_memory_pool};
        use axum::extract::{Form, State};

        fn extract_location(resp: axum::response::Response) -> String {
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_default()
        }

        fn upsert_form(id: Option<i64>, name: &str) -> IndexerUpsertForm {
            IndexerUpsertForm {
                id,
                name: name.to_string(),
                kind: "torznab".to_string(),
                url: "https://prowlarr.local/1/api".to_string(),
                api_key: "k".to_string(),
                priority: Some("25".to_string()),
                enabled: Some("on".to_string()),
                is_private_tracker: None,
                seed_ratio: None,
                seed_time_minutes: None,
                min_seeders: Some("1".to_string()),
                request_timeout_secs: None,
                download_client_id: None,
                rss_enabled: None,
            }
        }

        #[tokio::test]
        async fn upsert_insert_toast_names_added_indexer() {
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let resp =
                settings_indexers_upsert(State(state), Form(upsert_form(None, "Test Indexer")))
                    .await;
            let location = extract_location(resp);
            // `'` percent-encodes to %27 via urlencoding::encode.
            assert!(
                location.contains("msg=Indexer%20%27Test%20Indexer%27%20added")
                    || location.contains("msg=Indexer+%27Test+Indexer%27+added"),
                "expected descriptive 'added' toast in redirect URL; got: {location}"
            );
        }

        #[tokio::test]
        async fn upsert_update_toast_names_updated_indexer() {
            let db = in_memory_pool().await;
            // Seed an existing row so the update branch fires.
            let row_id = insert(
                &db,
                IndexerForm {
                    name: "Original Name",
                    kind: KIND_TORZNAB,
                    url: "https://prowlarr.local/1/api",
                    api_key: "k",
                    priority: 25,
                    enabled: true,
                    is_private_tracker: false,
                    seed_ratio: None,
                    seed_time_minutes: None,
                    min_seeders: 1,
                    request_timeout_secs: None,
                    download_client_id: None,
                    rss_enabled: false,
                },
            )
            .await
            .expect("seed indexer");
            let state = build_test_app_state(db, None);
            let resp =
                settings_indexers_upsert(State(state), Form(upsert_form(Some(row_id), "Renamed")))
                    .await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=Indexer%20%27Renamed%27%20updated")
                    || location.contains("msg=Indexer+%27Renamed%27+updated"),
                "expected 'updated' toast naming the new value; got: {location}"
            );
        }

        #[tokio::test]
        async fn delete_toast_names_removed_indexer() {
            let db = in_memory_pool().await;
            let row_id = insert(
                &db,
                IndexerForm {
                    name: "Doomed",
                    kind: KIND_TORZNAB,
                    url: "https://prowlarr.local/1/api",
                    api_key: "k",
                    priority: 25,
                    enabled: true,
                    is_private_tracker: false,
                    seed_ratio: None,
                    seed_time_minutes: None,
                    min_seeders: 1,
                    request_timeout_secs: None,
                    download_client_id: None,
                    rss_enabled: false,
                },
            )
            .await
            .expect("seed indexer");
            let state = build_test_app_state(db, None);
            let resp =
                settings_indexers_delete(State(state), Form(IndexerDeleteForm { id: row_id }))
                    .await;
            // `delete` returns Redirect (not Response); `IntoResponse`
            // turns it into a Response that has the Location header.
            use axum::response::IntoResponse;
            let resp = resp.into_response();
            let location = extract_location(resp);
            assert!(
                location.contains("msg=Indexer%20%27Doomed%27%20deleted")
                    || location.contains("msg=Indexer+%27Doomed%27+deleted"),
                "expected 'deleted' toast naming the removed row; got: {location}"
            );
        }

        #[tokio::test]
        async fn delete_toast_falls_back_to_id_for_missing_row() {
            // A delete for a row that no longer exists (race or
            // stale tab) reaches the success path because SQLite's
            // `DELETE WHERE id = ?` is a no-op success on a
            // missing row, not an error. The handler's
            // pre-delete `get_by_id(...)` returns None, so
            // `display_name` falls back to `format!("id={}", id)`
            // and the toast becomes "Indexer 'id=9999' deleted".
            // The positive assertion below pins that fallback —
            // a future change that returned `Err(NotFound)`
            // for a missing row, or that dropped the id-fallback,
            // would surface here instead of slipping by under a
            // weaker `!contains("''")` check.
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let resp =
                settings_indexers_delete(State(state), Form(IndexerDeleteForm { id: 9999 })).await;
            use axum::response::IntoResponse;
            let resp = resp.into_response();
            let location = extract_location(resp);
            // `=` percent-encodes to `%3D` (uppercase via
            // `urlencoding::encode`); accept lowercase too in
            // case the encoder ever changes.
            assert!(
                location.contains("Indexer%20%27id%3D9999%27%20deleted")
                    || location.contains("Indexer+%27id%3D9999%27+deleted")
                    || location.contains("Indexer%20%27id%3d9999%27%20deleted")
                    || location.contains("Indexer+%27id%3d9999%27+deleted"),
                "expected id-based fallback in deleted-toast; got: {location}"
            );
        }
    }
}
