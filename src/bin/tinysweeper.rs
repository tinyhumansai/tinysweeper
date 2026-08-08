//! The `tinysweeper` command-line entry point.
//!
//! One binary, several entry points. `review` is what the GitHub Action runs,
//! `serve` is what the hosted GitHub App runs, and `local-review` is the same
//! engine over a local git range with no GitHub item and no tokens — which is
//! how prompt changes get iterated without burning pull requests.

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

    /// Run the webhook server for the hosted GitHub App deployment.
    Serve {
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1:8080", env = "TINYSWEEPER_BIND")]
        bind: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Review { .. } => not_yet("review", "M3"),
        Command::LocalReview { .. } => not_yet("local-review", "M3"),
        Command::Check { path } => tinysweeper::app::check(&path),
        Command::Doctor { path, json } => tinysweeper::app::doctor(&path, json),
        Command::Serve { .. } => not_yet("serve", "M10"),
    }
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
