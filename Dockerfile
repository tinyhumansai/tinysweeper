# syntax=docker/dockerfile:1

# tinysweeper ships as one binary. This image exists for self-hosted runners and
# for running the reviewer outside Actions; there is no server mode, so it has no
# port, no healthcheck, and nothing long-running. The offline default build is
# what CI and `local-review` use, not this.

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
# checkout with real git commands.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /usr/sbin/nologin tinysweeper

COPY --from=builder /out/tinysweeper /usr/local/bin/tinysweeper
COPY presets/ /opt/tinysweeper/presets/

ENV TINYSWEEPER_PRESETS_DIR=/opt/tinysweeper/presets \
    RUST_LOG=tinysweeper=info

USER tinysweeper
WORKDIR /home/tinysweeper

ENTRYPOINT ["tinysweeper"]
CMD ["--help"]
