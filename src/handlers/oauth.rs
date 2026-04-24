//! OAuth start / submit endpoints for AniList and MyAnimeList.
//!
//! Issue #62 PR A. Each provider has two endpoints:
//!
//!   - `GET  /settings/oauth/{provider}/start`  — redirects the user's
//!     browser to the provider's authorize URL. For MAL we first
//!     generate a PKCE verifier and stash it in
//!     [`services::oauth_state`] so the submit path can echo it back.
//!   - `POST /settings/oauth/{provider}/submit` — accepts the token
//!     (AL) or code (MAL) the user pasted from the Ryokan-hosted
//!     broker page, validates it by calling the provider's user-info
//!     endpoint, and persists the linked account via
//!     [`models::external_accounts::link`].
//!
//! Plus one shared unlink endpoint:
//!
//!   - `POST /settings/oauth/unlink` — drops the currently-linked
//!     account's row. Per decision #8, imported series stay put.
//!
//! Flow diagrams live in the #62 plan doc. Summary:
//!
//! - **AL (Implicit Grant):** Ryokan → AL authorize → broker page
//!   (token in URL fragment, never reaches a server) → user copies
//!   → pastes into Ryokan → Ryokan calls AL's `Viewer` GraphQL query
//!   with the pasted token to validate + fetch username +
//!   score_format.
//! - **MAL (Auth Code + PKCE plain):** Ryokan generates verifier →
//!   stashes → redirects to MAL authorize with
//!   `code_challenge = verifier` + `code_challenge_method = plain`
//!   (MAL doesn't support S256) → broker page displays code → user
//!   pastes → Ryokan retrieves verifier, POSTs to MAL's token
//!   endpoint with `code` + `code_verifier` → receives access +
//!   refresh tokens → calls `GET /v2/users/@me` for username.
//!
//! Client IDs are hardcoded per decision #2 (shared native-app
//! client pattern — Mihon, Taiga, etc. do the same). No secrets:
//! AL implicit grant doesn't need one; MAL's "other" app type gets
//! none since PKCE substitutes.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::external_accounts::{self, LinkRequest, PROVIDER_ANILIST, PROVIDER_MAL};
use crate::models::log::LogCategory;
use crate::services::{logger, oauth_state};

/// AniList public client ID. Registered 2026-04-22 against the
/// author's AL account; the AL app page documents the redirect URI
/// as `https://johnthreekay.github.io/Ryokan/auth/anilist/`. Safe
/// to ship in the binary per OAuth public-client conventions.
const ANILIST_CLIENT_ID: &str = "39806";

/// MyAnimeList public client ID (App Type `other`). Registered
/// 2026-04-22. Native-client OAuth flow substitutes PKCE for a
/// client secret, which is the honest OAuth 2.0 public-client model.
const MAL_CLIENT_ID: &str = "5205ccde38839a4afc6b03bbecfaa9c7";

/// Ryokan-hosted broker pages (orphan `gh-pages` branch on the
/// GitHub repo). Static HTML that reads the token / code out of the
/// URL and displays it for the user to copy-paste back into Ryokan.
/// URL fragments on the AL side never touch an HTTP server; the MAL
/// side query param is public-by-design (MAL's own auth page already
/// displayed it).
const ANILIST_REDIRECT_URI: &str = "https://johnthreekay.github.io/Ryokan/auth/anilist/";
const MAL_REDIRECT_URI: &str = "https://johnthreekay.github.io/Ryokan/auth/mal/";

/// Verifier length in base64url characters. RFC 7636 allows 43-128.
/// 43 is the minimum, derived from 32 random bytes. Enough entropy;
/// keeps the URL short.
const PKCE_VERIFIER_LEN: usize = 43;

// ── AniList start ────────────────────────────────────────────────────

pub async fn anilist_start() -> Redirect {
    // Implicit grant: response_type=token. AL returns the access
    // token directly in the URL fragment after user approval, so
    // there's no server-side code exchange step.
    let url = format!(
        "https://anilist.co/api/v2/oauth/authorize?client_id={}&redirect_uri={}&response_type=token",
        ANILIST_CLIENT_ID,
        urlencoding::encode(ANILIST_REDIRECT_URI),
    );
    Redirect::temporary(&url)
}

// ── MAL start ────────────────────────────────────────────────────────

pub async fn mal_start(State(state): State<AppState>) -> Redirect {
    // Fresh PKCE verifier per /start call. Overwrites any prior
    // pending MAL attempt (decision matched in services::oauth_state
    // — second stash wins, first is discarded).
    let verifier = generate_pkce_verifier();
    oauth_state::stash(&state.oauth_state, PROVIDER_MAL, verifier.clone());

    // MAL's authorize URL: response_type=code, code_challenge = the
    // verifier itself (plain method), code_challenge_method explicitly
    // set to `plain` because MAL rejects the request when S256 is
    // specified (live-probed 2026-04-22).
    let url = format!(
        "https://myanimelist.net/v1/oauth2/authorize?response_type=code&client_id={}&code_challenge={}&code_challenge_method=plain&redirect_uri={}",
        MAL_CLIENT_ID,
        urlencoding::encode(&verifier),
        urlencoding::encode(MAL_REDIRECT_URI),
    );
    Redirect::temporary(&url)
}

// ── AniList submit ───────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TokenSubmitForm {
    /// Raw access token the user pasted from the AL broker page.
    /// Validated by round-tripping through AL's `Viewer` GraphQL
    /// query — an invalid token gets rejected before we persist.
    pub access_token: String,
}

#[derive(Serialize)]
pub struct LinkResponse {
    pub ok: bool,
    pub provider: String,
    pub username: String,
}

pub async fn anilist_submit(
    State(state): State<AppState>,
    Json(form): Json<TokenSubmitForm>,
) -> Result<Json<LinkResponse>, (StatusCode, String)> {
    let token = form.access_token.trim().to_string();
    if token.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Paste the token from the AniList callback page.".into(),
        ));
    }

    // Round-trip through Viewer to validate + populate username /
    // score_format. A 400/401 from AL means the user pasted a bad
    // token; surface that as the same status code.
    let viewer = fetch_anilist_viewer(&token).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("AniList rejected the token: {e}"),
        )
    })?;

    let id = external_accounts::link(
        &state.db,
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: viewer.id.to_string(),
            username: viewer.name.clone(),
            access_token: token,
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: viewer.score_format.clone(),
        },
    )
    .await
    .map_err(|e| (StatusCode::CONFLICT, e))?;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!("Linked AniList account '{}'", viewer.name),
        &format!("external_account_id={id}"),
    )
    .await;

    Ok(Json(LinkResponse {
        ok: true,
        provider: PROVIDER_ANILIST.into(),
        username: viewer.name,
    }))
}

// ── MAL submit ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CodeSubmitForm {
    /// Authorization code the user pasted from the MAL broker page.
    pub code: String,
}

pub async fn mal_submit(
    State(state): State<AppState>,
    Json(form): Json<CodeSubmitForm>,
) -> Result<Json<LinkResponse>, (StatusCode, String)> {
    let code = form.code.trim().to_string();
    if code.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Paste the code from the MyAnimeList callback page.".into(),
        ));
    }

    let verifier = oauth_state::take(&state.oauth_state, PROVIDER_MAL).ok_or((
        StatusCode::BAD_REQUEST,
        "No pending MyAnimeList authorization — start the link flow again.".into(),
    ))?;

    let tokens = exchange_mal_code(&code, &verifier).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("MyAnimeList rejected the code: {e}"),
        )
    })?;

    let me = fetch_mal_me(&tokens.access_token).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("MAL /v2/users/@me failed: {e}"),
        )
    })?;

    // MAL access tokens expire in 30 days. `expires_in` is seconds-
    // until-expiry; add to current time for an absolute timestamp.
    let expires_at = current_unix_ts() + tokens.expires_in;

    let id = external_accounts::link(
        &state.db,
        LinkRequest {
            provider: PROVIDER_MAL.to_string(),
            provider_user_id: me.name.clone(),
            username: me.name.clone(),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            access_token_expires_at: Some(expires_at),
            score_format: "POINT_10".to_string(),
        },
    )
    .await
    .map_err(|e| (StatusCode::CONFLICT, e))?;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!("Linked MyAnimeList account '{}'", me.name),
        &format!("external_account_id={id}"),
    )
    .await;

    Ok(Json(LinkResponse {
        ok: true,
        provider: PROVIDER_MAL.into(),
        username: me.name,
    }))
}

// ── Shared unlink ────────────────────────────────────────────────────

pub async fn unlink(State(state): State<AppState>) -> impl IntoResponse {
    let current = external_accounts::get_current(&state.db).await;
    match current {
        Ok(Some(account)) => {
            if let Err(e) = external_accounts::unlink(&state.db, account.id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"ok": false, "error": e})),
                );
            }
            logger::info(
                &state.db,
                LogCategory::ExternalSync,
                &format!(
                    "Unlinked {} account '{}'",
                    account.provider, account.username
                ),
                &format!("external_account_id={}", account.id),
            )
            .await;
            (
                StatusCode::OK,
                Json(serde_json::json!({"ok": true, "provider": account.provider})),
            )
        }
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "already": "unlinked"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": e})),
        ),
    }
}

// ── Provider call helpers ────────────────────────────────────────────

#[derive(Deserialize)]
struct AniListViewer {
    id: i64,
    name: String,
    #[serde(default)]
    score_format: String,
}

async fn fetch_anilist_viewer(token: &str) -> Result<AniListViewer, String> {
    let query = r#"{"query":"query { Viewer { id name mediaListOptions { scoreFormat } } }"}"#;
    let client = http_client();
    let resp = client
        .post("https://graphql.anilist.co")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
        .body(query.to_string())
        .send()
        .await
        .map_err(|e| format!("AniList HTTP error: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("AniList response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("AniList returned {status}: {body}"));
    }

    // The GraphQL shape is `{"data": {"Viewer": { id, name, mediaListOptions: { scoreFormat } }}}`.
    // Flatten via a custom Deserialize pass — defining the whole tree
    // as serde structs would be overkill for three fields.
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("AniList Viewer parse failed: {e}"))?;
    let viewer_node = v
        .get("data")
        .and_then(|d| d.get("Viewer"))
        .ok_or_else(|| format!("AniList Viewer missing in response: {body}"))?;
    let id = viewer_node
        .get("id")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| format!("AniList Viewer.id missing: {body}"))?;
    let name = viewer_node
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("AniList Viewer.name missing: {body}"))?
        .to_string();
    let score_format = viewer_node
        .get("mediaListOptions")
        .and_then(|m| m.get("scoreFormat"))
        .and_then(|s| s.as_str())
        .unwrap_or("POINT_10")
        .to_string();
    Ok(AniListViewer {
        id,
        name,
        score_format,
    })
}

#[derive(Deserialize)]
struct MalTokenResponse {
    access_token: String,
    refresh_token: String,
    /// Seconds-until-expiry per the OAuth spec; MAL returns 2592000
    /// (30 days) for access tokens on issue.
    expires_in: i64,
}

async fn exchange_mal_code(code: &str, verifier: &str) -> Result<MalTokenResponse, String> {
    let client = http_client();
    let resp = client
        .post("https://myanimelist.net/v1/oauth2/token")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
        .form(&[
            ("client_id", MAL_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", MAL_REDIRECT_URI),
        ])
        .send()
        .await
        .map_err(|e| format!("MAL token HTTP error: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("MAL token response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("MAL token endpoint returned {status}: {body}"));
    }

    serde_json::from_str::<MalTokenResponse>(&body)
        .map_err(|e| format!("MAL token response parse failed: {e} (body: {body})"))
}

#[derive(Deserialize)]
struct MalUserInfo {
    name: String,
}

async fn fetch_mal_me(token: &str) -> Result<MalUserInfo, String> {
    let client = http_client();
    let resp = client
        .get("https://api.myanimelist.net/v2/users/@me")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("MAL @me HTTP error: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("MAL @me response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("MAL @me returned {status}: {body}"));
    }

    serde_json::from_str::<MalUserInfo>(&body)
        .map_err(|e| format!("MAL @me parse failed: {e} (body: {body})"))
}

// ── Small helpers ────────────────────────────────────────────────────

/// Build a 43-char base64url PKCE verifier from 32 random bytes.
/// Matches the MAL "plain" challenge format — we send the verifier
/// itself to MAL as the challenge, so it has to be URL-safe.
fn generate_pkce_verifier() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    // `URL_SAFE_NO_PAD` encoding of 32 bytes is 43 chars — defensive
    // truncate just in case the engine ever returns more.
    encoded.chars().take(PKCE_VERIFIER_LEN).collect()
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client build")
}

fn current_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_is_43_chars_url_safe() {
        let v = generate_pkce_verifier();
        assert_eq!(v.len(), PKCE_VERIFIER_LEN, "verifier must be 43 chars");
        // URL-safe alphabet: A-Z, a-z, 0-9, `-`, `_`. No `+` / `/` / `=`.
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier must be URL-safe: {v}"
        );
    }

    #[test]
    fn successive_verifiers_are_distinct() {
        // Not a crypto assertion — just a smoke check that the RNG
        // is wired up. A fixed or monotonic verifier would silently
        // break PKCE's whole point.
        let a = generate_pkce_verifier();
        let b = generate_pkce_verifier();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn anilist_start_redirects_to_authorize_url() {
        let redirect = anilist_start().await;
        let resp = redirect.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::TEMPORARY_REDIRECT);
        let location = resp
            .headers()
            .get("location")
            .expect("Location header")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.starts_with("https://anilist.co/api/v2/oauth/authorize"),
            "wrong host: {location}"
        );
        assert!(
            location.contains(&format!("client_id={}", ANILIST_CLIENT_ID)),
            "client_id missing: {location}"
        );
        assert!(
            location.contains("response_type=token"),
            "implicit grant response_type missing: {location}"
        );
        // Redirect URI must match the broker page exactly — AL
        // rejects the request with "redirect_uri mismatch" otherwise.
        assert!(
            location.contains("johnthreekay.github.io%2FRyokan%2Fauth%2Fanilist"),
            "redirect_uri not URL-encoded correctly: {location}"
        );
    }

    #[tokio::test]
    async fn mal_start_redirects_with_plain_pkce_challenge() {
        // Critical detail per decision #1 research: MAL requires
        // `code_challenge_method=plain`, NOT S256 (live-probed
        // 2026-04-22). A silent switch to S256 would break the
        // MAL flow entirely — token exchange would fail with
        // "invalid_grant." Pin the value.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let state = crate::test_support::build_test_app_state(db, None);

        let redirect = mal_start(State(state.clone())).await;
        let resp = redirect.into_response();
        let location = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.starts_with("https://myanimelist.net/v1/oauth2/authorize"),
            "wrong host: {location}"
        );
        assert!(location.contains(&format!("client_id={MAL_CLIENT_ID}")));
        assert!(location.contains("response_type=code"));
        assert!(location.contains("code_challenge_method=plain"));
        assert!(location.contains("code_challenge="));

        // A verifier must now be stashed — the submit path needs it.
        let stashed = oauth_state::take(&state.oauth_state, PROVIDER_MAL);
        assert!(
            stashed.is_some(),
            "start must stash verifier for the subsequent submit"
        );
    }
}
