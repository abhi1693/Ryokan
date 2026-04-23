//! Shared rtorrent wiremock fixture. Registers per-method XML
//! responses for `POST /RPC2` matching on
//! `body_string_contains("<methodName>...")`.
//!
//! Response bodies are the small subset of XML-RPC shapes the impl
//! actually decodes — `<string>`, `<i4>/<i8>`, `<array>` — and
//! each helper hands back a complete `<?xml?>…` document so the
//! inline tests stay readable.

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::services::download_client::rtorrent::RtorrentClient;

/// Wrap a `<value>…</value>` inside an XML-RPC `<methodResponse>`.
pub(super) fn method_response(inner_value_xml: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\
         <methodResponse>\
         <params><param>{inner_value_xml}</param></params>\
         </methodResponse>"
    )
}

/// `<methodResponse>` with a single `<i4>` — rtorrent uses i4 for
/// most return values (`load.start_verbose` returns 0, `d.pause`
/// returns 0, `d.erase` returns 0, etc.).
pub(super) fn int_response(n: i64) -> String {
    method_response(&format!("<value><i4>{n}</i4></value>"))
}

/// `<methodResponse>` carrying an `<array>` of arrays — the shape
/// `d.multicall2` / `f.multicall` return. `rows` is a vector of
/// per-row value XML (each row should already be
/// `<array><data>…</data></array>` text).
pub(super) fn array_response(rows: &[String]) -> String {
    let data: String = rows.iter().map(|r| format!("<value>{r}</value>")).collect();
    method_response(&format!(
        "<value><array><data>{data}</data></array></value>"
    ))
}

/// Register a mock that matches any `POST /RPC2` whose body
/// contains `<methodName>{name}</methodName>` and returns `body`.
/// Wiremock's first-match-wins means tests can install a specific
/// response before the fixture's catch-all.
pub(super) async fn install_xmlrpc(server: &MockServer, method_name: &str, body: String) {
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .and(body_string_contains(format!(
            "<methodName>{method_name}</methodName>"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

/// Spin up a mock server and return a client bound to it. No
/// handshake is necessary — rtorrent has no session/auth step
/// beyond per-request HTTP Basic (which the impl passes through
/// reqwest's basic_auth).
pub(super) async fn new_fixture() -> (MockServer, RtorrentClient) {
    let server = MockServer::start().await;
    let client = RtorrentClient::new(&server.uri(), "", "", "ryokan-test");
    (server, client)
}
