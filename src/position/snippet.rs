//! Snippet normalisation and the sliding window that matches it.
//!
//! Normalisation is the crux of the whole module, and it is deliberately
//! exactly three steps: **trim whitespace, strip one leading `+` or `-`, trim
//! again** — and blank lines are dropped entirely when splitting.
//!
//! That absorbs the three ways a model mangles code it copied out of a diff:
//!
//! 1. indentation drift, because it re-indented what it quoted;
//! 2. a leaked `+`/`-` marker, because it copied the diff rather than the code;
//! 3. blank-line mismatch, because it dropped or added an empty line.
//!
//! Both sides — the snippet and the haystack — go through the *same*
//! normalisation, which is what makes stripping a marker safe: a genuine line
//! of code beginning with `-` loses its marker on both sides and still
//! compares equal.
//!
//! Nothing beyond those three steps is normalised. Collapsing interior
//! whitespace or lowercasing would match code the model never quoted, and a
//! confident match on the wrong lines is worse than no match at all.

/// Normalise one line for comparison.
pub fn normalize(line: &str) -> &str {
    let trimmed = line.trim();
    let stripped = trimmed
        .strip_prefix('+')
        .or_else(|| trimmed.strip_prefix('-'))
        .unwrap_or(trimmed);
    stripped.trim()
}

/// Split a model-supplied snippet into normalised lines, dropping blanks.
///
/// A fenced block survives this: the fence lines normalise to backticks, which
/// simply fail to match anything in the haystack. Callers that know the model
/// fences its answer should strip the fence first — see
/// [`crate::position::relocate`].
pub fn needle(snippet: &str) -> Vec<&str> {
    snippet
        .lines()
        .map(normalize)
        .filter(|line| !line.is_empty())
        .collect()
}

/// Number the lines of a file, normalised, dropping blanks.
///
/// Line numbers are 1-based, and are the numbers of the *original* text, so a
/// match maps straight back through the dropped blanks.
pub fn haystack(text: &str) -> Vec<(u64, &str)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index as u64 + 1, normalize(line)))
        .filter(|(_, line)| !line.is_empty())
        .collect()
}

/// Slide `needle` over `haystack`, returning the keys of the first match.
///
/// First hit wins. A snippet that occurs twice is ambiguous, and preferring
/// the first occurrence is both what the diff order implies and what a human
/// reading top to bottom expects. An empty needle never matches: it would
/// otherwise match everything at line one.
pub fn find(haystack: &[(u64, &str)], needle: &[&str]) -> Option<(u64, u64)> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .find(|window| window.iter().map(|(_, text)| text).eq(needle.iter()))
        .map(|window| {
            let start = window[0].0;
            let end = window[window.len() - 1].0;
            (start.min(end), start.max(end))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_drift_is_absorbed() {
        assert_eq!(normalize("        let x = 1;"), "let x = 1;");
        assert_eq!(normalize("let x = 1;"), "let x = 1;");
    }

    #[test]
    fn one_leaked_diff_marker_is_stripped_and_only_one() {
        assert_eq!(normalize("+    let x = 1;"), "let x = 1;");
        assert_eq!(normalize("-    let x = 1;"), "let x = 1;");
        // A second marker is content: `--foo` is a real command-line flag, and
        // eating it would match the wrong line.
        assert_eq!(normalize("--force"), "-force");
    }

    #[test]
    fn a_carriage_return_does_not_survive_normalisation() {
        assert_eq!(normalize("+    let x = 1;\r"), "let x = 1;");
    }

    #[test]
    fn blank_lines_are_dropped_from_both_sides() {
        assert_eq!(needle("a\n\n   \nb\n"), vec!["a", "b"]);
        assert_eq!(haystack("a\n\nb\n"), vec![(1, "a"), (3, "b")]);
    }

    #[test]
    fn a_match_reports_original_line_numbers_across_dropped_blanks() {
        let text = "one\n\ntwo\nthree\n";
        let found = find(&haystack(text), &needle("two\nthree"));
        assert_eq!(found, Some((3, 4)));
    }

    #[test]
    fn blank_line_drift_between_snippet_and_file_still_matches() {
        let text = "fn a() {\n\n    body();\n}\n";
        assert_eq!(
            find(&haystack(text), &needle("fn a() {\n    body();\n}")),
            Some((1, 4))
        );
    }

    #[test]
    fn an_empty_needle_never_matches() {
        assert_eq!(find(&haystack("a\nb\n"), &needle("   \n\n")), None);
    }

    #[test]
    fn a_needle_longer_than_the_haystack_never_matches() {
        assert_eq!(find(&haystack("a\n"), &needle("a\nb\nc")), None);
    }

    #[test]
    fn the_first_of_two_occurrences_wins() {
        let text = "x\nx\n";
        assert_eq!(find(&haystack(text), &needle("x")), Some((1, 1)));
    }
}
