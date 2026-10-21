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
$EDITOR .env               # see "Configuration" below; .tinysweeper.toml is tracked

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

Start the stack and verify the server:

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
| `TINYSWEEPER_REPOSITORY_CACHE_DIR` | Server-local Git object cache. Defaults to `/var/cache/tinysweeper/repositories`, backed by the `repository-cache` named volume. |
| `TINYSWEEPER_ADMIN_TOKEN` | What `manual-review.yml` authenticates with. Unset means no `/admin` router. |
| `TINYSWEEPER_PREVIEW_TOKEN` | What the UI preview action in other repositories' CI authenticates with (`templates/ui-preview/README.md`). Unset means no `/preview` routes. Pairs with `[preview] enabled` and `public_base_url` in `.tinysweeper.toml`, and optionally `models.vision`. |
| `TINYSWEEPER_MCP_TOKEN` | Dedicated bearer for `/mcp`. Unset means the MCP router is not mounted. Keep it separate from the admin and preview credentials. |
| `TINYSWEEPER_ALLOWED_ORG` | Organisation manual reviews are bounded to. Defaults to `tinyhumansai`. |
| `LANGFUSE_BASE_URL`, `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY` | Optional direct Langfuse export; provide all three. `LANGFUSE_ENVIRONMENT` tags traces. Alternatively set `TINYHUMANS_LANGFUSE_PROXY_URL` and `TINYHUMANS_AUTH_TOKEN` for the authenticated proxy. |
| `SENTRY_DSN` | Optional server error reporting. Create a Sentry project for the deployed server and set its DSN here; never commit the DSN. |

`.env` is the only file on the box that is not in git; `.tinysweeper.toml` is
tracked. Back `.env` up somewhere with the same care as the App's private key,
because it *is* the deployment.

The server's bare Git object cache lives in the `repository-cache` volume and
survives image replacement. It contains repository objects, not GitHub tokens;
installation credentials are supplied only to individual fetch commands. Keep
the cache path at the mounted location when overriding it in `.env`.

To enable external tracing, provision a Langfuse project and put its endpoint
and keys in `.env` (or use the authenticated proxy variables above). To enable
Sentry reporting, provision a server project and set `SENTRY_DSN`. Restart the
server after changing these values. Keep all DSNs and keys in `.env` or the
operator's secret manager; the repository contains no production credentials.

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

## Upgrading to 0.1.2

Repository memory was removed in 0.1.2. Remove `[memory]` and any
`[[memory.questions]]` tables from the mounted `.tinysweeper.toml` before
deploying; the new image rejects them as unknown configuration. Remove
`CORTEX_API_KEY` from TinySweeper's environment and Compose forwarding.
The `memory` CLI command and `/admin/memory/` endpoints were retired, so
remove calls to them from operator scripts. Mongo retrieval and per-head
review state remain available.

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

**Backups**: Mongo state is in the `mongod-data` and `mongot-data` volumes,
and deployment credentials are in `.env`. The repository cache is disposable
and refills from GitHub if removed. Droplet snapshots cover all of it; for a logical backup,
`tsc exec mongod mongodump --archive -u … | gzip > backup.gz`.
