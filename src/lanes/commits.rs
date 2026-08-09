//! The `commits` lane: what entered the history, and how it was described.
//!
//! Two jobs, and only one of them needs a model:
//!
//! 1. **Adjudicating what the scanners found.** Secrets, oversized blobs and
//!    committed build output are found deterministically, before any token is
//!    spent, and those findings are facts. This lane republishes them unchanged
//!    and asks the model to say whether each is a genuine problem here. A model
//!    verdict never deletes one: "the reviewer was talked out of a committed
//!    private key" is not a failure mode anyone can audit, and the whole point
//!    of running a regular expression first is that it cannot be argued with.
//! 2. **Judging the commit range itself.** Messages that describe nothing,
//!    merge noise, unrelated work bundled into one commit, an author identity
//!    that looks accidental. None of that has a line number, so this lane's
//!    findings are demoted to summary-only rather than dropped — see
//!    `lanes::anchor`.
//!
//! Nothing here quotes a credential. The scanner findings carry a redacted hint
//! and never the value, and every free-text field the model produces is
//! scrubbed on the way into a [`Finding`](crate::findings::types::Finding).

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::evidence::diff::truncate_patch;
use crate::falsify::Falsifier;
use crate::forge::types::Commit;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema;
use crate::lanes::security::render_scanner;
use crate::lanes::{Anchoring, Lane, LaneInput, LaneOutcome};
use crate::ports::model::{Message, Model, ModelRequest, Spend};
use crate::scan::secrets::scrub;
use crate::scan::types::ScanKind;

/// The scanner findings this lane owns.
///
/// A partition with [`security::ADJUDICATES`](crate::lanes::security::ADJUDICATES):
/// two lanes discussing one scanner match would report it twice.
pub const ADJUDICATES: [ScanKind; 3] = [ScanKind::Secret, ScanKind::Blob, ScanKind::Junk];

/// How many commits are shown to the model.
///
/// A branch with two hundred commits is not a branch whose messages anybody is
/// going to rewrite, and sending them all is a large bill for a review nobody
/// asked for.
const MAX_COMMITS: usize = 50;

/// How many patch bytes the whole range may spend.
///
/// The commit list has been capped since this lane existed; the patches are the
/// part that can be a thousand times larger than the messages, so they get a
/// cap of their own. Once it is gone the remaining commits are still listed
/// with their messages — losing the history entirely would be a worse answer
/// than losing the diffs — and the omission is stated in the evidence.
const MAX_PATCH_BYTES: usize = 48 * 1024;

/// How much of the budget any single commit may take.
///
/// Without it, one commit that vendors a dependency spends the whole range's
/// budget and every commit after it is reduced to its subject line — which is
/// precisely the state that produced findings built from subjects alone.
const MAX_COMMIT_PATCH_BYTES: usize = 12 * 1024;

/// The smallest remaining budget worth spending on a patch at all.
const MIN_USEFUL_PATCH_BYTES: usize = 512;

/// The commits lane.
pub struct Commits {
    model: Arc<dyn Model>,
}

impl Commits {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Lane for Commits {
    fn id(&self) -> LaneId {
        LaneId::Commits
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        let scanner = input.scanner_findings_of(&ADJUDICATES);

        // A scanner finding is on its own reason to run: a committed key has to
        // be reported whether or not there is a commit message worth reading.
        if input.commits.is_empty() && scanner.is_empty() {
            return Ok(LaneOutcome::skipped(
                "No commits to review and nothing for the scanners to report.",
            ));
        }

        if let Some(skipped) = input.skip_as_draft() {
            return Ok(skipped);
        }

        let scanner_evidence = render_scanner(&scanner, None);
        let evidence = render_commits(input.commits);
        let changed_paths = input.changed_paths();

        let built = prompt::build(&PromptInputs {
            repo_policy: input.repo_policy,
            extracted_rules: input.extracted_rules,
            prior_findings: input.prior_findings,
            new_evidence: &evidence,
            evidence_label: "commits",
            changed_paths: &changed_paths,
            scanner_evidence: &scanner_evidence,
            ..PromptInputs::new(LaneId::Commits, input.config)
        });

        let response = self
            .model
            .complete(ModelRequest {
                model: input.config.model_for(LaneId::Commits).to_string(),
                messages: vec![
                    Message::system(built.prefix()),
                    Message::user(built.suffix()),
                ],
                schema: schema::json_schema(),
                schema_name: "tinysweeper_commits".into(),
                max_tokens: input.config.models.max_tokens,
            })
            .await?;

        // Taken before the value is moved out: the spend belongs to the model
        // that answered, whether or not its answer parses.
        let spend = Spend::of(&response);
        let parsed = schema::parse(LaneId::Commits, response.value)?;

        // `Demote`: a finding about a commit message has no line to sit on, and
        // dropping every unanchored one would leave the lane with nothing but
        // the scanner's output.
        let mut outcome = LaneOutcome::from_response(
            LaneId::Commits,
            parsed,
            input.diffs,
            Anchoring::Demote,
            spend,
        );

        // The falsification pass, on the model's findings and *before* the
        // scanner's are merged in. Two reasons for that order:
        //
        // - This lane's characteristic failure is a claim the evidence
        //   disproves — reading "kernel bypass" in a subject and reporting
        //   hardware access from a patch that does nothing of the sort (issue
        //   #47). That is exactly what a falsifier can prove wrong, which is
        //   not true of most lanes' output.
        // - A scanner match must never be filtered. The document the filter
        //   sees is the commit range, and a committed key found by a regular
        //   expression is not up for a model's opinion — the same rule that
        //   makes `merge_scanner_findings` unconditional.
        //
        // The filter is shown the same evidence the lane was, so a finding
        // about a commit message is judged against the messages rather than
        // rejected for being absent from a diff.
        let filtered = Falsifier::new(self.model.as_ref(), input.config)
            .filter(
                LaneId::Commits,
                std::mem::take(&mut outcome.findings),
                &evidence,
            )
            .await;
        outcome.spend.merge(filtered.spend);
        outcome.findings = filtered.findings;
        if !filtered.rejected.is_empty() {
            let _ = write!(
                outcome.summary,
                "\n\n{} finding(s) dropped: the commit range disproved them.",
                filtered.rejected.len()
            );
        }

        merge_scanner_findings(&mut outcome, &scanner);
        Ok(outcome)
    }
}

/// Republish this lane's scanner findings, and drop model findings that restate
/// one.
///
/// The same contract as the security lane's, for the same reason; it is
/// separate only because the lane label differs.
fn merge_scanner_findings(outcome: &mut LaneOutcome, scanner: &[&crate::scan::types::Finding]) {
    let mut deterministic: Vec<crate::findings::types::Finding> = scanner
        .iter()
        .map(|scan| crate::findings::types::Finding::from((*scan).clone()))
        .collect();

    outcome.findings.retain(|model| {
        !deterministic
            .iter()
            .any(|scan| scan.path == model.path && scan.rule == model.rule)
    });

    deterministic.append(&mut outcome.findings);
    outcome.findings = deterministic;
}

/// Render the commit range for the prompt.
///
/// Author emails are scrubbed like everything else and shown because the lane
/// is asked to judge whether an identity looks accidental — a commit authored
/// as `root@localhost` is worth a word. They are evidence, and they never reach
/// a comment: the model is told to describe the problem, not to quote it.
/// Each commit's patch follows its message, because the lane is specified to
/// review `git log -p`: without the patch it can only judge the subject line,
/// and a subject line is exactly the thin evidence that produced a confident
/// security finding about a change that did nothing of the kind (issue #47).
///
/// Patches are scrubbed like everything else here. The security lane sees the
/// diff unredacted; this one adjudicates the secret scanner's output, and a
/// lane that must never quote a credential is better off never holding one.
fn render_commits(commits: &[Commit]) -> String {
    let mut out = String::new();
    let mut budget = MAX_PATCH_BYTES;
    let mut over_budget = 0usize;
    let mut unfetched = 0usize;

    for commit in commits.iter().take(MAX_COMMITS) {
        let short: String = commit.sha.chars().take(8).collect();
        let _ = writeln!(
            out,
            "- {short} <{}> {}\n{}",
            scrub(&commit.author_email),
            scrub(&commit.author_name),
            indent(&scrub(&commit.message))
        );

        match commit.patch.as_deref() {
            None => {
                unfetched += 1;
                let _ = writeln!(out, "    [no patch was fetched for this commit]");
            }
            // A few hundred bytes of patch followed by a truncation note is
            // worse than an honest omission: it looks like the whole change.
            Some(_) if budget < MIN_USEFUL_PATCH_BYTES => {
                over_budget += 1;
                let _ = writeln!(
                    out,
                    "    [patch omitted: the range's patch budget is spent]"
                );
            }
            Some(patch) => {
                let allowance = budget.min(MAX_COMMIT_PATCH_BYTES);
                let shown = truncate_patch(&scrub(patch), allowance);
                budget = budget.saturating_sub(shown.len());
                let _ = writeln!(out, "{}", indent(&shown));
            }
        }
    }

    if commits.len() > MAX_COMMITS {
        let _ = writeln!(
            out,
            "… and {} more commits, not shown.",
            commits.len() - MAX_COMMITS
        );
    }
    // Stated, not hidden. The instructions tell the model it may not raise a
    // finding about a commit whose patch it was not shown, and it can only obey
    // that if it knows which those are.
    if unfetched > 0 {
        let _ = writeln!(out, "[{unfetched} commit(s) arrived without a patch.]");
    }
    if over_budget > 0 {
        let _ = writeln!(
            out,
            "[{over_budget} commit(s) had their patch omitted for size.]"
        );
    }
    out
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::{FileDiff, parse_file_patch};
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use crate::scan::types::{Finding as ScanFinding, redact};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch(
            "src/config.rs",
            "@@ -1,2 +1,3 @@\n a\n+const K: &str = \"…\";\n b\n",
        )]
    }

    fn commits() -> Vec<Commit> {
        vec![
            Commit {
                sha: "abc1234def".into(),
                message: "wip".into(),
                author_name: "Someone".into(),
                author_email: "someone@example.com".into(),
                patch: Some("--- a/src/config.rs\n+++ b/src/config.rs\n@@ -1,2 +1,3 @@\n a\n+const K: &str = \"…\";\n b\n".into()),
            },
            Commit {
                sha: "0987654fed".into(),
                message: "fix: guard the empty basket\n\nTotals now return zero.".into(),
                author_name: "Someone".into(),
                author_email: "someone@example.com".into(),
                patch: Some(
                    "--- a/src/basket.rs\n+++ b/src/basket.rs\n@@ -3,2 +3,3 @@\n fn total() {\n+    if items.is_empty() { return 0; }\n }\n"
                        .into(),
                ),
            },
        ]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "fix: things".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    /// The AWS key literal, split so this file does not itself contain one.
    fn key() -> String {
        format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE")
    }

    fn secret_finding() -> ScanFinding {
        ScanFinding::new(
            ScanKind::Secret,
            Severity::Critical,
            "src/config.rs",
            "aws-access-key-id",
            "Remove the committed AWS access key",
            "Rotate it, then purge it from history.",
        )
        .at_line(2)
        .with_hint(redact(&key()))
    }

    async fn run_with(
        model: MockModel,
        config: &Config,
        commits: &[Commit],
        scan_findings: &[ScanFinding],
    ) -> LaneOutcome {
        let pr = pull_request();
        let diffs = diffs();
        Commits::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings,
                commits,
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
            })
            .await
            .expect("lane runs")
    }

    // --- golden tests ------------------------------------------------------

    #[tokio::test]
    async fn golden_a_committed_secret_survives_and_a_message_nit_is_demoted() {
        let model = MockModel::always(json!({
            "summary": "The key is real; one commit message says nothing.",
            "findings": [{
                "path": "src/config.rs", "line": 900,
                "rule": "uninformative-commit-message",
                "title": "Describe what the `wip` commit does",
                "body": "`wip` tells a future reader nothing.",
                "severity": "medium", "confidence": 0.8
            }]
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 2, "{:#?}", outcome.findings);

        // The scanner's finding comes first and is untouched.
        let secret = &outcome.findings[0];
        assert_eq!(secret.rule, "aws-access-key-id");
        assert_eq!(secret.severity, Severity::Critical);
        assert_eq!(secret.confidence, 1.0);
        assert_eq!(secret.line, Some(2));

        // The model's finding kept, but with no anchor: line 900 is not a
        // changed line, and a commit message has no line at all.
        let message = &outcome.findings[1];
        assert_eq!(message.rule, "uninformative-commit-message");
        assert_eq!(message.line, None);
        assert_eq!(message.lane, LaneId::Commits);
    }

    #[tokio::test]
    async fn golden_a_secret_value_never_reaches_a_summary_or_a_finding() {
        // The model is shown the diff, so it can quote a credential back. This
        // is the last place that can stop it.
        let model = MockModel::always(json!({
            "summary": format!("The committed key is {}, which is real.", key()),
            "findings": [{
                "path": "src/config.rs", "line": 2,
                "rule": "hardcoded-credential",
                "title": format!("Remove {}", key()),
                "body": format!("`{}` is committed on line 2.", key()),
                "severity": "critical", "confidence": 1.0,
                "suggestion": format!("let k = std::env::var(\"K\")?; // was {}", key())
            }]
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        let rendered = serde_json::to_string(&outcome.findings).expect("serialises");
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
        assert!(
            !crate::scan::secrets::scrub(&outcome.summary).contains("IOSFODNN7EXAMPLE"),
            "{}",
            outcome.summary
        );
        assert!(
            rendered.contains("AKIA"),
            "the vendor prefix survives so a human knows what to rotate"
        );
    }

    #[tokio::test]
    async fn the_model_cannot_talk_the_lane_out_of_a_committed_secret() {
        let model = MockModel::always(json!({
            "summary": "That key is just an example, it is fine.",
            "findings": []
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule, "aws-access-key-id");
    }

    #[tokio::test]
    async fn a_model_finding_restating_a_scanner_match_is_dropped() {
        let model = MockModel::always(json!({
            "summary": "The scanner is right.",
            "findings": [{
                "path": "src/config.rs", "line": 2,
                "rule": "aws-access-key-id",
                "title": "Remove the committed key",
                "body": "…", "severity": "critical", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(
            outcome.findings[0].confidence, 1.0,
            "the scanner's certainty, not the model's estimate"
        );
    }

    #[tokio::test]
    async fn the_commit_messages_reach_the_prompt_fenced_as_data() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &commits(), &[]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("````commits"), "{prompt}");
        assert!(prompt.contains("fix: guard the empty basket"));
        assert!(prompt.contains("abc1234d"));
    }

    #[tokio::test]
    async fn the_scanner_findings_are_given_as_evidence_to_adjudicate() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &commits(), &[secret_finding()]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("scanner-findings"));
        assert!(prompt.contains("aws-access-key-id"));
        assert!(
            !prompt.contains("IOSFODNN7EXAMPLE"),
            "the value must not reach the prompt either"
        );
    }

    /// A commit whose subject says something alarming and whose patch does
    /// nothing of the kind. The regression from issue #47: the lane read
    /// "kernel bypass" in a subject and reported that a container had been
    /// given direct hardware access, from a patch that no-ops one function.
    fn loaded_subject_commits() -> Vec<Commit> {
        vec![Commit {
            sha: "feed1234ab".into(),
            message: "fix: kernel bypass shim so mongod starts on 6.19".into(),
            author_name: "Someone".into(),
            author_email: "someone@example.com".into(),
            patch: Some(
                "--- a/docker/entrypoint.py\n+++ b/docker/entrypoint.py\n\
                 @@ -10,6 +10,9 @@\n def main():\n\
                 +def _check_tuning():\n+    # no-op: the probe fails on 6.19\n+    return None\n\
                 \n     start_mongod()\n"
                    .into(),
            ),
        }]
    }

    // --- issue #47: a subject is not evidence ------------------------------

    #[tokio::test]
    async fn golden_a_loaded_subject_with_a_benign_patch_raises_no_finding() {
        // First call: the lane's model does what it did on PR #45 — reads the
        // subject and reports hardware access. Second call: the falsifier,
        // shown the same commit range, proves it wrong from the patch.
        let model = MockModel::new()
            .then(json!({
                "summary": "This branch grants a container direct hardware access.",
                "findings": [{
                    "path": "docker/entrypoint.py",
                    "rule": "privileged-container",
                    "title": "Do not bypass the host kernel's network stack",
                    "body": "The commit enables DPDK-style kernel-bypass networking, \
                             giving the MongoDB container direct hardware access.",
                    "severity": "critical", "confidence": 0.9
                }]
            }))
            .then(json!({
                "incorrect": [{
                    "index": 1,
                    "reason": "the patch only makes one tuning probe return None; \
                               nothing in it touches networking or privileges"
                }]
            }));

        let outcome = run_with(model.clone(), &config(), &loaded_subject_commits(), &[]).await;

        assert!(
            outcome.findings.is_empty(),
            "a finding built from a subject line survived: {:#?}",
            outcome.findings
        );
        assert!(outcome.summary.contains("disproved"), "{}", outcome.summary);
        assert_eq!(model.calls(), 2, "the falsification pass has to have run");
    }

    #[tokio::test]
    async fn the_instructions_say_a_word_in_a_message_is_not_evidence() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &loaded_subject_commits(), &[]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(
            prompt.contains("never evidence of the thing it names"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Every finding must quote the patch"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Anything you inferred from a message alone"),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn the_patch_for_each_commit_reaches_the_prompt() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &commits(), &[]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("+++ b/src/basket.rs"), "{prompt}");
        assert!(prompt.contains("if items.is_empty()"), "{prompt}");
    }

    #[tokio::test]
    async fn a_scanner_finding_is_not_up_for_falsification() {
        // The falsifier is told this one is wrong. It is a regular expression's
        // match, not a model's opinion, and it stays.
        let model = MockModel::new()
            .then(json!({"summary": "…", "findings": [{
                "path": "src/config.rs", "rule": "wobbly", "title": "Something else",
                "body": "…", "severity": "low", "confidence": 0.5
            }]}))
            .then(
                json!({"incorrect": [{"index": 1, "reason": "not in the range"},
                                       {"index": 2, "reason": "the key is an example"}]}),
            );

        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(outcome.findings[0].rule, "aws-access-key-id");
    }

    #[test]
    fn a_commit_with_no_patch_is_rendered_as_such_rather_than_as_an_empty_diff() {
        let commits = vec![Commit {
            sha: "abc1234".into(),
            message: "chore: something".into(),
            patch: None,
            ..Commit::default()
        }];
        let rendered = render_commits(&commits);

        assert!(
            rendered.contains("[no patch was fetched for this commit]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("1 commit(s) arrived without a patch."),
            "{rendered}"
        );
    }

    #[test]
    fn the_patch_budget_is_bounded_and_the_omission_is_reported() {
        let big = format!(
            "--- a/vendor.rs\n{}",
            "+a line of vendored code\n".repeat(4_000)
        );
        let commits: Vec<Commit> = (0..12)
            .map(|i| Commit {
                sha: format!("{i:040x}"),
                message: format!("chore: vendor {i}"),
                patch: Some(big.clone()),
                ..Commit::default()
            })
            .collect();
        let rendered = render_commits(&commits);

        assert!(
            rendered.len() < MAX_PATCH_BYTES * 2,
            "the range spent {} bytes",
            rendered.len()
        );
        assert!(
            rendered.contains("further bytes of this patch omitted"),
            "truncation is stated"
        );
        assert!(
            rendered.contains("had their patch omitted for size"),
            "the dropped commits are stated: {rendered}"
        );
        // Every commit is still listed, patch or no patch.
        assert!(rendered.contains("chore: vendor 11"), "{rendered}");
    }

    #[tokio::test]
    async fn nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[], &[]).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_scanner_finding_alone_is_enough_to_run_the_lane() {
        let outcome = run_with(MockModel::silent(), &config(), &[], &[secret_finding()]).await;

        assert!(outcome.skipped.is_none());
        assert_eq!(outcome.findings.len(), 1);
    }

    #[test]
    fn a_long_branch_is_truncated_rather_than_billed_in_full() {
        let many: Vec<Commit> = (0..MAX_COMMITS + 5)
            .map(|i| Commit {
                sha: format!("{i:040x}"),
                message: format!("commit {i}"),
                ..Commit::default()
            })
            .collect();
        let rendered = render_commits(&many);

        assert!(rendered.contains("and 5 more commits"), "{rendered}");
        assert!(!rendered.contains("commit 52"));
    }

    #[test]
    fn a_credential_pasted_into_a_commit_message_is_scrubbed_before_the_prompt() {
        let commits = vec![Commit {
            sha: "abc1234".into(),
            message: format!("chore: add key {}", key()),
            author_name: "Someone".into(),
            author_email: "someone@example.com".into(),
            patch: Some(format!("+const K: &str = \"{}\";\n", key())),
        }];
        let rendered = render_commits(&commits);

        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
    }
}
