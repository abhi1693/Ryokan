//! Interactive file-picker endpoints (issue #83).
//!
//! The five endpoints here implement the modal lifecycle documented
//! in `models::pending_grabs` and the plan doc at
//! `/home/john/Documents/ryokan-roadmap/issue-83-interactive-file-picker-plan.md`.
//!
//! | Method | Path                               | Purpose                              |
//! |--------|------------------------------------|--------------------------------------|
//! | POST   | `/api/grab/preview`                | Add torrent paused, return preview_id |
//! | GET    | `/api/grab/preview/{preview_id}`   | Poll for file-list readiness         |
//! | POST   | `/api/grab/heartbeat/{preview_id}` | Modal keepalive (~30s cadence)       |
//! | POST   | `/api/grab/confirm`                | Apply user's selections + resume     |
//! | POST   | `/api/grab/cancel`                 | Internal/error-path delete           |
//!
//! The preview POST is non-blocking: it writes the `pending_grabs` row
//! with an empty `file_list_json` and spawns a background task that
//! calls `add_torrent_paused` then `get_files`, writing the result
//! back to the row via `set_file_list`. The modal sees `status:
//! fetching_metadata` on GET until the spawned task completes, then
//! `status: ready` with the file list. That asymmetric shape (fast
//! POST + polled GET) avoids holding a request handler thread for
//! the full metadata-fetch budget while keeping the API surface
//! straightforward — no long-poll, no SSE, no WebSocket.
//!
//! Routes are mounted in `main.rs`'s `protected_routes` block and
//! sit behind the cookie-auth + CSRF layer like every other
//! browser-facing endpoint. Curl-test flow: authenticate to get a
//! session cookie, then `curl -b cookies.txt -X POST .../api/grab/preview ...`.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::pending_grabs;
use crate::services::download_client::{self, AddOutcome};

/// POST body for `/api/grab/preview`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GrabPreviewForm {
    /// Magnet URI or `http(s)://…/*.torrent` URL identifying the
    /// release. Required — used verbatim for the underlying
    /// `DownloadClient::add_torrent_paused` call.
    pub url: String,
    /// v1 info-hash (lowercase hex). Required for qBit-style paused-
    /// add workarounds and for the same-hash dedup check.
    pub info_hash: String,
    /// Target series id. Optional — present when the user triggered
    /// Grab from a specific series-page context. Kept nullable
    /// because a future bare-magnet grab flow may precede series
    /// selection.
    #[serde(default)]
    pub series_id: Option<i64>,
    /// Opaque JSON blob the modal renders in the header before the
    /// file list arrives — typically a serialized `SearchResult`
    /// shape (title, size, seeders, group). Stored verbatim in
    /// `pending_grabs.release_metadata_json` and echoed back on the
    /// GET preview endpoint.
    #[serde(default)]
    pub release_metadata: serde_json::Value,
}

/// POST `/api/grab/preview` response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrabPreviewCreated {
    pub preview_id: String,
    /// Always `"fetching_metadata"` on creation — the modal polls
    /// the GET endpoint until the file list arrives.
    pub status: String,
}

/// GET `/api/grab/preview/{id}` response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrabPreviewStatus {
    pub preview_id: String,
    /// One of `"fetching_metadata"` (file list not yet populated),
    /// `"ready"` (file_list is present and the user can pick), or
    /// `"error"` (metadata fetch failed — modal should show the
    /// retry/defaults dialog).
    pub status: String,
    /// Echoed-back release metadata so the modal can render the
    /// header without re-querying the search endpoint.
    pub release_metadata: serde_json::Value,
    /// File list, only populated when `status == "ready"`. Each entry
    /// carries the torrent-internal file path and size in bytes so
    /// the modal can render per-file sizes and a running total.
    #[serde(default)]
    pub file_list: Vec<PreviewFile>,
    /// Human-readable error message, only populated when
    /// `status == "error"`. Modal uses this verbatim in the
    /// retry/defaults dialog.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PreviewFile {
    pub name: String,
    pub size: i64,
}

/// POST body for `/api/grab/confirm`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GrabConfirmForm {
    pub preview_id: String,
    /// Indices into the preview's `file_list` the user kept checked.
    /// Files NOT in this list will be marked `wanted=false` on the
    /// underlying torrent; files IN this list are marked
    /// `wanted=true` (matters on qBit where `add_torrent_paused`
    /// leaves every file at priority 0 and confirmation is what
    /// flips the selection back on).
    pub wanted_indices: Vec<usize>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrabConfirmResult {
    pub ok: bool,
    /// Error messages for any `set_file_wanted` calls that didn't
    /// land. The grab still commits on partial failure (per plan
    /// decision #10 — best-effort + surface failures); failed files
    /// are left at default priority and the modal can warn the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_priority_errors: Vec<String>,
    /// Error message from the final `resume` call, if any. Separate
    /// from the per-file priority errors so the modal can distinguish
    /// "some priorities didn't apply" (recoverable via client UI)
    /// from "torrent may still be paused" (which matters because
    /// the user expected the grab to start downloading).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_error: Option<String>,
}

/// POST body for `/api/grab/cancel`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GrabCancelForm {
    pub preview_id: String,
}

fn generate_preview_id() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

async fn require_download_client(
    state: &AppState,
) -> Result<
    std::sync::Arc<dyn crate::services::download_client::DownloadClient>,
    (StatusCode, String),
> {
    let guard = state.download_client.read().await;
    guard
        .as_ref()
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Download client not configured".to_string(),
        ))
        .cloned()
}

#[utoipa::path(
    post,
    path = "/api/grab/preview",
    tag = "Grab",
    summary = "Open a pending grab preview (interactive file picker)",
    description = "Adds the torrent in a paused state and returns a \
        preview_id the modal uses to poll for the file list. May block \
        up to ~10s on qBittorrent while it waits for metadata before \
        returning (qBit 5.x can't publish files while stopped, so the \
        AddOutcome — needed to decide whether to store we_added_torrent=true \
        — must be resolved synchronously). Subsequent metadata waiting for \
        the remaining budget runs in a background task; the modal polls \
        GET /api/grab/preview/{id} for readiness.",
    request_body = GrabPreviewForm,
    responses(
        (status = 200, description = "Preview created; poll the GET endpoint for the file list", body = GrabPreviewCreated),
        (status = 400, description = "Missing url/info_hash or download client not configured"),
        (status = 500, description = "Torrent add failed"),
    ),
)]
pub async fn grab_preview(
    State(state): State<AppState>,
    Json(form): Json<GrabPreviewForm>,
) -> Result<Json<GrabPreviewCreated>, (StatusCode, String)> {
    if form.url.trim().is_empty() || form.info_hash.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "url and info_hash are required".to_string(),
        ));
    }

    // TODO (PR B): call `pending_grabs::get_by_hash` here as a pre-
    // flight dedup check. Current behavior on the Tab-1-Added /
    // Tab-2-AlreadyPresent race: if Tab 1 cancels, its
    // `we_added_torrent=true` lets it nuke the torrent, and Tab 2's
    // still-open modal breaks on confirm because the torrent is gone.
    // The dedup check turns Tab 2 into "reuse the existing
    // preview_id" (or a 409 "modal already open in another tab") and
    // sidesteps the race entirely. Deferred to PR B alongside the
    // same-hash-already-in-client flow (plan decision #6) so both
    // dedup surfaces land together.

    let client = require_download_client(&state).await?;

    let info_hash = form.info_hash.to_ascii_lowercase();
    let client_kind = client.sonarr_impl_name().to_string();
    let metadata_json = form.release_metadata.to_string();

    // Run the paused-add synchronously so we know whether the
    // torrent was added fresh or was pre-existing. `we_added_torrent`
    // gates the destructive delete in `grab_cancel` — we can't make
    // that decision after the fact because AddOutcome isn't stored
    // on the row. Blocking the HTTP handler here is acceptable
    // because non-qBit impls return immediately (they don't wait
    // for metadata in `add_torrent_paused`), and qBit's in-impl
    // wait is bounded to 10s.
    let outcome = match client.add_torrent_paused(&form.url, &info_hash).await {
        Ok(v) => v,
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let we_added = matches!(outcome, AddOutcome::Added);

    let preview_id = generate_preview_id();
    pending_grabs::create(
        &state.db,
        &preview_id,
        &info_hash,
        &client_kind,
        None,
        form.series_id,
        &metadata_json,
        we_added,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Spawn the metadata-wait + file-list-persist. qBit already
    // blocked up to 10s inside add_torrent_paused above and may have
    // the file list ready immediately; Deluge/Transmission/rTorrent
    // return before metadata arrives (add_paused=true is non-blocking
    // by design), so wait_for_files does the cross-client bounded
    // poll. Handler returns preview_id immediately either way.
    let db = state.db.clone();
    let hash = info_hash.clone();
    let preview_id_for_task = preview_id.clone();
    tokio::spawn(async move {
        let files = match download_client::wait_for_files(
            client.as_ref(),
            &hash,
            std::time::Duration::from_secs(METADATA_WAIT_SECS),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("metadata fetch failed: {}", e);
                tracing::warn!(
                    target: "ryokan::handlers::grab",
                    preview_id = %preview_id_for_task,
                    error = %e,
                    "wait_for_files failed; modal will flip to status=error"
                );
                if let Err(db_err) = pending_grabs::set_error(&db, &preview_id_for_task, &msg).await
                {
                    tracing::error!(
                        target: "ryokan::handlers::grab",
                        preview_id = %preview_id_for_task,
                        error = %db_err,
                        "set_error failed"
                    );
                }
                return;
            }
        };
        let preview_files: Vec<PreviewFile> = files
            .into_iter()
            .map(|f| PreviewFile {
                name: f.name,
                size: f.size,
            })
            .collect();
        let json = match serde_json::to_string(&preview_files) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("serialize file list failed: {}", e);
                tracing::error!(
                    target: "ryokan::handlers::grab",
                    preview_id = %preview_id_for_task,
                    error = %e,
                    "serialize file list failed"
                );
                let _ = pending_grabs::set_error(&db, &preview_id_for_task, &msg).await;
                return;
            }
        };
        if let Err(e) = pending_grabs::set_file_list(&db, &preview_id_for_task, &json).await {
            tracing::error!(
                target: "ryokan::handlers::grab",
                preview_id = %preview_id_for_task,
                error = %e,
                "set_file_list failed"
            );
            let _ = pending_grabs::set_error(&db, &preview_id_for_task, &e).await;
        }
    });

    Ok(Json(GrabPreviewCreated {
        preview_id,
        status: "fetching_metadata".to_string(),
    }))
}

/// Cross-client metadata-fetch budget for the spawned preview task.
/// Set to 2× qBit's in-impl 10s budget so:
///
/// * On qBit, the in-impl wait succeeds first (typical case), the
///   outer poll sees a populated file list immediately, and no time
///   is spent retrying what already succeeded.
/// * On Deluge / Transmission / rTorrent, the outer poll owns the
///   wait. 20s covers cold-DHT magnet bootstraps for the overwhelming
///   majority of magnet links. Bare magnets that take longer surface
///   as `status: error` on the next GET poll, flipping the modal to
///   the retry/defaults dialog (plan decision #1).
///
/// Changing either this value or qBit's in-impl budget: they should
/// be tuned as a pair — `OUTER ≥ qBit_inner`. If the inner budget is
/// shortened, qBit's time-at-default-priorities window shrinks with
/// it (issue #5 from the review), but the outer must stay larger or
/// the cross-client poll gives up before qBit would.
const METADATA_WAIT_SECS: u64 = 20;

#[utoipa::path(
    get,
    path = "/api/grab/preview/{preview_id}",
    tag = "Grab",
    summary = "Poll a pending grab's file-list readiness",
    description = "Returns `status: fetching_metadata` until the background \
        metadata fetch completes; then `status: ready` with the file list. \
        Returns 404 after the preview has been confirmed, cancelled, or \
        auto-committed by the sweep — modal should show \"already committed\".",
    params(
        ("preview_id" = String, Path, description = "Opaque id from POST /api/grab/preview"),
    ),
    responses(
        (status = 200, description = "Current status", body = GrabPreviewStatus),
        (status = 404, description = "Preview not found (committed, cancelled, or swept)"),
    ),
)]
pub async fn grab_preview_status(
    State(state): State<AppState>,
    Path(preview_id): Path<String>,
) -> Result<Json<GrabPreviewStatus>, (StatusCode, String)> {
    let row = pending_grabs::get(&state.db, &preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((StatusCode::NOT_FOUND, "preview not found".to_string()))?;

    let release_metadata: serde_json::Value = if row.release_metadata_json.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&row.release_metadata_json).unwrap_or(serde_json::Value::Null)
    };

    // Error takes precedence over fetching/ready. If the spawned
    // metadata-fetch task marked an error, surface it immediately so
    // the modal can offer retry/defaults without waiting for the TTL
    // sweep to drop the row.
    if !row.error_message.is_empty() {
        return Ok(Json(GrabPreviewStatus {
            preview_id,
            status: "error".to_string(),
            release_metadata,
            file_list: Vec::new(),
            error: row.error_message,
        }));
    }

    if row.file_list_json.is_empty() {
        return Ok(Json(GrabPreviewStatus {
            preview_id,
            status: "fetching_metadata".to_string(),
            release_metadata,
            file_list: Vec::new(),
            error: String::new(),
        }));
    }

    let file_list: Vec<PreviewFile> = serde_json::from_str(&row.file_list_json).unwrap_or_default();
    Ok(Json(GrabPreviewStatus {
        preview_id,
        status: "ready".to_string(),
        release_metadata,
        file_list,
        error: String::new(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/grab/heartbeat/{preview_id}",
    tag = "Grab",
    summary = "Keepalive for an open file-picker modal",
    description = "Bumps the pending grab's heartbeat timestamp so the \
        TTL sweep doesn't treat it as abandoned. Modal should call this \
        every ~30s while open. Returns 404 when the preview has already \
        been swept — modal should stop polling and show \"already committed\".",
    params(
        ("preview_id" = String, Path, description = "Opaque id from POST /api/grab/preview"),
    ),
    responses(
        (status = 200, description = "Heartbeat recorded", body = serde_json::Value),
        (status = 404, description = "Preview not found"),
    ),
)]
pub async fn grab_heartbeat(
    State(state): State<AppState>,
    Path(preview_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let bumped = pending_grabs::bump_heartbeat(&state.db, &preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !bumped {
        return Err((StatusCode::NOT_FOUND, "preview not found".to_string()));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/grab/confirm",
    tag = "Grab",
    summary = "Confirm file selections and commit the grab",
    description = "Applies wanted/unwanted priorities per the user's \
        selection, resumes the torrent, and deletes the pending_grabs \
        row. Best-effort on per-file priority writes: partial failures \
        (qBit down mid-apply, a priority write rejected) leave the \
        failed files at default priority rather than rolling back \
        the whole grab. Returns 404 if the preview was already \
        committed or swept. \
        \
        NOTE: unlike grab_cancel, confirm does NOT gate on \
        we_added_torrent — the user consciously submitted their \
        selection, so overwriting a pre-existing torrent's priorities \
        is the intended behavior (plan decision #6's \"show current \
        priorities + allow re-apply\" same-hash flow). Prior \
        partial-downloaded files remain on disk; qBit/rTorrent don't \
        delete previously-downloaded data when a file flips to skip, \
        so the data-risk on overwrite is low.",
    request_body = GrabConfirmForm,
    responses(
        (status = 200, description = "Grab committed", body = GrabConfirmResult),
        (status = 400, description = "preview_id missing or file list not yet populated"),
        (status = 404, description = "Preview not found"),
        (status = 500, description = "Download client error"),
    ),
)]
pub async fn grab_confirm(
    State(state): State<AppState>,
    Json(form): Json<GrabConfirmForm>,
) -> Result<Json<GrabConfirmResult>, (StatusCode, String)> {
    if form.preview_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "preview_id required".into()));
    }

    let row = pending_grabs::get(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((StatusCode::NOT_FOUND, "preview not found".to_string()))?;

    if row.file_list_json.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "file list not yet populated; poll GET first".to_string(),
        ));
    }

    let files: Vec<PreviewFile> = serde_json::from_str(&row.file_list_json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("stored file list corrupt: {}", e),
        )
    })?;
    let total = files.len();

    let client = require_download_client(&state).await?;

    // Compute the wanted / unwanted partitions. Any index outside
    // [0, total) in `wanted_indices` is silently ignored — the modal
    // is expected to send valid indices but we defend against a
    // racy modal state where the file list changed mid-flight.
    let wanted: Vec<usize> = form
        .wanted_indices
        .into_iter()
        .filter(|&i| i < total)
        .collect();
    let wanted_set: std::collections::HashSet<usize> = wanted.iter().copied().collect();
    let unwanted: Vec<usize> = (0..total).filter(|i| !wanted_set.contains(i)).collect();

    let mut file_priority_errors: Vec<String> = Vec::new();
    if !unwanted.is_empty()
        && let Err(e) = client
            .set_file_wanted(&row.info_hash, &unwanted, false)
            .await
    {
        file_priority_errors.push(format!("mark unwanted: {}", e));
    }
    if !wanted.is_empty()
        && let Err(e) = client.set_file_wanted(&row.info_hash, &wanted, true).await
    {
        file_priority_errors.push(format!("mark wanted: {}", e));
    }
    // Resume starts downloading on Deluge / Transmission / rTorrent
    // (they were added paused). On qBit the torrent is already
    // running — resume is idempotent.
    let resume_error = client.resume(&row.info_hash).await.err();

    // Drop the pending row — the user has committed, so the sweep
    // should never revisit this preview_id. Grab-row write happens
    // separately via the existing post-processing pipeline (PR C
    // scope). For now the caller takes the confirmation as "the
    // torrent is running with your selections applied."
    pending_grabs::delete(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(GrabConfirmResult {
        ok: file_priority_errors.is_empty() && resume_error.is_none(),
        file_priority_errors,
        resume_error,
    }))
}

#[utoipa::path(
    post,
    path = "/api/grab/cancel",
    tag = "Grab",
    summary = "Cancel a pending grab (internal/error path)",
    description = "Deletes the torrent from the download client AND drops \
        the pending_grabs row. Per plan decision #4 this endpoint is NOT \
        called by the modal's normal close flow (which falls through to \
        auto-commit via the sweep); it's reserved for error recovery and \
        the blocklisted-release keep-blocked path.",
    request_body = GrabCancelForm,
    responses(
        (status = 200, description = "Cancelled", body = serde_json::Value),
        (status = 404, description = "Preview not found"),
        (status = 500, description = "Download client error"),
    ),
)]
pub async fn grab_cancel(
    State(state): State<AppState>,
    Json(form): Json<GrabCancelForm>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let row = pending_grabs::get(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((StatusCode::NOT_FOUND, "preview not found".to_string()))?;

    let client = require_download_client(&state).await?;

    // Only delete the torrent if THIS preview added it fresh. If the
    // torrent was already in the client at add time (AlreadyPresent),
    // the user may have partial-downloaded it from a prior grab or
    // added it manually outside Ryokan — cancelling the preview
    // doesn't give us permission to delete data we didn't create.
    // The pending_grabs row is still dropped either way so the modal-
    // state doesn't linger.
    if row.we_added_torrent
        && let Err(e) = client.delete(&row.info_hash, true).await
    {
        tracing::warn!(
            target: "ryokan::handlers::grab",
            preview_id = %form.preview_id,
            hash = %row.info_hash,
            error = %e,
            "download client delete failed during cancel; proceeding with pending-row cleanup"
        );
    }

    pending_grabs::delete(&state.db, &form.preview_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    // Unit tests against the handler functions directly (not via a
    // live Axum router) so we can assert on concrete response types
    // without a full HTTP round-trip. Download client interactions
    // are minimized — these tests mostly exercise the database and
    // serialization paths. End-to-end client behavior gets its
    // coverage from the `live_smoke*` tests on each
    // `DownloadClient` impl.

    #[tokio::test]
    async fn preview_status_404_when_missing() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_preview_status(State(state), Path("nope".to_string())).await;
        assert!(matches!(res, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn preview_status_fetching_then_ready() {
        let db = in_memory_pool().await;
        pending_grabs::create(
            &db,
            "pid-1",
            "abc",
            "qbittorrent",
            None,
            None,
            "{\"title\":\"t\"}",
            true,
        )
        .await
        .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let status = grab_preview_status(State(state.clone()), Path("pid-1".to_string()))
            .await
            .unwrap();
        assert_eq!(status.status, "fetching_metadata");
        assert!(status.file_list.is_empty());

        // Populate file list → status flips to ready.
        let files = vec![PreviewFile {
            name: "episode_1.mkv".into(),
            size: 8192,
        }];
        pending_grabs::set_file_list(&db, "pid-1", &serde_json::to_string(&files).unwrap())
            .await
            .unwrap();

        let status = grab_preview_status(State(state), Path("pid-1".to_string()))
            .await
            .unwrap();
        assert_eq!(status.status, "ready");
        assert_eq!(status.file_list.len(), 1);
        assert_eq!(status.file_list[0].name, "episode_1.mkv");
    }

    #[tokio::test]
    async fn heartbeat_200_when_present_404_when_gone() {
        let db = in_memory_pool().await;
        pending_grabs::create(&db, "pid-1", "abc", "qbittorrent", None, None, "{}", true)
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let ok = grab_heartbeat(State(state.clone()), Path("pid-1".to_string())).await;
        assert!(ok.is_ok());

        pending_grabs::delete(&db, "pid-1").await.unwrap();
        let missing = grab_heartbeat(State(state), Path("pid-1".to_string())).await;
        assert!(matches!(missing, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn confirm_rejects_empty_preview_id() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "".to_string(),
                wanted_indices: vec![0],
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn confirm_404_when_missing() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "nope".to_string(),
                wanted_indices: vec![],
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn confirm_400_when_file_list_empty() {
        let db = in_memory_pool().await;
        pending_grabs::create(&db, "pid-1", "abc", "qbittorrent", None, None, "{}", true)
            .await
            .unwrap();
        let state = build_test_app_state(db, None);
        let res = grab_confirm(
            State(state),
            Json(GrabConfirmForm {
                preview_id: "pid-1".to_string(),
                wanted_indices: vec![],
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn preview_rejects_empty_url_or_hash() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_preview(
            State(state.clone()),
            Json(GrabPreviewForm {
                url: "".into(),
                info_hash: "abc".into(),
                series_id: None,
                release_metadata: serde_json::Value::Null,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));

        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: "magnet:?xt=urn:btih:abc".into(),
                info_hash: "".into(),
                series_id: None,
                release_metadata: serde_json::Value::Null,
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn preview_400_when_client_not_configured() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let res = grab_preview(
            State(state),
            Json(GrabPreviewForm {
                url: "magnet:?xt=urn:btih:abc".into(),
                info_hash: "abcdef0123".into(),
                series_id: Some(42),
                release_metadata: serde_json::json!({"title": "test"}),
            }),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }
}
