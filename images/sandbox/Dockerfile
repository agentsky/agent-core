# The sandbox image. agentd starts one container per session from it, with
# `sleep infinity` as the command under Docker's init, and execs `claude`
# into it (docs/design.md, "Why the CLI runs inside the sandbox" and
# "Lifecycle"). It holds Claude Code's native build at a pinned version,
# agentctl, and the everyday tools agents use, and runs as the non-root
# user `agent` (uid 10001). It has no Node.js, and no entrypoint: the
# sandbox crate asks Docker for its init (`init: true`), which reaps
# processes as PID 1.
#
# Build it from the repository root, or with
# `docker compose --profile sandbox build` in deploy/compose:
#
#   docker build -f images/sandbox/Dockerfile -t agent-core/sandbox:dev .
#
# CLAUDE_CODE_VERSION pins Claude Code; CI checks that `claude --version`
# prints it. The build downloads that version's binary for the target
# architecture from Claude Code's release bucket, as the native installer
# does, and refuses it unless its SHA-256 is the one pinned below, taken
# from the release's manifest.json
# (<CLAUDE_CODE_RELEASES>/<version>/manifest.json, `platforms.linux-x64`
# and `platforms.linux-arm64`). Change the version and both checksums
# together.
#
# The base images are pinned by digest; the tag is there for readers.
# Change them together. The Rust tag matches `rust-version` in Cargo.toml.

ARG CLAUDE_CODE_VERSION=2.1.285
ARG CLAUDE_CODE_SHA256_X64=33dad1ec615a2e08cc78b494f05c110e49916de2c79d78ec8799ebf46b233d29
ARG CLAUDE_CODE_SHA256_ARM64=24fac77749bed3d91365d6b6915aa4b824e14318ecb6bc17adbc192f01c9173d
ARG CLAUDE_CODE_RELEASES=https://downloads.claude.ai/claude-code-releases

# agentctl as one static musl binary: it needs no loader or libc in the
# image. The musl target builds for the architecture being built.
FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS agentctl
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=agent-core-sandbox-target \
    target="$(uname -m)-unknown-linux-musl" \
    && rustup target add "$target" \
    && cargo build --locked --release -p agentctl --target "$target" \
    && install -m 0755 "target/$target/release/agentctl" /usr/local/bin/agentctl

FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS base
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git jq ripgrep \
    && rm -rf /var/lib/apt/lists/*

# Claude Code, downloaded and checked by an unprivileged user in a stage of
# its own, so nothing it runs at build time runs as root and the download
# never lands in a layer of the image.
FROM base AS claude
ARG TARGETARCH
ARG CLAUDE_CODE_VERSION
ARG CLAUDE_CODE_SHA256_X64
ARG CLAUDE_CODE_SHA256_ARM64
ARG CLAUDE_CODE_RELEASES
RUN install -d -o nobody -g nogroup /download
USER nobody
WORKDIR /download
RUN case "$TARGETARCH" in \
        amd64) platform=linux-x64 sha256="$CLAUDE_CODE_SHA256_X64" ;; \
        arm64) platform=linux-arm64 sha256="$CLAUDE_CODE_SHA256_ARM64" ;; \
        *) echo "no Claude Code build for architecture '$TARGETARCH'" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o claude "$CLAUDE_CODE_RELEASES/$CLAUDE_CODE_VERSION/$platform/claude" \
    && echo "$sha256  claude" | sha256sum --check --strict - \
    && chmod 0755 claude \
    && version="$(HOME=/tmp DISABLE_AUTOUPDATER=1 ./claude --version)" \
    && if [ "$version" != "$CLAUDE_CODE_VERSION (Claude Code)" ]; then \
        echo "the downloaded Claude Code reports '$version', not $CLAUDE_CODE_VERSION" >&2; \
        exit 1; \
    fi

FROM base
COPY --from=agentctl /usr/local/bin/agentctl /usr/local/bin/agentctl
COPY --from=claude /download/claude /usr/local/bin/claude

RUN groupadd --gid 10001 agent \
    && useradd --uid 10001 --gid 10001 --create-home --shell /bin/bash agent

ENV DISABLE_AUTOUPDATER=1
WORKDIR /volume
USER 10001:10001
CMD ["sleep", "infinity"]
