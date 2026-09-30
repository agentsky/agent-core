# The agentd image: the release binary on distroless's glibc base, run as
# uid 10001, the sandbox user, so the volume directories agentd creates are
# the sandbox's to write (docs/impl-notes.md, "Agent-writable directories
# are given to the sandbox user").
#
# Build it from the repository root, or with `docker compose build` in
# deploy/compose:
#
#   docker build -f images/agentd/Dockerfile -t agent-core/agentd:dev .
#
# The build and runtime stages share a Debian release, so the binary finds
# the glibc it was linked against. RUST_VERSION matches `rust-version` in
# Cargo.toml.

ARG RUST_VERSION=1.98.1

FROM rust:${RUST_VERSION}-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=agent-core-agentd-target \
    cargo build --locked --release -p agentd \
    && install -m 0755 target/release/agentd /usr/local/bin/agentd \
    && install -d -m 0700 /rootfs/var/lib/agentd

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /usr/local/bin/agentd /usr/local/bin/agentd
COPY --from=build --chown=10001:10001 /rootfs/var/lib/agentd /var/lib/agentd
USER 10001:10001
EXPOSE 8443 8080 8081
ENTRYPOINT ["/usr/local/bin/agentd"]
CMD ["serve", "--config", "/etc/agentd/agentd.toml"]
