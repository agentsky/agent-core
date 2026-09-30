# agent-core

[![CI](https://github.com/agentsky/agent-core/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/agentsky/agent-core/actions/workflows/ci.yml?query=branch%3Amain)
[![Tests](https://github.com/agentsky/agent-core/raw/badges/main/tests.svg)](https://github.com/agentsky/agent-core/actions/workflows/ci.yml?query=branch%3Amain)
[![Coverage](https://github.com/agentsky/agent-core/raw/badges/main/coverage.svg)](https://github.com/agentsky/agent-core/actions/workflows/ci.yml?query=branch%3Amain)

Pure-Rust core for @mentionable Claude Code agents in Slack and Rocket.Chat.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo coverage
```

It needs Rust 1.98.1 or newer (`rust-version` in `Cargo.toml`). The repository is a Cargo workspace: the crates live in `crates/`, and plain
`cargo` commands at the root cover all of them. The work is planned in
[`docs/tasks-plan.md`](docs/tasks-plan.md), which implements
[`docs/design.md`](docs/design.md). Unexpected issues met along the way and
their solutions are recorded in [`docs/impl-notes.md`](docs/impl-notes.md).

`cargo coverage` is an alias (in `.cargo/config.toml`) for `cargo llvm-cov`
that fails if line coverage is below 85%. Change `--fail-under-lines` there to
move the threshold. It needs `cargo install cargo-llvm-cov` and
`rustup component add llvm-tools-preview`.

## Running agentd

agentd reads one TOML file, documented key by key in
[`config/agentd.example.toml`](config/agentd.example.toml), and takes its
secrets from the environment only:

```bash
export AGENTD_MASTER_KEY="$(agentd gen-key)"   # keep it: it decrypts stored secrets
agentd migrate --config /etc/agentd/agentd.toml
agentd serve --config /etc/agentd/agentd.toml
```

`serve` also applies pending migrations when it starts; `migrate` is for
running them as a separate step. Configuration errors name the key or
variable at fault. A near miss of a secret's name is refused, Kubernetes
service links such as `AGENTD_PORT` are skipped, and any other variable
starting with `AGENTD_` is ignored with a warning. Each listener binds
agentd's own address on its network, never `0.0.0.0`, and the proxy and ctl
listeners must be inside `internal.sandbox_subnet`.
`GET /healthz` on the public listener answers 200 while the database does.
The ctl listener serves the agentctl API that sandboxed agents call back
through; at startup agentd deletes every agentctl token and scope lock and
empties `ctl-outbox/` under `store.data_dir`, since the containers they
belonged to are gone.
On SIGTERM or SIGINT agentd stops accepting connections and gives in-flight
requests `server.drain_timeout_secs` to finish; a second signal drops them at
once. Logs go to standard error,
human-readable on a terminal and one JSON object per line otherwise.

## Development stack

[`deploy/compose`](deploy/compose/README.md) runs Rocket.Chat, MongoDB and
agentd with Docker Compose, on the two networks the plan describes, and
builds the sandbox image agentd starts sessions from. Its README walks
through bringing it up, setting up the Rocket.Chat manager, configuring
agentd, and the manual live checks. The images are built from
[`images/sandbox`](images/sandbox/Dockerfile), which pins the Claude Code
version with the `CLAUDE_CODE_VERSION` build argument, and
[`images/agentd`](images/agentd/Dockerfile).

## CI

GitHub Actions runs the same formatting, lint, test, doc, and coverage checks
on pushes to `main` and on pull requests, plus a `cargo check` on the minimum
supported Rust version declared in `Cargo.toml`. The formatting, lint, test
and doc checks run on both x86_64 and aarch64 Linux. Dependabot keeps actions
and crates up to date. The `agentctl-static` job builds `agentctl` for
`x86_64-unknown-linux-musl` and fails if `readelf -l` shows an `INTERP`
segment, so the binary copied into the sandbox image needs no dynamic
loader.

The `deny` job enforces the dependency policy in `deny.toml` with
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny): no OpenSSL or
`native-tls`, only the permissive licenses listed there, no crate with a
known vulnerability (unmaintained crates only warn), and crates.io as the only
source; `scripts/ci/check-path-deps.sh` also rejects path dependencies other
than the workspace's own crates. The job runs weekly on `main` as well, so a
newly published advisory fails a run of its own. Run it locally with
`cargo install cargo-deny --locked`,
`cargo deny --workspace --locked check -W unmaintained` and
`sh scripts/ci/check-path-deps.sh`.

The `docker-tests` job runs the tests that need a Docker daemon: they are
named `docker_*` and marked ignored, so the other jobs skip them. Run them
locally, with Docker running, as
`cargo test --workspace -- --ignored docker_`. They pull
`debian:stable-slim` and create and remove their own networks and
containers.

The `images` job builds both images through the Compose file, without
pushing them, and runs `scripts/ci/compose-test.sh`: the sandbox image
prints the pinned `claude --version`, runs as uid 10001 and has no `node`,
and a container on the Compose `sandbox` network reaches agentd's ports 8080
and 8081 but not its public port, Rocket.Chat, MongoDB, the host or the
internet. `deploy/compose/README.md` says how to run it locally.

A change that touches only documentation (Markdown files and `LICENSE`
outside `crates/` and `images/`, as decided by `scripts/ci/docs-only.sh`)
skips the build and test jobs and runs
only the doctests and rustdoc, since this README is also the crate docs. To
protect `main`, require the `CI passed` check: it passes when every other job
passed or was skipped.

The tests and coverage badges above are generated on every push to `main`
and stored as SVG files on the `badges` branch
(`scripts/ci/publish-badges.sh`, rendered by `scripts/ci/badge.sh`), so they
need no external service and render in a private repository too. The job
that publishes them checks that GitHub serves them as images from the
rendered README (`scripts/ci/verify-badges-render.sh`), and a docs-only push
to `main`, which publishes nothing, runs the same check.

## License

Copyright Sky Computing LLC. Licensed under the Functional Source License,
Version 1.1, Apache License 2.0 Future License (`FSL-1.1-ALv2`). See `LICENSE`
for the full text.
