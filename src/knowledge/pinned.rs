//! Rendering pinned knowledge documents into prompt text, under caps.
//!
//! Always compiled.
//!
//! Pinned documents bypass retrieval and go into every prompt, which is what
//! makes them useful and what makes them expensive: a convention nobody
//! retrieved is a convention nobody applied, but a pinned document is paid for
//! on every review of every pull request in scope. Two caps keep that bounded —
//! one per document, so a single runbook cannot crowd out the diff, and one
//! across all of them, so an organisation that pins twenty documents does not
//! silently spend its whole context on them.
//!
//! Over-cap documents are **truncated with a visible marker** rather than
//! dropped. A dropped pinned document is invisible: the operator who pinned it
//! sees a review that ignored it and concludes the feature does not work. A
//! truncated one shows up in the prompt saying it was truncated.

use std::fmt::Write as _;

use crate::index::types::KnowledgeDoc;

/// What is appended where a document was cut.
const TRUNCATED: &str = "\n… [truncated: this document is longer than the per-document cap]";

/// Render pinned documents into one block of prompt text.
///
/// `per_doc` caps each document's body and `total` caps the whole block, both
/// in characters. A zero cap disables the block entirely, which is how a
/// deployment turns pinned injection off without a second flag.
///
/// Ordering is by document id, not by whatever order the store returned. The
/// block lands in the cacheable prefix, so a set of documents that renders
/// differently between two runs would cost a prompt-cache hit for no reason.
pub fn render(docs: &[KnowledgeDoc], per_doc: usize, total: usize) -> String {
    if per_doc == 0 || total == 0 {
        return String::new();
    }

    let mut ordered: Vec<&KnowledgeDoc> = docs.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let mut out = String::new();
    for doc in ordered {
        let body = doc.body.trim();
        if body.is_empty() {
            continue;
        }

        let mut rendered = String::new();
        let _ = write!(
            rendered,
            "### {}\n\n{}\n\n",
            doc.title.trim(),
            cap(body, per_doc)
        );

        // Stop at the first document that would breach the total, rather than
        // skipping it and trying the next: a deterministic prefix is worth more
        // than squeezing one more short document in, and "documents after this
        // one were dropped" is a rule an operator can reason about.
        if out.chars().count() + rendered.chars().count() > total {
            out.push_str("… [truncated: the pinned document set is larger than the total cap]\n");
            break;
        }
        out.push_str(&rendered);
    }

    out
}

/// Truncate `body` to `limit` characters, marking the cut.
fn cap(body: &str, limit: usize) -> String {
    if body.chars().count() <= limit {
        return body.to_string();
    }
    let kept: String = body.chars().take(limit).collect();
    format!("{kept}{TRUNCATED}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::types::KnowledgeScope;

    fn doc(id: &str, title: &str, body: &str) -> KnowledgeDoc {
        KnowledgeDoc::new(id, KnowledgeScope::org("acme"), title, body).pinned()
    }

    #[test]
    fn a_document_is_rendered_with_its_title() {
        let out = render(&[doc("a", "House style", "Never unwrap.")], 100, 1000);
        assert!(out.contains("### House style"));
        assert!(out.contains("Never unwrap."));
    }

    #[test]
    fn an_oversized_document_is_truncated_rather_than_dropped() {
        // Dropping it would be invisible to the operator who pinned it.
        let out = render(&[doc("a", "Runbook", &"x".repeat(500))], 100, 10_000);
        assert!(out.contains("truncated"));
        assert!(out.matches('x').count() <= 100);
        assert!(out.contains("### Runbook"));
    }

    #[test]
    fn the_total_cap_stops_the_block_growing_without_bound() {
        let docs: Vec<KnowledgeDoc> = (0..20)
            .map(|i| doc(&format!("d{i:02}"), "T", &"y".repeat(200)))
            .collect();
        let out = render(&docs, 200, 600);
        assert!(
            out.chars().count() <= 700,
            "got {} chars",
            out.chars().count()
        );
        assert!(out.contains("larger than the total cap"));
    }

    #[test]
    fn rendering_is_stable_regardless_of_store_order() {
        // The block sits in the cacheable prefix; an unstable order would cost
        // a cache hit on every review for no behavioural difference.
        let a = doc("a", "A", "first");
        let b = doc("b", "B", "second");
        assert_eq!(
            render(&[a.clone(), b.clone()], 100, 1000),
            render(&[b, a], 100, 1000)
        );
    }

    #[test]
    fn a_zero_cap_disables_the_block() {
        assert!(render(&[doc("a", "A", "body")], 0, 1000).is_empty());
        assert!(render(&[doc("a", "A", "body")], 100, 0).is_empty());
    }

    #[test]
    fn an_empty_document_contributes_nothing() {
        assert!(render(&[doc("a", "A", "   ")], 100, 1000).is_empty());
    }
}
