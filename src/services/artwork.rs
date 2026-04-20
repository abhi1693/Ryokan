use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// Shared reqwest client for artwork downloads. cache_image runs once
/// per cover/banner/relation-card import — many in a row when the
/// metadata sweep refreshes a series — and previously rebuilt the TLS
/// pool every call. Timeouts (10s connect, 30s overall) bound a hung
/// CDN connection so it can't pin a pool slot waiting for TCP keepalive.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the artwork reqwest client should not fail")
});

use crate::models::artwork_cache;

fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn extension_for(content_type: &str, url: &str) -> &'static str {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("png") || url.ends_with(".png") {
        "png"
    } else if ct.contains("webp") || url.ends_with(".webp") {
        "webp"
    } else if ct.contains("gif") || url.ends_with(".gif") {
        "gif"
    } else {
        "jpg"
    }
}

fn blob_filename(blob_hash: &str, content_type: &str, source_url: &str) -> String {
    format!("{}.{}", blob_hash, extension_for(content_type, source_url))
}

pub fn media_cache_dir() -> PathBuf {
    let base = std::env::var("RYOKAN_MEDIA_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/cache/artwork"));
    // Always store absolute paths. Older builds wrote the relative
    // default ("data/cache/artwork/blobs/<hash>.jpg") straight into
    // image_blobs.local_path; those rows then break whenever the
    // process CWD changes (e.g. a Docker image that later adds
    // WORKDIR or sets RYOKAN_MEDIA_CACHE_DIR). Canonicalizing here
    // means every new write records an absolute path regardless of
    // how the env is configured.
    std::path::absolute(&base).unwrap_or(base)
}

pub fn local_url(cache_key: &str, last_write: i64) -> String {
    format!("/media/art/{}?v={}", cache_key, last_write)
}

pub fn canonical_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mid) = mal_id.filter(|mid| *mid > 0) {
        format!("mal-{}", mid)
    } else if provider_id > 0 {
        format!("al-{}", provider_id)
    } else {
        format!("prov-{}", provider_id)
    }
}

pub fn series_relation_cover_key(
    series_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
) -> String {
    format!(
        "series-{}-relation-{}-cover",
        series_id,
        canonical_identity_key(related_provider_id, related_mal_id)
    )
}

pub fn provider_cover_key(provider_id: i64, mal_id: Option<i64>) -> String {
    format!(
        "provider-{}-cover",
        canonical_identity_key(provider_id, mal_id)
    )
}

pub fn provider_banner_key(provider_id: i64, mal_id: Option<i64>) -> String {
    format!(
        "provider-{}-banner",
        canonical_identity_key(provider_id, mal_id)
    )
}

pub fn provider_relation_cover_key(
    provider_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
) -> String {
    format!(
        "provider-{}-relation-{}-cover",
        canonical_identity_key(provider_id, None),
        canonical_identity_key(related_provider_id, related_mal_id)
    )
}

pub async fn first_cached_url(db: &SqlitePool, cache_keys: &[String], source_url: &str) -> String {
    for key in cache_keys {
        if let Ok(Some(url)) = artwork_cache::get_local_url(db, key).await {
            return url;
        }
    }
    source_url.to_string()
}

pub async fn cache_image(
    db: &SqlitePool,
    cache_key: &str,
    parent_kind: &str,
    parent_id: Option<i64>,
    image_kind: &str,
    source_url: &str,
) -> Result<Option<String>, String> {
    if source_url.trim().is_empty() {
        return Ok(None);
    }

    let safe_key = sanitize_key(cache_key);
    let resp = HTTP_CLIENT
        .get(source_url)
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("artwork request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!(
            "artwork request failed with HTTP {}",
            resp.status()
        ));
    }

    let headers = resp.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("artwork body failed: {}", e))?;

    let blob_hash = hex::encode(Sha256::digest(&bytes));
    let dir = media_cache_dir().join("blobs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create artwork dir failed: {}", e))?;

    // Check whether we already have a row for this hash, and if so whether
    // the on-disk file is still where the DB says it is. `has_blob` used to
    // only return a bool; that meant a row with a stale relative path (or
    // a blob file that had been deleted out from under us) would short-
    // circuit the write forever, because the ref row would get refreshed
    // while the broken blob row sat untouched. Self-heal: if the stored
    // path is missing, rewrite the file to the current (absolute) path
    // and upsert_blob so image_blobs is updated in place.
    let existing_path = artwork_cache::get_blob_path(db, &blob_hash)
        .await
        .map_err(|e| e.to_string())?;

    let file_is_live = existing_path
        .as_deref()
        .map(|p| std::path::Path::new(p).is_file())
        .unwrap_or(false);

    if !file_is_live {
        let filename = blob_filename(&blob_hash, &content_type, source_url);
        let path = dir.join(&filename);
        if !path.exists() {
            std::fs::write(&path, &bytes).map_err(|e| format!("write artwork failed: {}", e))?;
        }
        artwork_cache::upsert_blob(
            db,
            &blob_hash,
            &path.to_string_lossy(),
            &content_type,
            bytes.len() as i64,
        )
        .await
        .map_err(|e| e.to_string())?;
    }

    let last_write = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    artwork_cache::upsert_ref(
        db,
        artwork_cache::RefUpsert {
            cache_key: &safe_key,
            parent_kind,
            parent_id,
            image_kind,
            source_url,
            blob_hash: &blob_hash,
            last_write,
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(Some(local_url(&safe_key, last_write)))
}

pub async fn cache_series_detail_artwork(
    db: &SqlitePool,
    series_id: i64,
    detail: &crate::services::anilist::AnimeDetail,
) {
    let _ = cache_image(
        db,
        &format!("series-{}-cover", series_id),
        "series",
        Some(series_id),
        "cover",
        &detail.cover_url,
    )
    .await;
    let _ = cache_image(
        db,
        &format!("series-{}-banner", series_id),
        "series",
        Some(series_id),
        "banner",
        &detail.banner_url,
    )
    .await;
}

pub async fn cache_relation_artwork(
    db: &SqlitePool,
    series_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
    source_url: &str,
) {
    let _ = cache_image(
        db,
        &series_relation_cover_key(series_id, related_provider_id, related_mal_id),
        "series_relation",
        Some(series_id),
        "cover",
        source_url,
    )
    .await;
}

pub async fn cache_provider_detail_artwork(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &crate::services::anilist::AnimeDetail,
) {
    let _ = cache_image(
        db,
        &provider_cover_key(provider_id, mal_id),
        "provider",
        Some(provider_id),
        "cover",
        &detail.cover_url,
    )
    .await;
    let _ = cache_image(
        db,
        &provider_banner_key(provider_id, mal_id),
        "provider",
        Some(provider_id),
        "banner",
        &detail.banner_url,
    )
    .await;
}

pub async fn cache_provider_relation_artwork(
    db: &SqlitePool,
    provider_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
    source_url: &str,
) {
    let _ = cache_image(
        db,
        &provider_relation_cover_key(provider_id, related_provider_id, related_mal_id),
        "provider_relation",
        Some(provider_id),
        "cover",
        source_url,
    )
    .await;
}

pub async fn cached_or_source_url(db: &SqlitePool, cache_key: &str, source_url: &str) -> String {
    artwork_cache::get_local_url(db, cache_key)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| source_url.to_string())
}

pub async fn load_bytes(db: &SqlitePool, cache_key: &str) -> Option<(Vec<u8>, String)> {
    let entry = artwork_cache::get(db, cache_key).await.ok().flatten()?;
    // Use tokio::fs::read so the artwork serving path doesn't block a
    // runtime worker — Seerr does a lot of artwork lookups during
    // discovery scans and the sync read would stack up behind itself.
    let bytes = tokio::fs::read(Path::new(&entry.local_path)).await.ok()?;
    Some((bytes, entry.content_type))
}
