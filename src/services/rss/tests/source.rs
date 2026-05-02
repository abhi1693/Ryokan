use super::super::RssSource;

#[test]
fn nyaa_source_labels_as_nyaa() {
    assert_eq!(RssSource::Nyaa.label(), "nyaa");
}

#[test]
fn user_feed_label_uses_feed_prefix() {
    let s = RssSource::UserFeed {
        id: 7,
        name: "SubsPlease 1080p".into(),
    };
    assert_eq!(s.label(), "feed:SubsPlease 1080p");
}

#[test]
fn indexer_label_uses_kind_prefix() {
    // Pin so log-grep `^torznab:` / `^newznab:` filters work
    // for filtering RSS decisions by indexer protocol.
    let t = RssSource::Indexer {
        id: 1,
        name: "Animebytes".into(),
        kind: "torznab".into(),
    };
    let n = RssSource::Indexer {
        id: 2,
        name: "NZBgeek".into(),
        kind: "newznab".into(),
    };
    assert_eq!(t.label(), "torznab:Animebytes");
    assert_eq!(n.label(), "newznab:NZBgeek");
}
