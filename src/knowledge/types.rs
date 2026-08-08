//! The knowledge centre's core types and its hard ceilings.
//!
//! Always compiled: nothing here talks to a network or a database.
//!
//! The constants in this file are the load-bearing part of the whole module.
//! Repository instruction files are written by whoever opened the pull request,
//! so the extraction pass in [`crate::knowledge::extract`] is assumed to be
//! jailbreakable and the ceilings are sized so that a *fully* jailbroken
//! extractor still cannot say much: 25 bullets of 200 characters is about 5 KB,
//! fenced and labelled as untrusted, in the volatile half of the prompt. Raise
//! either number and that argument stops holding.

use crate::index::types::KnowledgeScope;

/// How many rules survive extraction, at most.
///
/// Not configurable, and deliberately so. This is a security ceiling rather
/// than a tuning knob: it bounds what a compromised extraction can inject into
/// a review prompt, and a repository that could raise it could remove it.
pub const MAX_RULES: usize = 25;

/// How long one extracted rule may be, in characters.
///
/// Same reasoning as [`MAX_RULES`]. A rule is a sentence; anything longer is
/// either prose that was never a rule or a payload wearing a bullet point.
pub const MAX_RULE_CHARS: usize = 200;

/// How many configured instruction file names are accepted, at most.
///
/// Bounds the work a repository config can ask for: without it, a config
/// listing a thousand filenames is a thousand forge requests per pull request.
pub const MAX_CONFIGURED_INSTRUCTION_FILES: usize = 5;

/// How many scoped instruction files are read for one review, at most.
///
/// One configured name can apply at the repository root and at ancestors of
/// several changed files. This separate cap keeps that expansion bounded.
pub const MAX_INSTRUCTION_FILES: usize = 25;

/// Longest instruction filename accepted.
pub const MAX_FILENAME_LEN: usize = 64;

/// Whether `name` is a filename this module will fetch from each scope.
///
/// Strict allow-list, not a deny-list of bad shapes. An instruction filename
/// comes from configuration that a pull request author can edit, so the
/// question is not "is this path dangerous" but "is this one of the plain
/// repository-root filenames we support" — a path separator, a `..`, an
/// absolute path or a URL are all simply not that.
///
/// Rejecting `/` outright is what keeps the file list from becoming a way to
/// read arbitrary paths out of a repository (`.git/config`, a deploy key that
/// was committed by mistake) through an otherwise well-formed config.
pub fn valid_instruction_file(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FILENAME_LEN
        && !name.contains("..")
        && name != "."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Whether `path` is a safe, scoped instruction-file path.
///
/// Configuration supplies only a plain filename. Paths are constructed here
/// from changed-file ancestors, so accepting a slash does not let configuration
/// name arbitrary repository files.
pub fn valid_instruction_path(path: &str) -> bool {
    let Some((directory, name)) = path.rsplit_once('/') else {
        return valid_instruction_file(path);
    };

    !directory.is_empty()
        && valid_instruction_file(name)
        && directory.split('/').all(|component| {
            !component.is_empty()
                && !component.contains("..")
                && component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        })
}

/// The instruction files to actually fetch, from a configured list.
///
/// Invalid names are dropped rather than fatal: one bad entry in a repository's
/// config must not cost it the whole review, and the alternative — failing —
/// hands any contributor a way to break the bot by editing one line. Duplicates
/// collapse, and the result is capped at [`MAX_CONFIGURED_INSTRUCTION_FILES`].
pub fn selected_instruction_files(configured: &[String]) -> Vec<String> {
    let mut selected: Vec<String> = Vec::new();
    for name in configured {
        let name = name.trim();
        if !valid_instruction_file(name) {
            tracing::warn!(%name, "ignoring an instruction filename that is not a plain filename");
            continue;
        }
        if !selected.iter().any(|existing| existing == name) {
            selected.push(name.to_string());
        }
        if selected.len() == MAX_CONFIGURED_INSTRUCTION_FILES {
            break;
        }
    }
    selected
}

/// Expand each configured filename across every changed path's ancestors.
///
/// The root is always included. A file such as `src/bin/main.rs` therefore
/// considers both `AGENTS.md` and `src/AGENTS.md`; the latter is what lets a
/// directory scope its own review policy. The final cap bounds forge reads and
/// model work even for a pull request touching a very wide tree.
pub fn scoped_instruction_files(configured: &[String], changed_paths: &[String]) -> Vec<String> {
    let names = selected_instruction_files(configured);
    let mut paths = std::collections::BTreeSet::new();

    for name in &names {
        paths.insert(name.clone());
    }
    for changed_path in changed_paths {
        let components: Vec<_> = changed_path.split('/').collect();
        if components.len() < 2
            || components.iter().any(|component| {
                component.is_empty()
                    || *component == "."
                    || *component == ".."
                    || component.contains("..")
            })
        {
            continue;
        }
        for depth in 1..components.len() {
            let directory = components[..depth].join("/");
            for name in &names {
                paths.insert(format!("{directory}/{name}"));
            }
        }
    }

    paths.into_iter().take(MAX_INSTRUCTION_FILES).collect()
}

/// One instruction file as fetched, before extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// The repository-root filename it came from.
    pub path: String,
    /// The content, already truncated to the configured byte limit.
    pub content: String,
    /// Hash of `content`, and the key of the extraction cache.
    pub content_hash: String,
}

/// What the knowledge centre contributes to a review.
///
/// The two halves land in different halves of the prompt and that separation is
/// the point. Curated documents are written by operators through the admin API
/// and change only when someone edits them, so they belong in the cacheable
/// prefix. Extracted rules come from files the pull request author controls and
/// change per branch, so they belong in the volatile suffix, fenced and
/// labelled — putting them in the prefix would both destroy the prompt cache
/// and give attacker-controlled text the most authoritative position in the
/// conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewKnowledge {
    /// Pinned curated documents, rendered under the configured caps.
    pub pinned: String,
    /// Rules extracted from the repository's own instruction files.
    pub extracted_rules: Vec<String>,
}

impl ReviewKnowledge {
    /// The pinned text, or `None` when there is none.
    pub fn pinned_text(&self) -> Option<&str> {
        (!self.pinned.trim().is_empty()).then_some(self.pinned.as_str())
    }

    /// Whether the centre found nothing at all.
    pub fn is_empty(&self) -> bool {
        self.pinned.trim().is_empty() && self.extracted_rules.is_empty()
    }
}

/// The document id for a slug within a scope.
///
/// One function so the admin API and any future reader address the same
/// document. The scope is part of the id because the id is the primary key: two
/// organisations both calling a document `conventions` must not collide.
pub fn doc_id(scope: &KnowledgeScope, slug: &str) -> String {
    match scope {
        KnowledgeScope::Org { org } => format!("org:{org}:{slug}"),
        KnowledgeScope::Repo { repo_id } => format!("repo:{repo_id}:{slug}"),
    }
}

/// Whether `slug` is an acceptable document name.
///
/// The slug becomes part of a MongoDB `_id`, so an empty or unbounded one is a
/// way to write junk into the collection through an otherwise valid request.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && !slug.contains("..")
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filename_containing_a_path_separator_is_rejected() {
        // The whole point of the allow-list: a config entry must not be able to
        // name a path, only a repository-root file.
        assert!(!valid_instruction_file(".github/AGENTS.md"));
        assert!(!valid_instruction_file("/etc/passwd"));
        assert!(valid_instruction_file("AGENTS.md"));
    }

    #[test]
    fn a_filename_containing_dot_dot_is_rejected() {
        assert!(!valid_instruction_file("..AGENTS.md"));
        assert!(!valid_instruction_file(".."));
        assert!(!valid_instruction_file("a..b"));
    }

    #[test]
    fn an_empty_or_oversized_filename_is_rejected() {
        assert!(!valid_instruction_file(""));
        assert!(!valid_instruction_file(&"a".repeat(MAX_FILENAME_LEN + 1)));
        assert!(valid_instruction_file(&"a".repeat(MAX_FILENAME_LEN)));
    }

    #[test]
    fn exotic_characters_are_rejected() {
        assert!(!valid_instruction_file("AGENTS.md\0"));
        assert!(!valid_instruction_file("AGENTS md"));
        assert!(!valid_instruction_file("AGENTS.md?ref=evil"));
        assert!(!valid_instruction_file("AGENTS\u{202e}.md"));
    }

    #[test]
    fn selection_drops_bad_names_rather_than_failing() {
        // One bad entry must not cost the repository its whole review, and it
        // must not be a way for a contributor to break the bot.
        let configured = vec![
            "AGENTS.md".to_string(),
            "../secrets".to_string(),
            "CLAUDE.md".to_string(),
        ];
        assert_eq!(
            selected_instruction_files(&configured),
            vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()]
        );
    }

    #[test]
    fn selection_collapses_duplicates_and_caps_the_count() {
        let configured: Vec<String> = std::iter::repeat_n("AGENTS.md".to_string(), 3)
            .chain((0..10).map(|i| format!("F{i}.md")))
            .collect();
        let selected = selected_instruction_files(&configured);
        assert_eq!(selected.len(), MAX_CONFIGURED_INSTRUCTION_FILES);
        assert_eq!(selected[0], "AGENTS.md");
        assert_eq!(selected.iter().filter(|n| *n == "AGENTS.md").count(), 1);
    }

    #[test]
    fn instruction_paths_include_changed_file_ancestors() {
        let paths = scoped_instruction_files(
            &["AGENTS.md".to_string()],
            &["src/bin/tinysweeper.rs".to_string()],
        );
        assert_eq!(
            paths,
            vec![
                "AGENTS.md".to_string(),
                "src/AGENTS.md".to_string(),
                "src/bin/AGENTS.md".to_string(),
            ]
        );
    }

    #[test]
    fn scoped_instruction_paths_do_not_accept_unsafe_components() {
        assert!(valid_instruction_path("src/AGENTS.md"));
        assert!(!valid_instruction_path("src/../AGENTS.md"));
        assert!(!valid_instruction_path("/AGENTS.md"));
    }

    #[test]
    fn a_document_id_carries_its_scope() {
        assert_eq!(
            doc_id(&KnowledgeScope::org("acme"), "style"),
            "org:acme:style"
        );
        assert_eq!(
            doc_id(&KnowledgeScope::repo("acme/app"), "style"),
            "repo:acme/app:style"
        );
        assert_ne!(
            doc_id(&KnowledgeScope::org("acme"), "style"),
            doc_id(&KnowledgeScope::org("other"), "style")
        );
    }

    #[test]
    fn slugs_are_checked_before_they_reach_the_database() {
        assert!(valid_slug("house-style"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("../../etc"));
        assert!(!valid_slug(&"a".repeat(65)));
    }

    #[test]
    fn empty_knowledge_reports_itself_as_empty() {
        let knowledge = ReviewKnowledge::default();
        assert!(knowledge.is_empty());
        assert!(knowledge.pinned_text().is_none());
    }
}
