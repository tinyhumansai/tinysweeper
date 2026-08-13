//! The `tests` lane: whether the tests in this pull request earn their keep.
//!
//! It **does not run anything**. Contributor code is never executed here — no
//! build, no dependency install, no test invocation — so this lane reasons
//! about tests from the diff alone: which files changed behaviour, which
//! changed tests, and whether the assertions in the latter could ever fail.
//!
//! The cheap part is deterministic and happens first. Classifying changed paths
//! into source and test is a job for a path table, not a model, and doing it
//! here means the lane can skip entirely — before spending a token — on the
//! documentation-only pull requests where demanding tests is pure noise.
//!
//! Unlike `security`, this lane runs one conversation for the whole pull
//! request. Its subject is the *relationship* between two sets of files, and a
//! reviewer shown one file in isolation cannot see it.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::evidence::diff::{FileDiff, render as render_diffs};
use crate::harness::prompt::{self, PromptInputs};
use crate::flows::runner::{self, PanelRequest};
use crate::harness::schema;
use crate::lanes::{Anchoring, Lane, LaneInput, LaneOutcome};
use crate::ports::model::Model;

/// The tests lane.
pub struct Tests {
    model: Arc<dyn Model>,
}

impl Tests {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Lane for Tests {
    fn id(&self) -> LaneId {
        LaneId::Tests
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        if !input.has_reviewable_content() {
            return Ok(LaneOutcome::skipped(
                "No added or modified lines to review.",
            ));
        }

        if let Some(skipped) = input.skip_as_draft() {
            return Ok(skipped);
        }

        let inventory = Inventory::of(input.diffs);

        // A change with no behavioural component does not need a test, and
        // asking for one is exactly the noise the gates exist to suppress. This
        // is decided deterministically, before any spend.
        if inventory.source.is_empty() {
            return Ok(LaneOutcome::skipped(
                "No behavioural change: nothing outside documentation, configuration and tests.",
            ));
        }

        let changed_paths = input.changed_paths();

        // Split, rather than sending the whole diff alongside a replay of it.
        // Passing both — which this did — puts every previously reviewed line
        // in the prompt twice, so a re-review cost *more* than the first
        // review rather than less. The inventory rides with the delta because
        // it is a summary of the current state, not evidence that was reviewed.
        let rendered = render_diffs(input.diffs);
        let (reviewed_evidence, fresh) =
            crate::evidence::replay::split(input.reviewed_evidence, &rendered);
        let evidence = format!("{}\n{}", inventory.render(), fresh);

        let built = prompt::build(&PromptInputs {
            repo_policy: input.repo_policy,
            extracted_rules: input.extracted_rules,
            reviewed_evidence: &reviewed_evidence,
            prior_findings: input.prior_findings,
            new_evidence: &evidence,
            changed_paths: &changed_paths,
            retrieved_context: input.retrieved_context,
            ..PromptInputs::new(LaneId::Tests, input.config)
        });

        // One panel over the whole pull request, rather than one call. The
        // lenses split this lane's subject in two — whether the change is
        // covered, and whether the tests that cover it could ever fail — and
        // neither reading is much use without the other.
        let panel = runner::run(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
            PanelRequest {
                lane: LaneId::Tests,
                schema: runner::schema_with_questions(schema::json_schema()),
                suffix: built.suffix(),
                system_of: &|lens| runner::system_with_charter(built.prefix(), lens),
            },
        )
        .await;

        let mut outcome = LaneOutcome::from_response(
            LaneId::Tests,
            runner::into_response(&panel),
            input.diffs,
            Anchoring::Strict,
            panel.spend.clone(),
        );
        outcome.summary.push_str(&panel.failure_note());
        Ok(outcome)
    }
}

/// Which changed files carry behaviour and which carry tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Files whose change could alter behaviour.
    pub source: Vec<String>,
    /// Files that are tests.
    pub tests: Vec<String>,
    /// Files that are neither: documentation, configuration, generated data.
    pub inert: Vec<String>,
}

impl Inventory {
    /// Classify the changed files.
    pub fn of(diffs: &[FileDiff]) -> Self {
        let mut inventory = Inventory::default();
        for diff in diffs {
            if diff.changed_lines.is_empty() {
                continue;
            }
            let path = diff.path.clone();
            if is_test_path(&path) {
                inventory.tests.push(path);
            } else if is_inert_path(&path) {
                inventory.inert.push(path);
            } else if adds_inline_tests(diff) {
                // Rust and Go keep tests beside the code. A file that only
                // gained a `#[cfg(test)]` block changed no behaviour, and
                // counting it as source would demand a test for a test.
                inventory.tests.push(path);
            } else {
                inventory.source.push(path);
            }
        }
        inventory
    }

    /// Render the classification for the prompt.
    ///
    /// The model is given the answer rather than asked to derive it: path
    /// conventions are a lookup table, and a model that guesses wrong about
    /// which file is a test produces a finding built on a wrong premise.
    pub fn render(&self) -> String {
        let mut out = String::from("Files changed, already classified:\n");
        for (label, paths) in [
            ("behaviour", &self.source),
            ("tests", &self.tests),
            ("neither", &self.inert),
        ] {
            if paths.is_empty() {
                continue;
            }
            let _ = writeln!(out, "- {label}: {}", paths.join(", "));
        }
        if self.tests.is_empty() {
            out.push_str(
                "\nNo test file changed. Decide whether the behavioural changes above needed \
                 one; a change that cannot regress silently does not.\n",
            );
        }
        out
    }
}

/// Whether `path` is a test file, by the conventions of the common languages.
fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/")
        || path.starts_with("tests/")
        || path.contains("/test/")
        || path.starts_with("test/")
        || path.contains("__tests__/")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || name.ends_with("Test.java")
        || name.ends_with("Tests.cs")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name == "test.rs"
        || name == "conftest.py"
}

/// Whether `path` cannot carry behaviour: documentation, configuration, assets.
fn is_inert_path(path: &str) -> bool {
    const INERT_SUFFIXES: &[&str] = &[
        ".md",
        ".markdown",
        ".txt",
        ".rst",
        ".adoc",
        ".png",
        ".jpg",
        ".jpeg",
        ".gif",
        ".svg",
        ".ico",
        ".lock",
        ".toml",
        ".json",
        ".yaml",
        ".yml",
        ".cfg",
        ".ini",
    ];
    // A workflow file is configuration, but it is executable configuration and
    // the security lane cares about it. It is still not something a unit test
    // covers, so it stays inert here.
    // Presets are executable review policy represented as data: altering one
    // changes enabled lanes and thresholds, so it must stay in the source
    // inventory even though the file is TOML.
    if path.starts_with("presets/") && path.ends_with(".toml") {
        return false;
    }
    INERT_SUFFIXES
        .iter()
        .any(|suffix| path.to_ascii_lowercase().ends_with(suffix))
        || path.starts_with("docs/")
        || path.starts_with(".github/")
}

/// Whether the added lines are an in-file test block rather than behaviour.
fn adds_inline_tests(diff: &FileDiff) -> bool {
    let mut awaiting_test_module = false;
    let mut test_module_depth: Option<i32> = None;
    let mut saw_test_module = false;

    for hunk in &diff.hunks {
        for line in &hunk.lines {
            if line.kind == crate::evidence::diff::LineKind::Removed {
                continue;
            }

            let trimmed = line.text.trim_start();
            let is_cfg_test = trimmed.starts_with("#[cfg(test)]");
            let starts_test_module = awaiting_test_module && trimmed.starts_with("mod tests");

            if is_cfg_test {
                awaiting_test_module = true;
            }

            if starts_test_module {
                let depth = brace_delta(trimmed);
                test_module_depth = (depth > 0).then_some(depth);
                awaiting_test_module = false;
                saw_test_module = true;
            } else if line.kind == crate::evidence::diff::LineKind::Added
                && test_module_depth.is_none()
                && !is_cfg_test
            {
                // Do not infer test-only status from an attribute alone. A
                // production addition before, between, or after test blocks
                // means this file must stay in the source inventory.
                return false;
            }

            if !starts_test_module && let Some(depth) = &mut test_module_depth {
                *depth += brace_delta(trimmed);
                if *depth <= 0 {
                    test_module_depth = None;
                }
            }
        }
    }

    saw_test_module
}

/// Net brace count for the small structural test-block classifier.
fn brace_delta(text: &str) -> i32 {
    text.bytes().fold(0, |depth, byte| match byte {
        b'{' => depth + 1,
        b'}' => depth - 1,
        _ => depth,
    })
}

// Not `mod tests`: this file *is* the tests lane, and a `tests` module inside
// `lanes::tests` is module inception.
#[cfg(test)]
mod lane_tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::parse_file_patch;
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn source_diff() -> FileDiff {
        parse_file_patch(
            "src/pricing.rs",
            "@@ -1,3 +1,5 @@\n fn total(items: &[Item]) -> u64 {\n+    if items.is_empty() {\n+        return 0;\n     }\n }\n",
        )
    }

    #[test]
    fn preset_toml_is_reviewable_policy_not_inert_configuration() {
        let diff = parse_file_patch(
            "presets/rust-library/preset.toml",
            "@@ -1 +1 @@\n+lanes = [\"security\"]\n",
        );
        assert_eq!(
            Inventory::of(&[diff]).source,
            ["presets/rust-library/preset.toml"]
        );
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "fix: handle the empty basket".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    async fn run_with(model: MockModel, config: &Config, diffs: &[FileDiff]) -> LaneOutcome {
        let pr = pull_request();
        Tests::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
            })
            .await
            .expect("lane runs")
    }

    // --- golden test -------------------------------------------------------

    #[tokio::test]
    async fn golden_an_untested_new_branch_is_reported() {
        let model = MockModel::panel(json!({
            "summary": "A new early-return branch has no test.",
            "findings": [
                {
                    "path": "src/pricing.rs", "line": 2,
                    "rule": "untested-branch",
                    "title": "Cover the empty-basket branch with a test",
                    "body": "Nothing fails if the early return is deleted.",
                    "severity": "high", "confidence": 0.85
                },
                {
                    "path": "src/pricing.rs", "line": 1,
                    "rule": "untested-branch",
                    "title": "Something about an unchanged line",
                    "body": "…", "severity": "high", "confidence": 0.8
                }
            ]
        }));
        let outcome = run_with(model, &config(), &[source_diff()]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(outcome.findings[0].rule, "untested-branch");
        assert_eq!(outcome.findings[0].severity, Severity::High);
        assert_eq!(outcome.findings[0].lane, LaneId::Tests);
        assert!(outcome.summary.contains("1 finding discarded"));
    }

    #[tokio::test]
    async fn a_documentation_only_change_never_calls_the_model() {
        // Demanding a test for a README edit is the noise the gates exist to
        // suppress, and it is cheaper to refuse than to ask.
        let model = MockModel::new();
        let diffs = vec![parse_file_patch("README.md", "@@ -1 +1,2 @@\n a\n+b\n")];
        let outcome = run_with(model.clone(), &config(), &diffs).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_test_only_change_needs_no_further_tests() {
        let model = MockModel::new();
        let diffs = vec![parse_file_patch(
            "src/pricing_test.rs",
            "@@ -1 +1,2 @@\n a\n+    assert_eq!(total(&[]), 0);\n",
        )];
        let outcome = run_with(model.clone(), &config(), &diffs).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn the_prompt_carries_the_classification_the_model_should_not_guess() {
        let model = MockModel::silent();
        let diffs = vec![
            source_diff(),
            parse_file_patch(
                "src/pricing_test.rs",
                "@@ -1 +1,2 @@\n a\n+    assert_eq!(total(&[]), 0);\n",
            ),
        ];
        run_with(model.clone(), &config(), &diffs).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("behaviour: src/pricing.rs"), "{prompt}");
        assert!(prompt.contains("tests: src/pricing_test.rs"));
    }

    #[tokio::test]
    async fn a_change_with_no_tests_at_all_says_so_in_the_prompt() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &[source_diff()]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("No test file changed."), "{prompt}");
    }

    #[test]
    fn an_in_file_rust_test_block_counts_as_a_test_not_as_behaviour() {
        let diff = parse_file_patch(
            "src/pricing.rs",
            "@@ -10,2 +10,6 @@\n }\n+#[cfg(test)]\n+mod tests {\n+    #[test]\n+    fn empty_basket_is_free() { assert_eq!(total(&[]), 0); }\n",
        );
        let inventory = Inventory::of(&[diff]);

        assert_eq!(inventory.tests, vec!["src/pricing.rs".to_string()]);
        assert!(inventory.source.is_empty());
    }

    #[tokio::test]
    async fn mixed_inline_tests_and_production_edits_stay_in_the_source_inventory() {
        let model = MockModel::silent();
        let diff = parse_file_patch(
            "src/pricing.rs",
            "@@ -1,3 +1,10 @@\n fn total(items: &[Item]) -> u64 {\n+    let discount = 0;\n     items.len() as u64\n }\n+#[cfg(test)]\n+mod tests {\n+    #[test]\n+    fn total_is_counted() { assert_eq!(total(&[]), 0); }\n+}\n+pub fn discount() -> u64 { discount }\n",
        );

        let inventory = Inventory::of(std::slice::from_ref(&diff));
        assert_eq!(inventory.source, vec!["src/pricing.rs"]);
        assert!(inventory.tests.is_empty());

        run_with(model.clone(), &config(), &[diff]).await;
        // One call per lens: the panel replaced the single call this lane used
        // to make, so the assertion is that it ran at all, at the panel's width.
        assert_eq!(
            model.calls(),
            crate::flows::panel::lenses(LaneId::Tests).len(),
            "mixed edits must be reviewed"
        );
        assert!(
            model
                .last_prompt()
                .expect("recorded")
                .contains("behaviour: src/pricing.rs")
        );
    }

    #[test]
    fn test_paths_are_recognised_across_the_common_conventions() {
        for path in [
            "tests/api.rs",
            "src/foo/test.rs",
            "pkg/thing_test.go",
            "app/__tests__/button.tsx",
            "src/button.test.ts",
            "src/button.spec.ts",
            "tests/test_api.py",
            "src/FooTest.java",
        ] {
            assert!(is_test_path(path), "{path} was not recognised as a test");
        }
        assert!(!is_test_path("src/latest.rs"), "substring false positive");
    }

    #[tokio::test]
    async fn the_lane_never_executes_anything() {
        // The invariant is structural — this lane holds a `Model` and nothing
        // else — but the assertion is here so that adding a command runner has
        // to consciously delete a test that says not to.
        let source = std::fs::read_to_string(file!()).expect("reads its own source");
        let body = source
            .split("#[cfg(test)]")
            .next()
            .expect("source before the tests");
        for forbidden in ["std::process", "Command::new", "tokio::process"] {
            assert!(
                !body.contains(forbidden),
                "the tests lane must not execute contributor code: found `{forbidden}`"
            );
        }
    }
}
