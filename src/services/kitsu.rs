use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::services::anilist::AnimeDetail;
use crate::services::html::sanitize_rich_description;

const KITSU_API: &str = "https://kitsu.io/api/edge";

/// Shared reqwest client. Replaces a per-call `Client::new()` so the
/// connection pool is reused across the search/detail fetch helpers.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const NEGATIVE_CACHE_SENTINEL: &str = "__RYOKAN_EMPTY__";

#[derive(Debug, Clone)]
pub struct EpisodeInfo {
    pub title: String,
    pub aired: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: i64,
    canonical_title: String,
    titles: HashMap<String, String>,
    abbreviated_titles: Vec<String>,
    synopsis: String,
    poster_image: ImageSet,
    cover_image: ImageSet,
    subtype: String,
    status: String,
    episode_count: Option<i32>,
    episode_length: Option<i32>,
    start_date: Option<String>,
    end_date: Option<String>,
    average_rating: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CollectionResponse<T> {
    data: Vec<Resource<T>>,
    links: Option<PaginationLinks>,
}

#[derive(Debug, Deserialize)]
struct Resource<T> {
    id: String,
    attributes: T,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct PaginationLinks {
    next: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ImageSet {
    tiny: Option<String>,
    small: Option<String>,
    medium: Option<String>,
    large: Option<String>,
    original: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeAttributes {
    canonical_title: Option<String>,
    titles: Option<HashMap<String, String>>,
    abbreviated_titles: Option<Vec<String>>,
    synopsis: Option<String>,
    poster_image: Option<ImageSet>,
    cover_image: Option<ImageSet>,
    subtype: Option<String>,
    status: Option<String>,
    episode_count: Option<i32>,
    episode_length: Option<i32>,
    start_date: Option<String>,
    end_date: Option<String>,
    average_rating: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeAttributes {
    canonical_title: Option<String>,
    titles: Option<HashMap<String, String>>,
    number: Option<i32>,
    relative_number: Option<i32>,
    air_date: Option<String>,
}

fn first_image(images: &ImageSet) -> String {
    images
        .original
        .clone()
        .or_else(|| images.large.clone())
        .or_else(|| images.medium.clone())
        .or_else(|| images.small.clone())
        .or_else(|| images.tiny.clone())
        .unwrap_or_default()
}

fn normalize_title(value: &str) -> String {
    value
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            '\'' | '’' | '"' | ':' | ',' | '.' | '!' | '?' | '-' | '_' | '/' | '(' | ')' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn nonempty(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = normalize_title(trimmed);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

fn candidate_titles(candidate: &Candidate) -> Vec<String> {
    let mut vals = vec![candidate.canonical_title.clone()];
    vals.extend(candidate.titles.values().cloned());
    vals.extend(candidate.abbreviated_titles.clone());
    nonempty(vals)
}

fn parse_year(date: Option<&str>) -> Option<i32> {
    date.and_then(|s| s.get(0..4)).and_then(|y| y.parse::<i32>().ok())
}

fn score_candidate(candidate: &Candidate, wanted_titles: &[String], wanted_year: Option<i32>, wanted_eps: Option<i32>) -> i64 {
    let mut score = 0_i64;
    let cand_titles = candidate_titles(candidate)
        .into_iter()
        .map(|t| normalize_title(&t))
        .collect::<Vec<_>>();

    for wanted in wanted_titles {
        let wanted_norm = normalize_title(wanted);
        if wanted_norm.is_empty() {
            continue;
        }
        for cand in &cand_titles {
            if *cand == wanted_norm {
                score += 220;
            } else if cand.contains(&wanted_norm) || wanted_norm.contains(cand) {
                score += 120;
            }
        }
    }

    if let (Some(wy), Some(cy)) = (wanted_year, parse_year(candidate.start_date.as_deref())) {
        let delta = (wy - cy).abs();
        if delta == 0 {
            score += 40;
        } else if delta == 1 {
            score += 15;
        }
    }

    if let (Some(we), Some(ce)) = (wanted_eps, candidate.episode_count) {
        let delta = (we - ce).abs();
        if delta == 0 {
            score += 40;
        } else if delta <= 2 {
            score += 18;
        } else if delta <= 6 {
            score += 8;
        }
    }

    if candidate.subtype.eq_ignore_ascii_case("TV") {
        score += 10;
    }

    score
}

async fn fetch_collection<T: for<'de> serde::Deserialize<'de>>(url: &str, params: &[(&str, &str)]) -> Result<CollectionResponse<T>, String> {
    HTTP_CLIENT
        .get(url)
        .query(params)
        .header("Accept", "application/vnd.api+json")
        .header("Content-Type", "application/vnd.api+json")
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("Kitsu request failed: {}", e))?
        .error_for_status()
        .map_err(|e| format!("Kitsu request failed: {}", e))?
        .json::<CollectionResponse<T>>()
        .await
        .map_err(|e| format!("Failed to parse Kitsu response: {}", e))
}

fn to_candidate(resource: Resource<AnimeAttributes>) -> Option<Candidate> {
    let id = resource.id.parse::<i64>().ok()?;
    let attrs = resource.attributes;
    Some(Candidate {
        id,
        canonical_title: attrs.canonical_title.unwrap_or_default(),
        titles: attrs.titles.unwrap_or_default(),
        abbreviated_titles: attrs.abbreviated_titles.unwrap_or_default(),
        synopsis: attrs.synopsis.unwrap_or_default(),
        poster_image: attrs.poster_image.unwrap_or_default(),
        cover_image: attrs.cover_image.unwrap_or_default(),
        subtype: attrs.subtype.unwrap_or_default(),
        status: attrs.status.unwrap_or_default(),
        episode_count: attrs.episode_count,
        episode_length: attrs.episode_length,
        start_date: attrs.start_date,
        end_date: attrs.end_date,
        average_rating: attrs.average_rating,
    })
}

async fn best_candidate(queries: &[String], wanted_year: Option<i32>, wanted_eps: Option<i32>) -> Result<Option<Candidate>, String> {
    let queries = nonempty(queries.to_vec());
    if queries.is_empty() {
        return Ok(None);
    }

    let mut best: Option<(Candidate, i64)> = None;
    for query in &queries {
        let response = match fetch_collection::<AnimeAttributes>(
            &format!("{}/anime", KITSU_API),
            &[("filter[text]", query.as_str()), ("page[limit]", "10")],
        )
        .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };

        for item in response.data.into_iter().filter_map(to_candidate) {
            let score = score_candidate(&item, &queries, wanted_year, wanted_eps);
            if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                best = Some((item, score));
            }
        }
    }

    Ok(best.map(|(c, _)| c))
}

fn to_anime_detail(item: Candidate) -> AnimeDetail {
    let title_romaji = item
        .titles
        .get("en_jp")
        .cloned()
        .unwrap_or_else(|| item.canonical_title.clone());
    let title_english = item.titles.get("en").cloned().unwrap_or_default();
    let title_native = item.titles.get("ja_jp").cloned().unwrap_or_default();
    let score = item
        .average_rating
        .as_deref()
        .and_then(|v| v.parse::<f32>().ok())
        .map(|v| v.round() as i32);
    let score_class = match score {
        Some(s) if s >= 85 => "tag-score-purple",
        Some(s) if s >= 75 => "tag-score-green",
        Some(s) if s > 65 => "tag-score-yellow",
        _ => "tag-score-red",
    }
    .to_string();

    AnimeDetail {
        id: item.id,
        id_mal: None,
        title_romaji,
        title_english,
        title_native,
        cover_url: first_image(&item.poster_image),
        banner_url: first_image(&item.cover_image),
        format: item.subtype.to_ascii_uppercase(),
        status: item.status.to_ascii_uppercase().replace(' ', "_"),
        status_display: item.status.replace('-', " "),
        episodes: item.episode_count,
        duration: item.episode_length,
        season: String::new(),
        season_year: parse_year(item.start_date.as_deref()),
        end_year: parse_year(item.end_date.as_deref()),
        description: sanitize_rich_description(&item.synopsis, false),
        genres: Vec::new(),
        average_score: score,
        average_score_display: score.map(|s| format!("{:.2}/10", s as f32 / 10.0)),
        score_is_ten_point: false,
        score_class,
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

pub async fn get_anime_detail_by_titles(titles: &[String], wanted_year: Option<i32>, wanted_eps: Option<i32>) -> Result<AnimeDetail, String> {
    let candidate = best_candidate(titles, wanted_year, wanted_eps)
        .await?
        .ok_or_else(|| "Kitsu returned no matching anime".to_string())?;
    Ok(to_anime_detail(candidate))
}

async fn fetch_episode_page_via_relationship(kitsu_id: i64, offset: i32) -> Result<CollectionResponse<EpisodeAttributes>, String> {
    let offset_str = offset.to_string();
    let params = [("page[limit]", "20"), ("page[offset]", offset_str.as_str()), ("sort", "number")];
    fetch_collection::<EpisodeAttributes>(
        &format!("{}/anime/{}/episodes", KITSU_API, kitsu_id),
        &params,
    )
    .await
}

async fn fetch_episode_page_via_filter(kitsu_id: i64, offset: i32) -> Result<CollectionResponse<EpisodeAttributes>, String> {
    let kitsu_id_str = kitsu_id.to_string();
    let offset_str = offset.to_string();
    let params = [
        ("filter[mediaId]", kitsu_id_str.as_str()),
        ("page[limit]", "20"),
        ("page[offset]", offset_str.as_str()),
        ("sort", "number"),
    ];
    fetch_collection::<EpisodeAttributes>(&format!("{}/episodes", KITSU_API), &params).await
}

async fn get_cached_kitsu_episodes(
    db: &SqlitePool,
    kitsu_id: i64,
) -> Result<Option<HashMap<i32, EpisodeInfo>>, sqlx::Error> {
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        r#"
        SELECT episode_number, title, aired FROM kitsu_episode_cache
        WHERE kitsu_id = ?
        AND cached_at > datetime('now', ? || ' seconds')
        "#,
    )
    .bind(kitsu_id)
    .bind(-CACHE_TTL_SECS)
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut map = HashMap::new();
    let mut has_negative_sentinel = false;
    for (num, title, aired) in rows {
        if num == 0 && title == NEGATIVE_CACHE_SENTINEL {
            has_negative_sentinel = true;
            continue;
        }
        map.insert(num, EpisodeInfo { title, aired });
    }

    if has_negative_sentinel || !map.is_empty() {
        Ok(Some(map))
    } else {
        Ok(None)
    }
}

async fn cache_kitsu_episodes(
    db: &SqlitePool,
    kitsu_id: i64,
    episodes: &HashMap<i32, EpisodeInfo>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM kitsu_episode_cache WHERE kitsu_id = ?")
        .bind(kitsu_id)
        .execute(db)
        .await?;

    if episodes.is_empty() {
        sqlx::query(
            "INSERT INTO kitsu_episode_cache (kitsu_id, episode_number, title, aired) VALUES (?, 0, ?, '')",
        )
        .bind(kitsu_id)
        .bind(NEGATIVE_CACHE_SENTINEL)
        .execute(db)
        .await?;
        return Ok(());
    }

    for (num, info) in episodes {
        sqlx::query(
            "INSERT INTO kitsu_episode_cache (kitsu_id, episode_number, title, aired) VALUES (?, ?, ?, ?)",
        )
        .bind(kitsu_id)
        .bind(num)
        .bind(&info.title)
        .bind(&info.aired)
        .execute(db)
        .await?;
    }

    Ok(())
}

pub async fn fetch_episode_titles_fallback(
    db: &SqlitePool,
    titles: &[String],
    wanted_year: Option<i32>,
    wanted_eps: Option<i32>,
) -> HashMap<i32, EpisodeInfo> {
    let candidate = match best_candidate(titles, wanted_year, wanted_eps).await {
        Ok(Some(c)) => c,
        _ => return HashMap::new(),
    };

    if let Ok(Some(cached)) = get_cached_kitsu_episodes(db, candidate.id).await {
        return cached;
    }

    let mut out = HashMap::new();
    let mut offset = 0;
    let mut pages = 0;

    loop {
        let response = match fetch_episode_page_via_relationship(candidate.id, offset).await {
            Ok(v) => Ok(v),
            Err(_) => fetch_episode_page_via_filter(candidate.id, offset).await,
        };

        let response = match response {
            Ok(v) => v,
            Err(_) => break,
        };

        let count = response.data.len();
        let has_next = response.links.as_ref().and_then(|l| l.next.as_ref()).is_some();

        for resource in response.data {
            let attrs = resource.attributes;
            let ep_num = attrs.relative_number.or(attrs.number);
            let Some(ep_num) = ep_num else {
                continue;
            };
            let raw_title = attrs
                .canonical_title
                .or_else(|| attrs.titles.as_ref().and_then(|m| m.get("en").cloned()))
                .or_else(|| attrs.titles.as_ref().and_then(|m| m.get("en_jp").cloned()))
                .or_else(|| attrs.titles.as_ref().and_then(|m| m.get("ja_jp").cloned()))
                .unwrap_or_default();
            let title = if raw_title.trim().is_empty() {
                format!("Episode {}", ep_num)
            } else {
                raw_title
            };
            let aired = match attrs.air_date {
                Some(d) if !d.trim().is_empty() => d,
                _ => "-".to_string(),
            };
            out.insert(ep_num, EpisodeInfo { title, aired });
        }

        pages += 1;
        if count < 20 || pages >= 20 || !has_next {
            break;
        }
        offset += 20;
    }

    let _ = cache_kitsu_episodes(db, candidate.id, &out).await;
    out
}
