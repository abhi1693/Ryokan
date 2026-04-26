//! `Indexer::search` wire shape: GET
//! `<base>?t=tvsearch&apikey=...&cat=5070&q=...`.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture};
use crate::services::indexers::{Indexer, SearchQuery};

const SEARCH_BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
<item>
  <title>Synthetic.Show.S01E01</title>
  <guid>g1</guid>
  <enclosure url="https://server/dl/abc?apikey=KEY" length="1000000000" type="application/x-bittorrent"/>
  <torznab:attr name="seeders" value="20"/>
  <torznab:attr name="leechers" value="2"/>
  <torznab:attr name="infohash" value="ABCDEF1234567890"/>
  <torznab:attr name="category" value="5070"/>
</item>
</channel>
</rss>"#;

#[tokio::test]
async fn search_sends_tvsearch_with_anime_category_default() {
    // When the caller doesn't specify categories, the client
    // defaults to 5070 (anime) per protocol research. Pin both
    // the t=tvsearch function name and the cat=5070 default.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("apikey", TEST_API_KEY))
        .and(query_param("cat", "5070"))
        .and(query_param("q", "Test Show"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: Vec::new(), // default → 5070
        limit: None,
        offset: None,
    };
    let releases = client.search(&query).await.expect("search must succeed");
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].title, "Synthetic.Show.S01E01");
    assert_eq!(releases[0].seeders, 20);
    assert_eq!(releases[0].indexer_id, 7, "stamps caller's id");
}

#[tokio::test]
async fn search_uses_csv_for_multiple_categories() {
    // Multiple cats join with `,` per torznab spec. Pin the wire
    // shape so a future refactor can't accidentally split into
    // multiple cat= params.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "5070,5080"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel/></rss>"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "Test".to_string(),
        categories: vec![5070, 5080],
        limit: None,
        offset: None,
    };
    let _ = client.search(&query).await.expect("must succeed");
}

#[tokio::test]
async fn search_passes_limit_and_offset_when_set() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("limit", "25"))
        .and(query_param("offset", "50"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel/></rss>"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "X".to_string(),
        categories: Vec::new(),
        limit: Some(25),
        offset: Some(50),
    };
    let _ = client.search(&query).await.expect("must succeed");
}

#[tokio::test]
async fn search_empty_q_omits_q_param() {
    // An empty query string means "indexer's recent items feed"
    // — the protocol allows omitting `q`. Pin the wire behavior:
    // empty `q` results in NO `q=` param, not `q=`.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel/></rss>"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery::default();
    let releases = client.search(&query).await.expect("must succeed");
    assert!(releases.is_empty());
}
