//! Notifications handler surface (issue #119).
//!
//! Settings UI's "Send test" button lands here. The handler reads
//! the live provider out of the cache (the user just saved it; the
//! save handler fired `rebuild_notification_providers_cache`),
//! synthesizes a `Health` event, and returns the receiver's HTTP
//! status + truncated body inline so users can debug from the
//! Settings UI without opening browser devtools.
//!
//! Future endpoints (per-provider CRUD, the matrix-toggle endpoints
//! powering issue #121's Settings UI) land as siblings here.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::AppState;
use crate::services::notifications::{NotificationEvent, webhook};

/// `POST /api/notifications/{id}/test` — send a synthetic
/// `Health { kind: "test", message: "..." }` event to the targeted
/// provider only. Bypasses the per-event matrix (Health is
/// default-off so a matrix-honoring path would no-op).
///
/// Response shape:
/// - 200 + `{"status": <int>, "body": "<truncated>"}` on send-side
///   success (means the request hit the receiver — receiver may
///   still have returned a 4xx/5xx, which is what `status` reports).
/// - 4xx / 5xx + `{"error": "..."}` for transport failures, timeouts,
///   serialization errors, or "provider not in cache."
///
/// `provider not in cache` is a 404 because the row may exist in the
/// DB but be disabled, or have just been deleted from another tab.
/// `transport error` / `timeout` is 502 — Ryokan is the upstream
/// proxy here; the receiver is the unreachable origin. Serialization
/// failures are 500 (programmer error in Ryokan, not a user-fixable
/// state).
pub async fn test_provider(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<webhook::TestSendResult>, (StatusCode, Json<serde_json::Value>)> {
    // Resolve the provider out of the live cache snapshot. The cache
    // is rebuilt on every notifications-CRUD save, so the freshly-
    // saved row is visible here without a re-read.
    let providers = state.notification_providers.read().await.clone();
    let provider = providers
        .iter()
        .find(|p| p.id() == id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("notification provider #{id} not found in cache (disabled or recently deleted?)"),
                })),
            )
        })?;

    // Foundation PR ships the trait + dispatch; this PR ships the
    // webhook impl. Discord (issue #120) lands as another `kind` arm
    // here — the test endpoint stays generic over the trait but
    // currently knows only how to call into the webhook impl's
    // surface for the inline status+body response. A `kind`-shaped
    // match keeps that surface honest as more impls land.
    let event = NotificationEvent::Health {
        kind: "test".into(),
        message: "Test notification from Ryokan".into(),
    };
    match provider.kind() {
        "webhook" => {
            // Re-resolve through the trait id and look up the
            // concrete webhook impl from the cache snapshot. The
            // unsafe-style downcast pattern (Any) isn't worth the
            // ergonomic cost; instead we re-load the row from the
            // DB and reconstruct a one-shot `WebhookProvider` for
            // the test path. Cheap (one DB query, one URL parse)
            // and avoids tying the test endpoint into the trait's
            // object-safety constraints.
            let row = match crate::services::notifications::store::get_provider(&state.db, id).await
            {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return Err((
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "error": format!("notification provider #{id} not found"),
                        })),
                    ));
                }
                Err(e) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": format!("DB read failed: {e}")})),
                    ));
                }
            };
            let p = match webhook::WebhookProvider::from_row(row.id, row.name, &row.config_json) {
                Ok(p) => p,
                Err(e) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": format!("invalid webhook config: {e}"),
                        })),
                    ));
                }
            };
            match webhook::send_test(&p, &event).await {
                Ok(result) => Ok(Json(result)),
                Err(e) => Err((
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e})),
                )),
            }
        }
        other => Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": format!("test endpoint not yet wired for provider kind {other:?}"),
            })),
        )),
    }
}
