use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;

const NYAA_BASE: &str = "https://nyaa.si";

/// Process-global `reqwest::Client` for Nyaa search requests. A fresh
/// `Client` per search throws away keep-alive connections and forces a
/// new TLS handshake every call — Nyaa gets hit many times a minute
/// between RSS sync, auto-search, upgrade sweeps, and interactive
/// search, and the per-request client was needless overhead. A 30-second
/// per-call timeout caps the damage from a single hung connection so
/// the outer RSS/upgrade-search timeouts aren't the only backstop.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the Nyaa search reqwest client should not fail")
});

/// Pre-compiled scraper selectors for Nyaa search result rows. The old
/// code re-parsed these three strings per `parse_results` call.
static SEL_ROW: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("table.torrent-list tbody tr").expect("SEL_ROW parses"));
static SEL_TD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("td").expect("SEL_TD parses"));
static SEL_A: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").expect("SEL_A parses"));
static SEL_NEXT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("ul.pagination li.next:not(.disabled)").expect("SEL_NEXT parses")
});

/// Pre-compiled selectors for the single-torrent view page
/// (`/view/<id>`). Used by [`fetch_view_result`] for the SeaDex-bypass
/// path that ingests curated torrents directly from their view URLs
/// instead of going through the text search.
static SEL_VIEW_TITLE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("div.panel h3.panel-title").expect("SEL_VIEW_TITLE parses")
});
static SEL_VIEW_ROW: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.panel-body div.row").expect("SEL_VIEW_ROW parses"));
// Target Nyaa's actual Bootstrap grid columns (`col-md-1`, `col-md-5`, etc.)
// rather than every `<div>` in the row. The broader `div` selector also
// descended into nested `<div>`s like embedded MediaInfo blocks, which
// made the label/value pair-up (`while i + 1 < cols.len()` in
// `parse_view_page`) drift and silently zero out seeder/leecher counts
// on view pages that had any extra inner markup.
static SEL_VIEW_COL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("div[class*='col-md-']").expect("SEL_VIEW_COL parses")
});
static SEL_VIEW_MAGNET: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a.card-footer-item[href^='magnet:']").expect("SEL_VIEW_MAGNET parses")
});
static SEL_VIEW_TORRENT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a.card-footer-item[href$='.torrent']")
        .expect("SEL_VIEW_TORRENT parses")
});

/// Episode range like "01-12", "01~24", "1 - 24". Broader than the old
/// `01[-~]\d{2,3}` hard-coded form so releases that start at a non-01
/// episode (sequels, cour splits) still register as batches.
static BATCH_RANGE_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"(?i)\b\d{1,3}\s*[-~]\s*\d{2,3}\b").expect("BATCH_RANGE_RE parses")
});

/// Bare season marker: `S1`, `S01`, `Season 1`, etc. A season marker on
/// its own — without a paired single-episode indicator — means the
/// release covers the whole season. This is how most BD packs from
/// high-quality groups (MTBB, Okay-Subs, Sephirotic, YURASUKA, neoDESU)
/// are titled: `[Group] Show S1 (BD 1080p)` or `[Group] Show [Season 1]`.
static SEASON_MARKER_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"(?i)\b(s\d{1,2}|season\s*\d+)\b").expect("SEASON_MARKER_RE parses")
});

/// Bare Roman-numeral season marker: `II`, `III`, `IV`, `V`, `VI`,
/// `VII`, `VIII`, `IX`, `X`. Common in anime sequel titles that spell
/// the season out (`Mob Psycho 100 III`, `Overlord IV`, `KanColle II`)
/// — SeaDex and many BD groups use this form, so without it the batch
/// heuristic misses entire season packs.
///
/// Case-sensitive (uppercase only) to avoid matching lowercase letter
/// sequences like `ix` or `vi` that could appear inside words. `I`
/// alone is excluded — too noisy (pronoun, initialisms). Applied to
/// the raw title, not the lowercased form used by the other batch
/// checks.
static ROMAN_SEASON_MARKER_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"\b(II|III|IV|V|VI|VII|VIII|IX|X)\b")
        .expect("ROMAN_SEASON_MARKER_RE parses")
});

/// Single-episode indicator. If any of these hit, the release is
/// scoped to one episode (or a very small multi-ep span) and should
/// NOT be flagged as a batch even if a season marker is present.
/// Patterns covered:
///   - `S01E12`, `S1E05` — Western-style
///   - ` - 12`, ` - 24.5` — classic fansub single-ep suffix
///   - `Ep 12`, `Ep. 12`, `Episode 12`
///   - `#12`
static SINGLE_EP_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(
        r"(?i)(s\d{1,2}e\d{1,3}|\s-\s*\d{1,3}(?:\.\d+)?\b|\bep\.?\s*\d{1,3}\b|\bepisode\s*\d{1,3}\b|#\d{1,3})",
    )
    .expect("SINGLE_EP_RE parses")
});

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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
    /// Release group extracted via anitomy. Kept as `String` for backward-
    /// compat with the old ad-hoc bracket parse; empty string means "no
    /// group detected."
    pub group: String,
    /// Resolution as a bare digit string ("1080", "720", …) or empty. Kept
    /// for UI callers that render just the resolution tag; richer callers
    /// should use `quality_label` which encodes source+resolution+sub-tier.
    pub resolution: String,
    /// Pre-computed Sonarr-parity label (`WEB-1080p`, `BD-1080p Remux`,
    /// etc.) produced from the same [`crate::services::source::ClassificationResult::label`]
    /// logic as the grab-side pipeline, so the value the user sees in
    /// interactive search equals the value persisted once grabbed.
    /// Empty when neither source nor resolution was determined.
    pub quality_label: String,
    /// Source enum as a string (`"Web"`, `"BluRay"`, …) or empty when
    /// unknown. Mirrors `Source::as_str()` exactly.
    pub source: String,
    /// Web sub-variant (`"WEB-DL"`, `"WEBRip"`, or empty for Unknown).
    /// Only meaningful when `source == "Web"`.
    pub web_kind: String,
    pub is_remux: bool,
    pub is_bdmv: bool,
    pub is_batch: bool,
    pub is_trusted: bool,
    pub score: i32,
    pub info_hash: String,
}

#[derive(Clone)]
pub struct SearchOptions {
    pub query: String,
    pub category: String,
    pub filter: String,
    pub user: String,
    pub preferred_groups: Vec<String>,
    pub preferred_resolution: String,
    pub prefer_subs: bool,
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
            prefer_subs: true,
        }
    }
}

/// Result of a paginated search.
#[derive(Debug, Serialize, utoipa::ToSchema)]
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

    let html = HTTP_CLIENT
        .get(&url)
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

    let mut results = Vec::new();

    for row in document.select(&SEL_ROW) {
        let tds: Vec<_> = row.select(&SEL_TD).collect();
        if tds.len() < 8 {
            continue;
        }

        // Category td is index 0, name td is index 1.
        let name_td = tds[1];
        let links: Vec<_> = name_td.select(&SEL_A).collect();

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
        let link_anchors: Vec<_> = link_td.select(&SEL_A).collect();
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

        // Filename-layer classification (anitomy + source-token scan).
        // Drops the old ad-hoc bracket/regex extract and mirrors what the
        // grab-side pipeline's Layer 1 produces, so the label the user
        // sees in interactive search equals the value persisted on grab.
        let classified = classify_search_result(&title);
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
            group: classified.group,
            resolution: classified.resolution,
            quality_label: classified.quality_label,
            source: classified.source,
            web_kind: classified.web_kind,
            is_remux: classified.is_remux,
            is_bdmv: classified.is_bdmv,
            is_batch,
            is_trusted,
            score: 0,
            info_hash,
        };

        result.score = crate::services::scoring::score_result_with_sub_pref(&result, opts, opts.prefer_subs);
        results.push(result);
    }

    // Sort by score descending.
    results.sort_by(|a, b| b.score.cmp(&a.score));

    // Detect if there's a next page.
    let has_next = {
        let pagination_exists = document.select(&SEL_NEXT).next().is_some();
        // Fallback: if we got 75 results (full page), assume there might be more.
        pagination_exists || results.len() >= 75
    };

    (results, has_next)
}

/// Classification-derived fields for a single Nyaa row. Bundles the
/// values that used to come from three separate ad-hoc extractors with
/// the richer label the template now renders directly, so `parse_results`
/// only touches one helper per row.
struct ClassifiedFields {
    group: String,
    resolution: String,
    quality_label: String,
    source: String,
    web_kind: String,
    is_remux: bool,
    is_bdmv: bool,
}

/// Run the filename classifier over a release title and reshape the
/// output for [`SearchResult`]. Mirrors the backend's
/// [`crate::services::source::ClassificationResult::label`] so the UI
/// label in interactive search matches the value a grab would persist.
///
/// The group-map (Layer 3) lookup is not done here — it's async and the
/// parser is sync. Interactive paths that want Layer 3 enrichment call
/// [`enrich_results_with_group_map`] after parsing.
fn classify_search_result(title: &str) -> ClassifiedFields {
    use crate::services::source::{ClassificationResult, DecisionRule, Source, Resolution};
    use crate::services::source_filename::classify_filename;

    let fc = classify_filename(title);

    // Reduce the filename-layer evidence down to a winning source the
    // same way the multi-layer aggregator would if this were the only
    // layer's output. We don't need confidence/needs_review — we only
    // want a source token for the label — so take the highest-confidence
    // piece of evidence and use its source directly.
    let mut winning_source = Source::Unknown;
    let mut best_conf = 0.0_f32;
    for e in &fc.evidence {
        if e.confidence > best_conf {
            winning_source = e.source;
            best_conf = e.confidence;
        }
    }

    let cls = ClassificationResult {
        source: winning_source,
        resolution: fc.resolution,
        is_remux: fc.is_remux,
        web_kind: fc.web_kind,
        is_bdmv: fc.is_bdmv,
        confidence: best_conf,
        needs_review: false,
        evidence: Vec::new(),
        decision_rule: DecisionRule::default(),
    };

    let quality_label = match cls.label().as_str() {
        "Unknown" => String::new(),
        other => other.to_string(),
    };

    // Bare-digit resolution ("1080") for back-compat with existing
    // templates that render `{{ r.resolution }}p` tags.
    let resolution = match fc.resolution {
        Resolution::Unknown => String::new(),
        r => r.as_str().trim_end_matches('p').to_string(),
    };

    ClassifiedFields {
        group: fc.release_group.unwrap_or_default(),
        resolution,
        quality_label,
        source: match winning_source {
            Source::Unknown => String::new(),
            other => other.as_str().to_string(),
        },
        web_kind: fc.web_kind.as_str().to_string(),
        is_remux: fc.is_remux,
        is_bdmv: fc.is_bdmv,
    }
}

/// Enrich already-parsed search results with Layer 3 (group identity
/// table) signals. Walks each result whose filename classifier didn't
/// produce a source, looks up the group in `group_source_map`, and fills
/// in `source` / `quality_label` when the group is known.
///
/// No-op for results that already have a filename-derived source — the
/// filename is more specific than the group map (e.g. a SubsPlease
/// release explicitly tagged "BluRay" remains BluRay, even though the
/// group map says SubsPlease == Web).
///
/// Call this from interactive search handlers after `nyaa::search` —
/// auto-search runs the full source pipeline downstream so it doesn't
/// need the extra call.
pub async fn enrich_results_with_group_map(
    db: &sqlx::SqlitePool,
    results: &mut [SearchResult],
) {
    use crate::services::source::{Resolution, Source};
    use crate::services::source_groups::classify_group;

    // Small per-batch cache so we only hit the DB once per unique group
    // across a typical 75-row result page.
    let mut seen: std::collections::HashMap<
        String,
        Option<(Source, crate::services::source::WebKind)>,
    > = std::collections::HashMap::new();

    for r in results.iter_mut() {
        if !r.source.is_empty() || r.group.is_empty() {
            continue;
        }
        let group_key = r.group.to_ascii_lowercase();
        let group_hint = if let Some(cached) = seen.get(&group_key) {
            *cached
        } else {
            let looked_up = classify_group(db, &r.group)
                .await
                .map(|cls| (cls.evidence.source, cls.web_kind));
            seen.insert(group_key, looked_up);
            looked_up
        };

        if let Some((src, web_kind)) = group_hint {
            r.source = src.as_str().to_string();
            // Rebuild quality_label now that source is known. Resolution
            // string is bare digits ("1080"); translate back into the
            // Resolution enum for label formatting.
            let res_enum = if r.resolution.is_empty() {
                Resolution::Unknown
            } else {
                Resolution::from_str(&format!("{}p", r.resolution))
            };
            // Web releases unify the WebDl and bare-WEB sub-tiers into
            // a single "WEB" label (issue #48) — matches
            // `ClassificationResult::label()`. WebRip stays distinct
            // because it's the lower-quality sub-tier power users want
            // to spot.
            let source_label = match src {
                Source::Web => match web_kind {
                    crate::services::source::WebKind::WebRip => "WEBRip".to_string(),
                    crate::services::source::WebKind::Unknown
                    | crate::services::source::WebKind::WebDl => "WEB".to_string(),
                },
                Source::BluRay => "BD".to_string(),
                other => other.as_str().to_string(),
            };
            r.quality_label = match (source_label.as_str(), res_enum) {
                ("", Resolution::Unknown) => String::new(),
                (s, Resolution::Unknown) => s.to_string(),
                ("", r) => r.as_str().to_string(),
                (s, r) => format!("{}-{}", s, r.as_str()),
            };
        }
    }
}

fn detect_batch(title: &str) -> bool {
    let lower = title.to_lowercase();

    // Explicit batch keywords.
    if lower.contains("batch") || lower.contains("complete") {
        return true;
    }

    // Numeric episode ranges like "01-12", "01~24", "1 - 24".
    if BATCH_RANGE_RE.is_match(&lower) {
        return true;
    }

    // Season marker with no single-episode indicator — the dominant
    // batch form for BD season packs: `Show S1 (BD 1080p)`, `Show
    // [Season 1] [BD 1080p]`, `Show.S01.1080p.BluRay...`,
    // `Mob Psycho 100 III (BD 1080p)`. The single-ep guard keeps
    // `Show S01E12` / `Show S1 - 12` off the batch path.
    //
    // The Roman-numeral check runs against the raw title (not `lower`)
    // because the regex is case-sensitive — see ROMAN_SEASON_MARKER_RE.
    let has_season_marker = SEASON_MARKER_RE.is_match(&lower)
        || ROMAN_SEASON_MARKER_RE.is_match(title);
    if has_season_marker && !SINGLE_EP_RE.is_match(&lower) {
        return true;
    }

    false
}

fn extract_hash(magnet: &str) -> String {
    // Two quirks the prior impl got wrong:
    //   1. BTIH URN is case-insensitive (`urn:btih:` and `urn:BTIH:`
    //      both occur in the wild); searching for a lowercase literal
    //      missed uppercase variants entirely and returned "".
    //   2. 40-char hex hashes are case-insensitive, but 32-char base32
    //      hashes (uppercase A-Z + 2-7 only) are case-SENSITIVE.
    //      Lowercasing base32 corrupts the info-hash, which breaks
    //      dedup, blocklist re-grab detection, and SeaDex matching for
    //      any curated release whose magnet happens to use base32.
    let lower = magnet.to_ascii_lowercase();
    let Some(pos) = lower.find("btih:") else {
        return String::new();
    };
    let payload = &magnet[pos + 5..];
    let end = payload.find('&').unwrap_or(payload.len());
    let hash = &payload[..end];

    match hash.len() {
        40 => hash.to_ascii_lowercase(),
        32 => hash.to_string(),
        // Any other length is a malformed BTIH — not a valid
        // info-hash. The lowercase fallthrough preserves the prior
        // behaviour of returning *something* to downstream code
        // rather than silently swallowing the input; a future
        // stricter version could `return String::new()` here for
        // defensive rejection without breaking any caller that
        // already treats "" as "no hash".
        _ => hash.to_ascii_lowercase(),
    }
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

/// Fetch a single Nyaa view page by URL and return a populated
/// [`SearchResult`]. Used by the SeaDex bypass path in auto-search:
/// SeaDex tells us the curated torrent's info hash and view URL for a
/// given AniList ID, but the torrent's title may not contain any of
/// the query tokens (smol's Kizumonogatari pack is titled
/// `[smol] Monogatari (Season 9) ...` so searches for "Kizumonogatari
/// II: Nekketsu-hen" never surface it). Going direct to the view page
/// sidesteps the whole text-match problem.
///
/// The parser extracts the same fields `parse_results` extracts from a
/// search-listing row: title, seeders/leechers/completed, size,
/// magnet, torrent file URL, info hash. `group`, `resolution`, and
/// `is_batch` are derived from the title exactly the same way as in
/// `parse_results`. `score` is computed with the passed options so the
/// caller can merge these into the normal candidate pool seamlessly.
pub async fn fetch_view_result(
    view_url: &str,
    opts: &SearchOptions,
) -> Result<SearchResult, String> {
    let html = HTTP_CLIENT
        .get(view_url)
        .send()
        .await
        .map_err(|e| format!("Nyaa view fetch failed: {}", e))?
        .text()
        .await
        .map_err(|e| format!("Failed to read view body: {}", e))?;

    parse_view_page(&html, view_url, opts)
        .ok_or_else(|| "Nyaa view page parse failed".to_string())
}

fn parse_view_page(
    html: &str,
    view_url: &str,
    opts: &SearchOptions,
) -> Option<SearchResult> {
    let document = Html::parse_document(html);

    // Title is the first `<h3 class="panel-title">` under the first
    // `.panel` — the second instance is the "File list" header.
    let title = document
        .select(&SEL_VIEW_TITLE)
        .next()?
        .text()
        .collect::<String>()
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }

    // Scrape the labelled key/value rows in the header panel. Nyaa lays
    // them out as `<div class="row"><div class="col-md-1">Label:</div>
    // <div class="col-md-5">value</div> …</div>`, with a second
    // (label, value) pair on the same row for Leechers/Completed/etc.
    let mut seeders = 0i32;
    let mut leechers = 0i32;
    let mut downloads = 0i32;
    let mut size = String::new();
    for row in document.select(&SEL_VIEW_ROW) {
        let cols: Vec<_> = row
            .select(&SEL_VIEW_COL)
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // We want pairs: each "Label:" should be followed by its value.
        let mut i = 0;
        while i + 1 < cols.len() {
            let label = cols[i].trim_end_matches(':').trim().to_ascii_lowercase();
            let value = cols[i + 1].trim().to_string();
            match label.as_str() {
                "seeders" => seeders = parse_int(&value),
                "leechers" => leechers = parse_int(&value),
                "completed" => downloads = parse_int(&value),
                "file size" => size = value,
                _ => {}
            }
            i += 2;
        }
    }

    let size_bytes = parse_size(&size);

    // Magnet: first `a.card-footer-item` with a `magnet:` href. Info
    // hash comes from the same magnet via `extract_hash`.
    let magnet = document
        .select(&SEL_VIEW_MAGNET)
        .next()
        .and_then(|a| a.value().attr("href"))
        .unwrap_or("")
        .to_string();
    let info_hash = extract_hash(&magnet);

    // Torrent file URL: sibling `.card-footer-item` ending in .torrent.
    // Paths on nyaa.si are relative, so prefix NYAA_BASE when needed.
    let torrent = document
        .select(&SEL_VIEW_TORRENT)
        .next()
        .and_then(|a| a.value().attr("href"))
        .map(|href| {
            if href.starts_with("http") {
                href.to_string()
            } else {
                format!("{}{}", NYAA_BASE, href)
            }
        })
        .unwrap_or_default();

    let classified = classify_search_result(&title);
    let is_batch = detect_batch(&title);

    let mut result = SearchResult {
        title,
        link: view_url.to_string(),
        magnet,
        torrent,
        size,
        size_bytes,
        seeders,
        leechers,
        downloads,
        group: classified.group,
        resolution: classified.resolution,
        quality_label: classified.quality_label,
        source: classified.source,
        web_kind: classified.web_kind,
        is_remux: classified.is_remux,
        is_bdmv: classified.is_bdmv,
        is_batch,
        // We don't get the row-class `success` tag from a view page, so
        // the trusted flag stays false. Not a problem for the SeaDex
        // path because the SeaDex boost dominates any trusted bonus.
        is_trusted: false,
        score: 0,
        info_hash,
    };

    result.score = crate::services::scoring::score_result_with_sub_pref(
        &result,
        opts,
        opts.prefer_subs,
    );

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal fixture mirroring the real Nyaa view page structure we
    /// saw for the smol Kizumonogatari megapack (`/view/1713886`). This
    /// keeps `parse_view_page` tied to the actual DOM shape rather than
    /// the assumptions the parser makes in isolation. If Nyaa ever
    /// renumbers the column layout, this test fails loudly.
    const SMOL_VIEW_FIXTURE: &str = r#"
<html><body>
<div class="panel panel-default">
  <div class="panel-heading">
    <h3 class="panel-title">
      [smol] Monogatari (Season 9) (BD 1080p 1920x816 HEVC Opus) | Kizumonogatari | Monogatari Series | Kizumonogatari: Tekketsu-hen | Kizumonogatari: Nekketsu-hen | Kizumonogatari: Reiketsu-hen
    </h3>
  </div>
  <div class="panel-body">
    <div class="row">
      <div class="col-md-1">Category:</div>
      <div class="col-md-5"><a href="/?c=1_0">Anime</a> - <a href="/?c=1_2">English-translated</a></div>
      <div class="col-md-1">Date:</div>
      <div class="col-md-5" data-timestamp="1694025140">2023-09-06 18:32 UTC</div>
    </div>
    <div class="row">
      <div class="col-md-1">Submitter:</div>
      <div class="col-md-5"><a class="text-default" href="/user/smol">smol</a></div>
      <div class="col-md-1">Seeders:</div>
      <div class="col-md-5"><span style="color: green;">51</span></div>
    </div>
    <div class="row">
      <div class="col-md-1">Information:</div>
      <div class="col-md-5"><a href="https://anidb.net/anime/8357">https://anidb.net/anime/8357</a></div>
      <div class="col-md-1">Leechers:</div>
      <div class="col-md-5"><span style="color: red;">0</span></div>
    </div>
    <div class="row">
      <div class="col-md-1">File size:</div>
      <div class="col-md-5">23.8 GiB</div>
      <div class="col-md-1">Completed:</div>
      <div class="col-md-5">2286</div>
    </div>
    <div class="row">
      <div class="col-md-offset-6 col-md-1">Info hash:</div>
      <div class="col-md-5"><kbd>0f8ee3286d768fb53ae593f10155a5077e38e893</kbd></div>
    </div>
  </div>
  <div class="panel-footer clearfix">
    <a href="/download/1713886.torrent" class="card-footer-item">Download Torrent</a>
    or
    <a href="magnet:?xt=urn:btih:0f8ee3286d768fb53ae593f10155a5077e38e893&amp;dn=smol+pack" class="card-footer-item">Magnet</a>
  </div>
</div>
</body></html>
"#;

    #[test]
    fn parse_view_page_extracts_smol_pack_metadata() {
        let opts = SearchOptions::default();
        let result = parse_view_page(SMOL_VIEW_FIXTURE, "https://nyaa.si/view/1713886", &opts)
            .expect("parser should succeed on a well-formed view page");

        assert!(
            result.title.contains("smol") && result.title.contains("Kizumonogatari"),
            "title should be scraped from the header panel, got {:?}",
            result.title
        );
        assert_eq!(result.seeders, 51);
        assert_eq!(result.leechers, 0);
        assert_eq!(result.downloads, 2286);
        assert_eq!(result.size, "23.8 GiB");
        assert!(result.size_bytes > 20 * 1024 * 1024 * 1024, "size_bytes should parse to GiB range");
        assert_eq!(
            result.info_hash,
            "0f8ee3286d768fb53ae593f10155a5077e38e893",
            "info_hash should be extracted from the magnet link"
        );
        assert!(
            result.magnet.starts_with("magnet:?"),
            "magnet link should be captured, got {:?}",
            result.magnet
        );
        assert_eq!(result.torrent, "https://nyaa.si/download/1713886.torrent");
        assert_eq!(result.link, "https://nyaa.si/view/1713886");
        // `detect_batch` fires on the season marker in the title.
        assert!(result.is_batch, "smol pack titled with Season N should be flagged as batch");
        assert_eq!(result.resolution, "1080");
        assert_eq!(result.group, "smol");
    }

    // ── detect_batch — Roman-numeral season markers ──────────────────────
    //
    // SeaDex's curated picks for anime sequels frequently use Roman-numeral
    // season markers in the title (`Mob Psycho 100 III`, `Overlord IV`,
    // `KanColle II`). Before these tests were added, detect_batch missed
    // those entirely because SEASON_MARKER_RE only recognised `S\d+` /
    // `Season \d+` forms, so the curated pack got silently dropped at the
    // `candidates.retain(|c| c.is_batch)` filter in
    // `find_best_batch_for_target`.

    #[test]
    fn detect_batch_roman_numeral_season_marker_iii() {
        // The regression case from the PR #47 session: the DIY full-season
        // BD pack for Mob Psycho 100 III.
        assert!(detect_batch(
            "[DIY] Mob Psycho 100 III (BD 1080p HEVC FLAC) [Dual-Audio]"
        ));
    }

    #[test]
    fn detect_batch_roman_numeral_season_marker_ii_and_iv() {
        assert!(detect_batch("[MTBB] KanColle II (BD 1080p)"));
        assert!(detect_batch("[smol] Overlord IV (BD 1080p)"));
    }

    #[test]
    fn detect_batch_roman_numeral_with_single_ep_is_not_batch() {
        // Single-ep guard must still fire: a Roman-numeral season marker
        // paired with a per-episode indicator is an individual episode
        // release, not a batch.
        assert!(
            !detect_batch("[Group] Mob Psycho 100 III - 05 (1080p)"),
            "Roman season marker + single-ep suffix must not be a batch"
        );
        assert!(
            !detect_batch("[Group] Overlord IV Ep 12 (1080p)"),
            "Roman season marker + Ep N must not be a batch"
        );
    }

    #[test]
    fn detect_batch_lowercase_roman_numerals_are_not_matched() {
        // Case-sensitive on purpose: lowercase "ii"/"iii"/"ix" etc. could
        // appear inside words and would false-positive if we accepted them.
        // Torrent titles conventionally use uppercase Roman numerals, so
        // we don't pay for that false-positive risk.
        assert!(
            !detect_batch("[Group] some title iii (1080p)"),
            "lowercase roman numerals must not trigger the batch heuristic"
        );
    }

    #[test]
    fn detect_batch_single_i_does_not_fire() {
        // `I` alone is excluded from the Roman regex — too ambiguous
        // (pronoun, initial, etc.). A title with a bare `I` and no other
        // batch signal must stay off the batch path.
        assert!(
            !detect_batch("[Group] Show I vs Y (some subtitle)"),
            "bare `I` must not be treated as a season marker"
        );
    }

    #[test]
    fn extract_hash_lowercases_hex() {
        let magnet = "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=thing";
        assert_eq!(
            extract_hash(magnet),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn extract_hash_accepts_uppercase_prefix() {
        // Real-world magnets occasionally use `urn:BTIH:`; prior impl
        // returned "" for this and broke downstream lookups.
        let magnet = "magnet:?xt=urn:BTIH:ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        assert_eq!(
            extract_hash(magnet),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn extract_hash_preserves_base32_case() {
        // 32-char base32 info-hashes are case-sensitive. Prior impl
        // .to_lowercase()'d them, producing a hash that didn't match
        // what qBit actually stored.
        let base32 = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let magnet = format!("magnet:?xt=urn:btih:{base32}&dn=thing");
        assert_eq!(extract_hash(&magnet), base32);
    }

    #[test]
    fn extract_hash_no_prefix_returns_empty() {
        assert_eq!(extract_hash("https://example.com/t.torrent"), "");
        assert_eq!(extract_hash(""), "");
    }

    // ── #24 — anitomy-derived classification on SearchResult ──────────────

    #[test]
    fn classify_search_result_subsplease_1080p_webdl_from_filename_tokens() {
        // SubsPlease's own filename is silent on source — just "1080p",
        // no WEB/WEBDL token. Layer 1 (filename) should still surface
        // 1080p + empty source; the group-map enricher fills in Web.
        let c = classify_search_result("[SubsPlease] Frieren - 01 (1080p) [A1B2C3D4].mkv");
        assert_eq!(c.group, "SubsPlease");
        assert_eq!(c.resolution, "1080");
        // Without Layer 3 (group map), source is unknown here.
        assert!(c.source.is_empty() || c.source == "Web");
    }

    #[test]
    fn classify_search_result_bdmv_label_matches_grab_path() {
        // BDMV releases must produce `BD-1080p RAW` — the same label the
        // grab-side ClassificationResult::label() emits, so UI and DB agree.
        let c = classify_search_result("[smol] Monogatari S1 (BDMV 1080p x264 FLAC) [f00ba211].mkv");
        assert_eq!(c.resolution, "1080");
        assert_eq!(c.source, "BluRay");
        assert!(c.is_bdmv);
        assert_eq!(c.quality_label, "BD-1080p RAW");
    }

    #[test]
    fn classify_search_result_remux_gets_suffix() {
        let c = classify_search_result("[Tenrai-Sensei] Frieren - 01 (BD Remux 1080p).mkv");
        assert_eq!(c.source, "BluRay");
        assert!(c.is_remux);
        assert_eq!(c.quality_label, "BD-1080p Remux");
    }

    #[test]
    fn classify_search_result_web_dl_produces_full_label() {
        let c = classify_search_result("Show Name - 01 (1080p) [WEB-DL].mkv");
        assert_eq!(c.resolution, "1080");
        assert_eq!(c.source, "Web");
        // web_kind still tracks WebDl internally so CF value-3 specs
        // match releases with explicit WEB-DL tokens. The user-facing
        // label collapses WebDl and bare-WEB into "WEB" (issue #48).
        assert_eq!(c.web_kind, "WEB-DL");
        assert_eq!(c.quality_label, "WEB-1080p");
    }

    #[test]
    fn classify_search_result_empty_label_when_nothing_parses() {
        // No source, no resolution → empty label so the UI shows a dash
        // instead of a stray "Unknown" string.
        let c = classify_search_result("garbage title with no tokens");
        assert!(c.resolution.is_empty());
        assert!(c.source.is_empty());
        assert!(c.quality_label.is_empty());
    }

    #[tokio::test]
    async fn enrich_with_group_map_fills_source_for_known_group() {
        use crate::models::group_source_map;
        use crate::services::source::Source;

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&pool).await.unwrap();
        // Seeded map already ships SubsPlease=Web; rely on that rather
        // than re-inserting here so the test also exercises the
        // real seed data round-trip.
        assert_eq!(
            group_source_map::get(&pool, "SubsPlease")
                .await
                .unwrap()
                .map(|e| e.source),
            Some(Source::Web),
        );

        let mut results = vec![SearchResult {
            title: "[SubsPlease] Show - 01 (1080p) [abc].mkv".to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders: 0,
            leechers: 0,
            downloads: 0,
            group: "SubsPlease".to_string(),
            resolution: "1080".to_string(),
            quality_label: "1080p".to_string(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: String::new(),
        }];

        enrich_results_with_group_map(&pool, &mut results).await;

        assert_eq!(results[0].source, "Web");
        // Issue #48: the SubsPlease WebDl seed was dropped — the
        // distinction between "WEBDL" and "WEB" labels was more
        // confusing than useful and nothing in the file list said
        // the release was a stream remux vs a re-encode. The group
        // map still pins Source::Web; the label unifies to WEB.
        assert_eq!(results[0].quality_label, "WEB-1080p");
    }

    #[tokio::test]
    async fn enrich_with_group_map_does_not_overwrite_filename_source() {
        // Filename said BluRay explicitly; even if the group map would
        // claim Web, the filename's specificity wins.
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&pool).await.unwrap();

        let mut results = vec![SearchResult {
            title: "[SubsPlease] Show - 01 (BD 1080p) [abc].mkv".to_string(),
            link: String::new(), magnet: String::new(), torrent: String::new(),
            size: String::new(), size_bytes: 0,
            seeders: 0, leechers: 0, downloads: 0,
            group: "SubsPlease".to_string(),
            resolution: "1080".to_string(),
            quality_label: "BD-1080p".to_string(),
            source: "BluRay".to_string(),
            web_kind: String::new(),
            is_remux: false, is_bdmv: false,
            is_batch: false, is_trusted: false,
            score: 0, info_hash: String::new(),
        }];

        enrich_results_with_group_map(&pool, &mut results).await;

        assert_eq!(results[0].source, "BluRay");
        assert_eq!(results[0].quality_label, "BD-1080p");
    }
}
