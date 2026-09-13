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
| `deploy/nginx/sweeper.tinyhumans.ai.conf` | Host nginx vhost; only `/webhook`, `/healthz`, `/admin` reach the app |
| `deploy/mongo/` | mongod/mongot config, secrets generator, init scripts |
| `deploy/cortexdb/` | The box's shared CortexDB stack — the memory engine `.tinysweeper.toml` points at |

## The box

| | |
| --- | --- |
| Host | `174.138.35.76`, SSH as `droid` |
| Checkout | `/opt/tinysweeper` (owned by `droid`) |
| Public name | `https://sweeper.tinyhumans.ai`, Cloudflare-proxied to the host's nginx |
| App port | `127.0.0.1:8081` (8080 belongs to another service on the box) |
| MongoDB | `127.0.0.1:27017`, loopback only |
| CortexDB | `/opt/cortexdb`, reached as `http://cortexdb:3141` on `tinysweeper_default`; `127.0.0.1:3142` on the host |

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

### The memory engine, before the first `up`

`.tinysweeper.toml` turns `[memory]` on against `http://cortexdb:3141`, and
the server **refuses to boot** when an enabled engine cannot be reached — a
silently forgetful reviewer would be worse. So the engine comes up *before*
the stack does, and `CORTEX_API_KEY` goes in `.env` alongside the rest.

On the box it is one shared CortexDB for every service (teeny and tinysweeper
today), run from `/opt/cortexdb` with the files in `deploy/cortexdb/`. It joins
each client's compose network, which is what makes the name `cortexdb` resolve
from the server container, so the client network is created first:

```sh
docker network create --label com.docker.compose.project=tinysweeper \
  --label com.docker.compose.network=default tinysweeper_default

sudo install -d -o droid -g droid /opt/cortexdb
cp deploy/cortexdb/docker-compose.yml deploy/cortexdb/docker-compose.teeny.yml /opt/cortexdb/
cp deploy/cortexdb/.env.example /opt/cortexdb/.env && chmod 600 /opt/cortexdb/.env
$EDITOR /opt/cortexdb/.env      # CORTEX_API_KEY, LADDER_API_KEY
cd /opt/cortexdb
# With teeny on the box: its network and its data volume already exist, and
# its own CortexDB has to be stopped first — two engines on one volume is
# corruption, and the old one holds port 3142. This is the handoff the box
# went through on 2026-09-13; afterwards teeny's compose file drops its
# cortexdb and tika services and `up -d --remove-orphans` retires them.
docker compose --project-directory /home/droid/teeny/deploy \
  -f /home/droid/teeny/deploy/compose.prod.yaml \
  --env-file /home/droid/teeny/deploy/.env stop cortexdb tika
docker compose -f docker-compose.yml -f docker-compose.teeny.yml up -d --wait
# Without teeny (a replacement host, a laptop): the base file alone.
docker compose up -d --wait
docker network connect cortexdb_default ladder   # repeat if the ladder is recreated
cd /opt/tinysweeper
```

The engine's own model calls — embeddings, extraction, answers — go to the
`ladder`, the box's model router, which no project here provisions: it is a
container of its own that binds the host's loopback, which is why it is
attached to `cortexdb_default` by hand above. A host without one sets
`LADDER_URL` in `/opt/cortexdb/.env` to any OpenAI-compatible endpoint that
serves the `vectors` (3072-dimensional), `flash` and `reasoning` model names,
with `LADDER_API_KEY` its bearer, and skips the `network connect` line.

The teeny overlay adopts `teeny_cortexdb-data`, the volume teeny's own
CortexDB wrote before the engine became shared on 2026-09-13; back it up with
the others. A checkout that wants no engine at all edits
`[memory] enabled = false` out of the mounted config — there is deliberately no
environment switch, because a config that says memory is on and a server that
quietly runs without it is the failure this refuses.

```sh
cd /opt/tinysweeper
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --wait
curl -fsS https://sweeper.tinyhumans.ai/healthz
```

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
| `TINYSWEEPER_ALLOWED_ORG` | Organisation manual reviews are bounded to. Defaults to `tinyhumansai`. |
| `LANGFUSE_*` | Optional tracing; see the README. |
| `CORTEX_API_KEY` | The shared CortexDB's bearer, the value `/opt/cortexdb/.env` was started with. Required while `[memory]` is on. |

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
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --remove-orphans --wait
```

`--wait` makes the job fail if the new container never becomes healthy, and
Compose leaves the previous container in place until the new one is created,
so a bad image costs one failed run rather than an outage. The same two
commands, run by hand on the box, are the whole of a manual deploy.

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
