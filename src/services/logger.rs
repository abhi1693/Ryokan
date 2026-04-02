use sqlx::SqlitePool;

use crate::models::log::{self, LogCategory, LogLevel};

/// Application logger that writes to both SQLite and tracing.
///
/// Usage:
///   logger::info(&db, LogCategory::Nyaa, "Search completed", "Found 42 results for 'Dandadan'").await;
///   logger::error(&db, LogCategory::QBit, "Connection failed", &err.to_string()).await;

pub async fn log(db: &SqlitePool, level: LogLevel, category: LogCategory, message: &str, detail: &str) {
    // Write to tracing (console/container log).
    match level {
        LogLevel::Trace => tracing::trace!(category = category.as_str(), detail = detail, "{}", message),
        LogLevel::Debug => tracing::debug!(category = category.as_str(), detail = detail, "{}", message),
        LogLevel::Info => tracing::info!(category = category.as_str(), detail = detail, "{}", message),
        LogLevel::Warn => tracing::warn!(category = category.as_str(), detail = detail, "{}", message),
        LogLevel::Error => tracing::error!(category = category.as_str(), detail = detail, "{}", message),
    }

    // Write to SQLite. Don't propagate errors — logging should never crash the app.
    if let Err(e) = log::insert(db, level, category, message, detail).await {
        tracing::error!("Failed to write log to database: {}", e);
    }
}

#[allow(dead_code)]
pub async fn trace(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Trace, category, message, detail).await;
}

pub async fn debug(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Debug, category, message, detail).await;
}

pub async fn info(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Info, category, message, detail).await;
}

pub async fn warn(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Warn, category, message, detail).await;
}

pub async fn error(db: &SqlitePool, category: LogCategory, message: &str, detail: &str) {
    log(db, LogLevel::Error, category, message, detail).await;
}
