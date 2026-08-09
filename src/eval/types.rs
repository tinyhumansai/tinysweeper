//! What a labelled case is, and what scoring one produces.
//!
//! Always compiled. Nothing here calls a model or touches the network — a case
//! is data, a scorecard is arithmetic over data, and both are readable in a
//! pull request diff.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::types::{LaneId, Severity};
use crate::forge::types::{ChangedFile, Commit, IssueComment, PullRequest};

/// The corpus schema version this file was written against.
///
/// Checked on load rather than assumed: a case written for an older matcher
/// scores differently under a newer one, and silently re-scoring it under new
/// rules is how a baseline stops meaning anything.
pub const SCHEMA: u32 = 1;

/// One labelled pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Corpus schema version. See [`SCHEMA`].
    pub schema: u32,
    /// Stable slug, also the cassette and report key.
    pub id: String,
    /// One line for a human scanning the corpus.
    #[serde(default)]
    pub title: String,
    /// Path to the frozen fixture, relative to the case file.
    pub fixture: String,
    /// Which lanes to run. Empty means whatever the config enables.
    #[serde(default)]
    pub lanes: Vec<String>,
    /// Free-form tags for slicing a report — `rust`, `security`, `clean`.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Where the ground truth came from.
    pub provenance: Provenance,
    /// Ceilings this case is expected to review within.
    #[serde(default)]
    pub budget: Budget,
    /// Whether `expected` is the **complete** set of true findings.
    ///
    /// Off by default, and the default is the honest one. You can only call an
    /// unmatched finding a false positive if you have asserted every true one —
    /// otherwise the reviewer is penalised for finding something real that
    /// nobody had got round to labelling. A case labelled only for what the
    /// review must *not* say is not evidence about what it should.
    ///
    /// So a non-exhaustive case contributes to recall and to the forbidden
    /// count, and contributes nothing to precision. Setting this to `true` is a
    /// claim: *I read this whole diff and these are all the defects in it.*
    #[serde(default)]
    pub exhaustive: bool,
    /// What a good reviewer should find.
    #[serde(default)]
    pub expected: Vec<Expected>,
    /// What it must not say.
    #[serde(default)]
    pub forbidden: Vec<Forbidden>,
}

impl Case {
    /// Expectations that count against recall.
    ///
    /// `optional` ones are reported separately: they are the findings a very
    /// good reviewer would make, and holding the headline metric hostage to
    /// them makes every real improvement look like a failure.
    pub fn required(&self) -> impl Iterator<Item = &Expected> {
        self.expected.iter().filter(|e| !e.optional)
    }

    /// Whether the case asserts that a good review finds *nothing*.
    ///
    /// Both halves are required. An unlabelled case also has no expectations,
    /// and treating it as clean would score every real finding on it as noise.
    pub fn is_clean(&self) -> bool {
        self.exhaustive && self.expected.is_empty()
    }
}

/// Where a case's ground truth came from.
///
/// Not decoration, and the loader enforces it. An expectation written by
/// reading the bot's own output measures nothing except whether the bot still
/// agrees with itself — it bakes today's blind spots into the target. Every
/// case has to cite something the bot did not produce: a follow-up fix, an
/// acted-on human review comment, a revert.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// The repository the pull request came from.
    pub repo: String,
    /// Its number.
    pub pr: u64,
    /// A URL or commit sha showing the finding was real, external to the bot.
    pub evidence: String,
    /// Who labelled it.
    pub labelled_by: String,
    /// When, as `YYYY-MM-DD`.
    #[serde(default)]
    pub labelled_on: String,
}

/// What this case may spend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    /// Dollars, per review of this case.
    pub max_cost_usd: f64,
}

impl Default for Budget {
    fn default() -> Self {
        // The target the whole council design is built around. A case that
        // needs more says so explicitly.
        Self { max_cost_usd: 0.02 }
    }
}

/// A finding a good reviewer should produce.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// Stable id, so a report can name which expectation was missed.
    pub id: String,
    /// The file it should be on. `*` matches any file.
    pub path: String,
    /// The line range, inclusive. Absent means anywhere in the file.
    #[serde(default)]
    pub lines: Option<(u64, u64)>,
    /// The lowest severity that counts as finding it.
    #[serde(default)]
    pub severity_min: Option<Severity>,
    /// Which lanes may claim it. Empty means any.
    #[serde(default)]
    pub lanes: Vec<LaneId>,
    /// What the defect is, for a human reading the corpus. Never matched on.
    pub summary: String,
    /// Every slot must be satisfied by the finding's title or body.
    ///
    /// A slot is an alternation: `"evict|expire|unbounded"` is satisfied by any
    /// one of the three. Deliberately not a regular expression — a corpus that
    /// needs a regex engine to read is a corpus nobody audits.
    #[serde(default)]
    pub must_mention: Vec<String>,
    /// Excluded from headline recall and reported on its own line.
    #[serde(default)]
    pub optional: bool,
}

/// Something the review must not say.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forbidden {
    /// Stable id.
    pub id: String,
    /// The file, or `*` for anywhere.
    pub path: String,
    /// The line range, inclusive. Absent means anywhere in the file.
    #[serde(default)]
    pub lines: Option<(u64, u64)>,
    /// Which lanes this rules out. Empty means any.
    ///
    /// Some defects are structural rather than textual — "the `description`
    /// lane must not anchor to implementation code" is about *which lane
    /// landed where*, and no keyword expresses it. Scoping by lane is what
    /// makes that case sayable at all.
    #[serde(default)]
    pub lanes: Vec<LaneId>,
    /// Why this is not a defect. Written for the human who will disagree.
    pub reason: String,
    /// Any slot matching means the review said the forbidden thing.
    ///
    /// The opposite polarity to [`Expected::must_mention`]: one hit is enough,
    /// because this is looking for a specific wrong claim rather than
    /// confirming a right one. Empty is allowed when `lanes`, `lines` or a
    /// concrete `path` already narrows it — see `corpus::validate`.
    #[serde(default)]
    pub matches: Vec<String>,
}

impl Forbidden {
    /// Whether this entry narrows anything at all.
    ///
    /// A `path = "*"` entry with no lane, no line range and no keywords
    /// forbids *every* finding, which is never what anybody meant and would
    /// silently zero the case.
    pub fn is_constrained(&self) -> bool {
        !self.matches.is_empty()
            || !self.lanes.is_empty()
            || self.lines.is_some()
            || self.path != "*"
    }
}

/// The frozen forge state one case is reviewed against.
///
/// A subset of `MockState`: only what a review actually reads. Everything here
/// already derives `Serialize`, so freezing a real pull request needs no new
/// derive on a forge type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Fixture {
    /// The pull request under review.
    pub pull_request: PullRequest,
    /// Its changed files, patches included.
    pub files: Vec<ChangedFile>,
    /// Its commits.
    pub commits: Vec<Commit>,
    /// Issue comments already on it.
    pub comments: Vec<IssueComment>,
    /// File contents at the head sha, keyed by path.
    ///
    /// Only the instruction files the knowledge pass reads. Freezing the whole
    /// tree would make a fixture unreviewable in a diff and would carry the
    /// repository into the corpus.
    pub blobs: BTreeMap<String, String>,
}

/// What one produced finding turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Matched an expectation.
    TruePositive,
    /// Matched an expectation another finding had already claimed.
    ///
    /// Reported apart from a false positive on purpose: saying the same true
    /// thing twice and inventing something are different defects with different
    /// fixes, and folding them together hides which one is happening.
    Duplicate,
    /// Matched nothing, on a case whose labels claim to be complete.
    FalsePositive,
    /// Matched nothing, on a case that never claimed to list everything.
    ///
    /// Not a defect and not a credit. The corpus has no opinion about this
    /// finding, and pretending otherwise in either direction is how a
    /// precision figure stops meaning anything.
    Unscored,
    /// Matched a `[[forbidden]]` entry.
    Forbidden,
}

/// One produced finding, and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Judged {
    /// Which lane produced it.
    pub lane: LaneId,
    /// Where it landed.
    pub path: String,
    /// The line it anchored to, if any.
    pub line: Option<u64>,
    /// Its title.
    pub title: String,
    /// What it turned out to be.
    pub verdict: Verdict,
    /// The expectation or forbidden entry it matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    /// Why the matcher decided that. Written so a disagreement is arguable.
    pub reason: String,
}

/// One case's result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseScore {
    /// The case id.
    pub id: String,
    /// Expectations matched.
    pub true_positives: usize,
    /// Required expectations nobody found, by id.
    pub missed: Vec<String>,
    /// Findings that matched nothing, on an `exhaustive` case.
    pub false_positives: usize,
    /// Findings the corpus has no opinion about. See [`Verdict::Unscored`].
    #[serde(default)]
    pub unscored: usize,
    /// Whether this case's labels claim to be complete.
    #[serde(default)]
    pub exhaustive: bool,
    /// Findings that restated an already-claimed expectation.
    pub duplicates: usize,
    /// Findings that matched a `[[forbidden]]` entry, by id.
    pub forbidden_hits: Vec<String>,
    /// Optional expectations matched, reported apart from recall.
    pub optional_hits: usize,
    /// Every finding and its verdict, so a number can be argued with.
    pub judged: Vec<Judged>,
    /// What the run cost.
    pub cost_usd: f64,
    /// Whether it exceeded `[budget].max_cost_usd`.
    pub over_budget: bool,
    /// Fresh prompt tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens served from cache.
    pub cached_tokens: u64,
    /// Wall-clock seconds.
    pub wall_secs: f64,
    /// Every model that answered.
    pub models: Vec<String>,
    /// Per-lane dollars, so the expensive lane is named rather than averaged.
    pub lane_costs: BTreeMap<String, f64>,
    /// The run failed outright. Scored as zero recall rather than skipped: a
    /// reviewer that crashes found nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CaseScore {
    /// Required expectations that existed at all.
    pub fn required(&self) -> usize {
        self.true_positives + self.missed.len()
    }
}

/// Every case's result, plus the totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scorecard {
    /// The corpus digest the run was scored against.
    pub corpus_digest: String,
    /// The configuration digest the run used.
    pub config_digest: String,
    /// Whether any answer was replayed by call order rather than by key.
    pub loose_replays: usize,
    /// Per case, sorted by id so two reports diff cleanly.
    pub cases: Vec<CaseScore>,
}

impl Scorecard {
    /// Findings that matched an expectation.
    pub fn true_positives(&self) -> usize {
        self.cases.iter().map(|c| c.true_positives).sum()
    }

    /// Required expectations nobody found.
    pub fn missed(&self) -> usize {
        self.cases.iter().map(|c| c.missed.len()).sum()
    }

    /// Findings that matched nothing.
    pub fn false_positives(&self) -> usize {
        self.cases.iter().map(|c| c.false_positives).sum()
    }

    /// Findings that said something a case forbids.
    pub fn forbidden_hits(&self) -> usize {
        self.cases.iter().map(|c| c.forbidden_hits.len()).sum()
    }

    /// Findings that restated a claimed expectation.
    pub fn duplicates(&self) -> usize {
        self.cases.iter().map(|c| c.duplicates).sum()
    }

    /// Share of required expectations that were found.
    ///
    /// `None` when the corpus asserts nothing to find, which is a real corpus
    /// — an all-clean one — and not a division by zero to paper over.
    pub fn recall(&self) -> Option<f64> {
        let required: usize = self.cases.iter().map(CaseScore::required).sum();
        (required > 0).then(|| self.true_positives() as f64 / required as f64)
    }

    /// Share of posted findings that were real, over `exhaustive` cases only.
    ///
    /// Restricted to cases whose labels claim to be complete, because that is
    /// the only place an unmatched finding is evidence of anything. Counting a
    /// half-labelled case would penalise the reviewer for finding something
    /// real that nobody had written down yet.
    ///
    /// A forbidden hit counts against precision as well as being reported on
    /// its own: it is a false positive somebody already wrote down.
    pub fn precision(&self) -> Option<f64> {
        let scored: Vec<&CaseScore> = self.cases.iter().filter(|c| c.exhaustive).collect();
        let found: usize = scored.iter().map(|c| c.true_positives).sum();
        let posted: usize = found
            + scored
                .iter()
                .map(|c| c.false_positives + c.duplicates + c.forbidden_hits.len())
                .sum::<usize>();
        (posted > 0).then(|| found as f64 / posted as f64)
    }

    /// Findings the corpus has no opinion about.
    ///
    /// A large number here means the corpus is thin, not that the reviewer is
    /// noisy — it is the figure that says how much labelling is still owed.
    pub fn unscored(&self) -> usize {
        self.cases.iter().map(|c| c.unscored).sum()
    }

    /// Harmonic mean of precision and recall.
    pub fn f1(&self) -> Option<f64> {
        let (p, r) = (self.precision()?, self.recall()?);
        (p + r > 0.0).then(|| 2.0 * p * r / (p + r))
    }

    /// Findings posted on cases whose correct output is nothing.
    ///
    /// The most legible noise number in the report: every one of these is a
    /// comment on a pull request that had nothing wrong with it.
    pub fn clean_case_findings(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.exhaustive && case.required() == 0)
            .map(|case| case.judged.len())
            .sum()
    }

    /// Total dollars across the corpus.
    pub fn cost_usd(&self) -> f64 {
        self.cases.iter().map(|c| c.cost_usd).sum()
    }

    /// Cases that went over their own budget.
    pub fn over_budget(&self) -> usize {
        self.cases.iter().filter(|c| c.over_budget).count()
    }

    /// Cases whose review failed outright.
    pub fn errored(&self) -> usize {
        self.cases.iter().filter(|c| c.error.is_some()).count()
    }
}
