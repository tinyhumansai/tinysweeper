# syntax=docker/dockerfile:1

# tinysweeper ships as one binary with several entry points. The image is built
# with the full feature set because a hosted deployment needs the harness, the
# GitHub adapter and the webhook server; the offline default build is what CI
# and `local-review` use, not this.

# --- builder ---------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder

ARG FEATURES=all

WORKDIR /src

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# Cache mounts keep the registry and the target dir warm across builds. The
# binary is copied out to /out because the target dir is a mount and does not
# survive into the next stage.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features "${FEATURES}" \
    && install -Dm755 target/release/tinysweeper /out/tinysweeper

# --- development -----------------------------------------------------------
# Used by docker-compose.dev.yml, which bind-mounts the repository over
# /workspace so edits rebuild in place.
FROM rust:1-slim-bookworm AS development

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-watch

WORKDIR /workspace
CMD ["cargo", "watch", "-x", "check"]

# --- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# git is a runtime dependency, not a build one: evidence collection walks the
# checkout with real git commands. curl is here for the healthcheck only.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates git curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /usr/sbin/nologin tinysweeper

COPY --from=builder /out/tinysweeper /usr/local/bin/tinysweeper
COPY presets/ /opt/tinysweeper/presets/

ENV TINYSWEEPER_BIND=0.0.0.0:8080 \
    TINYSWEEPER_PRESETS_DIR=/opt/tinysweeper/presets \
    RUST_LOG=tinysweeper=info

USER tinysweeper
WORKDIR /home/tinysweeper

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["tinysweeper"]
CMD ["serve"]
