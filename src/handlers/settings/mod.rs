use askama::Template;
use axum::{
    Form, Json,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;
use std::sync::LazyLock;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, custom_formats as cf_model, group_source_map};
use crate::services::{
    custom_formats as cf_service, jellyfin::JellyfinClient, logger, source::Source,
};

pub mod autobrr_key;
pub mod custom_formats;
pub mod direct_rss_feeds;
pub mod download_clients;
pub mod indexers;
use custom_formats::ImportReviewView;

/// Process-wide serializer for `config` row read-modify-write across
/// every Settings save handler — the per-tab subforms
/// (`settings_general_submit`, `settings_quality_submit`,
/// `settings_integrations_submit`) and the legacy bulk
/// `settings_submit`. Each handler reads `existing_cfg`, builds a new
/// `Config` via struct-update, and writes it back. Without this lock,
/// two concurrent saves (the user has Settings open in two tabs and
/// hits Save in both) can interleave: A reads, B reads, A writes,
/// B writes — B's write is built on A's pre-modification snapshot,
/// silently losing A's changes.
///
/// Mutex (not transaction with `BEGIN IMMEDIATE`) because Ryokan is
/// single-process; a `tokio::sync::Mutex` matches the existing
/// `RSS_SYNC_LOCK` / `EXTERNAL_SYNC_LOCK` / `POST_PROC_LOCK` pattern
/// for serializing handler-level work that read-modify-writes shared
/// state. A multi-process deployment (which Ryokan doesn't support
/// today) would need DB-level locking instead.
static CONFIG_WRITE_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// View-model wrapper rendered on the Custom Formats tab. Surfaces
/// parse errors (so the user can spot broken CFs without tailing logs)
/// and carries the per-spec label list used by the card-grid UI to
/// render condition pills.
pub struct CustomFormatView {
    pub row: cf_model::CustomFormatRow,
    pub parse_error: Option<String>,
    /// Sonarr-style condition pills shown on the CF card. Extracted
    /// directly from the row's JSON `specifications[]` array (the
    /// compiled form drops the per-spec `name` field, which is exactly
    /// what the pill needs to render). Empty for parse-error rows; the
    /// template uses `.len()` for the count display too.
    pub spec_labels: Vec<SpecLabelView>,
}

pub struct SpecLabelView {
    pub name: String,
    pub implementation: String,
    pub negate: bool,
    pub required: bool,
}

/// Extract the per-spec labels used by CF card pills. Pulls
/// `name`/`implementation`/`negate`/`required` straight out of the
/// JSON — the compiled form at this layer already dropped the `name`
/// field, so re-parsing as a loose `Value` is the simplest path.
/// Returns an empty vec on any parse failure; the caller already
/// surfaces the parse error via `parse_error`.
fn extract_spec_labels(json: &str) -> Vec<SpecLabelView> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let specs = match value.get("specifications").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    specs
        .iter()
        .map(|s| {
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let implementation = s
                .get("implementation")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let negate = s.get("negate").and_then(|v| v.as_bool()).unwrap_or(false);
            let required = s.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
            SpecLabelView {
                name,
                implementation,
                negate,
                required,
            }
        })
        .collect()
}

/// View-model wrapper rendered when the Custom Formats tab is in edit
/// mode. Holds the full row plus any `trash_description` extracted
/// from the row's JSON body. Plan §5.7.6 wants descriptions to persist
/// through round-trips and surface in the edit drawer so the user
/// keeps the Trash Guides context that originally shipped the CF.
pub struct CustomFormatEditView {
    pub row: cf_model::CustomFormatRow,
    pub trash_description: Option<String>,
}

/// Parse a stored CF's JSON body and return the `trash_description`
/// string if it's present, non-empty, and a string. Silently returns
/// `None` on parse error — the row itself still renders via the raw
/// `edit.json` textarea, so the description is a nice-to-have, not a
/// blocker.
fn extract_trash_description(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let desc = value.get("trash_description")?.as_str()?.trim();
    if desc.is_empty() {
        None
    } else {
        Some(desc.to_string())
    }
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    page: String,
    tab: String,
    config: config::Config,
    groups: Vec<group_source_map::GroupSourceEntry>,
    suggestions: Vec<group_source_map::GroupSuggestion>,
    custom_formats: Vec<CustomFormatView>,
    custom_format_edit: Option<CustomFormatEditView>,
    /// Pre-rendered string for the minimum-score input. Empty when the
    /// floor is the `i32::MIN` "no floor" sentinel. Computed here so the
    /// Askama template doesn't need to compare against an integer path.
    custom_format_min_score_display: String,
    /// Populated when the import flow hit a name collision. The CF tab
    /// renders a review block with per-collision radio buttons so the
    /// user can pick overwrite/rename/skip for each conflicting CF.
    /// See plan §6.2.
    custom_format_import_review: Option<ImportReviewView>,
    message: Option<String>,
    error: Option<String>,
    version: &'static str,
    /// Issue #62 PR A — currently-linked AL or MAL account, if any.
    /// `None` renders the paired Link buttons; `Some(view)` renders
    /// the linked-state card with username, preferences checkboxes,
    /// and Unlink button. The view deliberately excludes tokens —
    /// plaintext tokens exist only in memory during outbound API
    /// calls, never in a rendered page.
    external_account: Option<ExternalAccountView>,
    /// Mirrors `config.title_language` so `base.html`'s pre-paint FOUC
    /// guard can bake the user's preference into the rendered page.
    /// Without this, opening Settings (or any other page) from a fresh
    /// browser snaps titles back to English even when the saved
    /// preference is Romaji or Native.
    title_language: String,
    /// Issue #28 PR A — torznab/newznab indexer rows for the
    /// Settings → Indexers tab placeholder. PR B replaces the
    /// placeholder with add/edit/delete forms and uses the same
    /// list. Empty on a fresh install since no indexers exist
    /// until the user adds one.
    indexers: Vec<crate::models::indexers::Indexer>,
    /// PR #107 review fix #5: when the indexers tab is active
    /// and `?edit_id=N` is set, populate this with the matching
    /// row so the upsert form prefills from the existing values.
    /// `None` renders the bare "Add Indexer" form; same prefill
    /// pattern as the Custom Formats tab.
    indexer_edit: Option<crate::models::indexers::Indexer>,
    /// Curated picker grid for the Settings → Indexers tab.
    /// Always populated from the static catalog so the grid
    /// renders identically regardless of DB state. The user
    /// can still skip the picker by clicking one of the
    /// `is_generic` cards or scrolling past to the form.
    indexer_catalog: &'static [crate::services::indexer_catalog::SeededIndexer],
    /// When the user clicks a picker card, the link sends
    /// `?tab=indexers&template=<slug>` and this gets
    /// populated with the matched seed so the Add form pre-
    /// fills name / kind / private-flag / priority / min-
    /// seeders / suggested seed-ratio. `None` renders the
    /// generic blank form (which is what the user sees if
    /// they bypass the picker).
    indexer_seed: Option<&'static crate::services::indexer_catalog::SeededIndexer>,
    /// Multi-client refactor — every configured download client
    /// for the Download Clients tab list. Sorted default-first then
    /// case-insensitive by name (see `models::download_clients::list_all`).
    download_clients: Vec<crate::models::download_clients::DownloadClientRow>,
    /// Multi-RSS PR G/H — user-supplied direct RSS feeds (e.g.
    /// SubsPlease per-quality feeds) rendered on the Indexers tab
    /// alongside the torznab/newznab indexer rows. Empty until the
    /// user adds one via the bottom-of-tab form.
    direct_rss_feeds: Vec<crate::models::direct_rss_feeds::DirectRssFeed>,
}

/// Safe-to-render projection of `ExternalAccount`. Holds everything
/// the Settings → External Accounts card needs and drops the
/// plaintext token strings.
pub(crate) struct ExternalAccountView {
    pub provider: String,
    pub provider_label: &'static str,
    pub username: String,
    /// Raw `score_format` enum string (e.g. `POINT_10_DECIMAL`).
    /// Kept distinct from `score_format_label` (the humanized form
    /// the template renders) so a future debug surface can inspect
    /// the canonical AL value without re-parsing the label.
    #[allow(dead_code)]
    pub score_format: String,
    pub import_watching: bool,
    pub import_planning: bool,
    pub import_paused: bool,
    pub import_dropped: bool,
    pub import_completed: bool,
    pub skip_already_watched: bool,
    /// #62 PR E — count of MAL→AL mapping failures from the most
    /// recent sync. Surfaces as a "N couldn't be mapped" info banner
    /// only when > 0 AND the linked provider is MAL (AL never
    /// produces deferred entries; the column always reads 0 there).
    pub last_sync_deferred_count: i64,
    /// #62 PR E — sticky auth-rejection flag. Drives the
    /// "Re-link required" red banner on the External Accounts card.
    /// Cleared by the next successful sync.
    pub last_sync_auth_failed: bool,
    /// #62 PR E (redesign) — relative-time label for the most
    /// recent successful sync. "Never" when `list_last_synced_at`
    /// is NULL; otherwise the largest reasonable unit ("4 minutes
    /// ago", "2 hours ago", "3 days ago"). Computed server-side
    /// once per render so the template doesn't carry the time math.
    pub last_sync_label: String,
    /// Raw unix timestamp the live-updater JS keys off via
    /// `data-relative-time`. `None` when no sync has succeeded yet
    /// (the JS skips the element in that case so "Never" stays
    /// rendered as-is). Splitting this out from the label lets the
    /// initial render stay correct when JS is disabled while the
    /// JS path keeps the label fresh between page loads.
    pub last_sync_unix_ts: Option<i64>,
}

impl ExternalAccountView {
    pub(crate) fn from_model(a: crate::models::external_accounts::ExternalAccount) -> Self {
        let provider_label = match a.provider.as_str() {
            crate::models::external_accounts::PROVIDER_ANILIST => "AniList",
            crate::models::external_accounts::PROVIDER_MAL => "MyAnimeList",
            _ => "External",
        };
        let last_sync_label = humanize_relative_time(a.list_last_synced_at);
        Self {
            provider: a.provider,
            provider_label,
            username: a.username,
            score_format: a.score_format,
            import_watching: a.import_watching,
            import_planning: a.import_planning,
            import_paused: a.import_paused,
            import_dropped: a.import_dropped,
            import_completed: a.import_completed,
            skip_already_watched: a.skip_already_watched,
            last_sync_deferred_count: a.last_sync_deferred_count,
            last_sync_auth_failed: a.last_sync_auth_failed,
            last_sync_label,
            last_sync_unix_ts: a.list_last_synced_at,
        }
    }
}

/// "4 minutes ago" / "2 hours ago" / "3 days ago" / "Never" from a
/// Unix-epoch timestamp. The largest-reasonable-unit policy is the
/// same one Sonarr/*arr use on their dashboards: pick the unit that
/// gives a single-digit-or-low-double-digit number and drop fine
/// granularity (the sync runs every N minutes; second-precision is
/// noise).
fn humanize_relative_time(unix_ts: Option<i64>) -> String {
    let Some(ts) = unix_ts else {
        return "Never".to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = (now - ts).max(0);
    if delta < 60 {
        "Just now".to_string()
    } else if delta < 60 * 60 {
        let m = delta / 60;
        format!("{m} minute{} ago", if m == 1 { "" } else { "s" })
    } else if delta < 60 * 60 * 24 {
        let h = delta / 3600;
        format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
    } else {
        let d = delta / 86400;
        format!("{d} day{} ago", if d == 1 { "" } else { "s" })
    }
}

fn min_score_display(score: i32) -> String {
    if score == i32::MIN {
        String::new()
    } else {
        score.to_string()
    }
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    tab: Option<String>,
    /// When the Custom Formats tab is active and `edit_id` is set, the
    /// upsert form prefills from the existing row so the user can fix
    /// the JSON in place rather than deleting and re-pasting.
    edit_id: Option<i64>,
    /// Optional flash message / error surfaced after a POST-redirect.
    /// Kept minimal — detailed validation errors skip the redirect path
    /// and re-render inline so the form state is preserved.
    msg: Option<String>,
    err: Option<String>,
    /// Indexers tab — slug from the seeded catalog
    /// (`services::indexer_catalog`). When set on the Add path,
    /// the form pre-fills from the matched seed instead of
    /// rendering blank.
    template: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsForm {
    tab: Option<String>,
    /// #63 Phase 2 — which download client is active. Accepted
    /// values: "qbittorrent" | "deluge". Settings save branches on
    /// this to construct the concrete trait impl.
    #[serde(default)]
    active_client: String,
    qbit_url: String,
    qbit_user: String,
    qbit_pass: String,
    qbit_category: String,
    qbit_download_path: String,
    #[serde(default)]
    deluge_url: String,
    #[serde(default)]
    deluge_password: String,
    #[serde(default)]
    deluge_label: String,
    #[serde(default)]
    deluge_download_path: String,
    #[serde(default)]
    transmission_url: String,
    #[serde(default)]
    transmission_user: String,
    #[serde(default)]
    transmission_password: String,
    #[serde(default)]
    transmission_label: String,
    #[serde(default)]
    transmission_download_path: String,
    #[serde(default)]
    rtorrent_url: String,
    #[serde(default)]
    rtorrent_user: String,
    #[serde(default)]
    rtorrent_password: String,
    #[serde(default)]
    rtorrent_label: String,
    #[serde(default)]
    rtorrent_download_path: String,
    jellyfin_url: String,
    jellyfin_api_key: String,
    preferred_groups: String,
    blocked_groups: String,
    preferred_source: String,
    preferred_resolution: String,
    cutoff_source: String,
    cutoff_resolution: String,
    finished_series_quality: String,
    media_root: String,
    title_language: String,
    rss_enabled: Option<String>,
    rss_interval_minutes: i32,
    /// Phase 7 PR E — Nyaa-specific RSS opt-out. Lives in the General
    /// tab next to `rss_enabled` / `rss_interval_minutes`. Checkbox →
    /// `Some(_)` when checked, `None` when not.
    disable_nyaa_rss: Option<String>,
    post_processing_enabled: Option<String>,
    post_processing_mode: String,
    /// #1.3.0 — opt-in: trigger auto-search when a series's
    /// monitoring mode changes. Default off. Settings → General.
    search_on_monitoring_change: Option<String>,
    prefer_subs: String,
    sonarr_enabled: Option<String>,
    sonarr_api_key: Option<String>,
    radarr_enabled: Option<String>,
    radarr_api_key: Option<String>,
    upgrade_search_enabled: Option<String>,
    seadex_enabled: Option<String>,
    default_custom_query_tokens: Option<String>,
    default_restrict_to_uploader: Option<String>,
    /// Issue #83 — interactive file-picker trigger policy. `batches_only`
    /// (default) opens the modal for multi-file torrents; `never`
    /// preserves 1.3.0 one-click behavior. Omitted from forms before
    /// PR C → falls back to the existing config value (or default).
    #[serde(default)]
    grab_preview_mode: Option<String>,
    /// Issue #62 PR B — watch-list sync cadence in minutes. Clamped
    /// to 15..=10080 (15 minutes .. 7 days) on save per decision #5.
    /// `None` means "field absent from this form submission" and
    /// falls through to the existing value, same pattern as
    /// `grab_preview_mode`.
    #[serde(default)]
    external_sync_interval_minutes: Option<i32>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct JellyfinTestForm {
    pub jellyfin_url: String,
    pub jellyfin_api_key: String,
}

/// HTMX swap-target partial for connection-test buttons (Phase 1.5
/// grab-bag, issue #129). Used as the `hx-target` content for
/// `/api/jellyfin/test`, `/api/jellyfin/refresh`, and the per-row
/// `/api/download-clients/test` endpoint. Renders a green message
/// on success and a red one on failure; previously the JS just wrote
/// plain text into the same element, so the only visible change is
/// color.
/// Issue #129 Phase 1 completion — Integrations tab subform. Final
/// piece of the per-tab split. Largest field set of the three:
/// includes the legacy single-slot download-client columns (qbit_*,
/// deluge_*, transmission_*, rtorrent_*) carried through as hidden
/// inputs in `integrations.html` for one-release rollback compat.
/// They're persisted verbatim from form input — the new
/// `download_clients` table is the runtime source of truth, but the
/// columns stay round-trippable through Save so a stale tab doesn't
/// blank them.
#[derive(Deserialize)]
pub struct IntegrationsForm {
    // Every String field carries `#[serde(default)]` so a hand-
    // crafted POST that omits any of them deserializes as empty
    // string rather than 422-ing. The legacy single-slot
    // `qbit_*` columns are populated via the hidden inputs in
    // `integrations.html` from the existing config row, so a
    // browser submit always carries them — but the defaults
    // are belt-and-braces against a `curl` user or any future
    // template that drops a hidden input. Same shape every
    // other Option<String> field already uses.
    #[serde(default)]
    active_client: String,
    #[serde(default)]
    qbit_url: String,
    #[serde(default)]
    qbit_user: String,
    #[serde(default)]
    qbit_pass: String,
    #[serde(default)]
    qbit_category: String,
    #[serde(default)]
    qbit_download_path: String,
    #[serde(default)]
    deluge_url: String,
    #[serde(default)]
    deluge_password: String,
    #[serde(default)]
    deluge_label: String,
    #[serde(default)]
    deluge_download_path: String,
    #[serde(default)]
    transmission_url: String,
    #[serde(default)]
    transmission_user: String,
    #[serde(default)]
    transmission_password: String,
    #[serde(default)]
    transmission_label: String,
    #[serde(default)]
    transmission_download_path: String,
    #[serde(default)]
    rtorrent_url: String,
    #[serde(default)]
    rtorrent_user: String,
    #[serde(default)]
    rtorrent_password: String,
    #[serde(default)]
    rtorrent_label: String,
    #[serde(default)]
    rtorrent_download_path: String,
    #[serde(default)]
    jellyfin_url: String,
    #[serde(default)]
    jellyfin_api_key: String,
    /// Checkboxes + their paired API keys — unchecked / unset
    /// omits the field; `#[serde(default)]` maps the absence to
    /// `None`.
    #[serde(default)]
    sonarr_enabled: Option<String>,
    #[serde(default)]
    sonarr_api_key: Option<String>,
    #[serde(default)]
    radarr_enabled: Option<String>,
    #[serde(default)]
    radarr_api_key: Option<String>,
    /// #83 — Interactive file-picker trigger policy.
    #[serde(default)]
    grab_preview_mode: Option<String>,
    /// #62 PR B — watch-list sync cadence in minutes. `None` means
    /// the field was absent from this submission (e.g. no account
    /// linked, so the input wasn't rendered) and the existing
    /// value is preserved.
    #[serde(default)]
    external_sync_interval_minutes: Option<i32>,
}

#[derive(Template)]
#[template(path = "partials/settings/integrations_form.html")]
pub(crate) struct IntegrationsFormPartial {
    pub config: config::Config,
    pub message: Option<String>,
    pub error: Option<String>,
    pub external_account: Option<ExternalAccountView>,
}

/// Issue #129 Phase 1 completion — Quality tab subform. Companion to
/// `GeneralForm`; same per-tab-isolation rationale.
#[derive(Deserialize)]
pub struct QualityForm {
    preferred_groups: String,
    blocked_groups: String,
    preferred_source: String,
    preferred_resolution: String,
    cutoff_source: String,
    cutoff_resolution: String,
    finished_series_quality: String,
    prefer_subs: String,
    /// Checkboxes — unchecked omits the field; `#[serde(default)]`
    /// makes serde_urlencoded map the absence to `None`.
    #[serde(default)]
    upgrade_search_enabled: Option<String>,
    #[serde(default)]
    seadex_enabled: Option<String>,
    #[serde(default)]
    default_custom_query_tokens: Option<String>,
    #[serde(default)]
    default_restrict_to_uploader: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/settings/quality_form.html")]
pub struct QualityFormPartial {
    pub config: config::Config,
    pub message: Option<String>,
    pub error: Option<String>,
}

/// Issue #129 Phase 1 completion — General tab subform. Replaces the
/// bulk-form path through `settings_submit` for the General tab so a
/// Save click only POSTs General fields (not the previously-bundled
/// integrations + quality fields too). HTMX path swaps the form
/// region in place; non-HTMX path falls back to the full
/// SettingsTemplate render so progressive enhancement holds.
#[derive(Deserialize)]
pub struct GeneralForm {
    media_root: String,
    title_language: String,
    /// Checkbox: unchecked omits the field from the POST entirely;
    /// `#[serde(default)]` makes serde_urlencoded map the absence to
    /// `None` rather than failing deserialization. Same shape every
    /// other Option<String> on the per-tab forms uses.
    #[serde(default)]
    rss_enabled: Option<String>,
    rss_interval_minutes: i32,
    /// Phase 7 PR E — Nyaa-specific RSS opt-out.
    #[serde(default)]
    disable_nyaa_rss: Option<String>,
    #[serde(default)]
    post_processing_enabled: Option<String>,
    post_processing_mode: String,
    /// 1.3.0 — opt-in: trigger auto-search when a series's monitoring
    /// mode changes. Default off.
    #[serde(default)]
    search_on_monitoring_change: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/settings/general_form.html")]
pub struct GeneralFormPartial {
    pub config: config::Config,
    pub message: Option<String>,
    pub error: Option<String>,
    pub version: &'static str,
}

#[derive(Template)]
#[template(path = "partials/settings/connection_test_result.html")]
pub struct ConnectionTestResultPartial {
    pub ok: bool,
    pub message: String,
}

impl ConnectionTestResultPartial {
    /// Render to a 200 OK Html response. Intentionally always 200
    /// (success and failure both render), since HTMX's default error-
    /// response policy in 2.x is "don't swap into the target" — and we
    /// *do* want the failure message to land in the target. Returning
    /// 502 on failure would silently fall through and leave the spinner
    /// visible.
    pub fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Resolve the `grab_preview_mode` value to persist on save.
///
/// The picker dropdown lives on the Integrations tab, so Integrations
/// saves (and the rare no-tab POST) honor the form value, while saves
/// from other tabs (Quality, Library, etc.) pass through the existing
/// config value so they can't accidentally reset the picker. Unknown
/// form values coerce to `batches_only` — the safe default that
/// matches a fresh install.
pub(crate) fn resolve_grab_preview_mode(
    form_value: Option<&str>,
    tab: Option<&str>,
    existing: Option<&str>,
) -> String {
    if tab == Some("integrations") || tab.is_none() {
        match form_value.unwrap_or("") {
            "never" => "never".to_string(),
            _ => "batches_only".to_string(),
        }
    } else {
        existing.unwrap_or("batches_only").to_string()
    }
}

/// Issue #62 PR B — watch-list sync cadence default + bounds.
pub(crate) const EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN: i32 = 30;
pub(crate) const EXTERNAL_SYNC_INTERVAL_FLOOR_MIN: i32 = 15;
pub(crate) const EXTERNAL_SYNC_INTERVAL_CEILING_MIN: i32 = 10080; // 7 days

/// Resolve the watch-list sync interval to persist on save. Same
/// cross-tab preservation pattern as `resolve_grab_preview_mode`:
/// the slider lives on the Integrations tab, so Integrations saves
/// (and no-tab POSTs) honor the form value, while saves from other
/// tabs pass through the existing value. Out-of-range values coerce
/// to the 30-minute default rather than the nearest bound — a value
/// outside `15..=10080` is more likely a hand-crafted POST or a
/// stale form than a deliberate edge.
///
/// Missing-form-value behavior: the integrations template always
/// emits the field, so the missing case shouldn't arise from the UI.
/// When it does (a future bug or a scripted POST that omits it), we
/// preserve the existing persisted value rather than resetting to the
/// default — losing a configured 7-day cadence to a UI bug would be a
/// user-visible regression.
pub(crate) fn resolve_external_sync_interval_minutes(
    form_value: Option<i32>,
    tab: Option<&str>,
    existing: Option<i32>,
) -> i32 {
    if tab == Some("integrations") || tab.is_none() {
        match form_value {
            Some(v)
                if (EXTERNAL_SYNC_INTERVAL_FLOOR_MIN..=EXTERNAL_SYNC_INTERVAL_CEILING_MIN)
                    .contains(&v) =>
            {
                v
            }
            // Out-of-range form value: still resets to default — the
            // out-of-range submission is a hand-crafted/malicious POST
            // signal, not a "user accidentally cleared the field"
            // case. Preserving an out-of-range existing value would
            // also be wrong (it can't be persisted via normal flow).
            Some(_) => EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN,
            // Field absent from the POST: keep the persisted value.
            // First-time save (existing = None) falls back to default.
            None => existing.unwrap_or(EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN),
        }
    } else {
        existing.unwrap_or(EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN)
    }
}

fn normalize_settings_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("quality") => "quality".to_string(),
        Some("custom_formats") => "custom_formats".to_string(),
        Some("groups") => "groups".to_string(),
        Some("general") => "general".to_string(),
        // Issue #28 PR A — torznab/newznab indexer registry. Tab
        // surface scaffolded; CRUD form lands in PR B alongside
        // the TorznabIndexer impl that needs caps probing on save.
        Some("indexers") => "indexers".to_string(),
        // Phase 7 follow-up — the multi-client picker was promoted
        // out of the Connections tab into its own page so the cards
        // grid + add slot has the full width and isn't wedged below
        // the bulk Save Settings button (HTML5 forbids nested forms,
        // so the picker has always lived outside the bulk form).
        Some("downloads") => "downloads".to_string(),
        _ => "integrations".to_string(),
    }
}

/// Load every CF row and annotate each one with its parsed spec count
/// (or the parse error string, if compilation fails). Used by the
/// Custom Formats tab to surface broken rows in the list view so the
/// user can find and fix them without trawling logs.
async fn load_custom_formats_view(db: &sqlx::SqlitePool) -> Vec<CustomFormatView> {
    let rows = cf_model::list_with_scores(db).await.unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            let spec_labels = extract_spec_labels(&row.json);
            match cf_service::compile_from_json(&row.json, row.score as i32, row.id) {
                Ok(_) => CustomFormatView {
                    parse_error: None,
                    spec_labels,
                    row,
                },
                Err(e) => CustomFormatView {
                    parse_error: Some(e),
                    spec_labels,
                    row,
                },
            }
        })
        .collect()
}

async fn load_groups(db: &sqlx::SqlitePool) -> Vec<group_source_map::GroupSourceEntry> {
    group_source_map::list_all(db).await.unwrap_or_default()
}

/// Load group-map suggestions inferred from the user's manual overrides.
/// Threshold of 2 matches `compute_suggestions`' docstring rationale: a
/// single override is noise, two matching overrides is the smallest
/// pattern worth surfacing.
async fn load_suggestions(db: &sqlx::SqlitePool) -> Vec<group_source_map::GroupSuggestion> {
    group_source_map::compute_suggestions(db, 2)
        .await
        .unwrap_or_default()
}

/// Sanitize a user-entered download-client scoping label: trim
/// surrounding whitespace, strip any control characters (newlines,
/// tabs, NUL, etc.) that could otherwise survive through to the
/// client's own command parsers (rtorrent's `d.custom1.set="..."`
/// inline command string is the most vulnerable — a literal newline
/// in the label would terminate the command early and let the rest
/// be re-parsed as a separate command). Falls back to `"ryokan"` if
/// the sanitized value is empty.
fn sanitize_label(raw: &str) -> String {
    let filtered: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        "ryokan".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Validate a form-submitted source string by round-tripping through
/// `Source::from_str`. Returns the canonical lowercase form on success, or
/// the supplied default when the value is unrecognized.
fn validate_source(value: &str, default: &str) -> String {
    use crate::services::source::Source;
    let parsed = Source::from_str(value);
    if parsed == Source::Unknown {
        default.to_string()
    } else {
        // Store the canonical lowercase form (e.g. "bluray", "web") so reads
        // via Source::from_str always succeed.
        parsed.as_str().to_ascii_lowercase()
    }
}

/// Validate a form-submitted cutoff-source string. Like `validate_source`
/// but also passes through the BluRay sub-tier markers "bluray_remux" and
/// "bluray_bdmv" so settings can store BD Remux / BD RAW as distinct
/// cutoffs. Reads go through `source::parse_cutoff_source`.
fn validate_cutoff_source(value: &str, default: &str) -> String {
    if value == "bluray_remux" || value == "bluray_bdmv" {
        return value.to_string();
    }
    validate_source(value, default)
}

/// Validate a form-submitted resolution string by round-tripping through
/// `Resolution::from_str`. Returns the bare numeric form ("1080", "720", …)
/// on success, or the supplied default when unrecognized.
fn validate_resolution(value: &str, default: &str) -> String {
    use crate::services::source::Resolution;
    let parsed = Resolution::from_str(value);
    if parsed == Resolution::Unknown {
        default.to_string()
    } else {
        // Strip the trailing 'p' for DB consistency ("1080" not "1080p").
        parsed.as_str().trim_end_matches('p').to_string()
    }
}

/// Build a fully-populated `SettingsTemplate` with the same loading
/// logic the main settings page uses. Extracted so the CF import
/// handler can re-render the settings page in place on a name
/// collision without duplicating every DB query the normal page
/// renderer runs. Callers override the `tab`, `edit_id`, `msg`, `err`,
/// and optional import-review fields to tailor the resulting page.
#[allow(clippy::too_many_arguments)]
async fn build_settings_template(
    state: &AppState,
    tab: Option<String>,
    edit_id: Option<i64>,
    msg: Option<String>,
    err: Option<String>,
    import_review: Option<ImportReviewView>,
    template_slug: Option<String>,
    cfg_override: Option<config::Config>,
) -> SettingsTemplate {
    // Fan out the five independent lookups — config row, release-group
    // table, suggestion panel, custom-format list, linked external
    // account — in parallel. The old code issued them sequentially so
    // the wall time was the sum of N round trips even though none
    // depends on the others.
    //
    // `cfg_override` skips the config fetch when the caller already
    // has a freshly-mutated `Config` in hand (the per-tab subform
    // handlers pass the just-saved cfg through so the rerendered
    // form reflects the mutation even if the caller got an
    // intervening write — and on the save-error path so the user's
    // unsaved input survives the failure render). The remaining 7
    // lookups still parallelize either way.
    let cfg_load = async {
        if cfg_override.is_some() {
            None
        } else {
            config::get_config(&state.db).await.ok().flatten()
        }
    };
    let (
        cfg_loaded,
        groups,
        suggestions,
        custom_formats,
        external_account_res,
        indexers_res,
        download_clients_res,
        direct_rss_feeds_res,
    ) = tokio::join!(
        cfg_load,
        load_groups(&state.db),
        load_suggestions(&state.db),
        load_custom_formats_view(&state.db),
        crate::models::external_accounts::get_current(&state.db),
        crate::models::indexers::list_all(&state.db),
        crate::models::download_clients::list_all(&state.db),
        crate::models::direct_rss_feeds::list_all(&state.db),
    );
    let cfg = cfg_override.or(cfg_loaded).unwrap_or_default();
    // A decrypt failure (tampered blob, key rotation without migration)
    // surfaces here as `Err` — treat as "nothing linked" for render
    // and rely on System → Logs to show the real error. The UI path
    // should never 500 just because the crypto layer hit a snag.
    let external_account = external_account_res
        .ok()
        .flatten()
        .map(ExternalAccountView::from_model);

    // Prefill the CF edit form only when the query param points at a row
    // that actually exists — stale edit links just fall through to the
    // "Add new" form, which is the safer default.
    let custom_format_edit = match edit_id {
        Some(id) => cf_model::get_by_id(&state.db, id)
            .await
            .ok()
            .flatten()
            .map(|row| {
                let trash_description = extract_trash_description(&row.json);
                CustomFormatEditView {
                    row,
                    trash_description,
                }
            }),
        None => None,
    };

    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    let title_language = cfg.title_language.clone();
    // PR #107 review fix #5: resolve the indexer edit prefill the
    // same way CF does. `edit_id` is shared across tabs — at most
    // one tab consumes it at a time.
    let indexer_edit = match edit_id {
        Some(id) => crate::models::indexers::get_by_id(&state.db, id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    // Resolve the picker template only on the Add path — the
    // Edit form is already row-driven, so the seed would just
    // shadow real values. `template_slug` is also discarded if
    // it doesn't match a known seed (stale or hand-typed
    // links fall through to the blank Add form).
    let indexer_seed = if indexer_edit.is_none() {
        template_slug
            .as_deref()
            .and_then(crate::services::indexer_catalog::find_seed)
    } else {
        None
    };
    let download_clients = download_clients_res.unwrap_or_default();
    let direct_rss_feeds = direct_rss_feeds_res.unwrap_or_default();
    SettingsTemplate {
        page: "settings".to_string(),
        tab: normalize_settings_tab(tab),
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit,
        custom_format_min_score_display,
        custom_format_import_review: import_review,
        message: msg,
        error: err,
        version: env!("CARGO_PKG_VERSION"),
        external_account,
        title_language,
        indexers: indexers_res.unwrap_or_default(),
        indexer_edit,
        indexer_catalog: crate::services::indexer_catalog::SEEDED,
        indexer_seed,
        download_clients,
        direct_rss_feeds,
    }
}

pub async fn settings_page(
    State(state): State<AppState>,
    Query(params): Query<SettingsQuery>,
) -> Html<String> {
    let template = build_settings_template(
        &state,
        params.tab,
        params.edit_id,
        params.msg,
        params.err,
        None,
        params.template,
        None,
    )
    .await;
    Html(template.render().unwrap_or_default())
}

pub async fn settings_submit(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Html<String> {
    // Hold `CONFIG_WRITE_LOCK` — the legacy bulk handler does the
    // same read-modify-write the per-tab subforms do (read existing
    // cfg, build a merged Config via the per-field tab-aware
    // preserve logic, save), so it's exposed to the same race.
    // Even though the UI no longer routes here, an external script
    // posting to `/settings` would race with a concurrent per-tab
    // save without this lock.
    let _guard = CONFIG_WRITE_LOCK.lock().await;
    // Load the existing config row once and derive every non-form
    // field from it. The previous code fetched it twice back-to-back
    // (once for force_mal_fallback, once for the rest), which was
    // harmless functionally but paid an extra SQLite round trip on
    // every settings save. `existing_cfg` feeds `force_mal_fallback`,
    // `force_kitsu_fallback`, the legacy quality tier columns, and
    // `auto_grab_on_add` / `allow_non_english` below.
    let existing_cfg = config::get_config(&state.db).await.ok().flatten();

    let current_force_mal_fallback = existing_cfg
        .as_ref()
        .map(|cfg| cfg.force_mal_fallback)
        .unwrap_or(false);
    let current_force_kitsu_fallback = existing_cfg
        .as_ref()
        .map(|cfg| cfg.force_kitsu_fallback)
        .unwrap_or(false);

    let cfg = config::Config {
        active_client: match form.active_client.trim() {
            "deluge" => "deluge".to_string(),
            "transmission" => "transmission".to_string(),
            "rtorrent" => "rtorrent".to_string(),
            // Any other value (including empty from pre-Phase-2 form
            // submissions) collapses to qbittorrent — preserves the
            // Phase 1 default and avoids accidentally switching users
            // onto a client they haven't configured.
            _ => "qbittorrent".to_string(),
        },
        qbit_url: form.qbit_url.trim().to_string(),
        qbit_user: form.qbit_user.trim().to_string(),
        qbit_pass: form.qbit_pass,
        qbit_category: form.qbit_category.trim().to_string(),
        qbit_download_path: form
            .qbit_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        deluge_url: form.deluge_url.trim().trim_end_matches('/').to_string(),
        deluge_password: form.deluge_password,
        deluge_label: sanitize_label(&form.deluge_label),
        deluge_download_path: form
            .deluge_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        transmission_url: form
            .transmission_url
            .trim()
            .trim_end_matches('/')
            .to_string(),
        transmission_user: form.transmission_user.trim().to_string(),
        transmission_password: form.transmission_password,
        transmission_label: sanitize_label(&form.transmission_label),
        transmission_download_path: form
            .transmission_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        rtorrent_url: form.rtorrent_url.trim().trim_end_matches('/').to_string(),
        rtorrent_user: form.rtorrent_user.trim().to_string(),
        rtorrent_password: form.rtorrent_password,
        rtorrent_label: sanitize_label(&form.rtorrent_label),
        rtorrent_download_path: form
            .rtorrent_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        jellyfin_url: form.jellyfin_url.trim().trim_end_matches('/').to_string(),
        jellyfin_api_key: form.jellyfin_api_key.trim().to_string(),
        // Quality-tab fields (preferred_groups, blocked_groups,
        // preferred_*/cutoff_*, finished_series_quality, prefer_subs)
        // are now owned by the dedicated `/settings/quality` subform
        // handler. Same gating shape as the General fields below:
        // preserve from `existing_cfg` when this isn't a tab=quality
        // POST so an Integrations save through the legacy bulk form
        // doesn't blank out Quality knobs.
        preferred_groups: if form.tab.as_deref() == Some("quality") {
            form.preferred_groups.trim().to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.preferred_groups.clone())
                .unwrap_or_default()
        },
        blocked_groups: if form.tab.as_deref() == Some("quality") {
            form.blocked_groups.trim().to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.blocked_groups.clone())
                .unwrap_or_default()
        },
        preferred_source: if form.tab.as_deref() == Some("quality") {
            validate_source(&form.preferred_source, "web")
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.preferred_source.clone())
                .unwrap_or_else(|| "web".to_string())
        },
        preferred_resolution: if form.tab.as_deref() == Some("quality") {
            validate_resolution(&form.preferred_resolution, "1080")
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.preferred_resolution.clone())
                .unwrap_or_else(|| "1080".to_string())
        },
        cutoff_source: if form.tab.as_deref() == Some("quality") {
            validate_cutoff_source(&form.cutoff_source, "bluray")
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.cutoff_source.clone())
                .unwrap_or_else(|| "bluray".to_string())
        },
        cutoff_resolution: if form.tab.as_deref() == Some("quality") {
            validate_resolution(&form.cutoff_resolution, "1080")
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.cutoff_resolution.clone())
                .unwrap_or_else(|| "1080".to_string())
        },
        // Legacy combined tier columns — kept one release for rollback.
        // No longer user-editable; carried forward from the existing row.
        quality_profile: existing_cfg
            .as_ref()
            .map(|c| c.quality_profile.clone())
            .unwrap_or_else(|| "web_1080".to_string()),
        quality_cutoff: existing_cfg
            .as_ref()
            .map(|c| c.quality_cutoff.clone())
            .unwrap_or_else(|| "bd_1080".to_string()),
        finished_series_quality: if form.tab.as_deref() == Some("quality") {
            match form.finished_series_quality.as_str() {
                "same" | "prefer_bd" | "bd_only" => form.finished_series_quality,
                _ => "prefer_bd".to_string(),
            }
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.finished_series_quality.clone())
                .unwrap_or_else(|| "prefer_bd".to_string())
        },
        // General-tab fields (media_root, title_language, rss_*, post_processing_*,
        // search_on_monitoring_change, disable_nyaa_rss) are now owned by the
        // dedicated `/settings/general` subform handler (issue #129 Phase 1
        // completion). The legacy bulk form covers integrations + quality
        // only, so the General fields aren't even in the POST body when
        // reaching this handler — `Form<SettingsForm>` deserializes them
        // as empty defaults via `#[serde(default)]`. Preserving them
        // from `existing_cfg` here when `tab != "general"` keeps the
        // legacy-bookmark / external-script case working: a POST with
        // tab=general still flows through the old per-field logic, but
        // a POST from the new integrations/quality forms doesn't blank
        // out General-tab values.
        media_root: if form.tab.as_deref() == Some("general") {
            form.media_root.trim().trim_end_matches('/').to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.media_root.clone())
                .unwrap_or_default()
        },
        title_language: if form.tab.as_deref() == Some("general") {
            match form.title_language.as_str() {
                "romaji" | "english" | "native" => form.title_language,
                _ => "english".to_string(),
            }
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.title_language.clone())
                .unwrap_or_else(|| "english".to_string())
        },
        force_mal_fallback: current_force_mal_fallback,
        rss_enabled: if form.tab.as_deref() == Some("general") {
            form.rss_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.rss_enabled)
                .unwrap_or(false)
        },
        rss_interval_minutes: if form.tab.as_deref() == Some("general") {
            form.rss_interval_minutes.clamp(1, 60)
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.rss_interval_minutes)
                .unwrap_or(15)
        },
        // multi-rss commit E — preserve the existing master flag
        // through the Settings save. The toggle UI for this flag
        // is deferred to 1.5.1; until then it can be flipped
        // directly via SQL on the `config.rss_master_enabled`
        // column. The save-on-the-main-form path here just keeps
        // the existing value intact rather than clobbering it.
        rss_master_enabled: existing_cfg
            .as_ref()
            .map(|cfg| cfg.rss_master_enabled)
            .unwrap_or(true),
        disable_nyaa_rss: if form.tab.as_deref() == Some("general") {
            form.disable_nyaa_rss.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.disable_nyaa_rss)
                .unwrap_or(false)
        },
        force_kitsu_fallback: current_force_kitsu_fallback,
        post_processing_enabled: if form.tab.as_deref() == Some("general") {
            form.post_processing_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.post_processing_enabled)
                .unwrap_or(false)
        },
        post_processing_mode: if form.tab.as_deref() == Some("general") {
            match form.post_processing_mode.as_str() {
                "move" | "copy" | "hardlink" => form.post_processing_mode,
                _ => "hardlink".to_string(),
            }
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.post_processing_mode.clone())
                .unwrap_or_else(|| "hardlink".to_string())
        },
        auto_grab_on_add: existing_cfg
            .as_ref()
            .map(|c| c.auto_grab_on_add)
            .unwrap_or(true),
        search_on_monitoring_change: if form.tab.as_deref() == Some("general") {
            form.search_on_monitoring_change.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.search_on_monitoring_change)
                .unwrap_or(false)
        },
        prefer_subs: if form.tab.as_deref() == Some("quality") {
            form.prefer_subs == "1"
        } else {
            existing_cfg.as_ref().map(|c| c.prefer_subs).unwrap_or(true)
        },
        allow_non_english: existing_cfg
            .as_ref()
            .map(|c| c.allow_non_english)
            .unwrap_or(false),
        sonarr_enabled: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.sonarr_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.sonarr_enabled)
                .unwrap_or(false)
        },
        sonarr_api_key: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.sonarr_api_key.unwrap_or_default().trim().to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.sonarr_api_key.clone())
                .unwrap_or_default()
        },
        radarr_enabled: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.radarr_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.radarr_enabled)
                .unwrap_or(false)
        },
        radarr_api_key: if form.tab.as_deref() == Some("integrations") || form.tab.is_none() {
            form.radarr_api_key.unwrap_or_default().trim().to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.radarr_api_key.clone())
                .unwrap_or_default()
        },
        // Issue #28 PR D — autobrr API key. Carried forward from the
        // existing row; rotated only via the dedicated
        // /settings/autobrr/regenerate-key handler so a stray POST to
        // the integrations tab can't silently wipe a working webhook.
        autobrr_api_key: existing_cfg
            .as_ref()
            .map(|c| c.autobrr_api_key.clone())
            .unwrap_or_default(),
        upgrade_search_enabled: if form.tab.as_deref() == Some("quality") || form.tab.is_none() {
            form.upgrade_search_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.upgrade_search_enabled)
                .unwrap_or(false)
        },
        // Carried forward from the existing row — edited via the
        // dedicated Custom Formats tab's minimum-score form, not here.
        custom_format_minimum_score: existing_cfg
            .as_ref()
            .map(|c| c.custom_format_minimum_score)
            .unwrap_or(i32::MIN),
        seadex_enabled: if form.tab.as_deref() == Some("quality") || form.tab.is_none() {
            form.seadex_enabled.is_some()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.seadex_enabled)
                .unwrap_or(false)
        },
        // #23 — Search defaults live on the Quality tab alongside the
        // other search-scoped knobs. Preserve on other-tab saves.
        default_custom_query_tokens: if form.tab.as_deref() == Some("quality") || form.tab.is_none()
        {
            form.default_custom_query_tokens
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.default_custom_query_tokens.clone())
                .unwrap_or_default()
        },
        default_restrict_to_uploader: if form.tab.as_deref() == Some("quality")
            || form.tab.is_none()
        {
            form.default_restrict_to_uploader
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            existing_cfg
                .as_ref()
                .map(|c| c.default_restrict_to_uploader.clone())
                .unwrap_or_default()
        },
        // #83 — Interactive file-picker lives on the Integrations tab
        // alongside the other download-client knobs. Preserve on
        // other-tab saves. Unknown values coerce to `batches_only`.
        grab_preview_mode: resolve_grab_preview_mode(
            form.grab_preview_mode.as_deref(),
            form.tab.as_deref(),
            existing_cfg.as_ref().map(|c| c.grab_preview_mode.as_str()),
        ),
        // #62 PR B — watch-list sync interval. Same Integrations-tab
        // ownership pattern as grab_preview_mode. Range clamped to
        // 15..=10080 (15 min .. 7 days) per decision #5; out-of-range
        // values coerce to the default 30 rather than erroring so a
        // hand-crafted POST can't break the supervised task.
        external_sync_interval_minutes: resolve_external_sync_interval_minutes(
            form.external_sync_interval_minutes,
            form.tab.as_deref(),
            existing_cfg
                .as_ref()
                .map(|c| c.external_sync_interval_minutes),
        ),
        // The Nyaa pin is owned by the dedicated `/settings/indexers/nyaa-pin`
        // endpoint, not the bulk save form. Preserve whatever the existing
        // row holds so a Settings save on any tab doesn't clobber it.
        nyaa_download_client_id: existing_cfg
            .as_ref()
            .and_then(|c| c.nyaa_download_client_id),
    };

    let active_tab = normalize_settings_tab(form.tab.clone());

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(
            &state.db,
            LogCategory::System,
            "Failed to save settings",
            &e.to_string(),
        )
        .await;
        let groups = load_groups(&state.db).await;
        let suggestions = load_suggestions(&state.db).await;
        let custom_formats = load_custom_formats_view(&state.db).await;
        let external_account = crate::models::external_accounts::get_current(&state.db)
            .await
            .ok()
            .flatten()
            .map(ExternalAccountView::from_model);
        let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
        let title_language = cfg.title_language.clone();
        let indexers = crate::models::indexers::list_all(&state.db)
            .await
            .unwrap_or_default();
        let download_clients = crate::models::download_clients::list_all(&state.db)
            .await
            .unwrap_or_default();
        let direct_rss_feeds = crate::models::direct_rss_feeds::list_all(&state.db)
            .await
            .unwrap_or_default();
        let template = SettingsTemplate {
            page: "settings".to_string(),
            tab: active_tab,
            config: cfg,
            groups,
            suggestions,
            custom_formats,
            custom_format_edit: None,
            custom_format_min_score_display,
            custom_format_import_review: None,
            message: None,
            error: Some(format!("Failed to save: {}", e)),
            indexer_edit: None,
            version: env!("CARGO_PKG_VERSION"),
            external_account,
            title_language,
            indexers,
            indexer_catalog: crate::services::indexer_catalog::SEEDED,
            indexer_seed: None,
            download_clients,
            direct_rss_feeds,
        };
        return Html(template.render().unwrap_or_default());
    }
    // Drop the write lock now that the read-modify-write is done —
    // the legacy bulk handler also has a Jellyfin connection-test
    // side effect (Integrations branch below) that's the slow path.
    drop(_guard);

    logger::info(&state.db, LogCategory::System, "Settings saved", "").await;
    let mut notices: Vec<String> = vec!["Settings saved.".to_string()];

    // Multi-client routing — client switching now happens through
    // the dedicated `/settings/download-clients/*` CRUD endpoints,
    // not through the bulk Settings save. Pending-grab cancellation
    // on row delete / disable lives in those handlers instead.
    // The legacy single-slot switch detection that used to live
    // here is gone — `cfg.active_client` is no longer the source
    // of truth, and the bulk POST doesn't change download-client
    // routing as a side effect.
    let _ = &existing_cfg; // legacy compat: still read elsewhere

    if active_tab == "integrations" {
        // Multi-client routing: download-client edits go through
        // dedicated `/settings/download-clients/*` CRUD endpoints
        // (which call `rebuild_clients_cache` themselves). The
        // legacy single-slot test-on-save logic is gone — the
        // bulk settings POST no longer touches client state. We
        // keep the legacy `qbit_url` etc. form fields wired so a
        // user with a stale browser tab doesn't blank them out
        // on save, but they don't drive runtime behavior.

        if !cfg.jellyfin_url.is_empty() && !cfg.jellyfin_api_key.is_empty() {
            let client = JellyfinClient::new(&cfg.jellyfin_url, &cfg.jellyfin_api_key);
            match client.test_connection().await {
                Ok(info) => {
                    let label = if info.server_name.trim().is_empty() {
                        format!("Jellyfin ({})", info.version)
                    } else {
                        format!(
                            "Jellyfin {} ({}) connected.",
                            info.server_name, info.version
                        )
                    };
                    logger::info(
                        &state.db,
                        LogCategory::Jellyfin,
                        &format!("{} connected", label),
                        &cfg.jellyfin_url,
                    )
                    .await;
                    notices.push(label);
                    *state.jellyfin.write().await = Some(client);
                }
                Err(e) => {
                    logger::error(&state.db, LogCategory::Jellyfin, "Connection failed", &e).await;
                    *state.jellyfin.write().await = None;
                    notices.push(format!("Jellyfin connection failed: {}.", e));
                }
            }
        } else {
            *state.jellyfin.write().await = None;
        }

        if !cfg.media_root.is_empty() && !std::path::Path::new(&cfg.media_root).is_dir() {
            notices.push(format!(
                "Warning: media root '{}' is not accessible.",
                cfg.media_root
            ));
        }
    }

    let groups = load_groups(&state.db).await;
    let suggestions = load_suggestions(&state.db).await;
    let custom_formats = load_custom_formats_view(&state.db).await;
    let external_account = crate::models::external_accounts::get_current(&state.db)
        .await
        .ok()
        .flatten()
        .map(ExternalAccountView::from_model);
    let custom_format_min_score_display = min_score_display(cfg.custom_format_minimum_score);
    let title_language = cfg.title_language.clone();
    let indexers = crate::models::indexers::list_all(&state.db)
        .await
        .unwrap_or_default();
    let download_clients = crate::models::download_clients::list_all(&state.db)
        .await
        .unwrap_or_default();
    let direct_rss_feeds = crate::models::direct_rss_feeds::list_all(&state.db)
        .await
        .unwrap_or_default();
    let template = SettingsTemplate {
        page: "settings".to_string(),
        tab: active_tab,
        config: cfg,
        groups,
        suggestions,
        custom_formats,
        custom_format_edit: None,
        custom_format_min_score_display,
        custom_format_import_review: None,
        // Joined with " " — not "<br>" — because the template now
        // auto-escapes `message`. Each notice is a complete sentence
        // ending in ".", so a space-joined run reads acceptably as a
        // single paragraph. Multi-notice POSTs are rare (only when
        // the user changes integration settings).
        message: Some(notices.join(" ")),
        error: None,
        indexer_edit: None,
        version: env!("CARGO_PKG_VERSION"),
        external_account,
        title_language,
        indexers,
        indexer_catalog: crate::services::indexer_catalog::SEEDED,
        indexer_seed: None,
        download_clients,
        direct_rss_feeds,
    };
    Html(template.render().unwrap_or_default())
}

/// Issue #129 Phase 1 completion — General tab dedicated POST handler.
/// Owns only General-tab fields (`media_root`, `title_language`, RSS
/// flags, post-processing flags, monitoring-change auto-search opt-in).
/// Reads existing config to preserve every other tab's fields,
/// validates + sanitizes the General fields, persists, and returns
/// either the General-tab subform partial (HTMX) or the full settings
/// page (non-HTMX). Mirrors the per-tab split pattern that the bulk
/// `settings_submit` handler used to do internally via `form.tab` checks.
pub async fn settings_general_submit(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<GeneralForm>,
) -> Response {
    // Hold `CONFIG_WRITE_LOCK` for the full read-modify-write so a
    // concurrent save through any other Settings handler can't
    // interleave between our `get_config` and `save_config` and lose
    // our struct-update merge. The lock spans get_config → save_config
    // (any post-save side effects can run after we drop it).
    let _guard = CONFIG_WRITE_LOCK.lock().await;
    let existing_cfg = match config::get_config(&state.db).await {
        Ok(Some(cfg)) => cfg,
        // No config row yet (first-run): bail with a friendly error
        // so the operator runs through /setup first instead of getting
        // a save-into-nothing silent no-op.
        _ => {
            let err = "No config row found — run /setup first.".to_string();
            return general_response(&state, None, None, Some(err), is_htmx).await;
        }
    };

    // Build the merged config: General-tab fields from form, every
    // other field copied from existing.
    let cfg = config::Config {
        media_root: form.media_root.trim().trim_end_matches('/').to_string(),
        title_language: match form.title_language.as_str() {
            "romaji" | "english" | "native" => form.title_language,
            _ => "english".to_string(),
        },
        rss_enabled: form.rss_enabled.is_some(),
        rss_interval_minutes: form.rss_interval_minutes.clamp(1, 60),
        disable_nyaa_rss: form.disable_nyaa_rss.is_some(),
        post_processing_enabled: form.post_processing_enabled.is_some(),
        post_processing_mode: match form.post_processing_mode.as_str() {
            "move" | "copy" | "hardlink" => form.post_processing_mode,
            _ => "hardlink".to_string(),
        },
        search_on_monitoring_change: form.search_on_monitoring_change.is_some(),
        // Everything else: preserved verbatim.
        ..existing_cfg
    };

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(
            &state.db,
            LogCategory::System,
            "Failed to save General settings",
            &e.to_string(),
        )
        .await;
        return general_response(
            &state,
            Some(cfg),
            None,
            Some(format!("Failed to save: {}", e)),
            is_htmx,
        )
        .await;
    }
    // Drop the write lock now that the read-modify-write is done.
    // Post-save work (logger, notices, response render) doesn't
    // need it, and a concurrent saver shouldn't be blocked by it.
    drop(_guard);

    logger::info(
        &state.db,
        LogCategory::System,
        "Settings saved (General)",
        "",
    )
    .await;

    let mut notices = vec!["Settings saved.".to_string()];
    if !cfg.media_root.is_empty() && !std::path::Path::new(&cfg.media_root).is_dir() {
        notices.push(format!(
            "Warning: media root '{}' is not accessible.",
            cfg.media_root
        ));
    }

    general_response(&state, Some(cfg), Some(notices.join(" ")), None, is_htmx).await
}

/// Render the General response in either HTMX (subform partial) or
/// non-HTMX (full SettingsTemplate) shape. Factored out so the
/// success + DB-error paths in `settings_general_submit` share the
/// same render logic without duplicating the field-load +
/// template-build code.
async fn general_response(
    state: &AppState,
    cfg: Option<config::Config>,
    message: Option<String>,
    error: Option<String>,
    is_htmx: bool,
) -> Response {
    let cfg = match cfg {
        Some(c) => c,
        None => match config::get_config(&state.db).await {
            Ok(Some(c)) => c,
            _ => config::Config::default(),
        },
    };

    if is_htmx {
        return Html(
            GeneralFormPartial {
                config: cfg,
                message,
                error,
                version: env!("CARGO_PKG_VERSION"),
            }
            .render()
            .unwrap_or_default(),
        )
        .into_response();
    }

    // Non-HTMX: render the full settings page through the shared
    // `build_settings_template` helper. Passes the post-save cfg
    // through `cfg_override` so the form rerenders with the user's
    // mutations rather than re-fetching what's in the DB (which on
    // a save-error path would lose their unsaved input). The other
    // 7 fan-out queries parallelize via `tokio::join!`.
    let template = build_settings_template(
        state,
        Some("general".to_string()),
        None,
        message,
        error,
        None,
        None,
        Some(cfg),
    )
    .await;
    Html(template.render().unwrap_or_default()).into_response()
}

/// Issue #129 Phase 1 completion — Quality tab dedicated POST handler.
/// Mirrors `settings_general_submit`. Owns only Quality-tab fields;
/// preserves every other tab's fields via struct-update on existing
/// config.
pub async fn settings_quality_submit(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<QualityForm>,
) -> Response {
    // Hold `CONFIG_WRITE_LOCK` — see `settings_general_submit` for the
    // read-modify-write race rationale.
    let _guard = CONFIG_WRITE_LOCK.lock().await;
    let existing_cfg = match config::get_config(&state.db).await {
        Ok(Some(cfg)) => cfg,
        _ => {
            let err = "No config row found — run /setup first.".to_string();
            return quality_response(&state, None, None, Some(err), is_htmx).await;
        }
    };

    let cfg = config::Config {
        preferred_groups: form.preferred_groups.trim().to_string(),
        blocked_groups: form.blocked_groups.trim().to_string(),
        preferred_source: validate_source(&form.preferred_source, "web"),
        preferred_resolution: validate_resolution(&form.preferred_resolution, "1080"),
        cutoff_source: validate_cutoff_source(&form.cutoff_source, "bluray"),
        cutoff_resolution: validate_resolution(&form.cutoff_resolution, "1080"),
        finished_series_quality: match form.finished_series_quality.as_str() {
            "same" | "prefer_bd" | "bd_only" => form.finished_series_quality,
            _ => "prefer_bd".to_string(),
        },
        prefer_subs: form.prefer_subs == "1",
        upgrade_search_enabled: form.upgrade_search_enabled.is_some(),
        seadex_enabled: form.seadex_enabled.is_some(),
        default_custom_query_tokens: form
            .default_custom_query_tokens
            .unwrap_or_default()
            .trim()
            .to_string(),
        default_restrict_to_uploader: form
            .default_restrict_to_uploader
            .unwrap_or_default()
            .trim()
            .to_string(),
        ..existing_cfg
    };

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(
            &state.db,
            LogCategory::System,
            "Failed to save Quality settings",
            &e.to_string(),
        )
        .await;
        return quality_response(
            &state,
            Some(cfg),
            None,
            Some(format!("Failed to save: {}", e)),
            is_htmx,
        )
        .await;
    }
    // See `settings_general_submit` for the lock-drop rationale.
    drop(_guard);

    logger::info(
        &state.db,
        LogCategory::System,
        "Settings saved (Quality)",
        "",
    )
    .await;

    quality_response(
        &state,
        Some(cfg),
        Some("Settings saved.".to_string()),
        None,
        is_htmx,
    )
    .await
}

/// Render the Quality response in either HTMX (subform partial) or
/// non-HTMX (full SettingsTemplate) shape. Mirrors `general_response`.
async fn quality_response(
    state: &AppState,
    cfg: Option<config::Config>,
    message: Option<String>,
    error: Option<String>,
    is_htmx: bool,
) -> Response {
    let cfg = match cfg {
        Some(c) => c,
        None => match config::get_config(&state.db).await {
            Ok(Some(c)) => c,
            _ => config::Config::default(),
        },
    };

    if is_htmx {
        return Html(
            QualityFormPartial {
                config: cfg,
                message,
                error,
            }
            .render()
            .unwrap_or_default(),
        )
        .into_response();
    }

    // Non-HTMX: shared template path. See `general_response` for the
    // `cfg_override` rationale.
    let template = build_settings_template(
        state,
        Some("quality".to_string()),
        None,
        message,
        error,
        None,
        None,
        Some(cfg),
    )
    .await;
    Html(template.render().unwrap_or_default()).into_response()
}

/// Issue #129 Phase 1 completion — Integrations tab dedicated POST
/// handler. Owns Jellyfin URL+key, Sonarr/Radarr API enable+key,
/// grab_preview_mode, external_sync_interval_minutes, and the legacy
/// single-slot download-client columns (preserved verbatim through
/// the hidden inputs in `integrations.html` so a stale tab can't
/// blank them).
///
/// Side effect: when both jellyfin_url and jellyfin_api_key are set,
/// runs a connection test and surfaces the result in the save toast +
/// updates `state.jellyfin`. Matches the legacy bulk handler's
/// `if active_tab == "integrations"` block.
pub async fn settings_integrations_submit(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<IntegrationsForm>,
) -> Response {
    // Hold `CONFIG_WRITE_LOCK` — see `settings_general_submit` for the
    // read-modify-write race rationale. We drop the lock explicitly
    // after `save_config` completes (below) so the Jellyfin
    // connection-test side effect — which can hang for the full
    // connect-timeout when the URL points at an unreachable host —
    // doesn't block other Settings saves on its network round trip.
    let _guard = CONFIG_WRITE_LOCK.lock().await;
    let existing_cfg = match config::get_config(&state.db).await {
        Ok(Some(cfg)) => cfg,
        _ => {
            let err = "No config row found — run /setup first.".to_string();
            return integrations_response(&state, None, None, Some(err), is_htmx).await;
        }
    };

    let cfg = config::Config {
        active_client: match form.active_client.trim() {
            "deluge" => "deluge".to_string(),
            "transmission" => "transmission".to_string(),
            "rtorrent" => "rtorrent".to_string(),
            _ => "qbittorrent".to_string(),
        },
        qbit_url: form.qbit_url.trim().to_string(),
        qbit_user: form.qbit_user.trim().to_string(),
        qbit_pass: form.qbit_pass,
        qbit_category: form.qbit_category.trim().to_string(),
        qbit_download_path: form
            .qbit_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        deluge_url: form.deluge_url.trim().trim_end_matches('/').to_string(),
        deluge_password: form.deluge_password,
        deluge_label: sanitize_label(&form.deluge_label),
        deluge_download_path: form
            .deluge_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        transmission_url: form
            .transmission_url
            .trim()
            .trim_end_matches('/')
            .to_string(),
        transmission_user: form.transmission_user.trim().to_string(),
        transmission_password: form.transmission_password,
        transmission_label: sanitize_label(&form.transmission_label),
        transmission_download_path: form
            .transmission_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        rtorrent_url: form.rtorrent_url.trim().trim_end_matches('/').to_string(),
        rtorrent_user: form.rtorrent_user.trim().to_string(),
        rtorrent_password: form.rtorrent_password,
        rtorrent_label: sanitize_label(&form.rtorrent_label),
        rtorrent_download_path: form
            .rtorrent_download_path
            .trim()
            .trim_end_matches('/')
            .to_string(),
        jellyfin_url: form.jellyfin_url.trim().trim_end_matches('/').to_string(),
        jellyfin_api_key: form.jellyfin_api_key.trim().to_string(),
        sonarr_enabled: form.sonarr_enabled.is_some(),
        sonarr_api_key: form.sonarr_api_key.unwrap_or_default().trim().to_string(),
        radarr_enabled: form.radarr_enabled.is_some(),
        radarr_api_key: form.radarr_api_key.unwrap_or_default().trim().to_string(),
        grab_preview_mode: resolve_grab_preview_mode(
            form.grab_preview_mode.as_deref(),
            Some("integrations"),
            Some(existing_cfg.grab_preview_mode.as_str()),
        ),
        external_sync_interval_minutes: resolve_external_sync_interval_minutes(
            form.external_sync_interval_minutes,
            Some("integrations"),
            Some(existing_cfg.external_sync_interval_minutes),
        ),
        ..existing_cfg
    };

    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(
            &state.db,
            LogCategory::System,
            "Failed to save Integrations settings",
            &e.to_string(),
        )
        .await;
        return integrations_response(
            &state,
            Some(cfg),
            None,
            Some(format!("Failed to save: {}", e)),
            is_htmx,
        )
        .await;
    }
    // Drop before the Jellyfin connection-test side effect: that
    // network probe is the slow path of this handler and there's no
    // reason to make a concurrent saver on a different tab wait
    // through it.
    drop(_guard);

    logger::info(
        &state.db,
        LogCategory::System,
        "Settings saved (Integrations)",
        "",
    )
    .await;

    let mut notices = vec!["Settings saved.".to_string()];

    // Side effect: Jellyfin connection test on save. Mirrors the
    // legacy bulk handler so the user gets immediate feedback in the
    // save toast about whether the credentials they just entered
    // actually reach a Jellyfin server. Updates `state.jellyfin` so
    // every other request that reads it sees the live (or cleared)
    // client.
    if !cfg.jellyfin_url.is_empty() && !cfg.jellyfin_api_key.is_empty() {
        let client = JellyfinClient::new(&cfg.jellyfin_url, &cfg.jellyfin_api_key);
        match client.test_connection().await {
            Ok(info) => {
                let label = if info.server_name.trim().is_empty() {
                    format!("Jellyfin ({})", info.version)
                } else {
                    format!(
                        "Jellyfin {} ({}) connected.",
                        info.server_name, info.version
                    )
                };
                logger::info(
                    &state.db,
                    LogCategory::Jellyfin,
                    &format!("{} connected", label),
                    &cfg.jellyfin_url,
                )
                .await;
                notices.push(label);
                *state.jellyfin.write().await = Some(client);
            }
            Err(e) => {
                logger::error(&state.db, LogCategory::Jellyfin, "Connection failed", &e).await;
                *state.jellyfin.write().await = None;
                notices.push(format!("Jellyfin connection failed: {}.", e));
            }
        }
    } else {
        *state.jellyfin.write().await = None;
    }

    integrations_response(&state, Some(cfg), Some(notices.join(" ")), None, is_htmx).await
}

/// Render the Integrations response in either HTMX (subform partial)
/// or non-HTMX (full SettingsTemplate) shape. Mirrors
/// `general_response` / `quality_response` but also threads the
/// external_account through (the partial uses it for the linked-
/// account legend badges + the prefs section).
async fn integrations_response(
    state: &AppState,
    cfg: Option<config::Config>,
    message: Option<String>,
    error: Option<String>,
    is_htmx: bool,
) -> Response {
    let cfg = match cfg {
        Some(c) => c,
        None => match config::get_config(&state.db).await {
            Ok(Some(c)) => c,
            _ => config::Config::default(),
        },
    };

    if is_htmx {
        // The partial needs the linked-account view for its legend
        // badge + prefs section. Fetch on the HTMX path only — the
        // non-HTMX path goes through `build_settings_template` which
        // does this in parallel with the rest of the fan-out.
        let external_account = crate::models::external_accounts::get_current(&state.db)
            .await
            .ok()
            .flatten()
            .map(ExternalAccountView::from_model);
        return Html(
            IntegrationsFormPartial {
                config: cfg,
                message,
                error,
                external_account,
            }
            .render()
            .unwrap_or_default(),
        )
        .into_response();
    }

    // Non-HTMX: shared template path. See `general_response` for the
    // `cfg_override` rationale.
    let template = build_settings_template(
        state,
        Some("integrations".to_string()),
        None,
        message,
        error,
        None,
        None,
        Some(cfg),
    )
    .await;
    Html(template.render().unwrap_or_default()).into_response()
}

// ─────────────────────────────────────────────────────────────────────────
// Release group source map CRUD
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GroupUpsertForm {
    group_name: String,
    source: String,
    confidence: Option<f32>,
    notes: Option<String>,
}

#[derive(Deserialize)]
pub struct GroupDeleteForm {
    pub group_name: String,
}

/// Upsert a user-edited row in `group_source_map`. Silently no-ops on an
/// empty group name or unknown source. Redirects back to the groups tab
/// regardless so the user sees the updated list.
pub async fn settings_groups_upsert(
    State(state): State<AppState>,
    Form(form): Form<GroupUpsertForm>,
) -> Redirect {
    let name = form.group_name.trim();
    if name.is_empty() {
        return Redirect::to("/settings?tab=groups");
    }
    let source = Source::from_str(&form.source);
    if source == Source::Unknown {
        return Redirect::to("/settings?tab=groups");
    }
    let confidence = form.confidence.unwrap_or(0.95).clamp(0.0, 1.0);
    let notes = form.notes.unwrap_or_default();
    let notes = notes.trim();

    match group_source_map::upsert_user_edit(&state.db, name, source, confidence, notes).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Group source updated: {}", name),
                source.as_str(),
            )
            .await;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Group source upsert failed",
                &e.to_string(),
            )
            .await;
        }
    }
    Redirect::to("/settings?tab=groups")
}

/// Delete a row from `group_source_map` by group name. Works on both seeded
/// and user-edited rows — seeded rows will be re-inserted on the next
/// startup via `seed_defaults`, so deletes of seeds are effectively a
/// one-session reset.
pub async fn settings_groups_delete(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<GroupDeleteForm>,
) -> Response {
    let name = form.group_name.trim();
    if name.is_empty() {
        return if is_htmx {
            // Empty name from HTMX is a programmer error (the form
            // should have a hidden input); 400 makes it visible in
            // devtools rather than silently no-op'ing the row removal.
            StatusCode::BAD_REQUEST.into_response()
        } else {
            Redirect::to("/settings?tab=groups").into_response()
        };
    }
    match group_source_map::delete(&state.db, name).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Group source deleted: {}", name),
                "",
            )
            .await;
            // HTMX migration (issue #129) — empty 200 lets the row form's
            // `hx-target="closest tr" hx-swap="outerHTML"` remove the row.
            if is_htmx {
                StatusCode::OK.into_response()
            } else {
                Redirect::to("/settings?tab=groups").into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Group source delete failed",
                &e.to_string(),
            )
            .await;
            if is_htmx {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            } else {
                Redirect::to("/settings?tab=groups").into_response()
            }
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/jellyfin/test",
    tag = "System",
    summary = "Test Jellyfin connection",
    description = "Test connectivity to a Jellyfin instance with the provided URL and API key. \
                   Returns an HTML fragment (Phase 1.5 grab-bag, issue #129) — `hx-swap=innerHTML` \
                   on the result span renders the message inline. Always 200 so HTMX swaps in both \
                   success and failure cases (htmx 2.x default error policy skips the swap on 4xx/5xx).",
    request_body = JellyfinTestForm,
    responses(
        (status = 200, description = "Result rendered as an HTML fragment (success or failure)"),
    ),
)]
pub async fn jellyfin_test(Form(form): Form<JellyfinTestForm>) -> Response {
    let client = JellyfinClient::new(form.jellyfin_url.trim(), &form.jellyfin_api_key);

    let result = match client.test_connection().await {
        Ok(info) => ConnectionTestResultPartial {
            ok: true,
            message: if info.server_name.trim().is_empty() {
                format!("Connected to Jellyfin {}", info.version)
            } else {
                format!(
                    "Connected to Jellyfin {} ({})",
                    info.server_name, info.version
                )
            },
        },
        Err(err) => ConnectionTestResultPartial {
            ok: false,
            message: err,
        },
    };
    result.into_html_ok()
}

#[utoipa::path(
    get,
    path = "/api/health",
    tag = "System",
    summary = "Health check",
    description = "Returns connection status of the active download client and Jellyfin.",
    responses(
        (status = 200, description = "Health status", body = serde_json::Value),
    ),
)]
pub async fn api_health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let download_client_status = {
        let client = state.default_download_client().await;
        match client {
            Some(c) => {
                // Emit `type` on both Ok and Err so the template JS
                // can route the Disconnected badge to the right
                // fieldset when test() fails (daemon down, wrong
                // creds). Without this, a configured-but-failing
                // client renders no badge at all.
                let impl_name = c.sonarr_impl_name();
                match c.test().await {
                    Ok(version) => serde_json::json!({
                        "ok": true,
                        "message": format!("{} {}", impl_name, version),
                        "type": impl_name,
                    }),
                    Err(e) => serde_json::json!({
                        "ok": false,
                        "message": e,
                        "type": impl_name,
                    }),
                }
            }
            None => serde_json::json!({"ok": false, "message": "Not configured"}),
        }
    };

    let jellyfin_status = {
        let client = state.jellyfin.read().await.clone();
        match client {
            Some(c) => match c.test_connection().await {
                Ok(info) => {
                    let label = if info.server_name.trim().is_empty() {
                        format!("Jellyfin {}", info.version)
                    } else {
                        format!("{} ({})", info.server_name, info.version)
                    };
                    serde_json::json!({"ok": true, "message": label})
                }
                Err(e) => serde_json::json!({"ok": false, "message": e}),
            },
            None => serde_json::json!({"ok": false, "message": "Not configured"}),
        }
    };

    Json(serde_json::json!({
        "download_client": download_client_status,
        "jellyfin": jellyfin_status,
    }))
}

#[utoipa::path(
    post,
    path = "/api/jellyfin/refresh",
    tag = "System",
    summary = "Refresh Jellyfin library",
    description = "Trigger a library scan in Jellyfin to pick up newly added media. \
                   Returns an HTML fragment for HTMX swap into the test-result span; always 200 \
                   (see /api/jellyfin/test for the swap-on-error rationale).",
    responses(
        (status = 200, description = "Result rendered as an HTML fragment (success or failure)"),
    ),
)]
pub async fn jellyfin_refresh(State(state): State<AppState>) -> Response {
    let client = {
        let jellyfin = state.jellyfin.read().await;
        jellyfin.as_ref().cloned()
    };
    let result = match client {
        None => ConnectionTestResultPartial {
            ok: false,
            message: "Jellyfin not configured".to_string(),
        },
        Some(c) => match c.refresh_library().await {
            Ok(()) => ConnectionTestResultPartial {
                ok: true,
                message: "Jellyfin library refresh queued".to_string(),
            },
            Err(err) => ConnectionTestResultPartial {
                ok: false,
                message: err,
            },
        },
    };
    result.into_html_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_label_strips_control_chars() {
        // A label with an embedded newline would survive through to
        // rtorrent's `d.custom1.set="..."` inline command and could
        // terminate the command early. sanitize_label strips any
        // control character before it can reach the wire.
        assert_eq!(sanitize_label("ryokan\nmalicious"), "ryokanmalicious");
        assert_eq!(sanitize_label("ry\tokan"), "ryokan");
        assert_eq!(sanitize_label("ryokan\0"), "ryokan");
        assert_eq!(sanitize_label("  ryokan  "), "ryokan");
    }

    #[test]
    fn sanitize_label_defaults_to_ryokan_when_empty_or_only_control() {
        assert_eq!(sanitize_label(""), "ryokan");
        assert_eq!(sanitize_label("   "), "ryokan");
        assert_eq!(sanitize_label("\n\t\r"), "ryokan");
    }

    #[test]
    fn sanitize_label_preserves_unicode_and_spaces() {
        // Only control characters are stripped — internal spaces and
        // non-ASCII characters (users' native-script labels) survive.
        assert_eq!(sanitize_label("anime batch"), "anime batch");
        assert_eq!(sanitize_label("アニメ"), "アニメ");
    }

    /// A Sonarr/Trash Guides CF that carries a `trash_description`
    /// should be surfaced verbatim so the edit drawer can render it.
    #[test]
    fn extract_trash_description_returns_string_when_present() {
        let json = serde_json::json!({
            "name": "Example",
            "trash_description": "This CF matches high-quality BluRay releases.",
            "specifications": []
        })
        .to_string();
        assert_eq!(
            extract_trash_description(&json),
            Some("This CF matches high-quality BluRay releases.".to_string())
        );
    }

    /// Absent, empty, whitespace-only, wrong-typed, or unparseable
    /// payloads should all return `None` so the template simply
    /// doesn't render the description block.
    #[test]
    fn extract_trash_description_returns_none_for_missing_or_invalid() {
        let no_field = serde_json::json!({"name": "X", "specifications": []}).to_string();
        assert_eq!(extract_trash_description(&no_field), None);

        let empty = serde_json::json!({"trash_description": ""}).to_string();
        assert_eq!(extract_trash_description(&empty), None);

        let whitespace = serde_json::json!({"trash_description": "   "}).to_string();
        assert_eq!(extract_trash_description(&whitespace), None);

        let wrong_type = serde_json::json!({"trash_description": 42}).to_string();
        assert_eq!(extract_trash_description(&wrong_type), None);

        assert_eq!(extract_trash_description("not json at all"), None);
    }

    #[test]
    fn resolve_grab_preview_mode_integrations_tab_accepts_form_value() {
        // On the Integrations tab, the form value is the source of
        // truth. "never" and "batches_only" both persist.
        assert_eq!(
            resolve_grab_preview_mode(Some("never"), Some("integrations"), Some("batches_only")),
            "never"
        );
        assert_eq!(
            resolve_grab_preview_mode(Some("batches_only"), Some("integrations"), Some("never")),
            "batches_only"
        );
    }

    #[test]
    fn resolve_grab_preview_mode_unknown_form_value_coerces_to_default() {
        // A garbage form value (hand-crafted POST, dropped `always`
        // option from the plan doc, etc.) coerces to "batches_only"
        // so the config can't end up in an unenumerated state.
        assert_eq!(
            resolve_grab_preview_mode(Some(""), Some("integrations"), Some("never")),
            "batches_only"
        );
        assert_eq!(
            resolve_grab_preview_mode(Some("always"), Some("integrations"), Some("never")),
            "batches_only"
        );
        assert_eq!(
            resolve_grab_preview_mode(None, Some("integrations"), Some("never")),
            "batches_only"
        );
    }

    #[test]
    fn resolve_grab_preview_mode_other_tabs_preserve_existing() {
        // A save from the Quality tab (or anywhere else) must not
        // reset the picker. Critical — the same form shape is
        // submitted from every tab and the picker field is simply
        // omitted outside Integrations.
        assert_eq!(
            resolve_grab_preview_mode(None, Some("quality"), Some("never")),
            "never"
        );
        assert_eq!(
            resolve_grab_preview_mode(None, Some("groups"), Some("batches_only")),
            "batches_only"
        );
        // A stray form value from a non-Integrations tab is ignored —
        // only the existing value matters there.
        assert_eq!(
            resolve_grab_preview_mode(Some("never"), Some("library"), Some("batches_only")),
            "batches_only"
        );
    }

    #[test]
    fn resolve_grab_preview_mode_missing_tab_uses_form_value() {
        // No tab on the form = the no-tab POST shape; treat like
        // Integrations so the field round-trips on a "save all" flow.
        assert_eq!(
            resolve_grab_preview_mode(Some("never"), None, Some("batches_only")),
            "never"
        );
    }

    #[test]
    fn resolve_grab_preview_mode_missing_existing_defaults_to_batches_only() {
        // Pre-PR-C DB rows never wrote the column; reads default to
        // "batches_only" in the model layer, and the settings
        // save path must do the same if the read path ever produces
        // a missing value.
        assert_eq!(
            resolve_grab_preview_mode(None, Some("quality"), None),
            "batches_only"
        );
    }

    // ── #62 PR B watch-list sync interval resolver tests ───────────

    #[test]
    fn resolve_external_sync_interval_integrations_tab_accepts_in_range() {
        // Bounds match the plan-doc-decided range (15 min .. 7 days).
        // 15 and 10080 are inclusive endpoints.
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(15), Some("integrations"), Some(30)),
            15
        );
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(10080), Some("integrations"), Some(30)),
            10080
        );
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(60), Some("integrations"), Some(30)),
            60
        );
    }

    #[test]
    fn resolve_external_sync_interval_out_of_range_coerces_to_default() {
        // 14 and 10081 are just outside the allowed range; both coerce
        // to the 30-minute default so a hand-crafted POST or stale
        // form can't end up with a too-aggressive (rate-limit-
        // pressuring) or effectively-disabled cadence persisted.
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(14), Some("integrations"), Some(30)),
            EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
        );
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(10081), Some("integrations"), Some(30)),
            EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
        );
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(0), Some("integrations"), Some(30)),
            EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
        );
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(-1), Some("integrations"), Some(30)),
            EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
        );
    }

    #[test]
    fn resolve_external_sync_interval_other_tabs_preserve_existing() {
        // Same cross-tab guarantee as grab_preview_mode: a Quality-
        // tab save shouldn't reset the picker, so a Quality save also
        // shouldn't reset the sync interval.
        assert_eq!(
            resolve_external_sync_interval_minutes(None, Some("quality"), Some(60)),
            60
        );
        // Stray form value from a non-Integrations tab is ignored —
        // only the existing value matters there.
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(120), Some("quality"), Some(60)),
            60
        );
    }

    #[test]
    fn resolve_external_sync_interval_missing_tab_uses_form_value() {
        // No-tab POST shape (full-form save) honors the form value
        // same as Integrations does.
        assert_eq!(
            resolve_external_sync_interval_minutes(Some(120), None, Some(30)),
            120
        );
    }

    #[test]
    fn resolve_external_sync_interval_missing_form_value_preserves_existing() {
        // Field absent from a POST that should have included it
        // (template bug, scripted POST that omits it). Preserve the
        // user's persisted value rather than resetting to default —
        // losing a configured 7-day cadence to a UI bug would be a
        // user-visible regression. Out-of-range form values still
        // reset (separate test) since those signal hand-crafted
        // POSTs we don't trust.
        assert_eq!(
            resolve_external_sync_interval_minutes(None, Some("integrations"), Some(60)),
            60
        );
    }

    #[test]
    fn resolve_external_sync_interval_missing_existing_uses_default() {
        // Pre-PR-B DB rows never wrote the column; the read path
        // defaults to 30, but if it ever returns None for any other
        // reason the resolver should still produce a valid value.
        assert_eq!(
            resolve_external_sync_interval_minutes(None, Some("quality"), None),
            EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
        );
    }

    #[test]
    fn resolve_external_sync_interval_missing_form_and_existing_uses_default() {
        // First-time save on a fresh install with the field missing.
        // No existing value to preserve, so default is the only sane
        // landing.
        assert_eq!(
            resolve_external_sync_interval_minutes(None, Some("integrations"), None),
            EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
        );
    }

    // ── validate_source ───────────────────────────────────────────────
    //
    // Settings save uses these to coerce form values into
    // canonical-lowercase strings the rest of the codebase reads
    // back via `Source::from_str`. A regression that forgot to
    // canonicalize would persist mixed-case values and break the
    // CF / scoring matchers that case-sensitive-compare the column.

    #[test]
    fn validate_source_canonicalizes_known_values_to_lowercase() {
        // The user-facing dropdown emits canonical strings, but
        // hand-crafted POSTs / older DB rows can carry mixed case.
        // Every recognized variant lands in lowercase.
        assert_eq!(validate_source("BluRay", "web"), "bluray");
        assert_eq!(validate_source("BD", "web"), "bluray");
        assert_eq!(validate_source("BDRIP", "web"), "bluray");
        assert_eq!(validate_source("Web-DL", "bluray"), "web");
        assert_eq!(validate_source("WEBRIP", "bluray"), "web");
        assert_eq!(validate_source("HDTV", "web"), "hdtv");
        assert_eq!(validate_source("DVD", "web"), "dvd");
    }

    #[test]
    fn validate_source_falls_back_to_default_on_unknown() {
        // A garbage form value resolves to the supplied default
        // rather than persisting `Unknown` — every read path
        // assumes a known variant.
        assert_eq!(validate_source("garbage", "web"), "web");
        assert_eq!(validate_source("", "bluray"), "bluray");
        // The default itself isn't canonicalized — it's a static
        // string the caller already chose.
        assert_eq!(validate_source("unknown-source", "WEB"), "WEB");
    }

    #[test]
    fn validate_source_trims_whitespace() {
        // `Source::from_str` trims, so the validator inherits that.
        assert_eq!(validate_source("  bluray  ", "web"), "bluray");
    }

    // ── validate_cutoff_source ────────────────────────────────────────

    #[test]
    fn validate_cutoff_source_passes_through_bluray_subtiers() {
        // The cutoff dropdown surfaces three BluRay tiers: plain
        // bluray, bluray_remux, bluray_bdmv. The latter two are stored
        // as-is so `parse_cutoff_source` can branch on the exact string.
        assert_eq!(
            validate_cutoff_source("bluray_remux", "bluray"),
            "bluray_remux"
        );
        assert_eq!(
            validate_cutoff_source("bluray_bdmv", "bluray"),
            "bluray_bdmv"
        );
    }

    #[test]
    fn validate_cutoff_source_falls_through_to_validate_source_for_other_values() {
        // Plain BluRay / WEB / etc. take the regular validate_source
        // path, including canonicalization.
        assert_eq!(validate_cutoff_source("BluRay", "web"), "bluray");
        assert_eq!(validate_cutoff_source("garbage", "bluray"), "bluray");
    }

    #[test]
    fn validate_cutoff_source_is_case_sensitive_on_subtier_markers() {
        // `bluray_remux` / `bluray_bdmv` are exact-match in the
        // passthrough — `BLURAY_REMUX` doesn't get the special
        // treatment. It then falls through to validate_source where
        // `Source::from_str` (which underscore-matches "bdremux" /
        // "bluray" / etc., but NOT "bluray_remux") returns Unknown
        // → the default fires. Net result: a hand-crafted POST with
        // an uppercase sub-tier marker silently loses both the
        // sub-tier intent AND the BluRay source classification —
        // ends up with the supplied default. Worth pinning so a
        // refactor that adds case-folding to either path has to
        // confront this asymmetry.
        assert_eq!(validate_cutoff_source("BLURAY_REMUX", "web"), "web");
    }

    // ── validate_resolution ───────────────────────────────────────────

    #[test]
    fn validate_resolution_strips_p_suffix_for_db_storage() {
        // The DB column convention is bare-digit strings ("1080") so
        // `Resolution::from_str` reads them back uniformly. The
        // validator strips the trailing `p` Settings emits with the
        // dropdown.
        assert_eq!(validate_resolution("1080p", "1080"), "1080");
        assert_eq!(validate_resolution("720p", "1080"), "720");
        assert_eq!(validate_resolution("2160p", "1080"), "2160");
        assert_eq!(validate_resolution("480p", "1080"), "480");
    }

    #[test]
    fn validate_resolution_accepts_bare_digit() {
        // Both shapes in the wild — bare digit and suffixed.
        assert_eq!(validate_resolution("1080", "720"), "1080");
        assert_eq!(validate_resolution("720", "1080"), "720");
    }

    #[test]
    fn validate_resolution_accepts_4k_aliases() {
        // 4k / UHD aliases canonicalize to "2160" via Resolution::from_str.
        assert_eq!(validate_resolution("4k", "1080"), "2160");
        assert_eq!(validate_resolution("UHD", "1080"), "2160");
    }

    #[test]
    fn validate_resolution_falls_back_to_default_on_garbage() {
        assert_eq!(validate_resolution("garbage", "1080"), "1080");
        assert_eq!(validate_resolution("", "720"), "720");
        // Sonarr's 360p / 540p don't have Ryokan tiers and fold to
        // the default rather than persisting an unrecognized value.
        assert_eq!(validate_resolution("360p", "1080"), "1080");
        assert_eq!(validate_resolution("540p", "1080"), "1080");
    }

    // ── normalize_settings_tab ───────────────────────────────────────

    #[test]
    fn normalize_settings_tab_known_tabs_pass_through() {
        for tab in ["quality", "custom_formats", "groups", "general", "indexers"] {
            assert_eq!(normalize_settings_tab(Some(tab.into())), tab);
        }
    }

    #[test]
    fn normalize_settings_tab_unknown_or_missing_defaults_to_integrations() {
        // Integrations is the default landing — first-run users
        // most often need to wire a download client + Jellyfin
        // before doing anything else, so that's the natural first
        // tab.
        assert_eq!(normalize_settings_tab(None), "integrations");
        assert_eq!(
            normalize_settings_tab(Some("garbage".into())),
            "integrations"
        );
        assert_eq!(normalize_settings_tab(Some("".into())), "integrations");
    }

    // ── min_score_display ────────────────────────────────────────────

    #[test]
    fn min_score_display_renders_blank_for_no_floor_sentinel() {
        // i32::MIN is the "no minimum score floor" sentinel — must
        // render as an empty string so the input shows blank, not
        // "-2147483648".
        assert_eq!(min_score_display(i32::MIN), "");
    }

    #[test]
    fn min_score_display_renders_normal_values_as_string() {
        assert_eq!(min_score_display(0), "0");
        assert_eq!(min_score_display(50), "50");
        assert_eq!(min_score_display(-5), "-5");
        // Just-above-the-sentinel renders normally — only the exact
        // i32::MIN value is special.
        assert_eq!(min_score_display(i32::MIN + 1), (i32::MIN + 1).to_string());
    }

    // ── humanize_relative_time ───────────────────────────────────────

    #[test]
    fn humanize_relative_time_none_renders_never() {
        // No row in scheduled_task_runs yet → "Never", which is
        // what the Settings dashboard shows for unrun tasks.
        assert_eq!(humanize_relative_time(None), "Never");
    }

    fn now_ts() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn humanize_relative_time_under_one_minute_says_just_now() {
        assert_eq!(humanize_relative_time(Some(now_ts())), "Just now");
        assert_eq!(humanize_relative_time(Some(now_ts() - 30)), "Just now");
    }

    #[test]
    fn humanize_relative_time_under_one_hour_uses_minutes() {
        // The pluralization arm: 1 minute is singular, 2+ is plural.
        assert_eq!(humanize_relative_time(Some(now_ts() - 60)), "1 minute ago");
        assert_eq!(
            humanize_relative_time(Some(now_ts() - 120)),
            "2 minutes ago"
        );
        assert_eq!(
            humanize_relative_time(Some(now_ts() - 30 * 60)),
            "30 minutes ago"
        );
    }

    #[test]
    fn humanize_relative_time_under_one_day_uses_hours() {
        assert_eq!(humanize_relative_time(Some(now_ts() - 3600)), "1 hour ago");
        assert_eq!(humanize_relative_time(Some(now_ts() - 7200)), "2 hours ago");
        // 23h59m is still in hours.
        assert_eq!(
            humanize_relative_time(Some(now_ts() - (23 * 3600 + 59 * 60))),
            "23 hours ago"
        );
    }

    #[test]
    fn humanize_relative_time_one_day_or_more_uses_days() {
        assert_eq!(humanize_relative_time(Some(now_ts() - 86400)), "1 day ago");
        assert_eq!(
            humanize_relative_time(Some(now_ts() - 86400 * 7)),
            "7 days ago"
        );
    }

    #[test]
    fn humanize_relative_time_future_timestamp_renders_just_now() {
        // Defensive: a clock skew or pre-clock-init timestamp could
        // produce ts > now. The `.max(0)` keeps the delta non-negative
        // so we render "Just now" rather than "-N days ago".
        assert_eq!(humanize_relative_time(Some(now_ts() + 10000)), "Just now");
    }

    // ── extract_spec_labels ──────────────────────────────────────────

    #[test]
    fn extract_spec_labels_parses_spec_array_into_views() {
        let json = r#"{
            "name": "BD",
            "specifications": [
                {"name": "BluRay", "implementation": "ReleaseTitleSpecification", "negate": false, "required": true},
                {"name": "WEB", "implementation": "ReleaseTitleSpecification", "negate": true, "required": false}
            ]
        }"#;
        let labels = extract_spec_labels(json);
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].name, "BluRay");
        assert_eq!(labels[0].implementation, "ReleaseTitleSpecification");
        assert!(!labels[0].negate);
        assert!(labels[0].required);
        assert_eq!(labels[1].name, "WEB");
        assert!(labels[1].negate);
        assert!(!labels[1].required);
    }

    #[test]
    fn extract_spec_labels_returns_empty_on_invalid_json() {
        // Defensive: a parse failure mustn't bubble up — the caller
        // already surfaces the raw parse error via `parse_error`,
        // and the spec-pill row just renders empty.
        assert!(extract_spec_labels("{not json").is_empty());
        assert!(extract_spec_labels("").is_empty());
    }

    #[test]
    fn extract_spec_labels_returns_empty_when_specifications_missing() {
        // CF JSON without a "specifications" array (e.g. malformed
        // import or partial CF in flight) yields zero labels rather
        // than a panic.
        assert!(extract_spec_labels(r#"{"name": "BD"}"#).is_empty());
        // Wrong type for "specifications" → also empty.
        assert!(extract_spec_labels(r#"{"specifications": "oops"}"#).is_empty());
    }

    #[test]
    fn extract_spec_labels_uses_defaults_for_missing_fields() {
        // Each spec entry that omits a field falls back to the
        // typed default — empty strings for `name`/`implementation`,
        // false for both bools. This is what unblocks rendering
        // half-imported CFs in the edit drawer.
        let json = r#"{"specifications": [{}]}"#;
        let labels = extract_spec_labels(json);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].name, "");
        assert_eq!(labels[0].implementation, "");
        assert!(!labels[0].negate);
        assert!(!labels[0].required);
    }

    /// Indexer-picker pre-fill flow on the Settings → Indexers
    /// tab. `?template=<slug>` resolves through
    /// [`crate::services::indexer_catalog::find_seed`] and
    /// populates [`SettingsTemplate::indexer_seed`]; the form
    /// then renders pre-filled defaults from the seed instead of
    /// blank. The two precedence rules below (edit shadows seed,
    /// unknown-slug falls through) are the load-bearing
    /// invariants for the picker UX.
    mod indexer_picker {
        use super::super::*;
        use crate::test_support::{build_test_app_state, in_memory_pool};

        #[tokio::test]
        async fn template_slug_populates_indexer_seed() {
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let template = build_settings_template(
                &state,
                Some("indexers".to_string()),
                None,
                None,
                None,
                None,
                Some("animebytes".to_string()),
                None,
            )
            .await;
            let seed = template
                .indexer_seed
                .expect("animebytes slug should resolve to a seed");
            assert_eq!(seed.slug, "animebytes");
            assert_eq!(seed.display_name, "AnimeBytes");
            assert!(seed.is_private_tracker);
        }

        #[tokio::test]
        async fn unknown_template_slug_falls_through_to_picker() {
            // Stale `?template=…` links from a removed catalog
            // entry must degrade gracefully — the user lands on
            // the blank picker grid, not an error page.
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let template = build_settings_template(
                &state,
                Some("indexers".to_string()),
                None,
                None,
                None,
                None,
                Some("does-not-exist".to_string()),
                None,
            )
            .await;
            assert!(template.indexer_seed.is_none());
        }

        #[tokio::test]
        async fn edit_id_shadows_template_slug() {
            // `?edit_id=N&template=…` URLs must pick the row
            // (real DB values) over the seed (catalog defaults)
            // — otherwise an Edit click on an existing row that
            // happened to land on a seed-less form would
            // overwrite real values with catalog defaults on
            // Save.
            let db = in_memory_pool().await;
            let row_id = crate::models::indexers::insert(
                &db,
                crate::models::indexers::IndexerForm {
                    name: "Existing",
                    kind: "torznab",
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
            let template = build_settings_template(
                &state,
                Some("indexers".to_string()),
                Some(row_id),
                None,
                None,
                None,
                Some("animebytes".to_string()),
                None,
            )
            .await;
            // Prove the row's fields actually reached the
            // template — `is_some()` alone wouldn't catch a
            // future `get_by_id` projection bug that returned
            // a default-constructed row but kept the
            // seed-suppression branch intact.
            let edit = template
                .indexer_edit
                .as_ref()
                .expect("edit_id must populate indexer_edit");
            assert_eq!(edit.name, "Existing");
            assert_eq!(edit.id, row_id);
            assert!(
                template.indexer_seed.is_none(),
                "edit_id must shadow the template slug — seed defaults would clobber real row values"
            );
        }

        #[tokio::test]
        async fn no_template_no_edit_renders_blank_picker() {
            // The default landing on `?tab=indexers` shows the
            // grid + table; neither edit_id nor template
            // populated.
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let template = build_settings_template(
                &state,
                Some("indexers".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await;
            assert!(template.indexer_seed.is_none());
            assert!(template.indexer_edit.is_none());
            assert!(
                !template.indexer_catalog.is_empty(),
                "picker grid is always populated from the static catalog"
            );
        }
    }

    /// Issue #129 Phase 1 completion — non-HTMX path coverage for the
    /// three new per-tab subform handlers
    /// (`settings_general_submit`, `settings_quality_submit`,
    /// `settings_integrations_submit`).
    ///
    /// The browser-e2e suite at
    /// `tests/htmx_browser_e2e_settings_subforms.rs` covers the HTMX
    /// path (request lands with `HX-Request: true`, handler returns
    /// the small subform partial). It can't reach the no-JS fallback
    /// because every request from a real browser carries the htmx
    /// header once the vendored script loads. These unit tests fill
    /// that gap by calling the handlers directly with
    /// `HxRequest(false)`, which is the shape Axum produces when no
    /// `HX-Request` header is present (regular form-POST from a JS-
    /// disabled browser, or any external script hitting the
    /// endpoint with `curl`).
    ///
    /// Each test asserts:
    /// 1. The DB write happened — the handler did the same persistence
    ///    work as the HTMX path.
    /// 2. The response is the full `SettingsTemplate` HTML (carries
    ///    the `<h2>Settings</h2>` page header from `settings.html`,
    ///    which the per-tab subform partials don't include) — so
    ///    a regression that returns the partial regardless of the
    ///    HxRequest flag would visibly break a no-JS save (the
    ///    user would see a fragment with no nav / chrome).
    mod non_htmx_path {
        use super::super::*;
        use crate::test_support::{build_test_app_state, in_memory_pool};
        use axum::body::to_bytes;
        use axum::extract::State;
        use axum::response::IntoResponse;
        use axum_htmx::HxRequest;
        use sqlx::SqlitePool;

        /// Read the response body as a UTF-8 string. axum's `Response`
        /// is `Response<Body>` where `Body` is opaque; `to_bytes` with
        /// a generous limit (2 MiB) covers the full SettingsTemplate
        /// without truncating.
        async fn body_string(resp: axum::response::Response) -> String {
            let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024)
                .await
                .expect("read body");
            String::from_utf8(bytes.to_vec()).expect("utf-8 body")
        }

        async fn seed_initial_config(db: &SqlitePool) {
            // Minimum-viable Config row — every per-tab handler reads
            // the existing row to preserve fields it doesn't own. With
            // no row, the handlers early-return with the
            // "No config row found" error path; we want to exercise
            // the success path here.
            config::save_config(db, &config::Config::default())
                .await
                .expect("seed config");
        }

        #[tokio::test]
        async fn general_submit_non_htmx_returns_full_settings_page() {
            let db = in_memory_pool().await;
            seed_initial_config(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_general_submit(
                State(state),
                HxRequest(false),
                axum::Form(GeneralForm {
                    media_root: "/srv/media-non-htmx".to_string(),
                    title_language: "romaji".to_string(),
                    rss_enabled: None,
                    rss_interval_minutes: 30,
                    disable_nyaa_rss: None,
                    post_processing_enabled: None,
                    post_processing_mode: "hardlink".to_string(),
                    search_on_monitoring_change: None,
                }),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = body_string(resp).await;
            // `<h2>Settings</h2>` is the page-header marker that lives
            // in `settings.html` — the per-tab subform partials
            // (`general_form.html` etc.) don't include it. If the
            // handler accidentally returned the partial regardless of
            // `is_htmx`, this assertion fails.
            assert!(
                body.contains("<h2>Settings</h2>"),
                "non-HTMX response must be the full SettingsTemplate, not the partial"
            );
            // Persistence: the General-tab field landed.
            let saved = config::get_config(&db)
                .await
                .expect("get_config")
                .expect("config row");
            assert_eq!(saved.media_root, "/srv/media-non-htmx");
            assert_eq!(saved.title_language, "romaji");
        }

        #[tokio::test]
        async fn quality_submit_non_htmx_returns_full_settings_page() {
            let db = in_memory_pool().await;
            seed_initial_config(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_quality_submit(
                State(state),
                HxRequest(false),
                axum::Form(QualityForm {
                    preferred_groups: "TestGroup".to_string(),
                    blocked_groups: String::new(),
                    preferred_source: "bluray".to_string(),
                    preferred_resolution: "2160".to_string(),
                    cutoff_source: "bluray".to_string(),
                    cutoff_resolution: "2160".to_string(),
                    finished_series_quality: "prefer_bd".to_string(),
                    prefer_subs: "1".to_string(),
                    upgrade_search_enabled: None,
                    seadex_enabled: None,
                    default_custom_query_tokens: None,
                    default_restrict_to_uploader: None,
                }),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = body_string(resp).await;
            assert!(
                body.contains("<h2>Settings</h2>"),
                "non-HTMX response must be the full SettingsTemplate, not the partial"
            );
            let saved = config::get_config(&db)
                .await
                .expect("get_config")
                .expect("config row");
            assert_eq!(saved.preferred_groups, "TestGroup");
            assert_eq!(saved.preferred_resolution, "2160");
            assert_eq!(saved.preferred_source, "bluray");
        }

        #[tokio::test]
        async fn integrations_submit_non_htmx_returns_full_settings_page() {
            let db = in_memory_pool().await;
            seed_initial_config(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_integrations_submit(
                State(state),
                HxRequest(false),
                axum::Form(IntegrationsForm {
                    active_client: "qbittorrent".to_string(),
                    qbit_url: "http://qbit.local:8080".to_string(),
                    qbit_user: "admin".to_string(),
                    qbit_pass: "pw".to_string(),
                    qbit_category: "ryokan".to_string(),
                    qbit_download_path: "/downloads".to_string(),
                    deluge_url: String::new(),
                    deluge_password: String::new(),
                    deluge_label: String::new(),
                    deluge_download_path: String::new(),
                    transmission_url: String::new(),
                    transmission_user: String::new(),
                    transmission_password: String::new(),
                    transmission_label: String::new(),
                    transmission_download_path: String::new(),
                    rtorrent_url: String::new(),
                    rtorrent_user: String::new(),
                    rtorrent_password: String::new(),
                    rtorrent_label: String::new(),
                    rtorrent_download_path: String::new(),
                    // Empty Jellyfin so the connection-test side
                    // effect is a no-op (else this test would try to
                    // hit a non-existent server and slow down the
                    // suite by the connect-timeout).
                    jellyfin_url: String::new(),
                    jellyfin_api_key: String::new(),
                    sonarr_enabled: Some(String::new()),
                    sonarr_api_key: Some("sonarr-key-non-htmx".to_string()),
                    radarr_enabled: None,
                    radarr_api_key: None,
                    grab_preview_mode: Some("never".to_string()),
                    external_sync_interval_minutes: Some(45),
                }),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = body_string(resp).await;
            assert!(
                body.contains("<h2>Settings</h2>"),
                "non-HTMX response must be the full SettingsTemplate, not the partial"
            );
            let saved = config::get_config(&db)
                .await
                .expect("get_config")
                .expect("config row");
            assert_eq!(saved.qbit_url, "http://qbit.local:8080");
            assert_eq!(saved.qbit_user, "admin");
            assert_eq!(saved.sonarr_api_key, "sonarr-key-non-htmx");
            assert!(saved.sonarr_enabled);
            assert_eq!(saved.grab_preview_mode, "never");
            assert_eq!(saved.external_sync_interval_minutes, 45);
        }

        /// Regression for PR 133 review item #3: read-modify-write
        /// race across concurrent saves. Without `CONFIG_WRITE_LOCK`,
        /// the General handler reading existing_cfg + the Quality
        /// handler reading existing_cfg in parallel both see the
        /// pre-mutation row, then each writes back its own merge —
        /// the second writer's write loses whatever the first
        /// writer changed (because the second writer's struct-update
        /// merge built on a stale snapshot).
        ///
        /// With the lock, the second handler waits for the first to
        /// commit, reads the post-first-save row, and merges its
        /// change on top. Both fields land.
        ///
        /// Two concurrent saves via `tokio::join!`: General sets
        /// `title_language = "romaji"`, Quality sets
        /// `preferred_resolution = "2160"`. Final config must have
        /// **both** (the loser's write would silently drop one).
        #[tokio::test]
        async fn concurrent_general_and_quality_saves_dont_lose_updates() {
            let db = in_memory_pool().await;
            seed_initial_config(&db).await;
            let state = build_test_app_state(db.clone(), None);

            let general_state = state.clone();
            let quality_state = state.clone();
            let general = settings_general_submit(
                State(general_state),
                HxRequest(false),
                axum::Form(GeneralForm {
                    media_root: String::new(),
                    title_language: "romaji".to_string(),
                    rss_enabled: None,
                    rss_interval_minutes: 15,
                    disable_nyaa_rss: None,
                    post_processing_enabled: None,
                    post_processing_mode: "hardlink".to_string(),
                    search_on_monitoring_change: None,
                }),
            );
            let quality = settings_quality_submit(
                State(quality_state),
                HxRequest(false),
                axum::Form(QualityForm {
                    preferred_groups: String::new(),
                    blocked_groups: String::new(),
                    preferred_source: "web".to_string(),
                    preferred_resolution: "2160".to_string(),
                    cutoff_source: "bluray".to_string(),
                    cutoff_resolution: "1080".to_string(),
                    finished_series_quality: "prefer_bd".to_string(),
                    prefer_subs: "1".to_string(),
                    upgrade_search_enabled: None,
                    seadex_enabled: None,
                    default_custom_query_tokens: None,
                    default_restrict_to_uploader: None,
                }),
            );
            let (_a, _b) = tokio::join!(general, quality);

            let saved = config::get_config(&db)
                .await
                .expect("get_config")
                .expect("config row");
            // Both handlers' field changes must land — interleaving
            // would have dropped one of them.
            assert_eq!(saved.title_language, "romaji");
            assert_eq!(saved.preferred_resolution, "2160");
        }

        /// Companion regression: the early-return path when the
        /// config row is missing. Surfaces the "No config row found —
        /// run /setup first." error string in the response, which
        /// the operator sees when they hit the endpoint before
        /// completing first-run setup.
        #[tokio::test]
        async fn general_submit_with_no_config_row_renders_friendly_error() {
            // Note: NO seed_initial_config call here — pool is empty.
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let resp = settings_general_submit(
                State(state),
                HxRequest(false),
                axum::Form(GeneralForm {
                    media_root: String::new(),
                    title_language: "english".to_string(),
                    rss_enabled: None,
                    rss_interval_minutes: 15,
                    disable_nyaa_rss: None,
                    post_processing_enabled: None,
                    post_processing_mode: "hardlink".to_string(),
                    search_on_monitoring_change: None,
                }),
            )
            .await
            .into_response();
            let body = body_string(resp).await;
            assert!(
                body.contains("No config row found"),
                "expected friendly first-run error in response body"
            );
        }
    }
}
