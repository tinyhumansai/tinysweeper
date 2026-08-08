//! Composing the retrieval query from a pull request.
//!
//! Always compiled.
//!
//! The obvious implementation is to embed a slice of the raw diff, and it is
//! wrong in a way that only shows up on the pull requests that need retrieval
//! most. A fixed byte slice of a large diff is the *first* files in it: every
//! later file is unrepresented in the query, so the reviewer retrieves context
//! for the top of the change and nothing for the bottom. It also makes
//! embedding cost scale with diff size, on the one input that has no upper
//! bound.
//!
//! So the query is **composed**, from four things a diff states about itself:
//!
//! | section     | share | what it contributes                         |
//! |-------------|-------|---------------------------------------------|
//! | title       |   5%  | what the author says the change is for      |
//! | paths       |  20%  | where in the repository it lands            |
//! | headings    |  20%  | the enclosing signatures git already named  |
//! | identifiers |  55%  | the names the change actually moves          |
//!
//! The shares are per-section **caps**, and that is the load-bearing part: a
//! diff that renames a directory is hundreds of paths and would otherwise fill
//! the whole query with directory names, leaving no room for the identifiers
//! that decide what comes back. Unused budget flows forward to the identifiers,
//! never backwards, so a small diff spends its slack on the section that always
//! wants more.
//!
//! Where a section does not fit, it is **sampled across its whole list** rather
//! than truncated at the front, and the last entry is always kept. A 300-file
//! diff that only ever describes its first forty files is the failure this
//! replaces.

use std::collections::BTreeMap;

use crate::evidence::diff::{FileDiff, LineKind};

/// Share of the query budget each section may claim, in percent.
///
/// Identifiers get the majority because they are the only section whose content
/// is the vocabulary of the change itself; the rest is metadata about where it
/// happened.
const TITLE_SHARE: usize = 5;
const PATH_SHARE: usize = 20;
const HEADING_SHARE: usize = 20;

/// Shortest token treated as an identifier.
///
/// Two characters is `ok`, `id`, `fn` — noise in a bag-of-words query, and the
/// lexical arm scores them near zero anyway because they appear everywhere.
const MIN_IDENTIFIER: usize = 3;

/// Words that appear in every diff and discriminate nothing.
///
/// Deliberately short and language-mixed rather than a curated per-language
/// list: the cost of missing a stopword is one wasted token, and the cost of
/// wrongly listing a real identifier is a query that cannot find the code it
/// was about.
const STOPWORDS: &[&str] = &[
    "and",
    "are",
    "as",
    "async",
    "await",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "def",
    "default",
    "else",
    "enum",
    "err",
    "error",
    "export",
    "extern",
    "false",
    "final",
    "float",
    "fmt",
    "fn",
    "for",
    "from",
    "func",
    "function",
    "get",
    "self",
    "if",
    "impl",
    "import",
    "in",
    "int",
    "interface",
    "is",
    "let",
    "map",
    "match",
    "mod",
    "move",
    "mut",
    "new",
    "next",
    "nil",
    "none",
    "not",
    "null",
    "num",
    "of",
    "or",
    "package",
    "pass",
    "priv",
    "pub",
    "public",
    "ref",
    "return",
    "set",
    "some",
    "static",
    "str",
    "string",
    "struct",
    "super",
    "switch",
    "the",
    "this",
    "throw",
    "to",
    "trait",
    "true",
    "try",
    "type",
    "use",
    "using",
    "val",
    "var",
    "void",
    "where",
    "while",
    "with",
    "yield",
];

/// Whether a token is worth putting in the query.
fn is_meaningful(token: &str) -> bool {
    if token.len() < MIN_IDENTIFIER {
        return false;
    }
    // A bare number carries no meaning on its own and diffs are full of them.
    if token.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    !STOPWORDS.contains(&token)
}

/// Split source text into candidate identifiers.
///
/// `snake_case` and `kebab-case` fall out of splitting on non-alphanumerics.
/// `camelCase` is deliberately *not* split: an embedding of `parseRequest`
/// matches indexed code containing `parseRequest`, and splitting it into two
/// common words would match everything.
fn tokenise(text: &str) -> impl Iterator<Item = String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| is_meaningful(token))
}

/// Take as much of `items` as fits in `budget`, spread across the whole list.
///
/// When everything fits this is just "everything", in order. When it does not,
/// entries are sampled at an even stride and the **last** entry is always
/// included — which is the property the whole function exists for. Truncating
/// at the front would leave a large diff's later files with no representation
/// in the query at all, and those are exactly the files a reviewer is least
/// likely to have context for already.
fn sample_into(items: &[String], budget: usize) -> Vec<String> {
    if budget == 0 || items.is_empty() {
        return Vec::new();
    }
    let total: usize = items.iter().map(|item| item.len() + 1).sum();
    if total <= budget {
        return items.to_vec();
    }

    // How many average-length entries fit. Ceil-divided length keeps the
    // estimate conservative for a list of mixed sizes; the exact trim below
    // enforces the budget regardless.
    let average = total.div_ceil(items.len()).max(1);
    let room = (budget / average).max(1);

    let mut picked = Vec::with_capacity(room.min(items.len()));
    let mut used = 0usize;
    for slot in 0..room.min(items.len()) {
        // Spread over the full range, ending on the final entry.
        let index = if room >= items.len() {
            slot
        } else {
            slot * (items.len() - 1) / (room - 1).max(1)
        };
        let item = &items[index];
        if picked.last() == Some(item) {
            continue;
        }
        if used + item.len() + 1 > budget {
            break;
        }
        used += item.len() + 1;
        picked.push(item.clone());
    }

    // The stride can stop short of the end when a long entry blew the budget.
    // The last entry is the one that proves late files are represented, so it
    // displaces an earlier one rather than being dropped.
    if let Some(last) = items.last()
        && !picked.contains(last)
    {
        while used + last.len() + 1 > budget && picked.len() > 1 {
            let removed = picked.remove(picked.len() - 1);
            used -= removed.len() + 1;
        }
        if used + last.len() < budget {
            picked.push(last.clone());
        }
    }

    picked
}

/// Build the bounded query text for one pull request.
///
/// `budget` is a hard ceiling on the returned string, so embedding cost is
/// constant regardless of how large the pull request is.
pub fn build_retrieval_query(title: &str, diffs: &[FileDiff], budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }

    let mut sections: Vec<String> = Vec::with_capacity(4);
    let mut spent = 0usize;

    let title = title.trim();
    if !title.is_empty() {
        let allowance = (budget * TITLE_SHARE / 100).max(1);
        let text: String = title.chars().take(allowance).collect();
        spent += text.len() + 1;
        sections.push(text);
    }

    let paths: Vec<String> = diffs.iter().map(|diff| diff.path.clone()).collect();
    let picked = sample_into(&paths, budget * PATH_SHARE / 100);
    if !picked.is_empty() {
        let text = picked.join(" ");
        spent += text.len() + 1;
        sections.push(text);
    }

    // Git writes the enclosing definition after the second `@@`, which makes it
    // the cheapest available statement of what a hunk is inside — usually the
    // exact signature the reviewer needs the callers of.
    let mut headings: Vec<String> = Vec::new();
    for diff in diffs {
        for hunk in &diff.hunks {
            let heading = hunk.heading.trim();
            if !heading.is_empty() && !headings.iter().any(|seen| seen == heading) {
                headings.push(heading.to_string());
            }
        }
    }
    let picked = sample_into(&headings, budget * HEADING_SHARE / 100);
    if !picked.is_empty() {
        let text = picked.join(" ");
        spent += text.len() + 1;
        sections.push(text);
    }

    // Everything left over, which is at least the identifiers' own share and
    // usually more. Frequency ranking across *every* file is what keeps a late
    // file's vocabulary in the query even when its path was sampled out.
    // `(count, first appearance)`. The ordinal is what stops the tie-break
    // being alphabetical — see the band sampling below.
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut seen = 0usize;
    for diff in diffs {
        for hunk in &diff.hunks {
            for line in &hunk.lines {
                if matches!(line.kind, LineKind::Added | LineKind::Removed) {
                    for token in tokenise(&line.text) {
                        let entry = counts.entry(token).or_insert((0, seen));
                        entry.0 += 1;
                        seen += 1;
                    }
                }
            }
        }
    }
    let mut ranked: Vec<(String, usize, usize)> = counts
        .into_iter()
        .map(|(token, (count, first))| (token, count, first))
        .collect();
    // Frequency first; ties in **diff order**, not alphabetical order. That
    // second half is not cosmetic. In a large diff almost every identifier
    // occurs once, so the tie-break decides nearly the whole section — and
    // sorting it alphabetically means the tail of the alphabet is cut, which in
    // practice means the files whose names sort late lose their vocabulary for
    // no reason anyone chose.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));

    let remaining = budget.saturating_sub(spent);
    let mut identifiers: Vec<String> = Vec::new();
    let mut used = 0usize;
    for band in ranked.chunk_by(|a, b| a.1 == b.1) {
        let tokens: Vec<String> = band.iter().map(|(token, _, _)| token.clone()).collect();
        let width: usize = tokens.iter().map(|token| token.len() + 1).sum();
        if used + width <= remaining {
            used += width;
            identifiers.extend(tokens);
            continue;
        }
        // The band that does not fit is a set of equally frequent identifiers,
        // so there is no ranking left to respect: sampling across it in diff
        // order keeps the late files represented, where taking its head would
        // stop somewhere in the middle of the change.
        identifiers.extend(sample_into(&tokens, remaining - used));
        break;
    }
    if !identifiers.is_empty() {
        sections.push(identifiers.join(" "));
    }

    let mut query = sections.join("\n");
    // The section budgets already add up to the whole, but rounding and the
    // separators can put it a byte or two over. Truncate on a character
    // boundary rather than a byte one: a title can be any UTF-8 at all.
    if query.len() > budget {
        let cut = (0..=budget)
            .rev()
            .find(|index| query.is_char_boundary(*index))
            .unwrap_or(0);
        query.truncate(cut);
    }
    query
}

#[cfg(test)]
#[path = "query_test.rs"]
mod tests;
