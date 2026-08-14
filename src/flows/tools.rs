//! The tools a sub-agent may call, and everything they are not allowed to do.
//!
//! Always compiled. A reviewer that needs more than the diff can ask a question
//! (see [`crate::flows::subagent`]); the sub-agent answering that question is
//! the only thing in this crate with tools, and these are they:
//!
//! | slug | answers |
//! |---|---|
//! | `read_file` | "show me the rest of this file" |
//! | `search` | "does this appear anywhere else" |
//!
//! Both read. Neither writes, and there is nothing else on the list — see
//! [`ReadOnlyTools::invoke`] for what an unrecognised slug gets.
//!
//! # Why a tool loop needs its own limits
//!
//! Every other cost control in this crate counts *model calls*: the budget in
//! [`crate::flows::caps::ModelCapability`], the question cap in
//! `subagent`, the round cap in `runner`. A tool call is not a model call and
//! costs nothing to make — but what it returns is pasted into the next prompt,
//! so a loop that reads twenty files bills for twenty files of input tokens on
//! every subsequent turn. The spend tally would show it only after the fact.
//!
//! So the limits here are on **bytes returned**, per call and per sub-agent,
//! and they are enforced in this file rather than by the caller. A truncated
//! result says so in the text, because a reviewer that cannot tell a short file
//! from a truncated one will conclude something false about the end of it.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::error::{EngineError, Result as FlowResult};

use crate::ports::corpus::Corpus;

/// Most bytes one `read_file` may return.
///
/// A whole file is the point of the tool, so this is generous. It exists for
/// the vendored dependency or the generated bundle a reviewer asks for by
/// accident, not to keep ordinary source out.
pub const MAX_READ_BYTES: usize = 24_000;

/// Most bytes one sub-agent may accumulate across every tool call it makes.
///
/// The load-bearing limit. Per-call caps bound one paste; only this bounds the
/// conversation, and the conversation is what gets re-sent every turn.
pub const MAX_TOTAL_BYTES: usize = 60_000;

/// Most hits one `search` may return.
pub const MAX_HITS: usize = 40;

/// The slugs offered, in the order they are described to a model.
pub const SLUGS: [&str; 2] = ["read_file", "search"];

/// What a model is told it may call.
///
/// Offered in the `agent` node's `tools` config, which is also what the engine
/// checks a returned `tool_call` against — a slug that is not in this list is
/// never invoked, whatever the model asks for.
pub fn descriptors() -> Value {
    json!([
        {
            "slug": "read_file",
            "description": "Read a file from the repository at the revision under review. \
                            Args: {\"path\": \"repository-relative path\"}.",
        },
        {
            "slug": "search",
            "description": "Find lines containing a literal string, across the repository. \
                            Not a regular expression. Args: {\"pattern\": \"literal text\"}.",
        },
    ])
}

/// Tools over a [`Corpus`], with a byte budget shared across every call.
///
/// One per sub-agent rather than one per lane: the budget is what stops a
/// single question reading the repository, and a lane-wide one would let the
/// first question spend every later question's allowance.
pub struct ReadOnlyTools<'a> {
    corpus: &'a dyn Corpus,
    spent_bytes: Mutex<usize>,
}

impl<'a> ReadOnlyTools<'a> {
    /// Grant `corpus` to one sub-agent.
    pub fn new(corpus: &'a dyn Corpus) -> Self {
        Self {
            corpus,
            spent_bytes: Mutex::new(0),
        }
    }

    /// Bytes returned so far, across every call.
    ///
    /// A poisoned lock reads as exhausted rather than as zero: losing the tally
    /// must fail toward spending nothing, not toward spending without limit.
    fn spent(&self) -> usize {
        self.spent_bytes
            .lock()
            .map(|n| *n)
            .unwrap_or(MAX_TOTAL_BYTES)
    }

    /// Clip `text` to what is left of both budgets, and say so if it was cut.
    ///
    /// The marker is not decoration. A reviewer shown the first 24kB of a file
    /// with no indication has been told, in effect, that the file ends there,
    /// and "the cleanup is missing" is exactly the sort of finding that gets
    /// invented from a truncated read.
    fn clip(&self, text: String) -> String {
        let remaining = MAX_TOTAL_BYTES.saturating_sub(self.spent());
        let allowed = remaining.min(MAX_READ_BYTES);

        let clipped = if text.len() > allowed {
            // On a character boundary, because a byte-sliced UTF-8 string does
            // not serialize and would fail the call rather than shorten it.
            let mut end = allowed;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}\n\n[truncated: {} bytes not shown]",
                &text[..end],
                text.len() - end
            )
        } else {
            text
        };

        if let Ok(mut spent) = self.spent_bytes.lock() {
            *spent += clipped.len();
        }
        clipped
    }

    /// One argument, as a non-empty string.
    fn arg<'a>(args: &'a Value, key: &str) -> FlowResult<&'a str> {
        let value = args
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();

        if value.is_empty() {
            return Err(EngineError::Capability(format!(
                "this tool needs a non-empty string `{key}`"
            )));
        }
        Ok(value)
    }

    /// Reject a path a reviewer must not be able to reach.
    ///
    /// The arguments come from a model that has just read a pull request body,
    /// which is untrusted input, so `../../.ssh/id_rsa` is a thing that gets
    /// asked for. A [`Corpus`] over a forge confines by construction, but one
    /// over a working tree joins the path onto a directory, so the check lives
    /// here — in front of every implementation — rather than in the one that
    /// happens to need it.
    fn safe_path(path: &str) -> FlowResult<&str> {
        let rejected = path.starts_with('/')
            || path.starts_with('~')
            || path.contains(':')
            || std::path::Path::new(path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));

        if rejected {
            return Err(EngineError::Capability(format!(
                "`{path}` is not a repository-relative path; \
                 a review reads inside the repository under review and nowhere else"
            )));
        }
        Ok(path)
    }
}

#[async_trait]
impl ToolInvoker for ReadOnlyTools<'_> {
    /// Invoke one tool by slug.
    ///
    /// An unrecognised slug is an error naming what is available rather than a
    /// silent empty result: a model that asked for `run_tests` and got `{}`
    /// back learns nothing, and may well conclude the tests passed.
    async fn invoke(&self, slug: &str, args: Value, _conn: Option<&str>) -> FlowResult<Value> {
        if self.spent() >= MAX_TOTAL_BYTES {
            return Ok(json!({
                "error": "no tool budget left for this question; \
                          answer from what you have already read",
            }));
        }

        match slug {
            "read_file" => {
                let path = Self::safe_path(Self::arg(&args, "path")?)?;

                match self
                    .corpus
                    .read(path)
                    .await
                    .map_err(|e| EngineError::Capability(e.to_string()))?
                {
                    Some(content) => Ok(json!({ "path": path, "content": self.clip(content) })),
                    // Not an error. A reviewer guessing at a filename is the
                    // ordinary case and it should guess again, not give up.
                    None => Ok(json!({ "path": path, "error": "no such file at this revision" })),
                }
            }

            "search" => {
                let pattern = Self::arg(&args, "pattern")?;

                match self
                    .corpus
                    .search(pattern, MAX_HITS)
                    .await
                    .map_err(|e| EngineError::Capability(e.to_string()))?
                {
                    Some(hits) => {
                        let rendered: Vec<Value> = hits
                            .iter()
                            .map(|h| json!({ "path": h.path, "line": h.line, "text": h.text }))
                            .collect();

                        // Counted against the same budget as a read: a search
                        // that matches every line is a file paste with extra
                        // steps.
                        let text = self.clip(serde_json::to_string(&rendered).unwrap_or_default());
                        Ok(json!({ "pattern": pattern, "hits": text }))
                    }
                    // Distinct from no hits, and said in words, because the two
                    // support opposite conclusions.
                    None => Ok(json!({
                        "pattern": pattern,
                        "error": "search is not available for this review; \
                                  do not conclude anything from the absence of matches",
                    })),
                }
            }

            _ => Err(EngineError::Capability(format!(
                "`{slug}` is not a tool a review may call; available: {}",
                SLUGS.join(", ")
            ))),
        }
    }
}

#[cfg(test)]
#[path = "tools_test.rs"]
mod tests;
