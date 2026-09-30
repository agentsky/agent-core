# Implementation notes

Unexpected issues met while implementing [`tasks-plan.md`](tasks-plan.md), and
how they were solved. Newest entries go at the end of their task's section.
When a note changes a decision in the plan or the design, the same PR updates
that document too.

## T01: workspace skeleton

### reqwest 0.13 defaults to aws-lc-rs, not ring

**Issue.** The plan chose rustls on the `ring` provider. reqwest 0.13 made
`aws-lc-rs` its only built-in rustls provider: its `rustls` feature enables
`aws-lc-rs`, and the provider-free alternative, `rustls-no-provider`, panics
at `Client::new()` unless a provider is installed first (`reqwest`
`src/async_impl/client.rs`, `default_rustls_crypto_provider`). Staying on
`ring` would mean installing it before every client is built, including in
every library crate's tests.

**Solution.** Use reqwest's `rustls` feature, so `aws-lc-rs` is the one
provider in the tree. `tokio-tungstenite` with `rustls-tls-native-roots`
resolves to the same provider, and `cargo tree` shows no `ring`, `openssl-sys`
or `native-tls`. A probe build with reqwest, tokio-tungstenite, sqlx (SQLite)
and bollard compiled in 32 seconds on this machine. `aws-lc-sys` needs a C
compiler (and `cmake` on some targets), which the GitHub Ubuntu runners have.

Consequences:

- The workspace declares reqwest with `default-features = false` and only
  `json`. Crates that talk HTTPS add the `rustls` feature. `agentctl` talks
  plain HTTP to `agentctl.internal` and doesn't, so its static musl build
  stays free of C code.
- T02's license policy has to allow the `OpenSSL` license for `aws-lc-sys`
  (its expression is `ISC AND (Apache-2.0 OR ISC) AND OpenSSL`) as a
  per-crate exception. (Superseded: current `aws-lc-sys` releases no longer
  use that license; see
  [T02](#aws-lc-sys-no-longer-needs-an-openssl-exception).)
- The plan's Libraries table and T02 are updated to match.

### cargo-llvm-cov ignores `default-members`

**Issue.** The plan relied on `default-members = [".", "crates/*"]` to make
the existing `cargo coverage` alias cover every crate. `cargo test` honors it,
and every crate's tests ran under coverage, but `cargo llvm-cov` still
reported only the root package's files. The gate would have measured
`src/lib.rs` alone.

**Solution.** Add `--workspace` to the `coverage` alias in
`.cargo/config.toml`, and to the CI step that prints the summary
(`cargo llvm-cov report --workspace --summary-only`). The report now lists
every crate.

## T02: dependency policy

### cargo-deny checks only the root package by default

**Issue.** The root `Cargo.toml` is a package as well as the workspace, not a
virtual manifest. For such a manifest cargo-deny makes the root package the
only root of the crate graph, so the member crates' dependencies are never
checked. The action's default arguments (`--all-features`) have this gap: a
throwaway commit adding `native-tls` to `agentd` passed `cargo deny check
bans`.

**Solution.** Pass `--workspace`, which makes every workspace member a root.
With it the same commit fails. cargo-deny has no configuration key for this,
so the CI job passes it through the action's `arguments`, and the README and
the header of `deny.toml` give the full local command. `all-features = true`
lives in `deny.toml`'s `[graph]` table, so local runs match CI without
repeating it. The job also passes `--locked`, so it checks the committed
`Cargo.lock` instead of silently re-resolving it.

### aws-lc-sys no longer needs an OpenSSL exception

**Issue.** The plan, following the T01 note, expected `aws-lc-sys` to need an
`OpenSSL` license exception. That was true up to `aws-lc-sys` 0.38. Since
0.39 its expression is `ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND
BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)`,
and `aws-lc-rs` 1.18, which reqwest 0.13.5 resolves to, requires
`aws-lc-sys` 0.45. An `OpenSSL` exception would never match, and cargo-deny
warns about unmatched exceptions.

**Solution.** No exception. The allowlist was derived from a scratch copy of
the workspace in which `agentd` depends on every crate in
`[workspace.dependencies]`, with reqwest's `rustls` feature, and each license
was kept only if removing it made `cargo deny --workspace check licenses`
fail. That gives MIT, Apache-2.0, BSD-3-Clause (`aws-lc-sys`, `matchit`,
`subtle`), ISC (`aws-lc-rs`, `aws-lc-sys`, `rustls-webpki`, `untrusted`),
Unicode-3.0 (the ICU crates and `unicode-ident`), Zlib (`foldhash`) and
CDLA-Permissive-2.0 (`webpki-root-certs`, pulled in by
`rustls-platform-verifier` through reqwest's `rustls` feature; `webpki-roots`
is not in the graph). BSD-2-Clause, which the plan listed, is not needed.
Today's lockfile uses only a few of these licenses, so `deny.toml` sets
`unused-allowed-license = "allow"` to keep CI free of warnings about the
rest.

### cargo-deny 0.20 has no warning level for unmaintained crates

**Issue.** The plan says `[advisories]` warns on unmaintained crates and that
`[licenses]` denies GPL, LGPL and AGPL. In cargo-deny 0.20 (the version the
action's `v2` tag ships) every advisory that applies is an error:
`unmaintained` only chooses the scope (`all`, `workspace`, `transitive`,
`none`) in which unmaintained advisories are reported at all. The
`[licenses] deny` list is deprecated and ignored: any license not in `allow`
or an exception is rejected.

**Solution.** `deny.toml` keeps `unmaintained = "all"`, and the CI job passes
`-W unmaintained` to `cargo deny check`, which lowers that one lint to a
warning. A scratch check with `paste` (RUSTSEC-2024-0436) passes with a
warning; one with `smallvec` 1.6.0 (RUSTSEC-2021-0003) still fails. GPL,
LGPL, AGPL and MPL-2.0 are rejected by being absent from `allow`; a scratch
path dependency carrying each of them failed `check licenses`, while
`MIT OR GPL-3.0-only` passed.

### The action's image ships Rust 1.85

**Issue.** `EmbarkStudios/cargo-deny-action@v2` runs in a `rust:1.85.0`
image, and cargo-deny runs that image's `cargo metadata`. Cargo 1.85 reads
today's manifests and lockfile, including the full dependency set, but it
predates the workspace's `rust-version` (1.98.1) and could fail on a
dependency that uses a newer manifest feature or edition.

**Solution.** The job sets the action's `rust-version: stable`, so the action
switches to the current stable toolchain before running cargo-deny, like the
other jobs.

## T03: core-types

### The `Surface` trait's `Sender` has no runtime to come from

**Issue.** The design's trait takes `tx: Sender<InboundEvent>`, which reads
as a `tokio::sync::mpsc::Sender`. T03 limits core-types to `serde`,
`serde_json`, `uuid`, `time`, `thiserror` and `async-trait`, so no runtime's
channel is available. `std::sync::mpsc` blocks, which an async surface must
not do.

**Solution.** core-types defines `Sender<T>`, a cloneable handle over an
`Arc<dyn Sink<T>>`, where `Sink` is an `#[async_trait]` trait with one
method, `send(item) -> Result<(), SendError>`. The receiving side (agentd,
testkit's `MockSurface`) wraps its tokio sender in a small local type that
implements `Sink`. `SendError` converts to `SurfaceError::Closed`, so a
surface's event loop can end with `tx.send(event).await?`.

### `history` couldn't read a thread

**Issue.** The design's `history(&self, conv: &ConvRef, …)` names only a
conversation. Both of its callers read a thread: T23's turn message builder
("thread messages since the agent's last reply") and `agentctl history`
("more thread context"). Slack's `conversations.replies` and Rocket.Chat's
`chat.getThreadMessages` both need the thread root, which a `ConvRef` doesn't
carry.

**Solution.** `history` takes `thread: &ThreadKey`. A `ThreadKey` without a
root reads the conversation's top level, which is what a DM's continuous
session needs. The design's trait is updated to match. The rustdoc fixes the
order: at most `limit` messages, the newest ones older than `before`, oldest
first.

### `is_dm` couldn't tell a group DM from a channel

**Issue.** T22's router returns a `ScopeKind`, which has `GroupDm`, but
`InboundEvent` only had `is_dm: bool`. Nothing an event carried could produce
`GroupDm`.

**Solution.** `InboundEvent` has `conv_kind: ConvKind` (`Dm`, `GroupDm`,
`Channel`) instead, with an `is_dm()` method. T12 (Rocket.Chat room type `d`)
and T28 (Slack `channel_type`) are updated to set it.
`ScopeKey::for_conversation(kind, conv)` builds the matching scope.

### Key strings and separators inside platform ids

**Issue.** `dm:<surface>:<team>:<conv>` is ambiguous if an id contains `:`.
Slack's ids never do, but the Rocket.Chat "team" is whatever id agentd picks
for a server, and a `host:port` is a natural choice. A `/` in an id would
likewise break `VolumeKey`'s `<agent>/<scope>`.

**Solution.** Inside key strings, `%`, `:` and `/` in ids are written as
`%25`, `%3A` and `%2F`. Ids without them, which is every id Slack and
Rocket.Chat produce today, appear unchanged. Parsing accepts only what
`Display` writes (no lowercase or other escapes, and no raw `/`), and IDs
parse only in lowercase hyphenated form, so a key string that parses always
renders back to itself and one key never has two spellings in a database
column. A scope key never contains `/`, so it is safe as one path segment.
`MemberKey` and `ConvRef` have the same `<surface>:<team>:<id>` string form,
for columns such as T23's `requester_key`.

### `post` returns one `MsgRef`

**Issue.** T29's acceptance said `post` renders and splits through `render`,
but `post` returns a single `MsgRef`, and T23 records a `message_refs` row
for every chunk after calling `render` itself.

**Solution.** `Surface::post` sends one already-rendered chunk, as its
rustdoc says, and `Surface::render` does the converting and splitting. T29's
acceptance is reworded to test `render` instead.
