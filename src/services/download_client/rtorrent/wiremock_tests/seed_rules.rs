//! Issue #28 PR C — `set_seed_rules` against rTorrent's
//! `d.ratio.max.set` XML-RPC call.
//!
//! rTorrent's wire format:
//!   * Hash uppercase (every `d.<method>` call keyed by hash).
//!   * Ratio in **per-mille** (ratio × 1000): `1.5` becomes `1500`.
//!     This is rTorrent's standard ratio-group convention; the
//!     other ratio knobs (`min`/`upload`) govern graduated
//!     stopping behavior we don't need.
//!   * `time_minutes` is a no-op — rTorrent core has no native
//!     idle-time stop; the impl logs at debug and skips the call.

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{int_response, new_fixture};
use crate::services::download_client::{DownloadClient, SeedRules};

const HASH_LC: &str = "aabbccddeeff00112233445566778899aabbccdd";
const HASH_UC: &str = "AABBCCDDEEFF00112233445566778899AABBCCDD";

#[tokio::test]
async fn seed_rules_with_ratio_calls_d_ratio_max_set_in_permille() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(
            "<methodName>d.ratio.max.set</methodName>",
        ))
        .and(body_string_contains(HASH_UC))
        // 1.5 ratio × 1000 = 1500 per-mille.
        .and(body_string_contains("<i8>1500</i8>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(int_response(0)))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_seed_rules(
            HASH_LC,
            SeedRules {
                ratio: Some(1.5),
                time_minutes: None,
            },
        )
        .await
        .expect("seed rules");
}

#[tokio::test]
async fn seed_rules_with_only_time_skips_wire_call() {
    // rtorrent has no native idle-time stop. With ratio also None
    // the impl is a complete no-op — pin that no RPC fires.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    client
        .set_seed_rules(
            HASH_LC,
            SeedRules {
                ratio: None,
                time_minutes: Some(60),
            },
        )
        .await
        .expect("must not error when there's nothing to send");
}

#[tokio::test]
async fn seed_rules_empty_skips_wire_call() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    client
        .set_seed_rules(HASH_LC, SeedRules::default())
        .await
        .expect("empty rules must no-op");
}
