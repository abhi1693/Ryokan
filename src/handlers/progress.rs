use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;

use crate::services::progress::ProgressPoll;
use crate::AppState;

#[derive(Deserialize)]
pub struct PollQuery {
    /// Cursor returned by the previous poll. Omit on the first call to
    /// drain the buffer from the start.
    pub since: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/api/progress/{job_id}",
    tag = "System",
    summary = "Poll progress events for a tracked job",
    description = "Returns events appended to the job since the caller's cursor. The frontend should poll this on a short interval (e.g. 500ms) while a sticky toast is open, and stop polling once `terminal: true` is observed.",
    params(
        ("job_id" = String, Path, description = "Client-supplied opaque progress id."),
        ("since" = Option<usize>, Query, description = "Cursor from the previous poll. Omit to drain from the start."),
    ),
    responses(
        (status = 200, description = "Buffered events past the cursor", body = ProgressPoll),
        (status = 404, description = "Unknown or already-swept job id"),
    ),
)]
pub async fn poll_progress(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Query(q): Query<PollQuery>,
) -> Result<Json<ProgressPoll>, (StatusCode, &'static str)> {
    state
        .progress
        .poll(&job_id, q.since.unwrap_or(0))
        .await
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "unknown progress job"))
}
