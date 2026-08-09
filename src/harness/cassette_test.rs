//! Cassette recording, replay, and the two ways it is allowed to miss.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::harness::mock::MockModel;
use crate::ports::model::Message;

fn request(model: &str, prompt: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages: vec![Message::system("instructions"), Message::user(prompt)],
        schema: json!({"type": "object"}),
        schema_name: "tinysweeper_critique".into(),
        max_tokens: 8000,
    }
}

fn answer() -> serde_json::Value {
    json!({"summary": "Nothing to report.", "findings": []})
}

#[test]
fn the_key_covers_everything_that_can_change_an_answer() {
    let base = request("z-ai/glm-5.2", "the diff");

    assert_eq!(key(&base), key(&request("z-ai/glm-5.2", "the diff")));

    let other_model = request("deepseek/deepseek-v4-pro", "the diff");
    assert_ne!(
        key(&base),
        key(&other_model),
        "the model decides the answer"
    );

    let other_prompt = request("z-ai/glm-5.2", "a different diff");
    assert_ne!(key(&base), key(&other_prompt), "so does the prompt");

    let mut other_schema = base.clone();
    other_schema.schema_name = "tinysweeper_falsify".into();
    assert_ne!(key(&base), key(&other_schema), "and the schema");

    // The same name with a different body is a different output contract: a
    // provider that answers against `properties: {summary: string}` does not
    // answer like one against `properties: {findings: []}`.
    let mut other_schema = base.clone();
    other_schema.schema = json!({"type": "object", "properties": {"findings": {"type": "array"}}});
    assert_ne!(key(&base), key(&other_schema), "and the schema body");

    let mut other_ceiling = base.clone();
    other_ceiling.max_tokens = 4000;
    assert_ne!(
        key(&base),
        key(&other_ceiling),
        "a model that runs out of tokens answers differently"
    );
}

#[test]
fn the_key_distinguishes_who_said_what() {
    // The same text as a system instruction and as user evidence is not the
    // same prompt: one is policy and the other is attacker-controlled.
    let mut swapped = request("z-ai/glm-5.2", "text");
    swapped.messages = vec![Message::user("instructions"), Message::system("text")];
    assert_ne!(key(&request("z-ai/glm-5.2", "text")), key(&swapped));
}

#[tokio::test]
async fn a_recording_replays_the_same_answer_the_same_cost_and_the_same_model() {
    let dir = tempfile::tempdir().expect("tempdir");

    let live = Arc::new(
        MockModel::always(answer())
            .answering_as("deepseek/deepseek-v4-pro")
            .with_usage(Usage {
                input_tokens: 1_200,
                output_tokens: 300,
                cached_tokens: 900,
                embed_tokens: 0,
                cost_usd: 0.000_15,
            }),
    );
    let recorder = Cassette::record(live, dir.path());
    let recorded = recorder
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("records");
    assert_eq!(recorder.flush().expect("flushes"), 1);

    let player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    let replayed = player
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("replays");

    assert_eq!(replayed.value, recorded.value);
    // The fallback is part of the recording: the run being reproduced is the
    // one where a cheaper model answered, not the one that was configured.
    assert_eq!(replayed.model, "deepseek/deepseek-v4-pro");
    // Replayed verbatim rather than re-derived through the price table, so a
    // re-score reports the dollars the live run actually paid.
    assert_eq!(replayed.usage.cost_usd, 0.000_15);
    assert_eq!(replayed.usage.cached_tokens, 900);
}

#[tokio::test]
async fn a_changed_prompt_is_a_loud_miss_rather_than_a_stale_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    let err = player
        .complete(request("z-ai/glm-5.2", "the diff, reworded"))
        .await
        .expect_err("must not serve the old answer");

    let message = err.to_string();
    // The message has to say what to do, because "the corpus is stale" is the
    // single most common thing this will report.
    assert!(message.contains("prompt changed"), "{message}");
    assert!(message.contains("re-record"), "{message}");
    assert!(message.contains("tinysweeper_critique"), "{message}");
}

#[tokio::test]
async fn a_loose_replay_survives_a_cosmetic_edit_and_counts_that_it_did() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Loose).expect("loads");
    let replayed = player
        .complete(request("z-ai/glm-5.2", "the  diff"))
        .await
        .expect("falls back to call order");

    assert_eq!(replayed.value, answer());
    // Counted, and reported, so a loose run is never mistaken for a strict one.
    assert_eq!(player.loose_hits(), 1);
}

#[tokio::test]
async fn an_exhausted_loose_cassette_says_how_far_it_got() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "one"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Loose).expect("loads");
    player
        .complete(request("z-ai/glm-5.2", "two"))
        .await
        .expect("first falls back");
    let err = player
        .complete(request("z-ai/glm-5.2", "three"))
        .await
        .expect_err("nothing left");

    assert!(err.to_string().contains("exhausted"), "{err}");
}

#[tokio::test]
async fn calls_replay_in_the_order_they_were_made() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = MockModel::new()
        .then(json!({"n": 1}))
        .then(json!({"n": 2}))
        .then(json!({"n": 3}));
    let recorder = Cassette::record(Arc::new(live), dir.path());
    for prompt in ["one", "two", "three"] {
        recorder
            .complete(request("z-ai/glm-5.2", prompt))
            .await
            .expect("records");
    }
    assert_eq!(recorder.flush().expect("flushes"), 3);

    // Filename order is call order, which is what the loose fallback rides on.
    let player = Cassette::replay(dir.path(), Mode::Loose).expect("loads");
    for expected in 1..=3 {
        let got = player
            .complete(request("z-ai/glm-5.2", "unrecognised"))
            .await
            .expect("falls back in order");
        assert_eq!(got.value, json!({"n": expected}));
    }
}

#[tokio::test]
async fn two_identical_prompts_in_one_run_both_survive_the_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "same"))
        .await
        .expect("records");
    recorder
        .complete(request("z-ai/glm-5.2", "same"))
        .await
        .expect("records");

    // Same key both times. Keying the filename on the hash alone would have
    // written one file and lost a call, which shows up later as a cassette
    // that runs out early.
    assert_eq!(recorder.flush().expect("flushes"), 2);
    let files = std::fs::read_dir(dir.path()).expect("readable").count();
    assert_eq!(files, 2);
}

#[tokio::test]
async fn a_prompt_is_not_written_to_disk_unless_it_is_asked_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "a private repository's diff"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let written = std::fs::read_dir(dir.path())
        .expect("readable")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect::<String>();

    // A cassette committed from a private repository must not carry its diff.
    assert!(
        !written.contains("a private repository's diff"),
        "the prompt was written without being asked for: {written}"
    );

    let dir2 = tempfile::tempdir().expect("tempdir");
    let opted_in =
        Cassette::record(Arc::new(MockModel::always(answer())), dir2.path()).with_prompts(true);
    opted_in
        .complete(request("z-ai/glm-5.2", "a public repository's diff"))
        .await
        .expect("records");
    opted_in.flush().expect("flushes");
    let written2 = std::fs::read_dir(dir2.path())
        .expect("readable")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect::<String>();
    assert!(written2.contains("a public repository's diff"));
}

#[test]
fn replaying_a_directory_that_holds_no_cassette_says_how_to_make_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = Cassette::replay(dir.path().join("absent"), Mode::Strict).expect_err("no cassette");
    assert!(err.to_string().contains("eval run --record"), "{err}");
}
