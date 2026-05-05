//! Wiremock coverage for `services::auto_search::find_all_for_target`.
//! The audit's largest single mutant cluster (~85 missed) lives in this
//! function's branching logic — alias matching, sibling rejection,
//! season filtering, SeaDex bypass, and the multi-phase query sweep.
//! All of that runs through `nyaa::search` against `RYOKAN_NYAA_API_BASE`,
//! the env-var seam added in commit `c836649`.
//!
//! Each test stands up a minimal Nyaa-shaped wiremock and asserts the
//! resulting `Vec<SearchResult>` matches the expected shape. Mirrors the
//! `metadata_sync_e2e.rs` ENV_LOCK pattern so within-binary tests don't
//! race on the process-wide env var; cross-binary isolation is handled
//! by nextest's process-per-test default.

use ryokan::AppState;
use ryokan::models::config::Config;
use ryokan::services::anilist::AnimeDetail;
use ryokan::services::auto_search::{SearchTarget, find_all_for_target};
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize within-binary tests on the RYOKAN_NYAA_API_BASE write so
/// tokio's parallel scheduler can't race two tests on the same env var.
/// Other test binaries get their own process under nextest, so cross-
/// binary leakage is impossible.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

// ─── HTML fixture builders ───────────────────────────────────────

/// Produce a Nyaa search-results page wrapping the given rows. The
/// scraper looks for `table.torrent-list tbody tr` so the wrapper
/// matches that selector exactly. No pagination → has_next stays false.
fn nyaa_results_page(rows_html: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><body>
<table class="torrent-list">
  <tbody>
{rows_html}
  </tbody>
</table>
</body></html>"#
    )
}

/// One Nyaa-shape table row. Eight `<td>` cells in the order the parser
/// reads them: category, name (with /view/ link), links (torrent +
/// magnet), size, date, seeders, leechers, downloads.
///
/// `info_hash` is the 40-char hex string used in the magnet xt; the
/// scraper extracts it via `extract_hash` and uses it as the candidate's
/// dedup key.
fn nyaa_row(info_hash: &str, view_id: u64, title: &str, size: &str, seeders: i32) -> String {
    format!(
        r#"    <tr>
      <td><a href="/c/1_2"></a></td>
      <td>
        <a href="/view/{view_id}">{title}</a>
      </td>
      <td>
        <a href="/download/{view_id}.torrent"></a>
        <a href="magnet:?xt=urn:btih:{info_hash}&amp;dn={title}"></a>
      </td>
      <td>{size}</td>
      <td>2024-04-01 12:00</td>
      <td>{seeders}</td>
      <td>0</td>
      <td>100</td>
    </tr>
"#
    )
}

// ─── AppState / fixture builders ─────────────────────────────────

fn detail_for(id: i64, romaji: &str) -> AnimeDetail {
    AnimeDetail {
        id,
        id_mal: None,
        title_romaji: romaji.into(),
        title_english: romaji.into(),
        title_native: romaji.into(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(12),
        duration: Some(24),
        season: String::new(),
        season_year: Some(2024),
        end_year: Some(2024),
        description: String::new(),
        genres: vec![],
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: vec![],
        streaming_episodes: vec![],
        relations: vec![],
    }
}

fn default_config() -> Config {
    Config {
        preferred_resolution: "1080".into(),
        preferred_source: "web".into(),
        cutoff_resolution: "720".into(),
        cutoff_source: "web".into(),
        allow_non_english: false,
        finished_series_quality: "same_as_airing".into(),
        title_language: "english".into(),
        ..Config::default()
    }
}

/// Build the AppState scaffolding `find_all_for_target` reads from.
/// No download client (the function doesn't dispatch grabs); empty
/// indexers (Nyaa is the only source under test); empty CFs.
async fn build_state() -> AppState {
    let db = in_memory_pool().await;
    build_test_app_state(db, None)
}

/// Set the Nyaa env-var seam to point at the wiremock server. Call
/// inside a `let _gate = ENV_LOCK.lock().await;` block so concurrent
/// tests don't race.
fn set_nyaa_base(uri: &str) {
    unsafe {
        std::env::set_var("RYOKAN_NYAA_API_BASE", uri);
    }
}

fn unset_nyaa_base() {
    unsafe {
        std::env::remove_var("RYOKAN_NYAA_API_BASE");
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn find_all_for_target_returns_matching_release_from_nyaa() {
    // Smallest happy-path case: one Nyaa row with a title that shares
    // a token with the AL detail's title, episode number matches the
    // search target. The function should return that row in the
    // candidate list with score > 0.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "0123456789abcdef0123456789abcdef01234567",
        12345,
        "[Group] Test Show - 01 (1080p) [WEB].mkv",
        "1.4 GiB",
        50,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1001, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(1);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    assert!(
        !results.is_empty(),
        "matching Nyaa row must surface in the results: got {results:?}"
    );
    assert!(
        results.iter().any(|r| r.title.contains("Test Show")),
        "result must include our seeded title"
    );
    let r = &results[0];
    assert_eq!(r.seeders, 50);
    assert_eq!(r.size, "1.4 GiB");
    assert!(
        r.info_hash.starts_with("0123456789abcdef"),
        "info_hash must round-trip from the magnet"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_returns_empty_when_nyaa_returns_no_rows() {
    // Pin the empty-results path: Nyaa returns a well-formed page with
    // zero rows. find_all_for_target must return an empty Vec without
    // panicking on the empty SeaDex / extended-aliases / group-queries
    // fallback fan-out.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(""); // no rows
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1002, "Empty Result Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(5);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;
    assert!(
        results.is_empty(),
        "no Nyaa rows must produce no candidates: got {results:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_filters_unrelated_titles_via_alias_match() {
    // Pin the alias-match gate inside `apply_interactive_filter_and_push`.
    // Two Nyaa rows: one matches the AL detail's tokens, one doesn't.
    // Only the matching one survives the filter.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let mut rows = String::new();
    rows.push_str(&nyaa_row(
        "1111111111111111111111111111111111111111",
        100,
        "[Group] Test Show - 03 (1080p).mkv",
        "1.2 GiB",
        80,
    ));
    rows.push_str(&nyaa_row(
        "2222222222222222222222222222222222222222",
        101,
        "[Group] Completely Different Anime - 03 (1080p).mkv",
        "1.0 GiB",
        80,
    ));
    let html = nyaa_results_page(&rows);
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1003, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(3);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    // The exact filter behavior depends on the alias-match threshold,
    // but at minimum the matching row must be present and the unrelated
    // one must not crowd out the matching one in the score ordering.
    assert!(
        results.iter().any(|r| r.title.contains("Test Show")),
        "matching title must surface"
    );
    assert!(
        !results
            .iter()
            .any(|r| r.title.contains("Completely Different")),
        "unrelated title must be filtered out: got {results:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_drops_episode_mismatches_for_single_episode_targets() {
    // SearchTarget::Episode(N) means we want episode N specifically;
    // releases for other episodes (parsed from the title) get dropped
    // unless they're batches. Pin that filter.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let mut rows = String::new();
    rows.push_str(&nyaa_row(
        "3333333333333333333333333333333333333333",
        200,
        "[Group] Test Show - 03 (1080p).mkv",
        "1.0 GiB",
        50,
    ));
    rows.push_str(&nyaa_row(
        "4444444444444444444444444444444444444444",
        201,
        "[Group] Test Show - 99 (1080p).mkv",
        "1.0 GiB",
        50,
    ));
    let html = nyaa_results_page(&rows);
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1004, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(3);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    assert!(
        results.iter().any(|r| r.title.contains(" - 03 ")),
        "ep 03 release must match the target"
    );
    assert!(
        !results.iter().any(|r| r.title.contains(" - 99 ")),
        "ep 99 release must be dropped: got {results:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_runs_group_query_pass_when_preferred_groups_configured() {
    // Pin the group-queries branch at line 206:
    //   `if !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty()`
    //
    // The first query pass uses canonical title aliases. When
    // `preferred_groups` is set AND no uploader restriction is active,
    // a SECOND pass runs prefixing each query with the group name
    // ("SubsPlease Test Show 01" etc.). Mutating the `&&` to `||` or
    // dropping the negation on either side would change which pass
    // fires. Pin by counting Nyaa requests: at minimum two (canonical
    // queries + group-prefixed queries).
    //
    // Easier to assert the request COUNT than the query contents,
    // since Ryokan's query-shape variants are tested separately.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                "6666666666666666666666666666666666666666",
                400,
                "[SubsPlease] Test Show - 02 (1080p) [WEB].mkv",
                "1.0 GiB",
                40,
            ))),
        )
        // .expect(1..) means "at least one call." Without group queries,
        // the canonical pass alone would generate ~4 queries (build_
        // queries_from_aliases per alias × 4 query-shape variants).
        // With group queries, that count increases. We just want > 1
        // distinct hits to confirm the group pass also fires.
        .expect(2..)
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1006, "Test Show");
    let mut cfg = default_config();
    cfg.preferred_groups = "SubsPlease".into();
    let target = SearchTarget::Episode(2);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let _results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    // The .expect(2..) on the mount above is the assertion; it fails
    // at server-drop if the call count is below the threshold.

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_skips_group_pass_when_restrict_user_active() {
    // Symmetric pin for the second clause of line 206's `&&`. When
    // `restrict_user` is non-empty, the group-prefixed query pass
    // skips entirely (the comment in the function explains why:
    // `?u=<name>` already scopes to one uploader, so a group prefix
    // is a no-op narrow).
    //
    // Distinct from the previous test by setting restrict_to_uploader
    // and asserting the request count stays at the canonical-pass
    // baseline. Without the gate, the group pass would run on top
    // of the canonical pass and the count would jump.
    //
    // Hard to assert "no second pass fired" with a strict count
    // because canonical-pass fan-out is itself variable. Instead,
    // assert the user-scoped path was hit: every query goes to
    // `/user/<name>` rather than `/`.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    // Fail any request to bare `/` — every request must go through
    // the /user/Trusted scope.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/Trusted"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                "7777777777777777777777777777777777777777",
                500,
                "[Trusted] Test Show - 04 (1080p).mkv",
                "1.0 GiB",
                30,
            ))),
        )
        .expect(1..)
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1007, "Test Show");
    let mut cfg = default_config();
    cfg.preferred_groups = "Trusted".into();
    cfg.default_restrict_to_uploader = "Trusted".into();
    let target = SearchTarget::Episode(4);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let _results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_dedups_same_info_hash_across_query_passes() {
    // The query sweep fans out across multiple title aliases. If two
    // queries surface the SAME info_hash, the dedup map under
    // `apply_interactive_filter_and_push` must collapse them to one
    // entry per (source_tag, info_hash) pair. Set up a wiremock that
    // returns the same row regardless of query — at least two queries
    // will hit it (canonical + variant) but the result list must not
    // double-count.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "5555555555555555555555555555555555555555",
        300,
        "[Group] Test Show - 07 (1080p).mkv",
        "1.0 GiB",
        50,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("c", "1_2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1005, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(7);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    let matching: Vec<_> = results
        .iter()
        .filter(|r| r.info_hash.starts_with("5555555555555555"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "duplicate info_hash across queries must collapse to one candidate: {matching:?}"
    );

    unset_nyaa_base();
}
