//! Rendering the diff for a model, and splitting it into what was already
//! reviewed and what is new.
//!
//! The rendering lives here rather than in a lane because two callers need the
//! *same bytes*: the lane, which sends them, and `review`, which stores them so
//! the next cycle can replay them. Prompt layer 3 only pays if the replay is
//! byte-identical (see `crate::harness::prompt`), and two independent renderers
//! would drift apart on the first formatting change and silently cost every
//! re-review full input price.
//!
//! The split is per file. A file whose rendered block is unchanged since the
//! last review has already been reviewed, so it goes in the cacheable prefix;
//! anything else is the delta and goes in the volatile suffix. Per file rather
//! than per commit because a pull request diff is what the forge gives us —
//! there is no local checkout to compute a commit range against.

use crate::evidence::diff::{FileDiff, LineKind};

/// The marker that opens a file's block in rendered evidence.
const FILE_PREFIX: &str = "--- ";

/// Render diffs into the text a model sees.
///
/// Line numbers are included because models anchor badly when left to count,
/// and a finding anchored to the wrong line is a comment on somebody else's
/// code.
pub fn render(diffs: &[FileDiff]) -> String {
    let mut out = String::new();
    for diff in diffs {
        if diff.hunks.is_empty() {
            continue;
        }
        out.push_str(&format!("{FILE_PREFIX}{}\n", diff.path));
        for hunk in &diff.hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
            ));
            for line in &hunk.lines {
                let marker = match line.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                match line.new_line {
                    Some(n) => out.push_str(&format!("{n:>5} {marker}{}\n", line.text)),
                    None => out.push_str(&format!("      {marker}{}\n", line.text)),
                }
            }
        }
    }
    out
}

/// Split `current` into the part `previous` already contained, and the rest.
///
/// A block is "already reviewed" only when it matches byte for byte: a file
/// that gained one line since the last review is new work in its entirety,
/// because the model cannot be shown half a hunk. Returns `("", current)` when
/// there is nothing to replay, which is the first-review case.
pub fn split(previous: &str, current: &str) -> (String, String) {
    if previous.trim().is_empty() {
        return (String::new(), current.to_string());
    }

    let seen: Vec<&str> = blocks(previous);
    let mut reviewed = String::new();
    let mut fresh = String::new();

    for block in blocks(current) {
        if seen.contains(&block) {
            reviewed.push_str(block);
        } else {
            fresh.push_str(block);
        }
    }

    // Everything already reviewed and nothing new: replaying it as the prefix
    // would leave the model with an empty task. Send it as the work instead —
    // this is a re-run of the same commit, not an increment.
    if fresh.trim().is_empty() {
        return (String::new(), current.to_string());
    }

    (reviewed, fresh)
}

/// Split rendered evidence into one slice per file, each keeping its trailing
/// newline so reassembly is exact.
fn blocks(text: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = text
        .match_indices(FILE_PREFIX)
        .filter(|(index, _)| *index == 0 || text.as_bytes()[index - 1] == b'\n')
        .map(|(index, _)| index)
        .collect();

    if starts.first() != Some(&0) && !text.is_empty() {
        // Anything before the first file header is not attributable to a file;
        // treat it as its own block so no bytes are dropped.
        starts.insert(0, 0);
    }

    let mut out = Vec::with_capacity(starts.len());
    for (i, start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        out.push(&text[*start..end]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::diff::parse_file_patch;

    const PATCH: &str = "@@ -1,3 +1,4 @@\n fn main() {\n+    let x = items[i];\n }\n";

    #[test]
    fn rendering_carries_line_numbers() {
        let rendered = render(&[parse_file_patch("src/main.rs", PATCH)]);
        assert!(rendered.starts_with("--- src/main.rs\n"));
        assert!(rendered.contains("2 +    let x = items[i];"), "{rendered}");
    }

    #[test]
    fn a_first_review_has_nothing_to_replay() {
        let current = render(&[parse_file_patch("src/main.rs", PATCH)]);
        let (reviewed, fresh) = split("", &current);
        assert!(reviewed.is_empty());
        assert_eq!(fresh, current);
    }

    #[test]
    fn an_untouched_file_moves_into_the_replayed_half() {
        let old = render(&[parse_file_patch("src/main.rs", PATCH)]);
        let current = format!(
            "{old}{}",
            render(&[parse_file_patch("src/new.rs", PATCH)])
        );

        let (reviewed, fresh) = split(&old, &current);
        assert_eq!(reviewed, old, "the replay must be byte-identical");
        assert!(fresh.contains("--- src/new.rs"));
        assert!(!fresh.contains("--- src/main.rs"));
    }

    #[test]
    fn a_file_that_changed_since_the_last_review_is_new_work() {
        let old = render(&[parse_file_patch("src/main.rs", PATCH)]);
        let grown = render(&[parse_file_patch(
            "src/main.rs",
            "@@ -1,3 +1,5 @@\n fn main() {\n+    let x = items[i];\n+    dbg!(x);\n }\n",
        )]);

        let (reviewed, fresh) = split(&old, &grown);
        assert!(reviewed.is_empty());
        assert_eq!(fresh, grown);
    }

    #[test]
    fn re_running_the_same_commit_reviews_it_rather_than_replaying_everything() {
        // Otherwise the suffix is empty and the model is handed no work at all.
        let same = render(&[parse_file_patch("src/main.rs", PATCH)]);
        let (reviewed, fresh) = split(&same, &same);
        assert!(reviewed.is_empty());
        assert_eq!(fresh, same);
    }

    #[test]
    fn splitting_never_loses_bytes() {
        let old = render(&[parse_file_patch("a.rs", PATCH)]);
        let current = format!(
            "{old}{}{}",
            render(&[parse_file_patch("b.rs", PATCH)]),
            render(&[parse_file_patch("c.rs", PATCH)])
        );
        let (reviewed, fresh) = split(&old, &current);
        assert_eq!(reviewed.len() + fresh.len(), current.len());
    }
}
