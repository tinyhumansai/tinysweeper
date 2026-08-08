//! Unified-diff parsing and the changed-line map.
//!
//! This module exists to answer one question exactly: **which lines of the head
//! revision did this pull request touch?** Everything downstream depends on the
//! answer being right. A finding anchored to a line the pull request did not
//! change is either dropped or, worse, posted against unrelated code — which is
//! the single most common way a review bot loses a team's trust.
//!
//! Line numbers here are always **head-revision** line numbers, because that is
//! what GitHub's review-comment API anchors to.

use std::collections::BTreeSet;

use crate::forge::types::ChangedFile;

/// One hunk of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First line of the hunk in the base revision.
    pub old_start: u64,
    /// How many base-revision lines it covers.
    pub old_lines: u64,
    /// First line of the hunk in the head revision.
    pub new_start: u64,
    /// How many head-revision lines it covers.
    pub new_lines: u64,
    /// The hunk's lines, in order.
    pub lines: Vec<DiffLine>,
}

/// One line inside a hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// What the line is.
    pub kind: LineKind,
    /// Its line number in the head revision. Absent for removed lines, which
    /// do not exist there.
    pub new_line: Option<u64>,
    /// Its line number in the base revision. Absent for added lines.
    pub old_line: Option<u64>,
    /// The content, without the leading marker.
    pub text: String,
}

/// What a diff line represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Present in both revisions.
    Context,
    /// Introduced by this change.
    Added,
    /// Deleted by this change.
    Removed,
}

/// A parsed file diff, with its changed-line set precomputed.
#[derive(Debug, Clone, Default)]
pub struct FileDiff {
    /// Path in the head revision.
    pub path: String,
    /// The hunks, in order.
    pub hunks: Vec<Hunk>,
    /// Head-revision lines this change added or modified.
    ///
    /// This is the anchor set: a finding on any other line is not about this
    /// pull request.
    pub changed_lines: BTreeSet<u64>,
}

impl FileDiff {
    /// Whether `line` was added or modified by this change.
    pub fn touches(&self, line: u64) -> bool {
        self.changed_lines.contains(&line)
    }

    /// Whether any line in `start..=end` was added or modified.
    ///
    /// Multi-line findings anchor to a range, and a range that overlaps the
    /// change at all is fair game — requiring every line to be touched would
    /// throw away findings about a block that gained one line.
    pub fn touches_range(&self, start: u64, end: u64) -> bool {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.changed_lines.range(start..=end).next().is_some()
    }

    /// The added lines, as `(line number, text)`.
    ///
    /// This is what the secret scanner walks: a secret already in the base
    /// revision is not this pull request's problem, and re-reporting it every
    /// time anyone touches the file is exactly the noise we are avoiding.
    pub fn added_lines(&self) -> impl Iterator<Item = (u64, &str)> {
        self.hunks.iter().flat_map(|hunk| {
            hunk.lines.iter().filter_map(|line| match line.kind {
                LineKind::Added => Some((line.new_line.unwrap_or(0), line.text.as_str())),
                _ => None,
            })
        })
    }

    /// How many lines were added.
    pub fn additions(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == LineKind::Added)
            .count()
    }

    /// How many lines were removed.
    pub fn deletions(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == LineKind::Removed)
            .count()
    }
}

/// Parse the unified diff of one file.
///
/// Accepts either a bare hunk sequence (what GitHub's `patch` field contains)
/// or a full `diff --git` stanza, so the same parser serves the forge adapter
/// and `git diff` output.
pub fn parse_file_patch(path: &str, patch: &str) -> FileDiff {
    let mut diff = FileDiff {
        path: path.to_string(),
        ..FileDiff::default()
    };

    let mut current: Option<Hunk> = None;
    let mut new_line = 0u64;
    let mut old_line = 0u64;

    for raw in patch.lines() {
        if raw.starts_with("@@") {
            if let Some(hunk) = current.take() {
                diff.hunks.push(hunk);
            }
            match parse_hunk_header(raw) {
                Some(header) => {
                    new_line = header.new_start;
                    old_line = header.old_start;
                    current = Some(header);
                }
                // A malformed header means we no longer know where we are, so
                // stop attributing lines rather than guess and mis-anchor.
                None => current = None,
            }
            continue;
        }

        // Headers only appear *between* hunks. Inside one, `--- foo` is a
        // removed line whose content happens to start with `--`, and treating
        // it as a header drops it — desynchronising every line number after it,
        // which is the one thing this parser must never do.
        if current.is_none()
            && (raw.starts_with("diff --git")
                || raw.starts_with("index ")
                || raw.starts_with("--- ")
                || raw.starts_with("+++ "))
        {
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            continue;
        };

        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &raw[1..]),
            Some(b'-') => (LineKind::Removed, &raw[1..]),
            Some(b' ') => (LineKind::Context, &raw[1..]),
            // "\ No newline at end of file" carries no line of its own.
            Some(b'\\') => continue,
            // An empty line inside a hunk is a context line whose single space
            // some tools strip.
            None => (LineKind::Context, ""),
            _ => continue,
        };

        match kind {
            LineKind::Added => {
                hunk.lines.push(DiffLine {
                    kind,
                    new_line: Some(new_line),
                    old_line: None,
                    text: text.to_string(),
                });
                diff.changed_lines.insert(new_line);
                new_line += 1;
            }
            LineKind::Removed => {
                hunk.lines.push(DiffLine {
                    kind,
                    new_line: None,
                    old_line: Some(old_line),
                    text: text.to_string(),
                });
                old_line += 1;
            }
            LineKind::Context => {
                hunk.lines.push(DiffLine {
                    kind,
                    new_line: Some(new_line),
                    old_line: Some(old_line),
                    text: text.to_string(),
                });
                new_line += 1;
                old_line += 1;
            }
        }
    }

    if let Some(hunk) = current.take() {
        diff.hunks.push(hunk);
    }

    diff
}

/// Parse every file diff a forge reported.
///
/// Files the forge gave no patch for — binaries, and diffs it truncated —
/// produce an empty [`FileDiff`], so they still appear in the file list with an
/// empty anchor set rather than vanishing.
pub fn parse_changed_files(files: &[ChangedFile]) -> Vec<FileDiff> {
    files
        .iter()
        .map(|file| match &file.patch {
            Some(patch) => parse_file_patch(&file.path, patch),
            None => FileDiff {
                path: file.path.clone(),
                ..FileDiff::default()
            },
        })
        .collect()
}

fn parse_hunk_header(line: &str) -> Option<Hunk> {
    // @@ -old_start,old_lines +new_start,new_lines @@ optional heading
    let body = line.strip_prefix("@@")?;
    let end = body.find("@@")?;
    let ranges = &body[..end];

    let mut old = None;
    let mut new = None;
    for token in ranges.split_whitespace() {
        if let Some(rest) = token.strip_prefix('-') {
            old = parse_range(rest);
        } else if let Some(rest) = token.strip_prefix('+') {
            new = parse_range(rest);
        }
    }

    let (old_start, old_lines) = old?;
    let (new_start, new_lines) = new?;

    Some(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
    })
}

/// Parse `start` or `start,count`. A missing count means one line.
fn parse_range(text: &str) -> Option<(u64, u64)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
@@ -1,4 +1,5 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    println!(\"extra\");
 }
 ";

    #[test]
    fn added_lines_get_head_revision_numbers() {
        let diff = parse_file_patch("src/main.rs", SIMPLE);

        // Line 1 is context, 2 and 3 are the two added lines.
        assert_eq!(diff.changed_lines, [2, 3].into_iter().collect());
        assert!(diff.touches(2));
        assert!(diff.touches(3));
        assert!(!diff.touches(1), "context lines are not changed lines");
    }

    #[test]
    fn removed_lines_never_enter_the_anchor_set() {
        let diff = parse_file_patch("src/main.rs", SIMPLE);
        let removed: Vec<_> = diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == LineKind::Removed)
            .collect();

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].new_line, None, "a removed line has no head line");
    }

    #[test]
    fn additions_and_deletions_are_counted() {
        let diff = parse_file_patch("src/main.rs", SIMPLE);
        assert_eq!(diff.additions(), 2);
        assert_eq!(diff.deletions(), 1);
    }

    #[test]
    fn added_lines_yields_number_and_text() {
        let diff = parse_file_patch("src/main.rs", SIMPLE);
        let added: Vec<_> = diff.added_lines().collect();

        assert_eq!(
            added,
            vec![
                (2, "    println!(\"new\");"),
                (3, "    println!(\"extra\");")
            ]
        );
    }

    #[test]
    fn multiple_hunks_track_their_own_offsets() {
        let patch = "\
@@ -1,3 +1,4 @@
 a
+b
 c
 d
@@ -20,3 +21,4 @@
 x
+y
 z
 w
";
        let diff = parse_file_patch("f.txt", patch);

        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.changed_lines, [2, 22].into_iter().collect());
    }

    #[test]
    fn a_single_line_hunk_header_without_a_count_is_understood() {
        let diff = parse_file_patch("f.txt", "@@ -1 +1 @@\n-old\n+new\n");
        assert_eq!(diff.changed_lines, [1].into_iter().collect());
    }

    #[test]
    fn a_hunk_heading_after_the_second_marker_is_ignored() {
        let patch = "@@ -1,2 +1,3 @@ fn main() {\n a\n+b\n c\n";
        let diff = parse_file_patch("f.txt", patch);
        assert_eq!(diff.changed_lines, [2].into_iter().collect());
    }

    #[test]
    fn a_full_diff_git_stanza_parses_the_same_way() {
        let patch = "\
diff --git a/src/main.rs b/src/main.rs
index 1234567..89abcde 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,2 +1,3 @@
 fn main() {
+    let x = 1;
 }
";
        let diff = parse_file_patch("src/main.rs", patch);
        assert_eq!(diff.changed_lines, [2].into_iter().collect());
    }

    #[test]
    fn a_removed_line_starting_with_dashes_is_not_mistaken_for_a_header() {
        // `--- old text` inside a hunk is a removed line, not a file header.
        // Dropping it would shift every line number after it.
        let patch = "@@ -1,3 +1,3 @@\n a\n---- separator\n+=== separator\n c\n";
        let diff = parse_file_patch("README.md", patch);

        assert_eq!(diff.deletions(), 1);
        assert_eq!(
            diff.changed_lines,
            [2].into_iter().collect(),
            "the replacement line must still be line 2"
        );
    }

    #[test]
    fn a_no_newline_marker_does_not_consume_a_line_number() {
        let patch = "@@ -1,1 +1,2 @@\n a\n+b\n\\ No newline at end of file\n";
        let diff = parse_file_patch("f.txt", patch);
        assert_eq!(diff.changed_lines, [2].into_iter().collect());
    }

    #[test]
    fn a_malformed_hunk_header_stops_attribution_rather_than_guessing() {
        // Mis-anchoring is worse than missing: a wrong line number puts a
        // comment on unrelated code.
        let patch = "@@ garbage @@\n+never attributed\n";
        let diff = parse_file_patch("f.txt", patch);
        assert!(diff.changed_lines.is_empty());
    }

    #[test]
    fn ranges_overlap_when_any_line_is_touched() {
        let diff = parse_file_patch("f.txt", SIMPLE);

        assert!(diff.touches_range(1, 5), "spans the changed lines");
        assert!(diff.touches_range(3, 1), "reversed bounds are normalised");
        assert!(!diff.touches_range(10, 20), "entirely outside the change");
    }

    #[test]
    fn an_empty_patch_yields_an_empty_anchor_set() {
        let diff = parse_file_patch("f.txt", "");
        assert!(diff.changed_lines.is_empty());
        assert!(diff.hunks.is_empty());
    }

    #[test]
    fn a_file_with_no_patch_still_appears_with_an_empty_anchor_set() {
        // Binary files and truncated diffs must not vanish: the commits lane
        // still wants to know a 4MB blob landed.
        let files = vec![ChangedFile {
            path: "assets/logo.png".into(),
            patch: None,
            ..ChangedFile::default()
        }];
        let diffs = parse_changed_files(&files);

        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "assets/logo.png");
        assert!(diffs[0].changed_lines.is_empty());
    }

    #[test]
    fn a_pure_addition_file_anchors_every_line() {
        let patch = "@@ -0,0 +1,3 @@\n+one\n+two\n+three\n";
        let diff = parse_file_patch("new.rs", patch);
        assert_eq!(diff.changed_lines, [1, 2, 3].into_iter().collect());
    }
}
