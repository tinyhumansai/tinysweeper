//! Opt-in prompt tracing, behind the `harness` feature.
//!
//! Every model call is otherwise unrecoverable after the fact: the prompt is
//! assembled, sent, and dropped, so "why did the review say that" and "what did
//! the model actually see" have no answer once the process exits. Setting
//! `TINYSWEEPER_PROMPT_LOG` to a directory writes one JSON line per call —
//! prompt, answer, usage, finish reason — which is what makes a bad review
//! reproducible.
//!
//! **Off unless the variable is set, and deliberately so.** A traced line
//! contains the pull request's diff and body verbatim, which is untrusted input
//! and may itself contain a secret the contributor committed. That is fine in a
//! developer's checkout and is not fine on a shared host, so this is an
//! operator's decision rather than a default. Nothing here ever writes the API
//! key: the trace is built from the request and the response, and the key lives
//! on `GatewayModel`, which is never formatted into it.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::json;
use tinyagents::harness::middleware::AgentRun;

use crate::ports::model::ModelRequest;

/// The environment variable naming the directory traces are written to.
pub const TRACE_DIR_ENV: &str = "TINYSWEEPER_PROMPT_LOG";

/// The directory to trace into, read once.
///
/// Read once rather than per call because a review makes many model calls and
/// the answer cannot change mid-process; a `OnceLock` also keeps the "tracing
/// is off" path down to an atomic load.
fn trace_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let raw = std::env::var(TRACE_DIR_ENV).ok()?;
        let dir = PathBuf::from(raw.trim());
        if dir.as_os_str().is_empty() {
            return None;
        }
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "tracing prompts; they contain untrusted pull request text");
                Some(dir)
            }
            Err(err) => {
                tracing::warn!(dir = %dir.display(), %err, "cannot create the prompt trace directory; tracing stays off");
                None
            }
        }
    })
    .as_ref()
}

/// Appends one call — prompt, answer, usage — to the trace, when tracing is on.
///
/// Never fails the call it is tracing: a trace that cannot be written is a
/// warning, because losing a review over a full disk in a debugging aid would
/// be worse than losing the trace.
pub fn record(model: &str, cap: u32, request: &ModelRequest, run: &AgentRun) {
    let Some(dir) = trace_dir() else {
        return;
    };

    let totals = run.usage.usage;
    let line = json!({
        "model": model,
        "requested_model": request.model,
        "max_tokens": cap,
        "schema_name": request.schema_name,
        "messages": request
            .messages
            .iter()
            .map(|message| json!({
                "role": format!("{:?}", message.role).to_lowercase(),
                "content": message.content,
            }))
            .collect::<Vec<_>>(),
        "finish_reason": run
            .final_response
            .as_ref()
            .and_then(|response| response.finish_reason.clone()),
        // The raw text as well as the parsed value: a truncated or unparsable
        // answer has no structured output at all, and that is exactly the case
        // a trace is being read for.
        "text": run.final_response.as_ref().map(|response| response.text()),
        "structured": run.structured,
        "usage": {
            "input_tokens": totals.input_tokens,
            "cached_tokens": totals.cache_read_tokens,
            "output_tokens": totals.output_tokens,
            "reasoning_tokens": totals.reasoning_tokens,
        },
    });

    // One file per process: concurrent lanes in one review append to the same
    // file under `O_APPEND`, and two processes never interleave a half-written
    // line into each other's.
    let path = dir.join(format!("prompts-{}.jsonl", std::process::id()));
    let written = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(err) = written {
        tracing::warn!(path = %path.display(), %err, "could not append to the prompt trace");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_is_off_when_the_variable_is_unset() {
        // The guarantee that matters: a deployment that has not opted in writes
        // nothing, so untrusted pull request text never lands on disk by
        // accident. `trace_dir` is a `OnceLock`, so this asserts the shape of
        // the decision rather than mutating the environment under other tests.
        assert!(std::env::var(TRACE_DIR_ENV).is_err());
        assert!(trace_dir().is_none());
    }
}
