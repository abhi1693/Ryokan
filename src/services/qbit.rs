use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// qBittorrent Web API client with automatic re-authentication.
#[derive(Clone)]
pub struct QbitClient {
    base_url: String,
    user: String,
    pass: String,
    category: String,
    http: Client,
    logged_in: Arc<Mutex<bool>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Torrent {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub progress: f64,
    pub dlspeed: i64,
    pub state: String,
    pub category: String,
    pub eta: i64,
}

impl QbitClient {
    pub fn new(base_url: &str, user: &str, pass: &str, category: &str) -> Self {
        let http = Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(base_url),
            user: user.to_string(),
            pass: pass.to_string(),
            category: category.to_string(),
            http,
            logged_in: Arc::new(Mutex::new(false)),
        }
    }

    /// Authenticate with qBittorrent.
    pub async fn login(&self) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{}/api/v2/auth/login", self.base_url))
            .form(&[("username", &self.user), ("password", &self.pass)])
            .send()
            .await
            .map_err(|e| format!("qbit login failed: {}", e))?;

        let body = resp.text().await.unwrap_or_default();
        if body == "Fails." {
            return Err("qbit auth failed: invalid credentials".into());
        }

        *self.logged_in.lock().await = true;
        Ok(())
    }

    /// Ensure we're logged in.
    async fn ensure_login(&self) -> Result<(), String> {
        let logged_in = *self.logged_in.lock().await;
        if !logged_in {
            self.login().await?;
        }
        Ok(())
    }

    /// Perform a GET with automatic re-auth on 403.
    async fn do_get(&self, endpoint: &str) -> Result<reqwest::Response, String> {
        self.ensure_login().await?;

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            *self.logged_in.lock().await = false;
            self.login().await?;
            self.http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("retry failed: {}", e))
        } else {
            Ok(resp)
        }
    }

    /// Perform a POST with form data and automatic re-auth on 403.
    async fn do_post_form(&self, endpoint: &str, form: &[(&str, &str)]) -> Result<reqwest::Response, String> {
        self.ensure_login().await?;

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .post(&url)
            .form(form)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            *self.logged_in.lock().await = false;
            self.login().await?;
            self.http
                .post(&url)
                .form(form)
                .send()
                .await
                .map_err(|e| format!("retry failed: {}", e))
        } else {
            Ok(resp)
        }
    }

    /// Add a magnet or torrent URL to qBittorrent.
    pub async fn add_torrent(&self, url: &str) -> Result<(), String> {
        let form = [("urls", url), ("category", &self.category)];
        let resp = self.do_post_form("/api/v2/torrents/add", &form).await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qbit add failed: {}", body));
        }
        Ok(())
    }

    /// Get all torrents, optionally filtered by category.
    pub async fn get_torrents(&self) -> Result<Vec<Torrent>, String> {
        let endpoint = if self.category.is_empty() {
            "/api/v2/torrents/info".to_string()
        } else {
            format!("/api/v2/torrents/info?category={}", self.category)
        };

        let resp = self.do_get(&endpoint).await?;
        let torrents: Vec<Torrent> = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse torrents: {}", e))?;

        Ok(torrents)
    }

    /// Test the connection by fetching the app version.
    pub async fn test_connection(&self) -> Result<String, String> {
        let resp = self.do_get("/api/v2/app/version").await?;
        let version = resp.text().await.unwrap_or_default();
        Ok(version)
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
