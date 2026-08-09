//! Loading a corpus, and refusing to load a dishonest one.

use std::path::Path;

use super::*;

/// A minimal valid case, with `{extra}` spliced in for the test to break.
fn case_toml(id: &str, extra: &str) -> String {
    format!(
        r#"
schema = 1
id = "{id}"
fixture = "../fixtures/{id}.json"

[provenance]
repo = "tinyhumansai/tinysweeper"
pr = 1
evidence = "https://github.com/tinyhumansai/tinysweeper/pull/2"
labelled_by = "tester"
{extra}
"#
    )
}

/// A fixture with just enough to be loadable.
///
/// Serialized from the type rather than hand-written, because that is how a
/// real fixture is produced — `eval add` freezes a live pull request through
/// the same `Serialize`. A hand-written literal would drift from the struct and
/// fail on a field nobody meant to add.
fn fixture_json() -> String {
    let fixture = crate::eval::types::Fixture {
        pull_request: crate::forge::types::PullRequest {
            number: 1,
            head_sha: "a".repeat(40),
            ..Default::default()
        },
        ..Default::default()
    };
    serde_json::to_string(&fixture).expect("serializes")
}

/// Write a corpus into a temp dir and return its root.
fn corpus_with(cases: &[(&str, String)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("cases")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("fixtures")).expect("mkdir");
    for (id, toml) in cases {
        std::fs::write(dir.path().join("cases").join(format!("{id}.toml")), toml).expect("write");
        std::fs::write(
            dir.path().join("fixtures").join(format!("{id}.json")),
            fixture_json(),
        )
        .expect("write");
    }
    dir
}

fn load_at(dir: &Path) -> Result<Corpus> {
    load(dir)
}

#[test]
fn a_valid_corpus_loads_in_id_order() {
    let dir = corpus_with(&[
        ("ts-0002", case_toml("ts-0002", "")),
        ("ts-0001", case_toml("ts-0001", "")),
    ]);
    let corpus = load_at(dir.path()).expect("loads");

    let ids: Vec<&str> = corpus.cases.iter().map(|c| c.case.id.as_str()).collect();
    // Sorted, so two runs enumerate identically and two reports diff cleanly.
    assert_eq!(ids, ["ts-0001", "ts-0002"]);
    assert_eq!(corpus.digest.len(), 16);
}

#[test]
fn a_case_with_no_external_evidence_is_refused() {
    // The rule that keeps the corpus honest. An expectation justified by the
    // bot's own output measures whether the bot still agrees with itself.
    let toml = case_toml("ts-0001", "").replace(
        r#"evidence = "https://github.com/tinyhumansai/tinysweeper/pull/2""#,
        r#"evidence = """#,
    );
    let dir = corpus_with(&[("ts-0001", toml)]);

    let err = load_at(dir.path()).expect_err("must not load");
    let message = err.to_string();
    assert!(message.contains("provenance.evidence"), "{message}");
    assert!(message.contains("blind spots"), "{message}");
}

#[test]
fn every_problem_is_reported_at_once() {
    // Fixing a corpus one error per run is how people stop fixing it.
    let toml = case_toml("ts-0001", "")
        .replace(
            r#"evidence = "https://github.com/tinyhumansai/tinysweeper/pull/2""#,
            r#"evidence = """#,
        )
        .replace(r#"labelled_by = "tester""#, r#"labelled_by = """#);
    let dir = corpus_with(&[("ts-0001", toml)]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("2 problem(s)"), "{message}");
    assert!(message.contains("provenance.evidence"), "{message}");
    assert!(message.contains("labelled_by"), "{message}");
}

#[test]
fn a_backwards_line_range_is_refused() {
    let extra = r#"
[[expected]]
id = "E1"
path = "src/a.rs"
lines = [30, 10]
summary = "a defect"
"#;
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", extra))]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("backwards"), "{message}");
}

#[test]
fn an_empty_must_mention_slot_is_refused() {
    // An empty slot is satisfied by every finding, so the expectation silently
    // stops checking anything at all.
    let extra = r#"
[[expected]]
id = "E1"
path = "src/a.rs"
summary = "a defect"
must_mention = ["fingerprint", ""]
"#;
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", extra))]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("empty must_mention"), "{message}");
}

#[test]
fn a_forbidden_entry_that_narrows_nothing_is_refused() {
    // `*` with no lane, no lines and no keywords rules out every finding on
    // the case, which is never what anybody meant.
    let extra = r#"
[[forbidden]]
id = "F1"
path = "*"
reason = "not a defect"
matches = []
"#;
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", extra))]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("narrows nothing"), "{message}");
}

#[test]
fn a_forbidden_entry_scoped_by_lane_alone_is_allowed() {
    // Some defects are structural: "the description lane must not anchor to
    // implementation code" is about which lane landed where, and no keyword
    // expresses it.
    let extra = r#"
[[forbidden]]
id = "F1"
path = "*"
lanes = ["description"]
reason = "a PR-scoped finding has no code location"
"#;
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", extra))]);
    assert!(load_at(dir.path()).is_ok());
}

#[test]
fn a_forbidden_entry_with_no_reason_is_refused() {
    // The next person to read an unexplained exclusion will assume it is a
    // mistake and delete it.
    let extra = r#"
[[forbidden]]
id = "F1"
path = "*"
reason = ""
matches = ["dead code"]
"#;
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", extra))]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("no reason"), "{message}");
}

#[test]
fn a_case_from_a_newer_schema_is_refused_rather_than_rescored() {
    let toml = case_toml("ts-0001", "").replace("schema = 1", "schema = 2");
    let dir = corpus_with(&[("ts-0001", toml)]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("schema 2"), "{message}");
}

#[test]
fn an_id_that_would_escape_the_cassette_directory_is_refused() {
    let toml = case_toml("evil", "").replace(r#"id = "evil""#, r#"id = "../../etc/passwd""#);
    let dir = corpus_with(&[("evil", toml)]);

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("path separator"), "{message}");
}

#[test]
fn an_unknown_key_is_refused_rather_than_ignored() {
    // `deny_unknown_fields`, so a typo in a label is an error and not a
    // silently inert expectation.
    let dir = corpus_with(&[(
        "ts-0001",
        case_toml("ts-0001", "").replace("schema = 1", "schema = 1\nmust_menshun = [\"typo\"]"),
    )]);
    assert!(load_at(dir.path()).is_err());
}

#[test]
fn a_missing_fixture_names_the_case_that_wanted_it() {
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", ""))]);
    std::fs::remove_file(dir.path().join("fixtures/ts-0001.json")).expect("remove");

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("ts-0001"), "{message}");
}

#[test]
fn a_fixture_with_no_head_sha_is_refused() {
    // Every finding's identity is keyed on the head sha, so an empty one makes
    // dedupe collapse unrelated findings together.
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", ""))]);
    let headless = crate::eval::types::Fixture::default();
    std::fs::write(
        dir.path().join("fixtures/ts-0001.json"),
        serde_json::to_string(&headless).expect("serializes"),
    )
    .expect("write");

    let message = load_at(dir.path()).expect_err("must not load").to_string();
    assert!(message.contains("head sha"), "{message}");
}

#[test]
fn the_digest_moves_when_a_label_moves_and_not_otherwise() {
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", ""))]);
    let before = load_at(dir.path()).expect("loads").digest;
    assert_eq!(before, load_at(dir.path()).expect("loads").digest);

    let extra = r#"
[[expected]]
id = "E1"
path = "src/a.rs"
summary = "a defect"
"#;
    std::fs::write(
        dir.path().join("cases/ts-0001.toml"),
        case_toml("ts-0001", extra),
    )
    .expect("write");

    // Two baselines scored against different labels must not silently compare.
    assert_ne!(before, load_at(dir.path()).expect("loads").digest);
}

#[test]
fn selecting_an_unknown_case_lists_what_the_corpus_holds() {
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", ""))]);
    let corpus = load_at(dir.path()).expect("loads");

    let message = corpus
        .select(&["ts-9999".to_string()])
        .expect_err("no such case")
        .to_string();
    assert!(message.contains("ts-9999"), "{message}");
    assert!(message.contains("ts-0001"), "{message}");
}

#[test]
fn the_forge_a_case_builds_cannot_be_written_to() {
    let dir = corpus_with(&[("ts-0001", case_toml("ts-0001", ""))]);
    let corpus = load_at(dir.path()).expect("loads");
    let forge = corpus.cases[0].forge();

    // Belt and braces over the type-level guarantee: a lane is handed a
    // `ForgeRead`, but the runner builds this itself and a corpus run must
    // never be what discovers a write path exists.
    assert!(forge.wrote_nothing());
}
