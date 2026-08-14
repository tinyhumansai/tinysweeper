//! The deterministic auto-merge policy.
//!
//! Pure: it takes a snapshot of observable state and returns a [`Decision`].
//! No I/O, no clock, no model. That is what makes every refusal testable
//! offline and what keeps the security boundary in `AGENTS.md` true — a model
//! verdict is advisory, and nothing advisory reaches this function.
//!
//! The order of the checks below is load-bearing in one respect only: the
//! cheapest and most decisive refusals come first, so the reason an operator
//! is shown is the most useful one. A draft with a failing check reports the
//! draft, because that is the thing to fix first.

use crate::automerge::complexity::measure;
use crate::automerge::paths::{glob_set, logins_match};
use crate::automerge::types::{Decision, Refusal};
use crate::config::types::AutoMerge;
use crate::forge::types::{ChangedFile, CheckStatus, PullRequest, ReviewVerdict};

/// Everything the policy reads, gathered at one instant.
///
/// Taken as one value rather than four arguments because the instant matters:
/// checks read against one head SHA and files read against another describe no
/// pull request that ever existed.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The pull request itself.
    pub pull_request: PullRequest,
    /// The files it changes.
    pub files: Vec<ChangedFile>,
    /// The check runs reported on its head SHA.
    pub checks: Vec<CheckStatus>,
    /// Every review left on it, oldest first, dismissed ones already dropped.
    pub reviews: Vec<ReviewVerdict>,
}

/// Proof that the policy passed, for one specific pull request.
///
/// [`crate::ports::forge::ForgeWrite::merge`] takes one of these, so a merge
/// without a passing gate does not compile. That is the difference this type
/// exists to make: before it, the impossibility was "the only call site sits
/// behind two evaluations", which is discipline. `AGENTS.md` gives the reason
/// `ForgeRead` and `ForgeWrite` are separate traits — an invariant the compiler
/// enforces survives a contributor who has not read `AGENTS.md` — and this is
/// the same argument one level down, on the one module that can mutate the
/// default branch with nobody watching.
///
/// ## Why it lives here and not in `types.rs`
///
/// A private field restricts construction to the *defining module and its
/// descendants*. Declared in `types.rs` with a `pub(super)` field — the shape
/// `sentry::types::Scrubbed` uses — every module under `automerge` could mint
/// one. Declared here, only `policy.rs` can, and [`evaluate`] is the only
/// function in it that returns one. The tighter placement costs nothing, and
/// the whole value of a witness is how small the set of things that can forge
/// it is.
///
/// ## Why it carries the number
///
/// So a witness cannot be moved between pull requests. `merge` reads the
/// number *from the token* rather than taking it as a separate argument, which
/// makes "merge the pull request that was approved" the only expressible
/// operation — approving #1 and merging #2 stops being a thing a caller can
/// write. It also removes a parameter rather than adding one.
///
/// It is deliberately **not** a staleness guarantee. A witness says the policy
/// passed against some snapshot, not that the snapshot is still true; that is
/// what `merge_snapshot`'s head-SHA check and second evaluation are for, and
/// they are unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeApproved {
    /// The pull request the policy passed for. Private: this is the field that
    /// makes the type unforgeable outside this module.
    number: u64,
}

impl MergeApproved {
    /// The pull request this approval is for.
    pub fn number(&self) -> u64 {
        self.number
    }
}

/// Decide whether `snapshot` may be merged.
///
/// The `Allow` arm is the only place a [`MergeApproved`] is ever minted.
pub fn evaluate(config: &AutoMerge, snapshot: &Snapshot) -> Decision {
    match refuse(config, snapshot) {
        Some(refusal) => Decision::Refuse(refusal),
        None => Decision::Allow(MergeApproved {
            number: snapshot.pull_request.number,
        }),
    }
}

/// The first reason to stop, if there is one.
fn refuse(config: &AutoMerge, snapshot: &Snapshot) -> Option<Refusal> {
    if !config.enabled {
        return Some(Refusal::Disabled);
    }

    let pull_request = &snapshot.pull_request;

    if pull_request.draft {
        return Some(Refusal::Draft);
    }

    if let Some(label) = carries(&pull_request.labels, &config.block_labels) {
        return Some(Refusal::BlockedLabel(label.clone()));
    }

    // An empty allow list means "no label is required"; a non-empty one means
    // the pull request has to have been opted in by hand.
    if !config.allow_labels.is_empty()
        && carries(&pull_request.labels, &config.allow_labels).is_none()
    {
        return Some(Refusal::MissingAllowLabel);
    }

    // `None` is GitHub still computing the merge state. Unknown is not yes.
    if pull_request.mergeable != Some(true) {
        return Some(Refusal::NotMergeable);
    }

    if let Some(refusal) = review_refusal(config, &snapshot.reviews) {
        return Some(refusal);
    }

    if let Some(refusal) = check_refusal(config, &snapshot.checks) {
        return Some(refusal);
    }

    // Nothing to judge means nothing measured, and nothing measured means no
    // gate at all.
    if snapshot.files.is_empty() {
        return Some(Refusal::NoChangedFiles);
    }

    // The file cap applies to every pull request, dependency bumps included:
    // it is a plain bound on how much lands at once, and the exemption below
    // is about *which* paths are acceptable, not how many.
    if snapshot.files.len() > config.max_files {
        return Some(Refusal::TooManyFiles {
            files: snapshot.files.len(),
            max: config.max_files,
        });
    }

    // A verified dependency bump is exempt from the sensitive-path and
    // complexity refusals. It has to be: manifests and lockfiles are sensitive
    // by definition and a lockfile churns thousands of lines, so without this
    // no bump would ever qualify. The exemption is narrow — an exact bot login
    // that GitHub itself calls a bot, and every changed path a manifest — and
    // it waives nothing else. Checks, reviews, labels and the live head SHA
    // all still apply.
    match dependency_bump(config, snapshot) {
        Ok(true) => return None,
        Ok(false) => {}
        Err(refusal) => return Some(refusal),
    }

    if let Some(refusal) = path_refusal(config, &snapshot.files) {
        return Some(refusal);
    }

    complexity_refusal(config, &snapshot.files)
}

/// A standing objection, or too few human approvals.
fn review_refusal(config: &AutoMerge, reviews: &[ReviewVerdict]) -> Option<Refusal> {
    // Anything that objects blocks, human or bot. The question is whether a
    // concern stands unaddressed, not who raised it.
    if let Some(blocking) = reviews.iter().find(|review| review.is_blocking()) {
        return Some(Refusal::ChangesRequested {
            reviewer: blocking.reviewer.clone(),
        });
    }

    // Approvals are counted from the review history rather than read off
    // `PullRequest::approvals`, because only here is "human" knowable — a
    // bot approving its own policy is not a second opinion.
    let approvals = reviews
        .iter()
        .filter(|review| review.is_human_approval())
        .count() as u32;
    if approvals < config.require_approvals {
        return Some(Refusal::NotEnoughApprovals {
            have: approvals,
            want: config.require_approvals,
        });
    }

    None
}

/// Nothing red, nothing pending, and every required check actually reported.
///
/// The two loops ask deliberately different questions, and the difference is
/// where this used to be wrong. The sweep asks "is anything actively broken?"
/// — a repository's own CI is not listed in `require_checks` for tinysweeper
/// to respect it. The required-check loop asks "did the checks we named
/// produce evidence?", which is a stronger question and is asked only of the
/// names in the config.
///
/// `Neutral` and `Skipped` sit between the two. Reading them as failures, as
/// this did, makes auto-merge unreachable rather than conservative: a
/// `Docker (publish)` job gated behind `if: github.event_name == 'push'`
/// concludes `skipped` on *every* pull request, so "not green blocks" means
/// nothing in such a repository ever merges. That is not a safe default, it is
/// a broken one — and a broken gate teaches operators to widen it.
fn check_refusal(config: &AutoMerge, checks: &[CheckStatus]) -> Option<Refusal> {
    for check in checks {
        if check.is_pending() {
            return Some(Refusal::CheckPending {
                name: check.name.clone(),
            });
        }
        if check.is_failing() {
            return Some(Refusal::CheckFailing {
                name: check.name.clone(),
            });
        }
    }

    // The named ones must have reported. A check that never ran is not a check
    // that passed — this is the case that would otherwise let a deleted or
    // renamed workflow silently retire the whole gate.
    for required in &config.require_checks {
        let Some(check) = checks.iter().find(|check| &check.name == required) else {
            return Some(Refusal::RequiredCheckMissing {
                name: required.clone(),
            });
        };

        // `Success` is a pass. `Neutral` is too, and only here: a lane
        // concluding `Neutral` ran and reported that it had nothing to say —
        // the `commits` lane on a range with no secrets in it — which is a
        // verdict, not an absence. `Skipped` is neither, and is refused with a
        // reason of its own so the operator is not left reading
        // "has not reported" about a check that plainly appears in the list.
        if !check.is_green() && !check.is_inapplicable() {
            return Some(Refusal::RequiredCheckInconclusive {
                name: required.clone(),
            });
        }
    }

    None
}

/// The first sensitive path touched, if any.
fn path_refusal(config: &AutoMerge, files: &[ChangedFile]) -> Option<Refusal> {
    let sensitive = match glob_set(&config.sensitive_paths) {
        Ok(set) => set,
        Err(err) => return Some(Refusal::UnreadablePolicy(err)),
    };

    files
        .iter()
        .flat_map(|file| std::iter::once(&file.path).chain(file.previous_path.iter()))
        .find(|path| sensitive.is_match(path.as_str()))
        .map(|path| Refusal::SensitivePath { path: path.clone() })
}

/// The first complexity signal over its threshold, if any.
fn complexity_refusal(config: &AutoMerge, files: &[ChangedFile]) -> Option<Refusal> {
    let measured = match measure(files) {
        Ok(measured) => measured,
        Err(path) => return Some(Refusal::Unmeasurable { path }),
    };

    let signals = [
        (
            "changed_lines",
            measured.changed_lines,
            config.max_changed_lines,
        ),
        ("hunks", measured.hunks as u64, config.max_hunks as u64),
        (
            "directories",
            measured.directories as u64,
            config.max_directories as u64,
        ),
    ];

    signals
        .into_iter()
        .find(|(_, value, max)| value > max)
        .map(|(signal, value, max)| Refusal::TooComplex { signal, value, max })
}

/// Whether this is a dependency bump the exemption covers.
///
/// Three conditions, all required: the exemption is on, the author is one of
/// the configured logins *and* GitHub reports it as a bot, and every changed
/// path is a manifest or lockfile. A login alone would not do — anyone can
/// register `dependabot-something` — and the bot flag alone would not either,
/// since any app can open a pull request.
fn dependency_bump(config: &AutoMerge, snapshot: &Snapshot) -> Result<bool, Refusal> {
    if !config.allow_dependency_bumps || !snapshot.pull_request.author_is_bot {
        return Ok(false);
    }

    let author = &snapshot.pull_request.author;
    if !config
        .dependency_bots
        .iter()
        .any(|configured| logins_match(configured, author))
    {
        return Ok(false);
    }

    let manifests = glob_set(&config.dependency_paths).map_err(Refusal::UnreadablePolicy)?;
    Ok(snapshot.files.iter().all(|file| {
        manifests.is_match(file.path.as_str())
            && file
                .previous_path
                .as_ref()
                .is_none_or(|previous| manifests.is_match(previous.as_str()))
    }))
}
