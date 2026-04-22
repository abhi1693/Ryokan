use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;

const NYAA_BASE: &str = "https://nyaa.si";

/// Process-global `reqwest::Client` for Nyaa search requests. A fresh
/// `Client` per search throws away keep-alive connections and forces a
/// new TLS handshake every call — Nyaa gets hit many times a minute
/// between RSS sync, auto-search, upgrade sweeps, and interactive
/// search, and the per-request client was needless overhead. A 30-second
/// per-call timeout caps the damage from a single hung connection so
/// the outer RSS/upgrade-search timeouts aren't the only backstop.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the Nyaa search reqwest client should not fail")
});

mod scraper;

use scraper::{parse_results, parse_view_page, sanitize_query_for_nyaa};

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SearchResult {
    pub title: String,
    pub link: String,
    pub magnet: String,
    pub torrent: String,
    pub size: String,
    pub size_bytes: i64,
    pub seeders: i32,
    pub leechers: i32,
    pub downloads: i32,
    /// Release group extracted via anitomy. Kept as `String` for backward-
    /// compat with the old ad-hoc bracket parse; empty string means "no
    /// group detected."
    pub group: String,
    /// Resolution as a bare digit string ("1080", "720", …) or empty. Kept
    /// for UI callers that render just the resolution tag; richer callers
    /// should use `quality_label` which encodes source+resolution+sub-tier.
    pub resolution: String,
    /// Pre-computed Sonarr-parity label (`WEB-1080p`, `BD-1080p Remux`,
    /// etc.) produced from the same [`crate::services::source::ClassificationResult::label`]
    /// logic as the grab-side pipeline, so the value the user sees in
    /// interactive search equals the value persisted once grabbed.
    /// Empty when neither source nor resolution was determined.
    pub quality_label: String,
    /// Source enum as a string (`"Web"`, `"BluRay"`, …) or empty when
    /// unknown. Mirrors `Source::as_str()` exactly.
    pub source: String,
    /// Web sub-variant (`"WEB-DL"`, `"WEBRip"`, or empty for Unknown).
    /// Only meaningful when `source == "Web"`.
    pub web_kind: String,
    pub is_remux: bool,
    pub is_bdmv: bool,
    pub is_batch: bool,
    pub is_trusted: bool,
    pub score: i32,
    pub info_hash: String,
    /// #1.3.0 — per-component breakdown of the base score (what rules
    /// fired, with what delta and a human-readable detail). Populated
    /// by the search scraper alongside `score` so the UI can render a
    /// "why this score" expansion on search results. Note this covers
    /// only the base scoring rules; Custom Format contributions are
    /// tracked separately at the auto-search site.
    #[serde(default)]
    pub score_breakdown: Vec<crate::services::scoring::ScoreComponent>,
}

#[derive(Clone)]
pub struct SearchOptions {
    pub query: String,
    pub category: String,
    pub filter: String,
    pub user: String,
    pub preferred_groups: Vec<String>,
    pub preferred_resolution: String,
    pub prefer_subs: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            category: "1_0".to_string(), // Anime - All
            filter: "0".to_string(),
            user: String::new(),
            preferred_groups: Vec::new(),
            preferred_resolution: "1080".to_string(),
            prefer_subs: true,
        }
    }
}

/// Result of a paginated search.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub page: i32,
    pub has_next: bool,
}

/// Search Nyaa by scraping the HTML results page.
pub async fn search(opts: &SearchOptions, page: i32) -> Result<SearchResponse, String> {
    let sanitized_query = sanitize_query_for_nyaa(&opts.query);
    let mut url = format!(
        "{}/?f={}&c={}&q={}&p={}",
        NYAA_BASE,
        opts.filter,
        opts.category,
        urlencoding::encode(&sanitized_query),
        page
    );

    if !opts.user.is_empty() {
        url = format!(
            "{}/user/{}?f={}&c={}&q={}&p={}",
            NYAA_BASE,
            urlencoding::encode(&opts.user),
            opts.filter,
            opts.category,
            urlencoding::encode(&sanitized_query),
            page
        );
    }

    let html = HTTP_CLIENT
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Nyaa request failed: {}", e))?
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    let (results, has_next) = parse_results(&html, opts);
    Ok(SearchResponse {
        results,
        page,
        has_next,
    })
}

/// Enrich already-parsed search results with Layer 3 (group identity
/// table) signals. Walks each result whose filename classifier didn't
/// produce a source, looks up the group in `group_source_map`, and fills
/// in `source` / `quality_label` when the group is known.
///
/// No-op for results that already have a filename-derived source — the
/// filename is more specific than the group map (e.g. a SubsPlease
/// release explicitly tagged "BluRay" remains BluRay, even though the
/// group map says SubsPlease == Web).
pub async fn enrich_results_with_group_map(db: &sqlx::SqlitePool, results: &mut [SearchResult]) {
    use crate::services::source::{Resolution, Source};
    use crate::services::source_groups::classify_group;

    // Small per-batch cache so we only hit the DB once per unique group
    // across a typical 75-row result page.
    let mut seen: std::collections::HashMap<
        String,
        Option<(Source, crate::services::source::WebKind)>,
    > = std::collections::HashMap::new();

    for r in results.iter_mut() {
        if !r.source.is_empty() || r.group.is_empty() {
            continue;
        }
        let group_key = r.group.to_ascii_lowercase();
        let group_hint = if let Some(cached) = seen.get(&group_key) {
            *cached
        } else {
            let looked_up = classify_group(db, &r.group)
                .await
                .map(|cls| (cls.evidence.source, cls.web_kind));
            seen.insert(group_key, looked_up);
            looked_up
        };

        if let Some((src, web_kind)) = group_hint {
            r.source = src.as_str().to_string();
            // Rebuild quality_label now that source is known. Resolution
            // string is bare digits ("1080"); translate back into the
            // Resolution enum for label formatting.
            let res_enum = if r.resolution.is_empty() {
                Resolution::Unknown
            } else {
                Resolution::from_str(&format!("{}p", r.resolution))
            };
            // Web releases unify the WebDl and bare-WEB sub-tiers into
            // a single "WEB" label (issue #48) — matches
            // `ClassificationResult::label()`. WebRip stays distinct
            // because it's the lower-quality sub-tier power users want
            // to spot.
            let source_label = match src {
                Source::Web => match web_kind {
                    crate::services::source::WebKind::WebRip => "WEBRip".to_string(),
                    crate::services::source::WebKind::Unknown
                    | crate::services::source::WebKind::WebDl => "WEB".to_string(),
                },
                Source::BluRay => "BD".to_string(),
                other => other.as_str().to_string(),
            };
            r.quality_label = match (source_label.as_str(), res_enum) {
                ("", Resolution::Unknown) => String::new(),
                (s, Resolution::Unknown) => s.to_string(),
                ("", r) => r.as_str().to_string(),
                (s, r) => format!("{}-{}", s, r.as_str()),
            };
        }
    }
}

/// Extract the 40-char lowercase-hex info-hash from a magnet URI.
/// Returns an empty string when the magnet doesn't carry a `btih:`
/// URN. Handles both the 40-char hex form and the 32-char
/// base32-encoded form — the latter gets canonicalized to hex so
/// every downstream comparison (SeaDex hash set membership,
/// `grabbed_torrents.info_hash` uniqueness) can assume a single
/// representation. BTIH URN matching is case-insensitive.
pub(crate) fn extract_hash(magnet: &str) -> String {
    // BTIH URN is case-insensitive (`urn:btih:` and `urn:BTIH:` both
    // occur in the wild) — match on the lowercased copy.
    //
    // 40-char hex hashes: lowercase them, done.
    //
    // 32-char base32 hashes (RFC 4648 alphabet A-Z + 2-7,
    // case-insensitive per the RFC): decode to the 20 raw bytes of
    // the info-hash and re-emit as lowercase hex. We canonicalize at
    // the source so `grabbed_torrents.hash` — Ryokan's dedup key — is
    // one shape regardless of the magnet's encoding. Non-qBit clients
    // (Deluge, Transmission) normalize hashes to lowercase hex
    // internally; leaving a base32 string in the DB would mean our
    // stored hash didn't match the client's reported hash, silently
    // breaking dedup the first time a base32 magnet landed under a
    // non-qBit client. See #63 Phase 0.
    let lower = magnet.to_ascii_lowercase();
    let Some(pos) = lower.find("btih:") else {
        return String::new();
    };
    let payload = &magnet[pos + 5..];
    let end = payload.find('&').unwrap_or(payload.len());
    let hash = &payload[..end];

    match hash.len() {
        40 => hash.to_ascii_lowercase(),
        32 => match base32_decode_infohash(hash) {
            Some(bytes) => hex::encode(bytes),
            // Malformed 32-char string — not valid RFC 4648 base32.
            // Fall through to lowercase so we return *something*
            // rather than silently swallowing; callers that treat ""
            // as "no hash" stay unaffected, and a lowercased garbage
            // string is at least deterministic.
            None => hash.to_ascii_lowercase(),
        },
        // Any other length is a malformed BTIH — not a valid
        // info-hash. Lowercase fallthrough preserves the prior
        // behaviour of returning *something* to downstream code.
        _ => hash.to_ascii_lowercase(),
    }
}

/// Decode a 32-char RFC 4648 base32 info-hash to 20 raw bytes.
/// Case-insensitive (accepts both `ABCDEF…` and `abcdef…`). Returns
/// None for any input that isn't exactly 32 chars of A-Z/a-z/2-7.
pub(crate) fn base32_decode_infohash(s: &str) -> Option<[u8; 20]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 20];
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    let mut i = 0;
    for c in s.bytes() {
        let v: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32,
            b'2'..=b'7' => (c - b'2') as u32 + 26,
            _ => return None,
        };
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[i] = ((buf >> bits) & 0xff) as u8;
            i += 1;
        }
    }
    // 32 chars × 5 bits = 160 bits = 20 bytes exactly, no residual.
    debug_assert_eq!(bits, 0);
    debug_assert_eq!(i, 20);
    Some(out)
}

pub async fn fetch_view_result(
    view_url: &str,
    opts: &SearchOptions,
) -> Result<SearchResult, String> {
    let html = HTTP_CLIENT
        .get(view_url)
        .send()
        .await
        .map_err(|e| format!("Nyaa view fetch failed: {}", e))?
        .text()
        .await
        .map_err(|e| format!("Failed to read view body: {}", e))?;

    parse_view_page(&html, view_url, opts).ok_or_else(|| "Nyaa view page parse failed".to_string())
}
