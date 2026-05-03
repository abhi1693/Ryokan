use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, RwLock};

const MAPPINGS_URL: &str =
    "https://github.com/anibridge/anibridge-mappings/releases/latest/download/mappings.min.json";

/// How long the on-disk mappings JSON is considered fresh. This is
/// also re-used by `main.rs` as the `anibridge_refresh` background-
/// task cadence (via `REFRESH_INTERVAL`), so startup cache-freshness
/// and bg refresh both agree on "fresh vs stale" by definition.
/// Previously the two sites both hardcoded `24 * 60 * 60` and were
/// joined only by a comment — if either drifted you'd get the
/// re-download-every-boot or stale-bytes-at-startup bug the comment
/// was warning about. Now they share one constant in the type system.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_TTL: Duration = REFRESH_INTERVAL;

/// Returns the absolute path of the on-disk mappings cache. Lives
/// under `data/cache/anibridge/mappings.json` by default, which
/// stays consistent with the artwork cache layout. `std::path::absolute`
/// normalizes relative paths so the runtime CWD can't change which
/// file the cache refers to between runs.
///
/// `RYOKAN_ANIBRIDGE_CACHE_DIR` overrides the parent directory.
/// Docker sets it to `/data/cache/anibridge` so the cache lands on
/// the persistent volume rather than `/app/data/cache/anibridge`,
/// which is root-owned and unwritable by the runtime ryokan user
/// (the same CWD-relative footgun that bit `services::crypto`'s
/// key-file path on first boot — see `RYOKAN_KEY_FILE_PATH`).
/// Without the override, every container restart re-downloads the
/// ~9MB mappings blob because the disk cache write silently fails
/// with `Permission denied (os error 13)` and falls through to a
/// fresh fetch.
fn cache_file_path() -> PathBuf {
    let base = std::env::var("RYOKAN_ANIBRIDGE_CACHE_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|d| PathBuf::from(d).join("mappings.json"))
        .unwrap_or_else(|| PathBuf::from("data/cache/anibridge/mappings.json"));
    std::path::absolute(&base).unwrap_or(base)
}

/// Read the cached mappings bytes from disk if the file is younger
/// than `CACHE_TTL`. Returns `None` if the file is missing, unreadable,
/// or stale — the caller then falls back to re-downloading.
fn read_fresh_disk_cache() -> Option<Vec<u8>> {
    let path = cache_file_path();
    let meta = std::fs::metadata(&path).ok()?;
    let mtime = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(mtime).ok()?;
    if age > CACHE_TTL {
        return None;
    }
    std::fs::read(&path).ok()
}

/// Persist the downloaded mappings JSON bytes to disk. Writes to a
/// sibling `.tmp` file first and renames into place so a crash mid-
/// write can't leave a truncated cache file behind (the next startup
/// would then parse garbage and fall through to a fresh download
/// anyway, but half-files are still worth avoiding).
fn write_disk_cache(bytes: &[u8]) -> Result<(), String> {
    let path = cache_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create anibridge cache dir failed: {}", e))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write anibridge cache tmp failed: {}", e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("rename anibridge cache tmp failed: {}", e))?;
    Ok(())
}

/// Sidecar metadata for the on-disk mappings cache. Stores the upstream
/// `ETag` / `Last-Modified` so the next refresh can issue a conditional
/// GET (`If-None-Match` / `If-Modified-Since`) and skip the 8.5 MB
/// download entirely on the ~50% of days the upstream hasn't changed.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct DiskCacheMeta {
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
}

fn meta_file_path() -> PathBuf {
    cache_file_path().with_extension("meta.json")
}

fn read_disk_cache_meta() -> Option<DiskCacheMeta> {
    let bytes = std::fs::read(meta_file_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_disk_cache_meta(meta: &DiskCacheMeta) -> Result<(), String> {
    let path = meta_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create anibridge meta dir failed: {}", e))?;
    }
    let json =
        serde_json::to_vec(meta).map_err(|e| format!("serialize anibridge meta failed: {}", e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write anibridge meta tmp failed: {}", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename anibridge meta tmp failed: {}", e))?;
    Ok(())
}

/// Reset the on-disk mappings file's mtime to "now" so subsequent
/// `read_fresh_disk_cache` calls treat it as fresh again. Called after
/// a 304 response — the upstream confirms our copy is current, so the
/// freshness window restarts even though we didn't rewrite the file.
fn touch_disk_cache() -> Result<(), String> {
    let f = std::fs::File::open(cache_file_path())
        .map_err(|e| format!("open anibridge cache for touch failed: {}", e))?;
    f.set_modified(SystemTime::now())
        .map_err(|e| format!("set_modified anibridge cache failed: {}", e))?;
    Ok(())
}

/// Read the on-disk mappings without freshness checking. Used on the
/// 304 path where the upstream has confirmed our cached copy is the
/// current version regardless of mtime.
fn read_disk_cache_unconditional() -> Option<Vec<u8>> {
    std::fs::read(cache_file_path()).ok()
}

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
    /// MAL ID → AniList ID. Populated whenever a mappings entry pairs
    /// both providers at the same index. Used by the watch-list sync
    /// path (#62 PR B) to translate MAL list entries into AniList IDs
    /// before writing to `series` — without this, every MAL-sourced
    /// entry would land under the negated-MAL-id sentinel and become
    /// invisible to SeaDex / AL-keyed scoring.
    mal_to_anilist: HashMap<i64, i64>,
}

/// Ensure the mappings are loaded, downloading if necessary.
/// Returns true if cache is available. The download mutex prevents
/// concurrent callers from racing to download at the same time.
///
/// Precedence on a cold start:
///   1. In-memory cache (another task already populated it)
///   2. On-disk cache at `data/cache/anibridge/mappings.json`, if
///      its mtime is within `CACHE_TTL`
///   3. Fresh download from GitHub, which is then persisted to disk
///      for the next startup
///
/// The disk path is what makes `cargo run` fast on subsequent
/// launches — the GitHub download is ~8.5 MB and can take several
/// seconds, and it's wasteful to pay that on every restart when the
/// data almost never changes day-to-day. The conditional-GET path
/// (ETag / Last-Modified) inside `download_parse_and_persist` further
/// short-circuits to a 304 response on the days the upstream really
/// is unchanged.
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

    // Try the on-disk cache before hitting the network. `spawn_blocking`
    // is overkill for a single sync read, but we're in an async context
    // and the file can be 20MB+ — doing it inline would briefly hold up
    // other tasks sharing this runtime worker.
    let disk_bytes = tokio::task::spawn_blocking(read_fresh_disk_cache)
        .await
        .ok()
        .flatten();
    if let Some(bytes) = disk_bytes {
        match parse_bytes(&bytes) {
            Ok(data) => {
                tracing::info!("Loaded anibridge mappings from disk cache");
                let mut w = CACHE.write().await;
                *w = Some(CacheState { data });
                return true;
            }
            Err(e) => {
                // Corrupt cache file — fall through to a fresh download
                // and overwrite it below rather than staying broken.
                tracing::warn!("Anibridge disk cache unusable, re-downloading: {}", e);
            }
        }
    }

    match download_parse_and_persist().await {
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

/// Force-reload the mappings cache, e.g. from an admin endpoint or
/// from the `anibridge_refresh` background task. Always hits the
/// network — the caller is explicitly asking for a fresh fetch, so
/// disk TTL is ignored. The fresh bytes do get written back to disk
/// so subsequent restarts can pick them up from there.
pub async fn reload() -> bool {
    let _guard = DOWNLOAD_LOCK.lock().await;
    match download_parse_and_persist().await {
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
                if let Some(al) = e.anilist_id
                    && result.iter().any(|r: &AnimeIds| r.anilist_id == Some(al))
                {
                    continue;
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

/// Look up AniList ID by MAL ID. The watch-list sync (#62 PR B) calls
/// this to resolve MAL-list entries into AL IDs before merging into
/// `series`; on miss the caller falls back to the negated-MAL-id
/// sentinel (`series.anilist_id = -mal_id`) so the entry still lands
/// in the library but is flagged as needing reconciliation later.
pub async fn lookup_anilist_by_mal(mal_id: i64) -> Option<i64> {
    let cache = CACHE.read().await;
    cache
        .as_ref()
        .and_then(|c| c.data.mal_to_anilist.get(&mal_id))
        .copied()
}

/// Test-only: seed the in-memory cache with a controlled set of MAL→AL
/// pairs. Called by external_sync's resolve_mal_anilist_ids tests so
/// they exercise the cache-hit path without depending on the real
/// 8.5 MB mappings blob (which may or may not be present on a given
/// dev machine). All non-MAL→AL maps are left empty — the unit under
/// test only reads `mal_to_anilist`.
#[cfg(any(test, feature = "test-support"))]
pub async fn seed_mal_to_anilist_for_tests(pairs: &[(i64, i64)]) {
    let mut mal_to_anilist = HashMap::new();
    for &(mal, al) in pairs {
        mal_to_anilist.insert(mal, al);
    }
    let data = MappingCache {
        tmdb_to_anime: HashMap::new(),
        tvdb_to_anime: HashMap::new(),
        anilist_to_tmdb: HashMap::new(),
        mal_to_tmdb: HashMap::new(),
        mal_to_anilist,
    };
    let mut w = CACHE.write().await;
    *w = Some(CacheState { data });
}

/// Test-only: drop the in-memory cache so a subsequent
/// `lookup_anilist_by_mal` call exercises the cache-miss path. Pairs
/// with `seed_mal_to_anilist_for_tests` so a test can simulate both
/// branches without process-restart isolation.
#[cfg(any(test, feature = "test-support"))]
pub async fn clear_cache_for_tests() {
    let mut w = CACHE.write().await;
    *w = None;
}

/// Test-only: seed the in-memory cache with arbitrary TVDB/TMDB →
/// AniList/MAL mappings. Lets handler tests for `add_series` /
/// `add_movie` exercise the resolved-mapping branch (the one that
/// fetches AL detail and upserts the series row) without ever
/// reaching the network or disk-cache path.
///
/// `tvdb` / `tmdb` entries are tuples of `(external_id, season,
/// anilist_id, mal_id)`. Season `0` is the unscoped catch-all the
/// real data uses for shows TMDB hasn't sub-divided. Pass `None` for
/// `mal_id` when you only want the AL side mapped.
///
/// Distinct entry from `seed_mal_to_anilist_for_tests` because the
/// `add_series` path doesn't go through the MAL→AL bridge — it asks
/// the TVDB or TMDB index directly. Calling both helpers in the
/// same test would clobber each other (each seeds a fresh
/// `MappingCache`); pick whichever the code path under test actually
/// reads.
#[cfg(any(test, feature = "test-support"))]
pub async fn seed_external_mappings_for_tests(
    tvdb: &[(i64, i32, Option<i64>, Option<i64>)],
    tmdb: &[(i64, i32, Option<i64>, Option<i64>)],
) {
    let mut tvdb_to_anime: HashMap<(i64, i32), Vec<AnimeIds>> = HashMap::new();
    let mut tmdb_to_anime: HashMap<(i64, i32), Vec<AnimeIds>> = HashMap::new();
    let mut anilist_to_tmdb: HashMap<i64, i64> = HashMap::new();
    let mut mal_to_tmdb: HashMap<i64, i64> = HashMap::new();
    for &(tvdb_id, season, al, mal) in tvdb {
        tvdb_to_anime
            .entry((tvdb_id, season))
            .or_default()
            .push(AnimeIds {
                anilist_id: al,
                mal_id: mal,
            });
    }
    for &(tmdb_id, season, al, mal) in tmdb {
        tmdb_to_anime
            .entry((tmdb_id, season))
            .or_default()
            .push(AnimeIds {
                anilist_id: al,
                mal_id: mal,
            });
        if let Some(al_id) = al {
            anilist_to_tmdb.insert(al_id, tmdb_id);
        }
        if let Some(mid) = mal {
            mal_to_tmdb.insert(mid, tmdb_id);
        }
    }
    let data = MappingCache {
        tmdb_to_anime,
        tvdb_to_anime,
        anilist_to_tmdb,
        mal_to_tmdb,
        mal_to_anilist: HashMap::new(),
    };
    let mut w = CACHE.write().await;
    *w = Some(CacheState { data });
}

/// Resolve a TMDB ID from either an AniList ID or a MAL ID. Tries
/// AniList first, falls back to MAL when given. Returns 0 when neither
/// path produces a hit — callers (the Sonarr/Radarr compat handlers)
/// emit `tmdbId: 0` in that case so Seerr can still receive a
/// well-formed payload.
///
/// Centralised here because the same shape lived duplicated in both
/// sonarr_compat and radarr_compat — keeping the lookup chain in one
/// place means future changes (extra fallback IDs, retry semantics,
/// negative-cache) only need editing once.
pub async fn resolve_tmdb_id(anilist_id: i64, mal_id: impl Into<Option<i64>>) -> i64 {
    if let Some(tmdb) = lookup_tmdb_by_anilist(anilist_id).await {
        return tmdb;
    }
    if let Some(mid) = mal_id.into()
        && mid > 0
        && let Some(tmdb) = lookup_tmdb_by_mal(mid).await
    {
        return tmdb;
    }
    0
}

/// Parse raw mappings JSON bytes into the in-memory lookup tables.
/// Shared between the disk-cache path (`ensure_loaded`) and the
/// network path (`download_parse_and_persist`) so there's only one
/// place to fix if the JSON shape changes.
fn parse_bytes(bytes: &[u8]) -> Result<MappingCache, String> {
    let data: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("Failed to parse mappings JSON: {}", e))?;
    let cache = build_cache(&data);
    tracing::info!(
        "Anibridge mappings loaded: {} TMDB entries, {} TVDB entries, {} AniList reverse entries, {} MAL→AL entries",
        cache.tmdb_to_anime.len(),
        cache.tvdb_to_anime.len(),
        cache.anilist_to_tmdb.len(),
        cache.mal_to_anilist.len(),
    );
    Ok(cache)
}

async fn download_parse_and_persist() -> Result<MappingCache, String> {
    tracing::info!("Refreshing anibridge mappings...");

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // Only attach conditional headers when both the meta and the cached
    // bytes are present on disk — sending If-None-Match without having
    // the bytes to fall back to means a 304 is unrecoverable.
    let stored_meta = tokio::task::spawn_blocking(read_disk_cache_meta)
        .await
        .ok()
        .flatten();
    let cache_file_present = tokio::task::spawn_blocking(|| cache_file_path().exists())
        .await
        .unwrap_or(false);
    let conditional_meta = if cache_file_present {
        stored_meta
    } else {
        None
    };

    let mut req = client.get(MAPPINGS_URL).header("User-Agent", "Ryokan/0.1");
    if let Some(meta) = &conditional_meta {
        if let Some(etag) = &meta.etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(lm) = &meta.last_modified {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Failed to download mappings: {}", e))?;

    let status = resp.status();

    // 304 Not Modified — upstream confirms our cached bytes are current.
    // Read them off disk, bump the freshness mtime, and skip the 8.5 MB
    // body. The Azure blob fronting this asset emits ETag + Last-Modified
    // reliably; on the ~50% of days the upstream is unchanged this saves
    // the entire transfer.
    if status == reqwest::StatusCode::NOT_MODIFIED {
        tracing::info!("Anibridge mappings unchanged (HTTP 304); reusing disk cache");
        // RFC 7232 §4.1 permits a 304 to carry refreshed validators (e.g.
        // Azure may rotate an ETag without changing bytes). Capture them
        // and persist alongside the existing body so the next conditional
        // GET sends the current ETag — otherwise our stale `If-None-Match`
        // eventually misses, and we pay a full 8.5 MB body transfer until
        // the next 200 re-syncs the meta.
        let refreshed_meta = DiskCacheMeta {
            etag: resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            last_modified: resp
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        };
        let bytes = tokio::task::spawn_blocking(read_disk_cache_unconditional)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| {
                "Anibridge upstream returned 304 but disk cache is missing".to_string()
            })?;
        if let Err(e) = tokio::task::spawn_blocking(touch_disk_cache)
            .await
            .unwrap_or_else(|e| Err(format!("touch join failed: {}", e)))
        {
            tracing::warn!("Failed to bump anibridge cache mtime after 304: {}", e);
        }
        if (refreshed_meta.etag.is_some() || refreshed_meta.last_modified.is_some())
            && let Err(e) =
                tokio::task::spawn_blocking(move || write_disk_cache_meta(&refreshed_meta))
                    .await
                    .unwrap_or_else(|e| Err(format!("disk meta write join failed: {}", e)))
        {
            tracing::warn!("Failed to refresh anibridge cache meta after 304: {}", e);
        }
        return parse_bytes(&bytes);
    }

    if !status.is_success() {
        return Err(format!("Mappings download failed: HTTP {}", status));
    }

    // Capture caching headers before consuming the body so we can
    // persist them alongside the bytes for the next conditional GET.
    let new_meta = DiskCacheMeta {
        etag: resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        last_modified: resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    };

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read mappings response: {}", e))?;

    tracing::info!("Parsing anibridge mappings ({} bytes)...", bytes.len());

    let cache = parse_bytes(&bytes)?;

    // Persist to disk so the next startup can skip the download.
    // A write failure is logged but not fatal — we still have the
    // parsed mappings in memory and can serve this session from
    // them. The user will see a re-download on next restart but
    // nothing else breaks.
    //
    // The meta sidecar MUST only be persisted after the bytes write
    // succeeds. If we wrote fresh meta over a stale body, the next
    // refresh would attach the new ETag, get a 304 from upstream,
    // and serve the stale body indefinitely (until upstream rotated
    // the validator). Coupling the writes here keeps the disk pair
    // mutually consistent.
    let bytes_vec = bytes.to_vec();
    let bytes_write_result = tokio::task::spawn_blocking(move || write_disk_cache(&bytes_vec))
        .await
        .unwrap_or_else(|e| Err(format!("disk cache write join failed: {}", e)));
    match bytes_write_result {
        Ok(()) => {
            // Persist sidecar meta only when we got at least one validator
            // from upstream — without one, the next conditional GET would
            // just be a regular GET anyway.
            if (new_meta.etag.is_some() || new_meta.last_modified.is_some())
                && let Err(e) =
                    tokio::task::spawn_blocking(move || write_disk_cache_meta(&new_meta))
                        .await
                        .unwrap_or_else(|e| Err(format!("disk meta write join failed: {}", e)))
            {
                tracing::warn!("Failed to persist anibridge cache meta to disk: {}", e);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to persist anibridge mappings to disk: {}", e);
            // Don't write meta on a failed body write — that would
            // leave {old bytes, new validators} on disk, which the
            // next conditional GET would resolve to 304 and serve
            // the stale bytes as if fresh.
        }
    }

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
    let mut mal_to_anilist: HashMap<i64, i64> = HashMap::new();

    let obj = match data.as_object() {
        Some(o) => o,
        None => {
            return MappingCache {
                tmdb_to_anime,
                tvdb_to_anime,
                anilist_to_tmdb,
                mal_to_tmdb,
                mal_to_anilist,
            };
        }
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
                if !anilist_ids.contains(&id) {
                    anilist_ids.push(id);
                }
            } else if let Some(id) = parse_provider_id(key, "mal") {
                if !mal_ids.contains(&id) {
                    mal_ids.push(id);
                }
            } else if let Some(id_season) = parse_show_id(key, "tmdb_show") {
                if !tmdb_ids.contains(&id_season) {
                    tmdb_ids.push(id_season);
                }
            } else if let Some(id_season) = parse_show_id(key, "tvdb_show")
                && !tvdb_ids.contains(&id_season)
            {
                tvdb_ids.push(id_season);
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

        // MAL → AL reverse map. Populated alongside the TMDB/TVDB
        // entries (rather than only inside the indexing loops below)
        // because some mappings entries pair AL+MAL without naming a
        // TMDB or TVDB show — those entries still need to feed the
        // watch-list-sync resolver.
        for ids in &anime_entries {
            if let (Some(al), Some(m)) = (ids.anilist_id, ids.mal_id) {
                mal_to_anilist.insert(m, al);
            }
        }

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
                } else if let Some(m) = ids.mal_id
                    && entry.iter().any(|e| e.mal_id == Some(m))
                {
                    continue;
                }
                entry.push(ids.clone());
            }
        }
    }

    MappingCache {
        tmdb_to_anime,
        tvdb_to_anime,
        anilist_to_tmdb,
        mal_to_tmdb,
        mal_to_anilist,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── parse_provider_id ────────────────────────────────────────

    #[test]
    fn parse_provider_id_extracts_matching_prefix_id() {
        assert_eq!(parse_provider_id("anilist:12345", "anilist"), Some(12345));
        assert_eq!(parse_provider_id("mal:67890", "mal"), Some(67890));
    }

    #[test]
    fn parse_provider_id_rejects_wrong_prefix() {
        assert_eq!(parse_provider_id("mal:12345", "anilist"), None);
        assert_eq!(parse_provider_id("anilist:12345", "mal"), None);
    }

    #[test]
    fn parse_provider_id_ignores_trailing_fields_after_id() {
        // The mappings sometimes have suffixes like "anilist:12345:s1".
        // The id-parser should take only the first numeric part.
        assert_eq!(
            parse_provider_id("anilist:12345:extra", "anilist"),
            Some(12345)
        );
    }

    #[test]
    fn parse_provider_id_returns_none_on_non_numeric_id() {
        assert_eq!(parse_provider_id("anilist:notanumber", "anilist"), None);
    }

    // ─── parse_show_id ────────────────────────────────────────────

    #[test]
    fn parse_show_id_extracts_show_id_without_season() {
        // No `s<N>` segment → default season 0 ("unscoped").
        assert_eq!(parse_show_id("tmdb_show:42", "tmdb_show"), Some((42, 0)));
    }

    #[test]
    fn parse_show_id_extracts_show_id_with_season() {
        assert_eq!(parse_show_id("tmdb_show:42:s3", "tmdb_show"), Some((42, 3)));
        assert_eq!(
            parse_show_id("tvdb_show:100:s1", "tvdb_show"),
            Some((100, 1))
        );
    }

    #[test]
    fn parse_show_id_rejects_wrong_prefix() {
        assert_eq!(parse_show_id("tvdb_show:42", "tmdb_show"), None);
    }

    #[test]
    fn parse_show_id_falls_back_to_zero_season_on_malformed_suffix() {
        // "s-not-a-number" can't parse as i32 — falls back to 0
        // rather than dropping the entry entirely (the show id is
        // still useful as an unscoped lookup).
        assert_eq!(parse_show_id("tmdb_show:42:sX", "tmdb_show"), Some((42, 0)));
    }

    // ─── lookup_show ──────────────────────────────────────────────

    fn seed_map() -> HashMap<(i64, i32), Vec<AnimeIds>> {
        let mut map: HashMap<(i64, i32), Vec<AnimeIds>> = HashMap::new();
        map.insert(
            (100, 1),
            vec![AnimeIds {
                anilist_id: Some(11),
                mal_id: Some(111),
            }],
        );
        map.insert(
            (100, 2),
            vec![AnimeIds {
                anilist_id: Some(22),
                mal_id: Some(222),
            }],
        );
        map.insert(
            (100, 0),
            vec![AnimeIds {
                anilist_id: Some(99),
                mal_id: None,
            }],
        );
        map
    }

    #[test]
    fn lookup_show_returns_exact_season_match() {
        let map = seed_map();
        let results = lookup_show(&map, 100, Some(1));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].anilist_id, Some(11));
    }

    #[test]
    fn lookup_show_falls_back_to_season_zero_on_miss() {
        // Requesting a season that isn't present → falls through
        // to season 0 (the "unscoped" entry). Covers the anime-film
        // case where the mapping indexes the whole TMDB show under
        // season 0 but the caller asks for season 99.
        let map = seed_map();
        let results = lookup_show(&map, 100, Some(99));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].anilist_id, Some(99));
    }

    #[test]
    fn lookup_show_without_season_returns_all_seasons() {
        let map = seed_map();
        let mut results = lookup_show(&map, 100, None);
        results.sort_by_key(|ids| ids.anilist_id.unwrap_or(0));
        assert_eq!(results.len(), 3, "should include all seasons for this show");
    }

    #[test]
    fn lookup_show_without_season_dedupes_same_anilist_id_across_seasons() {
        // If two seasons map to the same AniList id (e.g. a
        // split-cour handled as two TMDB seasons but one AL entry),
        // collect-all-seasons should produce one entry, not two.
        let mut map: HashMap<(i64, i32), Vec<AnimeIds>> = HashMap::new();
        map.insert(
            (200, 1),
            vec![AnimeIds {
                anilist_id: Some(42),
                mal_id: None,
            }],
        );
        map.insert(
            (200, 2),
            vec![AnimeIds {
                anilist_id: Some(42),
                mal_id: None,
            }],
        );
        let results = lookup_show(&map, 200, None);
        assert_eq!(results.len(), 1, "duplicates by AniList id should collapse");
    }

    #[test]
    fn lookup_show_returns_empty_for_unknown_show_id() {
        let map = seed_map();
        assert!(lookup_show(&map, 9999, Some(1)).is_empty());
        assert!(lookup_show(&map, 9999, None).is_empty());
    }

    // ─── lookup_show_seasons ─────────────────────────────────────

    #[test]
    fn lookup_show_seasons_returns_sorted_pairs() {
        let map = seed_map();
        let pairs = lookup_show_seasons(&map, 100);
        let seasons: Vec<i32> = pairs.iter().map(|(s, _)| *s).collect();
        assert_eq!(seasons, vec![0, 1, 2], "seasons must be sorted ascending");
    }

    // ─── parse_bytes / build_cache round-trip ────────────────────

    #[test]
    fn parse_bytes_on_minimal_anibridge_entry_builds_lookup_tables() {
        // Shape matches the real anime-lists JSON: outer object
        // keyed by source provider+id, targets object keyed by
        // other-provider+id. This fixture is a synthetic minimal
        // entry that exercises build_cache's scan without relying
        // on the 8.5 MB real mappings blob.
        let raw = json!({
            "anilist:12345": {
                "mal:67890": {},
                "tmdb_show:200:s1": {},
                "tvdb_show:300:s1": {}
            }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let cache = parse_bytes(&bytes).expect("parse_bytes should succeed");
        // AniList 12345 → TMDB 200 reverse lookup.
        assert_eq!(cache.anilist_to_tmdb.get(&12345), Some(&200));
        // MAL 67890 → TMDB 200 reverse lookup.
        assert_eq!(cache.mal_to_tmdb.get(&67890), Some(&200));
        // (TMDB 200, season 1) → AnimeIds with both ids.
        let entry = cache
            .tmdb_to_anime
            .get(&(200, 1))
            .expect("tmdb entry must be present");
        assert_eq!(entry.len(), 1);
        assert_eq!(entry[0].anilist_id, Some(12345));
        assert_eq!(entry[0].mal_id, Some(67890));
    }

    #[test]
    fn parse_bytes_on_non_object_root_returns_empty_cache_not_error() {
        // Defensive: a JSON that deserializes but is structurally
        // wrong (array at root) should produce an empty cache, not
        // error. Matches how the impl treats unrecognized shapes.
        let bytes = b"[]".to_vec();
        let cache = parse_bytes(&bytes).expect("non-object root should parse to empty cache");
        assert!(cache.tmdb_to_anime.is_empty());
        assert!(cache.anilist_to_tmdb.is_empty());
    }

    #[test]
    fn parse_bytes_on_malformed_json_returns_error() {
        let bytes = b"this is not json".to_vec();
        assert!(parse_bytes(&bytes).is_err());
    }

    #[test]
    fn parse_bytes_populates_mal_to_anilist_for_paired_entries() {
        // The MAL→AL reverse map is what watch-list sync (#62 PR B)
        // uses to resolve MAL list entries before merging into series.
        // An entry that names both anilist:N and mal:M must populate
        // mal_to_anilist[M] = N regardless of whether it also names a
        // TMDB or TVDB show.
        let raw = json!({
            "anilist:12345": {
                "mal:67890": {}
            }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let cache = parse_bytes(&bytes).expect("parse_bytes should succeed");
        assert_eq!(cache.mal_to_anilist.get(&67890), Some(&12345));
    }

    #[test]
    fn parse_bytes_skips_mal_to_anilist_when_pair_incomplete() {
        // An AL-only or MAL-only entry must NOT seed mal_to_anilist
        // (we'd be writing 0 or stale ids). The watch-list sync caller
        // falls back to the negated-MAL-id sentinel on miss; missing
        // entries are the right surface for that.
        let mal_only = json!({
            "mal:67890": {
                "tmdb_show:200:s1": {}
            }
        });
        let bytes = serde_json::to_vec(&mal_only).unwrap();
        let cache = parse_bytes(&bytes).expect("parse_bytes should succeed");
        assert!(cache.mal_to_anilist.is_empty());
    }

    #[test]
    fn parse_bytes_skips_entries_without_anime_ids() {
        // A TMDB-only entry with no anilist/mal companion must not
        // land in the cache — otherwise the reverse lookups would
        // inherit an empty anilist_id and confuse downstream code.
        let raw = json!({
            "tmdb_show:500:s1": {
                "tvdb_show:600:s1": {}
            }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let cache = parse_bytes(&bytes).unwrap();
        assert!(
            !cache.tmdb_to_anime.contains_key(&(500, 1)),
            "anime-less entries should not be indexed"
        );
    }
}
