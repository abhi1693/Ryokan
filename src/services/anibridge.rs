use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::RwLock;

const MAPPINGS_URL: &str = "https://github.com/anibridge/anibridge-mappings/releases/latest/download/mappings.min.json";

/// Cached mapping data: TMDB show ID → list of (anilist_id, mal_id) pairs.
/// A single TMDB show may map to multiple AniList entries (e.g. multi-season).
static CACHE: LazyLock<RwLock<Option<MappingCache>>> = LazyLock::new(|| RwLock::new(None));

#[derive(Debug, Clone)]
pub struct AnimeIds {
    pub anilist_id: Option<i64>,
    pub mal_id: Option<i64>,
}

#[derive(Debug)]
struct MappingCache {
    /// TMDB show ID → Vec of anime IDs (one per season/entry).
    tmdb_to_anime: HashMap<i64, Vec<AnimeIds>>,
    /// AniList ID → TMDB show ID (reverse lookup).
    anilist_to_tmdb: HashMap<i64, i64>,
}

/// Ensure the mappings are loaded, downloading if necessary.
/// Returns true if cache is available.
pub async fn ensure_loaded() -> bool {
    {
        let cache = CACHE.read().await;
        if cache.is_some() {
            return true;
        }
    }

    match download_and_parse().await {
        Ok(cache) => {
            let mut w = CACHE.write().await;
            *w = Some(cache);
            true
        }
        Err(e) => {
            tracing::error!("Failed to load anibridge mappings: {}", e);
            false
        }
    }
}

/// Look up anime IDs by TMDB show ID. Returns all AniList/MAL entries for that show.
pub async fn lookup_by_tmdb(tmdb_id: i64) -> Vec<AnimeIds> {
    let cache = CACHE.read().await;
    cache
        .as_ref()
        .and_then(|c| c.tmdb_to_anime.get(&tmdb_id))
        .cloned()
        .unwrap_or_default()
}

/// Look up TMDB show ID by AniList ID.
pub async fn lookup_tmdb_by_anilist(anilist_id: i64) -> Option<i64> {
    let cache = CACHE.read().await;
    cache
        .as_ref()
        .and_then(|c| c.anilist_to_tmdb.get(&anilist_id))
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
        "Anibridge mappings loaded: {} TMDB entries, {} AniList reverse entries",
        cache.tmdb_to_anime.len(),
        cache.anilist_to_tmdb.len(),
    );

    Ok(cache)
}

/// Parse the anibridge mappings JSON into our lookup tables.
///
/// The format uses source descriptors as keys:
///   "tmdb_show:12345:s1" → { "anilist:67890": { "1-12": "1-12" } }
///
/// We extract TMDB show IDs and their corresponding AniList/MAL IDs.
fn build_cache(data: &serde_json::Value) -> MappingCache {
    let mut tmdb_to_anime: HashMap<i64, Vec<AnimeIds>> = HashMap::new();
    let mut anilist_to_tmdb: HashMap<i64, i64> = HashMap::new();

    let obj = match data.as_object() {
        Some(o) => o,
        None => return MappingCache { tmdb_to_anime, anilist_to_tmdb },
    };

    for (source_key, targets) in obj {
        // Parse source descriptor: "tmdb_show:12345" or "tmdb_show:12345:s1"
        let tmdb_id = match parse_tmdb_show_id(source_key) {
            Some(id) => id,
            None => continue,
        };

        let target_obj = match targets.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Collect AniList and MAL IDs from the targets.
        let mut anilist_ids: Vec<i64> = Vec::new();
        let mut mal_ids: Vec<i64> = Vec::new();

        for target_key in target_obj.keys() {
            if let Some(id) = parse_provider_id(target_key, "anilist") {
                anilist_ids.push(id);
            } else if let Some(id) = parse_provider_id(target_key, "mal") {
                mal_ids.push(id);
            }
        }

        // Build AnimeIds entries — pair up AniList and MAL IDs where possible.
        let max_len = anilist_ids.len().max(mal_ids.len());
        if max_len == 0 {
            continue;
        }

        let entry = tmdb_to_anime.entry(tmdb_id).or_default();
        for i in 0..max_len {
            let al_id = anilist_ids.get(i).copied();
            let mal = mal_ids.get(i).copied();
            // Avoid duplicate entries for the same AniList ID under this TMDB show.
            if let Some(al) = al_id {
                if entry.iter().any(|e| e.anilist_id == Some(al)) {
                    continue;
                }
                anilist_to_tmdb.insert(al, tmdb_id);
            }
            entry.push(AnimeIds {
                anilist_id: al_id,
                mal_id: mal,
            });
        }
    }

    MappingCache { tmdb_to_anime, anilist_to_tmdb }
}

/// Parse "tmdb_show:12345" or "tmdb_show:12345:s1" → Some(12345)
fn parse_tmdb_show_id(key: &str) -> Option<i64> {
    let rest = key.strip_prefix("tmdb_show:")?;
    // The ID is the next segment before an optional ":sN" scope.
    let id_str = rest.split(':').next()?;
    id_str.parse().ok()
}

/// Parse "anilist:12345" or "mal:12345" → Some(12345) if prefix matches.
fn parse_provider_id(key: &str, provider: &str) -> Option<i64> {
    let rest = key.strip_prefix(provider)?.strip_prefix(':')?;
    let id_str = rest.split(':').next()?;
    id_str.parse().ok()
}
