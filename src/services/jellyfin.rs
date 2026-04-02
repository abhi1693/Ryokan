use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct JellyfinClient {
    base_url: String,
    api_key: String,
    http: Client,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemInfo {
    #[serde(default, rename = "ServerName")]
    pub server_name: String,
    #[serde(default, rename = "Version")]
    pub version: String,
    #[serde(default, rename = "Id")]
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JellyfinItem {
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Name")]
    pub name: String,
    #[serde(default, rename = "Path")]
    pub path: String,
    #[serde(default, rename = "Type")]
    pub item_type: String,
    #[serde(default, rename = "ProductionYear")]
    pub production_year: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct JellyfinItemsResponse {
    #[serde(default, rename = "Items")]
    items: Vec<JellyfinItem>,
}

impl JellyfinClient {
    pub fn new(url: &str, api_key: &str) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(url),
            api_key: api_key.trim().to_string(),
            http,
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.api_key.is_empty()
    }

    pub fn web_base_url(&self) -> &str {
        &self.base_url
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, endpoint: &str, query: &[(&str, String)]) -> Result<T, String> {
        if !self.is_configured() {
            return Err("Jellyfin is not configured".to_string());
        }

        let url = format!("{}{}", self.base_url, endpoint);
        let mut req = self
            .http
            .get(url)
            .header("X-Emby-Token", &self.api_key)
            .header("Accept", "application/json");

        if !query.is_empty() {
            req = req.query(query);
        }

        let resp = req.send().await.map_err(|e| format!("Jellyfin request failed: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Jellyfin request failed ({}): {}", status, truncate(&body)));
        }

        resp.json::<T>().await.map_err(|e| format!("Jellyfin response parse failed: {}", e))
    }

    async fn post_empty(&self, endpoint: &str) -> Result<(), String> {
        if !self.is_configured() {
            return Err("Jellyfin is not configured".to_string());
        }

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .post(url)
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await
            .map_err(|e| format!("Jellyfin request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Jellyfin request failed ({}): {}", status, truncate(&body)));
        }

        Ok(())
    }

    pub async fn test_connection(&self) -> Result<SystemInfo, String> {
        self.get("/System/Info/Public", &[]).await
    }

    pub async fn refresh_library(&self) -> Result<(), String> {
        self.post_empty("/Library/Refresh").await
    }

    pub async fn find_items(&self, search_term: &str) -> Result<Vec<JellyfinItem>, String> {
        if search_term.trim().is_empty() {
            return Ok(Vec::new());
        }

        let resp: JellyfinItemsResponse = self
            .get(
                "/Items",
                &[
                    ("Recursive", "true".to_string()),
                    ("IncludeItemTypes", "Series,Season,Movie,BoxSet".to_string()),
                    ("SearchTerm", search_term.trim().to_string()),
                    ("Limit", "12".to_string()),
                    ("Fields", "Path".to_string()),
                ],
            )
            .await?;

        Ok(resp.items)
    }
}

fn truncate(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() > 180 {
        format!("{}...", &trimmed[..180])
    } else if trimmed.is_empty() {
        "empty response".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }

    let lower = trimmed.to_ascii_lowercase();
    let is_local = lower.starts_with("localhost")
        || lower.starts_with("127.")
        || lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.16.")
        || lower.starts_with("172.17.")
        || lower.starts_with("172.18.")
        || lower.starts_with("172.19.")
        || lower.starts_with("172.2")
        || lower.starts_with("172.30.")
        || lower.starts_with("172.31.");

    if is_local {
        format!("http://{}", trimmed)
    } else {
        format!("https://{}", trimmed)
    }
}
