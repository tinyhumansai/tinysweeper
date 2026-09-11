# Deploying tinysweeper to one box

tinysweeper runs as a Docker Compose stack on a single DigitalOcean droplet:
mongod and mongot, the server, and Caddy for TLS. There is no Kubernetes
cluster. The compose files are the deployment manifest, this file is the
runbook, and `.github/workflows/deploy.yml` is the button that rolls a new
image.

| File | Role |
| --- | --- |
| `docker-compose.yml` | The stack: MongoDB pair, the server built locally |
| `docker-compose.prod.yml` | Overlay: published image, Caddy, no host ports on the app |
| `deploy/Caddyfile` | Reverse proxy; only `/webhook`, `/healthz`, `/admin` reach the app |
| `deploy/mongo/` | mongod/mongot config, secrets generator, init scripts |

## Sizing

The MongoDB pair is the memory floor: mongot holds its indexes in the JVM and
mongod wants its cache. A **4 GB** droplet runs comfortably; 2 GB will OOM
mongot during a full index build. Attach a block-storage volume if the indexed
repositories are large — `mongod-data` and `mongot-data` are named volumes and
can be pointed at it.

Use an Ubuntu 24.04 LTS image. Its 6.8 kernel is below the 6.19 cutoff at
which the MongoDB community-server image refuses to start (see
`docker-compose.kernel-bypass.yml`); do not pick a bleeding-edge kernel for
this host.

## First-time setup

Once, by hand, as root on a fresh droplet:

```sh
# Docker Engine with the compose plugin, from Docker's own repository.
curl -fsSL https://get.docker.com | sh

# A dedicated deploy user. It owns the checkout and may talk to Docker, and
# that is the whole of what it can do.
useradd --create-home --shell /bin/bash --groups docker deploy
install -d -m 700 -o deploy -g deploy /home/deploy/.ssh
# Paste the public half of the key that becomes DEPLOY_SSH_KEY:
install -m 600 -o deploy -g deploy /dev/stdin /home/deploy/.ssh/authorized_keys <<'KEY'
ssh-ed25519 AAAA... tinysweeper-deploy
KEY

# Only 22, 80 and 443 are reachable. MongoDB is bound to loopback by the
# compose file, but the firewall is what makes that a property of the box
# rather than of one file.
ufw allow OpenSSH && ufw allow 80/tcp && ufw allow 443/tcp && ufw allow 443/udp
ufw --force enable

install -d -o deploy -g deploy /opt/tinysweeper
```

Then, as `deploy`:

```sh
cd /opt/tinysweeper
git clone --recurse-submodules https://github.com/tinyhumansai/tinysweeper .

cp .env.example .env
chmod 600 .env
$EDITOR .env             # see "Configuration" below
$EDITOR .tinysweeper.toml  # the deployment's policy: models, budget, embeddings

docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --wait
curl -fsS https://$TINYSWEEPER_DOMAIN/healthz
```

The first `up` generates the MongoDB keyfile and the mongot password into the
`mongo-secrets` volume, initiates the single-member replica set, and waits for
mongot to come up before starting the server. It takes a minute or two. Caddy
asks Let's Encrypt for a certificate as soon as it starts, so the DNS `A`
record for `TINYSWEEPER_DOMAIN` must already point at the droplet.

Point the GitHub App's webhook URL at `https://<TINYSWEEPER_DOMAIN>/webhook`.

## Configuration

Everything the stack reads lives in `/opt/tinysweeper/.env`, which Compose
loads automatically. Beyond the variables in `.env.example`, the production
overlay needs:

| Variable | Meaning |
| --- | --- |
| `TINYSWEEPER_DOMAIN` | Public hostname. Caddy provisions the certificate for it. Required. |
| `MONGO_ROOT_PASSWORD` | Root password for the bundled MongoDB. Required. Generate with `openssl rand -hex 24`. |
| `TINYSWEEPER_IMAGE_TAG` | Image tag the box tracks. Defaults to `latest`. Set to a `sha-…` tag to pin. |
| `TINYSWEEPER_ADMIN_TOKEN` | What `manual-review.yml` authenticates with. Unset means no `/admin` router. |
| `LANGFUSE_*` | Optional tracing; see the README. |

`.env` and `.tinysweeper.toml` are the only two files on the box that are not
in git. Back them up somewhere with the same care as the App's private key,
because together they *are* the deployment.

## Rolling a new image

Merging to `main` publishes `ghcr.io/tinyhumansai/tinysweeper:latest` (and a
`sha-<commit>` tag). Nothing deploys on its own. Dispatch **Deploy** from the
Actions tab; it opens an SSH session as `deploy` and runs, in
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
| `DEPLOY_SSH_KEY` | secret | Private key matching `deploy`'s `authorized_keys`. |
| `DEPLOY_SSH_KNOWN_HOSTS` | secret | Output of `ssh-keyscan -t ed25519 <host>`. |
| `DEPLOY_HOST` | variable | The droplet's address. |
| `DEPLOY_USER` | variable | Defaults to `deploy`. |
| `DEPLOY_STACK_DIR` | variable | Defaults to `/opt/tinysweeper`. |
| `TINYSWEEPER_SERVER_URL` | variable | `https://<TINYSWEEPER_DOMAIN>`; probed after the rollout. |

### Changing the compose files themselves

The deploy workflow pulls an *image*, not the repository. When a change to
`docker-compose*.yml`, the `Caddyfile` or anything under `deploy/mongo/`
lands, update the checkout on the box before dispatching:

```sh
cd /opt/tinysweeper && git pull --ff-only && git submodule update --init --recursive
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

**Backups**: the state that matters is the two named volumes plus `.env` and
`.tinysweeper.toml`. Droplet snapshots cover all of it; for a logical backup,
`tsc exec mongod mongodump --archive -u … | gzip > backup.gz`.
