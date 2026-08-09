//! Classification to GitHub's native issue type, deterministically.
//!
//! The model classifies; this module decides, and the rule it applies fits in
//! one sentence: **set the issue type whose name equals the classification
//! word, case-insensitively, and set nothing otherwise.** There is no fuzzy
//! match, no nearest neighbour and no default, because the type is a single
//! field — a wrong guess overwrites, rather than adds to, what is there.
//!
//! The available names are read from the forge rather than hard-coded. "Bug",
//! "Feature" and "Task" are only GitHub's defaults; an organisation may rename
//! them, add its own, or define none at all, and the last of those must triage
//! exactly as it does today.

use crate::issues::types::IssueKind;

/// What deterministic code decided about one issue's native type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Write this type name, exactly as the forge spells it.
    Set(String),
    /// Write nothing, and say why. For the log.
    Skip(&'static str),
}

/// Decide the native issue type for one issue.
///
/// `current` is the type the issue already carries, `available` the type names
/// the owner defines, in the order the forge returned them. Every refusal is a
/// reason rather than an error: an issue that keeps the type it has, or gets
/// none, is a triage that did its labelling job and left one field alone.
pub fn plan(
    kind: Option<IssueKind>,
    current: Option<&str>,
    available: &[String],
    enabled: bool,
) -> Decision {
    if !enabled {
        return Decision::Skip("issues.apply_issue_type is off");
    }
    // Checked before the classification is even read: an issue somebody has
    // already typed is not ours, whatever the model thinks it is.
    if current.is_some() {
        return Decision::Skip("the issue already carries an issue type");
    }
    if available.is_empty() {
        return Decision::Skip("this owner defines no issue types");
    }
    let Some(kind) = kind else {
        return Decision::Skip("the model gave no classification");
    };

    match available
        .iter()
        .find(|name| name.eq_ignore_ascii_case(kind.as_str()))
    {
        // The forge's spelling, not ours: the PATCH names a type that exists.
        Some(name) => Decision::Set(name.clone()),
        None => Decision::Skip("no issue type matches the classification"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> Vec<String> {
        vec!["Task".into(), "Bug".into(), "Feature".into()]
    }

    #[test]
    fn a_classification_matching_a_defined_type_is_set() {
        assert_eq!(
            plan(Some(IssueKind::Bug), None, &available(), true),
            Decision::Set("Bug".into())
        );
    }

    #[test]
    fn the_match_is_case_insensitive_but_the_forges_spelling_is_written() {
        // The name goes back to GitHub as the name GitHub gave us: a repository
        // that spells its type "bug" gets "bug" written, not "Bug".
        assert_eq!(
            plan(
                Some(IssueKind::Feature),
                None,
                &["bug".to_string(), "feature".to_string()],
                true
            ),
            Decision::Set("feature".into())
        );
    }

    #[test]
    fn a_type_a_human_already_set_is_never_overwritten() {
        // The rule labels follow, and it matters more here: the type is one
        // field, so writing it destroys the human's choice rather than joining
        // it.
        assert_eq!(
            plan(Some(IssueKind::Bug), Some("Feature"), &available(), true),
            Decision::Skip("the issue already carries an issue type")
        );
    }

    #[test]
    fn an_owner_that_defines_no_issue_types_is_skipped_rather_than_failed() {
        assert_eq!(
            plan(Some(IssueKind::Bug), None, &[], true),
            Decision::Skip("this owner defines no issue types")
        );
    }

    #[test]
    fn a_classification_no_defined_type_matches_is_skipped_rather_than_guessed() {
        // The whole point of a deterministic mapping: an organisation whose
        // types are "Defect" and "Epic" gets nothing, not the closest one.
        assert_eq!(
            plan(
                Some(IssueKind::Bug),
                None,
                &["Defect".to_string(), "Epic".to_string()],
                true
            ),
            Decision::Skip("no issue type matches the classification")
        );
    }

    #[test]
    fn a_model_that_committed_to_no_classification_sets_no_type() {
        assert_eq!(
            plan(None, None, &available(), true),
            Decision::Skip("the model gave no classification")
        );
    }

    #[test]
    fn the_policy_being_off_refuses_before_anything_else() {
        assert_eq!(
            plan(Some(IssueKind::Bug), None, &available(), false),
            Decision::Skip("issues.apply_issue_type is off")
        );
    }
}
