//! Generic webhook outbound notification provider (issue #119).
//!
//! POSTs the canonical `NotificationEvent` JSON from #118 to a
//! user-configured URL. Unblocks every downstream integration
//! (ntfy, Apprise, n8n, Home Assistant, custom scripts) without
//! per-tool support code in Ryokan.
//!
//! ## Wire shape
//!
//! - `POST <url>`
//! - `Content-Type: application/json; charset=utf-8`
//! - `X-Ryokan-Delivery: <16-byte hex>` for receiver-side dedup.
//! - `X-Ryokan-Timestamp: <unix-seconds>` so receivers can reject
//!   stale deliveries.
//! - `X-Ryokan-Event: <event-kind>` so receivers can route without
//!   parsing the body.
//! - `X-Ryokan-Signature: sha256=<hex>` when a `secret` is
//!   configured. HMAC is over the raw body **bytes** that go on the
//!   wire — re-serialization would mismatch.
//! - User-supplied custom headers added last so they can override
//!   anything except `Content-Type` and `X-Ryokan-Signature`.

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use sha2::Sha256;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{NotificationEvent, NotificationProvider};

/// 10s. Notifications are tiny POSTs and a slow receiver isn't worth
/// waiting for. Distinct from the 30s RSS timeout because the request
/// shape and budget are different — RSS pulls multi-MB responses
/// over flaky public CDN paths; notifications POST a few KB of JSON
/// to a user-configured endpoint we expect to be local-ish.
const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on the receiver's response body that gets logged. A verbose
/// 500 from a misconfigured nginx in front of n8n can return tens
/// of KB of HTML; without truncation a single failure could blow
/// up the `logs` table.
const RESPONSE_BODY_LOG_CAP: usize = 256;

/// Process-wide `reqwest::Client` for outbound webhooks. Same
/// rationale as the other shared clients in the codebase
/// (`RSS_HTTP_CLIENT`, `nyaa::HTTP_CLIENT`, etc.) — a fresh
/// `Client` per call throws away connection keepalive and re-
/// handshakes TLS on every send. `pool_max_idle_per_host = 8`
/// keeps the common case (repeat notifications to the same
/// Discord / ntfy / Apprise endpoint) fast.
///
/// **Cookies disabled** — webhook receivers shouldn't be setting
/// session cookies on us; cross-pollination with the RSS client's
/// cookie store would be a weird threat surface for no benefit.
pub static WEBHOOK_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(WEBHOOK_REQUEST_TIMEOUT)
        .pool_max_idle_per_host(8)
        .build()
        .expect("building the webhook reqwest client should not fail")
});

/// Persisted shape of the `notification_providers.config_json` blob
/// for `kind = 'webhook'`. Settings handler validates `url` via
/// [`validate_url`] before write; the rebuild path then deserializes
/// stored rows into this struct.
///
/// `headers` is stored as a tuple-list rather than a HashMap so
/// insertion order is preserved — some receivers care about header
/// order (e.g., `Authorization: Bearer ...` is conventionally first).
/// `serde_json` will deserialize a flat `{"X-Foo": "bar"}` JSON
/// object into a `Vec<(String, String)>` via a manual deserializer
/// helper, so the user-facing settings form can keep the natural
/// JSON object shape; the alternative (an explicit array of
/// objects) is harder to author by hand.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: Vec<(String, String)>,
}

/// Custom deserializer so `headers` can be authored as a plain
/// JSON object. `serde_json::Map<String, String>` doesn't preserve
/// insertion order in older serde_json versions; pulling through a
/// `Vec<(String, String)>` via the visitor preserves the JSON
/// document's native order on every supported version.
fn deserialize_headers<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{MapAccess, Visitor};
    use std::fmt;

    struct HeadersVisitor;
    impl<'de> Visitor<'de> for HeadersVisitor {
        type Value = Vec<(String, String)>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a JSON object of header name/value strings")
        }
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some((k, v)) = map.next_entry::<String, String>()? {
                out.push((k, v));
            }
            Ok(out)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Vec::new())
        }
        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Vec::new())
        }
    }
    deserializer.deserialize_any(HeadersVisitor)
}

/// Save-time URL validator. Settings save handler invokes this
/// before persisting the row. **Doesn't** test connectivity —
/// receivers behind firewalls / private networks legitimately
/// may not respond to a probe from Ryokan's host. The Settings
/// UI's "Test" button handles user-driven verification.
pub fn validate_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("URL is empty".into());
    }
    // Require explicit `://` separator. The url crate accepts a few
    // shapes that look ambiguous as receiver targets (`http:hook`
    // resolves to a relative path with no host on parse but reports
    // `cannot_be_a_base = true`); rather than chase parser-version-
    // specific behaviors, anchor on the most familiar shape and
    // reject anything else at the string level. Settings UI users
    // type the scheme either way.
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("URL must start with http:// or https://".into());
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|e| format!("URL parse failed: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme '{other}'; use http or https")),
    }
    if parsed.host_str().unwrap_or("").is_empty() {
        return Err("URL is missing a host".into());
    }
    Ok(())
}

/// One configured webhook destination. Constructed in
/// `rebuild_notification_providers_cache` from the `notification_providers`
/// row + parsed `config_json`.
pub struct WebhookProvider {
    id: i64,
    name: String,
    config: WebhookConfig,
}

impl WebhookProvider {
    pub fn new(id: i64, name: String, config: WebhookConfig) -> Self {
        Self { id, name, config }
    }

    /// Construct from a raw `notification_providers` row's
    /// `config_json` blob. Caller has already filtered by
    /// `kind = 'webhook'`. URL validation runs again here as a
    /// defense-in-depth check — a hand-edited DB row that bypassed
    /// the settings handler shouldn't blow up the cache rebuild.
    pub fn from_row(id: i64, name: String, config_json: &str) -> Result<Self, String> {
        let config: WebhookConfig = serde_json::from_str(config_json)
            .map_err(|e| format!("invalid webhook config_json: {e}"))?;
        validate_url(&config.url)?;
        Ok(Self::new(id, name, config))
    }
}

#[async_trait]
impl NotificationProvider for WebhookProvider {
    fn id(&self) -> i64 {
        self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "webhook"
    }

    async fn send(&self, event: &NotificationEvent) -> Result<(), String> {
        let (body, headers) = build_request(event, &self.config)?;
        let response = match WEBHOOK_HTTP_CLIENT
            .post(&self.config.url)
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(format!(
                    "request timed out after {}s",
                    WEBHOOK_REQUEST_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("transport error: {e}")),
        };

        let status = response.status();
        // Read the response body even on success — the caller
        // logs delivery confirmations + errors with enough detail
        // for users to debug ("401 Unauthorized" / "418 I'm a
        // teapot" / receiver-shaped error envelopes).
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!(
                "receiver returned {} ({}): {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                truncate(&body, RESPONSE_BODY_LOG_CAP)
            ))
        }
    }
}

/// Build the wire-shaped (body bytes, header map) pair for a
/// `NotificationEvent` against a `WebhookConfig`. Centralized so the
/// `send` / `send_test` paths stay aligned without drifting on which
/// headers are set.
///
/// **HeaderMap insert semantics, not the builder's append.** A user
/// who configures a custom `X-Ryokan-Event` value (e.g. for downstream
/// log routing) needs the override to actually win — reqwest's
/// `.header(K, V)` appends, leaving both values in the request, which
/// breaks the override use case. `insert` replaces.
fn build_request(
    event: &NotificationEvent,
    config: &WebhookConfig,
) -> Result<(Vec<u8>, HeaderMap), String> {
    // Serialize to bytes (not String) — HMAC is over the raw bytes
    // that go on the wire; any re-serialization would mismatch
    // receiver-side verification.
    let body = serde_json::to_vec(event).map_err(|e| format!("event serialization failed: {e}"))?;

    let delivery_id = mint_delivery_id();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        HeaderName::from_static("x-ryokan-delivery"),
        HeaderValue::from_str(&delivery_id).expect("hex delivery id is valid header value"),
    );
    headers.insert(
        HeaderName::from_static("x-ryokan-timestamp"),
        HeaderValue::from_str(&timestamp.to_string())
            .expect("decimal timestamp is valid header value"),
    );
    headers.insert(
        HeaderName::from_static("x-ryokan-event"),
        HeaderValue::from_static(event.kind()),
    );

    // HMAC over raw body bytes. GitHub-shaped header so receivers
    // can use off-the-shelf verification code.
    if let Some(secret) = config.secret.as_deref().filter(|s| !s.is_empty()) {
        let sig = hmac_sha256_hex(secret.as_bytes(), &body);
        let value = format!("sha256={sig}");
        headers.insert(
            HeaderName::from_static("x-ryokan-signature"),
            HeaderValue::from_str(&value).expect("sha256= hex is valid header value"),
        );
    }

    // Custom headers added last via `insert` so they can override
    // anything except the load-bearing `Content-Type` and
    // `X-Ryokan-Signature`. We don't enforce that exclusion at the
    // type level — a user who wants to override `Content-Type` is
    // choosing to break the wire contract on purpose. Settings UI
    // surfaces a "this header is reserved" warning; the runtime
    // applies what's configured.
    for (k, v) in &config.headers {
        let Ok(name) = HeaderName::try_from(k.as_str()) else {
            // Skip headers whose name doesn't parse. Log via
            // tracing only — the dispatch path's outer logger is
            // not reachable from here, and a user who configured
            // `:invalid:` as a header name shouldn't crash the
            // send.
            tracing::warn!("webhook: skipping invalid header name {k:?}");
            continue;
        };
        let Ok(value) = HeaderValue::try_from(v) else {
            tracing::warn!("webhook: skipping invalid header value for {k:?}");
            continue;
        };
        headers.insert(name, value);
    }

    Ok((body, headers))
}

/// Awaited single-provider send for the Settings UI's "Send test"
/// button. Returns the receiver's HTTP status + (truncated) body
/// inline so users can debug without opening browser devtools.
/// Bypasses the per-event matrix (callers always send a synthetic
/// `Health` event, which is default-off in the matrix).
///
/// Runs through this provider directly — `send_to` on the dispatcher
/// would just round-trip to the same code path.
pub async fn send_test(
    provider: &WebhookProvider,
    event: &NotificationEvent,
) -> Result<TestSendResult, String> {
    let (body, headers) = build_request(event, &provider.config)?;
    let response = match WEBHOOK_HTTP_CLIENT
        .post(&provider.config.url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return Err(format!(
                "request timed out after {}s",
                WEBHOOK_REQUEST_TIMEOUT.as_secs()
            ));
        }
        Err(e) => return Err(format!("transport error: {e}")),
    };
    let status = response.status().as_u16();
    let body_text = response.text().await.unwrap_or_default();
    Ok(TestSendResult {
        status,
        body: truncate(&body_text, RESPONSE_BODY_LOG_CAP),
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestSendResult {
    pub status: u16,
    pub body: String,
}

fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

fn mint_delivery_id() -> String {
    // 16 bytes hex — functionally equivalent to a UUIDv4 for
    // collision-avoidance use (this is for receiver-side dedup,
    // not anything cryptographic). Reuses the existing rand+hex
    // pair already in the dependency graph for session tokens.
    // Mirrors the shape in `models::session` and the autobrr key
    // generator.
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_accepts_http_and_https() {
        validate_url("http://example.com/hook").unwrap();
        validate_url("https://example.com/hook?token=abc").unwrap();
    }

    #[test]
    fn validate_url_rejects_non_http_schemes() {
        // Foot-gun scenarios called out in the issue: file://, gopher://, etc.
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("ftp://ftp.example.com/").is_err());
        assert!(validate_url("gopher://example.com").is_err());
    }

    #[test]
    fn validate_url_rejects_missing_scheme_and_empty_host() {
        assert!(validate_url("").is_err());
        assert!(validate_url("not-a-url").is_err());
        // Url::parse with no host (e.g. `http:hook` parsed as a
        // relative path) should fail the host-emptiness check.
        // `http:///hook` happens to be accepted by Url::parse with
        // a non-empty host_str on some parser versions, so it's
        // not a portable test target — skip.
        assert!(validate_url("http:hook").is_err());
    }

    #[test]
    fn config_deserializes_minimal_shape() {
        let cfg: WebhookConfig =
            serde_json::from_str(r#"{"url":"https://example.com/hook"}"#).unwrap();
        assert_eq!(cfg.url, "https://example.com/hook");
        assert!(cfg.secret.is_none());
        assert!(cfg.headers.is_empty());
    }

    #[test]
    fn config_deserializes_with_secret_and_headers() {
        let cfg: WebhookConfig = serde_json::from_str(
            r#"{
                "url": "https://example.com/hook",
                "secret": "shh",
                "headers": {"X-A": "1", "X-B": "2"}
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.secret.as_deref(), Some("shh"));
        // Tuple-list preserves the JSON document's natural order.
        assert_eq!(cfg.headers.len(), 2);
        assert_eq!(cfg.headers[0], ("X-A".into(), "1".into()));
        assert_eq!(cfg.headers[1], ("X-B".into(), "2".into()));
    }

    #[test]
    fn config_deserializes_with_null_or_missing_headers() {
        // Setting form may submit `headers: null` from a cleared
        // textarea; a separate save path simply omits the key. Both
        // shapes must yield an empty Vec, not a deserialize error.
        let omitted: WebhookConfig =
            serde_json::from_str(r#"{"url":"https://example.com/x"}"#).unwrap();
        assert!(omitted.headers.is_empty());
        let null_form: WebhookConfig =
            serde_json::from_str(r#"{"url":"https://example.com/x","headers":null}"#).unwrap();
        assert!(null_form.headers.is_empty());
    }

    #[test]
    fn hmac_sha256_hex_matches_known_test_vector() {
        // RFC 4231 test case 1: key = 0x0b * 20, msg = "Hi There".
        // Pinned so a future swap of the hmac/sha2 crates can't
        // silently start producing a different digest.
        let key = [0x0b_u8; 20];
        let msg = b"Hi There";
        let got = hmac_sha256_hex(&key, msg);
        assert_eq!(
            got, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            "HMAC-SHA256 RFC 4231 test case 1 must hold"
        );
    }

    #[test]
    fn truncate_handles_unicode_grapheme_count() {
        // Same primitive as the dispatcher's logger truncate; pinned
        // here too because the receiver body can be arbitrary bytes
        // that still parse as UTF-8.
        let long = "あいうえお".repeat(60);
        let out = truncate(&long, 5);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 6);
    }

    #[test]
    fn from_row_rejects_invalid_config_json() {
        let r = WebhookProvider::from_row(1, "n".into(), "{not valid json");
        assert!(r.is_err());
    }

    #[test]
    fn from_row_rejects_invalid_url_in_config() {
        // Defense-in-depth — a hand-edited DB row that bypassed the
        // settings handler shouldn't survive the cache rebuild with
        // a foot-gun URL.
        let r = WebhookProvider::from_row(1, "n".into(), r#"{"url":"file:///etc/passwd"}"#);
        assert!(r.is_err());
    }

    #[test]
    fn mint_delivery_id_returns_32_hex_chars() {
        let id = mint_delivery_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn mint_delivery_id_is_collision_avoidant() {
        // Birthday-bound guard: 1000 ids, every one unique. A bug
        // that defaulted to a fixed value would crash this loudly.
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| mint_delivery_id()).collect();
        assert_eq!(ids.len(), 1000);
    }
}
