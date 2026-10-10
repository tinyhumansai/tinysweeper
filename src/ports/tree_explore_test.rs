//! Additional read-only repository query behavior.

use super::*;

#[tokio::test]
async fn listing_and_symbol_queries_are_bounded_and_exclude_sensitive_paths() {
    let tree = MockTree::from_files([
        ("src/a.rs", "pub fn cursor() {}"),
        ("src/b.rs", "other"),
        (".env", "PASSWORD=secret"),
    ]);
    let Found::Hits {
        hits, truncated, ..
    } = tree
        .explore(&TreeQuery::List {
            path: ".".into(),
            limit: 1,
        })
        .await
        .unwrap()
    else {
        panic!("entries")
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/a.rs");
    assert!(truncated);
    let Found::Hits { hits, .. } = tree
        .explore(&TreeQuery::Symbol {
            symbol: "cursor".into(),
            limit: 1,
        })
        .await
        .unwrap()
    else {
        panic!("symbols")
    };
    assert_eq!(hits[0].path, "src/a.rs");
    assert!(matches!(
        tree.explore(&TreeQuery::List {
            path: "../".into(),
            limit: 1
        })
        .await
        .unwrap(),
        Found::Unavailable { .. }
    ));
}

#[tokio::test]
async fn directory_exploration_and_recordings_scrub_and_refuse_renamed_sensitive_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "pub fn cursor() {}\n").unwrap();
    std::fs::write(dir.path().join(".env"), "cursor SECRET=hidden").unwrap();
    let tree = DirTree::new(dir.path());
    let redacted = RedactingTree::refusing_paths(&tree, vec!["a.rs".into()]);
    let listed = redacted
        .explore(&TreeQuery::List {
            path: ".".into(),
            limit: 10,
        })
        .await
        .unwrap();
    assert!(matches!(listed, Found::Hits { hits, .. } if hits.is_empty()));
    let symbols = tree
        .explore(&TreeQuery::Symbol {
            symbol: "cursor".into(),
            limit: 10,
        })
        .await
        .unwrap();
    assert!(
        matches!(symbols, Found::Hits { hits, .. } if hits.len() == 1 && hits[0].path == "a.rs")
    );
    let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
    let mock =
        MockTree::from_files([("a.rs", format!("fn cursor() {{ let key = \"{secret}\"; }}"))]);
    let recording = RecordingTree::new(&mock);
    let query = TreeQuery::Symbol {
        symbol: "cursor".into(),
        limit: 10,
    };
    let found = recording.explore(&query).await.unwrap();
    assert!(!format!("{found:?}").contains(&secret));
    assert!(!format!("{:?}", recording.recorded()).contains(&secret));
}
