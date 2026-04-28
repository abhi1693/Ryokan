//! Settings → Connections → Downloads CRUD handlers (multi-client
//! refactor). Companion module to `models::download_clients`.
//!
//! Surface mirrors the indexers handler shape: form-driven upsert +
//! delete + set-default that redirect back to the Connections tab.
//! A separate JSON test endpoint at `/api/download-clients/test`
//! lets the user verify a configuration before saving without
//! mutating any DB row.

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::handlers::settings::ConnectionTestResultPartial;
use crate::models::download_clients::{
    DownloadClientForm, DownloadClientRow, delete, get_by_id, insert, list_all, set_default, update,
};
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

/// Pretty-print the wire `kind` discriminator for the per-card badge
/// in `templates/partials/settings/download_clients/list.html`.
/// Public because Askama calls it via the `crate::handlers::...`
/// path from the template.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        KIND_QBITTORRENT => "qBittorrent",
        KIND_DELUGE => "Deluge",
        KIND_TRANSMISSION => "Transmission",
        KIND_RTORRENT => "rTorrent",
        KIND_SABNZBD => "SABnzbd",
        _ => "Unknown",
    }
}

/// Section partial — the entire card list + add slot wrapped in
/// `#dc-section`. Every successful HTMX action (upsert / delete /
/// set-default) returns this so a single swap re-renders the
/// whole tab body without a page reload.
#[derive(Template)]
#[template(path = "partials/settings/download_clients/list.html")]
struct DownloadClientsListPartial {
    rows: Vec<DownloadClientRow>,
}

impl DownloadClientsListPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Inline edit form for one card. Returned by `GET
/// /settings/download-clients/{id}/edit-form`; replaces the
/// card's `<article>` in place via `hx-target="#dc-card-{id}"
/// hx-swap="outerHTML"`.
#[derive(Template)]
#[template(path = "partials/settings/download_clients/edit_form.html")]
struct DownloadClientEditFormPartial {
    row: DownloadClientRow,
}

impl DownloadClientEditFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Inline add form. Returned by `GET
/// /settings/download-clients/add-form`; replaces `#dc-add-slot`
/// when the user clicks "+ Add download client". `first_client`
/// pre-checks the "default" checkbox so the very first row is
/// guaranteed to land as default (empty default = grabs surface
/// "no download client configured" at routing time).
#[derive(Template)]
#[template(path = "partials/settings/download_clients/add_form.html")]
struct DownloadClientAddFormPartial {
    first_client: bool,
}

impl DownloadClientAddFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Default state of the add slot — just the "+ Add" button.
/// Returned by `GET /settings/download-clients/add-button` when
/// the user clicks Cancel inside the open add form.
#[derive(Template)]
#[template(path = "partials/settings/download_clients/add_button.html")]
struct DownloadClientAddButtonPartial;

impl DownloadClientAddButtonPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Helper — load the current rows and render the section partial.
/// Used by the success path of every state-changing endpoint plus
/// the `/api/download-clients/section` cancel-edit refresh route.
async fn render_section(state: &AppState) -> Response {
    let rows = list_all(&state.db).await.unwrap_or_default();
    DownloadClientsListPartial { rows }.into_html_ok()
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
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DownloadClientUpsertForm>,
) -> Response {
    // Validation errors fall back to the form-POST redirect path even
    // for HTMX callers — htmx 2.x's default error policy is skip-the-
    // swap on 4xx, so returning a redirect from the htmx form actually
    // works (the browser follows the redirect). The new tab path
    // replaces the legacy `?tab=integrations` redirect destination.
    let name = form.name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=downloads&err=Name+required").into_response();
    }
    if !is_known_kind(&form.kind) {
        return Redirect::to("/settings?tab=downloads&err=Invalid+client+kind").into_response();
    }
    let url = form.url.trim();
    if url.is_empty() {
        return Redirect::to("/settings?tab=downloads&err=URL+required").into_response();
    }
    if reqwest::Url::parse(url).is_err() {
        return Redirect::to("/settings?tab=downloads&err=Invalid+URL+syntax").into_response();
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
            if is_htmx {
                // Re-render the whole section in one swap — picks up
                // the new card, the moved "default" badge if the user
                // flipped that flag, and a refreshed "+ Add" button
                // (since the section partial re-emits the slot).
                render_section(&state).await
            } else {
                let msg =
                    urlencoding::encode(&format!("Download client '{name}' {verb}")).into_owned();
                Redirect::to(&format!("/settings?tab=downloads&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client upsert failed",
                &e.to_string(),
            )
            .await;
            Redirect::to("/settings?tab=downloads&err=Save+failed").into_response()
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
            // HTMX redesign (#129 follow-up) — re-render the whole
            // section so the "+ Add" button + empty-state CTA both
            // surface correctly when the table goes from N to 0.
            if is_htmx {
                render_section(&state).await
            } else {
                let msg = urlencoding::encode(&format!("Download client '{display_name}' deleted"))
                    .into_owned();
                Redirect::to(&format!("/settings?tab=downloads&msg={msg}")).into_response()
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
                Redirect::to("/settings?tab=downloads&err=Delete+failed").into_response()
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
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DownloadClientIdForm>,
) -> Response {
    let display_name = get_by_id(&state.db, form.id)
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
            if is_htmx {
                // Section re-render so the "default" badge moves
                // between cards in one swap. Per-card swap would
                // require an OOB pair (clear old, set new) and
                // get fragile fast.
                render_section(&state).await
            } else {
                let msg = urlencoding::encode(&format!("'{display_name}' is now the default"))
                    .into_owned();
                Redirect::to(&format!("/settings?tab=downloads&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client set-default failed",
                &e.to_string(),
            )
            .await;
            Redirect::to("/settings?tab=downloads&err=Save+failed").into_response()
        }
    }
}

/// Form payload for the inline "Test connection" button on the
/// Connections → Downloads add/edit form. Doesn't touch the DB.
/// `#[serde(default)]` on every field — the surrounding upsert form
/// has more inputs than this endpoint cares about (id, name,
/// is_default, enabled, …) and `hx-include="closest form"` will pull
/// all of them. Serde drops unknown fields by default, so the extras
/// are silently ignored.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientTestForm {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
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
    description = "Instantiates the requested client kind with the provided credentials and runs \
                   its `test()` method. Doesn't persist anything. The Connections → Downloads \
                   add/edit form calls this before saving so the user gets immediate feedback on \
                   bad URLs / wrong passwords / missing categories. Phase 1.5 grab-bag (issue \
                   #129) — returns an HTML fragment for HTMX swap into the test-result span; \
                   always 200 so HTMX renders both success and failure (default error policy in \
                   2.x is skip-the-swap on 4xx/5xx).",
    request_body = DownloadClientTestForm,
    responses(
        (status = 200, description = "Result rendered as an HTML fragment (success or failure)"),
    ),
)]
pub async fn settings_download_clients_test(Form(form): Form<DownloadClientTestForm>) -> Response {
    let url = form.url.trim();
    if url.is_empty() {
        return ConnectionTestResultPartial {
            ok: false,
            message: "URL required".to_string(),
        }
        .into_html_ok();
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
            return ConnectionTestResultPartial {
                ok: false,
                message: format!("Unknown client kind: {other}"),
            }
            .into_html_ok();
        }
    };

    let result = match client.test().await {
        Ok(version) => ConnectionTestResultPartial {
            ok: true,
            message: format!("Connected; {version}"),
        },
        Err(err) => ConnectionTestResultPartial {
            ok: false,
            message: err,
        },
    };
    result.into_html_ok()
}

// ── HTMX partial-fragment endpoints (Phase 7 follow-up) ────────────
//
// The Settings → Download Clients tab is rendered through three
// partials: `list.html` (the whole section, swapped on every state-
// changing action), `edit_form.html` (one card swapped in place
// when the user clicks Edit), and `add_form.html` (the slot
// expansion when the user clicks "+ Add"). These read-only
// endpoints surface those partials so HTMX can swap them in without
// a full page reload.

#[utoipa::path(
    get,
    path = "/api/download-clients/section",
    tag = "Settings",
    summary = "Render the Download Clients section partial",
    description = "Returns the cards-list + add-slot fragment that lives at #dc-section on the Download Clients tab. Used by Cancel buttons inside inline edit / add forms to restore the section to its baseline rendering without losing scroll position.",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_download_clients_section(State(state): State<AppState>) -> Response {
    render_section(&state).await
}

#[utoipa::path(
    get,
    path = "/settings/download-clients/{id}/edit-form",
    tag = "Settings",
    summary = "Render the inline edit form for one download client",
    description = "Returns the edit_form.html fragment for the targeted row, prefilled with current values. Replaces the row's `<article>` in place via `hx-target=\"#dc-card-{id}\" hx-swap=\"outerHTML\"`. Returns 404 when the row no longer exists (e.g. concurrent delete from another tab).",
    responses(
        (status = 200, description = "HTML fragment"),
        (status = 404, description = "Row not found"),
    ),
)]
pub async fn settings_download_clients_edit_form(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    match get_by_id(&state.db, id).await {
        Ok(Some(row)) => DownloadClientEditFormPartial { row }.into_html_ok(),
        Ok(None) => (StatusCode::NOT_FOUND, "Download client not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/settings/download-clients/add-form",
    tag = "Settings",
    summary = "Render the inline add form",
    description = "Returns the add_form.html fragment that replaces #dc-add-slot when the user clicks \"+ Add download client\". The form's Cancel button hits `/settings/download-clients/add-button` to restore the slot. The default-checkbox is pre-checked when no clients exist yet (the very first row must be default or grabs surface \"no download client configured\" at routing time).",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_download_clients_add_form(State(state): State<AppState>) -> Response {
    let first_client = list_all(&state.db)
        .await
        .map(|rows| rows.is_empty())
        .unwrap_or(false);
    DownloadClientAddFormPartial { first_client }.into_html_ok()
}

#[utoipa::path(
    get,
    path = "/settings/download-clients/add-button",
    tag = "Settings",
    summary = "Render the collapsed add slot button",
    description = "Returns the default \"+ Add download client\" button that #dc-add-slot collapses back to. Used by the Cancel button inside the open add form.",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_download_clients_add_button() -> Response {
    DownloadClientAddButtonPartial.into_html_ok()
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
            axum_htmx::HxRequest(false),
            Form(upsert_form(None, "Local qBit")),
        )
        .await;
        let location = extract_location(resp);
        assert!(location.contains("tab=downloads"));
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
            axum_htmx::HxRequest(false),
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
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(form),
        )
        .await;
        let location = extract_location(resp);
        assert!(location.contains("err=Invalid+client+kind"));
        assert_eq!(list_all(&state.db).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn upsert_rejects_blank_name() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(upsert_form(None, "  ")),
        )
        .await;
        assert!(extract_location(resp).contains("err=Name+required"));
    }

    #[tokio::test]
    async fn upsert_rejects_malformed_url() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let mut form = upsert_form(None, "qbit");
        form.url = "not a url".into();
        let resp =
            settings_download_clients_upsert(State(state), axum_htmx::HxRequest(false), Form(form))
                .await;
        assert!(extract_location(resp).contains("err=Invalid+URL+syntax"));
    }

    #[tokio::test]
    async fn upsert_returns_section_partial_when_htmx() {
        // HTMX-driven create: response body is the `#dc-section`
        // partial rendered with the new row included, not a 303.
        // This keeps the inline add slot working without a full
        // page reload.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(true),
            Form(upsert_form(None, "Local qBit")),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("id=\"dc-section\""), "section root missing");
        assert!(
            html.contains("Local qBit"),
            "freshly-added row should appear in the response: {html}"
        );
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
            axum_htmx::HxRequest(false),
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
