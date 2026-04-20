use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, Response, StatusCode, header},
};

use crate::{AppState, services::artwork};

pub async fn artwork(
    State(state): State<AppState>,
    Path(cache_key): Path<String>,
) -> Result<Response<Body>, StatusCode> {
    let (bytes, content_type) = artwork::load_bytes(&state.db, &cache_key)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("image/jpeg")),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(resp)
}
