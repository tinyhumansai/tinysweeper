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

docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --wait
curl -fsS https://sweeper.tinyhumans.ai/healthz
```

The first `up` generates the MongoDB keyfile and the mongot password into the
`mongo-secrets` volume, initiates the single-member replica set, and waits for
mongot to come up before starting the server. It takes a minute or two.

The GitHub App's webhook URL is `https://sweeper.tinyhumans.ai/webhook`; it did
not change in the move, only the origin behind Cloudflare did.

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
