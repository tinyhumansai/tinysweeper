//! Whether two findings raise the same concern, however they are worded.
//!
//! Always compiled, and deliberately dumb: lowercase, split, drop stopwords,
//! strip a few suffixes, compare word sets. No model, no embeddings, no
//! network — the default build links no HTTP client and dedupe has to work
//! there, and a deterministic rule is one a maintainer can reason about when it
//! keeps or drops a comment.
//!
//! ## Why the fingerprint was not enough
//!
//! [`Finding::fingerprint`] hashes the model-authored `rule` and the quoted
//! snippet, and [`PriorReview`](crate::findings::prior::PriorReview) used to
//! fall back only to an *identical title* from the *same lane* within three
//! lines. In October 2026 that let one concern through again and again:
//!
//! - `openhuman#7129`: "Test the write-again-without-actor fallback" eight
//!   times, from three lanes, across a file and its test sibling.
//! - `openhuman#7079`: "Require visible elements before clicking" from the
//!   `security`, `tests` and `e2e` lanes, on lines 23, 24 and 25.
//! - `tinyagents#341`: one budget concern under the rules `resource-budget`,
//!   `budget-bound` and `unbounded-budget`.
//! - `tinyskills#24`: "Keep integration tests free of live network sockets"
//!   reworded to "Use a deterministic transport instead of loopback sockets",
//!   after a maintainer had already declined it.
//!
//! Those are the fixtures in `concern_test.rs`, and the thresholds below were
//! tuned on them and on every other comment on those pull requests.
//!
//! ## The rule
//!
//! Two findings are one concern when they are **in the same place** and
//! **say the same thing**, where "the same place" has two strengths:
//!
//! - *Nearby*: one file, anchors within [`NEAR_LINES`]. A modest similarity is
//!   enough, because position already carries most of the evidence.
//! - *Same file*: one file anywhere, or a file and its own test sibling
//!   (`memory.rs` / `memory_tests.rs`). Only near-identical wording counts,
//!   because two real defects in one file are ordinary.
//!
//! An identical title in an unrelated file is **not** a repeat. On
//! `openhuman#7127` "Avoid logging the full module error" was raised on two
//! different modules that both logged; merging those would delete a real site.
//!
//! The lane is not compared at all. The lane-scoped fallback this replaces is
//! exactly what let three lanes post one concern three times.
//!
//! A concern a maintainer already declined is held to the nearby bar anywhere
//! in the file: a reworded, moved version of a "no" is still a "no".

use std::collections::BTreeSet;

use crate::findings::types::Finding;

/// How far apart, in lines, two anchors in one file may sit and still be
/// "nearby".
///
/// A function's worth. The tree-sitter chunker could name the enclosing symbol
/// instead, but neither a posted comment nor a finding carries one, and
/// re-chunking every touched file on every review to recover it would buy
/// little: the same-file tier already recognises a concern that moved further
/// than this.
pub const NEAR_LINES: u64 = 30;

/// How similar two findings must be to count as one concern.
struct Bar {
    /// Title similarity, on its own.
    title: f64,
    /// Title-and-body similarity, on its own.
    text: f64,
    /// Either similarity, when the rule names are synonyms. `None` disables
    /// the rule as evidence.
    with_rule: Option<f64>,
}

/// Nearby findings. Tuned so that every repeat in the fixtures clears it and
/// no pair of distinct concerns within a function does: the closest distinct
/// pair, `tinyagents#341` lines 329 and 330, scores 0.00 on title and 0.18 on
/// text.
const NEARBY: Bar = Bar {
    title: 0.5,
    text: 0.3,
    with_rule: Some(0.25),
};

/// Anywhere in one file or its test sibling. Near-identical wording only;
/// rule names are too generic to stand in for position here.
const SAME_FILE: Bar = Bar {
    title: 0.75,
    text: 0.45,
    with_rule: None,
};

/// Title similarity at which two titles say the same thing for polarity
/// purposes. The lowest title bar used anywhere, so a contradiction is never
/// suppressed by a match the nearby tier would have accepted.
const SAME_WORDING: f64 = 0.5;

/// One finding, reduced to what concern identity compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concern {
    path: String,
    range: Option<(u64, u64)>,
    title: BTreeSet<String>,
    text: BTreeSet<String>,
    rule: BTreeSet<String>,
    /// Whether the title negates itself ("Do not allow X"). Kept apart from the
    /// word sets, where negation is a stopword, so that opposite guidance is
    /// never mistaken for a repeat.
    negated: bool,
}

/// How two concerns relate in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Nearby,
    SameFile,
    Unrelated,
}

impl Concern {
    /// A concern from its parts. `range` is the head-revision line span, when
    /// there is one.
    pub fn new(path: &str, range: Option<(u64, u64)>, title: &str, body: &str, rule: &str) -> Self {
        let title_words = tokens(title);
        let mut text = tokens(body);
        text.extend(title_words.iter().cloned());
        Self {
            path: path.to_string(),
            range,
            title: title_words,
            text,
            rule: rule_tokens(rule),
            negated: negates(title),
        }
    }

    /// The concern a finding raises.
    pub fn of(finding: &Finding) -> Self {
        Self::new(
            &finding.path,
            finding.range(),
            &finding.title,
            &finding.body,
            &finding.rule,
        )
    }

    /// Whether `self` repeats `other`.
    pub fn same_as(&self, other: &Self) -> bool {
        match self.placement(other) {
            Placement::Nearby => self.clears(other, &NEARBY),
            Placement::SameFile => self.clears(other, &SAME_FILE),
            Placement::Unrelated => false,
        }
    }

    /// Whether `self` rewords a concern a maintainer already declined.
    ///
    /// The nearby bar, applied anywhere in the file or its test sibling.
    pub fn same_as_declined(&self, declined: &Self) -> bool {
        match self.placement(declined) {
            Placement::Nearby | Placement::SameFile => self.clears(declined, &NEARBY),
            Placement::Unrelated => false,
        }
    }

    fn placement(&self, other: &Self) -> Placement {
        if self.path == other.path {
            match (self.range, other.range) {
                (Some(left), Some(right)) if gap(left, right) <= NEAR_LINES => Placement::Nearby,
                _ => Placement::SameFile,
            }
        } else if siblings(&self.path, &other.path) {
            Placement::SameFile
        } else {
            Placement::Unrelated
        }
    }

    fn clears(&self, other: &Self, bar: &Bar) -> bool {
        let title = title_similarity(&self.title, &other.title);
        // "Allow X" and "Do not allow X" share every content word and say the
        // opposite. Whatever else matches, that is changed guidance, not a
        // repeat, so it must survive.
        if self.negated != other.negated && title >= SAME_WORDING {
            return false;
        }
        let text = jaccard(&self.text, &other.text);
        if title >= bar.title || text >= bar.text {
            return true;
        }
        bar.with_rule.is_some_and(|floor| {
            !self.rule.is_disjoint(&other.rule) && (title >= floor || text >= floor)
        })
    }
}

/// Lines between two ranges; zero when they overlap.
fn gap((left_start, left_end): (u64, u64), (right_start, right_end): (u64, u64)) -> u64 {
    // Each difference is zero unless that side lies wholly before the other.
    right_start
        .saturating_sub(left_end)
        .max(left_start.saturating_sub(right_end))
}

/// Size of the intersection over size of the union; zero for two empty sets.
fn jaccard(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let union = left.union(right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(right).count() as f64 / union as f64
}

/// Title similarity: Jaccard, except that a title wholly contained in the
/// other counts as identical.
///
/// "Test the write-again-without-actor fallback" and the same sentence ending
/// "…the change promises" are one title with a clause added. Jaccard scores
/// them 0.71. Containment needs three words on the shorter side so that a
/// two-word title cannot claim every longer one that happens to include it.
fn title_similarity(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let shorter = left.len().min(right.len());
    if shorter >= 3 && (left.is_subset(right) || right.is_subset(left)) {
        return 1.0;
    }
    jaccard(left, right)
}

/// Whether two rule names are synonyms: they share a word that names the
/// problem rather than its category.
///
/// `resource-budget`, `budget-bound` and `unbounded-budget` share `budget`.
/// `unbounded-retry` and `budget-bound` share only `bound`, which every
/// limit-shaped rule carries, so they do not.
pub fn rules_agree(left: &str, right: &str) -> bool {
    !rule_tokens(left).is_disjoint(&rule_tokens(right))
}

/// The distinctive words of a rule name.
fn rule_tokens(rule: &str) -> BTreeSet<String> {
    words(rule)
        .map(|word| {
            // `unbounded` and `bounded` are one idea; `non` likewise.
            let bare = ["non", "un"]
                .iter()
                .find_map(|prefix| word.strip_prefix(prefix).filter(|rest| rest.len() >= 4))
                .unwrap_or(&word);
            stem(bare)
        })
        .filter(|word| {
            !GENERIC_RULE_WORDS
                .iter()
                .any(|generic| stem(generic) == *word)
        })
        .collect()
}

/// Rule words that name a category of problem rather than a problem.
const GENERIC_RULE_WORDS: &[&str] = &[
    "behavior",
    "behaviour",
    "bound",
    "bug",
    "case",
    "check",
    "code",
    "contract",
    "correctness",
    "coverage",
    "e2e",
    "edge",
    "error",
    "handling",
    "incorrect",
    "invalid",
    "issue",
    "limit",
    "logic",
    "missing",
    "possible",
    "potential",
    "quality",
    "regression",
    "rule",
    "risk",
    "safety",
    "security",
    "style",
    "test",
    "unchecked",
    "violation",
    "wrong",
];

/// The normalised words of `text`: lowercased, split on anything that is not
/// an ASCII letter or digit, stopwords and numbers dropped, suffixes stripped.
pub fn tokens(text: &str) -> BTreeSet<String> {
    words(text)
        .map(|word| stem(&word))
        .map(
            |word| match SYNONYMS.iter().find(|(from, _)| *from == word) {
                Some((_, to)) => (*to).to_string(),
                None => word,
            },
        )
        .collect()
}

/// Words a reviewer uses interchangeably for one request, after stemming.
///
/// Kept tiny on purpose: each entry here is a way for two different findings
/// to look alike. "Exercise the … fallback" and "Test the … fallback" on
/// `openhuman#7129` are the evidence for these.
const SYNONYMS: &[(&str, &str)] = &[("exercis", "test"), ("cover", "test"), ("coverag", "test")];

/// Lowercase ASCII words of `text`, without stopwords or bare numbers.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() >= 2 && !word.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_ascii_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
}

/// Words that carry no concern.
///
/// Function words, plus the imperative scaffolding every finding title opens
/// with — "ensure", "make", "keep", "use", "avoid", "add" — which says what
/// kind of sentence it is, not what it is about. Also the HTML entity names
/// the renderer's escaping leaves in a posted title.
const STOPWORDS: &[&str] = &[
    "a", "about", "add", "after", "again", "all", "also", "amp", "an", "and", "any", "are", "as",
    "at", "avoid", "be", "been", "before", "being", "both", "but", "by", "can", "could", "did",
    "do", "does", "doing", "don", "done", "each", "eg", "ensure", "every", "few", "for", "from",
    "further", "gt", "had", "has", "have", "having", "he", "her", "here", "how", "ie", "if", "in",
    "instead", "into", "is", "it", "its", "just", "keep", "let", "lets", "lt", "make", "makes",
    "may", "me", "might", "more", "most", "must", "my", "nor", "not", "no", "of", "off", "on",
    "once", "only", "onto", "or", "other", "our", "out", "over", "own", "per", "quot", "rather",
    "same", "shall", "she", "should", "so", "some", "such", "than", "that", "the", "their", "them",
    "then", "there", "these", "they", "this", "those", "to", "too", "under", "up", "us", "use",
    "uses", "using", "very", "via", "was", "we", "were", "what", "when", "where", "which", "while",
    "who", "whom", "whose", "why", "will", "with", "would", "you", "your",
];

/// Suffixes stripped by [`stem`], longest first within each family.
const SUFFIXES: &[&str] = &[
    "izations", "ization", "ations", "ation", "ities", "ity", "ments", "ment", "ness", "ingly",
    "ings", "ing", "edly", "ed", "ies", "es", "s", "ly", "istic", "ism", "ic", "ate", "ive", "al",
    "e",
];

/// Light suffix stripping: up to three passes, never leaving fewer than three
/// letters — four for a derivational suffix, so `element` keeps its `ment`.
///
/// Not Porter, and not trying to produce English. Both sides of every
/// comparison go through the same function, so all it has to do is send
/// "exceeds", "exceeded" and "exceeding" to the same place.
fn stem(word: &str) -> String {
    let mut word = word.to_ascii_lowercase();
    for _ in 0..3 {
        let Some(suffix) = SUFFIXES.iter().find(|suffix| {
            let inflection = matches!(
                **suffix,
                "s" | "es" | "ies" | "ed" | "edly" | "ing" | "ings" | "ingly" | "ly" | "e"
            );
            word.len() >= suffix.len() + if inflection { 3 } else { 4 }
                && word.ends_with(*suffix)
                // `class`, `status`, `analysis` are not plurals.
                && !(**suffix == "s"
                    && (word.ends_with("ss") || word.ends_with("us") || word.ends_with("is")))
        }) else {
            break;
        };
        word.truncate(word.len() - suffix.len());
        if *suffix == "ies" {
            word.push('y');
        }
        // `emitting` → `emitt` → `emit`, to meet `emits` → `emit`.
        if matches!(*suffix, "ing" | "ings" | "ingly" | "ed" | "edly") {
            let bytes = word.as_bytes();
            if let [.., a, b] = bytes
                && a == b
                && !b"aeiouylsz".contains(b)
            {
                word.pop();
            }
        }
    }
    word
}

/// Whether two paths are a file and its own tests.
///
/// Same stem once the test marker is removed (`_test`, `_tests`, `.test`,
/// `.spec`, `test_`), at least one side actually a test, and the same home
/// directory once a trailing `tests`, `test`, `__tests__`, `spec` or `src`
/// component is set aside — so `crate/src/store.rs` and
/// `crate/tests/store.rs` pair, and two `mod.rs` files do not.
pub fn siblings(left: &str, right: &str) -> bool {
    if left == right {
        return false;
    }
    let (left_dir, left_file) = split_path(left);
    let (right_dir, right_file) = split_path(right);
    let (left_stem, left_marked) = test_stem(left_file);
    let (right_stem, right_marked) = test_stem(right_file);
    let left_test = left_marked || is_test_dir(left_dir);
    let right_test = right_marked || is_test_dir(right_dir);
    !left_stem.is_empty()
        && left_stem == right_stem
        && (left_test || right_test)
        && home(left_dir) == home(right_dir)
}

/// `(directory, file name)`; the directory is empty at the root.
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// The file name without its extension or test marker, and whether it had one.
fn test_stem(file: &str) -> (&str, bool) {
    let mut stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let mut marked = false;
    for suffix in [
        ".test", ".spec", "_tests", "_test", "_spec", "-test", "-spec",
    ] {
        if let Some(bare) = stem.strip_suffix(suffix) {
            stem = bare;
            marked = true;
            break;
        }
    }
    if let Some(bare) = stem.strip_prefix("test_") {
        stem = bare;
        marked = true;
    }
    (stem, marked)
}

/// Directory components that hold tests or sources next to each other.
const SIDE_DIRS: &[&str] = &["tests", "test", "__tests__", "spec", "src"];

fn is_test_dir(dir: &str) -> bool {
    dir.split('/')
        .any(|component| matches!(component, "tests" | "test" | "__tests__" | "spec"))
}

/// The directory a file and its tests share.
fn home(dir: &str) -> &str {
    let (parent, last) = dir.rsplit_once('/').unwrap_or(("", dir));
    if SIDE_DIRS.contains(&last) {
        parent
    } else {
        dir
    }
}

#[cfg(test)]
#[path = "concern_test.rs"]
mod tests;
