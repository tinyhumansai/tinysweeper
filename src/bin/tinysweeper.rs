//! The `tinysweeper` command-line entry point.
//!
//! One binary, three entry points that matter. `serve` runs the webhook server,
//! which is how tinysweeper reaches every installed repository without a
//! workflow file in any of them — it is the only distribution path, since the
//! GitHub Actions one was removed.
//!
//! `review` and `apply` are the same engine driven by hand against one pull
//! request, kept for operator use and for debugging a delivery the server
//! already handled. `local-review` is that engine over a local git range with
//! no GitHub item and no tokens — which is how prompt changes get iterated
//! without burning pull requests.

use clap::{Parser, Subcommand};
use tinysweeper::Result;

#[derive(Debug, Parser)]
#[command(name = "tinysweeper", author, version, about)]
struct Cli {
    /// Increase log verbosity. Repeat for more (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Review a pull request and publish the check runs.
    Review {
        /// The repository, as `owner/name`.
        #[arg(long, env = "GITHUB_REPOSITORY")]
        repo: String,

        /// The pull request number.
        #[arg(long)]
        pr: u64,

        /// Path to the config file. Defaults to discovery from the repo root.
        #[arg(long)]
        config: Option<std::path::PathBuf>,

        /// Only run these lanes, overriding the config.
        #[arg(long, value_delimiter = ',')]
        lanes: Vec<String>,

        /// Render what would be posted without writing anything to GitHub.
        #[arg(long)]
        dry_run: bool,

        /// Write the proposal here instead of publishing it.
        ///
        /// This is the normal path: `review` proposes, `apply` disposes, and
        /// only `apply` ever holds a write token.
        #[arg(long, default_value = "findings.json")]
        propose_to: std::path::PathBuf,
    },

    /// Publish a proposal produced by `review`. Makes no model calls.
    Apply {
        /// The repository, as `owner/name`.
        #[arg(long, env = "GITHUB_REPOSITORY")]
        repo: String,

        /// The pull request number.
        #[arg(long)]
        pr: u64,

        /// The proposal written by `review`.
        #[arg(long, default_value = "findings.json")]
        findings: std::path::PathBuf,
    },

    /// Label a pull request from a proposal `review` already produced.
    ///
    /// The manual half of triage: `apply` does this automatically, so this is
    /// for re-labelling a pull request whose review was published before triage
    /// existed, or whose labels a maintainer cleared. Makes no model calls and
    /// costs nothing — every input is evidence the proposal already holds.
    Triage {
        /// The repository, as `owner/name`.
        #[arg(long, env = "GITHUB_REPOSITORY")]
        repo: String,

        /// The pull request number.
        #[arg(long)]
        pr: u64,

        /// The proposal written by `review`.
        #[arg(long, default_value = "findings.json")]
        findings: std::path::PathBuf,
    },

    /// Merge a pull request if it qualifies under `[automerge]`.
    ///
    /// Deterministic and off unless the repository opts in. Makes no model
    /// calls: every criterion is arithmetic over check conclusions, review
    /// states, paths and SHAs.
    Automerge {
        /// The repository, as `owner/name`.
        #[arg(long, env = "GITHUB_REPOSITORY")]
        repo: String,

        /// The pull request number.
        #[arg(long)]
        pr: u64,

        /// Report the decision without merging anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Run the review engine over a local git range. No GitHub, no tokens.
    LocalReview {
        /// The base revision to diff against.
        #[arg(long, default_value = "origin/main")]
        base: String,

        /// The head revision. Defaults to the working tree.
        #[arg(long)]
        head: Option<String>,

        /// Path to the config file. Defaults to discovery from the repo root.
        #[arg(long)]
        config: Option<std::path::PathBuf>,

        /// Only run these lanes, overriding the config.
        #[arg(long, value_delimiter = ',')]
        lanes: Vec<String>,

        /// The checkout to review. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        dir: std::path::PathBuf,

        /// Title shown to the `description` lane.
        ///
        /// A git range has none, so it defaults to the newest commit's subject.
        /// Set this to review a real pull request description before opening
        /// one.
        #[arg(long)]
        title: Option<String>,

        /// Body shown to the `description` lane. Defaults to empty.
        #[arg(long)]
        body: Option<String>,
    },

    /// Measure review quality against the labelled corpus in `evals/`.
    #[command(subcommand)]
    Eval(EvalCommand),

    /// Validate a `.tinysweeper.toml`, reporting every problem at once.
    Check {
        /// The config file, or a directory to discover one in.
        #[arg(default_value = ".")]
        path: std::path::PathBuf,
    },

    /// Run the webhook server.
    Serve {
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1:8080", env = "TINYSWEEPER_BIND")]
        bind: String,

        /// Path to the config file used for repositories without one.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },

    /// Report the effective configuration and which layer set each value.
    Doctor {
        /// The config file, or a directory to discover one in.
        #[arg(default_value = ".")]
        path: std::path::PathBuf,

        /// Emit JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
}

/// The corpus commands.
///
/// Split into run / score / report because the first costs money and the other
/// two do not. A scoring rule is rewritten many times before it is right, and
/// welding it to the run would price every rewrite at another live corpus.
#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Review every case and score what came back.
    Run {
        /// The corpus directory.
        #[arg(long, default_value = "evals")]
        corpus: std::path::PathBuf,

        /// Where to write proposals and the scorecard.
        #[arg(long, default_value = "evals/runs/latest")]
        out: std::path::PathBuf,

        /// Only these cases. Repeatable.
        #[arg(long = "case")]
        cases: Vec<String>,

        /// Path to the config file. Defaults to discovery from the repo root.
        #[arg(long)]
        config: Option<std::path::PathBuf>,

        /// Call the real model and record every answer. Needs `harness`.
        #[arg(long)]
        record: bool,

        /// Also store prompts in the cassette.
        ///
        /// Off by default: a prompt embeds the reviewed repository's diff, and
        /// a cassette committed from a private repository would carry it into
        /// git.
        #[arg(long)]
        record_prompts: bool,

        /// On a cassette miss, fall back to call order instead of failing.
        ///
        /// Survives a cosmetic prompt edit without a re-record. Stamped into
        /// the report, because a loose run's numbers describe the prompt that
        /// was recorded rather than the one in the tree.
        #[arg(long)]
        loose: bool,

        /// Stop the whole run at this many dollars.
        #[arg(long, default_value_t = 5.0)]
        max_cost_usd: f64,
    },

    /// Re-score proposals already on disk. Free, offline, no model.
    Score {
        /// The run directory written by `eval run`.
        #[arg(long, default_value = "evals/runs/latest")]
        run: std::path::PathBuf,

        /// The corpus directory.
        #[arg(long, default_value = "evals")]
        corpus: std::path::PathBuf,

        /// Path to the config file, for the digest stamped into the scorecard.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },

    /// Render a scorecard, optionally against a baseline.
    Report {
        /// The run directory holding `scorecard.json`.
        #[arg(long, default_value = "evals/runs/latest")]
        run: std::path::PathBuf,

        /// A scorecard to compare against.
        #[arg(long)]
        baseline: Option<std::path::PathBuf>,

        /// `md` or `json`.
        #[arg(long, default_value = "md")]
        format: String,

        /// Compare even when the corpus or configuration moved.
        #[arg(long)]
        allow_config_drift: bool,

        /// Exit non-zero when the comparison fails. For CI.
        ///
        /// The comparison is against `--baseline`, so gating without one would
        /// silently print a report and exit zero. Clap rejects the combination
        /// rather than leaving that footgun.
        #[arg(long, requires = "baseline")]
        gate: bool,
    },

    /// Freeze a live pull request into a corpus fixture. Needs `github`.
    Add {
        /// The repository, as `owner/name`.
        #[arg(long, env = "GITHUB_REPOSITORY")]
        repo: String,

        /// The pull request number.
        #[arg(long)]
        pr: u64,

        /// The case id, which is also the fixture and cassette name.
        #[arg(long)]
        id: String,

        /// The corpus directory.
        #[arg(long, default_value = "evals")]
        corpus: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    dispatch(cli.command).await
}

/// Dispatch one parsed command to its application entry point.
///
/// Kept separate from process setup so command tests can exercise the same
/// parse-to-application path an operator invokes without initialising a
/// process-global tracing subscriber.
async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Review {
            repo,
            pr,
            config,
            lanes,
            dry_run,
            propose_to,
        } => run_review(&repo, pr, config, lanes, dry_run, &propose_to).await,
        Command::Apply { repo, pr, findings } => run_apply(&repo, pr, &findings).await,
        Command::Triage { repo, pr, findings } => run_triage(&repo, pr, &findings).await,
        Command::Automerge { repo, pr, dry_run } => run_automerge(&repo, pr, dry_run).await,
        Command::Eval(command) => run_eval(command).await,
        Command::LocalReview {
            base,
            head,
            config,
            lanes,
            dir,
            title,
            body,
        } => run_local_review(base, head, config, lanes, &dir, title, body).await,
        Command::Serve { bind, config } => run_serve(bind, config).await,
        Command::Check { path } => tinysweeper::app::check(&path),
        Command::Doctor { path, json } => tinysweeper::app::doctor(&path, json),
    }
}

/// Run the read-only half: scanners, lanes, and a proposal on disk.
#[cfg(all(feature = "github", feature = "harness"))]
async fn run_review(
    repo: &str,
    pr: u64,
    config_path: Option<std::path::PathBuf>,
    lanes: Vec<String>,
    dry_run: bool,
    propose_to: &std::path::Path,
) -> Result<()> {
    use std::sync::Arc;
    use tinysweeper::forge::RepoId;
    use tinysweeper::forge::github::GitHubRead;
    use tinysweeper::harness::openrouter::GatewayModel;

    let repo_id = RepoId::parse(repo)
        .ok_or_else(|| tinysweeper::Error::config(format!("`{repo}` is not owner/name")))?;

    let mut loaded =
        tinysweeper::config::load_validated(std::path::Path::new("."), config_path.as_deref())?;
    if !lanes.is_empty() {
        loaded.config.review.lanes = lanes;
    }

    let forge = GitHubRead::from_env()?;
    let model = Arc::new(GatewayModel::from_config(&loaded.config.models)?);

    let proposal = tinysweeper::app::review(&forge, model, &loaded.config, &repo_id, pr).await?;

    println!("{}", render(&proposal));

    if dry_run {
        println!("\n(dry run — nothing was written to GitHub)");
        return Ok(());
    }

    tinysweeper::app::write_proposal(&proposal, propose_to)?;
    println!("\nproposal written to {}", propose_to.display());
    Ok(())
}

#[cfg(not(all(feature = "github", feature = "harness")))]
async fn run_review(
    _repo: &str,
    _pr: u64,
    _config: Option<std::path::PathBuf>,
    _lanes: Vec<String>,
    _dry_run: bool,
    _propose_to: &std::path::Path,
) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "reviewing a pull request",
        "github,harness",
    ))
}

/// The corpus commands. Only `run --record` and `add` need a network.
async fn run_eval(command: EvalCommand) -> Result<()> {
    use tinysweeper::eval;

    match command {
        EvalCommand::Run {
            corpus,
            out,
            cases,
            config,
            record,
            record_prompts,
            loose,
            max_cost_usd,
        } => {
            let loaded =
                tinysweeper::config::load_validated(std::path::Path::new("."), config.as_deref())?;
            let corpus_data = eval::load(&corpus)?.select(&cases)?;
            let model = if record {
                Some(live_model(&loaded.config)?)
            } else {
                None
            };

            let options = eval::RunOptions {
                out: out.clone(),
                record,
                loose,
                record_prompts,
                max_cost_usd,
            };
            let outcome = eval::run(&corpus_data, &loaded.config, model, &options).await?;

            let card = eval::Scorecard {
                corpus_digest: corpus_data.digest.clone(),
                config_digest: outcome.config_digest.clone(),
                loose_replays: outcome.loose_replays,
                cases: outcome.scores,
            };
            let path = eval::write_scorecard(&out, &card)?;

            for id in &outcome.skipped {
                println!("skipped `{id}`: the run hit --max-cost-usd");
            }
            println!("{}", eval::markdown(&card, None, false));
            println!("scorecard written to {}", path.display());
            Ok(())
        }

        EvalCommand::Score {
            run,
            corpus,
            config,
        } => {
            let loaded =
                tinysweeper::config::load_validated(std::path::Path::new("."), config.as_deref())?;
            let corpus_data = eval::load(&corpus)?;
            let scores = eval::rescore(&corpus_data, &run)?;

            // Re-scoring reads proposals, not cassettes, so nothing is replayed
            // here. `loose_replays` does not describe this command, though — it
            // describes the run whose proposals are being scored, so carry the
            // recorded figure over rather than erasing it from the scorecard.
            let previous = eval::read_scorecard(&run.join("scorecard.json")).ok();
            let card = eval::Scorecard {
                corpus_digest: corpus_data.digest.clone(),
                config_digest: eval::runner::digest_of(&loaded.config),
                loose_replays: previous
                    .as_ref()
                    .map(|card| card.loose_replays)
                    .unwrap_or(0),
                cases: scores,
            };
            let path = eval::write_scorecard(&run, &card)?;
            println!("{}", eval::markdown(&card, None, false));
            println!("scorecard written to {}", path.display());
            Ok(())
        }

        EvalCommand::Report {
            run,
            baseline,
            format,
            allow_config_drift,
            gate,
        } => {
            let card = eval::read_scorecard(&run.join("scorecard.json"))?;
            let baseline = baseline.as_deref().map(eval::read_scorecard).transpose()?;

            match format.as_str() {
                "json" => println!("{}", serde_json::to_string_pretty(&card)?),
                "md" => println!(
                    "{}",
                    eval::markdown(&card, baseline.as_ref(), allow_config_drift)
                ),
                other => {
                    return Err(tinysweeper::Error::config(format!(
                        "`{other}` is not a format; use `md` or `json`"
                    )));
                }
            }

            // Non-zero only when asked for, so a report can be read without
            // failing a shell.
            if gate && let Some(baseline) = baseline.as_ref() {
                match eval::compare(&card, baseline, allow_config_drift) {
                    eval::Comparison::Pass => {}
                    eval::Comparison::Fail(reasons) => {
                        return Err(tinysweeper::Error::config(format!(
                            "review quality regressed:\n  - {}",
                            reasons.join("\n  - ")
                        )));
                    }
                    eval::Comparison::Incomparable(why) => {
                        return Err(tinysweeper::Error::config(why));
                    }
                }
            }
            Ok(())
        }

        EvalCommand::Add {
            repo,
            pr,
            id,
            corpus,
        } => add_case(&repo, pr, &id, &corpus).await,
    }
}

/// The live provider, when the build has one.
#[cfg(feature = "harness")]
fn live_model(
    config: &tinysweeper::config::types::Config,
) -> Result<std::sync::Arc<dyn tinysweeper::ports::model::Model>> {
    Ok(std::sync::Arc::new(
        tinysweeper::harness::openrouter::GatewayModel::from_config(&config.models)?,
    ))
}

#[cfg(not(feature = "harness"))]
fn live_model(
    _config: &tinysweeper::config::types::Config,
) -> Result<std::sync::Arc<dyn tinysweeper::ports::model::Model>> {
    Err(tinysweeper::Error::FeatureDisabled(
        "recording a corpus",
        "harness",
    ))
}

/// Run the engine over a local git range. No token, no forge, no writes.
///
/// Needs `harness` and nothing else: the evidence comes from `git`, so the
/// GitHub adapter is not linked on this path at all.
#[cfg(feature = "harness")]
#[allow(clippy::too_many_arguments)]
async fn run_local_review(
    base: String,
    head: Option<String>,
    config_path: Option<std::path::PathBuf>,
    lanes: Vec<String>,
    dir: &std::path::Path,
    title: Option<String>,
    body: Option<String>,
) -> Result<()> {
    use std::sync::Arc;
    use tinysweeper::app::{LocalInput, local_review};
    use tinysweeper::evidence::git::Range;
    use tinysweeper::harness::openrouter::GatewayModel;

    let mut loaded = tinysweeper::config::load_validated(dir, config_path.as_deref())?;
    if !lanes.is_empty() {
        loaded.config.review.lanes = lanes;
    }

    let model = Arc::new(GatewayModel::from_config(&loaded.config.models)?);
    let input = LocalInput {
        range: Range { base, head },
        title,
        body,
    };

    let (proposal, context) = local_review(dir, &input, model, &loaded.config).await?;

    println!(
        "{} {}..{}{}\n",
        context.repo,
        short(&context.range.base_sha),
        short(&context.range.head_sha),
        if context.range.dirty {
            " + working tree"
        } else {
            ""
        }
    );
    println!("{}", render(&proposal));
    println!("(local review — nothing was written anywhere)");
    Ok(())
}

#[cfg(not(feature = "harness"))]
#[allow(clippy::too_many_arguments)]
async fn run_local_review(
    _base: String,
    _head: Option<String>,
    _config: Option<std::path::PathBuf>,
    _lanes: Vec<String>,
    _dir: &std::path::Path,
    _title: Option<String>,
    _body: Option<String>,
) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "reviewing a local range",
        "harness",
    ))
}

/// Freeze a live pull request into a fixture and a case stub.
///
/// The case stub is written with the expectations left empty and the provenance
/// half-filled, because the one thing this cannot do is label it. An
/// expectation written from the bot's own output measures whether the bot still
/// agrees with itself — so `evidence` is left blank and the corpus loader
/// refuses the case until a human puts something real there.
#[cfg(feature = "github")]
async fn add_case(repo: &str, pr: u64, id: &str, corpus: &std::path::Path) -> Result<()> {
    use tinysweeper::forge::RepoId;
    use tinysweeper::forge::github::GitHubRead;
    use tinysweeper::ports::forge::ForgeRead;

    let repo_id = RepoId::parse(repo)
        .ok_or_else(|| tinysweeper::Error::config(format!("`{repo}` is not owner/name")))?;
    let loaded = tinysweeper::config::load_validated(std::path::Path::new("."), None)?;

    let forge = GitHubRead::from_env()?;
    let context = forge.pull_request_context(&repo_id, pr).await?;

    // Only the instruction files, not the tree: a fixture carrying a whole
    // repository is unreviewable in a diff.
    let mut blobs = std::collections::BTreeMap::new();
    for name in &loaded.config.knowledge.files {
        if let Some(content) = forge
            .file_at(&repo_id, name, &context.pull_request.head_sha)
            .await?
        {
            blobs.insert(name.clone(), content);
        }
    }

    let fixture = tinysweeper::eval::Fixture {
        pull_request: context.pull_request.clone(),
        files: context.files,
        commits: context.commits,
        comments: context.comments,
        blobs,
    };

    let fixtures = corpus.join("fixtures");
    let cases = corpus.join("cases");
    std::fs::create_dir_all(&fixtures)?;
    std::fs::create_dir_all(&cases)?;

    let fixture_path = fixtures.join(format!("{id}.json"));
    std::fs::write(&fixture_path, serde_json::to_string_pretty(&fixture)?)?;

    let case_path = cases.join(format!("{id}.toml"));
    if case_path.exists() {
        println!("fixture refreshed: {}", fixture_path.display());
        println!("case left alone: {}", case_path.display());
        return Ok(());
    }
    std::fs::write(
        &case_path,
        case_stub(id, repo, pr, &context.pull_request.title),
    )?;

    println!("fixture written to {}", fixture_path.display());
    println!("case stub written to {}", case_path.display());
    println!(
        "\nIt will not load yet. Fill in `provenance.evidence` with something outside this bot \
         — the follow-up fix, an acted-on review comment, a revert — and then write the \
         expectations."
    );
    Ok(())
}

#[cfg(not(feature = "github"))]
async fn add_case(_repo: &str, _pr: u64, _id: &str, _corpus: &std::path::Path) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "freezing a pull request into a fixture",
        "github",
    ))
}

/// The case file `eval add` leaves for a human to finish.
#[cfg(feature = "github")]
fn case_stub(id: &str, repo: &str, pr: u64, title: &str) -> String {
    format!(
        r#"schema = 1
id = "{0}"
title = "{1}"
fixture = "../fixtures/{0}.json"
labels = []

[provenance]
repo = "{repo}"
pr = {pr}
# REQUIRED, and the loader refuses the case without it. Cite something this bot
# did not produce: the follow-up commit that fixed it, a human review comment
# somebody acted on, a revert. An expectation written from the bot's own output
# measures whether the bot still agrees with itself.
evidence = ""
labelled_by = ""

# What a good reviewer should find. Delete the block for a clean case — a
# pull request with nothing wrong with it is how noise gets measured.
# [[expected]]
# id = "E1"
# path = "src/some/file.rs"
# lines = [10, 14]
# severity_min = "medium"
# summary = "One sentence a human can check the label against."
# must_mention = ["keyword", "either|or"]

# What it must not say. Every entry needs a reason, because the next person to
# read it will otherwise assume it is a mistake and delete it.
# [[forbidden]]
# id = "F1"
# path = "src/some/file.rs"
# reason = "Three earlier runs called this dead code. It is not."
# matches = ["dead code", "unused"]
"#,
        toml_escape(id),
        toml_escape(title),
    )
}

/// Escape one value for a `"..."` line of a case stub.
///
/// A pull request title is a single line, but it can still carry a quote, a
/// backslash, or a tab — each of which would silently corrupt the TOML the
/// stub guards. `id` gets the same treatment for the same reason: it is
/// operator input with no upstream validation. Escaping all of them keeps the
/// file parseable whatever the value is; the crate's own `toml` module is not
/// used because the stub is comment-rich and hand-shaped, and only these two
/// values are interpolated.
#[cfg(feature = "github")]
fn toml_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// The first eight characters of a sha, for a human reading a terminal.
#[cfg(feature = "harness")]
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// Publish a proposal. No model key reaches this path.
#[cfg(feature = "github")]
async fn run_apply(repo: &str, pr: u64, findings: &std::path::Path) -> Result<()> {
    use tinysweeper::forge::RepoId;
    use tinysweeper::forge::github::{GitHubRead, GitHubWrite};

    let repo_id = RepoId::parse(repo)
        .ok_or_else(|| tinysweeper::Error::config(format!("`{repo}` is not owner/name")))?;

    let proposal = tinysweeper::app::read_proposal(findings)?;
    validate_apply_target(&proposal, &repo_id, pr)?;

    let loaded = tinysweeper::config::load_validated(std::path::Path::new("."), None)?;
    let read = GitHubRead::from_env()?;
    let write = GitHubWrite::from_env()?;
    tinysweeper::app::apply(&read, &write, &loaded.config, &proposal, None).await?;

    println!("published {} check run(s)", proposal.lanes.len());
    Ok(())
}

/// Label a pull request from a proposal already on disk. No model calls.
///
/// The manual trigger. `app::apply` already triages on every review, so this
/// exists for the pull request whose labels a maintainer cleared, or whose
/// review predates triage.
#[cfg(feature = "github")]
async fn run_triage(repo: &str, pr: u64, findings: &std::path::Path) -> Result<()> {
    use tinysweeper::forge::RepoId;
    use tinysweeper::forge::github::{GitHubRead, GitHubWrite};
    use tinysweeper::ports::forge::ForgeRead;

    let repo_id = RepoId::parse(repo)
        .ok_or_else(|| tinysweeper::Error::config(format!("`{repo}` is not owner/name")))?;

    let proposal = tinysweeper::app::read_proposal(findings)?;
    validate_apply_target(&proposal, &repo_id, pr)?;

    let loaded = tinysweeper::config::load_validated(std::path::Path::new("."), None)?;

    // The live pull request, not the proposal's copy: `draft` and the labels
    // already applied are exactly the two things that move between the review
    // and this call, and both change the answer.
    let read = GitHubRead::from_env()?;
    let live = read.pull_request(&repo_id, pr).await?;

    let write = GitHubWrite::from_env()?;
    let added = tinysweeper::issues::pull_request::apply_triage(
        &write,
        &repo_id,
        &live,
        &proposal,
        &loaded.config.issues,
    )
    .await?;

    match added.as_slice() {
        [] => println!("already labelled; nothing to add"),
        labels => println!("added {}", labels.join(", ")),
    }
    Ok(())
}

#[cfg(not(feature = "github"))]
async fn run_triage(_repo: &str, _pr: u64, _findings: &std::path::Path) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "publishing to GitHub",
        "github",
    ))
}

/// Evaluate the auto-merge policy, and merge if it qualifies.
///
/// `--dry-run` reads and decides but is handed no write token at all, rather
/// than being handed one and asked not to use it.
#[cfg(feature = "github")]
async fn run_automerge(repo: &str, pr: u64, dry_run: bool) -> Result<()> {
    use tinysweeper::automerge::types::{Decision, Outcome};
    use tinysweeper::automerge::{evaluate, merge_if_qualified, snapshot};
    use tinysweeper::forge::RepoId;
    use tinysweeper::forge::github::{GitHubRead, GitHubWrite};

    let repo_id = RepoId::parse(repo)
        .ok_or_else(|| tinysweeper::Error::config(format!("`{repo}` is not owner/name")))?;

    let loaded = tinysweeper::config::load_validated(std::path::Path::new("."), None)?;
    let config = &loaded.config.automerge;
    let read = GitHubRead::from_env()?;

    if dry_run {
        let taken = snapshot(&read, &repo_id, pr).await?;
        match evaluate(config, &taken) {
            Decision::Merge => println!("#{pr} qualifies; would merge with `{}`", config.method),
            Decision::Refuse(refusal) => println!("#{pr} not merged: {refusal}"),
        }
        return Ok(());
    }

    let write = GitHubWrite::from_env()?;
    match merge_if_qualified(&read, &write, config, &repo_id, pr).await? {
        Outcome::Merged { method } => println!("#{pr} merged with `{method}`"),
        Outcome::Refused(refusal) => println!("#{pr} not merged: {refusal}"),
        Outcome::Rejected { method, reason } => {
            println!("#{pr} qualified but GitHub refused a `{method}` merge: {reason}")
        }
    }
    Ok(())
}

#[cfg(not(feature = "github"))]
async fn run_automerge(_repo: &str, _pr: u64, _dry_run: bool) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "merging on GitHub",
        "github",
    ))
}

/// A `findings.json` must actually describe the pull request the caller
/// named before anything touches the network.
///
/// `--repo`/`--pr` were only syntax-checked and otherwise discarded, so a
/// proposal reused from a previous run — or simply the wrong file — would
/// have published under whatever repository and number it names, silently,
/// to a token that happens to have access to both.
#[cfg(feature = "github")]
fn validate_apply_target(
    proposal: &tinysweeper::app::Proposal,
    repo: &tinysweeper::forge::RepoId,
    pr: u64,
) -> Result<()> {
    if proposal.number != pr {
        return Err(tinysweeper::Error::config(format!(
            "the proposal is for #{} but --pr says #{pr}",
            proposal.number
        )));
    }
    if proposal.repo != repo.to_string() {
        return Err(tinysweeper::Error::config(format!(
            "the proposal is for `{}` but --repo says `{repo}`",
            proposal.repo
        )));
    }
    Ok(())
}

#[cfg(not(feature = "github"))]
async fn run_apply(_repo: &str, _pr: u64, _findings: &std::path::Path) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "publishing to GitHub",
        "github",
    ))
}

/// Run the webhook server.
#[cfg(feature = "serve")]
async fn run_serve(bind: String, config_path: Option<std::path::PathBuf>) -> Result<()> {
    use tinysweeper::server::{ServerConfig, Store, admin, auth::AppAuth, serve};

    let loaded =
        tinysweeper::config::load_validated(std::path::Path::new("."), config_path.as_deref())?;

    let webhook_secret = require_webhook_secret(std::env::var("TINYSWEEPER_WEBHOOK_SECRET").ok())?;

    // Validated here rather than at first use: a bad admin token should stop
    // the process at startup, not surface as a 401 an operator debugs later.
    let admin_auth = admin::AdminAuth::from_env()?;

    let store = Store::from_env().await?;
    let auth = AppAuth::from_env()?;

    // The embedding provider comes from `[embeddings]` in the config, not from
    // the environment. It is the index partition key, so a second place to set
    // it is a second way for the key to disagree with the vectors already
    // written — and that disagreement is silent, not an error. `serve` opens
    // the provider before it binds, so a half-configured one stops the process
    // rather than quietly running without retrieval.
    serve(
        ServerConfig {
            bind,
            webhook_secret,
            config: loaded.config,
            admin_auth,
        },
        store,
        auth,
    )
    .await
}

#[cfg(not(feature = "serve"))]
async fn run_serve(_bind: String, _config: Option<std::path::PathBuf>) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "running the webhook server",
        "serve",
    ))
}

/// Refuse to start with a missing or empty webhook secret.
///
/// `env::var` returns `Ok("")` for a variable that is set but empty — exactly
/// what happens when `.env.example` is copied without filling it in. An empty
/// HMAC key is trivially forgeable, so that must be rejected the same way an
/// unset secret is.
#[cfg(feature = "serve")]
fn require_webhook_secret(raw: Option<String>) -> Result<String> {
    let secret = raw.ok_or_else(|| {
        tinysweeper::Error::config(
            "TINYSWEEPER_WEBHOOK_SECRET is not set. Without it every delivery would be \
             unauthenticated, so the server refuses to start rather than accept forged events.",
        )
    })?;

    if secret.trim().is_empty() {
        return Err(tinysweeper::Error::config(
            "TINYSWEEPER_WEBHOOK_SECRET is set but empty. An empty secret can be forged by \
             anyone; set it to a real value before starting the server.",
        ));
    }

    Ok(secret)
}

/// Render a proposal for a human reading CI logs.
///
/// Gated on `harness` rather than on both features: `local-review` reaches it
/// without the GitHub adapter linked at all.
#[cfg(feature = "harness")]
fn render(proposal: &tinysweeper::app::Proposal) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} #{} @ {}\n\n",
        proposal.repo,
        proposal.number,
        &proposal.head_sha[..proposal.head_sha.len().min(8)]
    ));
    for lane in &proposal.lanes {
        out.push_str(&format!(
            "  {:<28} {:?}  {}\n",
            lane.check_name, lane.conclusion, lane.summary
        ));
        for finding in &lane.findings {
            out.push_str(&format!(
                "      {}:{} [{}] {}\n",
                finding.path,
                finding.line.unwrap_or(0),
                finding.severity,
                finding.title
            ));
        }
    }
    out.push_str(&format!(
        "\n  {}\n",
        tinysweeper::findings::render::cost_line(&proposal.usage(), &proposal.models)
    ));
    out
}

/// Wire up `tracing` at a level chosen by `-v` flags, unless `RUST_LOG` says
/// otherwise.
fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "tinysweeper=info",
        1 => "tinysweeper=debug",
        _ => "tinysweeper=trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn review_parses_repo_and_pr() {
        let cli = Cli::try_parse_from([
            "tinysweeper",
            "review",
            "--repo",
            "tinyhumansai/tinysweeper",
            "--pr",
            "7",
            "--dry-run",
        ])
        .expect("parses");
        match cli.command {
            Command::Review {
                repo, pr, dry_run, ..
            } => {
                assert_eq!(repo, "tinyhumansai/tinysweeper");
                assert_eq!(pr, 7);
                assert!(dry_run);
            }
            other => panic!("expected review, got {other:?}"),
        }
    }

    #[test]
    fn triage_parses_repo_pr_and_a_default_proposal() {
        let cli = Cli::try_parse_from([
            "tinysweeper",
            "triage",
            "--repo",
            "tinyhumansai/tinysweeper",
            "--pr",
            "7",
        ])
        .expect("parses");
        match cli.command {
            Command::Triage { repo, pr, findings } => {
                assert_eq!(repo, "tinyhumansai/tinysweeper");
                assert_eq!(pr, 7);
                assert_eq!(findings, std::path::PathBuf::from("findings.json"));
            }
            other => panic!("expected triage, got {other:?}"),
        }
    }

    #[test]
    fn lanes_accept_a_comma_separated_list() {
        let cli = Cli::try_parse_from([
            "tinysweeper",
            "local-review",
            "--lanes",
            "critique,security",
        ])
        .expect("parses");
        match cli.command {
            Command::LocalReview { lanes, base, .. } => {
                assert_eq!(lanes, ["critique", "security"]);
                assert_eq!(base, "origin/main");
            }
            other => panic!("expected local-review, got {other:?}"),
        }
    }

    #[test]
    fn local_review_defaults_to_the_working_tree_of_the_current_directory() {
        let cli = Cli::try_parse_from(["tinysweeper", "local-review"]).expect("parses");
        match cli.command {
            Command::LocalReview {
                head,
                dir,
                title,
                body,
                ..
            } => {
                // `None` is the working tree, uncommitted changes included.
                // That is the default because the change being iterated on has
                // usually not been committed yet.
                assert_eq!(head, None);
                assert_eq!(dir, std::path::PathBuf::from("."));
                assert_eq!(title, None);
                assert_eq!(body, None);
            }
            other => panic!("expected local-review, got {other:?}"),
        }
    }

    #[test]
    fn local_review_takes_a_description_for_the_description_lane() {
        let cli = Cli::try_parse_from([
            "tinysweeper",
            "local-review",
            "--head",
            "HEAD",
            "--title",
            "feat: add a lane",
            "--body",
            "Why it exists.",
        ])
        .expect("parses");
        match cli.command {
            Command::LocalReview {
                head, title, body, ..
            } => {
                assert_eq!(head.as_deref(), Some("HEAD"));
                assert_eq!(title.as_deref(), Some("feat: add a lane"));
                assert_eq!(body.as_deref(), Some("Why it exists."));
            }
            other => panic!("expected local-review, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_runs_end_to_end_from_parsed_command_line_arguments() {
        // This is deliberately the repository configuration: resolving its
        // preset makes the test cross the CLI, configuration discovery,
        // layering, and validation boundaries without credentials or network
        // access.
        let cli = Cli::try_parse_from(["tinysweeper", "check", "."]).expect("parses");

        dispatch(cli.command).await.expect("validates the config");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn a_missing_webhook_secret_is_rejected() {
        let err = require_webhook_secret(None).unwrap_err().to_string();
        assert!(err.contains("is not set"), "{err}");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn an_empty_webhook_secret_is_rejected_like_a_missing_one() {
        // `TINYSWEEPER_WEBHOOK_SECRET=` (set but empty) must fail the same
        // way as leaving it unset — an empty HMAC key is forgeable by anyone.
        for empty in ["", "   ", "\t\n"] {
            let err = require_webhook_secret(Some(empty.to_string()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("is set but empty"), "{err}");
        }
    }

    #[cfg(feature = "github")]
    fn proposal_for(repo: &str, number: u64) -> tinysweeper::app::Proposal {
        tinysweeper::app::Proposal {
            overview: None,
            unreviewed: vec![],
            version: 1,
            repo: repo.into(),
            number,
            head_sha: "abc123".into(),
            lanes: vec![],
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            embed_tokens: 0,
            models: vec![],
            threads: Default::default(),
        }
    }

    #[cfg(feature = "github")]
    #[test]
    fn apply_refuses_a_proposal_for_a_different_repository() {
        let repo = tinysweeper::forge::RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        let err = validate_apply_target(&proposal_for("someone-else/other-repo", 7), &repo, 7)
            .unwrap_err()
            .to_string();
        assert!(err.contains("someone-else/other-repo"), "{err}");
    }

    #[cfg(feature = "github")]
    #[test]
    fn apply_refuses_a_proposal_for_a_different_pull_request() {
        let repo = tinysweeper::forge::RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        let err = validate_apply_target(&proposal_for("tinyhumansai/tinysweeper", 3), &repo, 7)
            .unwrap_err()
            .to_string();
        assert!(err.contains("#3"), "{err}");
    }

    #[cfg(feature = "github")]
    #[test]
    fn apply_accepts_a_matching_proposal() {
        let repo = tinysweeper::forge::RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        validate_apply_target(&proposal_for("tinyhumansai/tinysweeper", 7), &repo, 7)
            .expect("matches");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn a_real_webhook_secret_is_accepted() {
        assert_eq!(
            require_webhook_secret(Some("it's a secret to everybody".into())).unwrap(),
            "it's a secret to everybody"
        );
    }
}
