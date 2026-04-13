use askama::Template;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::models::{session, user};
use crate::models::log::LogCategory;
use crate::services::logger;
use crate::AppState;

// ---------- Login rate limiting ----------
//
// In-process throttle: reject once a given key has accumulated 5 failed
// logins in a sliding 60-second window. We track two keys per attempt —
// one per username and one per client IP — so neither a per-account nor a
// distributed-across-usernames-from-one-box brute force can slip through.
// Keeping this in memory is fine for the self-hosted PVR deployment: a
// process restart resets the state, but an attacker sustaining 5/min across
// restarts is indistinguishable from an unlimited attacker in practice.

const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_MAX_FAILURES: usize = 5;

static LOGIN_FAILURES: LazyLock<Mutex<HashMap<String, Vec<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns `Ok(())` if `key` is under the failure threshold, or
/// `Err(())` if it is rate-limited right now. Always sweeps expired
/// entries for `key` as a side effect.
fn login_check(key: &str) -> Result<(), ()> {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let entry = guard.entry(key.to_string()).or_default();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    entry.retain(|t| *t > cutoff);
    if entry.len() >= LOGIN_MAX_FAILURES {
        Err(())
    } else {
        Ok(())
    }
}

/// Record a failed login attempt against `key`.
fn login_record_failure(key: &str) {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    let entry = guard.entry(key.to_string()).or_default();
    let cutoff = Instant::now() - LOGIN_WINDOW;
    entry.retain(|t| *t > cutoff);
    entry.push(Instant::now());
}

/// Reset the counter for `key` after a successful login so a
/// legitimate user who mistyped a few times isn't locked out by
/// their own prior failures.
fn login_clear(key: &str) {
    let mut guard = LOGIN_FAILURES.lock().unwrap();
    guard.remove(key);
}

/// Best-effort client IP extraction. Prefers the leftmost entry of
/// `X-Forwarded-For` (the address the reverse proxy saw from the
/// outside world), falling back to `X-Real-IP`, and finally to the
/// literal string `"unknown"` so the per-IP throttle key still exists
/// when we can't pin down an address. Defaults are safe because we
/// bucket by (username, ip) and the username bucket still works.
fn client_ip_from_headers(headers: &HeaderMap) -> String {
    if let Some(h) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = h.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if let Some(h) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "unknown".to_string()
}

/// Whether to append `Secure` to the session cookie. Read once at startup
/// from `RYOKAN_COOKIE_SECURE` (values `1`, `true`, `yes`, `on` enable it,
/// case-insensitive). Default off so `cargo run` on localhost keeps working
/// over HTTP; flip on for any HTTPS-fronted deployment so a stolen session
/// cookie can't leak over cleartext.
static COOKIE_SECURE: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("RYOKAN_COOKIE_SECURE")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
});

// ---------- Templates ----------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: Option<String>,
}

// ---------- Form data ----------

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
pub struct SetupForm {
    username: String,
    password: String,
    confirm: String,
}

// ---------- Helpers ----------

fn get_session_token(req: &Request<Body>) -> Option<String> {
    let cookie_header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix("session=") {
            return Some(value.to_string());
        }
    }
    None
}

fn set_session_cookie(token: &str) -> String {
    let secure = if *COOKIE_SECURE { "; Secure" } else { "" };
    format!(
        "session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800{}",
        token, secure
    )
}

fn clear_session_cookie() -> String {
    // Match the Secure attribute on the set path — some browsers refuse to
    // clear a Secure cookie from a non-Secure response, but the reverse is
    // harmless, so mirror whatever the set path emitted.
    let secure = if *COOKIE_SECURE { "; Secure" } else { "" };
    format!("session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}", secure)
}

// ---------- CSRF helpers ----------

/// Extract the host portion (without scheme or port) from an Origin or
/// Referer header value. Returns None if the value is not a well-formed
/// absolute URL we can reason about.
fn url_host(value: &str) -> Option<String> {
    // Strip scheme.
    let after_scheme = value.split_once("://").map(|(_, rest)| rest)?;
    // Host ends at the first `/`, `?`, `#`, or end of string.
    let host_end = after_scheme
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let host_with_port = &after_scheme[..host_end];
    if host_with_port.is_empty() {
        return None;
    }
    // Strip port so we compare against the Host header cleanly — Host may or
    // may not include a port depending on the client, and we want to match
    // either way. An attacker can't spoof Host from a cross-origin browser
    // anyway, so we're comparing hosts for equality as a sanity check.
    let host_only = host_with_port.split_once(':').map(|(h, _)| h).unwrap_or(host_with_port);
    Some(host_only.to_ascii_lowercase())
}

fn host_of(req: &Request<Body>) -> Option<String> {
    let raw = req.headers().get(header::HOST)?.to_str().ok()?;
    let host_only = raw.split_once(':').map(|(h, _)| h).unwrap_or(raw);
    Some(host_only.to_ascii_lowercase())
}

/// Verify that a state-changing request came from the same origin this
/// server is serving. Uses the Origin header if present (modern browsers
/// set this on all POST/PUT/PATCH/DELETE requests, including cross-site
/// form submissions), falling back to Referer. This is the OWASP
/// "Verifying Origin With Standard Headers" CSRF mitigation and is
/// sufficient because an attacker page cannot forge either header from
/// cross-origin JavaScript.
///
/// Returns `Ok(())` if the method is safe (GET/HEAD/OPTIONS) or the
/// request is same-origin. Returns `Err` with a short reason otherwise.
fn verify_same_origin(req: &Request<Body>) -> Result<(), &'static str> {
    match *req.method() {
        Method::GET | Method::HEAD | Method::OPTIONS => return Ok(()),
        _ => {}
    }

    let host = match host_of(req) {
        Some(h) => h,
        None => return Err("missing Host header"),
    };

    // Prefer Origin (always set by browsers on unsafe methods).
    if let Some(origin) = req.headers().get("origin").and_then(|v| v.to_str().ok()) {
        // "null" is what browsers send for e.g. sandboxed iframes — never
        // same-origin by definition.
        if origin == "null" {
            return Err("null origin");
        }
        return match url_host(origin) {
            Some(h) if h == host => Ok(()),
            Some(_) => Err("origin host mismatch"),
            None => Err("malformed Origin header"),
        };
    }

    // Fall back to Referer when Origin is absent (older clients, some
    // proxies). Reject if neither header is present — on POST from a real
    // browser at least one of them will be set.
    if let Some(referer) = req.headers().get(header::REFERER).and_then(|v| v.to_str().ok()) {
        return match url_host(referer) {
            Some(h) if h == host => Ok(()),
            Some(_) => Err("referer host mismatch"),
            None => Err("malformed Referer header"),
        };
    }

    Err("missing Origin and Referer headers")
}

fn csrf_forbidden(reason: &str) -> Response {
    tracing::warn!("CSRF rejection: {}", reason);
    (StatusCode::FORBIDDEN, "Forbidden: cross-origin request rejected").into_response()
}

// ---------- Auth middleware ----------

pub async fn require_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // If no users exist, redirect to setup.
    if let Ok(false) = user::has_users(&state.db).await {
        return Redirect::to("/setup").into_response();
    }

    // Check session cookie.
    let token = match get_session_token(&req) {
        Some(t) => t,
        None => return Redirect::to("/login").into_response(),
    };

    match session::validate_session(&state.db, &token).await {
        Ok(Some(_user_id)) => {
            // Session is valid. Enforce same-origin on state-changing
            // requests to block CSRF — a malicious page at evil.com cannot
            // forge either Origin or Referer from cross-origin JS, so even
            // though the browser will attach our session cookie on top-level
            // form POSTs (SameSite=Lax permits this for GET-style
            // navigations, but the rejection here catches the rest), a
            // cross-origin POST is rejected.
            if let Err(reason) = verify_same_origin(&req) {
                return csrf_forbidden(reason);
            }
            next.run(req).await
        }
        _ => Redirect::to("/login").into_response(),
    }
}

/// CSRF middleware for the public `/login` and `/setup` POST paths. These
/// routes have no session to attach a token to, so we fall back to the
/// same Origin/Referer same-origin check used on authenticated routes.
/// An attacker's page cannot set either header to our host from
/// cross-origin JavaScript, so a drive-by POST to `/setup` from a
/// malicious site is rejected before `setup_submit` ever sees the form.
pub async fn csrf_public(req: Request<Body>, next: Next) -> Response {
    if let Err(reason) = verify_same_origin(&req) {
        return csrf_forbidden(reason);
    }
    next.run(req).await
}

// ---------- Setup ----------

pub async fn setup_page(State(state): State<AppState>) -> impl IntoResponse {
    // If users already exist, redirect to login.
    if let Ok(true) = user::has_users(&state.db).await {
        return Redirect::to("/login").into_response();
    }

    let template = SetupTemplate { error: None };
    Html(template.render().unwrap_or_default()).into_response()
}

pub async fn setup_submit(
    State(state): State<AppState>,
    Form(form): Form<SetupForm>,
) -> impl IntoResponse {
    if let Ok(true) = user::has_users(&state.db).await {
        return Redirect::to("/login").into_response();
    }

    if form.username.trim().is_empty() || form.password.is_empty() {
        let template = SetupTemplate {
            error: Some("Username and password are required.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    if form.password != form.confirm {
        let template = SetupTemplate {
            error: Some("Passwords do not match.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    match user::create_user(&state.db, form.username.trim(), &form.password).await {
        Ok(user_id) => {
            logger::info(&state.db, LogCategory::Auth, &format!("Account created: {}", form.username.trim()), "").await;
            let token = session::create_session(&state.db, user_id)
                .await
                .unwrap_or_default();

            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::SET_COOKIE, set_session_cookie(&token))
                .body(Body::empty())
                .expect("setup-redirect response uses only static headers, should always build")
                .into_response()
        }
        Err(e) => {
            let template = SetupTemplate {
                error: Some(format!("Failed to create account: {}", e)),
            };
            Html(template.render().unwrap_or_default()).into_response()
        }
    }
}

// ---------- Login ----------

pub async fn login_page(State(state): State<AppState>) -> impl IntoResponse {
    if let Ok(false) = user::has_users(&state.db).await {
        return Redirect::to("/setup").into_response();
    }

    let template = LoginTemplate { error: None };
    Html(template.render().unwrap_or_default()).into_response()
}

pub async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    // Resolve the bucket keys up front so we always rate-limit, even when
    // the incoming form has an empty username.
    let ip = client_ip_from_headers(&headers);
    let ip_key = format!("ip:{}", ip);
    let user_key = format!("u:{}", form.username.trim().to_ascii_lowercase());

    // Pre-check: if either bucket is already over the limit, reject the
    // attempt without running bcrypt. The response is the same as a
    // wrong-password failure on purpose — we don't want to leak to a
    // probing attacker whether they're throttled vs. just wrong.
    if login_check(&user_key).is_err() || login_check(&ip_key).is_err() {
        logger::warn(
            &state.db,
            LogCategory::Auth,
            &format!("Login rate-limited: {} from {}", form.username, ip),
            "",
        )
        .await;
        let template = LoginTemplate {
            error: Some("Too many failed attempts. Please wait a minute and try again.".into()),
        };
        return Html(template.render().unwrap_or_default()).into_response();
    }

    match user::verify_user(&state.db, &form.username, &form.password).await {
        Ok(Some(u)) => {
            // Successful login — clear the counters so an honest user who
            // mistyped a few times isn't punished for their own typos.
            login_clear(&user_key);
            login_clear(&ip_key);
            logger::info(&state.db, LogCategory::Auth, &format!("Login: {}", form.username), "").await;
            let token = session::create_session(&state.db, u.id)
                .await
                .unwrap_or_default();

            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::SET_COOKIE, set_session_cookie(&token))
                .body(Body::empty())
                .expect("login-redirect response uses only static headers, should always build")
                .into_response()
        }
        _ => {
            login_record_failure(&user_key);
            login_record_failure(&ip_key);
            logger::warn(&state.db, LogCategory::Auth, &format!("Failed login attempt: {}", form.username), "").await;
            let template = LoginTemplate {
                error: Some("Invalid username or password.".into()),
            };
            Html(template.render().unwrap_or_default()).into_response()
        }
    }
}

// ---------- Logout ----------

pub async fn logout(State(state): State<AppState>, req: Request<Body>) -> impl IntoResponse {
    if let Some(token) = get_session_token(&req) {
        let _ = session::delete_session(&state.db, &token).await;
    }

    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/login")
        .header(header::SET_COOKIE, clear_session_cookie())
        .body(Body::empty())
        .expect("logout-redirect response uses only static headers, should always build")
        .into_response()
}
