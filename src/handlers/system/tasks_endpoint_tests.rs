use crate::services::task_registry::ExitKind;
use crate::test_support::{build_test_app_state, in_memory_pool};

#[tokio::test]
async fn endpoint_returns_registered_tasks_with_snapshot_state() {
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);

    // Register two tasks and mark them through different lifecycle
    // states so the snapshot has something distinguishable to assert on.
    let rss_state = state.tasks.register("rss_sync").await;
    rss_state.mark_started(1_000);
    let cleanup_state = state.tasks.register("cleanup").await;
    cleanup_state.mark_started(500);
    cleanup_state.mark_exited(600, ExitKind::Panic);
    cleanup_state.mark_backoff(10_000);

    let resp = super::api_system_tasks(axum::extract::State(state)).await;
    let tasks = resp.0.tasks;
    assert_eq!(tasks.len(), 2);

    let cleanup = tasks.iter().find(|t| t.name == "cleanup").unwrap();
    assert_eq!(cleanup.status, "backoff");
    assert_eq!(cleanup.last_exit_kind, "panic");
    assert_eq!(cleanup.exit_count, 1);
    assert_eq!(cleanup.current_backoff_ms, 10_000);

    let rss = tasks.iter().find(|t| t.name == "rss_sync").unwrap();
    assert_eq!(rss.status, "running");
    assert_eq!(rss.last_exit_kind, "none");
    assert_eq!(rss.exit_count, 0);
    assert_eq!(rss.current_backoff_ms, 0);
}

#[tokio::test]
async fn endpoint_returns_empty_array_when_no_tasks_registered() {
    // Fresh AppState shouldn't have any tasks yet — the
    // `services::task_registry::TaskRegistry::new()` shipped on
    // `AppState` is lazy; supervise() registers on first call.
    // Tests + cargo run between starts both go through this state,
    // so the empty-snapshot shape needs to be valid JSON.
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, None);
    let resp = super::api_system_tasks(axum::extract::State(state)).await;
    assert!(resp.0.tasks.is_empty());
}
