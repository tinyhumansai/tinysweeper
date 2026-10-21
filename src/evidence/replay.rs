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

use std::collections::{BTreeMap, BTreeSet};

use crate::evidence::diff::{FileDiff, LineKind};

/// The marker that opens a file's block in rendered evidence.
const FILE_PREFIX: &str = "--- ";

/// Render diffs into the text a model sees.
///
/// Line numbers are included because models anchor badly when left to count,
/// and a finding anchored to the wrong line is a comment on somebody else's
/// code.
pub fn render(diffs: &[FileDiff]) -> String {
    // Delegates rather than duplicates. This was a second, byte-identical copy
    // of `diff::render`, which is precisely the drift this module's own doc
    // comment warns about: the moment the two disagreed by one space, every
    // replay would stop matching and every re-review would silently pay full
    // input price with nothing to show for it.
    crate::evidence::diff::render(diffs)
}

/// Deterministic links between changed files that can contradict one another.
///
/// Built once per lane run and borrowed by each group. The index deliberately
/// links only mechanical relationships visible in the diff: an added module
/// declaration and its added module file, or a changed function signature and
/// a changed call site. It is not a substitute for repository lookup.
pub struct CrossFileEvidenceIndex<'a> {
    by_path: BTreeMap<&'a str, &'a FileDiff>,
    related: BTreeMap<&'a str, BTreeSet<&'a str>>,
}

impl<'a> CrossFileEvidenceIndex<'a> {
    /// Index changed module declarations and signature/call-site pairs.
    pub fn new(diffs: &'a [FileDiff]) -> Self {
        let by_path: BTreeMap<&str, &FileDiff> = diffs
            .iter()
            .map(|diff| (diff.path.as_str(), diff))
            .collect();
        let mut related: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

        for declaration in diffs {
            for target in added_module_targets(declaration) {
                if by_path.contains_key(target.as_str()) {
                    related
                        .entry(declaration.path.as_str())
                        .or_default()
                        .insert(by_path[target.as_str()].path.as_str());
                    related
                        .entry(by_path[target.as_str()].path.as_str())
                        .or_default()
                        .insert(declaration.path.as_str());
                }
            }
        }

        let changed_signatures: BTreeMap<&str, Vec<&str>> = diffs
            .iter()
            .flat_map(|diff| {
                changed_function_names(diff)
                    .into_iter()
                    .map(move |name| (name, diff.path.as_str()))
            })
            .fold(BTreeMap::new(), |mut index, (name, path)| {
                index.entry(name).or_default().push(path);
                index
            });
        for diff in diffs {
            for name in added_call_names(diff) {
                if let Some(signatures) = changed_signatures.get(name.as_str()) {
                    for signature in signatures {
                        if *signature != diff.path {
                            related
                                .entry(signature)
                                .or_default()
                                .insert(diff.path.as_str());
                            related
                                .entry(diff.path.as_str())
                                .or_default()
                                .insert(signature);
                        }
                    }
                }
            }
        }

        Self { by_path, related }
    }

    /// Render a group's own diffs plus bounded changed-file evidence linked to
    /// them. Omitted files and bytes are disclosed so the falsifier cannot
    /// treat a partial excerpt as the complete pull request.
    pub fn render_for(&self, group_diffs: &[FileDiff]) -> String {
        const MAX_FILES: usize = 8;
        const MAX_BYTES: usize = 18_000;
        const TRUNCATION: &str = "\n[Cross-file evidence is incomplete: related files or bytes were omitted by the evidence bound.]\n";

        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();
        // Keep the group being reviewed first. Alphabetical ordering used to
        // let unrelated `a*.rs` files crowd out the anchor under the file cap.
        for diff in group_diffs {
            if seen.insert(diff.path.as_str()) {
                paths.push(diff.path.as_str());
            }
        }
        for diff in group_diffs {
            if let Some(related) = self.related.get(diff.path.as_str()) {
                for path in related {
                    if seen.insert(*path) {
                        paths.push(path);
                    }
                }
            }
        }
        let mut rendered = String::new();
        let mut incomplete = paths.len() > MAX_FILES;
        for path in paths.iter().take(MAX_FILES) {
            let Some(diff) = self.by_path.get(path) else {
                continue;
            };
            let separator = usize::from(!rendered.is_empty());
            let remaining = MAX_BYTES.saturating_sub(rendered.len() + separator);
            let (block, cut) = render_bounded(diff, remaining.saturating_sub(TRUNCATION.len()));
            incomplete |= cut;
            if separator > 0 {
                rendered.push('\n');
            }
            rendered.push_str(&block);
            if cut {
                break;
            }
        }
        if incomplete {
            if rendered.len() + TRUNCATION.len() > MAX_BYTES {
                let keep = MAX_BYTES - TRUNCATION.len();
                let mut boundary = keep.min(rendered.len());
                while !rendered.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                rendered.truncate(boundary);
            }
            rendered.push_str(TRUNCATION);
        }
        rendered
    }
}

/// Render complete headers and diff lines until the byte bound is reached.
/// Formatting each line as it is visited avoids first materializing an
/// unbounded changed file just to throw most of it away.
fn render_bounded(diff: &FileDiff, limit: usize) -> (String, bool) {
    use std::fmt::Write as _;

    let mut out = String::new();
    if diff.hunks.is_empty() {
        return (out, false);
    }
    let header = format!("--- {}\n", crate::scan::scrub(&diff.path));
    if header.len() > limit {
        return (out, true);
    }
    out.push_str(&header);
    for hunk in &diff.hunks {
        let header = format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        );
        if out.len() + header.len() > limit {
            return (out, true);
        }
        out.push_str(&header);
        for line in &hunk.lines {
            let marker = match line.kind {
                LineKind::Added => '+',
                LineKind::Removed => '-',
                LineKind::Context => ' ',
            };
            let mut rendered = String::new();
            match line.new_line {
                Some(number) => {
                    let _ = writeln!(rendered, "{number:>5} {marker}{}", line.text);
                }
                None => {
                    let _ = writeln!(rendered, "      {marker}{}", line.text);
                }
            }
            if out.len() + rendered.len() > limit {
                return (out, true);
            }
            out.push_str(&rendered);
        }
    }
    (out, false)
}

fn added_module_targets(diff: &FileDiff) -> Vec<String> {
    let parent = std::path::Path::new(&diff.path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let stem = std::path::Path::new(&diff.path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let base = if matches!(stem, "lib" | "main" | "mod") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    diff.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == LineKind::Added)
        .filter_map(|line| {
            let declaration = ["pub ", "pub(crate) ", "pub(super) ", ""]
                .iter()
                .find_map(|prefix| line.text.trim().strip_prefix(prefix))?
                .strip_prefix("mod ")?
                .strip_suffix(';')?
                .trim();
            (declaration
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !declaration.is_empty())
            .then(|| {
                vec![
                    base.join(format!("{declaration}.rs")),
                    base.join(declaration).join("mod.rs"),
                ]
            })
        })
        .flatten()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn function_name(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("pub(") {
            rest = after.get(after.find(')')? + 1..)?.trim_start();
        } else if let Some(after) = ["pub ", "async ", "unsafe ", "const "]
            .iter()
            .find_map(|prefix| rest.strip_prefix(prefix))
        {
            rest = after;
        } else {
            break;
        }
    }
    let tail = rest.strip_prefix("fn ")?;
    let name = tail.split('(').next()?.trim();
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(name)
}

fn changed_function_names(diff: &FileDiff) -> BTreeSet<&str> {
    let removed: BTreeSet<&str> = diff
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == LineKind::Removed)
        .filter_map(|line| function_name(&line.text))
        .collect();
    diff.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == LineKind::Added)
        .filter_map(|line| function_name(&line.text))
        .filter(|name| removed.contains(name))
        .collect()
}

fn added_call_names(diff: &FileDiff) -> BTreeSet<String> {
    let mut calls = BTreeSet::new();
    for line in diff
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == LineKind::Added)
    {
        let bytes = line.text.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if !(bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_') {
                cursor += 1;
                continue;
            }
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
            {
                cursor += 1;
            }
            let end = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'(' {
                calls.insert(line.text[start..end].to_owned());
            }
        }
    }
    calls
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

/// Which of `diffs` a previous review has already seen, byte for byte.
///
/// The same rule as [`split`], asked per file instead of over one string: a
/// file whose rendered block is identical to one in `previous` was reviewed and
/// has not changed since.
///
/// This is what lets a per-file lane skip work rather than merely cache it.
/// `split` moves already-reviewed bytes into the cacheable prefix, which only
/// pays when the provider honours the cache; skipping the file entirely pays
/// always, because the cheapest call is the one not made.
///
/// Returns everything as unreviewed when `previous` is empty — the first-review
/// case — so a caller can use this unconditionally.
pub fn unreviewed<'a>(previous: &str, diffs: &'a [FileDiff]) -> Vec<&'a FileDiff> {
    if previous.trim().is_empty() {
        return diffs.iter().collect();
    }

    let seen = blocks(previous);

    let fresh: Vec<&FileDiff> = diffs
        .iter()
        .filter(|diff| {
            // Rendered one file at a time, through the same function that
            // produced `previous`. Comparing anything other than the exact
            // bytes both sides emit would drift on the first formatting change
            // and silently start re-reviewing everything.
            let block = render(std::slice::from_ref(*diff));
            !block.is_empty() && !seen.contains(&block.as_str())
        })
        .collect();

    // Nothing new: this is a re-run of the same commit rather than an
    // increment, and returning an empty list would make the lane report that a
    // pull request has no attack surface. Same reasoning as `split`.
    if fresh.is_empty() {
        return diffs.iter().collect();
    }

    fresh
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
    fn bounded_cross_file_evidence_keeps_the_reviewed_group_first() {
        let mut diffs = vec![parse_file_patch(
            "src/zown.rs",
            "@@ -1 +1 @@\n-fn changed() {}\n+fn changed(value: u8) {}\n",
        )];
        for index in 0..10 {
            diffs.push(parse_file_patch(
                &format!("src/a{index}.rs"),
                &format!("@@ -1 +1 @@\n-fn run() {{}}\n+fn run() {{ changed({index}); }}\n"),
            ));
        }

        let evidence = CrossFileEvidenceIndex::new(&diffs).render_for(&diffs[..1]);

        assert!(evidence.starts_with("--- src/zown.rs\n"), "{evidence}");
        assert!(evidence.contains("fn changed(value: u8)"), "{evidence}");
        assert!(
            evidence.contains("Cross-file evidence is incomplete"),
            "{evidence}"
        );
        assert!(evidence.len() <= 18_000, "{} bytes", evidence.len());
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
        let current = format!("{old}{}", render(&[parse_file_patch("src/new.rs", PATCH)]));

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

    #[test]
    fn an_unchanged_file_is_not_offered_for_review_again() {
        // The single largest avoidable cost in a review: the security lane
        // spends one model call per file, so a push touching one file used to
        // pay for every file in the pull request, every time.
        let old = parse_file_patch("src/old.rs", "@@ -1,1 +1,2 @@\n a\n+b\n");
        let new = parse_file_patch("src/new.rs", "@@ -1,1 +1,2 @@\n x\n+y\n");
        let previous = render(std::slice::from_ref(&old));

        let both = [old.clone(), new.clone()];
        let fresh = unreviewed(&previous, &both);

        let paths: Vec<&str> = fresh.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["src/new.rs"], "{paths:?}");
    }

    #[test]
    fn a_file_that_gained_a_line_is_reviewed_again_in_full() {
        // Byte-for-byte or not at all. A model cannot be shown half a hunk, so
        // a file that changed at all is new work in its entirety.
        let before = parse_file_patch("src/a.rs", "@@ -1,1 +1,2 @@\n a\n+b\n");
        let after = parse_file_patch("src/a.rs", "@@ -1,1 +1,3 @@\n a\n+b\n+c\n");
        let previous = render(std::slice::from_ref(&before));

        let fresh = unreviewed(&previous, std::slice::from_ref(&after));

        assert_eq!(fresh.len(), 1, "{fresh:?}");
    }

    #[test]
    fn a_rerun_of_the_same_commit_reviews_everything_rather_than_nothing() {
        // Nothing new is a re-run, not an increment. Returning an empty list
        // would make the lane report that a pull request has no attack surface,
        // which is a far worse answer than doing the work twice.
        let file = parse_file_patch("src/a.rs", "@@ -1,1 +1,2 @@\n a\n+b\n");
        let previous = render(std::slice::from_ref(&file));

        let fresh = unreviewed(&previous, std::slice::from_ref(&file));

        assert_eq!(fresh.len(), 1, "{fresh:?}");
    }

    #[test]
    fn a_first_review_offers_every_file() {
        let file = parse_file_patch("src/a.rs", "@@ -1,1 +1,2 @@\n a\n+b\n");
        assert_eq!(unreviewed("", std::slice::from_ref(&file)).len(), 1);
    }

    #[test]
    fn the_two_renderers_agree_byte_for_byte() {
        // They were separate copies of the same code. The replay only pays if
        // the bytes match exactly, so a divergence here would not fail loudly —
        // it would quietly make every re-review cost full price.
        let file = parse_file_patch("src/a.rs", "@@ -1,2 +1,3 @@\n a\n+b\n c\n");
        assert_eq!(
            render(std::slice::from_ref(&file)),
            crate::evidence::diff::render(std::slice::from_ref(&file))
        );
    }
}
