//! Tests for the composed retrieval query.
//!
//! The one that matters is
//! [`the_query_stays_in_budget_for_a_huge_diff_and_still_names_its_last_file`]:
//! it is the regression this module replaced an 8000-character raw-diff slice
//! for.

use super::*;
use crate::evidence::diff::parse_file_patch;

/// A diff that touches `count` files, each with its own vocabulary.
fn wide_diff(count: usize) -> Vec<FileDiff> {
    (0..count)
        .map(|index| {
            let patch = format!(
                "@@ -1,3 +1,4 @@ fn handler_{index}(request: Request) -> Response\n \
                 context_line\n+    let outcome_{index} = compute_{index}(request);\n \
                 trailing\n"
            );
            parse_file_patch(&format!("src/module_{index:03}/handler.rs"), &patch)
        })
        .collect()
}

#[test]
fn the_query_stays_in_budget_for_a_huge_diff_and_still_names_its_last_file() {
    let diffs = wide_diff(300);
    let query = build_retrieval_query("Rework every handler", &diffs, 4000);

    assert!(
        query.len() <= 4000,
        "the query must be bounded, got {} bytes",
        query.len()
    );
    // Late files are the whole point. A front-truncated slice of the diff would
    // stop somewhere in the first few dozen and never mention these.
    assert!(
        query.contains("src/module_299/handler.rs"),
        "the last changed path must survive sampling: {query}"
    );
    assert!(
        query.contains("compute_299") || query.contains("outcome_299"),
        "the last file's vocabulary must reach the query: {query}"
    );
}

#[test]
fn a_path_heavy_diff_cannot_starve_the_identifiers() {
    // The failure the per-section caps exist for: hundreds of long paths would
    // otherwise fill the query with directory names and leave no room for what
    // the change actually says.
    let diffs: Vec<FileDiff> = (0..200)
        .map(|index| {
            parse_file_patch(
                &format!("services/very/deeply/nested/package/number_{index:04}/src/main.rs"),
                "@@ -1,2 +1,3 @@\n a\n+    reticulate_splines(harness_budget);\n b\n",
            )
        })
        .collect();
    let query = build_retrieval_query("Move packages", &diffs, 2000);

    assert!(query.len() <= 2000);
    assert!(query.contains("reticulate_splines"), "{query}");
    assert!(query.contains("harness_budget"), "{query}");
}

#[test]
fn the_enclosing_signature_git_named_reaches_the_query() {
    let diffs = vec![parse_file_patch(
        "src/lib.rs",
        "@@ -10,3 +10,4 @@ pub fn settle_invoice(order: &Order) -> Result<()>\n a\n+    b();\n c\n",
    )];
    let query = build_retrieval_query("Fix settlement", &diffs, 4000);

    assert!(query.contains("settle_invoice"), "{query}");
}

#[test]
fn identifiers_are_ranked_by_frequency_and_stripped_of_stopwords() {
    let patch = "@@ -1,1 +1,6 @@\n a\n+    let ledger = ledger_entry(ledger);\n\
                 +    return true;\n+    for x in y {}\n";
    let diffs = vec![parse_file_patch("src/a.rs", patch)];
    let query = build_retrieval_query("", &diffs, 4000);

    assert!(query.contains("ledger"));
    // `let`, `return`, `true`, `for`, `in` are all stopwords; `x` and `y` are
    // below the length floor.
    for noise in [" let ", " return ", " true ", " for "] {
        assert!(
            !format!(" {query} ").contains(noise),
            "{noise:?} in {query}"
        );
    }
    // Most frequent first: `ledger` appears three times, `ledger_entry` once.
    let ledger = query.find("ledger ").expect("ranked");
    let entry = query.find("ledger_entry").expect("ranked");
    assert!(ledger < entry, "{query}");
}

#[test]
fn the_same_diff_always_composes_the_same_query() {
    // Retrieval is billed per embedding call, and an unstable query is an
    // unstable cost as well as an untestable one.
    let diffs = wide_diff(40);
    assert_eq!(
        build_retrieval_query("Title", &diffs, 3000),
        build_retrieval_query("Title", &diffs, 3000)
    );
}

#[test]
fn a_zero_budget_composes_nothing_rather_than_a_stub() {
    assert!(build_retrieval_query("Title", &wide_diff(3), 0).is_empty());
}

#[test]
fn a_diff_with_no_hunks_still_yields_the_title_and_the_paths() {
    let diffs = vec![parse_file_patch("assets/logo.png", "")];
    let query = build_retrieval_query("Add the logo", &diffs, 400);

    assert!(query.contains("Add the logo"));
    assert!(query.contains("assets/logo.png"));
}

#[test]
fn a_title_longer_than_its_share_is_cut_on_a_character_boundary() {
    // A title is arbitrary UTF-8 written by whoever opened the pull request,
    // so a byte-wise truncation here would panic on a multi-byte character.
    let title = "é".repeat(500);
    let query = build_retrieval_query(&title, &[], 100);
    assert!(query.len() <= 100);
}
