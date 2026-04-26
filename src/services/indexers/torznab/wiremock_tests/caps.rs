//! `Indexer::caps` wire shape: GET `<base>?t=caps&apikey=...`.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture};
use crate::services::indexers::Indexer;

const CAPS_BODY: &str = r#"<?xml version="1.0"?>
<caps>
  <limits max="100" default="50"/>
  <searching>
    <tv-search available="yes" supportedParams="q,cat"/>
  </searching>
  <categories>
    <category id="5000" name="TV">
      <subcat id="5070" name="Anime"/>
    </category>
  </categories>
</caps>"#;

#[tokio::test]
async fn caps_sends_get_with_t_caps_and_apikey() {
    // Pin the URL shape Prowlarr expects: GET to the base URL's
    // path with `t=caps` and the configured apikey. Other clients
    // using the same Prowlarr instance won't see this request
    // since wiremock isolates per-test.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "caps"))
        .and(query_param("apikey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(CAPS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let caps = client.caps().await.expect("caps must succeed");
    assert_eq!(caps.max_limit, Some(100));
    assert!(
        caps.search_modes
            .iter()
            .any(|m| m.mode == "tv-search" && m.available),
        "tv-search must be in caps"
    );
}

#[tokio::test]
async fn caps_surfaces_torznab_error_body_as_err() {
    // HTTP 200 + <error/> body is the spec'd shape for bad
    // creds. The client must surface this as Err even though
    // the status was 200.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0"?>
<error code="100" description="Invalid API Key"/>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let result = client.caps().await;
    let err = result.expect_err("must be Err");
    assert!(
        err.contains("100") && err.contains("Invalid API Key"),
        "error must surface code + description: {err}"
    );
}
