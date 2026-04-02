use askama::Template;
use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;

use crate::models::{session, user};
use crate::models::log::LogCategory;
use crate::services::logger;
use crate::AppState;

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
    format!("session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800", token)
}

fn clear_session_cookie() -> String {
    "session=; Path=/; HttpOnly; Max-Age=0".to_string()
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
        Ok(Some(_user_id)) => next.run(req).await,
        _ => Redirect::to("/login").into_response(),
    }
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
                .unwrap()
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
    Form(form): Form<LoginForm>,
) -> impl IntoResponse {
    match user::verify_user(&state.db, &form.username, &form.password).await {
        Ok(Some(u)) => {
            logger::info(&state.db, LogCategory::Auth, &format!("Login: {}", form.username), "").await;
            let token = session::create_session(&state.db, u.id)
                .await
                .unwrap_or_default();

            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::SET_COOKIE, set_session_cookie(&token))
                .body(Body::empty())
                .unwrap()
                .into_response()
        }
        _ => {
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
        .unwrap()
        .into_response()
}
