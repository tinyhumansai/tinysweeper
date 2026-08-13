//! Driving a panel to a set of findings.
//!
//! Three rounds, and the order is the design:
//!
//! 1. **Propose** — every lens over the evidence, concurrently. Each may end
//!    its turn with questions rather than a guess.
//! 2. **Answer** — those questions, one sub-agent each, one level deep.
//! 3. **Verify** — every proposal put to independent judges, with the answers
//!    in hand. A majority keeps it; anything less drops it.
//!
//! A round that fails is not a round that returns nothing. A lens whose call
//! errors is recorded and the panel continues with the rest, because a panel
//! that fails whole because one member timed out is a review that reports
//! "all clear" for an infrastructure reason. What is *not* tolerated is a
//! proposal reaching the output unverified — see `consensus::settle`.

use std::sync::Arc;

use serde_json::{Value, json};
use tinyflows::engine;
use tinyflows::model::WorkflowGraph;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::flows::caps::{ChildGraphs, ModelCapability};
use crate::flows::consensus::{self, Opinion, Verdict};
use crate::flows::panel::{self, Lens};
use crate::flows::subagent::{self, Answered};
use crate::harness::schema::{LaneResponse, RawFinding};
use crate::ports::model::{Model, Spend};

/// What one panel produced.
#[derive(Debug, Default)]
pub struct PanelOutcome {
    /// The findings that survived verification.
    pub findings: Vec<RawFinding>,
    /// Earlier findings the panel agreed were fixed.
    pub resolved: Vec<String>,
    /// One sentence, taken from the panel.
    pub summary: String,
    /// What every call in every round cost.
    pub spend: Spend,
    /// Lenses whose call failed, with why. Reported rather than swallowed: a
    /// partial review that reads like a complete one is worse than none.
    pub failures: Vec<(String, String)>,
    /// How many panellists produced a usable answer.
    ///
    /// Zero is the case a caller must not treat as a clean review — see
    /// [`PanelOutcome::nothing_was_read`].
    pub read: usize,
}

impl PanelOutcome {
    /// A sentence naming what could not be read, or nothing.
    ///
    /// Appended to every lane's summary. A partial review that reads like a
    /// complete one is worse than no review at all, because a human stops
    /// looking — so a lost panellist is stated rather than absorbed.
    /// Whether the panel produced no usable reading at all.
    ///
    /// The distinction a caller has to preserve: a panel that read the evidence
    /// and found nothing is a clean review, and a panel where every member
    /// failed is *no review*. Both leave `findings` empty, so a lane that does
    /// not check this reports a file nobody looked at as a file with nothing
    /// wrong — and that is what branch protection would then approve.
    pub fn nothing_was_read(&self) -> bool {
        self.read == 0 && !self.failures.is_empty()
    }

    pub fn failure_note(&self) -> String {
        if self.failures.is_empty() {
            return String::new();
        }

        let names: Vec<&str> = self.failures.iter().map(|(who, _)| who.as_str()).collect();

        format!(
            " {} reader{} could not be consulted: {}.",
            self.failures.len(),
            if self.failures.len() == 1 { "" } else { "s" },
            names.join(", ")
        )
    }
}

/// Everything a panel needs that is not the graph.
pub struct PanelRequest<'a> {
    /// Which lane's lenses to run.
    pub lane: LaneId,
    /// The lane response schema every lens answers under.
    pub schema: Value,
    /// The volatile half of the prompt: the evidence itself.
    pub suffix: &'a str,
    /// Builds the cacheable prefix for one lens. Called once per lens.
    ///
    /// `Sync` because the panel is held across an await inside a lane whose
    /// future must be `Send`.
    pub system_of: &'a (dyn Fn(&Lens) -> String + Sync),
}

/// Read one agent node's structured answer out of a finished run.
///
/// Two envelopes, not one, and the difference is easy to get wrong in a way
/// nothing reports. The engine wraps a node's result as
/// `nodes.<id>.items[0].{json, raw, text}`, and the `json` there is whatever
/// [`crate::flows::caps::ModelCapability`] returned — which is this crate's own
/// `{json, model}` pair. So the model's structured answer is two `json` hops
/// down, and stopping one hop early yields `{json, model}`, which deserializes
/// into an *empty* [`LaneResponse`] rather than failing. That reads exactly
/// like a panellist that found nothing.
fn node_answer(output: &Value, node_id: &str) -> Option<(Value, String)> {
    let envelope = output.get("nodes")?.get(node_id)?.get("items")?.get(0)?;
    let payload = envelope.get("json")?.get("json")?;

    Some((
        payload.get("json")?.clone(),
        payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    ))
}

/// Run one graph against `capabilities`.
///
/// The engine's error is mapped to this crate's, losing nothing: a capability
/// error already carries the model or budget message that produced it.
async fn run_graph(
    graph: &WorkflowGraph,
    capabilities: &tinyflows::caps::Capabilities,
) -> Result<Value> {
    let compiled = tinyflows::compiler::compile(graph)
        .map_err(|e| crate::error::Error::Model(format!("graph did not compile: {e}")))?;

    let outcome = engine::run(&compiled, json!({}), capabilities)
        .await
        .map_err(|e| crate::error::Error::Model(e.to_string()))?;

    Ok(outcome.output)
}

/// The model capability a whole lane shares.
///
/// A per-file lane builds one of these and hands it to every file's panel, so
/// the budget is a *lane* ceiling rather than a per-file one. That is what lets
/// files run concurrently: the previous design serialised them precisely
/// because there was nowhere else to enforce the limit.
pub fn lane_llm(
    model: Arc<dyn Model>,
    config: &crate::config::types::Config,
    budget_usd: f64,
) -> Arc<ModelCapability> {
    Arc::new(ModelCapability::new(model, config.models.clone()).with_budget(budget_usd))
}

/// Run a full panel: propose, answer, verify.
pub async fn run(
    model: Arc<dyn Model>,
    config: &crate::config::types::Config,
    budget_usd: f64,
    request: PanelRequest<'_>,
) -> PanelOutcome {
    run_with_llm(lane_llm(model, config, budget_usd), request).await
}

/// Run a panel against a capability the caller owns.
///
/// The three rounds share it, so the budget is enforced across all of them —
/// three rounds each staying under the ceiling would spend three times it.
pub async fn run_with_llm(llm: Arc<ModelCapability>, request: PanelRequest<'_>) -> PanelOutcome {
    let lenses = panel::lenses(request.lane);
    let mut outcome = PanelOutcome::default();

    if lenses.is_empty() {
        return outcome;
    }

    let capabilities = crate::flows::caps::with_llm(llm.clone(), ChildGraphs::none());

    // --- round one: propose -------------------------------------------------
    let propose = panel::propose_graph(
        request.lane,
        request.schema.clone(),
        request.suffix,
        request.system_of,
    );

    let output = match run_graph(&propose, &capabilities).await {
        Ok(output) => output,
        Err(err) => {
            // The whole round failed — a budget refusal on the first call, or a
            // graph that did not compile. There is nothing to verify and
            // nothing to report but the reason.
            outcome.failures.push(("panel".into(), err.to_string()));
            outcome.spend = llm.spend();
            return outcome;
        }
    };

    let mut opinions: Vec<Opinion> = Vec::new();
    let mut questions: Vec<(String, String)> = Vec::new();

    for lens in lenses {
        let node_id = format!("lens_{}", lens.id);

        let Some((value, model_id)) = node_answer(&output, &node_id) else {
            outcome
                .failures
                .push((lens.id.to_string(), "the lens produced no answer".into()));
            continue;
        };

        // A lens that answered off-schema is dropped, not guessed at. The panel
        // has other members, and a malformed answer is exactly the case the
        // strict schema exists to catch.
        match crate::harness::schema::parse(request.lane, value.clone()) {
            Ok(response) => {
                for question in read_questions(&value) {
                    questions.push((lens.id.to_string(), question));
                }
                opinions.push(Opinion {
                    lens: lens.id.to_string(),
                    model: model_id,
                    response,
                });
            }
            Err(err) => outcome
                .failures
                .push((lens.id.to_string(), err.to_string())),
        }
    }

    outcome.read = opinions.len();

    if opinions.is_empty() {
        outcome.spend = llm.spend();
        outcome.summary = "No panellist produced a usable answer.".into();
        return outcome;
    }

    // --- round two: answer the panel's questions -----------------------------
    let answers = answer_questions(&questions, request.suffix, &capabilities).await;

    // --- round three: verify -------------------------------------------------
    let proposals = consensus::propose(&opinions);
    let mut verdicts: Vec<Vec<Verdict>> = Vec::with_capacity(proposals.len());

    for proposal in &proposals {
        let graph = panel::verify_graph(
            request.lane,
            verdict_schema(),
            VERIFY_SYSTEM,
            &verify_prompt(&proposal.finding, request.suffix, &answers),
        );

        match run_graph(&graph, &capabilities).await {
            Ok(output) => verdicts.push(read_verdicts(&output)),
            // A verify round that could not run leaves the proposal with no
            // votes, and `settle` drops an unverified proposal. That is the
            // conservative direction: the failure suppresses a finding rather
            // than publishing an unchecked one.
            Err(err) => {
                outcome
                    .failures
                    .push((proposal.finding.title.clone(), err.to_string()));
                verdicts.push(Vec::new());
            }
        }
    }

    outcome.read = opinions.len();
    outcome.resolved = consensus::resolved(&opinions);
    outcome.summary = summarize(&opinions);
    outcome.findings = consensus::settle(proposals, &verdicts);
    outcome.spend = llm.spend();
    outcome
}

/// Dispatch each question to its own sub-agent.
async fn answer_questions(
    questions: &[(String, String)],
    evidence: &str,
    capabilities: &tinyflows::caps::Capabilities,
) -> Vec<Answered> {
    let mut answers = Vec::new();

    for (_lens, question) in questions {
        let graph = subagent::answer_graph(
            subagent::ANSWER_SYSTEM,
            &format!("{evidence}\n\nThe question:\n{question}\n"),
        );

        // A question that could not be answered is simply not answered. It was
        // a request for extra confidence; failing to get it leaves the verify
        // round exactly where it would have been without sub-agents at all.
        if let Ok(output) = run_graph(&graph, capabilities).await
            && let Some((value, _)) = node_answer(&output, "answer")
        {
            answers.push(Answered {
                question: question.clone(),
                answer: value
                    .get("answer")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                confident: value
                    .get("confident")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }

    answers
}

/// The questions a lens asked, capped.
///
/// The cap is applied here as well as in the schema: a schema is a request and
/// a provider may honour it loosely, while this is the number of sub-agents
/// that actually get spawned.
fn read_questions(value: &Value) -> Vec<String> {
    value
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|q| q.get("question").and_then(Value::as_str))
                .filter(|q| !q.trim().is_empty())
                .take(subagent::MAX_QUESTIONS_PER_LENS)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// What a verifier answers.
fn verdict_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["real", "why"],
        "properties": {
            "real": {
                "type": "boolean",
                "description": "True only if this finding describes a real problem in the diff \
                                shown. False if it is speculative, already handled elsewhere in \
                                the evidence, or about code the diff did not change."
            },
            "why": {
                "type": "string",
                "description": "One sentence. Cite the line that decides it."
            }
        }
    })
}

/// The system prompt every verifier answers under.
///
/// It asks for refutation rather than assessment, and the asymmetry is
/// deliberate. A model asked "is this right?" agrees; a model asked "can you
/// show this is wrong?" goes and looks. The default when a verifier cannot tell
/// is `false`, which drops the finding — because the cost of a review that
/// argues with a contributor about something imaginary is far higher than the
/// cost of missing one finding.
pub const VERIFY_SYSTEM: &str = "\
You are checking one claim another reviewer made about a diff. Try to refute \
it. Set `real` to true only if you cannot — if the diff shown genuinely has \
the problem described, in the code the pull request changed. Set it to false \
if the claim is speculative, if the evidence does not show it, if the concern \
is already handled somewhere in the evidence, or if it is about code this diff \
did not touch. If you cannot tell either way, set it to false: an unverifiable \
claim must not reach a contributor as a review comment.";

/// The prompt one verifier sees.
fn verify_prompt(finding: &RawFinding, evidence: &str, answers: &[Answered]) -> String {
    format!(
        "{evidence}\n\nThe claim to check:\n\
         - file: {path}\n\
         - rule: {rule}\n\
         - title: {title}\n\
         - reasoning: {body}\n\
         - the code it points at:\n{code}\n{answered}",
        path = finding.path,
        rule = finding.rule,
        title = finding.title,
        body = finding.body,
        code = finding.existing_code.as_deref().unwrap_or("(not quoted)"),
        answered = subagent::render(answers),
    )
}

/// Read one verify round's votes.
fn read_verdicts(output: &Value) -> Vec<Verdict> {
    (0..panel::VERIFIERS)
        .filter_map(|n| node_answer(output, &format!("verifier_{n}")))
        .filter_map(|(value, _)| value.get("real").and_then(Value::as_bool))
        .map(|real| Verdict { real })
        .collect()
}

/// One sentence for the check-run summary.
///
/// Taken from the panellist that had the most to say rather than concatenated:
/// three summaries of one file read as three reviews of three files.
fn summarize(opinions: &[Opinion]) -> String {
    opinions
        .iter()
        .map(|o| o.response.summary.trim())
        .filter(|s| !s.is_empty())
        .max_by_key(|s| s.len())
        .unwrap_or_default()
        .to_string()
}

/// Add the questions key to a lane's response schema.
///
/// Optional, so a lens with nothing to ask answers the schema it always did and
/// every existing golden response still validates.
pub fn schema_with_questions(mut schema: Value) -> Value {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("questions".into(), subagent::questions_schema());
    }
    schema
}

/// Compose a lane's system prompt with one lens's charter.
pub fn system_with_charter(prefix: &str, lens: &Lens) -> String {
    format!(
        "{prefix}\n\nYour assignment on this panel:\n{}\n\nOther reviewers are reading the same \
         evidence for different things. Report only what your assignment covers — a problem \
         outside it is someone else's to find, and reporting it twice is worse than not \
         reporting it here. If something would change your verdict and the evidence does not \
         settle it, ask it in `questions` rather than guessing.",
        lens.charter
    )
}

/// The response a lane's panel produces, as a [`LaneResponse`].
///
/// Kept as a distinct step so a lane's own filtering pipeline — anchoring,
/// severity gates, capping — receives exactly the shape it received from a
/// single model call before.
pub fn into_response(outcome: &PanelOutcome) -> LaneResponse {
    LaneResponse {
        summary: outcome.summary.clone(),
        findings: outcome.findings.clone(),
        resolved: outcome.resolved.clone(),
    }
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod tests;
