# AGENTS.md

## Pull request branches

Name pull request branches `<agent-name>/<name-describing-the-task>`, in
lowercase kebab-case, for example `claude/add-coverage-gate`.

- `<agent-name>` names the coding agent, not the model: use `claude` for all
  Claude models, `codex` for Codex, `copilot` for GitHub Copilot.
- `<name-describing-the-task>` is a short summary of the change.

## Code layout

The work is planned task by task in [`docs/tasks-plan.md`](docs/tasks-plan.md),
which implements [`docs/design.md`](docs/design.md). Read the task, the design
sections it links to, and the plan's "Decisions" and "Definition of done"
before starting.

- The root package keeps `README.md` as its rustdoc. Every other crate lives
  in `crates/<name>/`. The workspace's `default-members` makes plain
  `cargo test` and `cargo clippy` cover all of them, and the `cargo coverage`
  alias passes `--workspace` because `cargo llvm-cov` ignores
  `default-members`.
- Third-party dependency versions live only in the root `Cargo.toml` under
  `[workspace.dependencies]`. Crates use `workspace = true` and add extra
  features in their own `Cargo.toml`.
- `Cargo.lock` is committed and CI builds with `--locked`. Commit lockfile
  changes with the change that causes them.
- `testkit` is only ever a dev-dependency.
- `core-types` does no I/O.
- Surface crates (`surface-slack`, `surface-rocketchat`) never depend on each
  other.

## Implementation notes

Record anything unexpected you hit while implementing a task, and how you
solved it, in [`docs/impl-notes.md`](docs/impl-notes.md): a library that
behaves differently from what the plan assumed, a platform limit, a decision
the plan didn't cover. Add the entry in the same PR as the change.
