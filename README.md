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

The repository is a Cargo workspace: the crates live in `crates/`, and plain
`cargo` commands at the root cover all of them. The work is planned in
[`docs/tasks-plan.md`](docs/tasks-plan.md), which implements
[`docs/design.md`](docs/design.md). Unexpected issues met along the way and
their solutions are recorded in [`docs/impl-notes.md`](docs/impl-notes.md).

`cargo coverage` is an alias (in `.cargo/config.toml`) for `cargo llvm-cov`
that fails if line coverage is below 85%. Change `--fail-under-lines` there to
move the threshold. It needs `cargo install cargo-llvm-cov` and
`rustup component add llvm-tools-preview`.

## CI

GitHub Actions runs the same formatting, lint, test, doc, and coverage checks
on pushes to `main` and on pull requests, plus a `cargo check` on the minimum
supported Rust version declared in `Cargo.toml`. The formatting, lint, test
and doc checks run on both x86_64 and aarch64 Linux. Dependabot keeps actions
and crates up to date.

A change that touches only documentation (Markdown files and `LICENSE`, as
decided by `scripts/ci/docs-only.sh`) skips the build and test jobs and runs
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
