//! Recognising a mechanical substitution, so it is verified rather than read.
//!
//! opencompany#2313 renamed `openhuman_core::openhuman` to `openhuman_core`
//! across 51 files and changed the logic of one. The critique lane sent all
//! 58 to a model, one conversation each, at fifteen thousand tokens a piece:
//! $0.20 of the $0.23 the review cost, for fifty-one answers of "nothing
//! here" and two confident hallucinations about a crate layout no file
//! showed. The file that carried the two real bugs got the same fifteen
//! thousand tokens as `chargebee.rs`.
//!
//! A rename is not something to *read*; it is something to *check*. If every
//! removed line in a file, with one literal substitution applied, is exactly
//! the added line that replaced it, the file is the rename and nothing else —
//! a deterministic fact, provable line for line, needing no model. So those
//! files are verified here and named in the summary as verified, and the
//! budget goes to the residue.
//!
//! The check is exact, which makes its only failure mode a false negative: a
//! file with one line that is *not* the substitution goes to the model like
//! any other, and gets read there with the rename already explained to it.
//! A file is never skipped on a guess.

use std::collections::BTreeMap;

use crate::evidence::diff::{FileDiff, LineKind};

/// A substitution the pull request applies mechanically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    /// The literal every removed line contains.
    pub from: String,
    /// What it became. May be empty.
    pub to: String,
    /// Paths whose whole diff is this substitution, in diff order.
    pub verified: Vec<String>,
}

/// The fewest files a substitution has to explain in full to count.
///
/// Below this a rename is not what the pull request is about, and reading two
/// files costs less than being wrong about the pattern.
pub const MIN_FILES: usize = 3;

/// Find the one substitution that fully explains the most files, if any.
///
/// Candidates are taken from every removed/added pair's differing span; the
/// one that verifies the most files wins, and only if it verifies at least
/// [`MIN_FILES`]. A file verified by the winner is returned in `verified`
/// and should not be sent to a model; every other file should.
pub fn detect(diffs: &[&FileDiff]) -> Option<Substitution> {
    let mut candidates: BTreeMap<(String, String), usize> = BTreeMap::new();
    for diff in diffs {
        for (removed, added) in pairs(diff) {
            if let Some(candidate) = differing_span(&removed, &added) {
                *candidates.entry(candidate).or_default() += 1;
            }
        }
    }

    let mut best: Option<Substitution> = None;
    for (from, to) in candidates.into_keys() {
        if from.is_empty() || from.len() < 3 {
            continue;
        }
        let verified: Vec<String> = diffs
            .iter()
            .filter(|diff| explains(diff, &from, &to))
            .map(|diff| diff.path.clone())
            .collect();
        if verified.len() >= MIN_FILES
            && best.as_ref().is_none_or(|b| verified.len() > b.verified.len())
        {
            best = Some(Substitution { from, to, verified });
        }
    }
    best
}

/// Whether `diff` is entirely `from`→`to`: every removed line contains
/// `from`, every removed line rewritten is exactly the added line in the same
/// position, and nothing is added or removed beyond those pairs.
pub fn explains(diff: &FileDiff, from: &str, to: &str) -> bool {
    let mut any = false;
    for hunk in &diff.hunks {
        let removed: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .map(|l| l.text.as_str())
            .collect();
        let added: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .map(|l| l.text.as_str())
            .collect();
        if removed.len() != added.len() {
            return false;
        }
        for (r, a) in removed.iter().zip(&added) {
            if !r.contains(from) || r.replace(from, to) != *a {
                return false;
            }
            any = true;
        }
    }
    any
}

/// Removed/added line pairs, matched positionally within each hunk.
fn pairs(diff: &FileDiff) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for hunk in &diff.hunks {
        let removed: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .map(|l| l.text.as_str())
            .collect();
        let added: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .map(|l| l.text.as_str())
            .collect();
        if removed.len() == added.len() {
            for (r, a) in removed.iter().zip(&added) {
                out.push(((*r).to_string(), (*a).to_string()));
            }
        }
    }
    out
}

/// The span where `removed` and `added` differ, as `(from, to)`.
///
/// Common prefix and suffix are stripped; what is left of each is the
/// substitution. `None` when the lines are identical.
fn differing_span(removed: &str, added: &str) -> Option<(String, String)> {
    if removed == added {
        return None;
    }
    let r: Vec<char> = removed.chars().collect();
    let a: Vec<char> = added.chars().collect();
    let prefix = r.iter().zip(&a).take_while(|(x, y)| x == y).count();
    let max_suffix = r.len().min(a.len()) - prefix;
    let suffix = r
        .iter()
        .rev()
        .zip(a.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let from: String = r[prefix..r.len() - suffix].iter().collect();
    let to: String = a[prefix..a.len() - suffix].iter().collect();
    Some((from, to))
}

/// The sentence a lane summary carries for verified files.
pub fn note(sub: &Substitution) -> String {
    format!(
        "{} file(s) are the mechanical rename `{}` → `{}`, verified line for line and not \
         sent to a model.",
        sub.verified.len(),
        sub.from,
        if sub.to.is_empty() { "" } else { &sub.to }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::diff::parse_unified;

    fn diff(path: &str, pairs: &[(&str, &str)], extra_added: &[&str]) -> FileDiff {
        let mut text = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,{} +1,{} @@\n", pairs.len(), pairs.len() + extra_added.len());
        for (r, _) in pairs {
            text.push_str(&format!("-{r}\n"));
        }
        for (_, a) in pairs {
            text.push_str(&format!("+{a}\n"));
        }
        for a in extra_added {
            text.push_str(&format!("+{a}\n"));
        }
        parse_unified(&text).into_iter().next().unwrap()
    }

    #[test]
    fn a_rename_across_files_is_detected_and_the_residue_is_not_claimed() {
        let a = diff(
            "src/a.rs",
            &[("use openhuman_core::openhuman as oh;", "use openhuman_core as oh;")],
            &[],
        );
        let b = diff(
            "src/b.rs",
            &[(
                "    openhuman_core::openhuman::tools::x()",
                "    openhuman_core::tools::x()",
            )],
            &[],
        );
        let c = diff(
            "src/c.rs",
            &[("openhuman_core::openhuman::A", "openhuman_core::A")],
            &[],
        );
        let residue = diff(
            "src/d.rs",
            &[("openhuman_core::openhuman::B", "openhuman_core::B")],
            &["let leak = 1;"],
        );
        let sub = detect(&[&a, &b, &c, &residue]).expect("detected");
        assert_eq!(sub.from, "::openhuman");
        assert_eq!(sub.to, "");
        assert_eq!(sub.verified, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert!(note(&sub).contains("3 file(s)"));
    }

    #[test]
    fn two_files_are_not_a_pattern() {
        let a = diff("a", &[("x::old", "x::new")], &[]);
        let b = diff("b", &[("y::old", "y::new")], &[]);
        assert_eq!(detect(&[&a, &b]), None);
    }

    #[test]
    fn a_line_that_is_not_the_substitution_disqualifies_the_file() {
        let a = diff("a", &[("foo::old()", "foo::new()")], &[]);
        let b = diff("b", &[("bar::old()", "bar::new()")], &[]);
        let c = diff("c", &[("baz::old()", "baz::new()"), ("if x < 1", "if x <= 1")], &[]);
        let d = diff("d", &[("qux::old()", "qux::new()")], &[]);
        let sub = detect(&[&a, &b, &c, &d]).unwrap();
        assert_eq!(sub.verified, vec!["a", "b", "d"]);
    }

    #[test]
    fn the_differing_span_strips_common_prefix_and_suffix() {
        assert_eq!(
            differing_span("use a::b as c;", "use a as c;"),
            Some(("::b".into(), "".into()))
        );
        assert_eq!(differing_span("same", "same"), None);
        assert_eq!(
            differing_span("x", "xy"),
            Some(("".into(), "y".into()))
        );
    }
}
