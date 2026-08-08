//! The `tinysweeper` command-line entry point.
//!
//! One binary, two entry points that matter. `review` is what the GitHub Action
//! runs, and `local-review` is the same engine over a local git range with no
//! GitHub item and no tokens — which is how prompt changes get iterated without
//! burning pull requests.
//!
//! There is no `serve`. tinysweeper runs in Actions; see the note on the `all`
//! feature in Cargo.toml for why.

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

    let _ = RepoId::parse(repo)
        .ok_or_else(|| tinysweeper::Error::config(format!("`{repo}` is not owner/name")))?;

    let proposal = tinysweeper::app::read_proposal(findings)?;
    if proposal.number != pr {
        return Err(tinysweeper::Error::config(format!(
            "the proposal is for #{} but --pr says #{pr}",
            proposal.number
        )));
    }

    let read = GitHubRead::from_env()?;
    let write = GitHubWrite::from_env()?;
    tinysweeper::app::apply(&read, &write, &proposal).await?;

    println!("published {} check run(s)", proposal.lanes.len());
    Ok(())
}

#[cfg(not(feature = "github"))]
async fn run_apply(_repo: &str, _pr: u64, _findings: &std::path::Path) -> Result<()> {
    Err(tinysweeper::Error::FeatureDisabled(
        "publishing to GitHub",
        "github",
    ))
}

/// Render a proposal for a human reading CI logs.
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
        "\n  ${:.4} · {} cached prompt tokens\n",
        proposal.cost_usd, proposal.cached_tokens
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
}
