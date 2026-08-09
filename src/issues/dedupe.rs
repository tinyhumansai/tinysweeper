//! Finding candidate duplicates, deterministically.
//!
//! This is a token-overlap shortlist, not a semantic search. The repository
//! already has a hybrid MongoDB index over *code* (`src/index`, `src/retrieve`),
//! but nothing embeds issues, and half-wiring a vector path that silently
//! returns nothing when the index is cold would be worse than a similarity
//! score anyone can reproduce by hand. So: normalise, shingle, score, sort.
//!
//! The shortlist is also the anti-hallucination boundary. Only numbers that
//! appear here are shown to the model, and [`crate::issues::close::decide`]
//! refuses any claim naming a number that was not on this list — so the worst a
//! confused model can do is pick the wrong one of a handful of real issues.

use std::collections::BTreeSet;

use crate::forge::types::Issue;

/// Below this overlap two issues are not related enough to show the model.
///
/// Tuned to be permissive: this only decides what gets *shown*, and both the
/// model and the close gate filter further. Missing the real duplicate here is
/// unrecoverable, whereas an extra candidate costs a few tokens.
pub const MIN_SIMILARITY: f64 = 0.15;

/// One possible duplicate, with the score that got it here.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The other issue's number.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Jaccard overlap of the two issues' token sets, 0..=1.
    pub score: f64,
    /// Its age in days, so the model can see which one came first.
    pub age_days: u32,
}

/// Shortlist the issues in `others` that look like `subject`, best first.
pub fn shortlist(subject: &Issue, others: &[Issue], limit: usize) -> Vec<Candidate> {
    let mine = tokens(&subject.title, &subject.body);
    if mine.is_empty() {
        // An empty subject overlaps everything and nothing. Scoring it would
        // divide by an empty union, and a NaN sorts arbitrarily — which would
        // put a random issue at the top of a list used to justify a close.
        return Vec::new();
    }

    let mut scored: Vec<Candidate> = others
        .iter()
        .filter(|other| other.number != subject.number)
        .filter_map(|other| {
            let score = jaccard(&mine, &tokens(&other.title, &other.body));
            (score >= MIN_SIMILARITY).then(|| Candidate {
                number: other.number,
                title: other.title.clone(),
                score,
                age_days: other.age_days,
            })
        })
        .collect();

    // Score descending, then number ascending, so a tie resolves to the older
    // issue and the same inputs always produce the same shortlist.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.number.cmp(&b.number))
    });
    scored.truncate(limit);
    scored
}

/// Overlap of two token sets, 0..=1. Empty on either side is zero, never NaN.
fn jaccard(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let union = left.union(right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(right).count() as f64 / union as f64
}

/// The tokens an issue contributes to the comparison.
///
/// Title and body together, lowercased, split on anything that is not a letter
/// or digit, with one-character fragments dropped — they are punctuation noise
/// and they inflate every score equally.
pub fn tokens(title: &str, body: &str) -> BTreeSet<String> {
    [title, body]
        .iter()
        .flat_map(|text| text.split(|c: char| !c.is_alphanumeric()))
        .filter(|word| word.chars().count() > 1)
        .map(|word| word.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: u64, title: &str, body: &str) -> Issue {
        Issue {
            number,
            title: title.into(),
            body: body.into(),
            author: "reporter".into(),
            open: true,
            age_days: 10,
            ..Issue::default()
        }
    }

    #[test]
    fn tokens_are_lowercased_and_punctuation_is_dropped() {
        let got = tokens("Crash on Save!", "It crashes, badly.");
        assert!(got.contains("crash"));
        assert!(got.contains("crashes"));
        assert!(got.contains("badly"));
        assert!(!got.contains("Crash"));
    }

    #[test]
    fn single_character_fragments_are_not_tokens() {
        let got = tokens("a b crash", "");
        assert_eq!(got.into_iter().collect::<Vec<_>>(), vec!["crash"]);
    }

    #[test]
    fn the_nearest_issue_scores_highest() {
        let subject = issue(10, "Crash when saving a large file", "The editor crashes.");
        let others = vec![
            issue(1, "Crash when saving a large file", "The editor crashes."),
            issue(2, "Add a dark theme", "Please add a dark theme option."),
        ];

        let got = shortlist(&subject, &others, 5);
        assert_eq!(got.first().map(|c| c.number), Some(1));
        assert!(got.first().expect("a candidate").score > 0.9);
    }

    #[test]
    fn an_unrelated_issue_is_below_the_floor_and_omitted() {
        let subject = issue(10, "Crash when saving a large file", "The editor crashes.");
        let others = vec![issue(2, "Add a dark theme", "Please add a dark theme.")];
        assert!(shortlist(&subject, &others, 5).is_empty());
    }

    #[test]
    fn the_subject_is_never_its_own_duplicate() {
        let subject = issue(10, "Crash when saving", "The editor crashes.");
        let others = vec![subject.clone()];
        assert!(shortlist(&subject, &others, 5).is_empty());
    }

    #[test]
    fn the_shortlist_is_capped_and_ordered() {
        let subject = issue(10, "Crash when saving a large file", "The editor crashes.");
        let others = vec![
            issue(
                3,
                "Crash when saving a large file",
                "Editor crashes rarely.",
            ),
            issue(1, "Crash when saving a large file", "The editor crashes."),
            issue(2, "Crash when saving a file", "The editor crashes."),
        ];

        let got = shortlist(&subject, &others, 2);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].number, 1, "the closest match leads");
        assert!(got[0].score >= got[1].score);
    }

    #[test]
    fn an_empty_subject_matches_nothing() {
        // Division by an empty union would otherwise be a NaN score that sorts
        // unpredictably and could put an arbitrary issue at the top.
        let subject = issue(10, "", "");
        let others = vec![issue(1, "Crash when saving", "The editor crashes.")];
        assert!(shortlist(&subject, &others, 5).is_empty());
    }
}
