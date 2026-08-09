//! The label planner: which labels get *added*, and never which get removed.
//!
//! This is the reusable seam. It takes the labels an item already carries, the
//! labels something suggested, and the `[issues]` policy — nothing about issues
//! specifically — so pull request triage can call it with a pull request's
//! labels and get the same guarantees. Keep it that way: a parameter of type
//! `Issue` here is what would force the next caller to build a fake one.

use crate::config::types::Issues;
use crate::issues::types::{IssueSeverity, Priority};

/// What labelling decided for one item.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LabelPlan {
    /// Labels to add, in order, already filtered and capped.
    pub add: Vec<String>,
    /// Labels to remove, because a newer label supersedes them.
    ///
    /// Only ever labels from **this bot's own facets** — see `supersede`.
    pub remove: Vec<String>,
    /// Suggestions that were refused, with the reason. For the log.
    pub declined: Vec<(String, &'static str)>,
}

/// Plan the labels to add to an item.
///
/// Refusals are recorded rather than dropped: a suggestion that vanished
/// silently is a suggestion nobody can debug, and "why did it not label this"
/// is the first question a maintainer asks.
pub fn plan(existing: &[String], suggested: &[String], policy: &Issues) -> LabelPlan {
    let mut plan = LabelPlan::default();

    let refuse_all = |plan: &mut LabelPlan, reason: &'static str| {
        for label in suggested {
            plan.declined.push((label.clone(), reason));
        }
    };

    if !policy.apply_labels {
        refuse_all(&mut plan, "issues.apply_labels is off");
        return plan;
    }
    if policy
        .block_labels
        .iter()
        .any(|blocked| existing.iter().any(|label| same(label, blocked)))
    {
        refuse_all(&mut plan, "the item carries a blocked label");
        return plan;
    }

    let mut chose_priority = false;
    let mut chose_severity = false;

    for label in suggested {
        if let Some(reason) = refusal(label, existing, policy, chose_priority, chose_severity) {
            plan.declined.push((label.clone(), reason));
            continue;
        }
        if plan.add.len() >= policy.max_labels {
            plan.declined
                .push((label.clone(), "issues.max_labels reached"));
            continue;
        }
        // A facet is exclusive by nature: `severity: high` beside
        // `severity: low` is not two opinions, it is a contradiction, and a
        // maintainer scanning the list cannot tell which one is current. So
        // adding a label in a facet retires the others in that same facet.
        //
        // Bounded to the two facets this bot owns and to labels in its own
        // vocabulary. Anything a human applied outside `priority:`/`severity:`
        // is untouched, and an operator who wants to pin one of these by hand
        // uses `issues.block_labels`, which refuses the whole plan.
        plan.remove.extend(supersede(label, existing));

        chose_priority |= is_priority(label);
        chose_severity |= is_severity(label);
        plan.add.push(label.clone());
    }

    plan
}

/// Existing labels in the same facet as `label`, which it replaces.
///
/// Returns nothing for a label outside the owned facets, so this can be called
/// unconditionally.
fn supersede(label: &str, existing: &[String]) -> Vec<String> {
    let facet: fn(&str) -> bool = if is_priority(label) {
        is_priority
    } else if is_severity(label) {
        is_severity
    } else {
        return Vec::new();
    };

    existing
        .iter()
        .filter(|have| facet(have) && !same(have, label))
        .cloned()
        .collect()
}

/// Why one suggestion is refused, or `None` if it survives.
///
/// Split out so the order of the checks is readable: the cheapest and most
/// specific reason wins, because that is the one worth logging.
fn refusal(
    label: &str,
    existing: &[String],
    policy: &Issues,
    chose_priority: bool,
    chose_severity: bool,
) -> Option<&'static str> {
    if existing.iter().any(|have| same(have, label)) {
        return Some("already applied");
    }
    if policy.allow_labels.is_empty() {
        if !vocabulary().iter().any(|known| same(known, label)) {
            return Some("not a label triage may apply");
        }
    } else if !policy.allow_labels.iter().any(|known| same(known, label)) {
        return Some("not in issues.allow_labels");
    }
    if chose_priority && is_priority(label) {
        return Some("a priority label was already chosen");
    }
    if chose_severity && is_severity(label) {
        return Some("a severity label was already chosen");
    }
    None
}

/// Label comparison. GitHub label names are case-insensitive in practice, and
/// treating `Bug` and `bug` as different is how an item ends up with both.
fn same(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn is_priority(label: &str) -> bool {
    Priority::labels().iter().any(|known| same(known, label))
}

fn is_severity(label: &str) -> bool {
    IssueSeverity::labels()
        .iter()
        .any(|known| same(known, label))
}

/// The built-in vocabulary: what triage may apply when `allow_labels` is empty.
pub fn vocabulary() -> Vec<&'static str> {
    Priority::labels()
        .into_iter()
        .chain(IssueSeverity::labels())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Issues {
        Issues {
            enabled: true,
            apply_labels: true,
            max_labels: 3,
            block_labels: vec!["tinysweeper:manual-only".into()],
            ..Issues::default()
        }
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_priority_and_a_severity_are_added() {
        let plan = plan(&[], &owned(&["priority: p1", "severity: high"]), &policy());
        assert_eq!(plan.add, owned(&["priority: p1", "severity: high"]));
        assert!(plan.declined.is_empty());
    }

    #[test]
    fn a_label_already_present_is_not_added_again() {
        let plan = plan(
            &owned(&["priority: p1"]),
            &owned(&["priority: p1", "severity: high"]),
            &policy(),
        );
        assert_eq!(plan.add, owned(&["severity: high"]));
        assert_eq!(
            plan.declined,
            vec![("priority: p1".to_string(), "already applied")]
        );
    }

    #[test]
    fn a_label_outside_the_vocabulary_is_refused() {
        let plan = plan(&[], &owned(&["wontfix", "severity: low"]), &policy());
        assert_eq!(plan.add, owned(&["severity: low"]));
        assert_eq!(
            plan.declined,
            vec![("wontfix".to_string(), "not a label triage may apply")]
        );
    }

    #[test]
    fn allow_labels_replaces_the_built_in_vocabulary() {
        let policy = Issues {
            allow_labels: owned(&["needs-triage"]),
            ..policy()
        };
        let plan = plan(&[], &owned(&["needs-triage", "priority: p0"]), &policy);
        assert_eq!(plan.add, owned(&["needs-triage"]));
        assert_eq!(
            plan.declined,
            vec![("priority: p0".to_string(), "not in issues.allow_labels")]
        );
    }

    #[test]
    fn a_blocked_label_on_the_item_stops_all_labelling() {
        let plan = plan(
            &owned(&["tinysweeper:manual-only"]),
            &owned(&["priority: p0"]),
            &policy(),
        );
        assert!(plan.add.is_empty());
        assert_eq!(
            plan.declined,
            vec![(
                "priority: p0".to_string(),
                "the item carries a blocked label"
            )]
        );
    }

    #[test]
    fn labelling_turned_off_adds_nothing() {
        let policy = Issues {
            apply_labels: false,
            ..policy()
        };
        let plan = plan(&[], &owned(&["priority: p0"]), &policy);
        assert!(plan.add.is_empty());
        assert_eq!(
            plan.declined,
            vec![("priority: p0".to_string(), "issues.apply_labels is off")]
        );
    }

    #[test]
    fn only_the_first_priority_and_the_first_severity_survive() {
        // A model asked for two priorities. Applying both would leave the issue
        // in two buckets at once, which is worse than picking one.
        let plan = plan(
            &[],
            &owned(&["priority: p0", "priority: p2", "severity: low"]),
            &policy(),
        );
        assert_eq!(plan.add, owned(&["priority: p0", "severity: low"]));
        assert_eq!(
            plan.declined,
            vec![(
                "priority: p2".to_string(),
                "a priority label was already chosen"
            )]
        );
    }

    #[test]
    fn max_labels_caps_what_is_applied() {
        let policy = Issues {
            max_labels: 1,
            ..policy()
        };
        let plan = plan(&[], &owned(&["priority: p1", "severity: high"]), &policy);
        assert_eq!(plan.add, owned(&["priority: p1"]));
        assert_eq!(
            plan.declined,
            vec![("severity: high".to_string(), "issues.max_labels reached")]
        );
    }

    #[test]
    fn the_plan_has_no_way_to_express_removing_a_label() {
        // Not a behaviour test — a shape test. `LabelPlan` is add-only, so
        // "never remove a label a human applied" cannot be violated by a caller
        // who forgets the rule.
        let plan = plan(&owned(&["human-applied"]), &[], &policy());
        assert!(plan.add.is_empty());
        assert!(plan.declined.is_empty());
    }

    #[test]
    fn a_new_severity_supersedes_the_stale_one() {
        // The defect this exists to stop: a pull request whose finding was
        // fixed keeps `severity: high` beside the new `severity: low`. That is
        // not two opinions, it is a contradiction, and it says the opposite of
        // the truth right next to the truth.
        let plan = plan(
            &["severity: high".into()],
            &["severity: low".into()],
            &policy(),
        );

        assert_eq!(plan.add, ["severity: low"]);
        assert_eq!(plan.remove, ["severity: high"]);
    }

    #[test]
    fn a_label_outside_the_owned_facets_is_never_superseded() {
        // The bound on the rule. `needs-design` is a human's, in no facet
        // triage owns, and not ours to retire.
        let plan = plan(
            &["needs-design".into(), "severity: high".into()],
            &["severity: low".into()],
            &policy(),
        );

        assert_eq!(plan.remove, ["severity: high"]);
    }

    #[test]
    fn re_applying_the_same_label_removes_nothing() {
        // A no-op run must be a no-op. Removing and re-adding the same label
        // would churn the timeline on every push for no change at all.
        let plan = plan(
            &["severity: low".into()],
            &["severity: low".into()],
            &policy(),
        );

        assert!(plan.add.is_empty(), "{plan:?}");
        assert!(plan.remove.is_empty(), "{plan:?}");
    }
}
