use serde::{Deserialize, Serialize};
use crate::services::html::sanitize_rich_description;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::services::jikan;

const ANILIST_API: &str = "https://graphql.anilist.co";

/// TTL for the search result cache. Short enough to stay fresh, long enough to
/// absorb bursts of repeat queries (which is what actually hammers AniList/Jikan
/// during testing or when a user re-searches the same title).
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(60);

/// How long to treat AniList as unavailable after a 429/5xx. If AniList sends a
/// `Retry-After` header we use that value instead (capped at 5 minutes).
const ANILIST_COOLDOWN_DEFAULT: Duration = Duration::from_secs(60);
const ANILIST_COOLDOWN_MAX: Duration = Duration::from_secs(300);

/// In-memory cache TTL for anime detail responses (15 minutes).
const DETAIL_CACHE_TTL_SECS: u64 = 15 * 60;

/// Maximum number of entries in the in-memory detail cache. When exceeded,
/// expired entries are evicted first; if still over limit the oldest entry
/// is removed.
const DETAIL_CACHE_MAX_ENTRIES: usize = 500;

/// In-memory cache for AniList detail responses to avoid rate limiting.
struct CacheEntry {
    detail: AnimeDetail,
    fetched_at: Instant,
}

static DETAIL_CACHE: LazyLock<RwLock<HashMap<i64, CacheEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Shared reqwest client. Each `reqwest::Client::new()` call previously
/// rebuilt the TLS context and connection pool from scratch — wasteful
/// across the search / detail / fallback paths that all hit
/// graphql.anilist.co. Using a single Lazy client lets the pool reuse
/// connections across calls. No timeout configured here — callers own
/// retry/cooldown semantics that interact in non-obvious ways with a
/// blanket per-request timeout.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

type SearchCacheEntry = (Instant, Vec<AnimeEntry>);

/// Search result cache, keyed on (provider-mode, normalized query).
/// Provider-mode is "al" for the normal AniList-first path and "mal" for the
/// force_mal_fallback path; we keep them separate because they return different
/// `source` fields per entry and the frontend displays the distinction.
static SEARCH_CACHE: LazyLock<StdMutex<HashMap<String, SearchCacheEntry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Monotonically-set "AniList is in cooldown until Instant". Consulted at the
/// top of every search so that once we've learned AniList is rate-limiting us,
/// we stop wasting a round-trip per search (which was dragging Jikan into the
/// rate-limit bucket too).
static ANILIST_COOLDOWN_UNTIL: LazyLock<StdMutex<Option<Instant>>> =
    LazyLock::new(|| StdMutex::new(None));

fn normalize_search_key(force_fallback: bool, query: &str) -> String {
    let folded: String = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{}::{}", if force_fallback { "mal" } else { "al" }, folded)
}

fn search_cache_get(key: &str) -> Option<Vec<AnimeEntry>> {
    let now = Instant::now();
    let mut cache = SEARCH_CACHE.lock().ok()?;
    if let Some((fetched_at, results)) = cache.get(key)
        && now.duration_since(*fetched_at) <= SEARCH_CACHE_TTL {
            return Some(results.clone());
        }
    cache.remove(key);
    None
}

fn search_cache_put(key: String, results: Vec<AnimeEntry>) {
    if let Ok(mut cache) = SEARCH_CACHE.lock() {
        // Bound the cache. Simple heuristic — if we're >200 entries, drop expired
        // ones; if still too big, just clear. Search queries are long-tail anyway.
        if cache.len() > 200 {
            let now = Instant::now();
            cache.retain(|_, (t, _)| now.duration_since(*t) <= SEARCH_CACHE_TTL);
            if cache.len() > 200 {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), results));
    }
}

fn anilist_cooldown_active() -> bool {
    if let Ok(guard) = ANILIST_COOLDOWN_UNTIL.lock()
        && let Some(until) = *guard {
            return Instant::now() < until;
        }
    false
}

fn set_anilist_cooldown(retry_after_secs: Option<u64>, default_dur: Duration) {
    let dur = retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or(default_dur)
        .min(ANILIST_COOLDOWN_MAX);
    if let Ok(mut guard) = ANILIST_COOLDOWN_UNTIL.lock() {
        *guard = Some(Instant::now() + dur);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnimeEntry {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    pub source: String,
}

/// Search AniList for anime by title, falling back to MAL/Jikan if AniList 403s.
#[allow(dead_code)]
pub async fn search_anime(query: &str) -> Result<Vec<AnimeEntry>, String> {
    search_anime_with_options(query, false).await
}

pub async fn search_anime_with_options(query: &str, force_mal_fallback: bool) -> Result<Vec<AnimeEntry>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Cache lookup — skip all upstream work for repeat queries within the TTL.
    let cache_key = normalize_search_key(force_mal_fallback, query);
    if let Some(cached) = search_cache_get(&cache_key) {
        tracing::debug!("anilist search cache hit for {:?} ({} results)", query, cached.len());
        return Ok(cached);
    }

    if force_mal_fallback {
        let results = fallback_jikan(query, None).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    // 2. If AniList is known to be rate-limited, don't bother hitting it — go
    //    straight to Jikan. This is the key fix for the "both APIs rate-limited
    //    at once" symptom: previously every search during the 60s AL cooldown
    //    still pinged AL, got another 429, then called Jikan, burning Jikan's
    //    (stricter) rate-limit budget alongside.
    if anilist_cooldown_active() {
        tracing::debug!(
            "anilist search skipping AniList for {:?} (still in cooldown)",
            query
        );
        let results = fallback_jikan(
            query,
            Some("AniList rate-limited (skipped during cooldown)".to_string()),
        ).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    let gql = serde_json::json!({
        "query": r#"
            query ($search: String) {
                Page(page: 1, perPage: 10) {
                    media(search: $search, type: ANIME, sort: SEARCH_MATCH) {
                        id
                        idMal
                        title {
                            romaji
                            english
                            native
                        }
                        coverImage {
                            large
                        }
                        format
                        status
                        episodes
                    }
                }
            }
        "#,
        "variables": { "search": query }
    });

    let client = &*HTTP_CLIENT;
    let resp = match client
        .post(ANILIST_API)
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "AniList request failed for query {:?}: {}; falling back to Jikan/MAL",
                query, e
            );
            let results = fallback_jikan(
                query,
                Some(format!("AniList unreachable: {}", e)),
            ).await?;
            search_cache_put(cache_key, results.clone());
            return Ok(results);
        }
    };

    let status = resp.status();

    // Silently fall back to Jikan/MAL on transient AniList outages:
    //   403 — Cloudflare challenge / geo-block
    //   429 — rate limit (30 req/min anon)
    //   5xx — upstream outage
    // These are the cases where the user's search should just Work via a
    // fallback provider rather than surfacing a cryptic HTTP error.
    if status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        tracing::warn!(
            "AniList search HTTP {} for query {:?} (retry-after={:?}); falling back to Jikan/MAL",
            status, query, retry_after_secs
        );
        // Start a cooldown so subsequent searches in this window skip
        // AL entirely. 403 (Cloudflare challenge) is the most common
        // AniList outage mode; without this branch, every request kept
        // round-tripping through AL just to bounce on the 403 again
        // before falling back to Jikan. Cloudflare doesn't include
        // Retry-After, so pick a longer default — 60s rarely outlasts
        // a real challenge — and let ANILIST_COOLDOWN_MAX cap it.
        let default_cooldown = if status == reqwest::StatusCode::FORBIDDEN {
            Duration::from_secs(300)
        } else {
            ANILIST_COOLDOWN_DEFAULT
        };
        set_anilist_cooldown(retry_after_secs, default_cooldown);
        let reason = match status.as_u16() {
            429 => format!(
                "AniList rate-limited{}",
                retry_after_secs
                    .map(|r| format!(" (retry in {}s)", r))
                    .unwrap_or_default()
            ),
            403 => "AniList blocked our request (Cloudflare challenge)".to_string(),
            code => format!("AniList upstream error (HTTP {})", code),
        };
        let results = fallback_jikan(query, Some(reason)).await?;
        search_cache_put(cache_key, results.clone());
        return Ok(results);
    }

    // Read the body as text first so a non-JSON error body (common on 4xx/5xx)
    // produces a useful error instead of "Failed to parse AniList response".
    let body_text = resp
        .text()
        .await
        .map_err(|e| format!("AniList response read failed (HTTP {}): {}", status, e))?;

    let body: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(parse_err) => {
            if !status.is_success() {
                let snippet: String = body_text.chars().take(200).collect();
                return Err(format!("AniList search failed (HTTP {}): {}", status, snippet.trim()));
            }
            return Err(format!("Failed to parse AniList response: {}", parse_err));
        }
    };

    if !status.is_success() {
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!("AniList search failed (HTTP {}): {}", status, msg));
    }

    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!("AniList search failed: {}", msg));
    }

    let media = match body["data"]["Page"]["media"].as_array() {
        Some(arr) => arr,
        None => {
            // Schema mismatch — `data.Page.media` is missing or not an
            // array. Don't cache the empty result here: a legitimate
            // 0-hit search hits the Some branch with an empty arr and
            // *does* get cached at line 317 below. Caching the
            // schema-mismatch case would lock us out of fresh requests
            // for SEARCH_CACHE_TTL even after AniList recovers.
            tracing::warn!(
                target: "ryokan::anilist",
                query = %query,
                "AniList response missing data.Page.media; not caching empty result"
            );
            return Ok(Vec::new());
        }
    };

    let entries: Vec<AnimeEntry> = media
        .iter()
        .map(|m| AnimeEntry {
            id: m["id"].as_i64().unwrap_or(0),
            id_mal: m["idMal"].as_i64(),
            title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
            title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
            title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
            cover_url: m["coverImage"]["large"].as_str().unwrap_or("").to_string(),
            format: m["format"].as_str().unwrap_or("").to_string(),
            status: m["status"].as_str().unwrap_or("").to_string(),
            status_display: prettify_status(m["status"].as_str().unwrap_or("")),
            episodes: m["episodes"].as_i64().map(|e| e as i32),
            season_year: m["seasonYear"].as_i64().map(|y| y as i32),
            source: "anilist".to_string(),
        })
        .collect();

    search_cache_put(cache_key, entries.clone());
    Ok(entries)
}

/// Shared fallback helper used when AniList is unavailable or force_mal_fallback is set.
/// Tries Jikan (MAL-backed). If Jikan also fails, `compose_search_error` builds a
/// user-friendly combined message for the frontend.
async fn fallback_jikan(
    query: &str,
    anilist_reason: Option<String>,
) -> Result<Vec<AnimeEntry>, String> {
    match jikan::search_anime(query).await {
        Ok(results) => Ok(results),
        Err(jikan_err) => Err(compose_search_error(anilist_reason.as_deref(), &jikan_err)),
    }
}

/// Produce a clean, human-readable error message from an AniList failure reason
/// (optional — e.g. "AniList rate-limited (retry in 28s)") and a Jikan failure
/// reason. Callers see something like:
///   "Both AniList and Jikan/MAL are rate-limited right now. Try again in ~30s."
/// instead of a raw JSON dump concatenation.
fn compose_search_error(anilist_reason: Option<&str>, jikan_err: &str) -> String {
    let al_rate_limited = anilist_reason
        .map(|r| r.contains("rate-limited") || r.contains("429"))
        .unwrap_or(false);
    let jikan_rate_limited = jikan_err.contains("rate-limited") || jikan_err.contains("429");

    if al_rate_limited && jikan_rate_limited {
        // Try to surface the AL retry hint if we parsed one earlier.
        let hint = anilist_reason
            .and_then(|r| {
                let start = r.find("retry in ")?;
                let tail = &r[start + "retry in ".len()..];
                let end = tail.find(')').unwrap_or(tail.len());
                Some(tail[..end].to_string())
            })
            .map(|s| format!(" Try again in ~{}.", s))
            .unwrap_or_else(|| " Try again in a minute.".to_string());
        return format!("Both AniList and Jikan/MAL are rate-limited right now.{}", hint);
    }

    match anilist_reason {
        Some(al) => format!("{}. MAL/Jikan fallback also failed: {}", al, jikan_err),
        None => format!("MAL/Jikan search failed: {}", jikan_err),
    }
}


pub async fn find_anime_by_mal_id(mal_id: i64) -> Result<Option<AnimeEntry>, String> {
    let gql = serde_json::json!({
        "query": r#"
            query ($idMal: Int) {
                Media(idMal: $idMal, type: ANIME) {
                    id
                    idMal
                    title {
                        romaji
                        english
                        native
                    }
                    coverImage {
                        large
                    }
                    format
                    status
                    episodes
                    seasonYear
                }
            }
        "#,
        "variables": { "idMal": mal_id }
    });

    let client = &*HTTP_CLIENT;
    let resp = client
        .post(ANILIST_API)
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
        .map_err(|e| format!("AniList request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse AniList response: {}", e))?;

    if !status.is_success() {
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!("AniList MAL lookup failed (HTTP {}): {}", status, msg));
    }

    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!("AniList MAL lookup failed: {}", msg));
    }

    let m = &body["data"]["Media"];
    if m.is_null() {
        return Ok(None);
    }

    Ok(Some(AnimeEntry {
        id: m["id"].as_i64().unwrap_or(0),
        id_mal: m["idMal"].as_i64(),
        title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
        title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
        title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
        cover_url: m["coverImage"]["large"].as_str().unwrap_or("").to_string(),
        format: m["format"].as_str().unwrap_or("").to_string(),
        status: m["status"].as_str().unwrap_or("").to_string(),
        status_display: prettify_status(m["status"].as_str().unwrap_or("")),
        episodes: m["episodes"].as_i64().map(|e| e as i32),
        season_year: m["seasonYear"].as_i64().map(|y| y as i32),
        source: "anilist".to_string(),
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RelatedEntry {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub relation_type: String,
    pub season_year: Option<i32>,
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StreamingEpisode {
    pub title: String,
    pub thumbnail: String,
    pub url: String,
    pub site: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnimeDetail {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub banner_url: String,
    pub format: String,
    pub status: String,
    pub status_display: String,
    pub episodes: Option<i32>,
    pub duration: Option<i32>,
    pub season: String,
    pub season_year: Option<i32>,
    /// AniList `endDate.year` when the show has finished. `#[serde(default)]`
    /// so cached JSON blobs from before this field existed deserialize
    /// cleanly to `None`. Consumed by Layer 4 temporal inference.
    #[serde(default)]
    pub end_year: Option<i32>,
    pub description: String,
    pub genres: Vec<String>,
    pub average_score: Option<i32>,
    pub average_score_display: Option<String>,
    pub score_is_ten_point: bool,
    pub score_class: String,
    pub next_airing_episode: Option<i32>,
    pub next_airing_at: Option<i64>,
    pub synonyms: Vec<String>,
    pub streaming_episodes: Vec<StreamingEpisode>,
    pub relations: Vec<RelatedEntry>,
}

impl AnimeDetail {
    /// Effective episode count for rendering, episode cache building, and
    /// monitoring. AniList reports `episodes: null` for currently-airing
    /// series because the final count isn't known yet, so we fall back to
    /// `nextAiringEpisode - 1` (the number of episodes that have already
    /// aired). Without this every airing show looks like it has zero
    /// episodes, which breaks the episode list and the monitoring UI.
    pub fn effective_episode_count(&self) -> i32 {
        match self.episodes.unwrap_or(0) {
            0 => self.next_airing_episode.map(|n| (n - 1).max(0)).unwrap_or(0),
            n => n,
        }
    }

    /// True when the series has finished airing (or was cancelled) per any
    /// of the three metadata providers' vocabularies. AniList uses
    /// `FINISHED` / `CANCELLED`, Jikan normalizes "Finished Airing" →
    /// `FINISHED_AIRING`, and Kitsu uses `FINISHED`. Without this helper
    /// the callsites that just compared against the literal `"FINISHED"`
    /// string silently misclassified every Jikan-fed series as "still
    /// airing", breaking the finished-mode BD probe and the 2-year
    /// sequel-rejection filter whenever the AniList fallback kicked in.
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "FINISHED" | "FINISHED_AIRING" | "CANCELLED"
        )
    }
}

fn prettify_status(status: &str) -> String {
    status.replace('_', " ")
}

fn score_class(score: Option<i32>, is_ten_point: bool) -> String {
    let class = if is_ten_point {
        match score {
            Some(s) if s >= 9 => "tag-score-purple",
            Some(s) if s >= 7 => "tag-score-green",
            Some(s) if s > 5 => "tag-score-yellow",
            _ => "tag-score-red",
        }
    } else {
        match score {
            Some(s) if s >= 85 => "tag-score-purple",
            Some(s) if s >= 75 => "tag-score-green",
            Some(s) if s > 65 => "tag-score-yellow",
            _ => "tag-score-red",
        }
    };
    class.to_string()
}

pub async fn get_anime_detail(id: i64) -> Result<AnimeDetail, String> {
    get_anime_detail_with_options(id, None, false).await
}

pub async fn get_anime_detail_with_options(id: i64, mal_id_hint: Option<i64>, force_mal_fallback: bool) -> Result<AnimeDetail, String> {
    if id < 0 {
        return jikan::get_anime_detail_cached(-id).await;
    }
    if force_mal_fallback
        && let Some(mid) = mal_id_hint {
            return jikan::get_anime_detail_cached(mid).await;
        }

    {
        let cache = DETAIL_CACHE.read().await;
        if let Some(entry) = cache.get(&id)
            && entry.fetched_at.elapsed().as_secs() < DETAIL_CACHE_TTL_SECS {
                return Ok(entry.detail.clone());
            }
    }

    let detail = fetch_anime_detail(id).await?;

    {
        let mut cache = DETAIL_CACHE.write().await;
        cache.insert(id, CacheEntry {
            detail: detail.clone(),
            fetched_at: Instant::now(),
        });
        // Evict stale/oldest entries when the cache grows too large.
        if cache.len() > DETAIL_CACHE_MAX_ENTRIES {
            let expired: Vec<i64> = cache
                .iter()
                .filter(|(_, e)| e.fetched_at.elapsed().as_secs() >= DETAIL_CACHE_TTL_SECS)
                .map(|(k, _)| *k)
                .collect();
            for k in &expired {
                cache.remove(k);
            }
            // If still over limit, drop the oldest entry.
            if cache.len() > DETAIL_CACHE_MAX_ENTRIES
                && let Some((&oldest_key, _)) = cache.iter().min_by_key(|(_, e)| e.fetched_at) {
                    cache.remove(&oldest_key);
                }
        }
    }

    Ok(detail)
}

async fn fetch_anime_detail(id: i64) -> Result<AnimeDetail, String> {
    let gql = serde_json::json!({
        "query": r#"
            query ($id: Int) {
                Media(id: $id, type: ANIME) {
                    id
                    idMal
                    title { romaji english native }
                    synonyms
                    coverImage { large extraLarge }
                    bannerImage
                    format
                    status
                    episodes
                    duration
                    season
                    seasonYear
                    endDate { year }
                    description(asHtml: true)
                    genres
                    averageScore
                    nextAiringEpisode {
                        episode
                        airingAt
                    }
                    streamingEpisodes {
                        title
                        thumbnail
                        url
                        site
                    }
                    relations {
                        edges {
                            relationType(version: 2)
                            node {
                                id
                                idMal
                                title { romaji english native }
                                format
                                status
                                episodes
                                coverImage { large }
                                type
                                seasonYear
                            }
                        }
                    }
                }
            }
        "#,
        "variables": { "id": id }
    });

    let client = &*HTTP_CLIENT;
    let resp = client
        .post(ANILIST_API)
        .header("User-Agent", "Ryokan/0.1")
        .json(&gql)
        .send()
        .await
        .map_err(|e| format!("AniList request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse AniList response: {}", e))?;

    if !status.is_success() {
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!("AniList detail failed (HTTP {}): {}", status, msg));
    }

    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!("AniList detail failed: {}", msg));
    }

    let m = &body["data"]["Media"];
    if m.is_null() {
        return Err("Anime not found".into());
    }

    let streaming_episodes = m["streamingEpisodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|ep| StreamingEpisode {
                    title: ep["title"].as_str().unwrap_or("").to_string(),
                    thumbnail: ep["thumbnail"].as_str().unwrap_or("").to_string(),
                    url: ep["url"].as_str().unwrap_or("").to_string(),
                    site: ep["site"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let relations = m["relations"]["edges"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|edge| {
                    let node = &edge["node"];
                    Some(RelatedEntry {
                        id: node["id"].as_i64()?,
                        id_mal: node["idMal"].as_i64(),
                        title_romaji: node["title"]["romaji"].as_str().unwrap_or("").to_string(),
                        title_english: node["title"]["english"].as_str().unwrap_or("").to_string(),
                        title_native: node["title"]["native"].as_str().unwrap_or("").to_string(),
                        cover_url: node["coverImage"]["large"].as_str().unwrap_or("").to_string(),
                        format: node["format"].as_str().unwrap_or("").to_string(),
                        status: node["status"].as_str().unwrap_or("").to_string(),
                        status_display: prettify_status(node["status"].as_str().unwrap_or("")),
                        episodes: node["episodes"].as_i64().map(|e| e as i32),
                        relation_type: edge["relationType"].as_str().unwrap_or("").to_string(),
                        season_year: node["seasonYear"].as_i64().map(|y| y as i32),
                        media_type: node["type"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(AnimeDetail {
        id: m["id"].as_i64().unwrap_or(0),
        id_mal: m["idMal"].as_i64(),
        title_romaji: m["title"]["romaji"].as_str().unwrap_or("").to_string(),
        title_english: m["title"]["english"].as_str().unwrap_or("").to_string(),
        title_native: m["title"]["native"].as_str().unwrap_or("").to_string(),
        cover_url: m["coverImage"]["extraLarge"]
            .as_str()
            .or_else(|| m["coverImage"]["large"].as_str())
            .unwrap_or("")
            .to_string(),
        banner_url: m["bannerImage"].as_str().unwrap_or("").to_string(),
        format: m["format"].as_str().unwrap_or("").to_string(),
        status: m["status"].as_str().unwrap_or("").to_string(),
        episodes: m["episodes"].as_i64().map(|e| e as i32),
        duration: m["duration"].as_i64().map(|d| d as i32),
        season: m["season"].as_str().unwrap_or("").to_string(),
        season_year: m["seasonYear"].as_i64().map(|y| y as i32),
        end_year: m["endDate"]["year"].as_i64().map(|y| y as i32),
        description: sanitize_rich_description(m["description"].as_str().unwrap_or(""), true),
        genres: m["genres"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|g| g.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
        average_score: m["averageScore"].as_i64().map(|s| s as i32),
        average_score_display: m["averageScore"].as_i64().map(|s| format!("{}%", s)),
        score_is_ten_point: false,
        score_class: score_class(m["averageScore"].as_i64().map(|s| s as i32), false),
        status_display: prettify_status(m["status"].as_str().unwrap_or("")),
        next_airing_episode: m["nextAiringEpisode"]["episode"].as_i64().map(|e| e as i32),
        next_airing_at: m["nextAiringEpisode"]["airingAt"].as_i64(),
        synonyms: m["synonyms"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|s| s.as_str().map(|v| v.to_string())).collect())
            .unwrap_or_default(),
        streaming_episodes,
        relations,
    })
}

fn extract_graphql_error(body: &serde_json::Value) -> Option<String> {
    body["errors"]
        .as_array()
        .and_then(|errs| errs.first())
        .and_then(|err| err["message"].as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_search_key_folds_whitespace_and_case() {
        assert_eq!(
            normalize_search_key(false, "  Jojo  Part  3 "),
            "al::jojo part 3"
        );
        assert_eq!(
            normalize_search_key(true, "\tFrieren\n"),
            "mal::frieren"
        );
    }

    #[test]
    fn normalize_search_key_separates_al_from_mal_modes() {
        assert_ne!(
            normalize_search_key(false, "Bleach"),
            normalize_search_key(true, "Bleach")
        );
    }

    #[test]
    fn compose_error_when_both_rate_limited_suggests_retry() {
        let msg = compose_search_error(
            Some("AniList rate-limited (retry in 28s)"),
            "Jikan rate-limited (HTTP 429): You are being rate-limited",
        );
        assert!(msg.contains("Both AniList and Jikan/MAL are rate-limited"), "msg was: {}", msg);
        assert!(msg.contains("28s"), "retry hint lost: {}", msg);
    }

    #[test]
    fn compose_error_falls_back_when_only_one_rate_limited() {
        let msg = compose_search_error(
            Some("AniList rate-limited (retry in 28s)"),
            "Jikan unreachable: connection refused",
        );
        assert!(msg.starts_with("AniList rate-limited"), "msg was: {}", msg);
        assert!(msg.contains("connection refused"), "jikan detail lost: {}", msg);
        assert!(!msg.contains("Both AniList and Jikan/MAL"), "wrong branch: {}", msg);
    }

    #[test]
    fn compose_error_without_anilist_reason_uses_mal_prefix() {
        let msg = compose_search_error(None, "Jikan HTTP 500: upstream down");
        assert!(msg.starts_with("MAL/Jikan search failed"), "msg was: {}", msg);
        assert!(msg.contains("upstream down"));
    }

    #[test]
    fn search_cache_roundtrips_and_expires_on_ttl_mismatch() {
        // We can't sleep for 60s in tests, but we can validate that distinct
        // keys don't collide and that a put/get returns the same Vec.
        let key = normalize_search_key(false, "test query unique 1");
        let entries = vec![AnimeEntry {
            id: 42,
            id_mal: None,
            title_romaji: "Test".into(),
            title_english: "".into(),
            title_native: "".into(),
            cover_url: "".into(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: "Finished".into(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
        }];
        search_cache_put(key.clone(), entries.clone());
        let got = search_cache_get(&key).expect("cached value should be present");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 42);

        // A different normalized key should miss.
        let other = normalize_search_key(false, "completely different");
        assert!(search_cache_get(&other).is_none());
    }
}
