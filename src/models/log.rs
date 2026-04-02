use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// Log levels, ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "trace" => LogLevel::Trace,
            "debug" => LogLevel::Debug,
            "warn" => LogLevel::Warn,
            "error" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }
}

/// Categories for log entries, matching the major subsystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogCategory {
    Search,
    Grab,
    AutoSearch,
    Nyaa,
    AniList,
    Jikan,
    QBit,
    Jellyfin,
    Media,
    Library,
    Auth,
    System,
}

impl LogCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogCategory::Search => "search",
            LogCategory::Grab => "grab",
            LogCategory::AutoSearch => "auto_search",
            LogCategory::Nyaa => "nyaa",
            LogCategory::AniList => "anilist",
            LogCategory::Jikan => "jikan",
            LogCategory::QBit => "qbit",
            LogCategory::Jellyfin => "jellyfin",
            LogCategory::Media => "media",
            LogCategory::Library => "library",
            LogCategory::Auth => "auth",
            LogCategory::System => "system",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "search" => Some(LogCategory::Search),
            "grab" => Some(LogCategory::Grab),
            "auto_search" => Some(LogCategory::AutoSearch),
            "nyaa" => Some(LogCategory::Nyaa),
            "anilist" => Some(LogCategory::AniList),
            "jikan" => Some(LogCategory::Jikan),
            "qbit" => Some(LogCategory::QBit),
            "jellyfin" => Some(LogCategory::Jellyfin),
            "media" => Some(LogCategory::Media),
            "library" => Some(LogCategory::Library),
            "auth" => Some(LogCategory::Auth),
            "system" => Some(LogCategory::System),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LogCategory::Search => "Search",
            LogCategory::Grab => "Grab",
            LogCategory::AutoSearch => "Auto Search",
            LogCategory::Nyaa => "Nyaa",
            LogCategory::AniList => "AniList",
            LogCategory::Jikan => "Jikan",
            LogCategory::QBit => "qBittorrent",
            LogCategory::Jellyfin => "Jellyfin",
            LogCategory::Media => "Media",
            LogCategory::Library => "Library",
            LogCategory::Auth => "Auth",
            LogCategory::System => "System",
        }
    }
}

/// A single log entry as stored in the database.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub id: i64,
    pub timestamp: String,
    pub level: String,
    pub category: String,
    pub message: String,
    pub detail: String,
}

/// Query parameters for fetching logs.
pub struct LogQuery {
    pub level: Option<String>,
    pub category: Option<String>,
    pub search: Option<String>,
    pub limit: i64,
    pub before_id: Option<i64>,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            level: None,
            category: None,
            search: None,
            limit: 200,
            before_id: None,
        }
    }
}

/// Insert a log entry.
pub async fn insert(
    db: &SqlitePool,
    level: LogLevel,
    category: LogCategory,
    message: &str,
    detail: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO logs (level, category, message, detail) VALUES (?, ?, ?, ?)",
    )
    .bind(level.as_str())
    .bind(category.as_str())
    .bind(message)
    .bind(detail)
    .execute(db)
    .await?;
    Ok(())
}

/// Query log entries with optional filters. Returns newest first.
pub async fn query(db: &SqlitePool, params: &LogQuery) -> Result<Vec<LogEntry>, sqlx::Error> {
    let mut sql = String::from(
        "SELECT id, timestamp, level, category, message, detail FROM logs WHERE 1=1",
    );
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref level) = params.level {
        // Filter to this level and above.
        let levels = levels_at_or_above(level);
        if !levels.is_empty() {
            let placeholders: Vec<&str> = levels.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND level IN ({})", placeholders.join(",")));
            binds.extend(levels);
        }
    }

    if let Some(ref cat) = params.category {
        sql.push_str(" AND category = ?");
        binds.push(cat.clone());
    }

    if let Some(ref search) = params.search {
        sql.push_str(" AND (message LIKE ? OR detail LIKE ?)");
        let pattern = format!("%{}%", search);
        binds.push(pattern.clone());
        binds.push(pattern);
    }

    if let Some(before) = params.before_id {
        sql.push_str(" AND id < ?");
        binds.push(before.to_string());
    }

    sql.push_str(" ORDER BY id DESC LIMIT ?");
    binds.push(params.limit.to_string());

    // sqlx doesn't support dynamic bind lists easily, so we use query_as with raw SQL.
    // Build the query manually.
    let rows: Vec<(i64, String, String, String, String, String)> =
        build_dynamic_query(&sql, &binds, db).await?;

    Ok(rows
        .into_iter()
        .map(|(id, timestamp, level, category, message, detail)| LogEntry {
            id,
            timestamp,
            level,
            category,
            message,
            detail,
        })
        .collect())
}

/// Delete logs older than `days` days. Returns number of deleted rows.
pub async fn cleanup(db: &SqlitePool, days: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM logs WHERE timestamp < datetime('now', ?)",
    )
    .bind(format!("-{} days", days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Get total log count (for the UI).
pub async fn count(db: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM logs")
        .fetch_one(db)
        .await?;
    Ok(row.0)
}

/// Get the most recent log ID (for polling).
#[allow(dead_code)]
pub async fn latest_id(db: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT MAX(id) FROM logs").fetch_optional(db).await?;
    Ok(row.map(|r| r.0).unwrap_or(0))
}

/// Get new entries after a given ID (for live polling).
pub async fn entries_after(
    db: &SqlitePool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<LogEntry>, sqlx::Error> {
    let rows: Vec<(i64, String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, timestamp, level, category, message, detail FROM logs WHERE id > ? ORDER BY id DESC LIMIT ?",
    )
    .bind(after_id)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, timestamp, level, category, message, detail)| LogEntry {
            id,
            timestamp,
            level,
            category,
            message,
            detail,
        })
        .collect())
}

fn levels_at_or_above(level: &str) -> Vec<String> {
    let all = ["trace", "debug", "info", "warn", "error"];
    let idx = all.iter().position(|l| l.eq_ignore_ascii_case(level)).unwrap_or(0);
    all[idx..].iter().map(|s| s.to_string()).collect()
}

/// Execute a dynamically-bound query. We chain `.bind()` calls in a loop.
async fn build_dynamic_query(
    sql: &str,
    binds: &[String],
    db: &SqlitePool,
) -> Result<Vec<(i64, String, String, String, String, String)>, sqlx::Error> {
    let mut q = sqlx::query_as::<_, (i64, String, String, String, String, String)>(sql);
    for b in binds {
        q = q.bind(b);
    }
    q.fetch_all(db).await
}
