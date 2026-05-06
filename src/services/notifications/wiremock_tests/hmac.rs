//! HMAC signature path. Pins the `sha256=<hex>` shape (matches the
//! GitHub webhook convention so receivers can use off-the-shelf
//! verification code) and asserts the receiver-side
//! `hmac::Mac::verify` over the raw body bytes succeeds.

use super::fixture::{make_provider_with_secret, sample_event};
use crate::services::notifications::NotificationProvider;
use ::hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn signature_header_emitted_when_secret_is_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header_exists("X-Ryokan-Signature"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let provider = make_provider_with_secret(&server.uri(), "shared-secret");
    provider.send(&sample_event()).await.expect("send ok");
}

#[tokio::test]
async fn signature_verifies_against_raw_body_bytes_with_configured_secret() {
    // The whole point of the bytes-not-string contract: receivers
    // verify HMAC against the bytes they actually received. Any
    // re-serialization (whitespace, key-order) breaks that. Pinned.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let secret = "shared-secret-12345";
    let provider = make_provider_with_secret(&server.uri(), secret);
    provider.send(&sample_event()).await.expect("send ok");
    let received = server.received_requests().await.expect("recordings");
    assert_eq!(received.len(), 1);
    let req = &received[0];
    let sig_header = req
        .headers
        .iter()
        .find(|(n, _)| n.as_str().eq_ignore_ascii_case("x-ryokan-signature"))
        .map(|(_, v)| v.to_str().unwrap_or("").to_string())
        .expect("signature header present");
    let hex_sig = sig_header
        .strip_prefix("sha256=")
        .expect("sha256= prefix on signature");
    let sig_bytes = hex::decode(hex_sig).expect("signature is hex");

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(&req.body);
    mac.verify_slice(&sig_bytes)
        .expect("HMAC over raw body bytes must verify against the configured secret");
}

#[tokio::test]
async fn empty_secret_string_treated_as_no_secret() {
    // Settings UI may submit an empty string when the user clears
    // the field. Pinned so a regression that emitted
    // `sha256=<HMAC of empty key over body>` doesn't silently send
    // a forgeable signature — receivers verifying HMAC with an
    // empty key would accept it.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let provider = make_provider_with_secret(&server.uri(), "");
    provider.send(&sample_event()).await.expect("send ok");
    let received = server.received_requests().await.expect("recordings");
    assert!(
        received[0]
            .headers
            .iter()
            .all(|(n, _)| !n.as_str().eq_ignore_ascii_case("x-ryokan-signature")),
        "empty secret must produce no signature header"
    );
}
