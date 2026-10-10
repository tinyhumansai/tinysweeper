//! Running a lane's reviewers concurrently, and reporting who could not be
//! reached.
//!
//! This is the whole of what the runner does for a lane: it makes N model calls
//! at once and hands back one answer per reviewer, in the order they were
//! asked. Placement, merging and removal all stay where they were — in the
//! lane, in `council`, and in `falsify` respectively — because those are the
//! steps whose behaviour the golden tests pin.
//!
//! A reviewer whose call fails is reported, not swallowed. A council that
//! returns nothing because one member timed out is a review that reads "all
//! clear" for an infrastructure reason, which is the failure every layer here
//! is arranged against.

use std::sync::Arc;

use serde_json::Value;

use crate::config::types::{LaneId, LookupPolicy};
use crate::error::Result;
use crate::flows::caps::ModelCapability;
use crate::flows::lookup;
use crate::flows::panel::Call;
use crate::flows::subagent::{self, Answered};
use crate::ports::model::{Model, Usage};
use crate::ports::tree::{Lookup, TreeReader};

/// What one reviewer said, or why it said nothing.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The reviewer's id, as configured.
    pub id: String,
    /// The structured answer, when there was one.
    pub value: Option<Value>,
    /// The model that actually answered. A fallback taking over is worth
    /// knowing about and is otherwise invisible by the time findings merge.
    pub model: String,
    /// Why there was no answer.
    pub error: Option<String>,
    /// What the host read from the repository for this reviewer — seeded
    /// definitions and answered lookups, rendered as the reviewer saw them.
    /// Empty when nothing was looked up. Carried out so the stages after the
    /// review — the falsifier above all — judge the finding against the same
    /// evidence the reviewer had, rather than against the diff alone.
    pub looked_up: String,
    /// Usage reported for the model call that produced this answer.
    usage: Usage,
}

impl Answer {
    /// A reviewer that could not be reached.
    fn failed(id: &str, error: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            value: None,
            model: String::new(),
            error: Some(error.into()),
            looked_up: String::new(),
            usage: Usage::default(),
        }
    }
}

/// Answers and accounting isolated to one invocation of [`ask_all_accounted`].
pub struct AskOutcome {
    /// One answer per requested reviewer, in request order.
    pub answers: Vec<Answer>,
    /// Usage from every successful model call made during this invocation.
    pub usage: Usage,
    /// Wall time for the complete invocation, including follow-up turns.
    pub elapsed: std::time::Duration,
}

/// The capability a whole lane shares.
///
/// One per lane, not one per file: the budget ceiling and the spend tally both
/// live in it, and a fresh one per file would let each file spend the whole
/// pull request's allowance. It is also what makes the per-file fan-out safe to
/// run concurrently — see [`crate::flows::caps::ModelCapability::new`].
pub fn lane_llm(
    model: Arc<dyn Model>,
    config: &crate::config::types::Config,
    budget_usd: f64,
) -> Arc<ModelCapability> {
    Arc::new(ModelCapability::new(model, config.models.clone()).with_budget(budget_usd))
}

/// Ask every call in one round at once, and read an answer per call, in order.
///
/// A reviewer that fails must not fail the round: one provider timeout would
/// otherwise lose every other reviewer's work, and a lane that returns nothing
/// is indistinguishable from a lane that found nothing. So a failure becomes an
/// [`Answer`] carrying the error, and the lane reports that file unreviewed.
///
/// `lane` is carried for the trace only; the calls are identical across lanes.
async fn one_round(
    llm: &ModelCapability,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
) -> Vec<Answer> {
    let results = futures::future::join_all(calls.iter().map(|call| llm.call(call, schema))).await;
    calls
        .iter()
        .zip(results)
        .map(|(call, result)| match result {
            Ok(response) => Answer {
                id: call.id.clone(),
                value: Some(response.value),
                model: response.model,
                error: None,
                looked_up: String::new(),
                usage: response.usage,
            },
            Err(err) => {
                tracing::debug!(lane = lane.as_str(), reviewer = %call.id, %err, "reviewer call failed");
                Answer::failed(&call.id, err.to_string())
            }
        })
        .collect()
}

/// The questions one answer carried, capped.
///
/// The cap is applied here as well as in the schema: a schema is a request, and
/// under `json_object` the provider is not enforcing it at all. This is the
/// number of sub-agents that actually get spawned.
fn read_questions(value: &Value) -> Vec<String> {
    value
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|q| q.get("question").and_then(Value::as_str))
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .take(subagent::MAX_QUESTIONS_PER_REVIEWER)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Answer one reviewer's questions, one sub-agent each, all at once.
///
/// A question that could not be answered is simply absent from the result. It
/// was a request for more certainty; failing to get it leaves the reviewer
/// exactly where it would have been without sub-agents.
async fn answer_questions(
    llm: &ModelCapability,
    lane: LaneId,
    model: &str,
    questions: &[String],
    evidence: &str,
) -> (Vec<Answered>, Usage) {
    let calls: Vec<Call> = questions
        .iter()
        .enumerate()
        .map(|(index, question)| subagent::answer_call(model, index, question, evidence))
        .collect();
    let answers = one_round(llm, lane, &calls, &subagent::answer_schema()).await;

    let mut usage = Usage::default();
    let answered = questions
        .iter()
        .zip(answers)
        .filter_map(|(question, answer)| {
            usage.add(answer.usage);
            let value = answer.value?;
            Some(Answered {
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
            })
        })
        .collect();
    (answered, usage)
}

/// How a lane wants its reviewers asked, beyond the one call.
///
/// Both extras are off when their field is absent, and the prompt is then
/// byte-identical to the plain one — which is what keeps the cassettes of a
/// deployment that has neither valid.
#[derive(Clone, Copy, Default)]
pub struct Asking<'a> {
    /// The model sub-agents answer questions on. `None` disables questions.
    pub subagent_model: Option<&'a str>,
    /// The tree a reviewer may look things up in. `None` disables lookups.
    pub tree: Option<&'a dyn TreeReader>,
    /// How much it may look up.
    pub lookup: Option<&'a LookupPolicy>,
    /// The files this conversation is about — one for an ungrouped
    /// conversation, several for a grouped one: the definitions their changed
    /// lines call into are fetched before the first turn, unasked, for every
    /// file in the slice — see [`crate::flows::lookup::Ledger::seed`]. Empty
    /// disables seeding.
    pub seed: &'a [crate::evidence::diff::FileDiff],
}

impl<'a> Asking<'a> {
    /// The lookup policy in force, when lookups are possible at all.
    fn lookups(&self) -> Option<(&'a dyn TreeReader, &'a LookupPolicy)> {
        let tree = self.tree?;
        let policy = self.lookup?;
        (policy.enabled && policy.rounds > 0 && policy.per_round > 0).then_some((tree, policy))
    }
}

/// Ask every reviewer at once, and return one [`Answer`] each, in order.
///
/// Two kinds of follow-up, both bounded, both host-owned:
///
/// - **Lookups** — when a tree is available, a reviewer may end a turn with
///   reads and searches instead of a verdict; the host answers them and asks
///   again, up to `lookup.rounds` times. See [`crate::flows::lookup`].
/// - **Questions** — when `subagent_model` is set, a reviewer may end its
///   turn with questions; each is answered by a sub-agent and that reviewer is
///   asked once more. Exactly one such turn — see [`crate::flows::subagent`].
///
/// Lookups run first, so a sub-agent answering a question is handed the
/// evidence the reviewer already fetched rather than the diff alone. The last
/// turn always answers the plain schema: there is genuinely no turn after it.
///
/// Never returns `Err` for a single reviewer's failure — that is an [`Answer`]
/// carrying an `error`. The `Result` is kept so a future failure that means no
/// reviewer was asked at all has somewhere to go other than an empty answer.
pub async fn ask_all_accounted(
    llm: Arc<ModelCapability>,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
    asking: Asking<'_>,
) -> Result<AskOutcome> {
    let started = std::time::Instant::now();
    if calls.is_empty() {
        return Ok(AskOutcome {
            answers: Vec::new(),
            usage: Usage::default(),
            elapsed: started.elapsed(),
        });
    }

    let lookups = asking.lookups();
    let subagent_model = asking.subagent_model;

    // The schema and the instruction travel together: a reviewer told it may
    // ask, answering a schema with no `questions` key, produces a refusal under
    // strict mode and a dropped key under `json_object`. Same for `lookups`.
    let schema_for = |may_lookup: bool, may_ask: bool| -> Value {
        let mut s = schema.clone();
        if may_lookup && let Some((_, policy)) = lookups {
            s = lookup::with_lookups(s, policy);
        }
        if may_ask {
            s = subagent::with_questions(s);
        }
        s
    };

    // The system prompt for a turn says exactly what that turn may do: a turn
    // that may look up is told so, a turn that may ask is told so, and the
    // settling turn is told neither — an instruction to ask, on a turn nothing
    // will answer, invites a question that is never answered.
    let system_for = |base: &str, may_lookup: bool, may_ask: bool| -> String {
        let mut system = base.to_string();
        if may_lookup && let Some((tree, policy)) = lookups {
            system.push_str(&lookup::instruction(&tree.describe(), policy));
        }
        if may_ask {
            system.push_str(subagent::ASK_INSTRUCTION);
        }
        if !may_lookup && !may_ask {
            system.push_str(crate::harness::prompt::SETTLE_INSTRUCTION);
        }
        system
    };

    let max_rounds = lookups.map_or(0, |(_, p)| p.rounds);
    let mut prompts: Vec<Call> = calls
        .iter()
        .cloned()
        .map(|mut call| {
            call.system = system_for(&call.system, max_rounds > 0, subagent_model.is_some());
            call
        })
        .collect();

    // One ledger per reviewer, for the whole conversation: what the host
    // fetched unasked and what the reviewer then asks for share the budget
    // and the dedupe.
    let mut ledgers: Vec<lookup::Ledger> = (0..calls.len())
        .map(|_| lookup::Ledger::default())
        .collect();
    if let Some((tree, policy)) = lookups
        && !asking.seed.is_empty()
    {
        for (index, prompt) in prompts.iter_mut().enumerate() {
            let seeded = ledgers[index].seed(tree, asking.seed, policy).await;
            if !seeded.rendered.is_empty() {
                prompt.prompt.push_str(&seeded.rendered);
                tracing::debug!(
                    reviewer = %prompt.id,
                    definitions = seeded.answered,
                    chars = ledgers[index].chars(),
                    "definitions looked up for the reviewer"
                );
            }
        }
    }

    let mut answers = one_round(
        &llm,
        lane,
        &prompts,
        &schema_for(max_rounds > 0, subagent_model.is_some()),
    )
    .await;
    let mut usage = Usage::default();
    for answer in &answers {
        usage.add(answer.usage);
    }

    // The lookup rounds. Each reviewer that asked gets its results appended
    // and is asked again; one that did not ask is settled and left alone. The
    // schema for the final permitted round offers no `lookups` key, so a
    // reviewer cannot ask for something no turn will answer.
    if let Some((tree, policy)) = lookups {
        for round in 1..=max_rounds {
            let pending: Vec<(usize, Vec<Lookup>)> = answers
                .iter()
                .enumerate()
                .filter_map(|(index, answer)| {
                    let asked = lookup::read_lookups(answer.value.as_ref()?, policy);
                    (!asked.is_empty()).then_some((index, asked))
                })
                .collect();
            if pending.is_empty() {
                break;
            }
            let may_lookup_again = round < max_rounds;
            let round_schema = schema_for(may_lookup_again, subagent_model.is_some());
            for (index, asked) in pending {
                let gathered = ledgers[index].gather(tree, &asked, policy).await;
                if gathered.rendered.is_empty() {
                    continue;
                }
                prompts[index].prompt.push_str(&gathered.rendered);
                prompts[index].system = system_for(
                    &calls[index].system,
                    may_lookup_again,
                    subagent_model.is_some(),
                );
                tracing::debug!(
                    reviewer = %prompts[index].id,
                    round,
                    answered = gathered.answered,
                    chars = ledgers[index].chars(),
                    "a reviewer looked something up"
                );
                // The turn that asked was provisional by its own instruction,
                // so a follow-up that fails cannot leave it standing as the
                // verdict: the file is reported unreviewed instead.
                let again = one_round(
                    &llm,
                    lane,
                    std::slice::from_ref(&prompts[index]),
                    &round_schema,
                )
                .await;
                for answer in &again {
                    usage.add(answer.usage);
                }
                answers[index] = again.into_iter().next().unwrap_or_else(|| {
                    Answer::failed(
                        &prompts[index].id,
                        "the reviewer produced no answer after looking things up",
                    )
                });
            }
        }
    }

    // Everything the prompt grew by is what was read: the suffix started as
    // the lane's evidence and only lookups were appended to it. Filled here,
    // before the sub-agent branch below, so a reviewer's own lookups reach
    // the falsifier even with `council.subagents = false` — the default —
    // when the early return below would otherwise skip it entirely and leave
    // every `looked_up` empty.
    for (index, answer) in answers.iter_mut().enumerate() {
        answer.looked_up = prompts[index]
            .prompt
            .strip_prefix(calls[index].prompt.as_str())
            .unwrap_or_default()
            .to_string();
    }

    let Some(model) = subagent_model else {
        return Ok(AskOutcome {
            answers,
            usage,
            elapsed: started.elapsed(),
        });
    };

    // Which reviewers asked something, and what.
    let pending: Vec<(usize, Vec<String>)> = answers
        .iter()
        .enumerate()
        .filter_map(|(index, answer)| {
            let questions = read_questions(answer.value.as_ref()?);
            (!questions.is_empty()).then_some((index, questions))
        })
        .collect();

    for (index, questions) in pending {
        // The evidence as the reviewer last saw it: the diff plus whatever it
        // looked up. A sub-agent handed only the diff was answering "from the
        // repository" in name alone.
        let evidence = &prompts[index].prompt;
        let (answered, subagent_usage) =
            answer_questions(&llm, lane, model, &questions, evidence).await;
        usage.add(subagent_usage);

        // Nothing came back, so a second turn would be the same turn with the
        // same evidence — one more call that cannot say anything new.
        if answered.is_empty() {
            continue;
        }

        // The final turn answers the plain schema: there is genuinely no turn
        // after this one, so offering `questions` again would invite a question
        // nothing will ever answer.
        let mut again = prompts[index].clone();
        again.system = system_for(&calls[index].system, false, false);
        again.prompt.push_str(&subagent::render(&answered));

        let round_two = one_round(&llm, lane, std::slice::from_ref(&again), schema).await;
        for answer in &round_two {
            usage.add(answer.usage);
        }
        if let Some(mut settled) = round_two.into_iter().next()
            && settled.value.is_some()
        {
            // The evidence the reviewer read travels with its final answer.
            settled.looked_up = answers[index].looked_up.clone();
            answers[index] = settled;
        }
    }

    Ok(AskOutcome {
        answers,
        usage,
        elapsed: started.elapsed(),
    })
}

/// Ask every reviewer, preserving the original answers-only API.
pub async fn ask_all(
    llm: Arc<ModelCapability>,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
    asking: Asking<'_>,
) -> Result<Vec<Answer>> {
    Ok(ask_all_accounted(llm, lane, calls, schema, asking)
        .await?
        .answers)
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod tests;
