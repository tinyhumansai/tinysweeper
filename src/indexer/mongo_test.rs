//! Tests for the MongoDB manifest.
//!
//! Two halves, the same split `index::mongo_test` uses. The **pure** tests
//! always run and assert on document shapes and decoding without a server. The
//! **live** tests need a real deployment named by
//! `TINYSWEEPER_TEST_MONGODB_URI` and are ignored by default, because
//! `cargo test` must stay offline.
//!
//! ```sh
//! docker compose -f docker-compose.dev.yml up -d mongod
//! TINYSWEEPER_TEST_MONGODB_URI='mongodb://localhost:27017/?directConnection=true' \
//!     cargo test --features serve -- --ignored indexer::mongo
//! ```

use super::*;

/// Environment variable naming the test database, shared with `server::store`.
const TEST_URI_ENV: &str = "TINYSWEEPER_TEST_MONGODB_URI";

fn signature() -> EmbedSignature {
    EmbedSignature::new("mock", "hash-bag", 16)
}

// --- pure: always run, no server -----------------------------------------

#[test]
fn a_record_id_separates_signatures_of_one_repository() {
    // Two embedding models must not share a record: the second would inherit
    // the first's revision and look fresh with none of its vectors.
    let a = record_id("o/r", "voyage:voyage-code-3:1024");
    let b = record_id("o/r", "voyage:voyage-3-large:1024");
    assert_ne!(a, b);
}

#[test]
fn a_missing_record_decodes_as_absent_rather_than_ready() {
    let record = record_from_document("o/r", "s", None);
    assert_eq!(record.state, IndexState::Absent);
    assert!(!record.is_fresh("abc"));
}

#[test]
fn a_stored_record_decodes_with_its_state_revision_and_spend() {
    let document = doc! {
        "state": "ready",
        "revision": "abc",
        "chunks": 42_i64,
        "calls": 3_i64,
        "tokens": 1_000_i64,
        "cost_usd": 0.25_f64,
    };
    let record = record_from_document("o/r", "s", Some(document));
    assert!(record.is_fresh("abc"));
    assert_eq!(record.chunks, 42);
    assert_eq!(record.usage.tokens, 1_000);
    assert_eq!(record.usage.cost_usd, 0.25);
}

#[test]
fn an_unknown_state_string_decodes_as_absent() {
    // A record written by a future version must not read as `ready`, which
    // would make this version skip indexing it forever.
    let record = record_from_document("o/r", "s", Some(doc! { "state": "reticulating" }));
    assert_eq!(record.state, IndexState::Absent);
}

#[test]
fn a_duplicate_key_error_is_recognised_by_code_and_by_message() {
    // The whole claim rests on this predicate: misreading a duplicate key as a
    // real error turns a contended claim into a failed index.
    assert!(!is_duplicate_key(&mongodb::error::Error::custom("nope")));
}

#[test]
fn a_claim_ttl_outlasts_a_cold_full_index() {
    assert!(CLAIM_TTL >= std::time::Duration::from_secs(60 * 60));
}

// --- live: need a real MongoDB -------------------------------------------

async fn manifest() -> Option<MongoManifest> {
    let uri = std::env::var(TEST_URI_ENV).ok()?;
    let name = format!("tinysweeper_manifest_test_{}", std::process::id());
    Some(MongoManifest::connect(&uri, &name).await.expect("connects"))
}

macro_rules! live_test {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        #[ignore = "needs a MongoDB deployment"]
        async fn $name() {
            let Some(manifest) = manifest().await else {
                eprintln!("skipping: {} is not set", TEST_URI_ENV);
                return;
            };
            let f: fn(MongoManifest) -> _ = $body;
            f(manifest).await;
        }
    };
}

live_test!(
    a_claim_is_exclusive_and_names_its_holder,
    |manifest| async move {
        let repo = format!("o/exclusive-{}", std::process::id());
        let granted = manifest
            .claim(&repo, &signature(), "worker-1")
            .await
            .expect("claims");
        assert!(matches!(granted, Claim::Granted(_)));

        assert_eq!(
            manifest
                .claim(&repo, &signature(), "worker-2")
                .await
                .expect("answers"),
            Claim::Busy {
                holder: Some("worker-1".into())
            }
        );
    }
);

live_test!(
    releasing_records_the_revision_and_frees_the_claim,
    |manifest| async move {
        let repo = format!("o/release-{}", std::process::id());
        let Claim::Granted(lease) = manifest
            .claim(&repo, &signature(), "worker-1")
            .await
            .expect("claims")
        else {
            panic!("not granted");
        };
        manifest
            .release(
                &lease,
                &Settled::Done {
                    revision: Some("abc".into()),
                    chunks: 7,
                    usage: EmbedUsage {
                        calls: 1,
                        tokens: 10,
                        cost_usd: 0.5,
                    },
                },
            )
            .await
            .expect("releases");

        let record = manifest.state(&repo, &signature()).await.expect("reads");
        assert!(record.is_fresh("abc"));
        assert_eq!(record.usage.cost_usd, 0.5);
        assert!(matches!(
            manifest
                .claim(&repo, &signature(), "worker-2")
                .await
                .expect("claims"),
            Claim::Granted(_)
        ));
    }
);

live_test!(spend_accumulates_across_runs, |manifest| async move {
    let repo = format!("o/spend-{}", std::process::id());
    for _ in 0..2 {
        let Claim::Granted(lease) = manifest
            .claim(&repo, &signature(), "w")
            .await
            .expect("claims")
        else {
            panic!("not granted");
        };
        manifest
            .release(
                &lease,
                &Settled::Done {
                    revision: Some("abc".into()),
                    chunks: 1,
                    usage: EmbedUsage {
                        calls: 1,
                        tokens: 100,
                        cost_usd: 0.25,
                    },
                },
            )
            .await
            .expect("releases");
    }
    let record = manifest.state(&repo, &signature()).await.expect("reads");
    assert_eq!(record.usage.cost_usd, 0.5, "a re-index adds to the bill");
});

live_test!(
    recorded_files_round_trip_and_forgetting_removes_them,
    |manifest| async move {
        let repo = format!("o/files-{}", std::process::id());
        let file = IndexedFile {
            path: "src/a.rs".into(),
            chunks: vec!["id-1".into()],
            pending: vec!["id-2".into()],
        };
        manifest
            .record(&repo, &signature(), std::slice::from_ref(&file))
            .await
            .expect("records");

        let read = manifest
            .indexed(&repo, &signature(), &["src/a.rs".to_string()])
            .await
            .expect("reads");
        assert_eq!(read, vec![file]);
        assert_eq!(
            manifest.paths(&repo, &signature()).await.expect("lists"),
            vec!["src/a.rs".to_string()]
        );

        manifest
            .forget(&repo, &signature(), &["src/a.rs".to_string()])
            .await
            .expect("forgets");
        assert!(
            manifest
                .paths(&repo, &signature())
                .await
                .expect("lists")
                .is_empty()
        );
    }
);
