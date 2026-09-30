# The sandbox image. agentd starts one container per session from it, with
# `sleep infinity` as the command, and execs `claude` into it
# (docs/design.md, "Why the CLI runs inside the sandbox" and "Lifecycle").
# It holds Claude Code's native build at a pinned version, agentctl, and the
# everyday tools agents use, and runs as the non-root user `agent`
# (uid 10001). It has no Node.js.
#
# Build it from the repository root, or with
# `docker compose --profile sandbox build` in deploy/compose:
#
#   docker build -f images/sandbox/Dockerfile -t agent-core/sandbox:dev .
#
# CLAUDE_CODE_VERSION pins Claude Code; CI checks that `claude --version`
# prints it. RUST_VERSION matches `rust-version` in Cargo.toml.

ARG RUST_VERSION=1.98.1
ARG DEBIAN_RELEASE=trixie

# agentctl as one static musl binary: it needs no loader or libc in the
# image. The musl target builds for the architecture being built.
FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS agentctl
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=agent-core-sandbox-target \
    target="$(uname -m)-unknown-linux-musl" \
    && rustup target add "$target" \
    && cargo build --locked --release -p agentctl --target "$target" \
    && install -m 0755 "target/$target/release/agentctl" /usr/local/bin/agentctl

FROM debian:${DEBIAN_RELEASE}-slim

ARG CLAUDE_CODE_VERSION=2.1.285

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git jq ripgrep tini \
    && rm -rf /var/lib/apt/lists/*

# The native installer puts the pinned version under a scratch HOME
# (~/.local/share/claude/versions/, linked from ~/.local/bin/claude). The
# binary is copied to /usr/local/bin, since sessions get a HOME of their
# own, and the scratch HOME is removed. A copy outside ~/.local/bin is left
# alone by the auto-updater, which DISABLE_AUTOUPDATER also turns off.
RUN export HOME=/tmp/claude-install \
    && mkdir "$HOME" \
    && curl -fsSL -o "$HOME/install.sh" https://claude.ai/install.sh \
    && bash "$HOME/install.sh" "$CLAUDE_CODE_VERSION" \
    && install -m 0755 "$(readlink -f "$HOME/.local/bin/claude")" /usr/local/bin/claude \
    && version="$(DISABLE_AUTOUPDATER=1 claude --version)" \
    && rm -rf "$HOME" \
    && if [ "$version" != "$CLAUDE_CODE_VERSION (Claude Code)" ]; then \
        echo "installed Claude Code reports '$version', not $CLAUDE_CODE_VERSION" >&2; \
        exit 1; \
    fi

COPY --from=agentctl /usr/local/bin/agentctl /usr/local/bin/agentctl

RUN groupadd --gid 10001 agent \
    && useradd --uid 10001 --gid 10001 --create-home --shell /bin/bash agent

ENV DISABLE_AUTOUPDATER=1
WORKDIR /volume
USER 10001:10001
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["sleep", "infinity"]
