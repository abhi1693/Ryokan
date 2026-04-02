use scraper::{Html, Selector};
use serde::Serialize;

const NYAA_BASE: &str = "https://nyaa.si";

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub link: String,
    pub magnet: String,
    pub torrent: String,
    pub size: String,
    pub size_bytes: i64,
    pub seeders: i32,
    pub leechers: i32,
    pub downloads: i32,
    pub group: String,
    pub resolution: String,
    pub is_batch: bool,
    pub is_trusted: bool,
    pub score: i32,
    pub info_hash: String,
}

pub struct SearchOptions {
    pub query: String,
    pub category: String,
    pub filter: String,
    pub user: String,
    pub preferred_groups: Vec<String>,
    pub preferred_resolution: String,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            category: "1_0".to_string(), // Anime - All
            filter: "0".to_string(),
            user: String::new(),
            preferred_groups: Vec::new(),
            preferred_resolution: "1080".to_string(),
        }
    }
}

/// Result of a paginated search.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub page: i32,
    pub has_next: bool,
}

/// Search Nyaa by scraping the HTML results page.
pub async fn search(opts: &SearchOptions, page: i32) -> Result<SearchResponse, String> {
    let mut url = format!(
        "{}/?f={}&c={}&q={}&p={}",
        NYAA_BASE,
        opts.filter,
        opts.category,
        urlencoding::encode(&opts.query),
        page
    );

    if !opts.user.is_empty() {
        url = format!(
            "{}/user/{}?f={}&c={}&q={}&p={}",
            NYAA_BASE,
            urlencoding::encode(&opts.user),
            opts.filter,
            opts.category,
            urlencoding::encode(&opts.query),
            page
        );
    }

    let client = reqwest::Client::new();
    let html = client
        .get(&url)
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("Nyaa request failed: {}", e))?
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    let (results, has_next) = parse_results(&html, opts);
    Ok(SearchResponse { results, page, has_next })
}

fn parse_results(html: &str, opts: &SearchOptions) -> (Vec<SearchResult>, bool) {
    let document = Html::parse_document(html);
    let row_sel = Selector::parse("table.torrent-list tbody tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let a_sel = Selector::parse("a").unwrap();

    let mut results = Vec::new();

    for row in document.select(&row_sel) {
        let tds: Vec<_> = row.select(&td_sel).collect();
        if tds.len() < 8 {
            continue;
        }

        // Category td is index 0, name td is index 1.
        let name_td = tds[1];
        let links: Vec<_> = name_td.select(&a_sel).collect();

        // Find the last non-comment link as the title link.
        let title_link = links.iter().rev().find(|a| {
            a.value()
                .attr("href")
                .map(|h| h.starts_with("/view/"))
                .unwrap_or(false)
        });

        let (title, link) = match title_link {
            Some(a) => {
                let title = a.text().collect::<String>().trim().to_string();
                let href = a.value().attr("href").unwrap_or("");
                let link = format!("{}{}", NYAA_BASE, href);
                (title, link)
            }
            None => continue,
        };

        // Torrent and magnet links (td index 2).
        let link_td = tds[2];
        let link_anchors: Vec<_> = link_td.select(&a_sel).collect();
        let torrent = link_anchors
            .iter()
            .find_map(|a| {
                let href = a.value().attr("href").unwrap_or("");
                if href.ends_with(".torrent") {
                    Some(format!("{}{}", NYAA_BASE, href))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let magnet = link_anchors
            .iter()
            .find_map(|a| {
                let href = a.value().attr("href").unwrap_or("");
                if href.starts_with("magnet:") {
                    Some(href.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Size (td index 3).
        let size = tds[3].text().collect::<String>().trim().to_string();
        let size_bytes = parse_size(&size);

        // Seeders, leechers, downloads (td indices 5, 6, 7).
        let seeders = parse_int(&tds[5].text().collect::<String>());
        let leechers = parse_int(&tds[6].text().collect::<String>());
        let downloads = parse_int(&tds[7].text().collect::<String>());

        // Trusted/remake detection from row class.
        let row_class = row.value().attr("class").unwrap_or("");
        let is_trusted = row_class.contains("success");

        // Extract group, resolution, batch, hash from title/magnet.
        let group = extract_group(&title);
        let resolution = extract_resolution(&title);
        let is_batch = detect_batch(&title);
        let info_hash = extract_hash(&magnet);

        let mut result = SearchResult {
            title,
            link,
            magnet,
            torrent,
            size,
            size_bytes,
            seeders,
            leechers,
            downloads,
            group,
            resolution,
            is_batch,
            is_trusted,
            score: 0,
            info_hash,
        };

        result.score = crate::services::scoring::score_result(&result, opts);
        results.push(result);
    }

    // Sort by score descending.
    results.sort_by(|a, b| b.score.cmp(&a.score));

    // Detect if there's a next page.
    let has_next = {
        let next_sel = Selector::parse("ul.pagination li.next:not(.disabled)").unwrap();
        let pagination_exists = document.select(&next_sel).next().is_some();
        // Fallback: if we got 75 results (full page), assume there might be more.
        pagination_exists || results.len() >= 75
    };

    (results, has_next)
}

fn extract_group(title: &str) -> String {
    if let Some(start) = title.find('[') {
        if let Some(end) = title[start..].find(']') {
            return title[start + 1..start + end].to_string();
        }
    }
    String::new()
}

fn extract_resolution(title: &str) -> String {
    let lower = title.to_lowercase();
    for res in &["2160", "1080", "720", "480"] {
        if lower.contains(&format!("{}p", res)) || lower.contains(&format!("{}i", res)) {
            return res.to_string();
        }
    }
    String::new()
}

fn detect_batch(title: &str) -> bool {
    let lower = title.to_lowercase();
    lower.contains("batch")
        || lower.contains("complete")
        || {
            // Match patterns like "01-12", "01~24", "S01 Complete".
            let re = regex_lite::Regex::new(r"(?i)(01[-~]\d{2,3}|s\d+\s*complete|\d{2,3}\s*[-~]\s*\d{2,3})").unwrap();
            re.is_match(&lower)
        }
}

fn extract_hash(magnet: &str) -> String {
    if let Some(pos) = magnet.find("btih:") {
        let rest = &magnet[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return rest[..end].to_lowercase();
    }
    String::new()
}

fn parse_size(s: &str) -> i64 {
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 2 {
        return 0;
    }
    let num: f64 = parts[0].parse().unwrap_or(0.0);
    match parts[1].to_uppercase().as_str() {
        "B" | "BYTES" => num as i64,
        "KIB" | "KB" => (num * 1024.0) as i64,
        "MIB" | "MB" => (num * 1024.0 * 1024.0) as i64,
        "GIB" | "GB" => (num * 1024.0 * 1024.0 * 1024.0) as i64,
        "TIB" | "TB" => (num * 1024.0 * 1024.0 * 1024.0 * 1024.0) as i64,
        _ => 0,
    }
}

fn parse_int(s: &str) -> i32 {
    s.trim().parse().unwrap_or(0)
}
