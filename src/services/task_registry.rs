//! Named-task registry for the supervised background loops.
//!
//! Pre-this PR, every long-running background task in `main.rs` was a
//! free-standing `tokio::spawn` wrapped in [`supervise`] (the
//! exponential-backoff respawn helper). The shape worked fine for ~5
//! tasks but doesn't scale: at 10+ tasks (`rss_sync`,
//! `metadata_refresh`, `cleanup`, `post_processing`,
//! `library_classify`, `upgrade_search`, `anibridge_refresh`,
//! `progress_sweep`, `grab_sweep`, `external_sync`) the operator has
//! no surface to answer "is task X running, when did it last
//! restart, what's its current backoff?" without grepping logs.
//!
//! This registry gives every supervised task a stable named handle
//! with lifecycle state — current status, restart count, last exit
//! cause, current backoff — that the System page can render. It
//! deliberately does NOT introduce new lifecycle hooks (pre-stop,
//! config-changed restart, etc.); those belong in a follow-up if a
//! future task type (e.g. autobrr IRC announce listener) needs them.
//!
//! Shape mirrors the existing `*Cache` types on [`crate::AppState`]:
//! `Arc<RwLock<HashMap<…>>>` so registration and snapshot reads
//! don't contend on the supervise hot path. The supervise loop
//! grabs a per-task `Arc<TaskState>` once at register time and
//! mutates atomics on it; no further locking until the (rare)
//! snapshot read.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::RwLock;

/// Stable status discriminator. Numeric so a hot-path
/// `AtomicU8::store` is cheap and the snapshot path can map to
/// `&'static str` for JSON without a string allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskStatus {
    /// The wrapped future is currently executing.
    Running = 0,
    /// The future returned (cleanly or via panic / join error) and
    /// the supervise loop is sleeping before respawning.
    Backoff = 1,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Backoff => "backoff",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => TaskStatus::Backoff,
            _ => TaskStatus::Running,
        }
    }
}

/// Why the task's last iteration ended. Surfaced on the System page
/// so an operator looking at "task X is in backoff" can see whether
/// the cause was a panic (real bug worth investigating) vs a normal
/// return (unusual but not a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitKind {
    /// Never exited — the task has been running since registration.
    None = 0,
    /// The wrapped future returned `()` from its outer loop. None of
    /// the existing tasks should ever do this (their outer loops are
    /// `loop { … }`), so this kind in the snapshot is a real signal
    /// something is wrong.
    Normal = 1,
    /// `tokio::JoinHandle::await` returned a panic'd join error. The
    /// inner future hit a `.unwrap()` or similar — without supervise
    /// this would silently kill the task forever.
    Panic = 2,
    /// Non-panic join error (cancellation token, runtime drop). Rare
    /// in production; tests can hit this on `tokio::test` timeout.
    JoinError = 3,
}

impl ExitKind {
    fn as_str(self) -> &'static str {
        match self {
            ExitKind::None => "none",
            ExitKind::Normal => "normal",
            ExitKind::Panic => "panic",
            ExitKind::JoinError => "join_error",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => ExitKind::Normal,
            2 => ExitKind::Panic,
            3 => ExitKind::JoinError,
            _ => ExitKind::None,
        }
    }
}

/// Per-task mutable state. Atomics on every field so the supervise
/// hot path doesn't contend with snapshot readers. `Arc`-shared so
/// the supervise closure and the registry's HashMap point at the
/// same instance.
///
/// Snapshot reads are NOT atomic across fields. Writes go through
/// `Ordering::Relaxed` and `mark_started` / `mark_exited` /
/// `mark_backoff` each touch multiple atomics. A reader can interleave
/// at the field-update granularity and observe transient
/// inconsistency — e.g. `status = Backoff` paired with
/// `started_at` already updated for the next iteration, or
/// `current_backoff_ms = 0` paired with `status = Backoff` for a
/// few microseconds. This is acceptable for the UI's polling cadence
/// (the System page re-fetches every few seconds) but worth knowing
/// before reading the snapshot programmatically. Don't alarm-trigger
/// off a single inconsistent read; require the same shape across
/// two consecutive polls if precision matters.
pub struct TaskState {
    pub name: &'static str,
    /// Unix seconds when the most recent iteration started. 0 until
    /// the first start. Updated on every respawn so the snapshot
    /// shows "currently-running iteration started N seconds ago"
    /// rather than "task originally registered at boot."
    started_at: AtomicI64,
    /// Unix seconds of the most recent iteration's exit. 0 means the
    /// task has never exited yet — i.e. either it just registered or
    /// it's been running steadily since.
    last_exit_at: AtomicI64,
    last_exit_kind: AtomicU8,
    /// Total iteration exits since registration. Bumped on every
    /// `mark_exited` call — i.e. once per completed iteration of the
    /// supervise loop. After N exits the task has been started N+1
    /// times (initial start + N restarts), so the count is the
    /// "iterations completed" / "restart count + 1" reading rather
    /// than literal "restart count" — a snapshot taken mid-backoff
    /// shows the count incremented before the next iteration starts.
    /// Monotonic; never reset (healthy-runtime backoff reset only
    /// touches the sleep duration, not the counter).
    exit_count: AtomicU64,
    /// Current sleep duration before the next respawn, in
    /// milliseconds. Reads as 0 while `Running`. Reads as the
    /// backoff value while `Backoff`. Surfaced so the System page
    /// can show "next restart in 47s" rather than just "in
    /// backoff."
    current_backoff_ms: AtomicU64,
    status: AtomicU8,
}

impl TaskState {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            started_at: AtomicI64::new(0),
            last_exit_at: AtomicI64::new(0),
            last_exit_kind: AtomicU8::new(ExitKind::None as u8),
            exit_count: AtomicU64::new(0),
            current_backoff_ms: AtomicU64::new(0),
            status: AtomicU8::new(TaskStatus::Running as u8),
        }
    }

    /// Called from the supervise loop right before each iteration's
    /// `tokio::spawn(make_fut())`. Stamps the start time and flips
    /// the status so a snapshot taken mid-iteration reads
    /// `running`, not the prior `backoff`.
    pub fn mark_started(&self, unix_seconds: i64) {
        self.started_at.store(unix_seconds, Ordering::Relaxed);
        self.current_backoff_ms.store(0, Ordering::Relaxed);
        self.status
            .store(TaskStatus::Running as u8, Ordering::Relaxed);
    }

    /// Called when the iteration's join handle resolves. Records the
    /// exit kind and bumps `exit_count` so the snapshot shows how
    /// many iterations have completed without the supervise loop
    /// having to hold a separate counter.
    pub fn mark_exited(&self, unix_seconds: i64, kind: ExitKind) {
        self.last_exit_at.store(unix_seconds, Ordering::Relaxed);
        self.last_exit_kind.store(kind as u8, Ordering::Relaxed);
        self.exit_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Called as the supervise loop enters its `tokio::time::sleep`
    /// before respawning. Status flips to `backoff` and the snapshot
    /// reports the configured wait so the System page can render a
    /// countdown.
    pub fn mark_backoff(&self, backoff_ms: u64) {
        self.current_backoff_ms.store(backoff_ms, Ordering::Relaxed);
        self.status
            .store(TaskStatus::Backoff as u8, Ordering::Relaxed);
    }
}

/// Lock-shape mirrors `IndexerCache` / `CompiledCfCache`: an outer
/// `RwLock` for the rarely-mutated registration table, with shared
/// `Arc<TaskState>` clones held by each supervise loop so the hot
/// path never touches the lock after registration.
#[derive(Clone, Default)]
pub struct TaskRegistry {
    tasks: Arc<RwLock<HashMap<&'static str, Arc<TaskState>>>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a task and return a handle the supervise loop
    /// retains for the life of the process. Re-registering the same
    /// name returns the existing handle so a hot-reloading test
    /// doesn't end up with two competing entries; in production
    /// this only fires once per task at startup.
    pub async fn register(&self, name: &'static str) -> Arc<TaskState> {
        let mut tasks = self.tasks.write().await;
        if let Some(existing) = tasks.get(name) {
            return existing.clone();
        }
        let state = Arc::new(TaskState::new(name));
        tasks.insert(name, state.clone());
        state
    }

    /// Read-only snapshot for the System page / `/api/system/tasks`.
    /// Sorted by name so the rendered list is stable across
    /// requests.
    pub async fn snapshot(&self) -> Vec<TaskSnapshot> {
        let tasks = self.tasks.read().await;
        let mut out: Vec<TaskSnapshot> = tasks.values().map(snapshot_one).collect();
        out.sort_by_key(|s| s.name);
        out
    }
}

/// Plain-data view for serialization. Captures the atomic-state
/// snapshot at one instant; subsequent mutations on the source
/// `TaskState` don't affect a previously-returned snapshot.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TaskSnapshot {
    pub name: &'static str,
    /// Current execution state: `running` (the wrapped future is
    /// executing) or `backoff` (the future returned and the supervise
    /// loop is sleeping before respawning). Field-level enum
    /// documentation per the OpenAPI reviewer feedback so Swagger UI
    /// shows the valid value set; the underlying `TaskStatus` enum
    /// in the registry stays internal.
    #[schema(example = "running")]
    pub status: &'static str,
    /// Unix seconds when the current (or most recent) iteration
    /// started. 0 means the task hasn't run yet — only possible in
    /// the narrow window between registration and the first
    /// `mark_started` call.
    pub started_at: i64,
    /// Unix seconds when the most recent iteration ended. 0 when the
    /// task has been running steadily since registration. Useful for
    /// "task last restarted N seconds ago" UI labels.
    pub last_exit_at: i64,
    /// Cause of the most recent iteration's exit: `none` (still
    /// running, never exited), `normal` (the wrapped future returned
    /// `()` from its outer loop — anomalous; outer loops are
    /// `loop { … }`), `panic` (the future panicked — supervise
    /// caught it at the join boundary), or `join_error` (non-panic
    /// JoinError — cancellation token, runtime drop). Lets the
    /// System page distinguish a panic'd task in backoff from a
    /// task that exited cleanly.
    #[schema(example = "none")]
    pub last_exit_kind: &'static str,
    /// Total iteration exits since registration. Bumped on every
    /// completed iteration of the supervise loop. Note this is "exits"
    /// not "restarts": the count reflects the number of times the
    /// wrapped future has returned (cleanly, panic'd, or join-errored),
    /// so a snapshot taken mid-backoff shows the count already
    /// incremented before the next iteration's restart fires. After
    /// N exits the task has been spawned N+1 times.
    pub exit_count: u64,
    /// Configured wait before the next respawn. 0 while running.
    pub current_backoff_ms: u64,
}

fn snapshot_one(state: &Arc<TaskState>) -> TaskSnapshot {
    TaskSnapshot {
        name: state.name,
        status: TaskStatus::from_u8(state.status.load(Ordering::Relaxed)).as_str(),
        started_at: state.started_at.load(Ordering::Relaxed),
        last_exit_at: state.last_exit_at.load(Ordering::Relaxed),
        last_exit_kind: ExitKind::from_u8(state.last_exit_kind.load(Ordering::Relaxed)).as_str(),
        exit_count: state.exit_count.load(Ordering::Relaxed),
        current_backoff_ms: state.current_backoff_ms.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn register_returns_same_handle_for_repeated_names() {
        let reg = TaskRegistry::new();
        let a = reg.register("rss_sync").await;
        let b = reg.register("rss_sync").await;
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn lifecycle_transitions_visible_in_snapshot() {
        let reg = TaskRegistry::new();
        let state = reg.register("test_task").await;
        let t0 = now();
        state.mark_started(t0);

        let snap = reg.snapshot().await;
        let task = snap.iter().find(|s| s.name == "test_task").unwrap();
        assert_eq!(task.status, "running");
        assert_eq!(task.started_at, t0);
        assert_eq!(task.last_exit_at, 0);
        assert_eq!(task.last_exit_kind, "none");
        assert_eq!(task.exit_count, 0);
        assert_eq!(task.current_backoff_ms, 0);

        state.mark_exited(t0 + 10, ExitKind::Panic);
        state.mark_backoff(5_000);
        let snap = reg.snapshot().await;
        let task = snap.iter().find(|s| s.name == "test_task").unwrap();
        assert_eq!(task.status, "backoff");
        assert_eq!(task.last_exit_at, t0 + 10);
        assert_eq!(task.last_exit_kind, "panic");
        assert_eq!(task.exit_count, 1);
        assert_eq!(task.current_backoff_ms, 5_000);
    }

    #[tokio::test]
    async fn mark_started_clears_backoff_field() {
        // After mark_started, the snapshot must read the backoff
        // field as 0 — otherwise the System page would show "next
        // restart in 5s" while the task is actually running.
        let reg = TaskRegistry::new();
        let state = reg.register("t").await;
        state.mark_exited(100, ExitKind::Normal);
        state.mark_backoff(5_000);
        let before = reg.snapshot().await[0].current_backoff_ms;
        assert_eq!(before, 5_000);

        state.mark_started(200);
        let after = reg.snapshot().await[0].current_backoff_ms;
        assert_eq!(after, 0, "backoff must clear when iteration restarts");
    }

    #[tokio::test]
    async fn snapshot_sorted_by_name_for_stable_render() {
        let reg = TaskRegistry::new();
        reg.register("zeta").await;
        reg.register("alpha").await;
        reg.register("middle").await;
        let snap = reg.snapshot().await;
        let names: Vec<&str> = snap.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["alpha", "middle", "zeta"]);
    }
}
