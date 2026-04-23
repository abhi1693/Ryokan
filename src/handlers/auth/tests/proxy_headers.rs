//! `client_ip_from_request_with_trust` coverage — the header-vs-peer
//! logic that decides whether `X-Forwarded-For` / `X-Real-IP` are
//! honored. The production wrapper reads `RYOKAN_TRUSTED_PROXY` once
//! at startup; the `_with_trust` variant takes the flag explicitly
//! so both code paths are reachable from a single test binary
//! (otherwise we'd need subprocess tests to exercise both values of
//! the LazyLock).

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;

use crate::handlers::auth::client_ip_from_request_with_trust;

fn peer(ip: &str) -> Option<SocketAddr> {
    Some(SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 12345))
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

// ─── Trust off: headers always ignored, peer wins ──────────────────

#[test]
fn trust_off_ignores_x_forwarded_for_uses_peer() {
    let h = headers(&[("x-forwarded-for", "203.0.113.1")]);
    let p = peer("10.0.0.5");
    let ip = client_ip_from_request_with_trust(&h, p, false);
    assert_eq!(ip, "10.0.0.5");
}

#[test]
fn trust_off_ignores_x_real_ip_uses_peer() {
    let h = headers(&[("x-real-ip", "203.0.113.7")]);
    let p = peer("10.0.0.5");
    assert_eq!(client_ip_from_request_with_trust(&h, p, false), "10.0.0.5");
}

#[test]
fn trust_off_both_headers_set_still_returns_peer() {
    let h = headers(&[
        ("x-forwarded-for", "203.0.113.1"),
        ("x-real-ip", "203.0.113.7"),
    ]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.9"), false),
        "10.0.0.9"
    );
}

#[test]
fn trust_off_no_peer_returns_unknown() {
    let h = headers(&[("x-forwarded-for", "203.0.113.1")]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, None, false),
        "unknown"
    );
}

// ─── Trust on: leftmost XFF wins, then XRI, then peer ──────────────

#[test]
fn trust_on_prefers_leftmost_x_forwarded_for() {
    // XFF accumulates upstream entries left-to-right; the leftmost
    // is the client the outermost proxy saw.
    let h = headers(&[
        ("x-forwarded-for", "203.0.113.1, 10.0.0.99, 10.0.0.2"),
        ("x-real-ip", "203.0.113.7"),
    ]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.5"), true),
        "203.0.113.1"
    );
}

#[test]
fn trust_on_falls_back_to_x_real_ip_when_xff_absent() {
    let h = headers(&[("x-real-ip", "203.0.113.7")]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.5"), true),
        "203.0.113.7"
    );
}

#[test]
fn trust_on_falls_back_to_peer_when_both_headers_missing() {
    let h = headers(&[]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.5"), true),
        "10.0.0.5"
    );
}

#[test]
fn trust_on_skips_empty_xff_entry_uses_x_real_ip() {
    // A proxy emitting "X-Forwarded-For: " with just whitespace
    // should not set the IP to empty string; we expect the trimmed
    // empty string to be ignored and XRI picked up.
    let h = headers(&[("x-forwarded-for", "   "), ("x-real-ip", "203.0.113.7")]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.5"), true),
        "203.0.113.7"
    );
}

#[test]
fn trust_on_trims_whitespace_around_leftmost_xff_entry() {
    // RFC 7239 doesn't mandate whitespace, but real-world proxies
    // often pad with a single space after the comma. Trim it.
    let h = headers(&[("x-forwarded-for", "  203.0.113.1  , 10.0.0.2")]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.5"), true),
        "203.0.113.1"
    );
}

#[test]
fn trust_on_empty_x_real_ip_falls_through_to_peer() {
    let h = headers(&[("x-real-ip", "")]);
    assert_eq!(
        client_ip_from_request_with_trust(&h, peer("10.0.0.5"), true),
        "10.0.0.5"
    );
}
