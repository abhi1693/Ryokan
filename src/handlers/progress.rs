use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::Stream;
use serde::Deserialize;

use crate::AppState;
use crate::services::progress::{ProgressEvent, ProgressPoll};

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

/// How often the SSE stream re-checks the registry for new events when
/// no new events have arrived since the last poll. Same number as the
/// frontend polling interval used by `ryokanProgressToast` (500 ms in
/// `static/js/base.js`) so the perceived responsiveness matches; users
/// migrating from the polling client to the SSE one shouldn't notice.
///
/// Server-side polling instead of pub-sub via `Notify` is a deliberate
/// simplicity trade-off — `ProgressRegistry`'s emit path stays
/// unchanged, no broadcast plumbing, and the wakeup latency under
/// 500 ms is below human-perceivable for toast updates. If the latency
/// becomes a concern (e.g. the SSE endpoint gets reused for tighter
/// real-time signals), swap to per-job `tokio::sync::Notify` here.
/// Avoid "optimizing" with exponential backoff without measuring first.
/// A long-running auto-search produces 120 wakeups across a minute of
/// idle, which sounds like a lot but each wakeup is a `tokio::sleep`
/// plus a quick mutex-guarded `Vec::skip(cursor).cloned()` over a
/// typical 0–5-event buffer. Keepalive every `SSE_KEEPALIVE_INTERVAL`
/// is the real proxy-disconnect backstop; the inner-poll cadence is
/// only about user-visible latency on event arrival.
const SSE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on how long the stream waits for new events before sending a
/// keepalive comment. Without this, a long-idle job (slow upstream
/// auto-search) plus a corporate proxy with a 60 s read timeout would
/// silently drop the SSE connection. Axum's `Sse::keep_alive` already
/// emits keepalive comments at a configurable interval; we set it
/// explicitly to 15 s so it always fires under the typical proxy
/// idle timeout (30–60 s) but not so often it spams the wire.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// State threaded through the `unfold`-based SSE generator.
///
/// `queue` carries events that came back in a single `registry.poll()`
/// batch but haven't been yielded yet — the stream yields one item per
/// poll, so a poll that returned 5 events drains across 5 stream
/// iterations. `cursor` tracks how far we've read from the registry's
/// buffer, `terminal_seen` ends the stream once the terminal event has
/// been yielded.
struct SseStreamState {
    cursor: usize,
    queue: VecDeque<ProgressEvent>,
    terminal_seen: bool,
}

#[utoipa::path(
    get,
    path = "/api/progress/{job_id}/stream",
    tag = "System",
    summary = "Stream progress events for a tracked job (SSE)",
    description = "Server-Sent Events variant of `GET /api/progress/{job_id}`. The frontend opens an `EventSource` against this endpoint while a sticky toast is open; the stream emits one `message` event per buffered `ProgressEvent` and closes when the terminal event has been delivered. Unknown job ids return 404 immediately so the frontend can fall through to the polling endpoint.",
    params(
        ("job_id" = String, Path, description = "Client-supplied opaque progress id."),
    ),
    responses(
        (status = 200, description = "SSE stream of `ProgressEvent` JSON payloads"),
        (status = 404, description = "Unknown or already-swept job id"),
    ),
)]
pub async fn stream_progress(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, &'static str)> {
    // 404 the unknown-job case before opening the stream so callers
    // get a synchronous failure to fall back on. After this point the
    // SSE generator handles "job swept mid-stream" by closing.
    if state.progress.poll(&job_id, 0).await.is_none() {
        return Err((StatusCode::NOT_FOUND, "unknown progress job"));
    }

    let registry = state.progress.clone();
    let initial = SseStreamState {
        cursor: 0,
        queue: VecDeque::new(),
        terminal_seen: false,
    };

    let stream = futures_util::stream::unfold(initial, move |mut s| {
        let registry = registry.clone();
        let job_id = job_id.clone();
        async move {
            // End the stream once the terminal event has been yielded
            // and any post-terminal queue items are drained.
            if s.terminal_seen && s.queue.is_empty() {
                return None;
            }
            // Drain queued events one per stream iteration.
            if let Some(ev) = s.queue.pop_front() {
                let json = serde_json::to_string(&ev).unwrap_or_default();
                let event = Event::default().event("progress").data(json);
                if ev.terminal {
                    s.terminal_seen = true;
                }
                return Some((Ok::<_, Infallible>(event), s));
            }
            // Empty queue → poll registry until something arrives or
            // the job ends. We loop on poll-then-sleep so the stream
            // wakes promptly on new emits without blocking the runtime.
            loop {
                let poll = match registry.poll(&job_id, s.cursor).await {
                    Some(p) => p,
                    None => return None, // job swept mid-stream
                };
                s.cursor = poll.next_cursor;
                for ev in poll.events {
                    s.queue.push_back(ev);
                }
                if let Some(ev) = s.queue.pop_front() {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    let event = Event::default().event("progress").data(json);
                    if ev.terminal {
                        s.terminal_seen = true;
                    }
                    return Some((Ok(event), s));
                }
                // Buffer is empty but the registry says terminal was
                // already observed — close the stream cleanly.
                if poll.terminal {
                    return None;
                }
                tokio::time::sleep(SSE_POLL_INTERVAL).await;
            }
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE_INTERVAL)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use crate::services::progress::{emit, scope};
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    /// Build a minimal `AppState` with a fresh progress registry.
    /// Other state fields aren't touched by the SSE handler so we
    /// reuse the test-support builder.
    async fn state_with_progress() -> AppState {
        let pool = crate::test_support::in_memory_pool().await;
        crate::test_support::build_test_app_state(pool, None)
    }

    /// Pre-seed all events into the registry, THEN call the handler.
    /// The stream should drain the buffer and close on the terminal
    /// event without needing the test to keep emitting. This is the
    /// minimum-viable assertion against the wire shape: events arrive,
    /// terminal closes the stream.
    #[tokio::test]
    async fn stream_drains_buffered_events_and_closes_on_terminal() {
        let state = state_with_progress().await;
        let handle = state.progress.register("job-sse".into()).await;
        scope(handle, async {
            emit("search", "info", "Searching", None, false).await;
            emit("score", "info", "Scoring", None, false).await;
            emit("done", "success", "Done", None, true).await;
        })
        .await;

        let resp = stream_progress(State(state.clone()), Path("job-sse".into()))
            .await
            .expect("stream should open for registered job")
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream"),
            "SSE responses must carry text/event-stream content-type"
        );

        // Drain the body. The stream closes on terminal so this
        // returns finite bytes — no timeout needed.
        let bytes = to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("collect body");
        let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");

        // Each event is `event: progress\ndata: <json>\n\n`. Three
        // events plus axum's own framing should produce exactly three
        // `data:` lines whose JSON parses to our event struct.
        let data_lines: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        assert_eq!(
            data_lines.len(),
            3,
            "expected 3 events buffered before terminal; got body: {body}"
        );
        // First and third events' shape — checking title is enough to
        // confirm the JSON payload round-trips. Parse into
        // `serde_json::Value` so the test doesn't force `Deserialize`
        // onto the production `ProgressEvent` struct (it's serialize-
        // only by design — server emits, never consumes).
        let first: serde_json::Value =
            serde_json::from_str(data_lines[0]).expect("first event parses");
        assert_eq!(first["title"], "Searching");
        assert_eq!(first["terminal"], false);
        let last: serde_json::Value =
            serde_json::from_str(data_lines[2]).expect("last event parses");
        assert_eq!(last["title"], "Done");
        assert_eq!(last["terminal"], true);
    }

    /// Unknown job id should 404 synchronously rather than open a
    /// stream that hangs. The frontend uses this synchronous failure
    /// to fall back to the polling endpoint cleanly.
    #[tokio::test]
    async fn stream_returns_404_for_unknown_job() {
        let state = state_with_progress().await;
        let result = stream_progress(State(state), Path("ghost".into())).await;
        let err = result.expect_err("unknown job must error");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }
}
