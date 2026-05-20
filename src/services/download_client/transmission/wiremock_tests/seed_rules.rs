//! Issue #28 — `set_seed_rules` against Transmission's
//! `torrent-set` RPC.
//!
//! Wire shape: `torrent-set` with `ids: [hash]` plus
//! `seedRatioLimit` / `seedRatioMode` for ratio rules and
//! `seedIdleLimit` / `seedIdleMode` for idle-time rules. Mode = 1
//! is the per-torrent override (vs 0 = global default, 2 =
//! unlimited). `None` rules are omitted entirely so any
//! pre-existing per-torrent setting from a prior grab stays in
//! place.

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_SESSION_ID, new_fixture};
use crate::services::download_client::{DownloadClient, SeedRules};

const HASH: &str = "aabbcc0011223344";

#[tokio::test]
async fn seed_rules_with_ratio_sets_mode_one_and_limit() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-set",
            "arguments": {
                "ids": [HASH],
                "seedRatioLimit": 2.0,
                "seedRatioMode": 1,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
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
async fn seed_rules_with_time_sets_idle_limit_minutes() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .and(header("x-transmission-session-id", TEST_SESSION_ID))
        .and(body_partial_json(json!({
            "method": "torrent-set",
            "arguments": {
                "ids": [HASH],
                "seedIdleLimit": 60,
                "seedIdleMode": 1,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "arguments": {},
            "tag": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_seed_rules(
            HASH,
            SeedRules {
                ratio: None,
                time_minutes: Some(60),
            },
        )
        .await
        .expect("seed rules");
}

#[tokio::test]
async fn seed_rules_empty_skips_wire_call() {
    // Both fields None — the impl shouldn't issue a torrent-set
    // call at all (mode-0 reset would clobber a previous rule).
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/transmission/rpc"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    client
        .set_seed_rules(HASH, SeedRules::default())
        .await
        .expect("empty rules must no-op");
}
