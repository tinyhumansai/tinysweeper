//! The `tinysweeper` command-line entry point.
//!
//! One binary, three entry points that matter. `review` is what the GitHub Action
//! runs, and `local-review` is the same engine over a local git range with no
//! GitHub item and no tokens — which is how prompt changes get iterated without
//! burning pull requests.
//!
//! `serve` runs the webhook server, which is how tinysweeper reaches many
//! repositories without a workflow file in each of them.

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
    },

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Review {
            repo,
            pr,
            config,
            lanes,
            dry_run,
            propose_to,
        } => run_review(&repo, pr, config, lanes, dry_run, &propose_to).await,
        Command::Apply { repo, pr, findings } => run_apply(&repo, pr, &findings).await,
        Command::LocalReview { .. } => not_yet("local-review", "M3"),
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
    tinysweeper::app::apply(&read, &write, &loaded.config, &proposal).await?;

    println!("published {} check run(s)", proposal.lanes.len());
    Ok(())
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
    use tinysweeper::server::{ServerConfig, Store, auth::AppAuth, serve};

    let loaded =
        tinysweeper::config::load_validated(std::path::Path::new("."), config_path.as_deref())?;

    let webhook_secret = require_webhook_secret(std::env::var("TINYSWEEPER_WEBHOOK_SECRET").ok())?;

    let store = Store::from_env().await?;
    let auth = AppAuth::from_env()?;

    serve(
        ServerConfig {
            bind,
            webhook_secret,
            config: loaded.config,
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
/// Only reachable when a review can actually run, which needs both features.
#[cfg(all(feature = "github", feature = "harness"))]
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
        tinysweeper::findings::render::cost_line(
            proposal.cost_usd,
            proposal.input_tokens,
            proposal.output_tokens,
            proposal.cached_tokens,
            &proposal.models,
        )
    ));
    out
}

/// Placeholder for a subcommand whose milestone has not landed yet.
///
/// The CLI surface is declared in full from the start so downstream workflows
/// and the composite action can be written against a stable interface while
/// the internals are still being filled in.
fn not_yet(command: &str, milestone: &str) -> Result<()> {
    Err(tinysweeper::Error::config(format!(
        "`tinysweeper {command}` is not implemented yet (scheduled for {milestone})"
    )))
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
    fn unimplemented_commands_name_their_milestone() {
        let err = not_yet("review", "M3").unwrap_err();
        assert!(err.to_string().contains("scheduled for M3"), "{err}");
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
            version: 1,
            repo: repo.into(),
            number,
            head_sha: "abc123".into(),
            lanes: vec![],
            cost_usd: 0.0,
            cached_tokens: 0,
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
