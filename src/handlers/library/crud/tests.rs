use super::*;
use crate::services::download_client::DownloadClient;
use crate::services::download_client::qbittorrent::QbitClient;
use crate::services::download_client::test_helpers;
use crate::test_support;
use std::sync::Arc;

// ─── CI-gated library-CRUD coverage (PR 7) ────────────────────
//
// These tests exercise the handler functions directly (not via
// the axum router) so they don't need a download client wired
// in. The complex client-backed paths (d1 / d2 / d3 above) stay
// env-gated; what lives below is the straight DB-mutation shape
// that drives the Library UI: set_folder, set_monitoring,
// set_allow_upgrades, set_episode_monitoring, set_search_overrides,
// set_manual_override. Handler calls take `State<AppState>` and
// `Json<Form>` and return `Result<Json<Value>, (StatusCode, _)>`
// — we construct Forms directly by field (crud.rs is a child
// module of `library`, so private form fields are visible).
mod crud_ci {
    use super::super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::Json as AxumJson;

    // Wrap the Result → extract the JSON body on success,
    // panicking on Err so tests surface the status / message.
    async fn ok_json<T>(res: Result<AxumJson<T>, (StatusCode, String)>) -> T {
        match res {
            Ok(AxumJson(body)) => body,
            Err((status, msg)) => panic!("handler returned error: {status} {msg}"),
        }
    }

    // For handlers that return `Response` (HTMX-aware ones that
    // branch on `HxRequest`), parse the body bytes as JSON. Used
    // by the non-HTMX path tests where the handler returns JSON.
    async fn response_json(resp: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&body).expect("parse response body as JSON")
    }

    // ─── set_folder ──────────────────────────────────────────

    #[tokio::test]
    async fn set_folder_persists_sanitized_name_on_happy_path() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 10, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let form = super::super::SetFolderForm {
            series_id,
            folder_name: "My Show - 2024".to_string(),
        };
        let _ = ok_json(set_folder(State(state), AxumJson(form)).await).await;
        let folder: String = sqlx::query_scalar("SELECT folder_name FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(folder, "My Show - 2024");
    }

    #[tokio::test]
    async fn set_folder_rejects_name_containing_path_separator() {
        // Caller passes `show/../etc`; sanitize strips the slash
        // and the sanitized output differs from the input — the
        // handler rejects with 400 per the policy of "we don't
        // silently rewrite folder names, we refuse the grey-area
        // input so the user picks an unambiguous one."
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 11, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetFolderForm {
            series_id,
            folder_name: "show/../etc".to_string(),
        };
        let err = set_folder(State(state), AxumJson(form))
            .await
            .expect_err("should reject slashed folder name");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_folder_rejects_empty_name() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 12, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetFolderForm {
            series_id,
            folder_name: String::new(),
        };
        let err = set_folder(State(state), AxumJson(form))
            .await
            .expect_err("should reject empty folder name");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    // ─── set_monitoring ──────────────────────────────────────

    #[tokio::test]
    async fn set_monitoring_accepts_all_mode_and_reports_summary() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 20, "Monitored Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetMonitoringForm {
            series_id,
            monitor_mode: "all".to_string(),
            auto_grab: Some(false),
        };
        let body: serde_json::Value =
            ok_json(set_monitoring(State(state), AxumJson(form)).await).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["monitor_mode"], "all");
    }

    #[tokio::test]
    async fn set_monitoring_accepts_none_mode_without_triggering_autosearch() {
        // `None` short-circuits the auto-grab branch regardless
        // of auto_grab flag. Pin the property — a refactor that
        // starts firing auto-search even on None would eat tokens
        // on transient monitoring flips.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 21, "Unmonitored").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetMonitoringForm {
            series_id,
            monitor_mode: "none".to_string(),
            auto_grab: Some(true),
        };
        let body: serde_json::Value =
            ok_json(set_monitoring(State(state), AxumJson(form)).await).await;
        assert_eq!(body["monitor_mode"], "none");
        assert_eq!(body["monitored_count"], 0);
    }

    #[tokio::test]
    async fn set_monitoring_unknown_mode_falls_back_to_future() {
        // MonitorMode::from_str on unrecognized input defaults to
        // the `future` bucket (the safest default — monitors
        // upcoming episodes but doesn't spam back-fill for a
        // long-finished series). A refactor that changes the
        // fallback to `all` would silently start mass-grabbing
        // old releases for every typo in the API body.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 22, "Typo Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetMonitoringForm {
            series_id,
            monitor_mode: "definitely-not-a-mode".to_string(),
            auto_grab: None,
        };
        let body: serde_json::Value =
            ok_json(set_monitoring(State(state), AxumJson(form)).await).await;
        assert_eq!(body["monitor_mode"], "future");
    }

    #[tokio::test]
    async fn set_monitoring_sets_manual_override_on_explicit_mode() {
        // Any explicit-mode pick from the dropdown pins
        // monitor_mode_manual_override = 1 so the next sync tick
        // doesn't silently overwrite the user's choice.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 40, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let form = super::super::SetMonitoringForm {
            series_id,
            monitor_mode: "all".to_string(),
            auto_grab: None,
        };
        let body: serde_json::Value =
            ok_json(set_monitoring(State(state), AxumJson(form)).await).await;
        assert_eq!(body["monitor_mode"], "all");
        assert_eq!(body["monitor_mode_manual_override"], true);

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert!(
            row.monitor_mode_manual_override,
            "explicit-mode pick must set the override flag"
        );
    }

    #[test]
    fn monitor_mode_sync_sentinel_matches_template_literal() {
        // The dropdown in templates/series.html hardcodes
        // <option value="sync">; the handler's branch keys off
        // MONITOR_MODE_SYNC_SENTINEL. A rename of the constant
        // would silently desync the two and the dropdown option
        // would no-op. Pin the value so the rename forces a
        // template edit.
        assert_eq!(super::super::MONITOR_MODE_SYNC_SENTINEL, "sync");
        // Also confirm the template still emits the literal —
        // catches the inverse: someone renames the template
        // option but forgets the const.
        let template = include_str!("../../../../templates/series.html");
        assert!(
            template.contains(r#"<option value="sync""#),
            "templates/series.html must keep the sync sentinel option in sync with MONITOR_MODE_SYNC_SENTINEL"
        );
    }

    #[tokio::test]
    async fn set_monitoring_sync_sentinel_clears_manual_override() {
        // The "sync" sentinel from the dropdown's "Sync from
        // AL/MAL" option clears the override flag without touching
        // monitor_mode — next sync tick computes the right mode
        // from the user's current AL/MAL status. Pre-condition the
        // series with the override on so we can assert the clear.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 50, "Show").await;
        sqlx::query(
            "UPDATE series SET monitor_mode = 'all', monitor_mode_manual_override = 1 WHERE id = ?",
        )
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let form = super::super::SetMonitoringForm {
            series_id,
            monitor_mode: "sync".to_string(),
            auto_grab: None,
        };
        let body: serde_json::Value =
            ok_json(set_monitoring(State(state), AxumJson(form)).await).await;
        assert_eq!(body["monitor_mode_manual_override"], false);

        let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
        assert!(
            !row.monitor_mode_manual_override,
            "the sync sentinel must clear the override flag"
        );
        // monitor_mode itself stays at "all" — the next sync tick
        // is responsible for replacing it with the AL-derived
        // value. Don't assert on the recomputed mode here since
        // the test setup didn't change it.
        assert_eq!(row.monitor_mode, "all");
    }

    // ─── set_episode_monitoring ──────────────────────────────

    #[tokio::test]
    async fn set_episode_monitoring_flips_monitored_flag() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 30, "Show").await;
        let state = build_test_app_state(db, None);
        for monitored in [true, false, true] {
            let form = super::super::SetEpisodeMonitoringForm {
                series_id,
                episode_number: 5,
                monitored,
            };
            let response =
                set_episode_monitoring(State(state.clone()), HxRequest(false), Form(form))
                    .await
                    .expect("set_episode_monitoring");
            let body: serde_json::Value = response_json(response).await;
            assert_eq!(body["monitored"], monitored);
            assert_eq!(body["episode_number"], 5);
        }
    }

    /// HTMX migration (issue #129) — when the request carries
    /// `HX-Request: true`, the handler returns the rendered button
    /// HTML instead of the JSON envelope. The button's class +
    /// hx-vals reflect the NEW state so the next click toggles
    /// in the opposite direction.
    #[tokio::test]
    async fn set_episode_monitoring_returns_button_html_for_htmx_request() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 31, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetEpisodeMonitoringForm {
            series_id,
            episode_number: 7,
            monitored: true,
        };
        let response = set_episode_monitoring(State(state.clone()), HxRequest(true), Form(form))
            .await
            .expect("set_episode_monitoring");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = std::str::from_utf8(&body).expect("utf8");
        // Class reflects the NEW state.
        assert!(
            html.contains("ep-mon-yes"),
            "monitored=true response must render the yes-state class; got: {html}"
        );
        assert!(
            !html.contains("ep-mon-no"),
            "monitored=true response must NOT render the no-state class; got: {html}"
        );
        // hx-vals carries the OPPOSITE state for the next click.
        assert!(
            html.contains(r#""monitored": false"#),
            "next-click should toggle to monitored=false; got: {html}"
        );
    }

    // ─── set_allow_upgrades ──────────────────────────────────

    #[tokio::test]
    async fn set_allow_upgrades_persists_flag() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 40, "Show").await;
        let state = build_test_app_state(db.clone(), None);

        // Flip off.
        let body: serde_json::Value = ok_json(
            set_allow_upgrades(
                State(state.clone()),
                AxumJson(super::super::SetAllowUpgradesForm {
                    series_id,
                    allow: false,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(body["allow_upgrades"], false);
        let stored: i64 = sqlx::query_scalar("SELECT allow_upgrades FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, 0, "allow=false must persist as 0");

        // Flip back on.
        let body: serde_json::Value = ok_json(
            set_allow_upgrades(
                State(state),
                AxumJson(super::super::SetAllowUpgradesForm {
                    series_id,
                    allow: true,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(body["allow_upgrades"], true);
    }

    // ─── set_allow_pt_upgrades (#28 PR E) ─────────────────────

    #[tokio::test]
    async fn set_allow_pt_upgrades_persists_flag_and_defaults_off() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 41, "Show").await;
        let state = build_test_app_state(db.clone(), None);

        // Default state — series.allow_pt_upgrades should be 0
        // even though we haven't called the toggle yet. Pin the
        // default-off invariant so a future migration that
        // changes the column default to 1 has to be deliberate.
        let stored: i64 = sqlx::query_scalar("SELECT allow_pt_upgrades FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, 0, "PT upgrade opt-in must default OFF");

        // Flip on.
        let body: serde_json::Value = ok_json(
            set_allow_pt_upgrades(
                State(state.clone()),
                AxumJson(super::super::SetAllowPtUpgradesForm {
                    series_id,
                    allow: true,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(body["allow_pt_upgrades"], true);
        let stored: i64 = sqlx::query_scalar("SELECT allow_pt_upgrades FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, 1);

        // Flip off.
        let body: serde_json::Value = ok_json(
            set_allow_pt_upgrades(
                State(state),
                AxumJson(super::super::SetAllowPtUpgradesForm {
                    series_id,
                    allow: false,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(body["allow_pt_upgrades"], false);
        let stored: i64 = sqlx::query_scalar("SELECT allow_pt_upgrades FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn set_allow_pt_upgrades_independent_of_allow_upgrades() {
        // The two flags are orthogonal — flipping one shouldn't
        // affect the other. Pin the invariant since both columns
        // live on the same row and a stray UPDATE on the wrong
        // column would silently ride out under simpler tests.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 42, "Show").await;
        let state = build_test_app_state(db.clone(), None);

        // allow_upgrades on, allow_pt_upgrades on.
        ok_json(
            set_allow_upgrades(
                State(state.clone()),
                AxumJson(super::super::SetAllowUpgradesForm {
                    series_id,
                    allow: true,
                }),
            )
            .await,
        )
        .await;
        ok_json(
            set_allow_pt_upgrades(
                State(state.clone()),
                AxumJson(super::super::SetAllowPtUpgradesForm {
                    series_id,
                    allow: true,
                }),
            )
            .await,
        )
        .await;
        // Now flip allow_upgrades off; allow_pt_upgrades must
        // stay on.
        ok_json(
            set_allow_upgrades(
                State(state),
                AxumJson(super::super::SetAllowUpgradesForm {
                    series_id,
                    allow: false,
                }),
            )
            .await,
        )
        .await;
        let row: (i64, i64) =
            sqlx::query_as("SELECT allow_upgrades, allow_pt_upgrades FROM series WHERE id = ?")
                .bind(series_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row, (0, 1), "allow_pt_upgrades must persist independently");
    }

    // ─── set_search_overrides ────────────────────────────────

    #[tokio::test]
    async fn set_search_overrides_persists_tokens_and_uploader() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 50, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let form = super::super::SetSearchOverridesForm {
            series_id,
            custom_query_tokens: "  1080p BD  ".to_string(),
            restrict_to_uploader: "TrustedUser".to_string(),
        };
        let body: serde_json::Value =
            ok_json(set_search_overrides(State(state), AxumJson(form)).await).await;
        // Response trims whitespace for display.
        assert_eq!(body["custom_query_tokens"], "1080p BD");
        assert_eq!(body["restrict_to_uploader"], "TrustedUser");
        // DB row should carry the stored values.
        let (tokens, uploader): (String, String) = sqlx::query_as(
            "SELECT custom_query_tokens, restrict_to_uploader FROM series WHERE id = ?",
        )
        .bind(series_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(tokens.contains("1080p"));
        assert_eq!(uploader, "TrustedUser");
    }

    #[tokio::test]
    async fn set_search_overrides_clears_on_empty_strings() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 51, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        // Set a value.
        let _ = set_search_overrides(
            State(state.clone()),
            AxumJson(super::super::SetSearchOverridesForm {
                series_id,
                custom_query_tokens: "initial".to_string(),
                restrict_to_uploader: "User".to_string(),
            }),
        )
        .await;
        // Clear with empty strings — per the form docstring, empty
        // resets to global defaults.
        let body: serde_json::Value = ok_json(
            set_search_overrides(
                State(state),
                AxumJson(super::super::SetSearchOverridesForm {
                    series_id,
                    custom_query_tokens: String::new(),
                    restrict_to_uploader: String::new(),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(body["custom_query_tokens"], "");
        assert_eq!(body["restrict_to_uploader"], "");
    }

    // ─── set_manual_override ─────────────────────────────────

    #[tokio::test]
    async fn set_manual_override_accepts_valid_source_and_resolution() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 60, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetManualOverrideForm {
            series_id,
            episode_number: 3,
            source: "BluRay".to_string(),
            resolution: "1080".to_string(),
            is_remux: true,
            is_bdmv: false,
            web_kind: String::new(),
        };
        let body: serde_json::Value =
            ok_json(set_manual_override(State(state), AxumJson(form)).await).await;
        assert_eq!(body["source"], "BluRay");
        assert_eq!(body["resolution"], "1080p");
        assert_eq!(body["is_remux"], true);
    }

    #[tokio::test]
    async fn set_manual_override_rejects_invalid_source() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 61, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetManualOverrideForm {
            series_id,
            episode_number: 3,
            source: "DefinitelyNotASource".to_string(),
            resolution: "1080".to_string(),
            is_remux: false,
            is_bdmv: false,
            web_kind: String::new(),
        };
        let err = set_manual_override(State(state), AxumJson(form))
            .await
            .expect_err("invalid source should 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1.contains("source"),
            "error should name the source field: {}",
            err.1
        );
    }

    #[tokio::test]
    async fn set_manual_override_rejects_invalid_resolution() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 62, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetManualOverrideForm {
            series_id,
            episode_number: 3,
            source: "BluRay".to_string(),
            resolution: "99999".to_string(),
            is_remux: false,
            is_bdmv: false,
            web_kind: String::new(),
        };
        let err = set_manual_override(State(state), AxumJson(form))
            .await
            .expect_err("invalid resolution should 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_manual_override_empty_source_clears_and_skips_validation() {
        // Empty `source` is the "clear override" path — it
        // should NOT run through Source::from_str validation.
        // The resolution can be anything, the web_kind can be
        // anything; it all resets.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 63, "Show").await;
        let state = build_test_app_state(db, None);
        let form = super::super::SetManualOverrideForm {
            series_id,
            episode_number: 3,
            source: String::new(),
            resolution: "garbage".to_string(),
            is_remux: false,
            is_bdmv: false,
            web_kind: "garbage".to_string(),
        };
        let body: serde_json::Value =
            ok_json(set_manual_override(State(state), AxumJson(form)).await).await;
        assert_eq!(body["source"], "");
        assert_eq!(body["resolution"], "");
    }

    // ─── remove_series (no-client path) ──────────────────────

    #[tokio::test]
    async fn remove_series_with_delete_files_false_drops_row_without_client() {
        // `delete_files = false` is the API path the Sonarr shim
        // takes: drop the DB tracking row only, leave torrents +
        // media alone. That path doesn't need a download client
        // wired up; a `None` client shouldn't interfere.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 70, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let form = super::super::RemoveSeriesForm {
            id: series_id,
            delete_files: Some(false),
        };
        let result = remove_series(State(state), AxumJson(form)).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(remaining, 0, "series row must be deleted");
    }

    #[tokio::test]
    async fn remove_series_on_missing_id_is_idempotent() {
        // Deleting a non-existent series is a no-op — the handler
        // short-circuits the "lookup returns None" branch and the
        // DB delete is harmless.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let form = super::super::RemoveSeriesForm {
            id: 99_999,
            delete_files: Some(false),
        };
        let result = remove_series(State(state), AxumJson(form)).await;
        assert!(result.is_ok(), "missing id must still return Ok");
    }
}

/// D1 live integration test: removing a series from the library
/// must also delete every grabbed torrent for that series from
/// the active download client.
///
/// Flow under test:
/// 1. Seed the DB with one series + two grabbed_torrents rows
///    pointing at it.
/// 2. Upload the two synthetic torrents to qBit so the hashes
///    are real + addressable.
/// 3. Call `remove_series(id, delete_files=true)`.
/// 4. Assert every hash is gone from qBit's scoped list and
///    the `grabbed_torrents` rows cascaded away with the series.
///
/// Env-gated (`RYOKAN_QBIT_E2E=1`) so the default `cargo test` run
/// never touches the download client.
#[tokio::test]
#[ignore = "requires live qBit + transmission-create"]
async fn d1_remove_series_cleans_up_torrents_and_rows() {
    if std::env::var("RYOKAN_QBIT_E2E").is_err() {
        eprintln!("skipping (set RYOKAN_QBIT_E2E=1)");
        return;
    }
    let Some((_tmp_a, torrent_a)) = test_helpers::build_named_torrent("d1-series-remove-a") else {
        return;
    };
    let Some((_tmp_b, torrent_b)) = test_helpers::build_named_torrent("d1-series-remove-b") else {
        return;
    };
    let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
    let base_url = "http://localhost:8080";
    let category = "ryokan-e2e-d1";

    // Two separate categories so upload_torrent_file_qbit's
    // first-returned-hash convention picks up each distinct
    // torrent rather than colliding in the same category.
    let cat_a = format!("{category}-a");
    let cat_b = format!("{category}-b");
    let hash_a =
        test_helpers::upload_torrent_file_qbit(base_url, "admin", &pass, &cat_a, &torrent_a).await;
    let hash_b =
        test_helpers::upload_torrent_file_qbit(base_url, "admin", &pass, &cat_b, &torrent_b).await;

    // Scaffolding: pool + migrations + AppState with a real
    // qBit client on the same category used for seeding, so
    // `remove_series`'s internal scoped-list lookup and delete
    // path hit the same torrents.
    let pool = test_support::in_memory_pool().await;
    let qbit: Arc<dyn DownloadClient> =
        Arc::new(QbitClient::new(base_url, "admin", &pass, category));
    let state = test_support::build_test_app_state(pool.clone(), Some(qbit.clone()));

    // Seed: one series, two grabbed rows. `folder_name` stays
    // empty so the handler's "delete media folder" step no-ops
    // (we're testing torrent cleanup, not filesystem removal).
    let series_id = test_support::seed_series(&pool, 12345, "D1 Test Series").await;
    let _gid_a =
        test_support::seed_grabbed_torrent(&pool, series_id, &hash_a, "d1-test-a.torrent", &[1])
            .await;
    let _gid_b =
        test_support::seed_grabbed_torrent(&pool, series_id, &hash_b, "d1-test-b.torrent", &[2])
            .await;
    assert_eq!(
        test_support::count_grabs_for_series(&pool, series_id).await,
        2,
        "precondition: 2 grabs seeded"
    );
    assert_eq!(
        test_support::count_series(&pool).await,
        1,
        "precondition: 1 series seeded"
    );

    // Exercise: call the handler directly.
    let form: RemoveSeriesForm = serde_json::from_value(serde_json::json!({
        "id": series_id,
        "delete_files": true,
    }))
    .expect("deserialize RemoveSeriesForm");
    let result = remove_series(axum::extract::State(state.clone()), axum::Json(form)).await;
    assert!(
        result.is_ok(),
        "remove_series returned error: {:?}",
        result
            .as_ref()
            .err()
            .map(|(s, b)| (s.as_u16(), b.0.to_string()))
    );

    // Assert: grab rows cascaded, series row gone.
    assert_eq!(
        test_support::count_grabs_for_series(&pool, series_id).await,
        0,
        "D1: grabbed_torrents must cascade on series delete"
    );
    assert_eq!(
        test_support::count_series(&pool).await,
        0,
        "D1: series row must be gone"
    );

    // Assert: torrents deleted from qBit. We check via a
    // scoped-list-agnostic lookup (separate QbitClients, one per
    // upload category) so category filtering doesn't mask a
    // "still exists but uncategorized" state.
    for (scope_cat, hash) in [(cat_a.as_str(), &hash_a), (cat_b.as_str(), &hash_b)] {
        let scoped_client = QbitClient::new(base_url, "admin", &pass, scope_cat);
        let list = scoped_client
            .list_scoped()
            .await
            .expect("list_scoped post-remove");
        assert!(
            !list.iter().any(|t| t.hash.eq_ignore_ascii_case(hash)),
            "D1: torrent {hash} must be deleted from qBit after remove_series"
        );
    }
    eprintln!("D1 integration verified");
}
