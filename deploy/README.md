# Deploying tinysweeper to one box

tinysweeper runs as a Docker Compose stack on a shared DigitalOcean droplet:
mongod and mongot and the server, behind the host's nginx and Cloudflare.
There is no Kubernetes cluster. The compose files are the deployment manifest,
this file is the runbook, and `.github/workflows/deploy.yml` is the button that
rolls a new image.

| File | Role |
| --- | --- |
| `docker-compose.yml` | The stack: MongoDB pair, the server built locally |
| `docker-compose.prod.yml` | Overlay: published image, loopback port, memory cap |
| `deploy/nginx/sweeper.tinyhumans.ai.conf` | Host nginx vhost; only `/webhook`, `/healthz`, `/admin`, `/preview` reach the app |
| `deploy/mongo/` | mongod/mongot config, secrets generator, init scripts |

## The box

| | |
| --- | --- |
| Host | `174.138.35.76`, SSH as `droid` |
| Checkout | `/opt/tinysweeper` (owned by `droid`) |
| Public name | `https://sweeper.tinyhumans.ai`, Cloudflare-proxied to the host's nginx |
| App port | `127.0.0.1:8081` (8080 belongs to another service on the box) |
| MongoDB | `127.0.0.1:27017`, loopback only |
| CortexDB | The box's `cortex` container, reached as `http://cortexdb:3141` on `tinysweeper_default`; `127.0.0.1:3141` on the host |

The box is shared with other services and its own nginx on 80/443, which is
why this stack publishes nothing but a loopback port and the vhost is a file
in `/etc/nginx/sites-enabled` like the others. It runs Ubuntu 24.04 on a 6.8
kernel, below the 6.19 cutoff at which the MongoDB community-server image
refuses to start (see `docker-compose.kernel-bypass.yml`); do not move it to a
bleeding-edge kernel.

## First-time setup

Done once, on 2026-09-11. Recorded so it can be repeated on a replacement box.

```sh
ssh droid@174.138.35.76
sudo install -d -o droid -g droid /opt/tinysweeper
cd /opt/tinysweeper
git clone --recurse-submodules https://github.com/tinyhumansai/tinysweeper .

cp .env.example .env && chmod 600 .env
$EDITOR .env               # see "Configuration" below (CORTEX_API_KEY included); .tinysweeper.toml is tracked

# nginx: the port 80 block first, so certbot can answer the challenge.
sudo install -m 644 deploy/nginx/sweeper.tinyhumans.ai.conf /etc/nginx/sites-available/
sudo ln -s ../sites-available/sweeper.tinyhumans.ai.conf /etc/nginx/sites-enabled/
sudo certbot certonly --webroot -w /var/www/certbot -d sweeper.tinyhumans.ai
sudo nginx -t && sudo systemctl reload nginx
```

The nginx file also defines a private TLS ladder listener on
`172.17.0.1:7443`. It reuses the server certificate; Compose maps
`sweeper.tinyhumans.ai` to the host gateway for this connection. The ladder
continues to validate its bearer, and no model route is exposed on public
ports. Install this nginx file before using the shipped HTTPS model endpoints.
This satisfies Embed's TLS requirement for bearer routes outside loopback.

### The memory engine, before the first `up`

`.tinysweeper.toml` turns `[memory]` on against
`http://host.docker.internal:3141`. The server refuses to boot when an enabled
engine cannot be reached, so configure `CORTEX_API_KEY` in `.env` first.

The shared CortexDB container (`cortex`) lives on a separate Docker network.
It publishes port 3141 on both `127.0.0.1` and the Docker host gateway
`172.17.0.1`; Compose maps `host.docker.internal` to that gateway. This avoids
an extra network attachment that disappears whenever Cortex is recreated.
TinySweeper neither defines nor starts the shared engine. Keep its bearer in
`.env`; do not print the container environment to recover credentials.

Isolation between the services sharing the engine is by CortexDB scope, not by
network or key: tinysweeper writes under `owner:<org>/repo:<name>/section:…`,
teeny under `agent:…`, and scope ids are what keep one service's memories out
of another's recall.

This engine embeds at 1024 dimensions. Until 2026-09-13 tinysweeper used a
separate 3072-dimensional CortexDB stack in `/opt/cortexdb` (container
`cortexdb-cortexdb-1`, port 3142, volume `teeny_cortexdb-data`); its two
tinysweeper scopes (443 events) were moved with `POST /v1/export` on the old
side and `POST /v1/import` on the new, re-embedded on import, and the stack was
taken down. The export and the import receipts are in `~/cortex-migration/` on
the box, and the old volume is still there — it is the only copy of the
pre-migration data, so leave it until nobody wants the rollback.

A host without a shared engine (a replacement box, a laptop) runs its own
CortexDB, publishes port 3141 on the Docker host gateway, and sets the
key in `.env`. The minimum is one container against any OpenAI-compatible
endpoint `$U` that serves an embedding model and a chat model — the ladder
serves them as `vectors-oai3` (1024-dimensional) and `flash`:

```sh
docker run -d --name cortex --restart unless-stopped \
  -p 127.0.0.1:3141:3141 -p 172.17.0.1:3141:3141 -v cortex-data:/data \
  -e CORTEX_API_KEY=<bearer> -e CORTEX_DEPLOYMENT_PRESET=on_prem_enterprise -e CORTEX_BIND_ALL=1 \
  -e CORTEX_EMBEDDING_URL=$U -e CORTEX_EMBEDDING_MODEL=vectors-oai3 -e CORTEX_EMBEDDING_DIMS=1024 \
  -e CORTEX_LLM_URL=$U -e CORTEX_LLM_MODEL=flash \
  -e CORTEX_ENRICHMENT_URL=$U -e CORTEX_ENRICHMENT_MODEL=flash \
  -e CORTEX_ANSWER_PROVIDER=openai -e CORTEX_ANSWER_URL=$U -e CORTEX_ANSWER_MODEL=flash \
  -e CORTEX_VERIFIER_URL=$U -e CORTEX_VERIFIER_MODEL=flash \
  -e OPENAI_API_KEY=<endpoint key> -e LLM_API_KEY=<endpoint key> \
  cortexdb/cortexdb:latest 3141 /data
```

The embedding size is pinned by the first write and cannot change afterwards,
so pick it before ingesting anything. A checkout that wants no engine at all
edits `[memory] enabled = false` out of the mounted config — there is
deliberately no environment switch, because a config that says memory is on
and a server that quietly runs without it is the failure this refuses.

```sh
cd /opt/tinysweeper
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --wait
curl -fsS https://sweeper.tinyhumans.ai/healthz
```

### Backfilling the memory, once per repository

Webhooks keep the engine's `discussions` section current from the moment the
server is up; the history before that — every earlier issue and pull request
and what was said on them, the reviewer's own remarks excluded — is walked
once, from any machine that holds the admin token:

```sh
TINYSWEEPER_SERVER_URL=https://sweeper.tinyhumans.ai \
TINYSWEEPER_ADMIN_TOKEN=… scripts/memory-backfill.sh tinyhumansai/tinysweeper
```

It prints a `resume_from`; a later run with `--since <that>` walks only what
changed, which is the thing to do after the server has been down for a while.
One walk per repository runs at a time; the first over a busy repository takes
minutes and stays well inside the installation's hourly API budget.

## Configuration

Everything the stack reads lives in `/opt/tinysweeper/.env`, which Compose
loads automatically. Beyond the variables in `.env.example`, the production
overlay reads:

| Variable | Meaning |
| --- | --- |
| `MONGO_ROOT_PASSWORD` | Root password for the bundled MongoDB. Required. |
| `TINYSWEEPER_HOST_PORT` | Loopback port nginx proxies to. `8081`; the vhost hard-codes the same number. |
| `TINYSWEEPER_IMAGE_TAG` | Image tag the box tracks. Defaults to `latest`. Set to a `sha-…` tag to pin. |
| `TINYSWEEPER_ADMIN_TOKEN` | What `manual-review.yml` authenticates with. Unset means no `/admin` router. |
| `TINYSWEEPER_PREVIEW_TOKEN` | What the UI preview action in other repositories' CI authenticates with (`templates/ui-preview/README.md`). Unset means no `/preview` routes. Pairs with `[preview] enabled` and `public_base_url` in `.tinysweeper.toml`, and optionally `models.vision`. |
| `TINYSWEEPER_MCP_TOKEN` | Dedicated bearer for `/mcp`. Unset means the MCP router is not mounted. Keep it separate from the admin and preview credentials. |
| `TINYSWEEPER_ALLOWED_ORG` | Organisation manual reviews are bounded to. Defaults to `tinyhumansai`. |
| `LANGFUSE_*` | Optional tracing; see the README. |
| `CORTEX_API_KEY` | The shared CortexDB's bearer, the value the `cortex` container was started with. Required while `[memory]` is on. |

`.env` is the only file on the box that is not in git; `.tinysweeper.toml` is
tracked. Back `.env` up somewhere with the same care as the App's private key,
because it *is* the deployment.

## Rolling a new image

Merging to `main` publishes `ghcr.io/tinyhumansai/tinysweeper:latest` (and a
`sha-<commit>` tag). Nothing deploys on its own. Dispatch **Deploy** from the
Actions tab; it opens an SSH session as `droid` and runs, in
`/opt/tinysweeper`:

```sh
docker compose -f docker-compose.yml -f docker-compose.prod.yml pull tinysweeper
docker compose -f docker-compose.yml -f docker-compose.prod.yml run --rm --no-deps --interactive=false tinysweeper check .tinysweeper.toml
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --remove-orphans --wait
```

The incoming image validates the mounted operator config before replacing the
running server. A rejected config leaves it running; keep finite output caps
and model budget prices current when upgrading. `--interactive=false` keeps
Compose from consuming the remaining SSH script. `--wait` then checks runtime
health. A runtime failure after replacement can still cause an outage; roll
back to the last known good image and configuration if it does. The same three
commands are the manual deploy path.

The workflow needs, on the `production` environment:

| Name | Kind | Meaning |
| --- | --- | --- |
| `DEPLOY_SSH_KEY` | secret | Private key matching `droid`'s `authorized_keys`. |
| `DEPLOY_SSH_KNOWN_HOSTS` | secret | Output of `ssh-keyscan -t ed25519 174.138.35.76`. |
| `DEPLOY_HOST` | variable | `174.138.35.76`. |
| `DEPLOY_USER` | variable | Defaults to `droid`. |
| `DEPLOY_STACK_DIR` | variable | Defaults to `/opt/tinysweeper`. |
| `TINYSWEEPER_SERVER_URL` | variable | `https://sweeper.tinyhumans.ai`; probed after the rollout. |

### Changing the compose files themselves

The deploy workflow pulls an *image*, not the repository. When a change to
`docker-compose*.yml`, `deploy/nginx/` or anything under `deploy/mongo/`
lands, update the checkout on the box before dispatching:

```sh
cd /opt/tinysweeper && git pull --ff-only && git submodule update --init --recursive
# and, for the vhost:
sudo install -m 644 deploy/nginx/sweeper.tinyhumans.ai.conf /etc/nginx/sites-available/
sudo nginx -t && sudo systemctl reload nginx
```

This is deliberate: a `git pull` in the deploy path would make the box's
manifest follow whatever is on `main` at the moment of the click, and a config
change is a thing to do with eyes on it.

## Operating it

```sh
alias tsc='docker compose -f docker-compose.yml -f docker-compose.prod.yml'
tsc ps                        # health of every service
tsc logs -f tinysweeper       # the server
tsc logs --since 1h mongot    # the search process, when retrieval looks off
tsc restart tinysweeper       # restart without pulling
tsc exec mongod mongosh -u tinysweeper -p "$MONGO_ROOT_PASSWORD" --authenticationDatabase admin
```

**Rolling back** is a deploy with an older tag: dispatch **Deploy** with
`image_tag` set to the `sha-…` of the last good commit, or set
`TINYSWEEPER_IMAGE_TAG` in `.env` and `tsc up -d tinysweeper`.

**Upgrading MongoDB** means bumping *both* image pins in `docker-compose.yml`
together; mongod and mongot are versioned independently and must match. Take a
snapshot of the droplet first.

**Backups**: the state that matters is the two named volumes plus `.env`.
Droplet snapshots cover all of it; for a logical backup,
`tsc exec mongod mongodump --archive -u … | gzip > backup.gz`.
