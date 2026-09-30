# The agentd image: the release binary on Debian slim with git, which
# agentd runs to clone skills (`/agent skill add`), and CA certificates for
# it. It runs as uid 10001, the sandbox user, so the volume directories
# agentd creates are the sandbox's to write (docs/impl-notes.md,
# "Agent-writable directories are given to the sandbox user").
#
# Build it from the repository root, or with `docker compose build` in
# deploy/compose:
#
#   docker build -f images/agentd/Dockerfile -t agent-core/agentd:dev .
#
# The build and runtime stages share a Debian release, so the binary finds
# the glibc it was linked against. The base images are pinned by digest;
# the tag is there for readers, so change them together. The Rust tag
# matches `rust-version` in Cargo.toml.

FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=agent-core-agentd-target \
    cargo build --locked --release -p agentd \
    && install -m 0755 target/release/agentd /usr/local/bin/agentd \
    && install -d -m 0700 /rootfs/var/lib/agentd

FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/agentd /usr/local/bin/agentd
COPY --from=build --chown=10001:10001 /rootfs/var/lib/agentd /var/lib/agentd
USER 10001:10001
EXPOSE 8443 8080 8081
ENTRYPOINT ["/usr/local/bin/agentd"]
CMD ["serve", "--config", "/etc/agentd/agentd.toml"]
