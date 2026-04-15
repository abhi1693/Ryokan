use std::collections::{HashMap, HashSet};

use sqlx::{Row, SqlitePool};

use crate::services::anilist::{AnimeDetail, RelatedEntry};

#[derive(Debug, Clone)]
pub struct CachedEpisodeMetadata {
    pub episode_number: i32,
    pub title: String,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub aired: String,
    pub source: String,
}

fn relation_is_cacheable(rel: &RelatedEntry) -> bool {
    matches!(rel.media_type.as_str(), "ANIME" | "MUSIC")
}

pub fn normalize_relation_type(rel_type: &str) -> &str {
    match rel_type {
        // MAL/Jikan uses PARENT_STORY for entries that are effectively the franchise
        // parent. Normalizing this lets us dedupe it against incoming SIDE_STORY/PARENT
        // edges instead of rendering the same title twice under slightly different labels.
        "PARENT_STORY" => "PARENT",
        other => other,
    }
}

pub fn reverse_relation_type(rel_type: &str) -> Option<&'static str> {
    match normalize_relation_type(rel_type) {
        "PREQUEL" => Some("SEQUEL"),
        "SEQUEL" => Some("PREQUEL"),
        "SIDE_STORY" => Some("PARENT"),
        "PARENT" => Some("SIDE_STORY"),
        "CHARACTER" => Some("CHARACTER"),
        "SPIN_OFF" => Some("PARENT"),
        // MAL/Jikan "Summary" / "Full Story" links are often one-way editorial
        // pointers rather than true bidirectional graph edges. Reversing them makes
        // recap/compilation entries leak onto pages where MAL does not show them.
        "SUMMARY" => None,
        "FULL_STORY" => None,
        _ => None,
    }
}

fn relation_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mal_id) = mal_id {
        format!("mal:{mal_id}")
    } else {
        format!("provider:{provider_id}")
    }
}

fn relation_dedupe_key(rel: &RelatedEntry) -> (String, String) {
    (
        relation_identity_key(rel.id, rel.id_mal),
        normalize_relation_type(&rel.relation_type).to_string(),
    )
}

async fn replace_relations_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
    owner_detail: Option<&AnimeDetail>,
    relations: &[RelatedEntry],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let delete_sql = format!("DELETE FROM {table} WHERE {key_col} = ?");
    sqlx::query(&delete_sql)
        .bind(owner_id)
        .execute(&mut *tx)
        .await?;

    let insert_sql = format!(
        "INSERT INTO {table} ({key_col}, related_provider_id, related_mal_id, title_romaji, title_english, title_native, cover_url, format, status, episodes, relation_type, season_year, media_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT({key_col}, related_provider_id, relation_type) DO UPDATE SET related_mal_id=excluded.related_mal_id, title_romaji=excluded.title_romaji, title_english=excluded.title_english, title_native=excluded.title_native, cover_url=excluded.cover_url, format=excluded.format, status=excluded.status, episodes=excluded.episodes, season_year=excluded.season_year, media_type=excluded.media_type, cached_at=CURRENT_TIMESTAMP"
    );
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let owner_identity = owner_detail
        .map(|o| relation_identity_key(o.id, o.id_mal))
        .unwrap_or_default();
    for rel in relations.iter().filter(|r| relation_is_cacheable(r)) {
        // Skip self-references.
        if rel.id == owner_id {
            continue;
        }
        if !owner_identity.is_empty()
            && relation_identity_key(rel.id, rel.id_mal) == owner_identity
        {
            continue;
        }
        let key = relation_dedupe_key(rel);
        if !seen.insert(key) {
            continue;
        }
        sqlx::query(&insert_sql)
            .bind(owner_id)
            .bind(rel.id)
            .bind(rel.id_mal)
            .bind(&rel.title_romaji)
            .bind(&rel.title_english)
            .bind(&rel.title_native)
            .bind(&rel.cover_url)
            .bind(&rel.format)
            .bind(&rel.status)
            .bind(rel.episodes)
            .bind(normalize_relation_type(&rel.relation_type))
            .bind(rel.season_year)
            .bind(&rel.media_type)
            .execute(&mut *tx)
            .await?;
    }

    if table == "provider_relations_cache"
        && let Some(owner) = owner_detail {
            for rel in relations.iter().filter(|r| relation_is_cacheable(r)) {
                let Some(reverse_type) = reverse_relation_type(&rel.relation_type) else {
                    continue;
                };
                // Skip self-references — the owner shouldn't point back to itself.
                if rel.id == owner_id {
                    continue;
                }
                // Also skip if the identity keys match (catches MAL ID overlap).
                if relation_identity_key(rel.id, rel.id_mal)
                    == relation_identity_key(owner.id, owner.id_mal)
                {
                    continue;
                }
                sqlx::query(&insert_sql)
                    .bind(rel.id)
                    .bind(owner.id)
                    .bind(owner.id_mal)
                    .bind(&owner.title_romaji)
                    .bind(&owner.title_english)
                    .bind(&owner.title_native)
                    .bind(&owner.cover_url)
                    .bind(&owner.format)
                    .bind(&owner.status)
                    .bind(owner.episodes)
                    .bind(reverse_type)
                    .bind(owner.season_year)
                    .bind("ANIME")
                    .execute(&mut *tx)
                    .await?;
            }
        }

    tx.commit().await?;
    Ok(())
}

async fn get_relations_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    let sql = format!(
        "SELECT related_provider_id, related_mal_id, title_romaji, title_english, title_native, cover_url, format, status, episodes, relation_type, season_year, media_type FROM {table} WHERE {key_col} = ? ORDER BY relation_type, related_provider_id"
    );
    let rows = sqlx::query(&sql).bind(owner_id).fetch_all(db).await?;
    Ok(rows.into_iter().map(row_to_related).collect())
}

pub async fn get_incoming_relations_for_provider(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    // When provider_id is negative (MAL-sourced, id = -mal_id), the related_provider_id
    // column stores the negated MAL ID and related_mal_id stores the positive MAL ID.
    // We match on both but must deduplicate since both can match the same row.
    // Important: for incoming relations, provider_relations_cache rows describe the *target*
    // in their title/cover columns because those columns are copied from the related entry at
    // insert time. Using those columns here makes the source relation card render as the current
    // series, which is exactly the self-card bug seen on Monogatari pages. Always hydrate the
    // source card from provider_metadata_cache instead.
    let rows = sqlx::query(
        r#"
        SELECT pr.provider_id AS source_provider_id,
               CASE
                   WHEN pm.mal_id IS NOT NULL THEN pm.mal_id
                   WHEN pr.provider_id < 0 THEN -pr.provider_id
                   ELSE NULL
               END AS source_mal_id,
               COALESCE(json_extract(pm.detail_json, '$.title_romaji'), '') AS title_romaji,
               COALESCE(json_extract(pm.detail_json, '$.title_english'), '') AS title_english,
               COALESCE(json_extract(pm.detail_json, '$.title_native'), '') AS title_native,
               COALESCE(json_extract(pm.detail_json, '$.cover_url'), '') AS cover_url,
               COALESCE(json_extract(pm.detail_json, '$.format'), '') AS format,
               COALESCE(json_extract(pm.detail_json, '$.status'), '') AS status,
               json_extract(pm.detail_json, '$.episodes') AS episodes,
               pr.relation_type AS relation_type,
               json_extract(pm.detail_json, '$.season_year') AS season_year,
               'ANIME' AS media_type
        FROM provider_relations_cache pr
        LEFT JOIN provider_metadata_cache pm ON pm.provider_id = pr.provider_id
        WHERE pr.related_provider_id = ?
           OR (? IS NOT NULL AND pr.related_mal_id = ?)
        GROUP BY pr.provider_id, pr.relation_type
        ORDER BY pr.relation_type, pr.provider_id
        "#,
    )
    .bind(provider_id)
    .bind(mal_id)
    .bind(mal_id)
    .fetch_all(db)
    .await?;

    let mut seen: HashSet<(String, String)> = HashSet::new();
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let rel_type: String = row.get("relation_type");
            let reverse = reverse_relation_type(&rel_type)?;
            let source_id: i64 = row.get("source_provider_id");
            let source_mal: Option<i64> = row.get::<Option<i64>, _>("source_mal_id");
            let key = (relation_identity_key(source_id, source_mal), reverse.to_string());
            if !seen.insert(key) {
                return None;
            }
            Some(RelatedEntry {
                id: source_id,
                id_mal: source_mal,
                title_romaji: row.get("title_romaji"),
                title_english: row.get("title_english"),
                title_native: row.get("title_native"),
                cover_url: row.get("cover_url"),
                format: row.get("format"),
                status: row.get("status"),
                status_display: row.get::<String, _>("status").replace('_', " "),
                episodes: row.get::<Option<i32>, _>("episodes"),
                relation_type: reverse.to_string(),
                season_year: row.get::<Option<i32>, _>("season_year"),
                media_type: row.get("media_type"),
            })
        })
        .collect())
}

fn row_to_related(row: sqlx::sqlite::SqliteRow) -> RelatedEntry {
    RelatedEntry {
        id: row.get("related_provider_id"),
        id_mal: row.get::<Option<i64>, _>("related_mal_id"),
        title_romaji: row.get("title_romaji"),
        title_english: row.get("title_english"),
        title_native: row.get("title_native"),
        cover_url: row.get("cover_url"),
        format: row.get("format"),
        status: row.get("status"),
        status_display: row.get::<String, _>("status").replace('_', " "),
        episodes: row.get::<Option<i32>, _>("episodes"),
        relation_type: row.get("relation_type"),
        season_year: row.get::<Option<i32>, _>("season_year"),
        media_type: row.get("media_type"),
    }
}

pub async fn replace_relations_for_series(db: &SqlitePool, series_id: i64, detail: &AnimeDetail) -> Result<(), sqlx::Error> {
    replace_relations_table(db, "series_relations_cache", "series_id", series_id, Some(detail), &detail.relations).await
}

pub async fn replace_relations_for_provider(db: &SqlitePool, provider_id: i64, detail: &AnimeDetail) -> Result<(), sqlx::Error> {
    replace_relations_table(db, "provider_relations_cache", "provider_id", provider_id, Some(detail), &detail.relations).await
}

pub async fn get_relations_for_series(db: &SqlitePool, series_id: i64) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    get_relations_table(db, "series_relations_cache", "series_id", series_id).await
}

pub async fn get_relations_for_provider(db: &SqlitePool, provider_id: i64) -> Result<Vec<RelatedEntry>, sqlx::Error> {
    get_relations_table(db, "provider_relations_cache", "provider_id", provider_id).await
}

async fn replace_episode_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
    episodes: &[CachedEpisodeMetadata],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let delete_sql = format!("DELETE FROM {table} WHERE {key_col} = ?");
    sqlx::query(&delete_sql).bind(owner_id).execute(&mut *tx).await?;
    let insert_sql = format!(
        "INSERT INTO {table} ({key_col}, episode_number, title, title_romaji, title_english, title_native, aired, source, cached_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)"
    );
    for ep in episodes {
        sqlx::query(&insert_sql)
            .bind(owner_id)
            .bind(ep.episode_number)
            .bind(&ep.title)
            .bind(&ep.title_romaji)
            .bind(&ep.title_english)
            .bind(&ep.title_native)
            .bind(&ep.aired)
            .bind(&ep.source)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn get_episode_table(
    db: &SqlitePool,
    table: &str,
    key_col: &str,
    owner_id: i64,
) -> Result<HashMap<i32, CachedEpisodeMetadata>, sqlx::Error> {
    let sql = format!(
        "SELECT episode_number, title, title_romaji, title_english, title_native, aired, source FROM {table} WHERE {key_col} = ? ORDER BY episode_number ASC"
    );
    let rows = sqlx::query(&sql).bind(owner_id).fetch_all(db).await?;
    let mut out = HashMap::new();
    for row in rows {
        let ep = CachedEpisodeMetadata {
            episode_number: row.get("episode_number"),
            title: row.get("title"),
            title_romaji: row.get("title_romaji"),
            title_english: row.get("title_english"),
            title_native: row.get("title_native"),
            aired: row.get("aired"),
            source: row.get("source"),
        };
        out.insert(ep.episode_number, ep);
    }
    Ok(out)
}

pub async fn replace_episode_metadata(db: &SqlitePool, series_id: i64, episodes: &[CachedEpisodeMetadata]) -> Result<(), sqlx::Error> {
    replace_episode_table(db, "series_episode_metadata", "series_id", series_id, episodes).await
}

pub async fn replace_episode_metadata_for_provider(db: &SqlitePool, provider_id: i64, episodes: &[CachedEpisodeMetadata]) -> Result<(), sqlx::Error> {
    replace_episode_table(db, "provider_episode_metadata", "provider_id", provider_id, episodes).await
}

pub async fn get_episode_map_for_series(db: &SqlitePool, series_id: i64) -> Result<HashMap<i32, CachedEpisodeMetadata>, sqlx::Error> {
    get_episode_table(db, "series_episode_metadata", "series_id", series_id).await
}

pub async fn get_episode_map_for_provider(db: &SqlitePool, provider_id: i64) -> Result<HashMap<i32, CachedEpisodeMetadata>, sqlx::Error> {
    get_episode_table(db, "provider_episode_metadata", "provider_id", provider_id).await
}
