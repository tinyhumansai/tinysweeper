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

- **The model never holds a write token.** Lanes take a `ForgeRead` and only the
  apply path takes a `ForgeWrite`, so a lane structurally cannot mutate a pull
  request. The installation token used for writing is minted after every model
  call has returned.
- **Contributor code is never executed.** tinysweeper reads the diff and the
  tree. It does not build, install dependencies, or run the target repo's
  scripts.
- **Automation never parses prose.** Review and apply communicate through hidden
  HTML markers carrying a verdict, a head SHA and a confidence score.
- **An empty review is a valid review.** Padding a finding list with style
  preferences is treated as a defect.
- **Offline by default.** The default build links no HTTP client, and the test
  suite never touches the network.

## How it runs

tinysweeper is a **GitHub App**: one server, installed on as many repositories
as you like, receiving webhooks. There is no workflow file to add to a
repository and no Action to pin — installing the App is the whole installation,
and improvements reach every repository at once because there is only one place
running the code.

There used to be a second path — a reusable workflow and a composite action.
It has been removed. Two distribution paths meant two trigger models, two
credential models and two things to keep honest, and the workflow half could
not carry the one thing that justified the server: a contributor whitelist is a
fact about a *person over time*, and a stateless job has nowhere to keep one.

### Run the server

```sh
git clone --recurse-submodules https://github.com/tinyhumansai/tinysweeper
cd tinysweeper
cargo build --release --features all
cp .env.example .env    # fill it in
./target/release/tinysweeper serve
```

The App needs `checks:write`, `contents:read`, `issues:write`,
`pull_requests:write` and `metadata:read`, and a webhook pointed at
`/webhook`. `deploy/github-app-manifest.json` and `scripts/create-github-app.sh`
create one with those settings. A Docker image is published from `Dockerfile`
by CI.

The server refuses to start without `TINYSWEEPER_WEBHOOK_SECRET`: an unsigned
delivery endpoint is a way for anyone to make the bot review anything.

Configure per-repository behaviour with a `.tinysweeper.toml` at the repository
root, and validate it with `tinysweeper check`. See
[docs/triggers.md](docs/triggers.md) for what fires when — including the things
GitHub emits no event for at all.

### Admin API

Operator endpoints live under `/admin`, guarded by a bearer token in
`TINYSWEEPER_ADMIN_TOKEN` and compared in constant time. **When that variable is
unset the admin router is not mounted at all**, so a misconfigured deployment
loses the API rather than exposing it.

```sh
curl -H "Authorization: Bearer $TINYSWEEPER_ADMIN_TOKEN" \
  https://tinysweeper.example/admin/contributors/octocat

curl -X PUT -H "Authorization: Bearer $TINYSWEEPER_ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"trust":"blocked","note":"spam pull requests"}' \
  https://tinysweeper.example/admin/contributors/octocat/trust
```

Index status (`/admin/index/…`) and knowledge documents
(`/admin/knowledge/…`) are declared and return `501` until their stores land.

### Run the engine locally

`local-review` runs every lane over a local git range, with no GitHub item and
no tokens — useful before you push, and the way prompt changes get iterated.

```sh
tinysweeper local-review --base origin/main
```

## Built on

[TinyAgents](https://github.com/tinyhumansai/tinyagents), a recursive
language-model harness for Rust, vendored at `vendor/tinyagents`. Models are
reached through an OpenAI-compatible gateway, so OpenRouter, Moonshot and
MiniMax are all the same code path.

## Documentation

- [ROADMAP.md](ROADMAP.md) — what has landed and what is next
- [CONTRIBUTING.md](CONTRIBUTING.md) — local checks and pull request expectations
- [AGENTS.md](AGENTS.md) — conventions for humans and agents working in this repo
- [docs/triggers.md](docs/triggers.md) — what wakes tinysweeper up, and what emits no event at all
- [docs/modules/server/README.md](docs/modules/server/README.md) — the server, its security boundary, and the admin API
- `docs/` — module documentation and design notes

## License

GPL-3.0-only. See [LICENSE](LICENSE).
