//! Settings → Connections → Downloads CRUD handlers (multi-client
//! refactor). Companion module to `models::download_clients`.
//!
//! Surface mirrors the indexers handler shape: form-driven upsert +
//! delete + set-default that redirect back to the Connections tab.
//! A separate JSON test endpoint at `/api/download-clients/test`
//! lets the user verify a configuration before saving without
//! mutating any DB row.

use axum::{
    Form, Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::models::download_clients::{DownloadClientForm, delete, insert, set_default, update};
use crate::models::log::LogCategory;
use crate::services::download_client::{
    DownloadClient, deluge, qbittorrent, rtorrent, sabnzbd, transmission,
};
use crate::services::logger;

/// `kind` discriminators accepted on the form. Mirrors the values
/// `services::download_client::rebuild_clients_cache` dispatches on
/// — keep these strings in sync if a new client is added.
const KIND_QBITTORRENT: &str = "qbittorrent";
const KIND_DELUGE: &str = "deluge";
const KIND_TRANSMISSION: &str = "transmission";
const KIND_RTORRENT: &str = "rtorrent";
const KIND_SABNZBD: &str = "sabnzbd";

fn is_known_kind(kind: &str) -> bool {
    matches!(
        kind,
        KIND_QBITTORRENT | KIND_DELUGE | KIND_TRANSMISSION | KIND_RTORRENT | KIND_SABNZBD
    )
}

/// Form payload for create/update. `id == None` creates a new row;
/// `Some(n)` updates row `n`. Empty / unsanitized strings — the
/// model layer trims and trims_end_matches('/') the URL +
/// download_path before persisting.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientUpsertForm {
    pub id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Label / category — qBit category, Deluge label, etc.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub download_path: String,
    /// Checkbox semantics: only POSTed when checked.
    pub enabled: Option<String>,
    pub is_default: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientIdForm {
    pub id: i64,
}

#[utoipa::path(
    post,
    path = "/settings/download-clients/upsert",
    tag = "Settings",
    summary = "Create or update a download client",
    description = "Form-driven upsert for the Connections → Downloads list. Creates a new row when `id` is omitted; updates the row identified by `id` otherwise. Validates kind ∈ {qbittorrent, deluge, transmission, rtorrent, sabnzbd}. Refreshes the in-process pool so the new/edited client is usable on the next grab without a process restart.",
    responses(
        (status = 303, description = "Redirect back to the Connections tab"),
    ),
)]
pub async fn settings_download_clients_upsert(
    State(state): State<AppState>,
    Form(form): Form<DownloadClientUpsertForm>,
) -> Response {
    let name = form.name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=integrations&err=Name+required").into_response();
    }
    if !is_known_kind(&form.kind) {
        return Redirect::to("/settings?tab=integrations&err=Invalid+client+kind").into_response();
    }
    let url = form.url.trim();
    if url.is_empty() {
        return Redirect::to("/settings?tab=integrations&err=URL+required").into_response();
    }
    if reqwest::Url::parse(url).is_err() {
        return Redirect::to("/settings?tab=integrations&err=Invalid+URL+syntax").into_response();
    }

    let payload = DownloadClientForm {
        name,
        kind: form.kind.as_str(),
        url,
        username: form.username.trim(),
        password: &form.password,
        label: form.label.trim(),
        download_path: form.download_path.as_str(),
        enabled: form.enabled.is_some(),
        is_default: form.is_default.is_some(),
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
                &format!("Download client {verb}: {name} ({})", form.kind),
                &format!("id={id}"),
            )
            .await;
            crate::services::download_client::rebuild_clients_cache(
                &state.download_clients,
                &state.db,
            )
            .await;
            let msg = urlencoding::encode(&format!("Download client '{name}' {verb}")).into_owned();
            Redirect::to(&format!("/settings?tab=integrations&msg={msg}")).into_response()
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client upsert failed",
                &e.to_string(),
            )
            .await;
            Redirect::to("/settings?tab=integrations&err=Save+failed").into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/settings/download-clients/delete",
    tag = "Settings",
    summary = "Delete a download client",
    description = "Removes the download_clients row by id. Indexer pins (`indexers.download_client_id`) and the Nyaa pin (`config.nyaa_download_client_id`) that referenced it are NULLed in the same transaction so dangling pins don't silently fall through to the default at grab time.",
    responses(
        (status = 303, description = "Redirect back to the Connections tab"),
    ),
)]
pub async fn settings_download_clients_delete(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DownloadClientIdForm>,
) -> Response {
    let display_name = crate::models::download_clients::get_by_id(&state.db, form.id)
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
                &format!("Download client deleted: {display_name} (id={})", form.id),
                "",
            )
            .await;
            crate::services::download_client::rebuild_clients_cache(
                &state.download_clients,
                &state.db,
            )
            .await;
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            // HTMX migration (issue #129) — empty 200 lets the row form's
            // `hx-target="closest tr" hx-swap="outerHTML"` remove the row
            // from the table without a full page reload.
            if is_htmx {
                StatusCode::OK.into_response()
            } else {
                let msg = urlencoding::encode(&format!("Download client '{display_name}' deleted"))
                    .into_owned();
                Redirect::to(&format!("/settings?tab=integrations&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client delete failed",
                &e.to_string(),
            )
            .await;
            if is_htmx {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            } else {
                Redirect::to("/settings?tab=integrations&err=Delete+failed").into_response()
            }
        }
    }
}

#[utoipa::path(
    post,
    path = "/settings/download-clients/set-default",
    tag = "Settings",
    summary = "Mark a download client as the default",
    description = "Flips `is_default = 1` on the targeted row and clears the flag on every other row in one transaction. Used by the per-row \"Set default\" button on the Connections → Downloads list.",
    responses(
        (status = 303, description = "Redirect back to the Connections tab"),
    ),
)]
pub async fn settings_download_clients_set_default(
    State(state): State<AppState>,
    Form(form): Form<DownloadClientIdForm>,
) -> Redirect {
    let display_name = crate::models::download_clients::get_by_id(&state.db, form.id)
        .await
        .ok()
        .flatten()
        .map(|r| r.name)
        .unwrap_or_else(|| format!("id={}", form.id));
    match set_default(&state.db, form.id).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!(
                    "Default download client set: {display_name} (id={})",
                    form.id
                ),
                "",
            )
            .await;
            crate::services::download_client::rebuild_clients_cache(
                &state.download_clients,
                &state.db,
            )
            .await;
            let msg =
                urlencoding::encode(&format!("'{display_name}' is now the default")).into_owned();
            Redirect::to(&format!("/settings?tab=integrations&msg={msg}"))
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client set-default failed",
                &e.to_string(),
            )
            .await;
            Redirect::to("/settings?tab=integrations&err=Save+failed")
        }
    }
}

/// JSON form for the inline "Test connection" button on the
/// Connections → Downloads add/edit form. Doesn't touch the DB.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientTestForm {
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub label: String,
}

#[utoipa::path(
    post,
    path = "/api/download-clients/test",
    tag = "System",
    summary = "Test a download client configuration",
    description = "Instantiates the requested client kind with the provided credentials and runs its `test()` method. Doesn't persist anything. The Connections → Downloads add/edit form calls this before saving so the user gets immediate feedback on bad URLs / wrong passwords / missing categories.",
    request_body = DownloadClientTestForm,
    responses(
        (status = 200, description = "Connection successful", body = serde_json::Value),
        (status = 400, description = "Unknown client kind"),
        (status = 502, description = "Connection failed"),
    ),
)]
pub async fn settings_download_clients_test(
    Json(form): Json<DownloadClientTestForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    let url = form.url.trim();
    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "message": "URL required"})),
        ));
    }
    let client: std::sync::Arc<dyn DownloadClient> = match form.kind.as_str() {
        KIND_QBITTORRENT => std::sync::Arc::new(qbittorrent::QbitClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        KIND_DELUGE => std::sync::Arc::new(deluge::DelugeClient::new(
            url,
            &form.password,
            form.label.trim(),
        )),
        KIND_TRANSMISSION => std::sync::Arc::new(transmission::TransmissionClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        KIND_RTORRENT => std::sync::Arc::new(rtorrent::RtorrentClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        KIND_SABNZBD => std::sync::Arc::new(sabnzbd::SabClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        other => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "message": format!("Unknown client kind: {other}"),
                })),
            ));
        }
    };

    match client.test().await {
        Ok(version) => Ok(Json(serde_json::json!({
            "ok": true,
            "message": format!("Connected; {version}"),
        }))),
        Err(err) => Err((
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "message": err})),
        )),
    }
}

/// Form payload for the small "Pin Nyaa to client" selector on
/// the Indexers tab. Empty string = NULL (use default).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct NyaaPinForm {
    pub download_client_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/settings/indexers/nyaa-pin",
    tag = "Settings",
    summary = "Pin or unpin Nyaa to a specific download client",
    description = "Sets `config.nyaa_download_client_id` to the selected client id, or NULL when no client is selected. The Indexers tab shows this as a small dropdown above the indexer list.",
    responses(
        (status = 303, description = "Redirect back to the Indexers tab"),
    ),
)]
pub async fn settings_indexers_nyaa_pin(
    State(state): State<AppState>,
    Form(form): Form<NyaaPinForm>,
) -> Redirect {
    let pin: Option<i64> = form.download_client_id.as_deref().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<i64>().ok()
        }
    });
    // Protocol guard — Nyaa surfaces torrent magnets / .torrent URLs.
    // A SAB pin would resolve at grab time and immediately fail at
    // SAB's `mode=addurl` ("invalid NZB"). Refuse the save with a
    // clear toast instead.
    //
    // PR 112 review #1 (4th pass) — fail closed on transient DB
    // error. The earlier `if let Ok(Some(row))` shape silently
    // skipped the gate when get_by_id returned Err, which would
    // let a Nyaa→SAB pin slip through under a hiccup at save
    // time. Match Err explicitly. Ok(None) still permits (client
    // deleted between page-load and submit is intentional).
    if let Some(client_id) = pin {
        let row = match crate::models::download_clients::get_by_id(&state.db, client_id).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => None, // intentional: client deleted between page-load and submit
            Err(e) => {
                let msg = urlencoding::encode(&format!(
                    "Couldn't verify protocol pin (DB error: {e}); please retry."
                ))
                .into_owned();
                return Redirect::to(&format!("/settings?tab=indexers&err={msg}"));
            }
        };
        if let Some(row) = row
            && let Some(client_proto) =
                crate::services::download_client::protocol_for_client_kind(&row.kind)
            && client_proto != "torrent"
        {
            let msg = urlencoding::encode(&format!(
                "Can't pin Nyaa to a {} client (Nyaa returns torrents; {} accepts {client_proto})",
                row.kind, row.kind
            ))
            .into_owned();
            return Redirect::to(&format!("/settings?tab=indexers&err={msg}"));
        }
    }
    let result = sqlx::query("UPDATE config SET nyaa_download_client_id = ? WHERE id = 1")
        .bind(pin)
        .execute(&state.db)
        .await;
    match result {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Nyaa pin updated",
                &format!("download_client_id={pin:?}"),
            )
            .await;
            Redirect::to("/settings?tab=indexers&msg=Nyaa+pin+updated")
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Nyaa pin update failed",
                &e.to_string(),
            )
            .await;
            Redirect::to("/settings?tab=indexers&err=Save+failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::download_clients::{DownloadClientForm, get_by_id, get_default, list_all};
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use axum::extract::{Form, State};

    fn upsert_form(id: Option<i64>, name: &str) -> DownloadClientUpsertForm {
        DownloadClientUpsertForm {
            id,
            name: name.to_string(),
            kind: "qbittorrent".to_string(),
            url: "http://localhost:8080".to_string(),
            username: "u".to_string(),
            password: "p".to_string(),
            label: "anime".to_string(),
            download_path: "/downloads".to_string(),
            enabled: Some("on".to_string()),
            is_default: None,
        }
    }

    fn extract_location(resp: axum::response::Response) -> String {
        resp.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn upsert_insert_persists_row_and_redirects_back() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            Form(upsert_form(None, "Local qBit")),
        )
        .await;
        let location = extract_location(resp);
        assert!(location.contains("tab=integrations"));
        assert!(location.contains("msg="));

        let rows = list_all(&state.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Local qBit");
        assert_eq!(rows[0].kind, "qbittorrent");
    }

    #[tokio::test]
    async fn upsert_update_renames_existing_row() {
        let db = in_memory_pool().await;
        let id = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "Original",
                kind: "qbittorrent",
                url: "http://localhost:8080",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let _ = settings_download_clients_upsert(
            State(state.clone()),
            Form(upsert_form(Some(id), "Renamed")),
        )
        .await;
        let row = get_by_id(&state.db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Renamed");
    }

    #[tokio::test]
    async fn upsert_rejects_invalid_kind() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let mut form = upsert_form(None, "Bad");
        form.kind = "premiumize".to_string();
        let resp = settings_download_clients_upsert(State(state.clone()), Form(form)).await;
        let location = extract_location(resp);
        assert!(location.contains("err=Invalid+client+kind"));
        assert_eq!(list_all(&state.db).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn upsert_rejects_blank_name() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp =
            settings_download_clients_upsert(State(state.clone()), Form(upsert_form(None, "  ")))
                .await;
        assert!(extract_location(resp).contains("err=Name+required"));
    }

    #[tokio::test]
    async fn upsert_rejects_malformed_url() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let mut form = upsert_form(None, "qbit");
        form.url = "not a url".into();
        let resp = settings_download_clients_upsert(State(state), Form(form)).await;
        assert!(extract_location(resp).contains("err=Invalid+URL+syntax"));
    }

    #[tokio::test]
    async fn delete_removes_row_and_nulls_indexer_pins() {
        let db = in_memory_pool().await;
        let id = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "X",
                kind: "qbittorrent",
                url: "http://x",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        // Pin an indexer to the soon-to-be-deleted client.
        crate::models::indexers::insert(
            &db,
            crate::models::indexers::IndexerForm {
                name: "AB",
                kind: crate::models::indexers::KIND_TORZNAB,
                url: "https://prowlarr.local/1/api",
                api_key: "k",
                priority: 25,
                enabled: true,
                is_private_tracker: true,
                seed_ratio: None,
                seed_time_minutes: None,
                min_seeders: 0,
                request_timeout_secs: None,
                download_client_id: Some(id),
                rss_enabled: false,
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let _ = settings_download_clients_delete(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(DownloadClientIdForm { id }),
        )
        .await;
        assert!(get_by_id(&state.db, id).await.unwrap().is_none());
        let pin: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM indexers WHERE name = 'AB'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(
            pin.is_none(),
            "indexer pin must be NULLed when client deleted"
        );
    }

    #[tokio::test]
    async fn set_default_promotes_one_row_and_demotes_others() {
        let db = in_memory_pool().await;
        let a = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "A",
                kind: "qbittorrent",
                url: "http://a",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        let b = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "B",
                kind: "deluge",
                url: "http://b",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let _ = settings_download_clients_set_default(
            State(state.clone()),
            Form(DownloadClientIdForm { id: b }),
        )
        .await;
        let default_row = get_default(&state.db).await.unwrap().unwrap();
        assert_eq!(default_row.id, b);
        let a_row = get_by_id(&state.db, a).await.unwrap().unwrap();
        assert!(!a_row.is_default);
    }

    #[tokio::test]
    async fn nyaa_pin_persists_id_when_set_and_clears_when_blank() {
        let db = in_memory_pool().await;
        let id = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "qbit",
                kind: "qbittorrent",
                url: "http://qbit",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        // Ensure config row exists (built-test-app-state doesn't seed one).
        let _ = sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
            .execute(&db)
            .await;
        let state = build_test_app_state(db, None);

        let _ = settings_indexers_nyaa_pin(
            State(state.clone()),
            Form(NyaaPinForm {
                download_client_id: Some(id.to_string()),
            }),
        )
        .await;
        let pinned: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(pinned, Some(id));

        let _ = settings_indexers_nyaa_pin(
            State(state.clone()),
            Form(NyaaPinForm {
                download_client_id: Some(String::new()),
            }),
        )
        .await;
        let pinned: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(pinned.is_none());
    }

    /// PR G follow-up — Nyaa surfaces torrent magnets, so pinning to
    /// a SAB client would resolve at grab time and immediately fail
    /// at SAB's `mode=addurl`. Reject the save with a clear toast.
    #[tokio::test]
    async fn nyaa_pin_to_sab_client_is_rejected() {
        let db = in_memory_pool().await;
        let sab = crate::models::download_clients::insert(
            &db,
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
        .unwrap();
        let _ = sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
            .execute(&db)
            .await;
        let state = build_test_app_state(db, None);
        let resp = settings_indexers_nyaa_pin(
            State(state.clone()),
            Form(NyaaPinForm {
                download_client_id: Some(sab.to_string()),
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            location.contains("err=") && location.contains("Nyaa"),
            "expected protocol-mismatch err redirect, got: {location}"
        );
        // The pin must NOT have been persisted.
        let pinned: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(
            pinned.is_none(),
            "Nyaa→SAB save must be rejected, not silently persisted"
        );
    }

    #[tokio::test]
    async fn nyaa_pin_db_error_during_lookup_fails_closed() {
        // PR 112 review #1 (4th pass) — a transient DB error on the
        // pin's protocol lookup must NOT silently skip the gate. The
        // prior `if let Ok(Some(row))` shape would let a Nyaa→SAB
        // pin through under a hiccup at save time. Provoke the
        // error by closing the pool and confirm we redirect to a
        // "DB error" toast instead of persisting the pin.
        let db = in_memory_pool().await;
        let sab = crate::models::download_clients::insert(
            &db,
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
        .unwrap();
        let _ = sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
            .execute(&db)
            .await;
        let state = build_test_app_state(db.clone(), None);
        db.close().await;
        let resp = settings_indexers_nyaa_pin(
            State(state.clone()),
            Form(NyaaPinForm {
                download_client_id: Some(sab.to_string()),
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            location.contains("err=")
                && (location.contains("DB%20error") || location.contains("DB+error")),
            "expected fail-closed err redirect mentioning DB error, got: {location}"
        );
    }
}
