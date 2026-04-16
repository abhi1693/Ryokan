//! In-memory progress streaming for long-running user-triggered tasks
//! (currently: manual auto-search).
//!
//! ## How it works
//! - The frontend mints an opaque `progress_id` string and sends it as a
//!   query param when triggering a long-running endpoint.
//! - The handler calls [`ProgressRegistry::register`] to bind that id to
//!   a fresh event buffer, then spawns the work inside an
//!   [`EMITTER`] task-local so any code on the call stack can call
//!   [`emit`] without threading a handle through every signature.
//! - The frontend simultaneously polls `GET /api/progress/{id}?since={n}`
//!   to drain newly-buffered events into a sticky toast.
//! - When the work calls [`emit`] with `terminal: true` (or the handler
//!   finishes), the job is marked finished and the cleanup sweep drops
//!   it after a grace period so the next poll can still surface the
//!   final state.
//!
//! ## Why polling, not SSE
//! SSE would need a `tokio-stream` (or futures-util) dep to bridge a
//! broadcast receiver into an axum stream. Polling is one extra request
//! per ~500ms of an auto-search — well under any rate that matters here,
//! and the implementation has no extra deps.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub type JobId = String;

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct ProgressEvent {
    /// Stable stage identifier (e.g. `"search"`, `"score"`, `"grab"`,
    /// `"done"`, `"error"`). Frontend can switch on this for icon swaps.
    pub stage: String,
    /// Toast accent: `"info"`, `"success"`, `"warn"`, `"error"`.
    pub kind: String,
    /// Replaces the toast title in place.
    pub title: String,
    /// Replaces the toast body in place. `None` clears it.
    pub body: Option<String>,
    /// Final event for this job. After consuming it, the frontend stops
    /// polling and the toast becomes user-dismissable (or auto-dismisses
    /// after a short delay for success cases).
    pub terminal: bool,
}

#[derive(Default)]
struct ProgressJob {
    events: Vec<ProgressEvent>,
    finished_at: Option<Instant>,
}

/// Snapshot of a job's buffered events past the caller's cursor.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ProgressPoll {
    pub events: Vec<ProgressEvent>,
    pub next_cursor: usize,
    pub terminal: bool,
}

/// Process-wide registry of in-flight jobs. Cheap to clone — the inner
/// state is a single `Arc<Mutex<HashMap<...>>>`.
#[derive(Clone, Default)]
pub struct ProgressRegistry {
    jobs: Arc<Mutex<HashMap<JobId, ProgressJob>>>,
}

/// Cap on the size of a client-supplied `progress_id`. Long enough for a
/// timestamp + random suffix from the JS side; short enough that an
/// abusive client can't blow up the registry's memory footprint by
/// minting megabyte-long ids.
pub const PROGRESS_ID_MAX_LEN: usize = 64;

impl ProgressRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `id` to a fresh event buffer and return an emit handle.
    /// Re-registering an existing id is a no-op so a flaky client retry
    /// can't corrupt an in-flight job's history.
    pub async fn register(&self, id: JobId) -> ProgressHandle {
        let mut jobs = self.jobs.lock().await;
        jobs.entry(id.clone()).or_default();
        ProgressHandle {
            id,
            registry: self.clone(),
        }
    }

    /// Append `event` to `id`'s buffer. Stamps `finished_at` the first
    /// time a terminal event arrives so the cleanup sweep can drop the
    /// job after its grace period.
    async fn emit(&self, id: &str, event: ProgressEvent) {
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get_mut(id) {
            let terminal = event.terminal;
            job.events.push(event);
            if terminal && job.finished_at.is_none() {
                job.finished_at = Some(Instant::now());
            }
        }
    }

    /// Drain events past `cursor`. Returns `None` if the job has been
    /// swept (or was never registered) so the frontend can stop polling.
    pub async fn poll(&self, id: &str, cursor: usize) -> Option<ProgressPoll> {
        let jobs = self.jobs.lock().await;
        let job = jobs.get(id)?;
        let events = job.events.iter().skip(cursor).cloned().collect::<Vec<_>>();
        let next_cursor = job.events.len();
        let terminal = job.finished_at.is_some();
        Some(ProgressPoll {
            events,
            next_cursor,
            terminal,
        })
    }

    /// Drop jobs whose terminal event landed more than `grace` ago.
    /// Drives the background cleanup task in `main.rs`.
    pub async fn sweep(&self, grace: Duration) {
        let now = Instant::now();
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|_, job| match job.finished_at {
            Some(t) => now.duration_since(t) < grace,
            None => true,
        });
    }
}

/// Per-job emit handle. The auto-search task captures one of these in
/// its [`EMITTER`] task-local so deep callees can [`emit`] without
/// passing the handle down.
#[derive(Clone)]
pub struct ProgressHandle {
    pub id: JobId,
    pub registry: ProgressRegistry,
}

impl ProgressHandle {
    /// Direct emit. Use this from the handler task (which doesn't have
    /// the task-local set) before spawning the worker.
    pub async fn emit(
        &self,
        stage: &str,
        kind: &str,
        title: impl Into<String>,
        body: Option<String>,
        terminal: bool,
    ) {
        let event = ProgressEvent {
            stage: stage.to_string(),
            kind: kind.to_string(),
            title: title.into(),
            body,
            terminal,
        };
        self.registry.emit(&self.id, event).await;
    }
}

tokio::task_local! {
    /// Set by [`scope`] for the duration of a tracked task. Code outside
    /// any tracked scope sees this as unset and [`emit`] becomes a no-op.
    pub static EMITTER: ProgressHandle;
}

/// Run `fut` with `handle` bound as the current task's emitter. Sub-`spawn`s
/// don't inherit task-locals, so the emitter only fires for code that runs
/// directly inside this future (which is what we want — fire-and-forget
/// post-grab work like the sibling auto-expand should not fight the
/// user-facing toast for the final state).
pub async fn scope<F: std::future::Future>(handle: ProgressHandle, fut: F) -> F::Output {
    EMITTER.scope(handle, fut).await
}

/// Run `fut` inside a [`scope`] when `handle` is `Some`, otherwise just
/// await it. This is the convenience the trigger handlers reach for:
/// progress reporting is opt-in via a frontend-supplied id, so the same
/// code path serves both progress-tracked and untracked invocations.
pub async fn run_with_progress<F: std::future::Future>(
    handle: Option<ProgressHandle>,
    fut: F,
) -> F::Output {
    match handle {
        Some(h) => scope(h, fut).await,
        None => fut.await,
    }
}

/// Validate and trim a client-supplied progress id. Returns `None` for
/// empty strings and rejects ids that exceed [`PROGRESS_ID_MAX_LEN`] —
/// the registry would happily store gigabyte-long keys otherwise.
pub fn sanitize_progress_id(id: Option<&str>) -> Option<String> {
    let id = id?.trim();
    if id.is_empty() || id.len() > PROGRESS_ID_MAX_LEN {
        return None;
    }
    Some(id.to_string())
}

/// Emit a progress event in the current task scope. No-op if no emitter
/// is bound — every callee can safely call this without first checking
/// whether it's running under a user-visible trigger or a background sweep.
pub async fn emit(
    stage: &str,
    kind: &str,
    title: impl Into<String>,
    body: Option<String>,
    terminal: bool,
) {
    let event = ProgressEvent {
        stage: stage.to_string(),
        kind: kind.to_string(),
        title: title.into(),
        body,
        terminal,
    };
    if let Ok(handle) = EMITTER.try_with(|h| h.clone()) {
        handle.registry.emit(&handle.id, event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poll_returns_buffered_events_past_cursor() {
        let registry = ProgressRegistry::new();
        let handle = registry.register("job-1".into()).await;
        scope(handle, async {
            emit("search", "info", "Searching", None, false).await;
            emit("done", "success", "Done", None, true).await;
        })
        .await;

        let first = registry.poll("job-1", 0).await.unwrap();
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.next_cursor, 2);
        assert!(first.terminal);

        // Subsequent poll past the cursor returns nothing new but still
        // reports terminal so the frontend knows it's safe to stop.
        let second = registry.poll("job-1", first.next_cursor).await.unwrap();
        assert!(second.events.is_empty());
        assert!(second.terminal);
    }

    #[tokio::test]
    async fn emit_outside_scope_is_a_no_op() {
        // No handle bound — must not panic, must not allocate a job.
        emit("search", "info", "no scope", None, false).await;
        let registry = ProgressRegistry::new();
        // Polling a never-registered id returns None.
        assert!(registry.poll("ghost", 0).await.is_none());
    }

    #[tokio::test]
    async fn sweep_drops_finished_jobs_past_grace() {
        let registry = ProgressRegistry::new();
        let handle = registry.register("job-2".into()).await;
        scope(handle, async {
            emit("done", "success", "Done", None, true).await;
        })
        .await;
        // Zero grace → finished job evicted immediately.
        registry.sweep(Duration::from_secs(0)).await;
        assert!(registry.poll("job-2", 0).await.is_none());
    }

    #[tokio::test]
    async fn re_register_is_idempotent() {
        let registry = ProgressRegistry::new();
        let h1 = registry.register("job-3".into()).await;
        scope(h1.clone(), async {
            emit("search", "info", "first", None, false).await;
        })
        .await;
        // Second register should not wipe the existing buffer.
        let _h2 = registry.register("job-3".into()).await;
        let poll = registry.poll("job-3", 0).await.unwrap();
        assert_eq!(poll.events.len(), 1);
    }
}
