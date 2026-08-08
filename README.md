<h1 align="center">tinysweeper</h1>

<p align="center">
  A GitHub review bot that fans a pull request out across independently-gated check runs.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/tinyhumansai/tinysweeper?style=flat-square" alt="License" /></a>
  <a href="https://github.com/tinyhumansai/tinysweeper/stargazers"><img src="https://img.shields.io/github/stars/tinyhumansai/tinysweeper?style=flat-square" alt="Stars" /></a>
  <a href="https://github.com/tinyhumansai/tinysweeper/issues"><img src="https://img.shields.io/github/issues/tinyhumansai/tinysweeper?style=flat-square" alt="Issues" /></a>
  <a href="https://github.com/tinyhumansai/tinysweeper/pulls"><img src="https://img.shields.io/github/issues-pr/tinyhumansai/tinysweeper?style=flat-square" alt="Pull requests" /></a>
  <a href="https://github.com/tinyhumansai/tinysweeper/commits/main"><img src="https://img.shields.io/github/last-commit/tinyhumansai/tinysweeper?style=flat-square" alt="Last commit" /></a>
  <img src="https://img.shields.io/badge/status-work%20in%20progress-orange?style=flat-square" alt="Status" />
</p>

> [!WARNING]
> Work in progress. The CLI surface is declared in full, but most subcommands
> are still returning "not implemented yet". See [ROADMAP.md](ROADMAP.md) for
> what has landed.

## Why another review bot

The incumbents produce one giant review from one giant model call. Everything
arrives as a suggestion, nothing is separable, and there is no way to require
only the parts you trust — so teams either take the noise or turn the whole
thing off.

tinysweeper splits the review into **lanes**. Each lane is a separate agent with
a narrow job, a narrow slice of evidence, and its own GitHub check run:

| Check run | What it looks at |
| --- | --- |
| `tinysweeper/critique` | Correctness of the diff, with the surrounding code pulled in as context |
| `tinysweeper/security` | Dependency changes, new network/exec sites, workflow permission widening |
| `tinysweeper/tests` | Whether changed behaviour is actually covered, and whether the assertions mean anything |
| `tinysweeper/commits` | Secrets, large blobs, vendored junk and other dirt committed into the history |
| `tinysweeper/description` | Whether the PR body matches what the diff actually does |
| `tinysweeper/gate` | The deterministic aggregate — the one check to require in branch protection |

Because they are separate check runs, branch protection can require exactly the
lanes you trust, and a noisy lane can be switched off without losing the rest.

Deterministic scanners run **before** any model call, so a committed private key
fails for free and the model is only asked to adjudicate what a scanner already
flagged.

## Design commitments

- **The model never holds a write token.** Lanes run against a read-only
  checkout; write credentials are minted only in the apply path, after every
  model call has returned.
- **Contributor code is never executed.** tinysweeper reads the diff and the
  tree. It does not build, install dependencies, or run the target repo's
  scripts.
- **Automation never parses prose.** Review and apply communicate through hidden
  HTML markers carrying a verdict, a head SHA and a confidence score.
- **An empty review is a valid review.** Padding a finding list with style
  preferences is treated as a defect.
- **Offline by default.** The default build links no HTTP client, and the test
  suite never touches the network.

## Install

```sh
git clone --recurse-submodules https://github.com/tinyhumansai/tinysweeper
cd tinysweeper
cargo build --release --features all
```

## Use it in a repository

```yaml
# .github/workflows/tinysweeper.yml
name: tinysweeper
on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review, edited]
  issue_comment:
    types: [created]
  pull_request_review:
    types: [submitted]
concurrency:
  group: tinysweeper-${{ github.event.pull_request.number || github.event.issue.number }}
  cancel-in-progress: true
jobs:
  review:
    uses: tinyhumansai/tinysweeper/.github/workflows/review.yml@v1
    secrets:
      openrouter-api-key: ${{ secrets.TINYSWEEPER_OPENROUTER_KEY }}
```

That is the whole installation. The review itself lives in the reusable
workflow, so improvements reach every repository without editing any of them.

Configure behaviour with a `.tinysweeper.toml` at the repository root, and
validate it with `tinysweeper check`. See [docs/triggers.md](docs/triggers.md)
for what fires when — including the things GitHub emits no event for at all,
and how fork pull requests differ.

## Run the engine locally

`local-review` runs every lane over a local git range, with no GitHub item and
no tokens — useful before you push, and the way prompt changes get iterated.

```sh
tinysweeper local-review --base origin/main
```

## No server

tinysweeper runs entirely in GitHub Actions. There is no webhook receiver to
host, no public endpoint, and no App private key sitting on a machine somewhere.

That is a deliberate choice rather than a missing feature: Actions already
receives every repository event a server would subscribe to, and the two things
a server is usually wanted for — reacting to a resolved review thread, or to a
👎 on a comment — have no webhook event at all, so a server could not do them
either. tinysweeper reads that state when it runs instead.

## Built on

[TinyAgents](https://github.com/tinyhumansai/tinyagents), a recursive
language-model harness for Rust, vendored at `vendor/tinyagents`. Models are
reached through an OpenAI-compatible gateway, so OpenRouter, Moonshot and
MiniMax are all the same code path.

## Documentation

- [ROADMAP.md](ROADMAP.md) — what has landed and what is next
- [CONTRIBUTING.md](CONTRIBUTING.md) — local checks and pull request expectations
- [AGENTS.md](AGENTS.md) — conventions for humans and agents working in this repo
- `docs/` — module documentation and design notes

## License

GPL-3.0-only. See [LICENSE](LICENSE).
