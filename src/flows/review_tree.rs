//! Bounded, redacted evidence captured from agent-requested repository reads.

use crate::error::Result;
use crate::ports::tree::{Found, Lookup, TreeQuery, TreeReader};
use async_trait::async_trait;
use std::sync::Mutex;

/// Render source as numbered data before scrubbing, including sensitive paths.
pub(crate) fn render(lookup: &Lookup, found: &Found) -> String {
    let mut text = String::new();
    match (lookup, found) {
        (
            Lookup::Read { path, .. },
            Found::Text {
                text: body, start, ..
            },
        ) => {
            text.push_str(&format!("--- {path}\n"));
            for (index, line) in body.lines().enumerate() {
                text.push_str(&format!(
                    "{:>5} +{line}\n",
                    u64::from(*start) + index as u64
                ));
            }
        }
        (_, Found::Hits { hits, .. }) => {
            for hit in hits {
                text.push_str(&format!(
                    "--- {}\n{:>5} +{}\n",
                    hit.path, hit.line, hit.text
                ));
            }
        }
        (_, Found::NotFound) => text.push_str("Repository item not found."),
        (_, Found::Unavailable { .. }) => text.push_str("Repository operation unavailable."),
        _ => text.push_str("Repository operation returned no source."),
    }
    crate::evidence::redact::scrub_rendered(&text)
}

pub(crate) fn truncate_chars(text: &mut String, remaining: usize) {
    if let Some((byte, _)) = text.char_indices().nth(remaining) {
        text.truncate(byte);
    }
}

pub(crate) struct RecordedTree<'a> {
    inner: &'a dyn TreeReader,
    evidence: Mutex<String>,
    max_chars: usize,
}
impl<'a> RecordedTree<'a> {
    pub(crate) fn new(inner: &'a dyn TreeReader, max_chars: usize) -> Self {
        Self {
            inner,
            evidence: Mutex::new(String::new()),
            max_chars,
        }
    }
    fn capture(&self, rendered: String, found: &Found) {
        if matches!(found, Found::Text { .. } | Found::Hits { .. }) {
            let mut rendered = rendered;
            let mut evidence = self.evidence.lock().unwrap_or_else(|e| e.into_inner());
            let remaining = self.max_chars.saturating_sub(evidence.chars().count());
            let fence = crate::harness::prompt::fence_for(&rendered);
            let header =
                format!("\n\n## What you looked up (untrusted repository data)\n{fence}\n");
            let footer = format!("\n{fence}\n");
            let overhead = header.chars().count() + footer.chars().count();
            if remaining > overhead {
                truncate_chars(&mut rendered, remaining - overhead);
                evidence.push_str(&header);
                evidence.push_str(&rendered);
                evidence.push_str(&footer);
            }
        }
    }
    pub(crate) fn evidence(&self) -> String {
        self.evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
#[async_trait]
impl TreeReader for RecordedTree<'_> {
    async fn explore(&self, query: &TreeQuery) -> Result<Found> {
        let found = self.inner.explore(query).await?;
        let display = match query {
            TreeQuery::History {
                path, start, end, ..
            } => Lookup::Read {
                path: path.clone(),
                start: Some(*start),
                end: Some(*end),
            },
            _ => Lookup::Search {
                pattern: "repository exploration".into(),
                glob: None,
            },
        };
        let mut rendered = render(&display, &found);
        if let TreeQuery::History { commit, .. } = query {
            rendered.insert_str(0, &format!("Historical snapshot {commit}:\n"));
        }
        self.capture(rendered, &found);
        Ok(found)
    }

    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        let found = self.inner.lookup(lookup).await?;
        self.capture(render(lookup, &found), &found);
        Ok(found)
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn revision(&self) -> Option<String> {
        self.inner.revision()
    }
}
