use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::{Mutex, RwLock};

const MAPPINGS_URL: &str = "https://github.com/anibridge/anibridge-mappings/releases/latest/download/mappings.min.json";

/// Cached mapping data: TMDB show ID → list of (anilist_id, mal_id) pairs.
/// A single TMDB show may map to multiple AniList entries (e.g. multi-season).
///
/// Uses RwLock for concurrent reads + Mutex to serialize downloads (no TOCTOU race).
static CACHE: LazyLock<RwLock<Option<CacheState>>> = LazyLock::new(|| RwLock::new(None));
static DOWNLOAD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct CacheState {
    data: MappingCache,
}

#[derive(Debug, Clone)]
pub struct AnimeIds {
    pub anilist_id: Option<i64>,
    pub mal_id: Option<i64>,
}

#[derive(Debug)]
struct MappingCache {
    /// (TMDB show ID, season) → Vec of anime IDs. Season 0 = unscoped.
    tmdb_to_anime: HashMap<(i64, i32), Vec<AnimeIds>>,
    /// (TVDB show ID, season) → Vec of anime IDs. Season 0 = unscoped.
    tvdb_to_anime: HashMap<(i64, i32), Vec<AnimeIds>>,
    /// AniList ID → TMDB show ID (reverse lookup).
    anilist_to_tmdb: HashMap<i64, i64>,
    /// MAL ID → TMDB show ID (reverse lookup for MAL fallback).
    mal_to_tmdb: HashMap<i64, i64>,
}

/// Ensure the mappings are loaded, downloading if necessary.
/// Returns true if cache is available. The download mutex prevents
/// concurrent callers from racing to download at the same time.
pub async fn ensure_loaded() -> bool {
    // Fast path: cache exists and is fresh.
    {
        let cache = CACHE.read().await;
        if cache.is_some() {
            return true;
        }
    }

    // Slow path: serialize downloads so only one caller fetches.
    let _guard = DOWNLOAD_LOCK.lock().await;

    // Re-check after acquiring the lock (another caller may have populated it).
    {
        let cache = CACHE.read().await;
        if cache.is_some() {
            return true;
        }
    }

    match download_and_parse().await {
        Ok(data) => {
            let mut w = CACHE.write().await;
            *w = Some(CacheState { data });
            true
        }
        Err(e) => {
            tracing::error!("Failed to load anibridge mappings: {}", e);
            false
        }
    }
}

/// Force-reload the mappings cache, e.g. from an admin endpoint.
/// Returns true if the reload succeeded.
pub async fn reload() -> bool {
    let _guard = DOWNLOAD_LOCK.lock().await;
    match download_and_parse().await {
        Ok(data) => {
            let mut w = CACHE.write().await;
            *w = Some(CacheState { data });
            true
        }
        Err(e) => {
            tracing::error!("Failed to reload anibridge mappings: {}", e);
            false
        }
    }
}

/// Look up anime IDs by TMDB show ID and optional season.
/// If a season is given, returns only entries for that season.
/// Otherwise returns all entries across all seasons.
pub async fn lookup_by_tmdb(tmdb_id: i64, season: Option<i32>) -> Vec<AnimeIds> {
    let cache = CACHE.read().await;
    let c = match cache.as_ref() {
        Some(c) => c,
        None => return Vec::new(),
    };
    lookup_show(&c.data.tmdb_to_anime, tmdb_id, season)
}

/// Look up anime IDs by TVDB show ID and optional season.
/// If a season is given, returns only entries for that season.
/// Otherwise returns all entries across all seasons.
pub async fn lookup_by_tvdb(tvdb_id: i64, season: Option<i32>) -> Vec<AnimeIds> {
    let cache = CACHE.read().await;
    let c = match cache.as_ref() {
        Some(c) => c,
        None => return Vec::new(),
    };
    lookup_show(&c.data.tvdb_to_anime, tvdb_id, season)
}

/// Search a season-keyed show map. If a specific season is requested, return
/// only that season's entries. Otherwise collect all seasons for the show.
fn lookup_show(
    map: &HashMap<(i64, i32), Vec<AnimeIds>>,
    show_id: i64,
    season: Option<i32>,
) -> Vec<AnimeIds> {
    if let Some(s) = season {
        // Try exact season first, fall back to unscoped (season 0).
        if let Some(v) = map.get(&(show_id, s)) {
            return v.clone();
        }
        if let Some(v) = map.get(&(show_id, 0)) {
            return v.clone();
        }
        return Vec::new();
    }
    // No season requested — collect all entries for this show across all seasons.
    let mut result = Vec::new();
    for ((id, _), entries) in map {
        if *id == show_id {
            for e in entries {
                if let Some(al) = e.anilist_id {
                    if result.iter().any(|r: &AnimeIds| r.anilist_id == Some(al)) {
                        continue;
                    }
                }
                result.push(e.clone());
            }
        }
    }
    result
}

/// Look up all season→anime mappings for a TVDB show. Returns a sorted
/// vec of (season_number, AnimeIds) pairs. Used by series_lookup to build
/// a multi-season Sonarr response.
pub async fn lookup_tvdb_seasons(tvdb_id: i64) -> Vec<(i32, AnimeIds)> {
    let cache = CACHE.read().await;
    let c = match cache.as_ref() {
        Some(c) => c,
        None => return Vec::new(),
    };
    lookup_show_seasons(&c.data.tvdb_to_anime, tvdb_id)
}

/// Same as lookup_tvdb_seasons but for TMDB.
pub async fn lookup_tmdb_seasons(tmdb_id: i64) -> Vec<(i32, AnimeIds)> {
    let cache = CACHE.read().await;
    let c = match cache.as_ref() {
        Some(c) => c,
        None => return Vec::new(),
    };
    lookup_show_seasons(&c.data.tmdb_to_anime, tmdb_id)
}

fn lookup_show_seasons(
    map: &HashMap<(i64, i32), Vec<AnimeIds>>,
    show_id: i64,
) -> Vec<(i32, AnimeIds)> {
    let mut result = Vec::new();
    for (&(id, season), entries) in map {
        if id == show_id {
            for e in entries {
                result.push((season, e.clone()));
            }
        }
    }
    result.sort_by_key(|(s, _)| *s);
    result
}

/// Look up TMDB show ID by MAL ID (for fallback when AniList is unavailable).
pub async fn lookup_tmdb_by_mal(mal_id: i64) -> Option<i64> {
    let cache = CACHE.read().await;
    cache
        .as_ref()
        .and_then(|c| c.data.mal_to_tmdb.get(&mal_id))
        .copied()
}

/// Look up TMDB show ID by AniList ID.
pub async fn lookup_tmdb_by_anilist(anilist_id: i64) -> Option<i64> {
    let cache = CACHE.read().await;
    cache
        .as_ref()
        .and_then(|c| c.data.anilist_to_tmdb.get(&anilist_id))
        .copied()
}

async fn download_and_parse() -> Result<MappingCache, String> {
    tracing::info!("Downloading anibridge mappings...");

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let resp = client
        .get(MAPPINGS_URL)
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("Failed to download mappings: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Mappings download failed: HTTP {}", resp.status()));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read mappings response: {}", e))?;

    tracing::info!("Parsing anibridge mappings ({} bytes)...", bytes.len());

    let data: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Failed to parse mappings JSON: {}", e))?;

    let cache = build_cache(&data);
    tracing::info!(
        "Anibridge mappings loaded: {} TMDB entries, {} TVDB entries, {} AniList reverse entries",
        cache.tmdb_to_anime.len(),
        cache.tvdb_to_anime.len(),
        cache.anilist_to_tmdb.len(),
    );

    Ok(cache)
}

/// Parse the anibridge v3 mappings JSON into our lookup tables.
///
/// The v3 format uses any source as a top-level key:
///   "tmdb_show:45790:s1" → { "anilist:14719": {...}, "tvdb_show:262954:s1": {...} }
///   "anilist:14719"       → { "tmdb_show:45790:s1": {...}, "tvdb_show:262954:s1": {...} }
///
/// We scan every entry and extract TMDB/TVDB show IDs paired with their
/// corresponding AniList/MAL IDs, regardless of which side is the source key.
fn build_cache(data: &serde_json::Value) -> MappingCache {
    let mut tmdb_to_anime: HashMap<(i64, i32), Vec<AnimeIds>> = HashMap::new();
    let mut tvdb_to_anime: HashMap<(i64, i32), Vec<AnimeIds>> = HashMap::new();
    let mut anilist_to_tmdb: HashMap<i64, i64> = HashMap::new();
    let mut mal_to_tmdb: HashMap<i64, i64> = HashMap::new();

    let obj = match data.as_object() {
        Some(o) => o,
        None => return MappingCache { tmdb_to_anime, tvdb_to_anime, anilist_to_tmdb, mal_to_tmdb },
    };

    for (source_key, targets) in obj {
        let target_obj = match targets.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Collect all IDs mentioned across source key + target keys.
        let all_keys: Vec<&str> = std::iter::once(source_key.as_str())
            .chain(target_obj.keys().map(|k| k.as_str()))
            .collect();

        let mut anilist_ids: Vec<i64> = Vec::new();
        let mut mal_ids: Vec<i64> = Vec::new();
        let mut tmdb_ids: Vec<(i64, i32)> = Vec::new();
        let mut tvdb_ids: Vec<(i64, i32)> = Vec::new();

        for key in &all_keys {
            if let Some(id) = parse_provider_id(key, "anilist") {
                if !anilist_ids.contains(&id) { anilist_ids.push(id); }
            } else if let Some(id) = parse_provider_id(key, "mal") {
                if !mal_ids.contains(&id) { mal_ids.push(id); }
            } else if let Some(id_season) = parse_show_id(key, "tmdb_show") {
                if !tmdb_ids.contains(&id_season) { tmdb_ids.push(id_season); }
            } else if let Some(id_season) = parse_show_id(key, "tvdb_show") {
                if !tvdb_ids.contains(&id_season) { tvdb_ids.push(id_season); }
            }
        }

        if anilist_ids.is_empty() && mal_ids.is_empty() {
            continue;
        }

        // Build AnimeIds entries — pair up AniList and MAL IDs where possible.
        let max_len = anilist_ids.len().max(mal_ids.len());
        let anime_entries: Vec<AnimeIds> = (0..max_len)
            .map(|i| AnimeIds {
                anilist_id: anilist_ids.get(i).copied(),
                mal_id: mal_ids.get(i).copied(),
            })
            .collect();

        // Index by each TMDB (show_id, season) found in this entry.
        for &(tmdb_id, season) in &tmdb_ids {
            let entry = tmdb_to_anime.entry((tmdb_id, season)).or_default();
            for ids in &anime_entries {
                if let Some(al) = ids.anilist_id {
                    if entry.iter().any(|e| e.anilist_id == Some(al)) {
                        continue;
                    }
                    anilist_to_tmdb.insert(al, tmdb_id);
                }
                if let Some(m) = ids.mal_id {
                    mal_to_tmdb.insert(m, tmdb_id);
                }
                entry.push(ids.clone());
            }
        }

        // Index by each TVDB (show_id, season) found in this entry.
        for &(tvdb_id, season) in &tvdb_ids {
            let entry = tvdb_to_anime.entry((tvdb_id, season)).or_default();
            for ids in &anime_entries {
                if let Some(al) = ids.anilist_id {
                    if entry.iter().any(|e| e.anilist_id == Some(al)) {
                        continue;
                    }
                }
                entry.push(ids.clone());
            }
        }
    }

    MappingCache { tmdb_to_anime, tvdb_to_anime, anilist_to_tmdb, mal_to_tmdb }
}

/// Parse "tmdb_show:12345:s1" or "tvdb_show:262954:s6" → Some((12345, 1)) / Some((262954, 6))
/// given the matching prefix ("tmdb_show" or "tvdb_show").
/// Returns (show_id, season) where season is 0 if no `:sN` scope is present.
fn parse_show_id(key: &str, prefix: &str) -> Option<(i64, i32)> {
    let rest = key.strip_prefix(prefix)?.strip_prefix(':')?;
    let mut parts = rest.split(':');
    let id: i64 = parts.next()?.parse().ok()?;
    let season: i32 = parts
        .next()
        .and_then(|s| s.strip_prefix('s'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    Some((id, season))
}

/// Parse "anilist:12345" or "mal:12345" → Some(12345) if prefix matches.
fn parse_provider_id(key: &str, provider: &str) -> Option<i64> {
    let rest = key.strip_prefix(provider)?.strip_prefix(':')?;
    let id_str = rest.split(':').next()?;
    id_str.parse().ok()
}
