use serde::{Deserialize, Serialize};
use crate::services::html::sanitize_rich_description;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::services::{jikan, kitsu};

const ANILIST_API: &str = "https://graphql.anilist.co";

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
    if force_mal_fallback {
        return match jikan::search_anime(query).await {
            Ok(results) if !results.is_empty() => Ok(results),
            Ok(_) | Err(_) => kitsu::search_anime(query).await,
        };
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

    let client = reqwest::Client::new();
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

    if status == reqwest::StatusCode::FORBIDDEN {
        tracing::warn!("AniList search 403 for query {:?}; falling back to Jikan/MAL", query);
        return match jikan::search_anime(query).await {
            Ok(results) if !results.is_empty() => Ok(results),
            Ok(_) | Err(_) => kitsu::search_anime(query).await,
        };
    }

    if !status.is_success() {
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!("AniList search failed (HTTP {}): {}", status, msg));
    }

    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!("AniList search failed: {}", msg));
    }

    let media = match body["data"]["Page"]["media"].as_array() {
        Some(arr) => arr,
        None => return Ok(Vec::new()),
    };

    let entries = media
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

    Ok(entries)
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

    let client = reqwest::Client::new();
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
        return jikan::get_anime_detail_cached((-id) as i64).await;
    }
    if force_mal_fallback {
        if let Some(mid) = mal_id_hint {
            return jikan::get_anime_detail_cached(mid).await;
        }
    }

    {
        let cache = DETAIL_CACHE.read().await;
        if let Some(entry) = cache.get(&id) {
            if entry.fetched_at.elapsed().as_secs() < DETAIL_CACHE_TTL_SECS {
                return Ok(entry.detail.clone());
            }
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
            if cache.len() > DETAIL_CACHE_MAX_ENTRIES {
                if let Some((&oldest_key, _)) = cache.iter().min_by_key(|(_, e)| e.fetched_at) {
                    cache.remove(&oldest_key);
                }
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

    let client = reqwest::Client::new();
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
