//! Several reviewers on one piece of evidence, folded into one review.
//!
//! Always compiled. Nothing here calls a model: the council decides *who* runs
//! and what becomes of their findings, and the lanes still do the calling.
//!
//! # What a council is for
//!
//! Not a second opinion. `src/falsify` is already that, and its README explains
//! at length why asking a second model "are these correct?" deletes the best
//! half of a review. A council is the opposite direction: more reviewers so
//! that *more is found*, with agreement used only to rank what comes back.
//!
//! The diversity that pays is diversity of **subject**, not of vendor. The
//! repository already learned this the expensive way — `lanes::fanout` splits a
//! lane into one conversation per file and `ISOLATION_CLAUSE` exists because,
//! without it, every one of N reviewers on overlapping evidence reports the
//! same cross-file problem. Two reviewers reading the same file for different
//! failure classes are additive; two reading it for the same thing are a
//! duplicate with a bill attached.
//!
//! # Off by default, and a no-op at one agent
//!
//! `council.enabled = false` ships in `defaults.toml`, and `[council]` is not
//! overridable by a reviewed repository — every key in it spends the operator's
//! money, which is the same line `src/config/remote` draws around `[models]`.
//!
//! With one agent configured, [`merge::merge`] returns its input untouched and
//! a persona-less agent builds the lane's own prompt byte for byte. That is
//! deliberate: it makes the wiring provable before the second agent is what is
//! being judged.

pub mod agree;
pub mod merge;
pub mod persona;

pub use crate::council::agree::corroborates;
pub use crate::council::merge::merge;

use crate::config::types::{Config, LaneId, Severity};

/// One reviewer, resolved from configuration.
#[derive(Debug, Clone, Copy)]
pub struct Reviewer<'a> {
    /// The agent's id, for the check-run summary and the cost line.
    pub id: &'a str,
    /// The model id it calls, already resolved from tier to id.
    pub model: &'a str,
    /// The persona text appended to the lane instructions. Empty is the lane's
    /// own prompt, unchanged.
    pub persona: &'static str,
    /// The severity this reviewer's findings are clamped to, if any.
    ///
    /// Set from the persona rather than from configuration: it is a property of
    /// the subject being reviewed, not an operator preference, and a cap an
    /// operator could raise is not a cap. See [`persona::ceiling`].
    pub ceiling: Option<Severity>,
}

impl Reviewer<'_> {
    /// Apply this reviewer's ceiling to what it reported.
    ///
    /// A no-op for every reviewer that has none, which is all of them but
    /// `style`. Applied to the model's answer rather than requested in the
    /// prompt, because a prompt is a request and this is a guarantee.
    pub fn clamp(&self, findings: &mut [crate::findings::types::Finding]) {
        let Some(ceiling) = self.ceiling else {
            return;
        };
        for finding in findings {
            finding.severity = finding.severity.min(ceiling);
        }
    }
}

/// Who reviews `lane`, in configuration order.
///
/// Always at least one. A disabled council, or one whose agents all sit out
/// this lane, yields the single default reviewer — the lane's own model and no
/// persona — so every caller has one code path rather than a council branch and
/// a legacy branch that drift apart.
pub fn reviewers<'a>(config: &'a Config, lane: LaneId) -> Vec<Reviewer<'a>> {
    let solo = || {
        vec![Reviewer {
            id: "reviewer",
            model: config.model_for(lane),
            persona: persona::NONE,
        }]
    };

    if !config.council.enabled {
        return solo();
    }

    let agents: Vec<Reviewer<'a>> = config
        .council
        .agents
        .iter()
        .filter(|agent| agent.lanes.is_empty() || agent.lanes.contains(&lane))
        .map(|agent| Reviewer {
            id: &agent.id,
            model: config.model_for_agent(agent, lane),
            // Validated at load, so an unknown name cannot reach here. Falling
            // back to no persona rather than panicking keeps a configuration
            // mistake a weaker review instead of an outage.
            persona: agent
                .persona
                .as_deref()
                .and_then(persona::lookup)
                .unwrap_or(persona::NONE),
        })
        .collect();

    if agents.is_empty() { solo() } else { agents }
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
