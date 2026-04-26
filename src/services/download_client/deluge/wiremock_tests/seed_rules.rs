//! Issue #28 PR C — `set_seed_rules` against Deluge's
//! `core.set_torrent_options`.
//!
//! Wire shape: `core.set_torrent_options([torrent_id], {...})`.
//! Setting `stop_at_ratio: true` flips the per-torrent override
//! so this torrent stops at its own ratio. `time_minutes` has no
//! Deluge core mapping — silently dropped (debug log) since the
//! autoremoveplus plugin isn't bundled with vanilla Deluge.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::{DownloadClient, SeedRules};

const HASH: &str = "aabbcc0011223344";

#[tokio::test]
async fn seed_rules_with_ratio_sets_stop_at_ratio_and_stop_ratio() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_partial_json(json!({
            "method": "core.set_torrent_options",
            "params": [[HASH], {"stop_at_ratio": true, "stop_ratio": 2.0}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": null,
            "id": 1,
        })))
        .expect(1)
        .mount(&server)
        .await;

    client
        .set_seed_rules(
            HASH,
            SeedRules {
                ratio: Some(2.0),
                time_minutes: None,
            },
        )
        .await
        .expect("seed rules");
}

#[tokio::test]
async fn seed_rules_with_only_time_skips_wire_call() {
    // time_minutes is unsupported on Deluge core. With ratio also
    // None, the options dict is empty and the impl skips the wire
    // call entirely. Pin that no RPC is made when there's nothing
    // for Deluge to apply.
    let (server, client) = new_fixture().await;
    // No mount — any wire call would hit a 404 and surface as Err.
    Mock::given(method("POST"))
        .and(path("/json"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    client
        .set_seed_rules(
            HASH,
            SeedRules {
                ratio: None,
                time_minutes: Some(120),
            },
        )
        .await
        .expect("must not error when there's nothing to send");
}

#[tokio::test]
async fn seed_rules_empty_skips_wire_call() {
    // SeedRules::default() — both fields None — must not call out.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    client
        .set_seed_rules(HASH, SeedRules::default())
        .await
        .expect("empty rules must no-op");
}
