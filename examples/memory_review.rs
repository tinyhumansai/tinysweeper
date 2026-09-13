//! Run one review against a real CortexDB and print what the lane remembered.
//!
//! Declared with `required-features = ["cortex"]` so it never builds in CI: it
//! needs a running engine and the default `cargo test` must not touch one.
//!
//! This is the smoke test for the memory seam. Everything under `src/memory/`
//! is covered offline against `MockMemory`; what a mock cannot tell you is
//! whether the engine ranks a section above its neighbours, whether a
//! question over one section comes back with the rule and its file, and
//! whether a citation resolves to a path. Those three are the point of this.
//!
//! ```sh
//! export CORTEX_API_KEY=…
//! cargo run --features cortex --example memory_review -- . owner/name
//! ```
//!
//! The engine comes from `[memory]` in the checkout's configuration, exactly
//! as the server reads it. The pull request and the model are mocks: the
//! forge serves one changed file and a review thread a maintainer closed with
//! "this is intentional", and the model answers "fine" — so the only thing
//! that varies between runs is what the engine gave the lane.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tinysweeper::error::{Error, Result};
use tinysweeper::forge::mock::{MockForge, MockState};
use tinysweeper::forge::types::{
    ChangedFile, FileStatus, PullRequest, RepoId, ReviewComment, ReviewThread, ThreadComment,
};
use tinysweeper::harness::mock::MockModel;
use tinysweeper::memory::cortex::CortexMemory;
use tinysweeper::memory::{Ingestor, Recaller};
use tinysweeper::ports::memory::Memory;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tinysweeper=info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let repo = args
        .next()
        .unwrap_or_else(|| "tinyhumansai/tinysweeper".to_string());
    let repo_id = RepoId::parse(&repo).ok_or_else(|| Error::config("repo must be owner/name"))?;

    let loaded = tinysweeper::config::load_validated(Path::new(&root), None)?;
    let mut config = loaded.config;
    if !config.memory.enabled {
        return Err(Error::config(
            "`[memory]` is disabled; set `enabled = true` and an `endpoint` in the checkout's \
             .tinysweeper.toml to run this example",
        ));
    }
    config.review.lanes = vec!["critique".into()];

    let memory = CortexMemory::from_config(&config.memory)?;
    memory.health().await?;
    println!("engine: {} at {}", memory.name(), config.memory.endpoint);

    // Feed it the checkout, as the server would from the base branch.
    let ingestor = Ingestor::new(&memory, &config.memory, &config.paths.ignore)?;
    let report = ingestor.ingest_checkout(&repo, Path::new(&root)).await?;
    println!("ingested: {}", report.summary());

    // One changed file in `src/app/`, and a thread from an earlier push that
    // a maintainer closed by hand. Observing the thread is what writes the
    // outcome; recalling is what should bring it back.
    let fingerprint = "0123456789abcdef";
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "apply: mint the write token later".into(),
            body: "Moves token minting after the last model call in apply.".into(),
            head_sha: "abc123".into(),
            base_sha: "def456".into(),
            ..PullRequest::default()
        },
    );
    state.files.insert(
        7,
        vec![ChangedFile {
            path: "src/app/apply.rs".into(),
            status: FileStatus::Modified,
            patch: Some(
                "@@ -1,3 +1,4 @@\n fn apply() {\n-    let token = mint();\n+    let plan = decide();\n+    let token = mint();\n }\n"
                    .into(),
            ),
            ..ChangedFile::default()
        }],
    );
    state.review_comments.insert(
        7,
        vec![ReviewComment {
            path: "src/app/apply.rs".into(),
            line: Some(2),
            start_line: None,
            author: "tinysweeper[bot]".into(),
            body: format!(
                "**Mint the token lazily**\n\nx\n\n<!-- tinysweeper:fp={fingerprint} -->"
            ),
        }],
    );
    let forge = MockForge::with_state(state).with_review_threads(
        7,
        vec![ReviewThread {
            id: "t1".into(),
            is_resolved: true,
            is_outdated: false,
            resolved_by_has_write_access: true,
            comments: vec![
                ThreadComment {
                    author: "tinysweeper[bot]".into(),
                    body: format!(
                        "**Mint the token lazily**\n\nx\n\n<!-- tinysweeper:fp={fingerprint} -->"
                    ),
                    bot: true,
                    maintainer: false,
                },
                ThreadComment {
                    author: "maintainer".into(),
                    body: "Intentional: the token is minted once, after every model call, and \
                           that ordering is the security boundary."
                        .into(),
                    bot: false,
                    maintainer: true,
                },
            ],
        }],
    );

    let model = MockModel::always(json!({"summary": "Fine.", "findings": []}));
    let recaller = Recaller::new(&memory);
    let started = std::time::Instant::now();
    let proposal = tinysweeper::app::review::review_with_memory(
        &forge,
        Arc::new(model.clone()),
        &config,
        &repo_id,
        7,
        None,
        None,
        None,
        Some(&recaller),
    )
    .await?;
    println!("reviewed in {:.1}s", started.elapsed().as_secs_f64());

    for lane in &proposal.lanes {
        println!("{}: {}", lane.check_name, lane.summary.trim());
    }
    let request = model
        .requests()
        .into_iter()
        .find(|r| r.schema_name == "tinysweeper_critique")
        .ok_or_else(|| Error::config("the critique lane did not run"))?;
    let user = &request.messages[1].content;
    match user.find("repository-memory") {
        Some(start) => {
            let block = &user[start..];
            let end = block.find("\n## ").unwrap_or(block.len());
            println!("\n--- what the lane remembered ---\n{}", &block[..end]);
        }
        None => println!("\nthe lane received no memory block"),
    }
    Ok(())
}
