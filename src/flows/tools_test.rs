//! What the toolset must refuse, and what it must not silently do.

use super::*;

use crate::ports::corpus::MapCorpus;

fn corpus() -> MapCorpus {
    MapCorpus::default()
        .with("src/a.rs", "fn one() {}\nfn two() {}\n")
        .with("src/b.rs", "fn three() {}\n")
}

fn tools(corpus: &MapCorpus) -> ReadOnlyTools<'_> {
    ReadOnlyTools::new(corpus)
}

async fn call(tools: &ReadOnlyTools<'_>, slug: &str, args: Value) -> FlowResult<Value> {
    tools.invoke(slug, args, None).await
}

#[tokio::test]
async fn read_file_returns_the_content() {
    let out = call(
        &tools(&corpus()),
        "read_file",
        json!({ "path": "src/a.rs" }),
    )
    .await
    .unwrap();

    assert_eq!(out["content"], json!("fn one() {}\nfn two() {}\n"));
}

#[tokio::test]
async fn a_missing_file_is_a_result_not_an_error() {
    // A reviewer guessing at a filename must be able to guess again. An `Err`
    // ends the tool loop and it stops looking.
    let out = call(&tools(&corpus()), "read_file", json!({ "path": "nope.rs" }))
        .await
        .expect("not an error");

    assert!(out["error"].is_string());
}

#[tokio::test]
async fn a_path_escaping_the_repository_is_refused() {
    // The arguments come from a model that has just read untrusted pull request
    // prose. This is the injection that matters.
    for path in [
        "../../../etc/passwd",
        "/etc/passwd",
        "~/.ssh/id_rsa",
        "src/../../secrets",
        "C:/Windows/System32/config",
    ] {
        let err = call(&tools(&corpus()), "read_file", json!({ "path": path }))
            .await
            .expect_err(path);

        assert!(err.to_string().contains("nowhere else"), "{path}: {err}");
    }
}

#[tokio::test]
async fn an_ordinary_relative_path_is_not_caught_by_the_traversal_check() {
    // The check must not be so broad that it rejects the paths the tool exists
    // to read — a refusal that looks like a missing file teaches the reviewer
    // the file does not exist.
    for path in ["src/a.rs", "./src/a.rs", "a/b/c/d.rs", "Cargo.toml"] {
        ReadOnlyTools::safe_path(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    }
}

#[tokio::test]
async fn an_unknown_slug_is_an_error_naming_what_exists() {
    // Not an empty result: a model that asked to run the tests and got `{}`
    // may well conclude they passed.
    let err = call(&tools(&corpus()), "run_tests", json!({}))
        .await
        .expect_err("refused");

    assert!(err.to_string().contains("read_file"), "{err}");
}

#[tokio::test]
async fn every_offered_slug_is_actually_invocable() {
    // `descriptors` is what the model is told it may call and `SLUGS` is the
    // error message; a slug in either that the `match` does not handle is a
    // tool that fails on first use.
    for slug in SLUGS {
        let args = json!({ "path": "src/a.rs", "pattern": "fn" });
        call(&tools(&corpus()), slug, args)
            .await
            .unwrap_or_else(|e| {
                panic!("`{slug}` is offered but not invocable: {e}");
            });
    }

    let offered: Vec<String> = descriptors()
        .as_array()
        .expect("a list")
        .iter()
        .map(|d| d["slug"].as_str().expect("a slug").to_string())
        .collect();

    assert_eq!(offered, SLUGS, "the descriptors and the slug list disagree");
}

#[tokio::test]
async fn a_missing_argument_is_an_error_rather_than_a_default() {
    // An empty pattern defaulted to "" would search for nothing and report no
    // matches, which a reviewer reads as "this appears nowhere".
    assert!(call(&tools(&corpus()), "search", json!({})).await.is_err());
    assert!(
        call(&tools(&corpus()), "search", json!({ "pattern": "  " }))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_truncated_read_says_that_it_was_truncated() {
    let big = "x".repeat(MAX_READ_BYTES * 2);
    let corpus = MapCorpus::default().with("big.txt", &big);
    let tools = ReadOnlyTools::new(&corpus);

    let out = call(&tools, "read_file", json!({ "path": "big.txt" }))
        .await
        .unwrap();
    let content = out["content"].as_str().expect("content");

    assert!(content.contains("[truncated:"), "silently cut");
    assert!(content.len() <= MAX_READ_BYTES + 100, "cap not applied");
}

#[tokio::test]
async fn truncation_does_not_split_a_utf8_character() {
    // A byte slice through a multi-byte character panics on `&text[..end]`,
    // which would take the whole review down rather than shorten one read.
    let big = "é".repeat(MAX_READ_BYTES);
    let corpus = MapCorpus::default().with("u.txt", &big);
    let tools = ReadOnlyTools::new(&corpus);

    let out = call(&tools, "read_file", json!({ "path": "u.txt" }))
        .await
        .unwrap();

    assert!(out["content"].as_str().expect("content").contains("é"));
}

#[tokio::test]
async fn the_total_budget_is_shared_across_calls_not_per_call() {
    // Per-call only, a loop reading twenty files pastes twenty files into every
    // later turn. This is the limit that actually bounds the conversation.
    let big = "x".repeat(MAX_READ_BYTES);
    let mut corpus = MapCorpus::default();
    for i in 0..10 {
        corpus = corpus.with(&format!("f{i}.txt"), &big);
    }
    let tools = ReadOnlyTools::new(&corpus);

    let mut exhausted = false;
    for i in 0..10 {
        let out = call(&tools, "read_file", json!({ "path": format!("f{i}.txt") }))
            .await
            .unwrap();

        if out["error"].as_str().is_some_and(|e| e.contains("budget")) {
            exhausted = true;
            break;
        }
    }

    assert!(exhausted, "ten full reads did not exhaust the total budget");
    assert!(tools.spent() >= MAX_TOTAL_BYTES);
}

#[tokio::test]
async fn being_unable_to_search_is_not_reported_as_no_matches() {
    // The two support opposite conclusions and must not read alike.
    struct NoSearch;

    #[async_trait]
    impl Corpus for NoSearch {
        async fn read(&self, _path: &str) -> crate::error::Result<Option<String>> {
            Ok(None)
        }
        async fn search(
            &self,
            _pattern: &str,
            _limit: usize,
        ) -> crate::error::Result<Option<Vec<crate::ports::corpus::Hit>>> {
            Ok(None)
        }
    }

    let corpus = NoSearch;
    let tools = ReadOnlyTools::new(&corpus);
    let out = call(&tools, "search", json!({ "pattern": "fn" }))
        .await
        .unwrap();

    assert!(out["hits"].is_null());
    assert!(
        out["error"]
            .as_str()
            .expect("an error")
            .contains("do not conclude"),
    );
}
