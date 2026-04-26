//! Settings → Indexers CRUD handlers (issue #28 PR B).
//!
//! Mirrors the shape of the groups + custom-formats settings
//! handlers: form-driven upsert + delete that redirect back to
//! the tab. The "test connection" path lands in a follow-up
//! commit since it needs the full search-pipeline integration to
//! be useful.

use axum::{
    Form,
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
    };

    let result = match form.id {
        Some(id) => update(&state.db, id, payload).await.map(|_| id),
        None => insert(&state.db, payload).await,
    };

    match result {
        Ok(id) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!(
                    "Indexer {}: {} ({})",
                    if form.id.is_some() {
                        "updated"
                    } else {
                        "created"
                    },
                    name,
                    kind,
                ),
                &format!("id={id}, priority={priority}"),
            )
            .await;
            // PR #107 review fix #4: rebuild the IndexerCache so
            // the next search picks up the new/edited row without
            // a process restart.
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            Redirect::to("/settings?tab=indexers&msg=Saved").into_response()
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
    description = "Removes the indexer row by id. Existing grabbed_torrents rows referencing this indexer keep their indexer_id (NULL FK semantics), so grab history isn't lost.",
    responses(
        (status = 303, description = "Redirect back to the indexers tab"),
    ),
)]
pub async fn settings_indexers_delete(
    State(state): State<AppState>,
    Form(form): Form<IndexerDeleteForm>,
) -> Redirect {
    // PR #107 round-2 review fix #3: SQLite can't add a real FK via
    // ALTER TABLE, so `grabbed_torrents.indexer_id` is structurally
    // unconstrained. NULL out matching rows explicitly here so the
    // post-delete state matches the migration comment's "ON DELETE
    // SET NULL" semantics. `pending_grabs.indexer_id` (already
    // nullable per its CREATE TABLE) gets the same treatment for
    // consistency.
    let _ = sqlx::query("UPDATE grabbed_torrents SET indexer_id = NULL WHERE indexer_id = ?")
        .bind(form.id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("UPDATE pending_grabs SET indexer_id = NULL WHERE indexer_id = ?")
        .bind(form.id)
        .execute(&state.db)
        .await;
    match delete(&state.db, form.id).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Indexer deleted: id={}", form.id),
                "",
            )
            .await;
            // PR #107 review fix #4: same cache refresh as upsert.
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Indexer delete failed",
                &e.to_string(),
            )
            .await;
        }
    }
    Redirect::to("/settings?tab=indexers")
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
}
