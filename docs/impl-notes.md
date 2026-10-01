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
  per-crate exception.
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

### `[licenses.private]` exempts any unpublished crate

**Issue.** `[licenses.private] ignore = true` is meant to exempt our own
crates, which carry only `license-file`. cargo-deny applies it to every crate
with `publish = false`, wherever it comes from. A scratch copy of the
workspace in which `agentd` depends on `vendor/gpl`, a path crate with
`license = "GPL-3.0-only"` and `publish = false`, printed `licenses ok`.
cargo-deny has no setting that limits the exemption to workspace members:
`[licenses.private]` only adds private registries, `[sources]` does not see
path dependencies, and a `[[licenses.clarify]]` entry per workspace crate
would have to be added for every new crate and pinned to the hash of
`LICENSE`.

**Solution.** Keep `private.ignore` and add `scripts/ci/check-path-deps.sh`,
which the `deny` job runs before cargo-deny. It reads
`cargo metadata --locked --all-features --format-version 1` and fails when a
package with no source (a path package) has a manifest other than the root
`Cargo.toml` or `crates/<name>/Cargo.toml`. Matching the manifest path, not
the `workspace_members` list, also rejects a vendored crate added to
`[workspace] members` outside `crates/`. The same scratch case fails the
script, as do a path dependency outside the repository, an optional one
behind a feature, a Windows-only one, one nested under
`crates/agentd/vendor/`, and a `[patch.crates-io]` entry pointing at a local
copy; with a stale lockfile, `--locked` makes it fail too. A crate placed
directly in `crates/` is a workspace crate by the layout rules in
`AGENTS.md`, so it is reviewed as our code.

### The `deny` job ran only when code changed

**Issue.** The `deny` job runs only when the `changes` job classifies a
change as code. An advisory published against a crate already in
`Cargo.lock` therefore surfaced on the next code pull request, unrelated to
it, rather than when it was published.

**Solution.** The workflow gains a weekly `schedule` trigger (Mondays at
04:23 UTC). The `changes` job sets a base commit only for `pull_request` and
`push` events, so a scheduled run is classified as code and runs every job,
which also catches breakage from a new stable toolchain. `publish-badges`
still runs only on pushes to `main` and manual dispatches, and the `docs`
job's badge check only on pushes to `main`. Scheduled runs take a
concurrency group of their own: in `main`'s group, where
`cancel-in-progress` is false, a scheduled run arriving while a push run is
pending would cancel that pending run, and its badges would not be
published. GitHub runs schedules on the default branch only, so the trigger
takes effect once this workflow is on `main`.

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
column. A scope key never contains `/`, which keeps `VolumeKey`'s single
separator unambiguous; it is still not a safe file name (see
[below](#scope-keys-are-not-file-or-docker-names)). `MemberKey` and `ConvRef` have the same `<surface>:<team>:<id>` string form,
for columns such as T23's `requester_key`.

### `post` returns one `MsgRef`

**Issue.** T29's acceptance said `post` renders and splits through `render`,
but `post` returns a single `MsgRef`, and T23 records a `message_refs` row
for every chunk after calling `render` itself.

**Solution.** `Surface::post` sends one already-rendered chunk, as its
rustdoc says, and `Surface::render` does the converting and splitting. T29's
acceptance is reworded to test `render` instead.

### The scope lock had no lease id

**Issue.** `LockResponse::Held` carried only an expiry, and renew and
release named no lease: the server could match them only by session. Claude
Code runs Bash tool calls in parallel, so one session can run two
`agentctl lock -- …` at once. Both would get `Held`, and when the first
command exited its `Release` would free the lock while the second command
was still writing to `shared/`.

**Solution.** A new `LeaseId` (a UUID newtype like the other ids) is minted
on every acquire and returned in `LockResponse::Held { lease, expires_at }`.
`LockRequest` is now an enum tagged by `op`, with `Renew { lease }` and
`Release { lease }`, so a renew or release without a lease fails to
deserialize. The lock is exclusive per lease, not per session: a second
acquire, from any session, gets `Busy`, and a renew or release naming any
lease but the current one answers `Released` and changes nothing. T15's
`scope_locks` table takes `lease_id` as its primary key next to
`holder_session`, and its acceptance tests the same-session case.

### Scope keys are not file or Docker names

**Issue.** `ScopeKey`'s rustdoc called its string "safe as one path
segment" because it never contains `/`. It always contains `:`, and may
contain `%` and any other character a platform id holds. A `:` splits a
bollard `binds` entry (`src:dst:ro`), and Docker volume names allow only
`[a-zA-Z0-9][a-zA-Z0-9_.-]*`, so a sandbox that named a directory or a
Docker volume after the key would break or be refused.

**Solution.** The rustdoc of `ScopeKey` and `VolumeKey` now says the string
is a key for columns, labels and logs, not a file or Docker object name. T17
in the plan fixes how volumes are named and mounted: host directories at
`volumes/<agent id>/<lowercase hex SHA-256 of the scope key>` in the agentd
data directory, mounted through bollard's `Mounts` API (`HostConfig::mounts`,
type `bind`), never `binds` strings or named Docker volumes. A digest was
chosen over a reversible encoding of the key (hex or base32), which grows
with the key and could pass the 255-byte file-name limit for a long
Rocket.Chat team id; the `volumes` table records which key a directory holds.

### A Slack bot message may name no user

**Issue.** `InboundEvent::sender` is a required `MemberKey`, but a Slack bot
message may carry only a `bot_id` and no `user`, and the router's
managed-bot lookup (T22) needs one key to look up. The plan did not say what
`sender.user` holds then, or whether the router keys on `sender` or on
`sender_bot_user`.

**Solution.** The router keys on `sender` when `sender_is_bot` is true, and a
surface puts the bot's user id in both `sender.user` and `sender_bot_user`,
so they never disagree: `u._id` on Rocket.Chat, and on Slack the event's
`user`, or the `user_id` from `bots.info` (T29) when the event has only a
`bot_id`. A bot with no known user id keeps its `bot_id` in `sender.user` and
has `sender_bot_user: None`; no binding has that id, so the router ignores
it as an unmanaged bot. `InboundEvent`'s rustdoc has a "Bot senders"
section saying this, and T12, T22, T28 and T29 in the plan match it.

## T04: testkit

### `fake_claude_path` built outside `cargo llvm-cov`'s target directory

**Issue.** The plan expected a nested `$CARGO build` to respect "whatever
target directory is in effect, including `cargo llvm-cov`'s". It doesn't.
`cargo llvm-cov` passes `--target-dir target/llvm-cov-target` on cargo's
command line, and a test process sees only the environment, which does carry
`cargo llvm-cov`'s `RUSTC_WRAPPER`. The nested build therefore went to
`target/debug`, instrumented, where no coverage report looks and where it
disturbs the next plain build. `fake-claude.rs` showed 3.7% line coverage and
pulled the workspace under the 85% gate.

**Solution.** `fake_claude_path()` finds the directory the running test
executable was built in (its nearest ancestor with cargo's `CACHEDIR.TAG`)
and passes it as `--target-dir`. With the inherited wrapper environment the
nested build then matches the outer one: a workspace run reuses the binary
it already built, and a run that didn't build it builds it instrumented in
the same place. `fake-claude.rs` is now at 96% line coverage. The plan's
Testing section says so.

### Clearing the environment loses `fake-claude`'s coverage

**Issue.** Under `cargo llvm-cov`, `fake-claude` is instrumented and writes
its profile where `LLVM_PROFILE_FILE` says. A test that starts it with
`env_clear()`, as a runner passing an explicit launch environment would,
drops that variable: the counts are lost and `default.profraw` lands in the
child's working directory.

**Solution.** testkit's own tests pass `LLVM_PROFILE_FILE` through when it is
set, and `fake_claude_path()`'s rustdoc tells other crates to do the same
(T17's `ProcessSandbox`, T20, T21).

### The real CLI prefers `ANTHROPIC_API_KEY`

**Issue.** T04 said `fake-claude` sends `Authorization: Bearer` from
`CLAUDE_CODE_OAUTH_TOKEN`, and `x-api-key` only when the API key is the only
credential. Against a local capture server, Claude Code 2.1.285 with both
variables set sent only `x-api-key` and reported `apiKeySource:
"ANTHROPIC_API_KEY"`.

**Solution.** `fake-claude` does the same, and its `init` line reports
`apiKeySource` like the real one. The T04 bullet and the plan's Claude Code
CLI section say so, and note that the runner sets exactly one of the two.

### What capturing the fixtures showed

**Issue.** Capturing with an unreachable `ANTHROPIC_BASE_URL`, as the plan
describes, gave ten `system/api_retry` lines and no result within two
minutes: the CLI retries with backoff. It also left open how the CLI behaves
on the paths `fake-claude` imitates.

**Solution.** The captures set `CLAUDE_CODE_MAX_RETRIES=0` (and `IS_SANDBOX=1`,
since the capture ran as root and `bypassPermissions` refuses root otherwise).
Two captures ran against a local server that answers with the same SSE
stream as `fake_anthropic()`, which the CLI accepted, with `--session-id`
and then `--resume`. `fake-claude` follows what they showed:

- `system/init` starts every turn, not only the process. The plan's Claude
  Code CLI section now says so.
- The transcript appears with the first user message, not at start. A
  process reaped before its first turn leaves nothing to `--resume`, so T21
  should mark a session started only after a turn.
- `--session-id` with an existing transcript prints `Error: Session ID … is
  already in use.` to stderr and exits 1. `--resume` without one prints `No
  conversation found with session ID: …` and a `result` line with `subtype:
  "error_during_execution"` and an `errors` list, but no `result`,
  `terminal_reason` or `api_error_status`, then exits 1. Unknown flags and
  both session flags also exit 1 on the real CLI; `fake-claude` keeps the
  plan's status 2 for them so tests can tell usage errors apart.
- Requests go to `/v1/messages?beta=true`, so T18's proxy must forward the
  query string. The CLI also sends `HEAD /api/hello` before the first
  request of each process, and an `x-claude-code-session-id` header.
- A 401 gives `api_error_status: 401` and a synthetic assistant message with
  `error: "authentication_failed"`. The CLI exits 1 when its last result was
  an error and 0 otherwise.
- `rate_limit_event` lines appear after a streamed reply. The
  `active_goal`, `autocompact_state` and `system/commands_changed` lines the
  plan lists didn't appear in these short runs; the fixtures hold only what
  was captured.

Absolute paths in the captures are rewritten to the sandbox layout
(`/volume/sessions/<id>/work` and `…/claude`).

### `rate_limit_event` comes once per process, and only with OAuth

**Issue.** Review asked for `fake-claude` to print a `rate_limit_event`
after every successful API call, so runner tests always meet a line they
must skip. The capture in `tool-turns.jsonl` has one such line for three
API calls, and the CLI's own schema describes the line as "emitted when rate
limit info changes". Re-running Claude Code 2.1.285 against a local
streaming server showed that with `CLAUDE_CODE_OAUTH_TOKEN` it prints one
right after the first `assistant` line of each process, a `--resume`d
process included, and with `ANTHROPIC_API_KEY` it prints none.

**Solution.** `fake-claude` does the same: with the OAuth token, the first
successful turn of each process prints `{"type":"rate_limit_event",
"rate_limit_info":{"status":"allowed","isUsingOverage":false},…}` after its
first `assistant` line. A test checks a tool turn and a reply against the
capture's line sequence. For anything else a parser must skip, including
lines that aren't JSON, a script turn lists raw `extra_lines`, printed after
its commands and before its reply.

### The real CLI's `anthropic-beta` header

**Issue.** `fake-claude` sent `anthropic-beta: oauth-2025-04-20` with the
OAuth token and nothing with an API key. The same capture showed Claude Code
2.1.285 sending a comma list with either credential: ten betas with the
OAuth token, starting `claude-code-20250219,oauth-2025-04-20,…`, and nine
with an API key, without `oauth-2025-04-20` and
`extended-cache-ttl-2025-04-11` but with
`mid-conversation-tool-changes-2026-07-01`. It also sends `x-app: cli`.

**Solution.** `fake-claude` sends the captured lists, exported as
`testkit::claude::OAUTH_BETA` and `API_KEY_BETA` so T18 can check the proxy
forwards them untouched, and `x-app: cli`. `fake_anthropic()` still records
every header.

### `MockSurface` lost an event when its loop ended mid-delivery

**Issue.** The `events` loop took each event off its channel and then sent
it. When the receiver was gone, or the loop was cancelled while the send
waited for room, that event was dropped, although the docs promise that
queued events stay for the next loop. The mock also ignored its own `Caps`
and could not fail, so tests could not drive the core's handling of
`Unsupported`, `RateLimited` or `Unauthorized`.

**Solution.** Each binding's events are a queue under the mock's lock,
with a `Notify` for new events. The loop sends a copy of the front event and
removes it only once the sender has taken it, so neither a closed receiver
nor a cancellation loses it. Calls check the `Caps` first (`edit` without
`supports_edit`, a thread root without `supports_threads`), then a
per-operation queue filled by `fail_next(op, error)`. Failed calls are not
recorded, as a failed upload already wasn't.

### A refused port the test keeps

**Issue.** The unreachable-upstream test bound a port, dropped the
listener and used the port. A server started by a parallel test could take
the port in between, and the request would succeed or hang.

**Solution.** The test binds a `tokio::net::TcpSocket` and never calls
`listen`. The port stays taken for the whole test, so nothing else can get
it, and a connection to a bound socket that isn't listening is refused at
once.

### `fake_claude_path()`'s nested build

**Issue.** Review found the first call blocking the test thread for about
20 seconds on a second dependency build, without `--locked`.

**Solution.** The nested build passes `--locked`, and the rustdoc says the
first call blocks and should come before any timeout. After a workspace
`cargo test`, the nested build finds everything fresh and takes about
0.2 seconds, and under `cargo coverage` the target directory holds a single
build of each dependency. It builds again only when the caller's package selection
resolved testkit's dependencies with other features, which a test process
can't see. A `--profile` flag doesn't change feature resolution: it would
only help under `cargo test --release`, which nothing here runs, so it isn't
passed.

## T05: store

### The key reaches the store through `open`

**Issue.** The plan gives `Store::open(url)` and `Store::open_in_memory()`,
but the store owns encryption, so it needs the master key, and nothing said
how the key gets there.

**Solution.** Both take a `Sealer`: `Store::open(url, sealer)` and
`Store::open_in_memory(sealer)`. agentd builds the `Sealer` with
`Sealer::from_base64(&master_key)` when it loads its configuration (T10), so
a bad key fails at startup rather than at the first login.
`Sealer::generate_key()` writes a key in the form `from_base64` reads, for
`agentd gen-key` and for tests. The key is standard base64 with padding;
surrounding whitespace, such as a trailing newline, is ignored.

### In-memory SQLite needs one connection that never closes

**Issue.** Every connection to a plain `:memory:` database is a separate,
empty database, so a pool sees a different database on each connection.
sqlx's `sqlite::memory:` URL works around that with a shared-cache database
under a unique name, but a shared-cache in-memory database is deleted when
its last connection closes, and the pool closes idle connections after 10
minutes and every connection after 30. Shared cache also swaps
`busy_timeout` for table-level locks, which can fail with `SQLITE_LOCKED`.

**Solution.** `open_in_memory` uses a pool of exactly one connection with no
idle timeout and no maximum lifetime. Concurrent callers queue for it. `open`
routes in-memory URLs (`sqlite::memory:`, or a `mode=memory` parameter) to the
same pool settings, so an agentd test configured with an in-memory URL (T10)
behaves the same way.

### Switching to WAL can't wait on `busy_timeout`

**Issue.** Changing a database into WAL mode needs an exclusive lock that
SQLite's busy handler doesn't wait for (sqlx says so where it declines to
set a journal mode by default). Two connections opening a new file at once
could fail.

**Solution.** `open` sets `journal_mode=WAL` on every connection, but the
pool opens one connection first and runs the migrations on it before
returning, so the switch happens before any concurrency. WAL mode is stored
in the file, and later connections find it already set. A test checks
`journal_mode`, `foreign_keys` and `busy_timeout` on a file database.

### A deferred transaction can't upgrade to a write under contention

**Issue.** In WAL mode, a transaction that reads and then writes fails at
once with `SQLITE_BUSY` if another connection committed in between;
`busy_timeout` doesn't retry it, because the read snapshot is already stale.
sqlx's `begin()` starts such a deferred transaction.

**Solution.** Writes that must be atomic use a single statement or
`BEGIN IMMEDIATE`, which takes the write lock up front and does wait on
`busy_timeout`. `take_pending_login` is one `DELETE … RETURNING`. With a
separate `SELECT` and `DELETE`, the concurrent test (eight callers, twenty
rounds, file database) failed in each of three runs. `ensure_member` checks
for the identity, and if it's missing takes
`pool.begin_with("BEGIN IMMEDIATE")`, checks again and inserts, so concurrent
calls for one identity create one member. Later tasks with read-then-write transactions (T15's scope locks,
T21's sessions) should do the same.

### Timestamps are Unix seconds and IDs are text

**Issue.** sqlx encodes `OffsetDateTime` as RFC 3339 text in the value's own
offset, writing fractional seconds only when they are non-zero. SQLite
compares that text byte by byte, so `expires_at <= ?` is wrong across
offsets, and even in UTC `…:00Z` sorts after `…:00.5Z`. sqlx encodes `Uuid`
as a 16-byte blob, which is unreadable in the `sqlite3` shell and doesn't
match the canonical text form core-types uses in keys.

**Solution.** Timestamps are `INTEGER` Unix seconds (sub-second precision is
dropped), and IDs are `TEXT` in core-types' lowercase hyphenated form, bound
with `to_string()` and parsed back with `FromStr`. A value that doesn't parse
is `StoreError::Corrupt`. Tables are `STRICT`, so a mistyped bind fails
instead of being stored. The foundation migration's header lists these
conventions for later migrations.

### `sqlx::migrate!` doesn't notice new migrations

**Issue.** `sqlx::migrate!` embeds `migrations/` at compile time, but on
stable Rust it can't ask Cargo to watch the directory. Adding a migration
without touching Rust code leaves a stale build that doesn't apply it.

**Solution.** `crates/store/build.rs` prints
`cargo:rerun-if-changed=migrations`. sqlx also checksums applied migrations,
so a migration must never be edited once merged; add a new one instead.

### sqlx 0.9.0 brings older copies of six crates

**Issue.** With sqlx in the lockfile, cargo-deny warned about duplicate
versions: sqlx-core 0.9.0, the latest release, depends on `base64` 0.22,
`sha2` 0.10 (so `block-buffer` 0.10, `cpufeatures` 0.2 and `crypto-common`
0.1), `hashlink` 0.11 (so `hashbrown` 0.16) and `syn` 2, while the workspace
uses the newer ones (`base64` 0.23, chacha20poly1305's RustCrypto 0.2/0.3
crates, `syn` 3).

**Solution.** `deny.toml` skips exactly those versions, each with the reason,
so a new duplicate of the same crates still warns. No license changed.

### The store needs no `rand`

**Issue.** The plan lists `rand` for crypto. chacha20poly1305 0.11 (aead
0.6) generates nonces and keys itself through its default `getrandom`
feature (`Nonce::try_generate()`), returning an error instead of panicking
if the OS generator fails.

**Solution.** `store` doesn't depend on `rand`. It enables chacha20poly1305's
`zeroize` feature, so the cipher wipes its key on drop, and decrypts into a
buffer that is wiped after the `SecretString` is built.

## T06: Slack mrkdwn

### Escaping applies inside code too

**Issue.** T06 said to escape `&`, `<` and `>` "outside code", while its
acceptance criterion says code must come out "untouched except for escaping".
Slack reads its control sequences (`<!here>`, `<@U…>`, `<url|label>`) before
it applies any formatting, so an unescaped `<!here>` inside a code block still
notifies the channel. qm-core leaves code verbatim and has that hole.

**Solution.** `to_mrkdwn` escapes the three characters everywhere, code
included, and changes nothing else inside code: no mention resolution, no
broadcast neutralization, no formatting. Slack shows `&lt;` as `<` inside code,
so the reader sees the original text. The T06 bullet in the plan now says
"everywhere".

### Literal Slack tokens are escaped, not passed through

**Issue.** qm-core passes literal wire tokens in model output through
unchanged (`<@U123>`, `<!subteam^S1>`, `<!date^…>`, `<https://x.io|label>`),
and only rewrites `<!here>`, `<!channel>` and `<!everyone>`. That lets model
output ping a whole user group, and it conflicts with escaping `<`.

**Solution.** The agent writes standard Markdown, and mentions go through the
`MentionDirectory`, as the design says. So every literal `<` is escaped, and
the tokens show as text. The broadcast forms still become qm-core's
`@\u{200B}here` text rather than escaped brackets, since the plan asks for
that. CommonMark parses `<https://x.io|label>` as an autolink whose URL
contains `|`; link URLs percent-encode `|` (and spaces), so it becomes
`<https://x.io%7Clabel>`. A link destination starting with `!`, `@` or `#` is
percent-encoded too, because `[x](<!here>)` would otherwise render as the
broadcast `<!here|x>`.

### Typed broadcasts get a zero-width space

**Issue.** qm-core leaves typed `@here`, `@channel` and `@everyone` alone. They
are inert only because qm-core posts without Slack's `link_names` flag.

**Solution.** `to_mrkdwn` inserts U+200B after the `@` (outside code, ignoring
case, not after a letter or digit, so `me@here.com` is untouched), which
matches what the wire forms become. These names are never offered to the
directory, so a member called "here" can't be pinged through them.

This does not make the output safe under every posting flag. With
`link_names=1` (or `parse=full`) Slack would still link an unresolved `@devs`
that stays text, and ping that user group, and code keeps a typed `@here`
as written. Rewriting every unresolved `@word` would mangle ordinary text, so
the renderer relies on the Slack surface never setting either flag. T29's
deliverables and acceptance in the plan now say so.

### Bare URLs get explicit bounds

**Issue.** T06 said bare URLs are "left alone". qm-core wraps them in `<…>`
because Slack's own URL detection pulled neighboring mrkdwn marks into the
link: `*https://x.io/#/device*` linked to `…/device*` (qm-core's
"device-code bug"). The Markdown parser strips the `**`, but the output puts
Slack's `*` right back next to the URL.

**Solution.** Bare `http://` and `https://` URLs in text become `<url>`, with
trailing punctuation and unmatched closing brackets left outside, as qm-core's
`trimUrlTail` does. Link labels and code are not scanned. `www.` addresses are
still left to Slack. The T06 bullet in the plan now says so.

### CommonMark disagrees with some qm-core regex cases

**Issue.** qm-core converts with regexes; this renderer walks the
`pulldown-cmark` tree, which follows CommonMark:

- `above\n---\nbelow` is a setext heading, not a rule.
- An unclosed fence runs to the end of the document instead of staying
  verbatim.
- A ```` ``` ```` run in the middle of a line is a code span, not a fence.
- Block spacing isn't in the event stream.

**Solution.** Follow the parse tree, and adapt the ported test cases, which
name each difference. Blocks are separated by a blank line when the source had
one between them (compared by source line numbers), and by a line break
otherwise, so `### Deep\nbody` still gives `*Deep*\nbody`. Fenced blocks keep
their info string, as qm-core and T07's fence reopening assume, even though
Slack doesn't highlight syntax. A code body that itself holds ```` ``` ````
still gets a backtick fence (see
[Backtick runs close a Slack code block](#backtick-runs-close-a-slack-code-block)).

### Unbounded nesting overflows the stack

**Issue.** `pulldown-cmark` emits a flat event stream, but any tree walk over
it recurses once per nesting level, and so does dropping the tree. A line of
100,000 `>` (or `*`) aborted the process with a stack overflow.

**Solution.** The tree builder keeps at most 64 levels (`MAX_DEPTH`). Elements
nested deeper are flattened into their ancestor at the limit: their text
stays, their markup is dropped. A test renders 100,000 levels on a test
thread's default stack.

### `@Name` grammar details

**Issue.** qm-core's `PLAIN_MENTION` regex relies on backtracking. When the
greedy one-to-three-word name runs into `/` or `@`, the regex retries shorter
matches, down to part of a word (`@ankit/x` tries `anki`). When nothing
resolves, it skips the whole matched run, so `@nobody https://x.io` never
wraps the URL.

**Solution.** `render::mention::scan` takes the greedy words, drops the last
word when it runs into `/` or `@` (a lone word then isn't a mention), and tries
the longest name first, as qm-core does, including its rule that a capitalized
next word means somebody else (`@Ankit Torres` stays text when only "Ankit" is
known). An unresolved name consumes only its first word, so the rest of the
line is still scanned. Names are passed to the directory as written;
`MentionDirectory` implementations own case folding. The scanner lives in
`render::mention` so T07's Rocket.Chat renderer can reuse it with its own
broadcast names.

### A link label can disguise its destination

**Issue.** `[https://good.com](https://evil.com)` became
`<https://evil.com|https://good.com>`: Slack shows `https://good.com` and
opens `evil.com`. Model output is untrusted (a prompt injection can write the
link), so a label must not be able to name a different site than the link.

**Solution.** Before writing `<url|label>`, the renderer takes the label as
plain text and looks at each word. A word with a `scheme://` or `mailto:`
prefix, or one shaped like a domain name (two or more dot-separated labels of
letters, digits and `-` ending in an alphabetic or `xn--` label, including
`user@host` addresses) or an IPv4 address, names a host. The authority ends at
`/`, `?`, `#` or `\`, as in browsers, and the host is what follows its last
`@`. Hosts are compared after dropping the port and a trailing dot, dropping
default-ignorable characters (zero-width spaces, soft hyphens, bidirectional
controls: they render as nothing and could hide a dot from the check), mapping
dot look-alikes (`。`, `．`, `｡`, `﹒`, `․`) to `.`, lowercasing, and dropping
a leading `www.`. When any named host differs from the URL's, the label is
written as text next to a bare link, `https://good.com (<https://evil.com>)`,
the same shape table cells already use. The label's text stays unarmed, as in
a link; if Slack links a URL in it on its own, that link shows its own
destination, so nothing needs neutralizing. Images get the same check on their
alt text. Autolinks have no separate label, and email autolinks pass because
the address and the `mailto:` URL name the same host.

The check errs toward showing the URL. There is no IDNA mapping, so a Unicode
label and its punycode URL (`bücher.de`, `https://xn--bcher-kva.de`) count as
different, and so do a domain and its subdomains. File names whose extension
is also a top-level domain (`main.rs`, `README.md`) look like domains, so
`[main.rs](https://github.com/…)` becomes `main.rs (<https://github.com/…>)`.
A homoglyph URL (`https://аpple.com` with a Cyrillic `а`) is still shown as
written; the check only stops a label from vouching for it.

### Slack doesn't format inside a word

**Issue.** CommonMark lets `*` emphasis start or end inside a word, so `5*3*2`
and `a*b*c` parse as emphasis. Rendering it as `_3_` gave `5_3_2`, which Slack
doesn't format either, since it only formats at word boundaries: the reader saw
underscores where the agent wrote asterisks. qm-core's regex leaves both
alone.

**Solution.** Emphasis, strong emphasis and strikethrough directly preceded or
followed by a letter or digit in the source keep their Markdown delimiter
character, once on each side, and their contents render without that style:
`5*3*2` → `5*3*2`, and `foo**bar**baz` → `foo*bar*baz`, which is also what
qm-core produces. Doubling the delimiter back to `**` would leave an inner
`*bar*` pair that Slack could format. `_` can't open or close emphasis inside
a word in CommonMark, so `snake_case_name` was already text.

### Backslash escapes have no Slack equivalent

**Issue.** `\*not bold\*` parses as the literal text `*not bold*`, and
Slack then bolds it; the same goes for `_`, `~` and `` ` ``, and for character
references such as `&ast;`. Slack's mrkdwn has no escape character.

**Solution.** pulldown-cmark starts a new text event at each escaped
character, so the tree builder records the offset of any text event that
begins with one of `*`, `_`, `~` or `` ` `` and either follows a backslash or
differs from its source (a character reference). Outside code those
characters get U+200B on both sides, which keeps them from opening or closing
Slack formatting if Slack treats U+200B as a word character or as a space.
(If it treated it as punctuation, no invisible character could help.) A
character with a letter or digit on both sides is left alone, because
Slack wouldn't format there and the zero-width space could act as a boundary
that lets it. Image alt text is plain text, so every such character in it is
treated as escaped. Table cells render inside a code block and need nothing.
A literal `*` right after an escaped backslash (`\\* x`) counts as escaped
too, which only adds zero-width spaces. Slack's exact boundary rules aren't
documented, so T29's live check should confirm this renders as intended.

### Backtick runs close a Slack code block

**Issue.** A code body holding ```` ``` ```` used to get a `~~~` fence, as in
qm-core. Slack doesn't know tilde fences, so the whole block rendered as
mrkdwn and the inner ```` ``` ```` opened a real code block.

**Solution.** Code blocks and tables always use a backtick fence. Slack closes
a code block at any run of three backticks and has no escape, so a U+200B goes
before every third backtick in a row inside the block, and before a backtick
that starts the info string. The block shows the same characters, but copying
it out carries the zero-width spaces along.

### A link with an empty URL

**Issue.** `[x]()` rendered as `<|x>`, which Slack doesn't parse as a link.

**Solution.** A link or image whose URL is empty shows only its label (`x`),
and nothing when the label is empty too. Table cells show just the label as
well, instead of `x ()`.

### Wire broadcast labels are searched a bounded distance

**Issue.** Each `<!here|` looked for its closing `>` to the end of the text,
so a message of many `<!here|` without `>` took quadratic time: 210 KB took
0.2 s.

**Solution.** The `>` must come within 256 bytes (`MAX_WIRE_LABEL`) and before
a line break. A longer or unclosed token stays escaped text (`&lt;!here|…`),
which is just as harmless. A test renders 2.1 MB of unclosed `<!here|` under a
time limit that the quadratic version exceeds several times over.

### Code spans holding backticks

**Issue.** Slack ends inline code at the next backtick and has no escape, so
a code span that holds one (``` `` a`b `` ```) renders as `` `a`b` ``, and
Slack shows `a` as code followed by a stray `` b` ``. qm-core has the same
limit.

**Solution.** Left as is. A zero-width space doesn't stop a backtick from
closing inline code, and replacing the backtick with a look-alike would change
the code's text. Agents rarely put backticks in inline code; a fenced block
shows them correctly.

## T07: splitting and directives

### Split the rendered text, not the Markdown

**Issue.** T07 didn't say whether `split` runs on the agent's Markdown or on
a renderer's output. Rendering changes the length (`&` becomes `&amp;`, a link
becomes `<url|label>`, a table grows padding), so chunks cut from Markdown
can exceed the limit once rendered. And the renderers need whole constructs:
a table or a list cut in two renders differently.

**Solution.** The order is extract directives, render, then split, as T23's
delivery steps already list. `split` knows both renderers' output syntax:
it never cuts inside a Slack `<…>` token, an HTML entity such as `&amp;`, a
Markdown link or image, an `@mention`, a fence line, or a character, and
keeps grapheme clusters together (see [Grapheme clusters need a mark
table](#grapheme-clusters-need-a-mark-table)). A property test renders
generated Markdown with `to_mrkdwn` and checks that no cut lands inside a
token or an entity. The crate docs state the order.

### Chunks keep their whitespace, so they rejoin exactly

**Issue.** The acceptance property says that rejoining the chunks with the
fences removed gives the original text. Trimming the whitespace at a cut
would break that.

**Solution.** Chunks are consecutive slices of the text, plus the fence lines
the splitter adds. A cut falls after a blank line, a line break or a run of
spaces, and that whitespace stays at the end of the earlier chunk, so the next
chunk doesn't start with a blank line. The tests check the property on the
splitter's internal pieces, and check separately that each piece renders as
reopening line, slice and closing fence.

### Where the splitter cuts

**Issue.** qm-core's `safeCutIndex` backs off from the limit to avoid a `<…>`
token or an unbalanced `` ` ``, `*` or `~`, but it doesn't look for paragraph,
line or word breaks, and it counts ```` ``` ```` anywhere in the text as a
fence.

**Solution.** A break is used when it falls in the second half of the chunk:
a blank line outside code, or the start or end of a code block, first, then a
line break, then a space outside code. Spaces inside a code block don't
count, since a cut there breaks a code line in two. Otherwise the latest break
of any kind wins, and a word is cut only as a last resort. Code spans and
pairs of `*`, `_` or `~` on a line are kept whole only if that still leaves
at least a quarter of the chunk, like qm-core's 25% floor, because an `_` in
an identifier can pair with one much further along the line. The hard rules
above give way only when one construct is longer than a chunk. Fences are
found line by line: three or more backticks or tildes after optional spaces
and `>` markers, and a closing line needs the same number of `>`, so a
blockquoted fence in Slack output (`> ```sh`) is closed and reopened with its
prefix. A cut never lands just after an opening line or just before a closing
line, which would leave an empty code block.

### Limits smaller than a fence or a character

**Issue.** A continuation chunk repeats the opening fence line and adds a
closing one. With a tiny limit, or a very long info string, those lines alone
leave no room for content, and a limit of 1 UTF-16 unit can't hold an emoji at
all. qm-core stops repeating fences below a 32-character budget.

**Solution.** A fence is repeated only when twice its opening line plus four
units fits the limit, which guarantees that every chunk can hold at least one
character between them. Otherwise the block is split like plain text. A chunk
always holds at least one character, so a limit below one character's size is
exceeded rather than looping; the rustdoc says so. Real limits (3,000 and
5,000) never get near either case.

### Rocket.Chat mentions need a username, not an id

**Issue.** `MentionDirectory::resolve` was documented as returning a platform
user id, which is what Slack's `<@U…>` needs. Rocket.Chat mentions are written
`@username`; a user id there is not a mention.

**Solution.** The trait now returns "the handle the surface's mention syntax
needs": a user id on Slack, a username on Rocket.Chat. The Rocket.Chat
renderer only accepts a username made of ASCII letters and digits, `.`, `_`
and `-` (the server's name pattern) that isn't a broadcast name, so a
directory entry can't turn a mention into a broadcast. Name resolution
leaves code spans, code blocks, link destinations, autolinks and bare URLs
alone, found with the same `pulldown-cmark` parse (`render::verbatim`).
Broadcasts are neutralized everywhere, code and URLs included (see [Code
doesn't protect a broadcast on
Rocket.Chat](#code-doesnt-protect-a-broadcast-on-rocketchat)). The bare URL
scanner moved from `slack.rs` to `render::url` to be shared.

### Code doesn't protect a broadcast on Rocket.Chat

**Issue.** The renderer first neutralized `@all` and `@here` only outside
code and link targets, with the Slack renderer's word rules (Unicode letters
and digits, not after a letter). Rocket.Chat decides who a message notifies
from its raw text, not from rendered Markdown. `MentionsParser.getUserMentions`
(`app/mentions/lib/MentionsParser.ts`, the same in 7.10.0 and 8.0.0) removes
`[label](dest)` links with `/\[[^\]]*\]\([^)]+\)/g`, then matches
`(^|\s|>)@([0-9a-zA-Z-_.]+…)` with flags `gm`, and
`MentionsServer.getUsersByMentions` (`app/mentions/server/Mentions.ts`)
notifies the room when the name is `all` or `here`. The name class is
ASCII-only and code isn't special. So `@all` in a fenced, indented or inline
code span, `@allé` and `@here٣` (the name ends at the first non-ASCII
character), a link title spanning lines, inline HTML (`<a>@all</a>`),
`[x](y)@all` at a line start (the link removal leaves `@all` there), and a
directory entry resolving to `allé` all broadcast.

**Solution.** After rendering, `to_markdown` makes one last pass over the
whole output, code and link targets included, with the server's grammar: it
inserts U+200B after every `@` whose following run of `[0-9A-Za-z._-]` is
`all` or `here`, ignoring case and trailing `.`, `_` and `-`, whatever
precedes the `@`. The server reads no name after the zero-width space. A
URL or a code sample containing `/@all` or `@here` gets the zero-width space
too; that is the price of the server not knowing about code. `@allison` and
`@all.hands` stay untouched. The pass is the only place that inserts the
space; name resolution just skips broadcasts so they are never offered to
the directory. Usernames from the directory must match the server's ASCII
class. The tests port the server's regex (`rocketchat::server`, checked
against the JavaScript regex under Node on 30,000 generated strings while
writing it) and assert that no output, and no chunk `split` makes from it,
yields `all` or `here`. The rule assumes the default `UTF8_Names_Validation`
pattern; a server configured with a narrower name pattern could read `@all`
out of `@all.hands`.

### A cut can create or shorten a mention

**Issue.** A mention was kept whole only when its `@` followed a
non-alphanumeric character, and a cut could fall right before any `@`. The
server's grammar accepts an `@` at the start of the message, so a harmless
`x@name` could become a mention at the start of the next chunk, and a forced
cut inside an oversized construct could shorten `@herectic` to `@here`.

**Solution.** `split` keeps every `@` and the name after it together,
whatever precedes the `@`, and never cuts right before an `@` that follows
anything but whitespace or `>`. When a single construct is longer than the
chunk and a cut has to fall inside it, the cut still avoids the inside of a
name: it falls right after the `@` instead, so neither chunk holds a
shortened name. Together with the final pass above, every `@` run in a chunk
is a run of the rendered text, and those are already neutralized.

### Grapheme clusters need a mark table

**Issue.** The splitter's list of characters that attach to the one before
them covered combining diacritics, variation selectors, emoji modifiers,
the joiner and tags, but no script-specific marks. `"कि"` repeated and split
at 1,001 characters gave chunks starting with the vowel sign U+093F; Thai
vowels and tone marks, Hebrew and Arabic points, and Hangul vowel and
trailing jamo were cut the same way. The workspace has no Unicode property
crate.

**Solution.** `split::graphemes` holds a table of 336 ranges: every
character of general category `Mn`, `Mc` or `Me`, plus everything Unicode 17
gives `Grapheme_Cluster_Break` `Extend`, `SpacingMark`, `V`, `T` or `ZWJ`,
generated from the `unicode-segmentation` crate's tables and Python's
`unicodedata`. A cut also never falls after an Indic virama
(`Indic_Conjunct_Break=Linker`) before a letter, after a zero-width joiner,
or after a Hangul leading jamo before another leading jamo or a syllable.
Prepend characters and the full emoji ZWJ grammar are left out; the rules
only ever remove cut positions, so an approximation errs toward longer
clusters.

### Reference links and long entity names

**Issue.** Codex's review found two cuts the splitter allowed: inside a
reference-style link, `aaaa[label][ref]` with `[ref]: /url` below, split at
13, and inside an HTML entity with a name longer than ten characters, such
as `&CounterClockwiseContourIntegral;`.

**Solution.** When the text defines a label (a line starting `[label]:`
after optional spaces and `>` markers), `[text][label]`, `[label][]` and
`[label]` on one line are kept whole like inline links, with labels matched
case-insensitively and with whitespace collapsed, as CommonMark does. Only
labels without brackets and up to 999 characters count, which also keeps
the matching linear. Brackets without a definition are ordinary text. An
entity name may now have up to 31 characters, the length of the longest
HTML5 name.

### Directive details qm-core decides and T07 doesn't

**Issue.** T07 names only `[[react: <emoji>]]`. qm-core's `extractReactions`
also accepts several names per directive, names with colons, literal emoji
characters (through a generated table of about 1,800 entries), and a target
message after `@`, caps a reply at five reactions, strips an unclosed
`[[react:` running to the end of the text, and trims every line of the reply,
code included, whenever it removed something.

**Solution.** `directives::extract` accepts several names separated by spaces
or commas, strips colons, lowercases, keeps only valid short names
(`[a-z0-9_+'-]+` with an optional `::skin-tone-2` to `-6`, at most
`MAX_NAME_LEN`, 64, characters in all), drops duplicates and returns at most
`MAX_REACTIONS` (5). Literal emoji characters are dropped:
the table would be a large data file for a case the agent's instructions can
avoid. A directive with an `@` target is removed without effect, since
reacting to the current message instead would be wrong and short message ids
belong to T23's `message_refs`. An unclosed `[[react:` is removed only on the
last line, so prose that mentions the syntax can't delete the rest of a
reply. Whitespace is cleaned only next to removed directives: a line that
held only directives goes, space before a directive at the end of a line
goes, and at most one blank line is left where a line was removed. Code is
found with the parse tree, and a directive overlapping it is left as text.

## T08: commands parser

### Chat text isn't a shell command line

**Issue.** clap parses an argument vector, but commands arrive as chat
text. Shell-style quoting would make `persona helper You're terse` fail on
the apostrophe, and would make members quote every persona. Splitting at
white space and letting clap collect the rest into a `Vec` loses the line
breaks and spacing of a multi-line persona.

**Solution.** The text is split at Unicode white space, with no quoting or
escaping: quotes are ordinary characters. The command words are matched
against a table of specs in `help.rs`, ignoring case, because phones
capitalize the first word of a message. For the commands with a free-text
tail (`create`'s persona, `persona`'s text, `admin ban`'s reason), the table
says how many positional arguments come first, and everything after them is
cut from the original text verbatim, trimmed only at its ends, and handed to
clap as one argument. The same table is the only source of usage text, for
`help`, `Command::help` and parse errors.

### clap errors quote the input

**Issue.** clap's rendered errors repeat the offending argument ("unexpected
argument 'sk-ant-…' found"), so a malformed `admin api-key set` or
`login <code>` would put the secret in the reply and in any log of the
error. Its `InvalidArg` context holds the typed value for some error kinds and
the argument's name for others.

**Solution.** `ParseError` never uses clap's text. `clap_problem` rewords
each error kind itself, reads `InvalidArg` only for missing arguments (where
it holds the argument names), and uses the value parser's own message for
validation errors. Every value parser's message is a fixed sentence. Unknown
commands and help topics aren't echoed either. A test feeds malformed text
holding a marker to every command and checks that neither `Display` nor
`Debug` of the error contains it.

### Secret-bearing text that fails to parse

**Issue.** `Command::is_secret_bearing` only helps when parsing succeeds.
`!agent admin api-key set <key> oops` or `!agent admin api-key <key>` (with
`set` forgotten) fails to parse, yet the key is now public in the channel,
and T13 has to tell the admin to revoke it.

**Solution.** `ParseError::is_secret_bearing` is true when the text starts
with `login` or `slack-token` and has arguments, or with `admin api-key` and
has anything but a bare `set` or `clear` after it. Callers apply the same
channel rule to both. Review widened the rule to misspelt commands and known
token prefixes; see
[Misspelt secret-bearing commands](#misspelt-secret-bearing-commands-arent-commands-at-all).

### Secrets are `SecretString`, not `String`

**Issue.** The plan listed `Login { code: Option<String> }` and asked for a
`Debug` that redacts the secret-bearing variants. A hand-written `Debug`
has to be kept in step with every new field, and the plan's Libraries table
says every token and key in memory is a `secrecy` type.

**Solution.** The login code, both Slack configuration tokens and the API
key are `SecretString`, so the derived `Debug` redacts them by construction
and they are zeroized on drop. `Command` therefore has no `PartialEq`; tests
use patterns and `expose_secret`. The plan's T08 bullets are updated.

### `skill rm` needs the agent and the skill

**Issue.** The design's `/agent skill add|rm <name> <source>` and T25's
`/agent skill rm <name>` leave `rm` with one argument. Everywhere else
`<name>` is the agent, and skills are stored per agent, so an owner with two
agents can't say which one loses the skill; if `<name>` were the skill, the
agent would be missing instead.

**Solution.** `skill add <name> [source]` takes the agent, with the skill's
own name coming from its `SKILL.md` as T25 says, and `skill rm <name>
<skill>` takes the agent and the skill. The skill name is validated as
Claude Code's `[a-z0-9-]{1,64}`, which also keeps it safe as the directory
`<data>/skills/<agent>/<name>/`. The design's command table and T25 are
updated.

### Slack rewrites mentions, channels and links

**Issue.** Slack delivers slash command text (with "escape channels, users,
and links" on) and every message text with `@name` rewritten to
`<@U123|name>`, `#room` to `<#C123|name>` and URLs to `<url|label>`, while
Rocket.Chat delivers what was typed. It also encodes `&`, `<` and `>` as
entities in message text.

**Solution.** Member and channel arguments accept both forms, as
`UserRef`/`RoomRef` `Name` or `Id`; the label after `|` is dropped because
only the id is stable. `everyone`, `@everyone` and `<!everyone>` all mean
everyone; other `<!…>` broadcasts are refused. A `skill add` source written
as a Slack link token becomes its URL. Decoding entities stays with the
Slack surface (T30), before it calls `parse`: mention and link tokens parse
the same either way, and a persona then gets `<` rather than `&lt;`.

### clap treats a lone `--` as the end of options

**Issue.** With every argument allowing leading hyphens, `pause -x` parses,
but clap still swallows a lone `--` as its end-of-options marker, so
`allow -- helper everyone` parsed as `allow helper everyone`, `login --`
started a new login, and `persona helper --` looked like a persona upload.
clap has no setting to turn the marker off.

**Solution.** A lone `--` among the arguments is refused with "A lone `--`
isn't an argument." A `--` inside a longer free-text tail is kept as text.
The name `--` itself is valid under the plan's `[a-z0-9-]{2,32}` rule, but it
can't be created through chat, so no agent has it.

### Help is an error, and `limits` needs at least one setting

**Issue.** The plan says an unknown command returns the help text as the
error message, but has no `help` command, and gives `limits` as
`turns=N/day hops=N` without saying whether both are required.

**Solution.** Empty text, `help` and `help <command>` return a `ParseError`
of kind `Help` whose message is the help text, and an unknown command one of
kind `UnknownCommand` whose message is "Unknown command." and the help text.
Callers reply with any `ParseError`'s message, so no `Help` variant is
needed. `limits` takes one or both settings in either order, each at most
once; a missing one is `None`, meaning unchanged. `turns=N` is accepted
without `/day`. Numbers are plain digits (`turns` is `u32`, `hops` is `u8`,
the width of `core_types::Hop`).

### A skill source reaches `git clone`

**Issue.** `skill add <name> <source>` took any word as the source, and every
argument may start with `-` (see the lone `--` entry above), so
`skill add helper --upload-pack=<command>` parsed. T25 passes the source to
`git clone`, where such a word is an option that runs a command, and other
forms are just as unwelcome there: `ext::` and `file://` transports, local
paths, SSH URLs, and a ref such as `#--upload-pack=…` that `git` would read
as an option. A URL with `user:token@` would also put a credential in the
agent's configuration.

**Solution.** The source is checked by a value parser and must be an
`https://` URL: the host is letters, digits, `.` and `-`, with an optional
numeric port and no user info; the path is letters, digits and `-._~/%+`;
and an optional `#ref` starts with a letter or digit, continues with letters,
digits and `._/-`, and has no `..`, no `//` and no trailing `/` or `.`.
Any other form, or a source over 2048 bytes, is refused with one fixed
message that says what a source is and that leaving it out adds an attached
`SKILL.md` or `.zip`. The Slack link token is unwrapped before the check. Query strings
and non-ASCII paths are refused; percent-encoding covers the rare path that
needs them. T25's plan now also has agentd pass the URL after `--` and the
ref only inside an `--opt=value` word, as a second line of defense.

### Misspelt secret-bearing commands aren't commands at all

**Issue.** The rule in "Secret-bearing text that fails to parse" only looked
at text whose command words parsed as `login`, `slack-token` or
`admin api-key`. `api-key set sk-…` without `admin`, `slack_token …`,
`slacktoken …` and `admin apikey set …` came back as `UnknownCommand` or
`Invalid` with `is_secret_bearing()` false, so T13 wouldn't tell the member
that the secret they just posted in a channel is public.

**Solution.** `parse` computes the flag once for every error, from the words
alone. It is true when a word naming a secret (`login`, `api-key` or
`slack-token`, compared ignoring case, `-`, `_` and surrounding punctuation,
so `apikey`, `API_KEY`, `slack_token`, `SlackToken:` and `log-in` match) is
followed by anything but a bare `set` or `clear`, wherever it stands, or when
any word contains a known token prefix: `sk-ant-` (Anthropic API keys and
OAuth tokens), `xoxb-`, `xoxp-`, `xoxe.`, `xoxe-` and `xapp-` (Slack). This
replaces the earlier rule, which it covers. It errs towards caution, since a
missed warning leaves a live secret in a channel while a false one costs the
member a new login: `how do I login here` counts, and so does a help request
naming a token. The flag is computed only for errors; a command that parses
is judged by `Command::is_secret_bearing` alone, so a persona mentioning
`sk-ant-` is still just a persona. Error messages still never repeat the
text.

## T09: auth

### Claude Code 2.1.285's OAuth requests, read from the binary

**Issue.** The plan's `[claude_oauth]` defaults were read from the binary, but
the request shapes weren't recorded, and the live login T09 asks for needs a
browser and a Claude account, which the environment T09 was built in doesn't
have.

**Solution.** `crates/auth` follows the bundled JavaScript in
`/opt/claude-code/bin/claude` (2.1.285), found with `grep -a` on the functions
around `grant_type:"authorization_code"`:

- The defaults in the plan's table are all current: `CLAUDE_AI_AUTHORIZE_URL`,
  `TOKEN_URL`, `MANUAL_REDIRECT_URL`, `CLIENT_ID`, and the profile at
  `BASE_API_URL + /api/oauth/profile`.
- Authorize URL parameters, in order: `code=true`, `client_id`,
  `response_type=code`, `redirect_uri`, `scope`, `code_challenge`,
  `code_challenge_method=S256`, `state`. The verifier and the state are each
  32 random bytes, base64url, drawn separately.
- Code exchange: `POST token_url`, `Content-Type: application/json`, body
  `{grant_type:"authorization_code", code, redirect_uri, client_id,
  code_verifier, state}`, 30 s timeout. It sends the `state` it generated;
  with a pasted code that is the pasted state too, since agentd looks the
  login up by it.
- Refresh: `POST token_url`, JSON body `{grant_type:"refresh_token",
  refresh_token, client_id, scope}`, with `scope` the space-joined scopes. A
  response without `refresh_token` keeps the old one; `expires_in` is
  required. Claude Code refreshes when `now + 300 s >= expires_at`, the
  plan's 5 minutes. Because `scope` is sent, widening `scopes` after members
  have linked makes their refreshes fail (`invalid_scope`), and they have to
  log in again.
- Profile: `GET`, `Authorization: Bearer`, `Content-Type: application/json`,
  `Cache-Control: no-cache`, 10 s timeout. `organization.organization_type`
  maps `claude_max`, `claude_pro`, `claude_team`, `claude_enterprise`, and
  anything else to no subscription type; `organization.rate_limit_tier` is
  kept. A failed profile read doesn't fail the login or the refresh.
- Pasted input is split on `#` and needs both parts.
- Scopes: Claude Code's own claude.ai login asks for `org:create_api_key
  user:profile user:inference user:sessions:claude_code user:mcp_servers
  user:file_upload user:plugins`, and `claude setup-token` for
  `user:inference` alone, so the authorization server accepts a subset for
  this client. agentd keeps the plan's `user:profile user:inference`: the
  profile needs the first, the proxy the second. Whether the server accepts
  exactly this pair is part of the live check still to be done.

### Claude Code revokes the refresh token on logout

**Issue.** The plan said revoking at Anthropic isn't part of Claude Code's
flow, so `logout` would only delete the link. 2.1.285's logout (and its
failure paths) calls `POST ${TOKEN_URL}/revoke` with
`{token: refresh_token, token_type_hint: "refresh_token", client_id}`, JSON,
5 s timeout, best effort.

**Solution.** `OAuthConfig` has a `revoke_url` key, default
`https://platform.claude.com/v1/oauth/token/revoke`. `logout` deletes the link
first, then revokes; a failed revocation is logged and the link stays
deleted. The plan's table and T09 are updated.

### A refresh failure is not always a dead link

**Issue.** The plan says a refresh failure returns `RelinkRequired` and marks
the link broken. Taken literally, a network blip, a timeout or a 5xx from the
token endpoint would force every member who happened to refresh then to log
in again.

**Solution.** Only a response saying the refresh token is dead marks the
link broken; which responses say so is in
[A 4xx from the token endpoint is not always a dead token](#a-4xx-from-the-token-endpoint-is-not-always-a-dead-token).
Any other failure leaves the link as it is. If the current access token
hasn't expired yet (a refresh starts 5 minutes early) it is returned;
otherwise the error is. A broken link returns `RelinkRequired` at once,
without calling the endpoint again, until the member logs in. The member is
told once per failure through the relink notices described in
[A cancelled caller lost the refresh](#a-cancelled-caller-lost-the-refresh).

### `put_claude_link` would let a refresh undo a logout

**Issue.** `put_claude_link` is an upsert. A refresh that read the link, then
waited on the token endpoint while the member logged out, would store its
result and link the member again.

**Solution.** Two layers. Every write of a member's link in `auth` (refresh,
login, logout) holds that member's lock from the `KeyedLocks` the
single-flight refresh uses, so a logout waits for a refresh in flight and then
deletes its result. And a refresh stores through a new store method,
`update_claude_tokens`, a plain `UPDATE` that returns false when there is no
row (or, since the generation fix below, a newer login's row), so even a
delete that bypasses `auth` isn't undone; `auth` then revokes the orphaned
refresh token and returns `NotLinked`. A login still upserts, since it is
meant to create the link.

### Error values could leak what they describe

**Issue.** `reqwest::Error`'s `Display` includes the request URL, and
`serde_json` errors quote the offending value (`invalid type: string "…"`),
which in a token response could be a token. OAuth error bodies can carry an
`error_description` that echoes the request.

**Solution.** Transport errors keep the `reqwest::Error` without its URL.
Bodies are read as bytes and parsed with `serde_json::from_slice`, and a parse
failure becomes `InvalidResponse` with a fixed reason. From an error body only
the OAuth `error` code is kept, and only if it is at most 64 lowercase letters
and underscores. The client follows no redirects, because a 307 or 308 from
the token endpoint would resend a body holding a code or refresh token to the
new location.

### Login choices the plan left open

**Issue.** The plan doesn't say what happens when a member starts several
logins, pastes someone else's code, or when the profile can't be read.

**Solution.**

- A member has at most one pending login: `start_login` drops the earlier
  ones, so only the newest link works, and the table can't be filled by
  repeated `login` commands. See
  [Two logins started at once both stayed pending](#two-logins-started-at-once-both-stayed-pending)
  for why that is one store transaction.
- `take_pending_login` deletes the row before the member check, so a code
  pasted by the wrong member is used up. The owner's code has leaked, so they
  have to start again, and the error (`UnknownLogin`) doesn't reveal that the
  state exists.
- If the profile can't be read after the exchange, the link is stored without
  a plan and `Linked { plan: None }` is returned; the plan is read again at
  the next refresh. After a refresh, a failed profile read keeps the old plan.
- The paste parser drops all whitespace (chat clients wrap long lines), strips
  backticks, quotes and angle brackets around the text, accepts a pasted
  callback URL (including Slack's `<url|label>` form), and rejects input over
  4 KiB.

### A cancelled caller lost the refresh

**Issue.** `access_token` refreshed inside the caller's future. If the caller
was dropped after the token endpoint answered but before the store was
updated, the rotated refresh token was lost, and with it the member's link
(Anthropic rotates refresh tokens, so the stored one may no longer work). A
test with a 100 ms timeout around a 300 ms token endpoint showed it. The same
drop could lose `newly_broken` after `mark_claude_link_broken` had set
`broken_at`, so no relink notice would ever be sent. T18's proxy drops
request futures whenever a client disconnects, so this would happen in
normal use.

**Solution.** A refresh runs in a task of its own, spawned by the first
caller that needs it. The task takes the member's lock, refreshes, stores
the tokens or marks the link broken, and publishes its result on a
`tokio::sync::watch` channel; every caller, the first included, only waits
on that channel, so dropping a caller cancels nothing. A guard removes the
member's entry from the in-flight map however the task ends, and a caller
whose task died without a result gets `AuthError::RefreshInterrupted`.

The relink notice no longer depends on a caller either. `RelinkRequired`
lost its `newly_broken` field; instead, when `mark_claude_link_broken`
returns true, the task sends the member on an unbounded channel whose
receiver agentd takes once with `Auth::take_relink_notices()` and turns into
the DM (T13). Notices queue until received. Tests cancel the caller mid-refresh
and check that the store ends up with the rotated tokens, and that the
broken mark and the notice both arrive.

### A failed refresh was retried by every waiter

**Issue.** Refreshes were single-flight only when they succeeded: a waiter
that got the lock after a failed refresh found the token still stale and
refreshed again, so ten callers during a 503 sent ten requests one after
another (about 2 s), and with a token endpoint that hangs up to the 30 s
timeout, up to 300 s.

**Solution.** Waiters share the refresh task's result, failure included, so
one refresh sends one request whatever it returns. After a failure that
doesn't break the link, the member's failure time is kept in memory (it is
disposable: a restart only means one extra attempt), and for
`REFRESH_BACKOFF` (30 s) a still-valid token is handed out without trying
again. An expired token is always retried, one request at a time, since there
is nothing to hand out instead. `AuthError` became `Clone` for this (its
`reqwest::Error` and `StoreError` are behind `Arc`). A test sends ten
concurrent calls during a delayed 503 and sees one request, and one more
call right after still sends none.

### A 4xx from the token endpoint is not always a dead token

**Issue.** Any HTTP 400, 401 or 403 from the token endpoint broke the link,
including a 403 HTML page such as a Cloudflare challenge, which says nothing
about the refresh token.

**Solution.** Claude Code 2.1.285's rule, read from the binary
(`grep -aoE 'function Vce\([a-z]+\)\{.{0,400}'` and the functions next to
it, `a5n`, `fLo`, `bBt`, `tl` and the caller `_5o`): it reads the OAuth
error code as `error` when that is a string and `error.type` when it is an
object, and treats a refresh token as dead (`known_dead_refresh_token`) only
for HTTP 400 or 401 with `invalid_grant`. It treats an account as on hold
for HTTP 400, 401 or 403 whose body is `{error: "invalid_grant" |
"access_denied", error_description: "account_on_hold"}`. `invalid_client`,
`invalid_scope` and `unauthorized_client` on a 400 are "expected" failures it
logs without reporting. Everything else is a plain `refresh_failed` it tries
again later.

`auth` marks the link broken for HTTP 400 or 401 with `invalid_grant`,
`invalid_client`, `invalid_scope` or `unauthorized_client`, and for an
account on hold on 400, 401 or 403. Any other 4xx, with or without a body,
is transient. Two deviations: Claude Code doesn't mark a token dead for the
three "expected" codes, but none of them can pass on retry (a wrong client
ID, or `scopes` widened after members linked), so `auth` asks the member to
log in again rather than retrying forever; and the plan's wording ("HTTP
400, 401 or 403") is narrowed to these codes. Tests cover each code, the
account-on-hold body, and 4xx responses that must not break the link.

### The plan was read while holding the member's lock

**Issue.** After a refresh, the profile request (10 s timeout) ran while the
member's lock was held, so a logout, a login, or the next refresh waited on
it, and every caller of `access_token` waited for it too.

**Solution.** The refresh task stores the tokens with
`update_claude_tokens`, hands the token to its callers and releases the
lock, then reads the profile and stores only the plan with
`update_claude_plan`, an `UPDATE` of the plan columns for the link's
generation. A failed read keeps the old plan. Tests show the caller and a
logout both finish while the profile is still answering, and that the late
plan write doesn't bring back a link deleted meanwhile.

### A stale refresh could break a newer login

**Issue.** A refresh that read the link, then waited on the token endpoint
while the member logged in again (from another instance, or through the
store directly), could mark the new login's link broken when its old
refresh token was refused, so T13 would send a wrong relink notice. A
successful one could overwrite the new login's tokens with the old grant's.

**Solution.** A new migration adds `claude_links.generation` and a one-row
`claude_link_generations` counter. `put_claude_link` takes the next value
from the counter and stores it with the link in one `BEGIN IMMEDIATE`
transaction and returns it. The counter never goes back, so a link deleted
by a logout and stored again gets a new generation too (a per-row counter
would restart). `update_claude_tokens`, `update_claude_plan` and
`mark_claude_link_broken` take the generation the refresh read and change
nothing unless it still matches. When one of them finds the link replaced,
`auth` revokes any refresh token it no longer needs and hands out whatever
the store now holds. Store and `auth` tests cover a mark after a new login
(nothing changes) and a new login after a mark (the new link is not broken),
and a refresh finishing after a new login.

### Two logins started at once both stayed pending

**Issue.** `start_login` deleted the member's pending logins and then
inserted the new one in two statements, so two concurrent starts could
interleave and leave both.

**Solution.** `put_pending_login` itself deletes the member's other pending
logins and inserts the new one in one `BEGIN IMMEDIATE` transaction, so
there is no separate step to forget. A store test with six concurrent puts
over ten rounds on a file database, and an `auth` test with six concurrent
`start_login` calls, each leave exactly one.

### An expired token could be handed out after a failed refresh

**Issue.** After a failed refresh, whether the current token was still valid
was checked against the time read before the request, which can take up to
30 s, so a token that expired during the request was still handed out.

**Solution.** The clock is read again after the attempt. A test lets a token
with 2 s left wait 2.5 s for a 503 and gets the error, not the token.

### `me` needs the link state without the tokens

**Issue.** T13's `me` only needs whether the member is linked, the plan and
whether the link is broken, and reading the link decrypted both tokens for
that.

**Solution.** `Auth::status(member) -> LinkStatus { linked, plan, broken }`
over a new store method, `claude_link_status`, that selects only `plan`,
`rate_limit_tier` and `broken_at`. A test reads the status of a link whose
tokens no longer decrypt.

### Store methods read the clock themselves

**Issue.** `ensure_member`, `mark_event_processed` and `put_claude_link`
(and the old `update_claude_link`) called `OffsetDateTime::now_utc()`
inside, unlike `sweep_expired` and `mark_claude_link_broken`, which take the
time. Their tests couldn't pin the stored timestamps.

**Solution.** They take a `now: OffsetDateTime`, as do the new
`update_claude_tokens`; callers pass the current time, and the store tests
pass fixed times and check them.

### sqlx's sha2 0.10 next to auth's sha2 0.11

**Issue.** `auth` computes the S256 challenge with `sha2` 0.11, the workspace
version. sqlx-core 0.9.0 still depends on `sha2` 0.10 and so `digest` 0.10,
and cargo-deny warned about both duplicates.

**Solution.** `deny.toml` skips exactly `sha2@0.10.9` and `digest@0.10.7`,
next to the other sqlx entries.

## T10: agentd skeleton

### axum's connections outlive an aborted `axum::serve`

**Issue.** The plan asks shutdown to stop accepting, then drain for a
configurable timeout. `axum::serve(…).with_graceful_shutdown(…)` does the
first part, but it runs each connection in its own `tokio::spawn`ed task.
Dropping or aborting the serve future when the timeout elapses leaves those
tasks running: a test with a handler that never returns got no connection
close, and the handler kept running after the store was closed. Only the
runtime shutting down at the end of `main` would stop them, which doesn't
hold for an in-process `Server`.

**Solution.** `server::serve_listener` is agentd's own accept loop over
axum's `Listener` trait (so axum's accept-error handling is kept), serving
the `Router` with `hyper-util`'s `auto::Builder` and upgrades, as
`axum::serve` does. Each connection is watched by a `GracefulShutdown` and
spawned into a `JoinSet` that the listener's task owns. On shutdown the loop
stops accepting, drops the listener and waits for the graceful shutdown;
when the drain timeout aborts the listener's task, its `JoinSet` is dropped
and every connection with it. Each request carries `ConnectInfo<SocketAddr>`,
which the proxy and ctl listeners need. The HTTP/1 side gets a `TokioTimer`,
which turns on hyper's 30-second header read timeout.

### Redaction can't wrap the stock `tracing-subscriber` formatters

**Issue.** The backstop has to replace a field's value in every line, but a
`Layer` can't change an event for the layers after it, and
`tracing_subscriber::field::RecordFields` is sealed, so the fields can't be
wrapped before the stock `DefaultFields` sees them. The stock JSON formatter
also records event fields with its own visitor, not through `FormatFields`.

**Solution.** `telemetry` has its own field formatters: `HumanFields` (used
by the stock human event format, which also formats span fields through it)
and `JsonFields` plus a `JsonEvents` event formatter for JSON lines. Both
visitors check the field's whole name against `REDACTED_FIELDS`. The human
formatter escapes control characters in values and messages, so a logged
value can't forge a line or send terminal sequences. Tests capture both formats
and check every listed name in events and spans, and that `scope_key` and
`token_count` still appear.

### Configuration errors name the key with `serde_path_to_error`

**Issue.** `toml`'s errors carry a byte span but not the key path, and serde
reports a missing field at its parent table.

**Solution.** The file is deserialized through `serde_path_to_error`, a new
dependency (MIT or Apache-2.0; axum's `json`, `form` and `query` features
already pull it in). A missing field's name, which serde's message quotes, is
appended to its parent's path, so the key reads `server.listen`. The line
from `toml`'s span is added to the message. Checks serde can't do (an
unspecified listen address, a public address inside the sandbox subnet, an
internal one outside it, the drain timeout bound, the log filter, a
non-SQLite URL, the environment variables) produce the same `key: message`
form. Values are never repeated,
except the offending value of a known, non-secret key in a type error.

### The public listener's subnet guard needed a key

**Issue.** The plan's network section says the public listener also refuses
connections from the sandbox subnet, but no key said what that subnet is, and
no task was named for it.

**Solution.** T10 builds the public listener, so it adds
`internal.sandbox_subnet` (CIDR, required) and `net::RefuseSubnet`, which
closes such connections as soon as they are accepted, before any byte is
read. Validation refuses a `server.listen` inside the subnet. A Linux-only
test binds the public listener to `127.0.0.1`, sets the subnet to
`127.0.0.2/32`, and checks that a client bound to `127.0.0.2` gets no answer
while one bound to `127.0.0.3` gets 200.

### `migrate` needs the master key

**Issue.** `agentd migrate` only needs the database, but `Store::open` takes
a `Sealer`, so the key has to be present.

**Solution.** `migrate` loads the full configuration, key included, like
`serve`. That also means a migration job checks the configuration it will be
served with. The store gained `ping` (for `/healthz`) and `close` (so
shutdown closes the pool after the drain instead of leaving it to drop).

### Address checks compare canonical forms

**Issue.** `[::ffff:0.0.0.0]:8443` passed the "never `0.0.0.0`" check, since
`Ipv6Addr::is_unspecified` is true only for `::`, yet on a dual-stack
socket it binds every IPv4 interface. Likewise `127.0.0.1:8080` and
`[::ffff:127.0.0.1]:8080` counted as different listeners. And a
`sandbox_subnet` written in mapped form, `::ffff:172.30.0.0/120`, never
matched a peer, because `Cidr::contains` turns a mapped peer into its IPv4
address and compared it against an IPv6 network, so the public listener's
guard and the listener checks were silently off.

**Solution.** Every address check in `Config` validation uses the canonical
form (`IpAddr::to_canonical`), so a mapped unspecified address is refused
and mapped duplicates are caught. `Cidr::new` stores a mapped network with
a prefix of at least 96 as the IPv4 subnet it names (prefix minus 96), so
`::ffff:172.30.0.0/120` is `172.30.0.0/24`, and `contains` compares an IPv4
peer against an IPv6 network by its mapped form, so `::/0` holds IPv4 peers
too.

### The internal listeners weren't tied to the sandbox network

**Issue.** Only `server.listen` was checked against `sandbox_subnet`. A
`proxy_listen` or `ctl_listen` on a public address was accepted, which
would have exposed the credential proxy and the agentctl API off the
sandbox network.

**Solution.** Validation requires both inside `internal.sandbox_subnet`.
Tests need two local addresses on different sides of the subnet, so the
test configurations put the public listener on `127.0.0.1` and the
internal ones on `127.0.0.2` with the subnet `127.0.0.2/32`. The reverse
(public on `127.0.0.2`, subnet `127.0.0.1/32`) doesn't work: Linux gives a
connection to `127.0.0.2` the source address `127.0.0.1`, so the public
listener would refuse the tests' own clients. Linux accepts every
`127.0.0.0/8` address without setup; CI runs only on Linux.

### Kubernetes sets `AGENTD_*` variables of its own

**Issue.** Any unknown `AGENTD_*` variable was fatal. Kubernetes injects
service-link variables for every Service in the namespace, so a Service
named `agentd` produces `AGENTD_PORT=tcp://…`, `AGENTD_SERVICE_HOST`,
`AGENTD_PORT_8443_TCP_ADDR` and more, and agentd would refuse to start in
the very Deployment that exposes it.

**Solution.** Unknown variables are sorted in three:

- Service links are skipped silently: names ending in `_PORT`,
  `_SERVICE_HOST` or `_SERVICE_PORT`, holding `_SERVICE_PORT_`, or ending
  in `_PORT_<number>_<TCP|UDP|SCTP>` with an optional `_PROTO` or `_ADDR`.
- Near misses of a secret's name are still errors, so a typo in a secret
  fails at startup: within two edits (Levenshtein, over bytes) of
  `AGENTD_MASTER_KEY` or `AGENTD_RC_MANAGER_TOKEN`, or starting within two
  edits of `AGENTD_SLACK_MANAGER_` without being a valid Slack manager name.
- Anything else is listed in `Config::unknown_env`, and `serve` and
  `migrate` log each name (never the value) as a warning once logging is
  set up. `Config` is loaded before the subscriber exists, so it can't log
  them itself.

The module docs, the example configuration and the README state the rule.

### A refused sandbox connection logged a warning each time

**Issue.** `RefuseSubnet` logged a `warn!` for every connection it
refused, so a sandbox retrying in a loop could flood the log.

**Solution.** A small `RefusalLog` keyed by peer IP warns at most once per
peer every `REFUSAL_WARN_INTERVAL` (a minute); the refusals in between are
logged at debug level and counted in the next warning's
`refused_since_last_warning`. Entries older than the interval are dropped
whenever a warning is logged, so the map holds only recently active peers.
A unit test drives it with explicit instants.

### A second signal during the drain was swallowed

**Issue.** `shutdown_signal` completed on the first SIGTERM or SIGINT and
then dropped its handlers' output, so a second signal during a long drain
(up to an hour) did nothing, and an operator's second Ctrl-C was ignored.

**Solution.** `cli::ShutdownSignals` counts the signals in a task that owns
the handlers; `first()` and `second()` are futures over the count.
`Server::run` (and `cli::serve`) take a second future, `abort`, that cuts
the drain short the way the drain timeout does: in-flight work is dropped,
then the store is closed, so the process exits promptly and cleanly. An
in-process test forces shutdown with a hanging request and an hour-long
drain timeout, and a unit test sends the test process a real SIGINT and
SIGTERM and checks that each future completes on its own signal.

## T11: Rocket.Chat REST

Server behavior below was read from the Rocket.Chat source on `develop`
(commit `fad30ab`, 8.8.x), where the REST API lives in
`apps/meteor/server/api/`, and checked against the 7.0.0 and 7.10.0 tags,
where it lives in `apps/meteor/app/api/server/`. No server was available to
run against, so none of it is verified live yet.

### `rooms.upload` is gone in Rocket.Chat 8.0

**Issue.** The plan names `rooms.upload/{rid}`. Rocket.Chat 8.0 removed it
(RocketChat/Rocket.Chat#36857 in `apps/meteor/CHANGELOG.md`). Its
replacement, `rooms.media/{rid}` followed by
`rooms.mediaConfirm/{rid}/{fileId}`, exists from 7.0 on. On 7.x,
`mediaConfirm` passes its whole body, less `description`, as `msgData` to
`sendFileMessage`, whose `check` rejects unknown keys, so `fileName` (which
8.x accepts and strips) breaks a 7.x upload.

**Solution.** `RestClient::upload` uses the two-step endpoints on every
version: a multipart `file` part carrying the name and a MIME type guessed
from the extension, then a confirm whose body is `{}` or `{"tmid": …}`.
`FakeRest` refuses confirm keys 7.x refuses. The T11 bullet in the plan now
names these endpoints.

### `users.createToken` needs a server secret, and its token expires

**Issue.** The plan says recent servers refuse `users.createToken` unless
started with `CREATE_TOKENS_FOR_USERS=true`. That was 6.x and 7.x, where the
endpoint was deprecated. Since 8.0 it takes a `secret` that must equal the
server's `CREATE_TOKENS_FOR_USERS_SECRET` environment variable, and the
caller needs `user-generate-access-token` for another user, which only
`admin` has by default (`apps/meteor/server/meteor-methods/auth/createToken.ts`,
`apps/meteor/server/lib/authorization/constant/permissions.ts`). It also
returns a login (resume) token, which expires after `Accounts_LoginExpiration`
(90 days by default) and counts against the user's login-token limit.

**Solution.** Use the other route, which works for a manager with only a
custom role: `RestClient::issue_bot_token` logs in as the bot with its random
password (`POST login`), calls `users.generatePersonalAccessToken`, then
`POST logout`s the login session. A personal access token doesn't expire. The
manager needs no permission for this step. The password is generated in
`create_bot_user`, held in a `BotPassword` that can't be cloned or
serialized, and consumed by `issue_bot_token`.

### The token route needs care around two-factor authentication

**Issue.** Three server behaviors get in the way of the login route:

- `users.generatePersonalAccessToken` is `twoFactorRequired`
  (`apps/meteor/server/api/v1/users.ts`). `checkCodeForUser`
  (`apps/meteor/server/lib/2fa/code/index.ts`) lets it through without a
  code only within `Accounts_TwoFactorAuthentication_RememberFor` (1800 s)
  of the user's creation, and only while the user has no 2FA method.
  Otherwise, with `Accounts_TwoFactorAuthentication_Enforce_Password_Fallback`
  on (the default), it wants `x-2fa-method: password` and
  `x-2fa-code: <sha256 hex of the password>`.
- With 2FA, email 2FA and `Accounts_TwoFactorAuthentication_By_Email_Auto_Opt_In`
  on (all defaults), every new user gets email 2FA enabled
  (`apps/meteor/server/lib/auth/startup.js`). It takes effect once the user
  has a verified email (`EmailCheck.isEnabled`). Then the password login
  itself asks for an emailed code (`apps/meteor/server/lib/2fa/loginHandler.ts`
  passes `disablePasswordFallback`), and the token endpoint picks email over
  the password fallback.
- `users.create` requires `email` (`packages/rest-typings/src/v1/users/UserCreateParamsPOST.ts`).

**Solution.** The bot is created with `verified: false` (the plan listed
`verified` without a value), so email 2FA never applies to it, and
`issue_bot_token` always sends the password-fallback headers, which the server
ignores when it doesn't need them. The token is created with
`bypassTwoFactor: true`: once the password is discarded the bot has no second
factor at all, so without it every 2FA-gated endpoint would fail for the bot
after the 30-minute grace period, and anyone holding the token can already act
as the bot. The caller supplies the email; T14 has to pick an address
(`<name>@<something>.invalid` passes the default checks). One case is left
for the live check: with `Accounts_EmailVerification` on (off by default),
password login refuses unverified emails (`validateLoginAttempt` in
`startup.js`), so such a server needs email 2FA auto opt-in off and
`verified: true`.

### What the server source says about the manager's custom role

**Issue.** The design leaves the custom role open. It stays open until the
live check, but the source narrows it down:

| Operation | Permission checked | Where |
| --- | --- | --- |
| `users.create` | `create-user`; `assign-admin-role` only if `roles` contains `admin`. No `assign-roles`, no edit-user permission (RocketChat/Rocket.Chat#7351 no longer applies). | `apps/meteor/server/lib/users/saveUser/validateUserData.ts` |
| `users.create` with `active` | also `edit-other-user-active-status` (so `active` is never sent) | `executeSetUserActiveStatus`, called from `users.create` in `apps/meteor/server/api/v1/users.ts` |
| bot token (login route) | none for the manager; the bot needs `create-personal-access-tokens`, which defaults to `admin` and `user` only, so the admin must add it to the `bot` role | `apps/meteor/imports/personal-access-tokens/server/api/methods/generateToken.ts`, `permissions.ts` |
| `users.setActiveStatus` | `edit-other-user-active-status` or `manage-moderation-actions` | `users.ts` |
| `users.update` of another user | `edit-other-user-info`; the endpoint is also `twoFactorRequired`, so the manager's personal access token must be created with "Ignore Two Factor Authentication" | `validateUserData.ts`, `users.ts` |
| `users.setAvatar` of another user | `edit-other-user-avatar`; a bot may set its own while `Accounts_AllowUserAvatarChange` is on (default) | `users.ts` |
| `channels.invite`, `groups.invite` | `add-user-to-joined-room` in a room the manager is in, else `add-user-to-any-c-room` or `add-user-to-any-p-room` | `apps/meteor/server/meteor-methods/rooms/addUsersToRoom.ts` |

**Solution.** Recorded here for T14's live check and README. The minimal
role for create is `create-user`, plus the one-time admin change that grants
`create-personal-access-tokens` to `bot`. Delete (T14) adds
`edit-other-user-active-status`. The manager is subject to the REST rate
limiter (10 calls per route per minute per IP by default; `bot` bypasses it
through `api-bypass-rate-limit`), so the role should include
`api-bypass-rate-limit` once the manager bot posts DMs (T13). The design's
open question is left as it is until a server confirms this.

### `x-ratelimit-reset` is an absolute time in milliseconds

**Issue.** The header isn't a delay: `enforceRateLimit`
(`apps/meteor/server/api/ApiClass.ts`) sets it to `Date.now() + timeToReset`.
The body is `{"success": false, "error": "… [error-too-many-requests]"}`
without `errorType`. Meteor's DDP rate limiter, which guards `login`, instead
surfaces through the login route as HTTP 401 with `error: "too-many-requests"`
and no header.

**Solution.** The wait is the header minus the response's `Date` (at first
the local clock; see
[Clock skew defeated the 429 retry](#clock-skew-defeated-the-429-retry)),
floored at zero, and one second when the header is missing or unreadable. A 429, or either
code in any status, is retried once when the wait is at most
`with_max_retry_wait` (60 s by default, the server's default window);
otherwise, or on a second limit, the call fails with
`SurfaceError::RateLimited`. The 429 is raised before the endpoint runs, so
retrying a POST can't apply it twice.

### Error bodies put the code in different places

**Issue.** Meteor errors arrive as `error: "<reason> [<code>]"` plus
`errorType: "<code>"`. `API.v1.failure("<code>")` sends the code as `error`
with no `errorType`, `rooms.info` swaps them (`error: "not-allowed"`,
`errorType: "Not Allowed"`), and the permissions middleware sends 403 with
only `error: "… [error-unauthorized]"`. `error-unauthorized` thrown by a
handler becomes 403 today but 401 from 9.0 (`applyBreakingChanges` in
`ApiClass.ts`), the status that otherwise means a rejected token.

**Solution.** `map_error` collects candidate codes from `errorType`, a
space-free `error`, and a trailing `[code]`, and matches them against
permission codes (`Forbidden`) and missing-object codes (`NotFound`) before
looking at the status, so only a 401 without such a code means
`Unauthorized`. On a 401 a bare `error` isn't read as a code, since
`API.v1.unauthorized()` sends `error: "unauthorized"` for a rejected login.
Everything else is
`Api` with the server's description, cut to 200 characters. A body with
`success: false` is an error even on 200. A success body of the wrong shape
reports serde's error category and column, never the value, so message text
can't leak into the error.

### Smaller server behaviors the client works around

- `chat.react` toggles when `shouldReact` is absent
  (`apps/meteor/server/lib/messaging/reactions/setReaction.ts`), so a second
  `:eyes:` would remove the first. The client always sends `shouldReact: true`.
- `groups.history` includes thread replies unless `showThreadMessages=false`;
  `channels.history` and `im.history` exclude them unless it is `true`. The
  client always sends `false`, and `inclusive=false`.
- `latest` is parsed with JavaScript's `new Date`, and messages carry
  millisecond timestamps, so the client formats it like `toISOString`.
- `chat.postMessage` with a `roomId` joins a public channel the poster isn't
  in (`getRoomByNameOrIdWithOptionToJoin` with `joinChannel: true` in
  `apps/meteor/server/lib/messages/processWebhookMessage.ts`). T12 and T23
  should expect a bot to join a channel it posts to.
- `chat.getThreadMessages` has no `latest`. The client asks for newest first
  (`sort={"ts":-1}`) and pages by `offset`; `aroundId` exists only on newer
  servers. To turn a `Cursor` (a message id) into a `latest` time for the
  top level, the client also has `chat.getMessage`, which the plan didn't
  list.

### Clock skew defeated the 429 retry

**Issue.** `x-ratelimit-reset` is the server's `Date.now()` plus the time to
reset, so subtracting the local clock folds in any skew between the two
hosts. With the local clock more than a minute behind the server's, the wait
exceeded `with_max_retry_wait` and a call that would have succeeded a
second later failed with `RateLimited`; with it ahead, the client retried at
once and hit the limit again.

**Solution.** The wait is the reset minus the response's own `Date` header,
which the server (Node's `http` sets it on every response) or a proxy in
front of it writes from a clock that is at worst next to the server's. It is
parsed as RFC 7231's IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) with the
`time` crate; the obsolete RFC 850 and asctime forms, which no current server
sends, count as unreadable. A missing or unreadable `Date` falls back to the
local clock, and the bounded maximum still applies. `Date` has whole seconds,
so the wait can come out up to a second longer than the server's, never
shorter; a reset near the end of a full 60-second window can therefore exceed
the default maximum by that second and fail with `RateLimited` instead of
waiting. `FakeRest::rate_limit_at` sends a 429 from a skewed server clock,
with a `Date` in whole seconds and a reset measured from it.

### Uploads are capped and read once

**Issue.** `upload` read the whole file into memory with no limit, and read
it again for the retry after a 429. The file comes from an agent's session,
so its size is whatever the agent wrote, and a path to a device such as
`/dev/zero` would read until memory ran out.

**Solution.** `RestClient::with_max_upload_size` sets a limit, 100 MiB by
default, which is Rocket.Chat's default `FileUpload_MaxFileSize`
(`apps/meteor/server/settings/file-upload.ts`); a server with a lower limit
still refuses with its own error. Before reading, the file's metadata must
show a regular file within the limit, else the call fails with
`SurfaceError::Api` naming the size and the limit, and nothing is sent. The
read itself stops after limit + 1 bytes, so a file that grows after the check
can't take more memory than that and is refused too. The bytes are read once
into `Bytes`, and each attempt's multipart part is a cheap clone of them.
Streaming the file instead would need reqwest's `stream` feature and
`tokio-util`, would reopen and reread the file for the retry (which could
then send different content), and would send a malformed body if the file
changed size after its length was declared; with the cap, reading into memory
is bounded and simpler.

## T12: Rocket.Chat realtime

DDP and stream behavior below was read from the Rocket.Chat source on
`develop` (commit `fad30ab`): `ee/apps/ddp-streamer/src/ddp/`,
`packages/streamer/src/` and `apps/meteor/server/lib/notifyListener.ts`. No
server was available, so none of it is verified live yet.

### The store reaches the surface through a `Dedup` trait

**Issue.** T12 deduplicates with `store.mark_event_processed("rocketchat",
_id)`, but surface crates don't depend on the store, and agentd is what holds
it.

**Solution.** surface-rocketchat defines `Dedup`, one async method with the
store's signature and contract, and `RocketChatSurface::new` takes an
`Arc<dyn Dedup>`. agentd implements it with a one-line call to the store;
`DEDUP_SOURCE` is `"rocketchat"`. The tests use a real in-memory `Store`
(a dev-dependency) behind it. A copy is recorded only after it was
normalized, and a failure to read the room or to record skips that copy
without recording it, so another bot's connection can still deliver the
message. A failure to read the sender's roles doesn't skip it: every surface
shares one `BotRoles`, so every connection would fail alike and the message
would be lost everywhere. The sender then counts as a person, with a warning
logged, as history already did; the router looks every sender up as a
managed agent whatever `sender_is_bot` says, so a managed agent's post still
takes the agent path. A copy recorded when the event receiver has just
closed is lost, which only happens at shutdown.

### Messages don't carry the sender's roles

**Issue.** A sender with the `bot` role must set `sender_is_bot`, but a
message's `u` holds only `_id`, `username` and `name`. `users.info` returns
another user's `roles` only to a caller with `view-full-other-user-info`
(`apps/meteor/server/lib/users/getFullUserData.ts`), which bots don't have.
Since whichever connection records a message first delivers it, a lookup made
with that connection's own token would classify agent A's post as a bot's
when A's connection won and as a human's when B's did. The `bot` field
doesn't help for agentd's own bots: `chat.postMessage` refuses it
(`additionalProperties: false` in its schema), so only integrations and apps
set it.

**Solution.** `BotRoles` reads roles with `users.info` and remembers them for
ten minutes. agentd builds one from the manager's client and shares it
between every surface, so the manager's custom role also needs
`view-full-other-user-info`; the design's Rocket.Chat section says so now. A
sender is a bot when the message has a non-false `bot` field or the sender has
the `bot` role. `RestClient` gains `user_info`, and `FakeRest` shows roles
only to the user itself and to the manager.

Without the permission, `users.info` leaves `roles` out rather than
failing, which would have classified every bot as a person without a word.
Every Rocket.Chat user has at least one role, so `BotRoles::is_bot` treats
an empty or missing list as `SurfaceError::Forbidden`, naming the missing
permission and the manager's user id (never the token), and doesn't cache
it. Messages still flow, as above, and each one logs the error, so a
misconfigured manager is loud until it is fixed. The cache keeps at most
10,000 users: making room drops expired entries, then the oldest.

### Room lists and room kinds come from REST

**Issue.** Nothing in the realtime API lists the rooms a user is in without
`__my_messages__`, and a `stream-room-messages` event has no room type, so it
can't tell a DM from a group DM.

**Solution.** Each connection lists rooms with REST `subscriptions.get`, and
the surface reads a room's `t`, `usersCount` and `uids` with `rooms.info` the
first time a message arrives from it, then keeps it for the surface's
lifetime (a DM's members are fixed), for at most 10,000 rooms, dropping the
oldest beyond that. `RestClient` gains `subscriptions`,
`file_url` (`<base>/file-upload/<id>/<name>`, for `InFile::url`) and
`credentials` (the realtime login reuses the token), and `FakeRest` answers
`subscriptions.get`. Only channels, private groups and DMs are listened to:
omnichannel rooms (`l`) and other types aren't agent conversations.

### `stream-room-messages` resends a message whenever it changes

**Issue.** `notifyOnMessageChange` broadcasts the whole message on every
change: a reaction, a reply in its thread (the root's `tcount`), a link
preview. Dedup by `_id` drops these for a message already recorded, but a
message recorded by nobody, posted before the bot joined or while every
connection was down, would look new when someone reacts to it, and would
arrive with the mentions it had then. Only `editedAt` marks an edit.

**Solution.** Besides `t` and `editedAt`, a message whose `_updatedAt` is
more than two minutes after its `ts` is skipped as a change to an old
message. The margin covers the `sendMessage` method, which accepts a client's
`ts` up to 60 seconds off the server's clock.

### `@all` and `@here` appear in `mentions[]`

**Issue.** Rocket.Chat puts the broadcasts in `mentions[]` with `_id` `all`
or `here`. They name no user.

**Solution.** They are left out of `InboundEvent::mentions`, so a broadcast
never counts as mentioning an agent. Repeated ids are kept once.

### Personal access tokens work as resume tokens

**Issue.** T12 logs in "with a `resume` token", and T11 issues personal
access tokens.

**Solution.** Rocket.Chat stores a personal access token as a hashed login
token with `type: "personalAccessToken"`
(`apps/meteor/imports/personal-access-tokens/server/api/methods/generateToken.ts`),
where the resume handler finds it, so the realtime client sends the bot's
token as `{"resume": token}`. A login error `403` ends `events` with
`SurfaceError::Unauthorized` instead of reconnecting forever. A login that
answers for another user id also ends it.

### DDP details the client relies on

- The server's first frame is `{"server_id": "0"}` from Meteor or
  `{"msg": "server_id", …}` from the split-out streamer. The client ignores
  anything it doesn't know.
- The streamer pings a client that has been silent for 30 seconds and closes
  the socket 30 seconds later (`TIMEOUT` in `ddp/constants.ts`). The client
  answers pings with their id, pings after 20 seconds of silence itself, and
  reconnects after 40 seconds without any frame.
- A stream subscription is `sub` with `params: [event, false]`; the second
  parameter turns off collection compatibility. A refused one gets `nosub`
  with `error: "not-allowed"`. A refused room is dropped and the connection
  stays up; a refused `subscriptions-changed` reconnects. The refused room
  is remembered until an `inserted` notice for it or the next connection,
  since `updated` notices would otherwise ask for it again on every unread
  change.
- `subscriptions-changed` carries `[action, subscription]`. `updated` fires
  on every unread-count change, so subscribing is idempotent. `removed` may
  lack `rid`, so each connection keeps the subscription document `_id` of
  each room, from `subscriptions.get` (`Subscription` gains `id`) and from
  `inserted` and `updated` notices, and resolves such a removal through it.
  The server also stops a room's message subscription itself when the user
  is removed, without telling the client, and the client sends `unsub`
  anyway. Because of that, `inserted` is authoritative: for a room already
  subscribed, the client sends `unsub` for the old subscription and
  subscribes again, so a removal it couldn't resolve doesn't leave the bot
  deaf after it is added back.

### Reconnecting lists the rooms again instead of remembering them

**Issue.** Resubscribing to every room a connection knew would include rooms
the bot was removed from while disconnected, and `stream-room-messages`
lets any user with `view-c-room` read a public channel's messages
(`canReadRoom`), so the bot would keep hearing a channel it left.

**Solution.** Each connection subscribes to what `subscriptions.get` lists
then, plus what `subscriptions-changed` adds, which also covers rooms joined
while disconnected. Messages posted while a bot had no connection are not
fetched; [Deferred work](tasks-plan.md#deferred-work) has a bullet for it.

### Changes that arrive while the rooms are listed

**Issue.** `subscriptions.get` runs after the `subscriptions-changed`
subscription is ready, so a notice can arrive while the listing is in
flight. Applied at once, a removal found nothing to unsubscribe, and the
listing, possibly read before the removal, then subscribed to the room
anyway.

**Solution.** Notices that arrive during the listing are kept, then applied
in order to the listed rooms before anything is subscribed: a removal takes
its room out (resolving a missing `rid` through the listing's document ids),
and `inserted` or `updated` puts it in. A notice about a change the listing
already reflects changes nothing.

### The backoff starts over only after a healthy connection

**Issue.** The backoff reset as soon as a connection had logged in and
subscribed, so a server that accepted and subscribed and then dropped the
socket, or refused `subscriptions-changed` right after, was reconnected at
the initial wait forever.

**Solution.** The backoff resets only when a connection stays up for the
longer of `backoff_max` and twice `heartbeat` (60 s by default). That is
long enough that reconnecting at once costs no more than waiting the
longest backoff, and longer than it takes to notice a silent server, which
is declared dead after twice `heartbeat`. A connection that drops sooner
counts as a failed attempt and the next wait doubles.

### Normalizing runs off the socket

**Issue.** Normalizing a message can call `rooms.info` and `users.info`, and
recording it hits the store. Doing that inline would delay answering the
server's pings.

**Solution.** The socket loop hands raw messages to the surface through a
channel of 256, and the surface normalizes them in order on its own.

### tokio-tungstenite uses rustls's default provider

**Issue.** tokio-tungstenite builds its TLS configuration with
`ClientConfig::builder()`, which takes the process-wide `CryptoProvider`, or
the one crate features select when exactly one of `aws-lc-rs` and `ring` is
enabled, and panics otherwise.

**Solution.** reqwest's `rustls` feature enables `aws-lc-rs`, and nothing
enables `ring`, so `wss://` connects work without installing a provider. A
test connects to a local port that drops the TLS handshake and checks that
the client retries instead of panicking, so a second provider appearing in
the graph fails CI.

### `Surface::render` has no mention directory

**Issue.** `render::rocketchat::to_markdown` resolves `@Name` through a
`MentionDirectory`, but `Surface::render(&self, markdown)` takes none, and
T23 says the pipeline builds a directory snapshot per reply.

**Solution.** `RocketChatSurface::render` passes a directory that resolves
nothing, so `@Name` stays as written. On Rocket.Chat that loses little, since
an `@username` in the text is already a mention and broadcasts are still
neutralized. If T23 needs display names resolved, it has to add the
directory to `Surface::render` (or render outside the trait).

### Thread history includes the root

**Issue.** `chat.getThreadMessages` returns replies only, newest first, and
has no `latest`, but T23 and `agentctl history` want the thread as a whole,
older than a cursor.

**Solution.** `history` for a thread reads pages of 100 newest first, keeps
messages older than the cursor message's `ts`, and adds the root once the
replies run out, as Slack's `conversations.replies` would. It reads at most
50 pages. Both thread and top-level history leave out system messages, so a
call can return fewer than `limit`.

## T13: account commands

### The relink channel is in memory, the notice has to be durable

**Issue.** `auth` sends a member on `Auth::take_relink_notices()` right after
its mark set `claude_links.broken_at`, so the store's `NULL`-to-set
transition already makes it one notice per break. But the channel lives in
one process: a crash or deploy between the mark and the DM loses the notice
for good, and a DM that fails (Rocket.Chat down, the manager rate limited)
has nothing to retry it. Remembering what was sent in memory would not
survive a restart and would not be shared by a second instance.

**Solution.** A migration adds `claude_links.relink_notified_at`, which
`put_claude_link` and `update_claude_tokens` clear together with
`broken_at`. What is owed is every link with `broken_at` set and
`relink_notified_at` empty (`Store::pending_relink_notices`). agentd's
`RelinkNotifier` claims each with a conditional `UPDATE` keyed by the link's
generation (`claim_relink_notice`) before it sends, so one instance at a
time sends it, and marks it sent afterwards (`mark_relink_notice_sent`). It
runs a pass at startup, whenever the `auth` channel wakes it, and every
minute. A member no manager bot can reach (a Slack-only member until T30)
is left pending without a claim.

A second migration adds `relink_attempts` and `relink_next_attempt_at`,
cleared with `relink_notified_at`, because a notice that can't be sent
(the member's account is gone, the manager is refused) would otherwise
cost a `users.info`, an `im.create` and maybe a post on the manager's
account, plus a warning, every minute forever:

- The claim is a lease: it counts an attempt and sets
  `relink_next_attempt_at` to ten minutes on. A process killed between the
  claim and marking the notice sent leaves the lease to run out, and the
  notice is pending again, so it is sent at least once, and twice only
  after such a crash. Before, the claim stayed set and the notice was lost
  for good.
- A failed send defers the next claim (`defer_relink_notice`) by a backoff
  that starts at a minute and doubles up to six hours.
- After 20 attempts, about three days, the notice is no longer pending,
  and the notifier logs once that it gave up. A crash during the last
  attempt gives up without that log line.

The notifier reads the clock through a function its tests replace, so the
backoff and the lease are tested without waiting.

### A DM to a member needs their username

**Issue.** `reply_private` for a channel command, and the relink notice,
open the manager bot's DM with the member. Rocket.Chat's `im.create` takes a
`username`, and an `InboundEvent` carries only the sender's user id.

**Solution.** `RocketChatDms` calls `users.info` for the username, then
`im.create`, which returns the existing DM when there is one. To keep the
common case to one call (the manager is subject to the REST rate limiter
unless its role has `api-bypass-rate-limit`), `Origin::RocketChatDm` carries
the DM's room, which the event already names, and a reply there posts
straight to it. The plan's `RocketChatDm` had no field; the T13 bullet
says so now. `Origin::SlackSlash`'s `response_url` is a `SecretString`,
since anyone holding it can post in the channel for a while.

### Which Rocket.Chat messages are commands

**Issue.** The plan says a DM to the manager bot is a command and a channel
message starting with `!agent` is one. It doesn't say what happens to the
manager bot's own replies, which come back on its connection like any
message in the DM, to bots' messages, or to a DM with an agent's bot.

**Solution.** `commands::rocketchat::command_in` drops every message from
a bot (`sender_is_bot` or `sender_bot_user`) and from the manager bot
itself, or its replies would be parsed as commands and answered in a loop,
and an agent could be talked into running `!agent delete`. Only a
one-to-one DM that reached the manager bot's own binding is
`RocketChatDm`. A DM with an agent's bot is treated as any other room: it
is a command only with `!agent`, and it is not private, since the agent's
sessions can read that conversation's history, so a login code there is
refused.

### Every bot connection has to look for commands

**Issue.** The surface records each Rocket.Chat message once, under one
source for every bot (T12's `Dedup`), and delivers it only on the
connection that records it first. The first version heard commands only on
the manager bot's connection. Once T14 adds the agents' connections, an
`!agent` message in a room the manager shares with an agent would be lost
whenever the agent's connection recorded it first, and one in a room or DM
without the manager would never be heard.

**Solution.** `commands::rocketchat::CommandIntake` owns the channel, the
per-member ordering and the drain at shutdown, and knows nothing of any
connection. Each connection delivers through a `CommandFeed`'s
`into_sender(onward)`, which runs `command_in` with the manager bot's
binding on every event the connection won, sends commands to the one
intake and passes only other messages to `onward`, so a command is never
also taken as a turn. The manager bot's connection is one feeder; T14
feeds every agent's too, and from then on passes the manager's other
messages onward as well, since whichever connection wins a message
delivers it for every bot in the room. `command_in` already treated a
message on another binding as not private, so a login code in a DM with
an agent's bot is refused whichever connection heard it. The intake runs
until every feed is dropped, so it finishes the commands it received after
the connections stop.

### A member's commands run in order, others' alongside

**Issue.** A code exchange can take the token endpoint's 30-second timeout,
so running commands one after another on the connection would hold up
every member. Running each in its own task could swap one member's
`logout` and `login`, or answer `me` before the `login <code>` sent just
before it.

**Solution.** Each command runs in its own task inside the intake's
task, and waits for the previous command of the same member to finish
first (a `oneshot` per member, pruned once finished). On shutdown the
connections stop listening, and the intake runs the commands it already
received (the store has recorded them as processed, so no other instance
would) and waits for them within the drain timeout.

### Secret-looking text that doesn't parse, in a channel

**Issue.** `ParseError::is_secret_bearing` says malformed text may hold a
secret, but not which kind, so the plan's two refusals (cancel pending
logins, or revoke the key) can't be picked.

**Solution.** Both apply: the member's pending logins are cancelled (it
costs at most a new `login`), and the reply tells them to revoke any key or
token they posted, followed by the parser's usage message. A secret-bearing
command that parses gets the refusal for its kind: `login <code>` cancels
the pending logins, `admin api-key set` says to revoke the key at the
Anthropic Console, `slack-token` says to revoke it at api.slack.com. None
of them is used. The refusal matches every command explicitly, so a new
secret-bearing command doesn't compile until it has its own advice. A
public `login <code>` also takes the pending login its `state` names
(`Auth::cancel_pasted_login`, with `auth`'s own paste parsing and no
exchange), whoever started it: the sender's own pending logins are not
necessarily the one the code belongs to. agentd can't delete the message (the `bot` role lacks
`delete-message`), so the reply suggests the member does.

### Smaller choices the plan left open

- `login` creates the member (`ensure_member`); `login <code>`, `logout`
  and `me` only look the member up, so asking `me` doesn't create anyone.
  A new member's display name is their user id, since the event has no
  username.
- `logout` also cancels the member's pending logins, so a login link sent
  before the logout can't link the account again.
- Commands later tasks implement answer "`<name>` isn't available yet."
  `admin api-key set` from a DM stores nothing until T26.
- The manager bot has no stored binding before T14's `agent_bindings`, so
  its `BindingId` is new at every start. `App` keeps its surface, binding
  and the one `BotRoles` every Rocket.Chat surface shares, for T14.
- T13 adds the `[claude_oauth]` section (all of `auth::OAuthConfig`,
  checked at load, key named as `claude_oauth.<key>`) and `[rocketchat]`
  with `base_url`, optional `websocket_url`, `team` and `manager_user_id`;
  `AGENTD_RC_MANAGER_TOKEN` is required with it. `team` has no default,
  because deriving it from the URL would change every stored identity when
  the URL changes.
- The manager bot now posts every command reply, and `users.info` plus
  `im.create` for a channel command, so its custom role should include
  `api-bypass-rate-limit`, as the T11 note on the role expected; the
  README says so.
- `surface-rocketchat`'s `conv_kind` took a `d` room whose `rooms.info`
  had neither `usersCount` nor `uids` for a one-to-one DM, which would
  make a DM of unknown size private enough for a login code if it reached
  the manager bot. It is a group DM now, and only a count or a member list
  of at most two makes a DM one-to-one.
- The captured-log test logs at `trace` for every crate through a whole
  DM login, exchange included, and finds neither the code nor the pasted
  text, so reqwest, hyper and sqlx don't log request bodies either.

## T14: Agent lifecycle on Rocket.Chat

No Rocket.Chat server was run for this task; the tests use `testkit`'s fake
server, and the server behavior below comes from the notes of T11 and T12.

### A paused agent's bot keeps listening

**Issue.** "Pause (events ignored)" could be read as stopping the paused
agent's connection, or as dropping what it delivers. Rocket.Chat
deduplication is global: whichever connection records a message first
delivers it for every bot in the room. A paused bot that dropped what it
recorded would lose messages for the other agents there, and a paused bot
that stopped listening would leave `!agent resume` unheard in a room it
alone shares with agentd.

**Solution.** A paused agent's connection keeps running and feeding the
command intake. "Ignored" is decided where messages go after the intake:
before T23, [`Acknowledge`](#before-turns-a-bot-reacts-instead-of-replying)
skips paused agents; from T23 the router refuses them, as T22 already does.
Only `delete` stops a connection.

### A creation can stop halfway

**Issue.** Creating an agent is several steps on two systems: the store,
then `users.create`, the bot's login and token, the avatar, then the store
again. A failure or a crash in between would leave an agent without a bot,
or a bot user nobody records, with the agent's name taken for good.

**Solution.** `create_agent` stores the agent with a binding in state
`creating` in one transaction; the unique index on `(owner_id, name)` only
covers agents that aren't deleted, so the name is reserved from then on.
Before each `users.create` the binding notes the username it asks for
(`set_binding_bot_username`), and the bot user is recorded on the binding
(`set_binding_bot_user`) as soon as `users.create` returns, whatever the
binding's state by then: a creation abandoned meanwhile leaves a disabled
binding with a bot user, which owes retirement like a deleted agent's and
gets its leased, backed-off retries, rather than one best-effort
deactivation. `activate_binding` stores the token and makes the binding
`active` only while it is still `creating`. Any failure abandons the
creation (`abandon_creation`): the binding is disabled, the agent deleted
and the name freed, and a recorded bot user is deactivated. If recording
the bot user fails in the store, it is deactivated at once, before the
error is reported. A creation still `creating` after `CREATION_LEASE` (ten
minutes, far longer than its REST calls with their 30-second timeouts can
take) is abandoned by the next supervisor pass, which covers a crash. If
that races a slow creation, the creation's own `activate_binding` fails and
it gives up.

A crash, or a store failure, between `users.create` answering and the bot
user being recorded leaves a bot user no binding records. Abandoning a
creation therefore looks the noted username up with `users.info` and, if
that user's email is the binding's (`agent-<binding id>@agent-core.invalid`),
records it, so it is retired. That needs the manager's
`view-full-other-user-info`, without which `users.info` leaves the emails
out, and it is tried once, when the creation is abandoned. A bot user
still missed has no token and no password anyone knows, so it can't be
used, but it keeps its username until an admin removes it.

### Deactivating a deleted agent's bot is owed until it happens

**Issue.** `delete` deactivates the bot user with the manager's
`users.setActiveStatus`, which can fail: the manager lacks
`edit-other-user-active-status`, Rocket.Chat is down, the rate limiter.
Forgetting the failure would leave an active bot user whose personal access
token still works.

**Solution.** `delete_agent` disables the agent's bindings and forgets their
tokens in the same transaction that marks the agent deleted. A disabled
binding with a bot user and no `retired_at` owes its retirement, in columns
modeled on the relink notices': a claim counts an attempt and holds a
ten-minute lease (`retire_attempts`, `retire_next_attempt_at`), success sets
`retired_at`, and a failure defers the next attempt by a backoff from a
minute doubling to six hours, for 20 attempts (about three days). `delete`
tries once at once and says whether it worked; the supervisor retries every
pass. A bot user Rocket.Chat no longer knows counts as retired, but only
when it says so with `error-invalid-user` or `error-user-not-found`: a bare
HTTP 404, such as a reverse proxy's, defers the attempt like any other
failure. `delete` reports the deletion even when looking up or retiring the
bindings afterwards fails in the store; that failure is logged and left to
the supervisor.

### Connections follow the store

**Issue.** agentd has to start a connection when an agent is created, stop
it when the agent is deleted, and restore every connection at startup. With
more than one instance (a blue-green deploy), an agent created on one
instance would be heard only there until a restart.

**Solution.** The `Supervisor` owns the connections and derives them from
the store: each pass starts one for every `active` binding of an active or
paused agent and stops the rest. A pass runs at startup, whenever a
command pokes it (`create`, `delete`), and every minute, so another
instance's changes are picked up within a minute. After the connections,
so slow REST calls don't delay them, it abandons stale creations and
retires what is owed. A binding whose row doesn't read (a token that no
longer decrypts) is logged and skipped, and the other bots keep listening. The command handlers never hold a
`CommandFeed`, only the poke: the intake runs until every feed is dropped,
and it owns the handlers, so a feed held there would keep it running
forever. The supervisor drops its feed when agentd stops, after stopping its
connections. A connection that ends on its own (a revoked token, a
deactivated bot) or panics is logged and started again by a later pass: the
supervisor maps each connection task's id to its binding, so a panic, which
returns no value, still frees the binding. A connection that keeps ending
waits longer each time, kept in memory per binding: the next pass after its
first end, then one interval, doubling up to 32 intervals (32 minutes), and
one that ran that long starts over. A broken bot so logs an error about
twice an hour, not every minute, until it is fixed or deleted.

### Retiring a bot and stopping its connection happen in either order

**Issue.** `the_supervisor_follows_the_store_and_restarts_ended_connections`
failed under CPU load, always at "the pass retired the bot": 1 of 200
runs with 8 busy loops on 4 CPUs, 34 of 200 with 16. It waited for the fake
server to count no connections, then read the binding once. A pass stops a
connection by signalling its task, which closes the socket on its own while
the pass goes on to `abandon_stale` and `retire_pending`, so the socket can
close before `mark_retired` runs.
Adding 300 ms before `mark_retired` failed it 10 of 10 runs without load.
The order can also flip: a delete that lands between a pass's `reconcile`
and its `retire_pending` is retired by that pass and disconnected by the
next, and the `delete` command retires the bot before it pokes.

**Solution.** The supervisor stays as it is. Both steps follow from the
disabled binding, every pass does both, and a delete pokes after disabling
it, so the end state is the same in either order within a pass: the bot
user deactivated, `retired_at` set, and no connection. The test now waits
for that whole end state through a shared `eventually` helper, and passed
200 of 200 runs with 8 busy loops, 200 of 200 with 16, and 10 of 10 with
the 300 ms added.

`a_bot_made_after_its_creation_was_abandoned_owes_retirement` had the same
shape: a 100 ms sleep stood in for "`create_bot` has recorded the username
and sent `users.create`", and a 300 ms response delay for "the abandonment
lands before the response". Adding 150 ms before the username is recorded
failed it 10 of 10 runs. Its `users.create` response is now held until the
test has abandoned the creation, so the abandonment always lands while the
request is in flight; it passes 10 of 10 with the 150 ms added.

The hold is `testkit::Held`, and the other tests that slept and assumed a
delayed response was still on its way now use it too: the auth tests that
act during a refresh, the command test that expects a second member's reply
while the first member's login is out, and the Slack test that changes the
managed bots during a member refresh. Each waits for its request to arrive,
acts, then releases the response, and fails after 30 seconds rather than
hanging when the request never comes. The logout test only waits for its
refresh to arrive: logout queues behind that refresh on the member's lock,
so holding the response would deadlock, and either order ends the same.

`concurrent_callers_share_a_failed_refresh_of_an_expired_token` needed more
than a hold. Its five callers must all join the refresh before the 503
lands; one that joins later finds no refresh in flight and starts its own,
which sends a second request. That is the intended behaviour, not a
bug: T09's notes say an expired token is always retried because there is
nothing to hand out instead, and the backoff only covers a still-valid
token. `Auth` had no observable point where every caller had joined, so the
200 ms response delay was the only margin, and 300 ms added before callers
2 to 5 start failed the test 10 of 10 runs. The in-flight map now holds the
refresh's `watch::Sender` rather than a receiver, and a doc-hidden
`Auth::refresh_waiters(member)` returns its receiver count, which is the
number of callers waiting. The test holds the 503, waits until that count
reaches 5, then releases it. It passes 10 of 10 runs with the 300 ms added
and 200 of 200 with 8 busy loops. The map's sender would keep the channel
open if the refresh task were dropped before it first ran, so the guard that
removes the map entry now goes into the task when it is spawned, not on its
first poll. Waiters of such a task get `RefreshInterrupted`, and the next
caller starts a new refresh.

### Before turns, a bot reacts instead of replying

**Issue.** The plan allows a fixed acknowledgement before T23, and the live
check needs to see which bot a mention reaches. A reply is a
`chat.postMessage`, which makes the bot join a public channel it isn't in
(see T11's notes). Rocket.Chat deduplication is global, so a mention of an
agent that isn't in the room can be delivered by another bot's connection,
and replying would pull the mentioned bot into the channel. T23 has the
same problem for real replies.

**Solution.** Until T23, every connection passes the messages that aren't
commands to `Acknowledge`, which makes each active agent a person's message
mentions, and in a one-to-one DM the agent whose bot received it, react
with `:eyes:` as its own bot. `chat.react` doesn't join the room. Messages
from bots, the manager bot and managed agents are ignored whatever the
surface flags say. T23 replaces `Acknowledge` with the pipeline, and should
check that a mentioned agent is in the room before posting there.

### Bot usernames

**Issue.** The plan names the bot `<name>`, or `<owner>-<name>` when taken,
but not what happens when both are taken, what email Rocket.Chat's required
field gets, or how an owner's username is found; members are stored with
their user id as display name.

**Solution.**

- The username is `<name>`, then `<owner>.<name>`, where `<owner>` is the
  owner's username from `users.info`. If both are taken, the agent isn't
  created and the owner is asked for another name. The separator is a dot
  because agent names can't contain one (they are `a-z`, `0-9` and `-`),
  while Rocket.Chat usernames can: with `-`, alice could name an agent
  `bob-helper` and take the username bob's `helper` would fall back to.
  Now no agent name, and no other owner's fallback, can be
  `bob.helper`. `all` and `here` go
  straight to the prefixed form, since Rocket.Chat reads `@all` and `@here`
  as broadcasts and nobody could mention such a bot.
- The display name is the agent's name. The email is
  `agent-<binding id>@agent-core.invalid`: unique, unverified (T11), and in
  a domain that can't receive mail.
- `agent_bindings.bot_username` records the username, which the plan's
  columns didn't have, so `list` and the create reply can show `@username`
  without a lookup.
- `create` stores the owner's username as their display name, which `list`
  shows as the owner.
- A deleted agent's bot user stays, deactivated, and keeps its username
  for good: creating an agent of the same name again gets the prefixed
  username, and once that one is deleted too, the name can't be created
  again by that owner until an admin removes the old bot users.

### Agent names are the owner's

**Issue.** Names are unique per owner, so `persona helper …` from a member
who isn't the owner can't name the owner's `helper` at all.

**Solution.** Every owner-only command looks the agent up among the
sender's own agents, so a non-owner gets "You have no agent named `helper`.
Only an agent's owner can change it." The name is free again once the agent
is deleted, since deleted agents keep their row for the volumes, sessions
and message refs that name them. `visibility` is `public` or `private`;
`list` shows private agents only to their owner, and nothing sets `private`
yet.

### A bot sets its own avatar

**Issue.** Setting another user's avatar needs `edit-other-user-avatar`
(T11's table), one more permission for the manager's role.

**Solution.** The new bot sets `rocketchat.avatar_url` as its own avatar
with its token, which Rocket.Chat allows while `Accounts_AllowUserAvatarChange`
is on (the default). A failure is logged and the agent is created anyway.
The Compose README's permission table drops `edit-other-user-avatar`, and
`edit-other-user-info`, since agentd renames no bot.

### The text of an upload

**Issue.** A `persona.md` upload's command is the message's text. Depending
on the client and version, Rocket.Chat puts the text typed with an upload
in `msg` or in the file attachment's `description` (`sendFileMessage`
builds the attachment from the upload's description and takes `msg` from
the confirm body).

**Solution.** `surface-rocketchat` takes the attachment's `description` as
the text of a file message whose `msg` is empty. Which one a real 7.x
client fills is for the live check. The file is downloaded from
`<base>/file-upload/<id>/<name>` with the manager's `X-User-Id` and
`X-Auth-Token` headers (Rocket.Chat's `requestCanAccessFiles` accepts them),
never with the token in the URL, and refused past 64 KB by its
`Content-Length` or while it is read, as `SurfaceError::TooLarge`, a
variant added so callers don't match on error text.

With the Amazon S3 or Google Cloud Storage file store, Rocket.Chat answers
the download with a 302 to a presigned URL in the bucket (the default,
`FileUpload_S3_Proxy_Uploads` off). reqwest follows redirects and drops
`Authorization` and `Cookie` on a cross-origin hop, but not custom headers,
so the manager's `X-User-Id` and `X-Auth-Token` would reach the object
store. `RestClient` now follows redirects only within the origin it called
(scheme, host and port; `http` to `https` on the same host counts as
another origin), up to 10, and stops at any other, for every REST call, so
the headers never leave the server. `download` then fetches a cross-origin
`Location` once, with a separate client that sends no Rocket.Chat header
and follows no redirect, and applies the same size limit: the presigned URL
authorizes itself. A file message's text falls back to the attachment's
`description` only when the message has files and an empty `msg`, so a
message without files, such as one quoting another, keeps its own text. Only a file attached in the manager
bot's DM is read: a persona uploaded to a room would be public anyway, but
the manager may not be able to read files there.

### A member's agents are capped

**Issue.** Each agent is a Rocket.Chat user with a token and a realtime
connection agentd keeps open, and nothing stopped one member from creating
hundreds of them.

**Solution.** `[agents] max_per_owner` (default 10, at least 1) caps the
agents that aren't deleted per member. `create_agent` counts them inside
its `BEGIN IMMEDIATE` transaction, so concurrent creations can't both slip
under the cap, and returns `LimitReached`; `create` then tells the member
the limit and to delete one first. Deleted agents don't count, but their
bot users stay, deactivated.

### The manager's permissions on the Community Edition are still open

**Issue.** T16 found that custom roles need an Enterprise license. T14 adds
nothing to the role T11 derived: `create-user`, `edit-other-user-active-status`
for `delete`, `add-user-to-joined-room` for `!agent create` in a room,
`view-full-other-user-info` and `api-bypass-rate-limit`, plus
`create-personal-access-tokens` on the `bot` role.

**Solution.** Unresolved, as the design's open question says. On the
Community Edition these permissions can only be added to a built-in role,
and the built-in roles the manager would hold are shared (`user` with every
member, `bot` with every agent), so granting them there grants them to
everyone who holds that role. Until a live check settles it, the Compose
README gives the manager `admin` for development. This task couldn't test
it without a server.

## T15: agentctl

### The token needs the turn's thread and message

**Issue.** The plan's `ctl_tokens` columns for the current turn are
`turn_id`, `requester`, `hop`, `kind` and `side`. The target rules need the
current conversation ("`post` may target only the current conversation"),
`react` without a message id needs the message that started the turn, and
`history` needs the thread. None of them is in those columns, and T15 can't
read T21's `sessions` table, which doesn't exist yet.

**Solution.** `begin_turn` takes a `CtlTurn` that also carries the turn's
`ThreadKey` and its trigger message, stored as `conversation` (the
`ConvRef` string form), `thread_root` and `trigger_message`. `requester` is
two columns, `requester_member` and `requester_key`, as in T23's
`message_refs`, and `kind` is `kind` plus `consent_id`. A `CHECK` makes the
turn columns all set or all NULL. The plan's T15 bullet lists the columns.

### One token per session, not only per process

**Issue.** The plan says one token per `claude` process, but T21's hooks are
keyed by session, and nothing stopped two live tokens for one session if a
process restart issued a token before revoking the old one.

**Solution.** `session_id` is unique in `ctl_tokens`, and
`issue_process_token` deletes the session's old token (and drops its
outbox) in the same transaction it inserts the new one. A session runs one
process at a time, so the newest process's token is the only one that
works.

### Targets needed a grammar

**Issue.** `PostRequest::to` and `ReactRequest::message` are "strings as
the model wrote them", and the plan doesn't say how the model names a
conversation or a thread.

**Solution.** `--to` takes `here` (the turn's thread), a conversation id
(its top level), or `<conversation id>/<message id>` (a thread in it). A
conversation id may be written `#C123` or as Slack's `<#C123|name>`, and
names a conversation on the turn's own surface and team. Ids start with a
letter or digit and hold only letters, digits, `.`, `_` and `-`, so `..` or
a path never reaches a surface's URL. `react` takes a message id, or
`<conversation id>/<message id>`, and on either side may name only messages
in the current conversation; the plan's `Owner` rule widens `post` only.
Channel names (`#general`) are not resolved: a public turn is refused with a
hint to use `here`, and an owner turn's post is refused by the surface.

### Message ids are platform ids until T23

**Issue.** The design shows the model short message ids from a per-session
table (T23's `message_refs`), but T15 comes first, so `react <emoji> <id>`
and `history --before <id>` have nothing to resolve a short id against.

**Solution.** Both take platform message ids for now (a Slack `ts`, a
Rocket.Chat `_id`). T23, which introduces short ids, resolves them in the
ctl handlers; its bullet in the plan says so.

### A refused upload lost its answer

**Issue.** `attach` refused a file over the cap as soon as it saw the
`Content-Length`, without reading the body. agentctl was still sending, so
when agentd closed the connection reqwest reported a failed request, and the
model saw "the request failed" instead of the limit. With an 8 MiB file and a
1 KiB cap this happened in about one run in three.

**Solution.** After any refusal inside `attach` (size, name, slots, no
turn), agentd reads the rest of the body and throws it away before
answering, within the same 5-minute limit as an upload. Nothing is written to
disk while draining, and only an authenticated caller gets this far. An
unauthenticated request is refused before its body is read.

### reqwest honors `HTTP_PROXY`

**Issue.** Sandboxes have `HTTP_PROXY` and `HTTPS_PROXY` pointing at the
egress proxy (T19), and reqwest reads them even with its default features
off, so agentctl's plain-HTTP calls to `agentctl.internal` would go through
the egress proxy.

**Solution.** agentctl builds its client with `no_proxy()`. The end-to-end
tests run it with `HTTP_PROXY`, `http_proxy` and `ALL_PROXY` pointing at a
closed port.

### Lease times are whole seconds

**Issue.** Store timestamps are Unix seconds, so a lease granted at
`now + ttl` really lasts between `ttl - 1` and `ttl` seconds, and a
one-second lease can end almost at once.

**Solution.** The lease lasts 30 seconds (`DEFAULT_LEASE_TTL`). agentctl
relies on it only until two seconds before its `seconds_left` (below) have
passed: one for the rounding, and one because another command may take the
lock at the expiry itself. It renews it when a third of that remaining time
has passed, at least every 200 ms. A `ttl` below one second counts as one.
agentctl needs a lease of at least three seconds: a new lease that leaves
less than half a second before that point, enough to wait 200 ms and renew,
is released at once, and `lock` fails with "the shared/ lock's lease is too
short to hold". With a two-second lease the deadline came at the grant, and
a first renewal lost the race about one run in five. Tests that renew use a
3-second lease.

### The lease is timed on agentctl's clock

**Issue.** agentctl compared the lease's `expires_at`, from agentd's wall
clock, with the sandbox's clock. Probes against the binary showed a server
40 seconds behind made every lease look expired, so `lock` killed its
command at once, and one 5 seconds ahead let the command run 5 seconds past
the real expiry, under the next holder.

**Solution.** `LockResponse::Held` also carries `seconds_left`, which agentd
computes from the same clock and second it stored the lease with
(`expires_at` minus now, in whole seconds; the lease really lasts between
`seconds_left - 1` and `seconds_left`). agentctl times the lease on its
monotonic clock from when it sent the request: its deadline is the send
instant plus `seconds_left`, minus a second for rounding and a second of
margin. agentd measured no earlier than the send, so a slow answer only
makes the deadline earlier. `expires_at` stays in the response for logs and
other readers.

### What agentctl does when it loses the lock

**Issue.** The plan says the lease is renewed while the command runs, but
not what happens when a renewal fails: agentd refused it (the lease
expired, or the turn ended), or agentd couldn't be reached. A first version
also awaited each renewal on its own, bounded only by the 30-second request
timeout, so a renewal agentd never answered let the command keep writing
past the lease's expiry, and delayed `SIGTERM` by as long.

**Solution.** The renewal runs in the same `select!` as the command's exit,
the stop signals and the lease's deadline, and its request timeout is capped
at the time left until that deadline. A renewal agentd refused (the lease
expired or was released, no turn, a revoked token), or no successful
renewal by the deadline, means another command may soon hold the lock, so
agentctl kills its command's process group with `SIGKILL` at once and exits
1 with "lost the shared/ lock (…); stopped the command". A renewal that
failed in transit, or that agentd answered with its internal error ("agentd
failed; try again", say a busy SQLite database), is retried until the
deadline: a probe that returned one 500 during a 30-second lease had killed
the command with 19 seconds of the lease left. The signal handlers are
installed before the lease is acquired and kept until the release is sent,
so a signal is never lost in between, and the release waits at most two
seconds, after which the lease expires on its own.

On `SIGTERM`, `SIGINT` or `SIGHUP` while the command runs, agentctl passes
the same signal on to the command's process group, which being its own
group no longer gets a terminal's signals, waits up to two seconds for the
command to exit (never past the lease's deadline), and then kills the group
with `SIGKILL`. A `SIGKILL` at once had left `git` no chance to remove its
`index.lock`. The command is left unreaped while agentctl waits
(`waitid` with `WNOWAIT`), so the group's id can't be reused before the
kill. agentctl then releases the lease and exits with 128 plus the signal.

A signal while an acquire is in flight used to drop the request, and a
lease agentd granted for it held the lock with nobody renewing it, for up
to 30 seconds. agentctl now lets a request already sent finish, for up to
two seconds, releases the lease if it was granted, and exits with 128 plus
the signal without running the command. A signal between attempts exits at
once. agentctl waits at most 100 seconds for the lock
by default (`--timeout`), below the 2 minutes Claude Code's Bash tool gives
a command by default, so the model sees why it failed rather than a killed
command.

### The command runs in its own process group

**Issue.** Killing the command's process stopped only that process. With
`sh -c '…'`, which the CLI help recommends, every process the shell started
survived: it kept writing under the next holder, and it held agentctl's
standard output and error open, so a caller reading them to the end hung.
Tokio's `Child::kill` signals one process, and the standard library has no
call to signal a process group.

**Solution.** agentctl spawns the command with `process_group(0)` and kills
the whole group with `SIGKILL`, through `rustix::process::kill_process_group`
(rustix 1, the `process` feature only: safe, pure Rust, and it builds for the
static musl target), before reaping the command. The group is signalled only
while the command hasn't been reaped, so its id can't have been reused. A
process that leaves the group (`setsid`, say) escapes, processes the
command leaves running when it exits on its own are not stopped, and an
agentctl killed with `SIGKILL` leaves its command running once the lease
expires. So does a caller that kills agentctl alone: if Claude Code's Bash
tool kills a command that runs past its timeout through the command's
process group, agentctl's command is no longer in that group and survives
it. Whether the CLI kills the group or the process, and with which signal,
has to be checked against the real CLI; T23's live check does. Being in its own group, the command is not in the terminal's
foreground group, so it can't read from a terminal; agentctl runs under the
model's Bash tool, which gives it none. The lock is a guard for cooperating
commands, as the design's "scope-level lock that `agentctl` takes for
writes" is.

### A lease outlived its turn

**Issue.** Release goes through the same extractor as every command, which
refuses a token between turns, and ending a turn or revoking a token left
`scope_locks` alone. A lease taken in a turn that ended, or by a process
whose token was revoked, held the lock until it expired, up to 30 seconds.

**Solution.** The store deletes the session's leases in the same
transaction that records or clears a token's turn (`set_ctl_turn`), deletes
the token (`delete_ctl_token`), or replaces it with a new token for the
session (`put_ctl_token`). A lease lasts no longer than the turn that took
it, and the lock is free as soon as `end_turn` or `revoke_process_token`
returns.

That alone didn't hold when `begin_turn` replaced a turn still recorded on
the token: an acquire authorized under the first turn could land after the
second turn's delete, and its lease was then renewed under the second turn.
So acquire and renew name the token's digest and the turn they were
authorized under, and each is one statement that takes the volume and
session from the token's row only while it still records that turn
(`INSERT … SELECT … FROM ctl_tokens WHERE hash = ? AND turn_id = ?`, and
`… (volume_key, holder_session) IN (SELECT …)` for renew). A request
authorized under a replaced or ended turn grants and renews nothing. Release
names only the token, since giving a lease back is always safe.

### `lock` is refused inside private tasks

**Issue.** The plan's refusal rule allows only `attach` inside a
`TurnKind::PrivateTask` turn, which includes `lock`. An owner-requested
private task mounts `shared/` read-write (T33), and without `lock` it can't
take the lock that guards writes there.

**Solution.** T15 follows the rule as written: every command but `attach`
goes through the extractor that refuses private tasks, so a new command is
refused unless it opts out. T33 should decide whether owner-requested tasks
may take the lock.

### A data directory key

**Issue.** Attachments are staged "under the agentd data directory", but no
key named one.

**Solution.** `store.data_dir`, required and absolute. Attachments go in
`ctl-outbox/<random>/` under it, one directory per turn, created with mode
0700, holding files named by random UUIDs; the model's file name is only
display text and is refused if it holds `/`, `\`, a control character, an
invisible formatting character (bidirectional controls such as U+202E, which
can make `exe.txt` read as `txt.exe`, zero-width characters, tag characters,
or a line or paragraph separator), or is `.` or `..`. Dropping the `Outbox` that `end_turn` returns deletes the
directory, and startup empties `ctl-outbox/`. The cap is
`limits.attach_max_bytes` (default 50 MiB). A turn may stage at most 10
files, queue 10 posts of up to 40,000 bytes and 20 reactions; uploads in
flight count against the 10.

### A musl build is static-pie

**Issue.** None; confirming the plan. `readelf -l` on the
`x86_64-unknown-linux-musl` release build shows `Elf file type is DYN` and a
`DYNAMIC` segment, but no `INTERP`, and `cargo tree` for the target shows no
`cc`, `ring`, `rustls` or `openssl`.

**Solution.** The `agentctl-static` CI job checks for `INTERP` only, as the
plan says.

## T16: sandbox image and Compose

### An internal network still reaches the host

**Issue.** `internal: true` removes a network's route out, but Docker still
gives the host the network's gateway address on the bridge. On Docker
29.3.1, a container on an internal network connected to a listener that a
host process had bound to `0.0.0.0`, through the gateway address. A sandbox
could therefore reach anything listening on the host's wildcard address:
sshd, a database, a development server. Published container ports were not
reachable that way, and names outside the network didn't resolve.

**Solution.** The `sandbox` network also sets the bridge driver option
`com.docker.network.bridge.inhibit_ipv4: "true"`, so the host has no
address on it; the same probe then fails, while agentd's address and its
aliases on the network still answer. `scripts/ci/compose-test.sh` starts a
listener on the host's wildcard address, shows it reachable through the
`egress` gateway, and checks that a sandbox reaches it through neither
gateway. Whatever creates the sandbox network outside this Compose file
(a production deployment) needs the same option. The plan's network
section says so.

### Static addresses need an `ip_range`, and the range moves the gateway

**Issue.** Compose starts containers in dependency order, and a container
without a static address can take any free one. MongoDB started before
agentd and took `172.31.0.2`, so agentd failed with "Address already in
use". Limiting dynamic addresses with `ip_range` fixed that, but Docker
then made the first address of the range, `172.31.0.128`, the gateway.

**Solution.** Both networks set `ip_range` to the upper half of their
subnet (`.128/25`) and name the gateway (`.1`) explicitly. agentd's static
addresses stay in the lower half, so a container started earlier, or a
sandbox started while agentd is recreated, can't take them. That leaves
about 126 addresses for sandboxes on the default `/24`; a deployment that
needs more running at once widens the subnet in both `compose.yaml` and
`internal.sandbox_subnet`.

### Custom roles need a Rocket.Chat Enterprise license

**Issue.** The design gives the manager a custom role. On 7.13.9,
`roles.create` is registered in `apps/meteor/ee/server/api/roles.ts` with
`license: ['custom-roles']` and refuses without that license module, and
`roles.update` refuses for any role that isn't protected (built in). The
Community Edition, which the Compose stack runs, can only change which
built-in roles hold a permission (`permissions.update`, which needs
`access-permissions`).

**Solution.** `deploy/compose/README.md` lists the permissions from T11's
reading of the source, creates the custom role where a license allows it,
and otherwise gives the manager `admin` for development. What least
privilege looks like on the Community Edition is added to the design's
open question on the manager's role, for T11's live check to settle.

### The native installer isn't pinned

**Issue.** `https://claude.ai/install.sh` redirects to
`https://downloads.claude.ai/claude-code-releases/bootstrap.sh`, which
downloads the latest build and runs `claude install <version>`: the pinned
build goes to `~/.local/share/claude/versions/<version>`, linked from
`~/.local/bin/claude`. The first image ran that as root, so whatever the
latest script and build were at build time ran with root in the image,
and nothing this repository pins said which bytes `<version>` was. Sessions
also run with a `HOME` of their own on a read-only root, so nothing under
the build's `HOME` is on their path.

**Solution.** The image skips the installer and downloads
`<releases>/<version>/<platform>/claude` itself, as `nobody` in a stage of
its own, with `platform` `linux-x64` or `linux-arm64` from BuildKit's
`TARGETARCH`, the layout the installer and the binary's updater use (the
2.1.285 binary names `https://downloads.claude.ai/claude-code-releases` as
its release base). The download must match `CLAUDE_CODE_SHA256_X64` or
`CLAUDE_CODE_SHA256_ARM64`, pinned next to `CLAUDE_CODE_VERSION`, and print
`<version> (Claude Code)`; only then is it copied to
`/usr/local/bin/claude`. The checksums are `platforms.<platform>.checksum`
in the release's `manifest.json`; for 2.1.285 the manifest was read from
the release bucket, and the `linux-x64` binary downloaded from it hashed to
the manifest's value and printed `2.1.285 (Claude Code)`. The CI build
downloads it from `downloads.claude.ai` and `sha256sum` reports it OK.
A copy outside
`~/.local/bin` is left alone by the auto-updater, which
`DISABLE_AUTOUPDATER=1` also turns off. `CLAUDE_CODE_VERSION` is still the
only place the version is written: the CI check reads it from there.

The base images (`rust`, `debian`, distroless `cc`) are pinned by the
digests the CI build log printed for their tags. The Compose file's
third-party images (Rocket.Chat, MongoDB, busybox) stay tags.

### One init, Docker's

**Issue.** The first image had `ENTRYPOINT ["/usr/bin/tini", "--"]`, and
the sandbox crate's `container_config` sets `init: true` and the command
`sleep infinity` without an entrypoint. A session container therefore ran
Docker's init, which ran tini, which ran `sleep`.

**Solution.** The crate's configuration is what production runs, so the
image has no entrypoint and no `tini` package, and keeps `CMD ["sleep",
"infinity"]` for a plain `docker run`. The Compose `sandbox` service sets
`init: true` to match. The image check asserts that the image's entrypoint
is empty and its command `sleep infinity`, and that PID 1 of a container
started with `--init` is `/sbin/docker-init -- sleep infinity`.

### agentd's data directory is created by root

**Issue.** agentd runs as uid 10001, and Docker creates a missing bind
mount source on the host owned by root, so agentd couldn't write its
database.

**Solution.** A one-shot `data-init` service (busybox) gives the directory
to `10001:10001` with mode 0700 before agentd starts, through
`depends_on` with `service_completed_successfully`. Only the top directory
is changed; agentd owns what it creates inside.

### Sandboxes on one network reach each other

**Issue.** The first stack left inter-container traffic on for the
`sandbox` network, and a reviewer showed a container on it connecting to a
listener in another container there. Every sandbox shares that network, so
a channel sandbox driven by any member's prompt could reach a private
task's sandbox, or anything a session listens on. Turning inter-container
traffic off (`com.docker.network.bridge.enable_icc: "false"`) stops that,
but also stops sandboxes reaching agentd, which is on the same bridge.

**Solution.** Both: `compose.yaml` turns inter-container traffic off and
fixes the bridge's name (`com.docker.network.bridge.name: br-agent-sbx`),
and `deploy/compose/isolate-sandbox.sh` adds a chain,
`AGENT-CORE-SANDBOX`, jumped to from `DOCKER-USER` for traffic in and out
of that bridge. It accepts new and established TCP connections to
`172.30.0.2` on 8080 and 8081 and agentd's replies, and drops the rest.
Docker evaluates `DOCKER-USER` before its own rules and never rewrites
it, and an accept there skips Docker's inter-container drop. Without the
rules the stack fails closed: sessions can't reach the credential proxy,
rather than reaching each other. The chain is rebuilt on each run and the
jump added once, so the script is idempotent, and `remove` takes both out.
Bridged traffic only passes through iptables with `br_netfilter`
(`net.bridge.bridge-nf-call-iptables=1`), which Docker turns on for a
network with inter-container traffic off. The rules name the bridge, so
they can go in before the network exists, but not survive a reboot.

On Docker 29.3.1 with busybox stand-ins on a network built like `sandbox`:
with inter-container traffic on and no rules a peer's listener answered;
off and no rules, agentd's 8080 and 8081 didn't either; off with the rules,
8080 and 8081 answered while agentd's 8443, the peer, and agentd
connecting out to the peer didn't; on with the rules, the peer didn't
answer either. `compose-test.sh`, run locally with stand-in images for
agentd and the sandbox and the real Rocket.Chat and MongoDB, passed with
the rules, and with inter-container traffic on and no rules failed exactly
the new peer check. In CI (Docker 28.0.4 on ubuntu-24.04) the script
adds the rules with `sudo` before the test and removes them after it, and
the real stack passes. The plan's network section makes the same
isolation a requirement for every deployment.

### The sandbox bridge keeps an IPv6 link-local address

**Issue.** The first test bound its host listener to IPv4 and checked
nothing about IPv6. With `enable_ipv6: false`, Docker 28.0.4 on the CI
runner still left the kernel's link-local address (`fe80::/64`, scope
link) on the sandbox bridge, so "the host has no address on the network"
holds for IPv4 only. No Compose or driver option removes it, and a
host-wide sysctl to stop it would reach every interface.

**Solution.** Docker turns IPv6 off on a container's interface on a
network without IPv6 (`disable_ipv6` is 1 on `eth0`), and `/proc/sys` is
read-only in a container without capabilities, so a sandbox has no IPv6
address to reach the bridge's link-local one from. The test checks both
sides: in a sandbox, `disable_ipv6` is 1 on `eth0` and
`/proc/net/if_inet6` lists only `lo`; on the host, `ip addr` in the host's
namespace shows no IPv4 address on the bridge and no IPv6 address but the
link-local one. The host listener binds `::`, dual-stack, where the host
has IPv6.

### What the network test checks, and how

**Issue.** Most "unreachable" checks pass trivially: a name on another
network doesn't resolve from `sandbox`, and a stopped service refuses
everyone.

**Solution.** The test probes by address as well as by name, with bash's
`/dev/tcp` under `timeout` as in T17's test, and runs every unreachable
target once from the `egress` network first as a control. The stack uses
the real images, so Rocket.Chat and MongoDB must be healthy (their Compose
health checks) and agentd must answer `/healthz` before the probes run.
The distroless agentd image has no shell or client for a health check, so
the test polls `/healthz` with curl from a container on `egress`. The
cloud metadata address `169.254.169.254` is checked as well, since the
design blocks it.

Three targets have no control, because nothing answers there by
construction: agentd's sandbox address on 8443 (the public listener binds
the egress address only), the sandbox gateway (the host has no address on
the bridge, which the test checks with `ip addr` in the host's namespace),
and the metadata address (runners have no metadata service to show). For
sandbox-to-sandbox traffic a busybox listener starts on `egress`, answers a
probe there, then moves to `sandbox` and must not answer the sandbox
probe. The test overrides the networks' names
(`AGENT_CORE_SANDBOX_NETWORK`, `AGENT_CORE_EGRESS_NETWORK`), which default
to `sandbox` and `egress` as the plan and the sandbox crate's default
expect. It sets `RC_ADMIN_PASS` and logs in as that admin through
`/api/v1/login` from a container on `egress`, as README.md's step 2 does
in a browser.

## T17: sandbox

### Docker can't signal an exec'd process

**Issue.** T20 kills a turn's process on timeout, but Docker's API has no
way to signal a process started with `exec`: `kill` reaches only a
container's PID 1, and the pid `inspect_exec` reports is in the host's PID
namespace, which agentd, itself in a container, can't see. Killing the
container would force the runner to start a new one for every timeout.

**Solution.** `DockerSandbox::exec` runs argv as
`/bin/sh -c 'echo $$; exec "$@"' sh <argv…>`. The shell prints its pid,
then `exec` replaces it with the command under the same pid, and `"$@"`
passes argv through without shell interpretation. The sandbox strips that
first line from stdout, and `ChildHandle::kill` runs
`sh -c 'kill -s KILL "$1"'` in the container as the same user. The image
needs `/bin/sh` (Debian has it). Only the process is killed; processes it
started are reparented to the container's init and end when the container
stops. If the pid line hasn't arrived within 2 seconds, `kill` returns an
error instead of `Ok`: nothing was signalled, so `wait` could hang, and the
caller must stop the container. `ProcessSandbox` kills the child's whole
process group instead, with rustix's safe `kill_process_group`, as
agentctl does. It used to run the `kill` command, from `Drop` in a
spawned task, which a current-thread runtime whose `block_on` returned,
or any runtime shutting down, dropped unpolled, so the process group
lived on. `kill(2)` returns at once, so `Drop` now sends the signal
itself.

### Agent-writable directories are given to the sandbox user

**Issue.** The sandbox runs as uid 10001, but agentd creates the volume
directories. Created by root, or by another non-root user, they aren't
writable in the container.

**Solution.** `shared/`, `memory/`, each session's `work/`, `claude/`,
`home/` and `tmp/`, and `settings.json` are given to the configured
`uid:gid` when their owner differs, through handles that follow no
symlink (below). That works when agentd
runs as root or as the sandbox user itself, and fails with an error naming
the cause otherwise. T16's agentd image should therefore run as uid 10001
(the plan's T16 says so).

That makes agentd and the agent the same user, so the agent owns its
`sessions/<id>/` too and can rename, replace or remove `work/`, `claude/`
and the rest. What it can't reach is everything above its mounts:
`volumes/` (`0700`), the volume directory and its `sessions/` are mounted
into no sandbox. Host-side code therefore treats everything inside a
session directory as hostile (next entry).

The Docker tests can't use 10001: on the CI runner the test process is not
root, so it can't give directories away. They run the sandbox as the test
process's own uid, which is still not root, and 10001 when the tests run
as root.

### The agent controls what is inside its session directory

**Issue.** agentd writes `claude/settings.json` on the host before every
start, and Docker mounts the skills directory at `claude/skills`. Both are
inside the session's read-write mount, so a previous run of the agent could
have replaced `claude` with a symlink to a host path, and agentd (possibly
root) would write through it.

**Solution.** Nothing on the host follows a symlink inside an
agent-writable tree. Before each start, each of `work/`, `claude/`,
`home/` and `tmp/` that isn't a real directory is removed and created
again, as is `claude/skills` when there are skills to mount there. It is
rewritten on every start, so an agent can't lower `cleanupPeriodDays` and
lose its transcripts.

The plan made this step a public `prepare_session_dirs` on the trait. Run
while the session's container was up, the agent could swap `claude` for a
symlink between its repair and the write, and agentd would write, and
give away, a file wherever the symlink pointed. So the step is
crate-private and runs only in `start`, before the container is created;
`start`'s rustdoc says the session must have no running container, and the
runner never runs two containers of one session.

The steps don't rely on that either. std has no `openat` (`std::fs::Dir`
is unstable), so the sandbox crate adds `rustix`, whose `*at` functions
are safe. The session directory, whose parent no sandbox reaches, is
opened by path; every entry in it is inspected (`statat` without
following), removed (`unlinkat`), created (`mkdirat`) and opened relative
to that handle, with `O_NOFOLLOW | O_DIRECTORY`. `settings.json` and
`claude/skills` are then written relative to the `claude/` handle, not
its path: a new file is created with `O_EXCL | O_NOFOLLOW`, given away
with `fchown`, and renamed over `settings.json` within the same
directory, which replaces a symlink instead of following it. A directory
in its place is renamed aside to a random name first, then removed. So a
`claude` swapped after its repair only means agentd writes into the
directory it repaired.

The agent can also `chmod` what it owns. With `claude` at `555`, or the
session directory at `0`, agentd running as the sandbox user (without
root's `CAP_DAC_OVERRIDE`) failed every later start with `EACCES`, so the
agent could break its own session for good. The session directory,
`shared/`, `memory/` and each directory repaired above therefore get mode
`0755` again, after their owner. A directory at mode `0` can't be opened
for reading, so the handles are opened with `O_PATH`, which needs no
permission on the directory itself. Linux has no `fchmod` on an
`O_PATH` handle (`fchmodat2` with `AT_EMPTY_PATH` needs Linux 6.6), so
modes are set through `/proc/self/fd/<handle>`, which the kernel
resolves to the handle's directory, not to what its path names now;
owners are changed, and the aside directory removed, the same way. agentd therefore needs
`/proc`, which every container has. `ProcessSandbox` links `claude/skills`
through the same handle.

### Several agentd, or test runs, on one Docker host

**Issue.** The plan's `reap_orphans` stops every container labeled
`agentd.session`, and `list_managed` and `events` filter on the same label.
Two agentd on one host, or a Docker test running next to another, would
stop each other's sandboxes.

**Solution.** Every container also gets `agentd.instance=<name>`, from
`[sandbox] instance` (default `agentd`), and listing, reaping and events
select on both labels. Each Docker test uses its own instance name and its
own internal network. The plan's T17 bullet says so.

### agentd's paths aren't the Docker daemon's

**Issue.** Bind mount sources are paths on the Docker host. agentd runs in
a container (plan: Network and deployment shape), where its data directory
may be mounted somewhere else than on the host.

**Solution.** `[sandbox] host_data_dir` names the data directory as the
daemon sees it. Every mount source under agentd's data directory (the
volume, the persona and skills directories) is rewritten to that prefix,
and one outside it is refused while the key is set. Unset, paths are used
as they are, which fits a data directory mounted at the same path. Docker
refuses a bind mount whose source doesn't exist, so a wrong setting fails
at start instead of mounting an empty directory.

### What the sandbox refuses in a `SessionSpec`

**Issue.** The plan leaves open what a spec may carry.

**Solution.** `container_config` and `ProcessSandbox::start` refuse a spec
that sets `HOME` or `TMPDIR` (the sandbox sets them), an environment name
that is empty or holds `=`, NUL anywhere, a label under `agentd.`, a
persona or skills path that isn't absolute or holds `..`, and `memory`
on any volume but the agent's `Private` one, which is the only one with a
`memory/` directory. The container environment is visible to anyone who
can inspect the container, so its rustdoc says it holds no secrets; the
placeholder, the agentctl token and the proxy variables go to `exec`.
Errors from `exec` requests never carry Docker's message, since the request
held that environment. `SessionSpec::new` gives the least access:
`shared/` read-only and no `memory/`.

### The volumes row records a relative path

**Issue.** The plan's `volumes.path` doesn't say relative to what.

**Solution.** It is `volumes/<agent id>/<digest>`, relative to the data
directory, so moving the data directory doesn't make every row wrong. The
path column is unique, and `Store::volume_by_path` answers which key a
directory holds. Recording a key again keeps its `created_at`.

### bollard's API version and Docker on the runner

**Issue.** bollard 0.21 sends API version 1.53 by default. An older daemon
refuses a client version it doesn't know.

**Solution.** `DockerSandbox::connect` calls `negotiate_version`, which
drops to the daemon's version. bollard's 2-minute request timeout covers
only the response headers, so long `exec` and event streams aren't cut.
A stop request's headers, though, come only after the container stopped,
up to `stop_timeout_secs` later, and a stop that timed out would leave the
container stopping and not removed. `stop_timeout_secs` is therefore at
most 60, not the 300 first allowed. That grace applies only to the init
and its `sleep`, the processes SIGTERM reaches; processes started with
`exec` are killed without one when the container stops.

### No curl in `debian:stable-slim`

**Issue.** The plan's test runs `curl https://example.com`, which in
`debian:stable-slim` fails only because curl isn't installed.

**Solution.** The test opens a TCP connection with bash's `/dev/tcp`, by
name (`example.com:443`) and by address (`1.1.1.1:443`), under `timeout`.
As a control, the same probe must succeed from a container on a network
the test creates without `internal`, so the test can't pass because the
probe itself is broken. The control used to run on Docker's `bridge`
network, but `[sandbox] network` now refuses `bridge`, along with `host`,
`none`, `default` and anything with a `:` (`container:<id>`): those are
Docker network modes, not the internal sandbox network, and `validate`
had only checked that the name wasn't empty. `container_config`, which is
public, validates the configuration too. The control's network isn't
internal, so its sandbox opts in (next entry).

### `[sandbox] network` by ID, or not internal

**Issue.** Docker finds a network by name, by ID and by ID prefix, so
`validate` refusing the name `bridge` didn't keep out the default
bridge's ID, which attaches the sandbox with a route out. Nothing checked
that the network was `internal` at all, and with an ID, `ip` (which looks
the container's address up under the configured name) returned
`NoAddress`.

**Solution.** `DockerSandbox::start` inspects the network before it
touches the disk and refuses, with `SandboxError::Config` for the
`network` key, unless Docker's name for it equals the configured value
and it is `internal`. It does so on every start, so a network that was
recreated without `internal` is caught too, for one request per start.
The Docker tests' control, which needs a route out, calls
`DockerSandbox::allowing_an_open_network_for_tests`, which skips only the
`internal` check; agentd never calls it and no configuration key reaches
it. A Docker test checks that the bridge's ID and ID prefix, an internal
network's ID, an open network and a missing one are all refused.

### A `ChildStdin` closes only when dropped

**Issue.** Shutting down a Docker exec's stdin half-closes the connection,
and the process sees end of input. A tokio `ChildStdin` ignores
`shutdown`: the pipe closes only when it is dropped, so a runner that
shut stdin down would hang `fake-claude` under `ProcessSandbox`.

**Solution.** `ProcessSandbox` wraps the pipe so that `shutdown` drops it,
and both sandboxes document that shutting stdin down closes the stream.

### bollard's event stream starts when it is first polled

**Issue.** `Docker::events` returns a stream that sends its request only
when first polled. A container that died between `Sandbox::events` and the
first poll would be missed, and the runner would keep a mapping for a dead
container's IP.

**Solution.** `DockerSandbox::events` passes `since` with the time of the
call, and Docker replays the buffered events from then. The stream ends
only after one `EventsMissed` item, so the runner re-subscribes and
compares `list_managed` with what it holds. That item comes on an error,
and also when Docker ends the stream cleanly, as a daemon restart does:
that used to end the stream with no item, which the runner could take for
a quiet stream and never re-subscribe. `ProcessSandbox` keeps the same
contract: it ends after `EventsMissed` when it lags or its sender is gone.

### bollard logs request bodies at debug level

**Issue.** bollard 0.21 logs every request body with `log::debug!`
(`serialize_payload`, which `create_exec` uses), and agentd forwards
`log` records to `tracing`. An `exec` body holds the process's
environment: the placeholder and the agentctl token. With
`server.log_filter` at `debug` or `trace`, both would be logged. A
`bollard=info` directive appended to the operator's filter isn't enough:
a more specific one such as `bollard::docker=trace`, or a span filter
such as `[turn]=trace`, outranks it.

**Solution.** `telemetry::subscriber` adds a separate `Targets` filter,
layered next to the operator's `EnvFilter`, that caps `bollard` at `info`
whatever that filter enables; a test feeds `log` records with target
`bollard::docker` through the bridge under filters from `trace` to
`bollard::docker=trace` and finds none. `Sandbox::exec`'s rustdoc says the
environment is kept out of logs only with that cap, and the plan's T23
says any other subscriber setup must keep it.

## T18: credential proxy

### Placeholders are looked up by digest and handled by id

**Issue.** The plan asks for a timing-safe placeholder check, and gives
`point(placeholder, …)` and `revoke(placeholder)`, which would have T23's
turn hooks keep the secret text around only to name the placeholder again.

**Solution.** The registry is a map keyed by `PlaceholderId`, the SHA-256 of
the placeholder's text. A presented token is hashed and looked up, so no
stored token is ever compared byte by byte with a guess, and lookup time says
nothing about how close the guess was. `point` and `revoke` take the
`PlaceholderId`, which is `Copy` and not secret; `Placeholder` itself isn't
`Clone`, redacts its text from `Debug`, and exposes it only through
`ExposeSecret`, with `env_var()` naming `CLAUDE_CODE_OAUTH_TOKEN` or
`ANTHROPIC_API_KEY` so the runner sets exactly one. `point` refuses a
credential of the other kind, so a placeholder can never be pointed across
kinds, whatever the caller does.

### An address belongs to one session

**Issue.** The plan binds a placeholder to its container's address and
relies on revocation before stop and on death, because Docker can give a dead
container's address to a new one. It doesn't say what the registry does if a
revocation is missed and a new session's container arrives at an address
that still has a live placeholder.

**Solution.** `mint` revokes every placeholder of another session bound to
the same address, and logs it (session and address only). The newest
container at an address owns it, so a stale mapping never outlives the next
mint there. Placeholders of the same session at that address are kept:
they are the session's own, and T21 revokes them itself when it restarts the
process. Tests that
need two sessions use two loopback addresses: the second client binds
`127.0.0.2`, which works on Linux, where all of `127.0.0.0/8` is loopback,
but not on a default macOS setup.

### What the proxy refuses, and how

**Issue.** The plan lists what the proxy must reject but not the answers,
and the answers reach the CLI, which reports `api_error_status` (T20 and T23
classify on it).

**Solution.** Every refusal is an Anthropic-style JSON error
(`{"type":"error","error":{"type":…,"message":…}}`) whose message is fixed
text, so nothing the client sent is echoed; a test checks the body and
headers of each refusal against the token presented. In order of checking:

| Case | Status |
| --- | --- |
| No `ConnectInfo` on the request (a wiring bug) | 500 |
| Peer address with no live placeholder, `HEAD /api/hello` included | 403 |
| Any method but `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` and `OPTIONS` (`TRACE`, `TRACK`, `CONNECT` until T19, extension methods) | 405, with `Allow` |
| Absolute-form target on HTTP/1 | 403 |
| `HEAD /api/hello` from a known address | 200, answered locally |
| No credential header, or `Authorization` that isn't `Bearer` | 401 |
| Both `Authorization` and `x-api-key`, or either one repeated | 400 |
| Unknown or revoked placeholder, or one bound to another address | 401 |
| Placeholder in the header of the other kind | 401 |
| Placeholder not pointed at a credential: before its first turn or between turns | 403 |
| `NotLinked` or `RelinkRequired` from `TokenSource`, or no community key | 401 |
| Any other credential error | 503 |
| Upstream unreachable | 502 |

A placeholder bound to another address gets exactly the answer of an unknown
one, so the answer doesn't confirm that a stolen token exists; the log line
tells them apart. An end-to-end test shows `fake-claude` reporting
`api_error_status: 401` for a revoked placeholder.

The method check is an allowlist, not a refusal of `CONNECT` alone: a
`TRACE` forwarded with the swapped header would have an echoing upstream
send the real credential back into the sandbox, and an extension method
means nothing the CLI needs. It runs before any credential lookup.

HTTP/2 requests always carry `:authority`, so the absolute-form check applies
to HTTP/1 only; for HTTP/2 the authority is ignored like `Host`. Every
upstream URL is built from the configured base and the request's path and
query, and must keep the base's origin and path prefix after the URL parser
resolves dot segments, or the request gets 400.

### Credentials are read once, and revocation is checked again

**Issue.** The plan requires a request in flight to keep the credential it
started with, but a request waits on `TokenSource` (a refresh can take
seconds), and the placeholder can be re-pointed or revoked meanwhile.

**Solution.** The pointer is read once, under the registry's lock, when the
request is authorized; re-pointing afterwards changes only later requests. A
test holds the token lookup, re-points, and sees the first member's token
upstream and the second member's on the next request. After the lookup the
proxy checks that the placeholder is still live and bound to the peer, and
refuses with 401 if it was revoked meanwhile, since revocation means the
container is going away. A request already forwarded is not cut off by
revocation; stopping the container ends it.

`unpoint` clears the pointer at turn end (T21's `turn_finished`, on every
exit), so between turns every request is refused with 403 "No turn is
running for this placeholder." Like re-pointing, it changes only later
requests: a request authorized before the turn ended keeps its credential,
through its token lookup and its whole response. That is a choice. Checking
the pointer again after the token lookup, as liveness is, would also refuse
a request that arrived during the turn and was still waiting on a refresh
when it ended; since `turn_finished` runs once the turn's result is in, such
a request is the model's own last call or a leftover process's, and the
window is one token lookup long.

`unpoint` returns whether the placeholder was live, like `revoke`, rather
than an error for a revoked one. A container that dies mid-turn has its
placeholder revoked by `process_stopping` before the turn's `turn_finished`
runs, so a revoked placeholder is a normal case at turn end, with nothing
left to clear.

What remains: a background process left from turn N can still spend turn
N+1's credential while N+1 runs, whoever its requester is. Only killing the
processes a turn leaves behind when it ends removes that; the plan's
Deferred work has an entry.

### Headers the proxy changes besides the credential

**Issue.** The plan says every other header passes untouched, but some
can't, and reqwest adds one.

**Solution.**

- Hop-by-hop headers (`Connection` and every header it names, `Keep-Alive`,
  `Proxy-Connection`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`,
  `Trailer`, `Transfer-Encoding`, `Upgrade`) are dropped both ways. The
  credential header is set after that, so naming it in `Connection` doesn't
  remove it.
- `Host` is dropped and reqwest sends the upstream's. `Expect` is dropped,
  since the proxy's own server already answered `100 Continue`.
- A request that came with `Transfer-Encoding` loses any `Content-Length`,
  so the upstream never sees conflicting framing. hyper's server already
  drops it; the check keeps the proxy safe under any server.
- reqwest adds `Accept: */*` when the client sent no `Accept`, from its
  builder's default headers, which can't be removed. The CLI sends its own
  `Accept`, so nothing changes for it.
- The client follows no redirects (a 3xx goes back to the CLI as it came,
  and `x-api-key` would otherwise follow it to any host), decompresses
  nothing (`no_gzip` and friends, in case a feature elsewhere in the
  workspace enables them), and honors the system proxy settings like `auth`'s
  client does.

### Streaming needs reqwest's `stream` feature

**Issue.** reqwest's `Body::wrap` takes any `http_body::Body`, but only one
that is `Sync`, which axum's request body isn't.

**Solution.** `cred-proxy` enables reqwest's `stream` feature and sends the
request body with `Body::wrap_stream(body.into_data_stream())`, frame by
frame. The feature adds no crate to the lockfile (`futures-util` and
`tokio-util` were already there). A request whose body is already at its end
is sent without one, so a `GET` doesn't become a chunked request. The
response goes back through reqwest's `http::Response<reqwest::Body>`
conversion, frame by frame too. Tests hold the upstream's SSE stream after
its first event and read that event through the proxy, and hold the client's
request body after its first chunk until the upstream has read it.

### The observer also gets the credential

**Issue.** The plan's `ProxyObserver` gets `(session, status, usage
headers)`. The session's pointer changes from turn to turn, so T27's meter
couldn't tell from the session alone whose request it was.

**Solution.** `observe` takes an `Observation` with the session, the
`CredentialRef` the request used, the status and the usage headers (every
`anthropic-ratelimit-*` header and `retry-after`), and is marked
`#[non_exhaustive]` so fields can be added. It is called once per forwarded
request when the upstream's response head arrives, before the body streams;
refusals and unreachable upstreams aren't observed.

### The proxy forwards any path

**Issue.** A placeholder lets the sandbox use its requester's token, or the
community key, on any path of the upstream, not only the Messages API. With
the community key that includes the Files and Batches APIs, which all
members' turns on that key share.

**Solution.** Not restricted in T18: which paths the real CLI needs isn't
known without a live capture, and refusing one it needs would break turns.
The plan's Deferred work has an entry for a path allowlist. Methods are
limited already (see the refusal table).

## T19: Credential proxy egress allowlist

### The target comes from the request line, in one spelling

**Issue.** A `CONNECT` names its target twice, in the request line and in
`Host`, and the plan's rules (`api.anthropic.com` always denied, exact and
`*.suffix` hosts) compare names. `API.Anthropic.COM.:443` is the same host
as `api.anthropic.com:443` to every resolver, and forms such as `127.1`,
`2130706433` or `0x7f.1` are addresses to `getaddrinfo` though they parse
as no `IpAddr`.

**Solution.** Only the request line counts, and it must be authority form,
`host:port`, over HTTP/1: `Host` is ignored, and user info, a scheme, a
path, a missing or zero port, and `CONNECT` over HTTP/2 are refused. The
host is lowercased and loses one trailing dot before any comparison, and
must be a DNS name of letters, digits and `-` with at least two labels, the
last starting with a letter. That refuses every numeric form, and IP
literals (bracketed IPv6 or dotted IPv4) are refused outright: a tunnel
always goes to a named host that a rule allows. Rules go through the same
function, so a rule and a request can't disagree on spelling.

### Addresses are checked after resolution, and the tunnel goes to them

**Issue.** The plan checks denial after DNS resolution so a rebind can't
reach a denied address. Checking the name's addresses and then connecting
by name would resolve twice, and a rebinding server answers the second
lookup differently.

**Solution.** The egress proxy resolves once, refuses the host if any
address in the answer is unreachable (a mixed answer is what a rebinding
attack looks like, and no legitimate public host answers with a metadata
or private address), and then connects to those checked addresses, in
order, never to the name. The system resolver is given the name with a
trailing dot, so search domains don't apply: under Kubernetes' `ndots:5`,
`github.com` would be tried as `github.com.<namespace>.svc.cluster.local`
first. Resolution has a 5-second timeout and all connection attempts share
the 10-second `CONNECT_TIMEOUT`, so a long answer of silent addresses can't
hold a request.

Never reachable, whatever resolves there and whichever rule allowed the
host: agentd's own listener addresses and the sandbox subnet (agentd passes
them from its configuration), the private ranges (`10.0.0.0/8`,
`172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`), loopback, link-local
(`169.254.0.0/16`, the metadata address among them), the other clouds'
metadata and platform addresses (AWS's `fd00:ec2::254`, GCP's
`fd20:ce::254`, Oracle's `fd00:c1::a9fe:a9fe` and Azure's WireServer
`168.63.129.16`, the last in public address space), `0.0.0.0/8`,
`100.64.0.0/10` (Alibaba's metadata address `100.100.100.200` is in it),
`192.0.0.0/24` (Oracle's `192.0.0.192`), documentation, benchmarking,
multicast, reserved and broadcast ranges. For
IPv6 only global unicast (`2000::/3`) is reachable, minus `2001::/23`
(Teredo among it), `2002::/16` (6to4), `2001:db8::/32` and `3fff::/20`:
that also refuses IPv4-compatible, NAT64 (`64:ff9b::/96`) and other forms
that embed an IPv4 address, which would otherwise carry a private or
metadata address past the IPv4 checks. IPv4-mapped addresses are checked as
the IPv4 address they hold.

### `Cidr` moved to core-types

**Issue.** The egress policy needs subnets for agentd's own networks, and
agentd's `net::Cidr` was the only implementation. cred-proxy can't depend
on agentd.

**Solution.** `Cidr` moved unchanged, with its tests, to
`core_types::net`, which does no I/O. agentd imports it from there.

### The CLI honors `NO_PROXY`, and needs it

**Issue.** With `HTTP_PROXY` set, a client sends plain-HTTP requests to the
proxy in absolute form, and the proxy refuses those with 403. If the CLI
sent its `ANTHROPIC_BASE_URL` traffic that way, every turn would fail.

**Solution.** `cred_proxy::EGRESS_ENV` sets `NO_PROXY` to
`cred-proxy.internal,agentctl.internal`, as the plan says, and every
variable in both cases: curl, and so git, reads only the lowercase
`http_proxy`, and tools differ on the others. Against two local capture
servers, the npm build of Claude Code 2.1.285 (the native build wasn't
available here) sent `POST /v1/messages?beta=true` straight to the base URL
when `NO_PROXY` named its host, and `POST http://localhost:…/v1/messages`
to the proxy when it didn't. T23's live check should confirm it on the
native build in the sandbox image.

### Tunnels leave agentd's drain

**Issue.** hyper hands an upgraded connection to the handler and stops
tracking it, so a tunnel isn't part of agentd's graceful shutdown or its
drain timeout, and nothing else would end one that stays open.

**Solution.** A tunnel ends when either side closes, after 5 minutes with
no byte in either direction, after an hour in any case, when its session
has no live placeholder left, or when the `EgressProxy` is dropped, which
happens once the listener and its connections are gone. Tunnel tasks hold
only `watch` receivers, not the proxy.

The session's receiver comes from the `Registry`, which drops the sender
once the session's last placeholder is revoked, however that happens:
`revoke`, `revoke_session`, or a mint for another session at the same
address. T23's `process_stopping` already calls `revoke_session`, so a
stopping container's tunnels close with no other call; an explicit
`EgressProxy::close_session` would have needed T23 to keep a handle on a
proxy that `CredProxy` owns. The receiver is taken with the session lookup,
under the registry's lock, and checked again before the 200, so a session
revoked while its `CONNECT` was being checked gets no tunnel.

### Tunnels and lookups are capped

**Issue.** Nothing bounded what one sandbox could hold: a review probe kept
300 tunnels open from one session. Host lookups were worse. The system
resolver (`getaddrinfo`, through `tokio::net::lookup_host`) blocks a thread
of tokio's blocking pool, and the 5-second timeout only dropped the future:
the lookup kept its thread, so a resolver that hangs let a sandbox fill the
pool with lookups nobody waited for.

**Solution.** `EgressLimits`, given to `EgressProxy::with_limits`:

- A tunnel place is taken per `CONNECT` before the extension is asked or
  the host looked up, and given back when its tunnel closes, or when the
  `CONNECT` is refused and any lookup it started has returned: 32 per
  session (429 past it) and 256 in all (503). agentd's
  `[proxy] max_session_tunnels` and `max_tunnels` set them.
- Lookups run under a semaphore of 32 places, and one session's under a
  semaphore of its own with a quarter of that (at least one place). Each
  runs in a task of its own that holds both places and the `CONNECT`'s
  tunnel place until the resolver returns, even after the timeout refused
  the `CONNECT`, and hands the tunnel place back when the lookup finishes
  in time. A second review found that with the tunnel place dropped at
  the 502 and a session allowed all 32 lookup places, a sandbox capped at
  one tunnel held three lookup places, and a session capped at 32 could
  hold them all and leave every other session with 503: now hung lookups
  count against their own session's tunnels, and no session can hold more
  than its share. Waiting for places counts toward the 5-second timeout;
  a `CONNECT` that gets no session place is refused with 429, and one
  that gets no proxy place with 503. The workspace has no asynchronous
  resolver, and the semaphores need none.
- The `EgressExtension` gets 2 seconds; past that the `CONNECT` is refused
  with 503, since a lookup that can't answer denies.
- A tunnel lives an hour at most, busy or not.

### What the allowlist doesn't stop

**Issue.** A byte tunnel can't see what goes through it.

**Solution.** Recorded, not solved: a tunnel to an allowed host that shares
a CDN front with other sites can reach them by SNI or `Host` inside TLS
(domain fronting), so an allowlist entry is only as narrow as its host's
front; a wildcard over names anyone can register (`*.ngrok.io`) lets a
sandbox pick any public address; and egress is gated on a live placeholder
at the source address, not on a running turn, so a process left from an
earlier turn can use the allowlist between turns (the plan's deferred
"Killing leftover processes" entry covers that).

### Testing without the network

**Issue.** Every address a real test server has is loopback, which the
policy never reaches, and tests don't touch the network. The log test
also missed events under a scoped subscriber.

**Solution.** `EgressProxy::with_network` takes a `Network` that resolves
and connects; the tests' fake answers with public-looking addresses and
connects them to local echo servers, and records what was resolved and
dialed. The log test installs a global subscriber for its test binary and
filters by its own source addresses: with `tracing::subscriber::set_default`,
callsites first hit by other tests' threads, which have no subscriber,
kept their "never" interest and dropped the test's events.

The Docker test serves the proxy on an internal network's gateway address,
which the host holds on the bridge, maps `cred-proxy.internal` to it with
`extra_hosts`, and runs `alpine/git:2.54.0` with `EGRESS_ENV`: cloning
`github.com/octocat/Hello-World` works, a GitLab clone gets
`403 from proxy after CONNECT`, and a clone without the proxy fails. It
needs a route to github.com, which the CI runner has, and it passes in CI's
Docker tests job. Where outbound TLS is intercepted, as in the environment
it was written in, it passes only with the intercepting proxy's CA mounted
into the container.

## T20: runner process driver

### The placeholder is not an environment entry

**Issue.** The plan passes the placeholder in `LaunchSpec.env`, next to
`AGENTCTL_TOKEN` and the proxy variables, and also says the runner sets
exactly one of `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY`. With the
placeholder as a map entry under a name the caller picks, the runner can't
guarantee that, and nothing would stop a caller from passing a real key,
or `ANTHROPIC_AUTH_TOKEN` (which the CLI also sends as a bearer token), or
its own `ANTHROPIC_BASE_URL` that bypasses the proxy.

**Solution.** `LaunchSpec` has `credential: CredentialKind` and
`placeholder: SecretString`, and the runner puts the placeholder in the one
variable the kind uses. `LaunchSpec.env` is refused if it sets any variable
the runner sets (`HOME`, `TMPDIR`, the design's credential proxy block) or
any `ANTHROPIC_*` or `CLAUDE_CODE_OAUTH_*` variable. The placeholder must be
non-empty printable ASCII. `LaunchSpec`'s `Debug` shows the environment's
names only, and its values are `SecretString`s, as the plan's secrets rule
asks for the agentctl token: they are exposed only in the map built for
`Sandbox::exec`, which is dropped once the process has started.
`ClaudeProcess::start` also takes a `ProcessConfig` (the
`claude` binary, `ANTHROPIC_BASE_URL`, the turn timeout), which the plan's
three-argument signature had no place for. The plan's T20 bullet says so.

### When a session has started

**Issue.** The plan uses `--session-id` for a session that "has never
started" and `--resume` otherwise, and T04 found that the transcript
appears with the first user message. The caller can't look for the
transcript itself: `Container::paths()` are the paths the CLI sees, which
under Docker aren't agentd's. And a turn can end without anyone knowing
whether the CLI read the message: a crash before it read stdin leaves no
transcript, a crash after it leaves one. Guessing wrong either way fails the
next start: `--session-id` on an existing transcript exits 1 without a
result, and `--resume` without one prints an `error_during_execution`
result and exits.

**Solution.** `TurnStats::init_seen` records whether the CLI printed its
`system`/`init` line for the turn, which it does once it has read the
message. The plan's T21 now marks a session started after a turn with
`init_seen`, whatever the outcome. The two refusals are tested: a
`--session-id` start on a started session gives `Crashed` with exit code 1
and `init_seen` false, and a `--resume` of a session that never started gives
a `Finished` error result with subtype `error_during_execution`.

That refusal also left the session looking unstarted forever: every
`--resume` gives it again, with `init_seen` false. `TurnOutcome::resume_refused()`
names it (an error result with that subtype and no `init` line), the
process is reaped since the CLI exits after it, and the plan's T21 resets
the session to `--session-id` under the same id and runs the turn again.

### A failed write can follow the CLI's answer

**Issue.** For a `--resume` without a transcript the CLI prints its result
and exits without reading stdin. Writing the turn's line then races the
exit: when the write lost, with `EPIPE`, the driver reported a crash and
dropped the result line waiting in the pipe.

**Solution.** A failed write is logged and the driver still reads stdout to
its end, so a result printed before the process went is returned. Review
found the process then still counted as running, and the next turn crashed
on it. Now a result after a failed write, or a resume refusal (where the
write can win the race), reaps the process before `Finished` is returned,
so `is_running()` is false and the next turn gets `NotRunning`.

### A spawn on another thread can hold the pipe open

**Issue.** The test for that, `a_result_after_a_failed_write_ends_the_process`,
failed once in CI's Coverage job with the process still running: the write
to a script that had closed its stdin succeeded. Locally it failed in 2 of
100 runs of the test binary, 5 of 150 with four busy loops on the CPUs, 1 of
150 under `cargo llvm-cov`, and never alone or with `--test-threads=1`. The
tests run in parallel and each spawns processes through `ProcessSandbox`. A
child starts with a copy of every descriptor open in the test process and
holds it until its exec closes it (`O_CLOEXEC`). A child forked on another
thread while this test's spawn had the stdin pipe open, and not scheduled
until after the script had closed its end, was still a reader when the
driver wrote. The same leak failed the test once with `ETXTBSY`: a child
forked while `std::fs::write` had the script open for writing still held it
when the script was exec'd. The driver was right; the test process broke
the precondition.

**Solution.** `ProcessSandbox::exec` holds a process-wide lock around the
spawn. A spawn returns only once its child has exec'd, so with one spawn at
a time no child is left holding another's pipes, or a file written before
the spawn. After the change: 0 failures in 300 runs with the busy loops,
and 0 in 150 under `cargo llvm-cov` with them. testkit's `fake_claude` test
binary spawns `fake-claude` directly, and one test writes a script for it
to run, so it holds its own lock, for the same reason, around its spawns
and that write.

### Codes and tool names can carry text

**Issue.** The first version logged the `type` of a skipped line, and kept
line codes (`subtype`, `terminal_reason`, the assistant line's `error`) and
tool names that matched `[A-Za-z0-9_.:-]`. The captured-log test put a fake
API key (`sk-ant-api03-…`) in a line's `type` and it reached the log: key
formats fit that pattern. Tool names are written by the model, so a
prompt-injected model could choose one.

**Solution.** A skipped line is logged with its length only. Codes are kept
only in the form the CLI's codes take, at most 64 bytes of lowercase ASCII
letters, digits and `_`, and dropped otherwise. A tool name is kept only if
it is one of the six tools the process was given, and as `<other>`
otherwise. The test covers a secret in each of these places, in the
reply, in the user message and in tool output, and checks the placeholder
and `AGENTCTL_TOKEN` too.

### Lines are parsed as values, then read field by field

**Issue.** A malformed line may be logged with its parse error only, but
serde's typed errors quote the value that failed (`invalid type: string
"…"`), so deserializing a line straight into structs would log whatever a
mistyped field held.

**Solution.** Each line is parsed into a `serde_json::Value`, whose errors
are syntax errors that name a position, not content, and fields are read
with `as_str`, `as_u64` and so on, so a field of the wrong type is absent
rather than an error. A result line without `is_error` counts as an error
unless its subtype is `success`. Lines are read with a 16 MiB cap
(`MAX_LINE_BYTES`): a longer line is skipped, and logged with its length, so
a huge tool result can't grow the buffer without bound. Result lines hold
only the final reply and stay far below it. The line buffer is cleared and
shrunk back to 64 KiB after each turn, so a warm process doesn't keep a
large line's capacity, or its bytes, between turns.

### Classifying errors by the assistant line's code

**Issue.** The plan classifies `is_error` results by `api_error_status` and
the text. The captures show the CLI's synthetic `assistant` line for an API
error carries an `error` code too (`authentication_failed` next to the 401,
`server_error` for an unreachable proxy), and `api_error_status` is null
when there was no HTTP answer.

**Solution.** `ErrorKind::classify` takes the status first (429 is
`UsageLimit`, 401 and 403 `Auth`), then the code (`rate_limit` and
`billing_error` are `UsageLimit`, `authentication_failed` is `Auth`), then
phrases in the text ("usage limit", "rate limit", "hit your limit",
"credit balance", "out of credits", "insufficient credit", "quota"). The
code is kept in `TurnStats::api_error`. The phrase list is a guess at the
CLI's messages; only the 401 path was seen live (T04's capture).

### A cancelled turn leaves the stream mid-turn

**Issue.** `send_turn` reads the turn's lines as it goes. If the caller
drops its future (a cancelled request, a `select!`), the process is left in
the middle of a turn, and the next turn would read the old turn's lines,
result included, as its own.

**Solution.** The process records that a turn is in progress, and a
`send_turn` that finds one still in progress kills the process and returns
`RunnerError::NotRunning`, as after a crash, so the caller resumes on a new
process. A timeout ends the turn the same way, with the process killed
before `TimedOut` is returned. The timeout counts from the call, so it
includes writing the message.

### A kill is not an exit

**Issue.** The timeout and cancelled-turn paths said the process was killed
and reaped, but a failed kill was only logged, a wait that didn't return
within the five-second grace was dropped, and the process was marked dead
either way. Under Docker a kill can signal nothing: the sandbox's kill
finds the process by a pid it may never have learned (T17's review fix
makes that an error that says to stop the container instead). A process marked dead might still be writing
the transcript the next process resumes.

**Solution.** The process records whether a wait returned after the kill
(or after stdin closed, for `stop`), and `ClaudeProcess::may_be_alive()`
reports it: false only once the exit was seen. It covers every way a
process ends, crash, timeout, cancelled turn, refused resume and `stop`
(which now takes `&mut self` so the answer can be read afterwards), rather
than a flag on each outcome. The plan's T21 stops the container when a
dead process may still be alive. `ChildHandle` is a concrete type, so the
kill-and-wait logic is written against a private two-method trait, and
unit tests drive it with a double whose kill fails or is ignored.

### `total_cost_usd` is the process's running total

**Issue.** The result line's `total_cost_usd` was passed on as the turn's
cost. T04's `tool-turns.jsonl` shows it is the process's running total:
the first turn reports usage 20/10 and 0.00014, the second usage 10/5 and
0.00021, which is 1.5 times the first, the cost of 30/15. `usage` is the
turn's own. T27 would have billed a member again for every earlier turn
on the process, including other members' turns on a shared warm process.

**Solution.** `ClaudeProcess` keeps the last total it saw, 0 on each new
process, and `TurnResult::cost_usd` is the rise since then, never below 0;
the raw value stays as `process_total_cost_usd`. The fixture test checks
that the second turn costs about 0.00007, and `fake-claude` now reports a
running total (`testkit::claude::REPLY_COST_USD` per reply, a power of two
so the sums are exact) for the integration tests. Whether the real CLI
starts a `--resume`d process's total from 0, as `fake-claude` does, or
restores the session's total, is unverified: no kept capture holds a
result from a `--resume`d process. A restored total would make
the first turn of every resumed process count the session's earlier turns
again. The plan's T23 live check now covers it.

serde_json's default float parser is not correctly rounded (its
`float_roundtrip` feature is off), so the fixture's
`0.00014000000000000001` parses one unit in the last place away from the
literal, and the differences carry such errors too. That stays far below
a cent in T27's daily sums; the fixture test compares with a tolerance.

## T21: runner sessions, queue and warm pool

### The hooks name the process, not only the session

**Issue.** The plan's `process_stopping(session)` and `turn_finished(session,
turn)` name only the session. The pool calls `process_stopping` from the
sandbox's event stream when a container dies, without the session's lock,
since a turn may be running for many minutes. A death reported for a
session's old container could then land after the session had stopped it
and started a new process, and revoke the new process's placeholder and
agentctl token. T23 would also have had to keep a map from session to
token to hand the turn's outbox back.

**Solution.** `TurnHooks` has two associated types. `process_starting`
returns `(ProcessEnv, Self::Process)`, and every later call for that
process gets the `Process` back, so agentd can keep the placeholder's id
and the agentctl token in it and a late call for an old process never
touches a new one. `turn_finished` returns `Self::Finished`, which
`run_turn` hands to the caller in `TurnReport::finished`: T23 returns the
turn's `Outbox` there. `SessionManager` is generic over its hooks rather
than holding a `dyn TurnHooks`.

### A turn cut off before its outcome was recorded

**Issue.** A session is marked started after a turn whose `init_seen` is
true. If agentd dies after the CLI read an unstarted session's first
message but before that was recorded, the next process starts with
`--session-id`, which the CLI refuses because the transcript exists, and
every later turn fails the same way: nothing would ever mark it started.

**Solution.** `sessions` has a `maybe_started` column. It is set in the
store before a turn goes to an unstarted session's CLI, and cleared when
the turn's outcome says what happened (`init_seen`, or a refused
`--resume`). A session with it set starts its next process with
`--resume`; if there is no transcript after all the CLI refuses, and the
plan's rerun with `--session-id` takes over. A `--session-id` start that
crashes without `init_seen` leaves it set too, so a transcript that
existed after all is resumed next time.

### `lookup_or_create` needs the scope

**Issue.** `lookup_or_create(agent, thread_key)` has no scope to record,
and a `ThreadKey` can't give one: the router decides that the owner's DM is
`Private` and another member's is `Dm`. If a thread's session were found
whatever scope it was made on, a DM that stopped being the owner's (the
agent changed hands) would keep running on the agent's `Private` volume.

**Solution.** `lookup_or_create(agent, thread, scope)`. A thread's live
session on another scope is reset and replaced in the same transaction,
and its warm container stopped in the background, so a session never
changes volume.

### `create_private` needs the thread

**Issue.** `create_private(agent, consent)` names no thread, but the row's
thread columns are not null, and T33 posts the task's result to the thread
recorded with the consent, whose table doesn't exist yet.

**Solution.** `create_private(agent, consent, thread)`, and a `consent_id`
column, set exactly for `private` rows. The thread columns of a private
row hold where its result goes; the partial unique index covers normal
rows only, so they never collide with the thread's own session. A private
session takes only `TurnKind::PrivateTask` with its own consent, and a
normal session only `TurnKind::Normal`.

### Idle containers hold places under the caps

**Issue.** With one warm container per active session, idle containers
fill a scope's cap, and a new session would wait up to the idle timeout
for the reaper.

**Solution.** A session that needs a container when a cap is full first
stops an idle one under that cap (a dead one first, then the one idle
longest), and otherwise waits on the cap's semaphore until a place frees
up or a turn ends and a container becomes idle. A container is idle when
its session's lock is free; the pool takes it with `try_lock`, so it never
waits on another session. The scope cap is per volume, `(agent, scope)`,
so two agents in one channel don't share one. The global cap defaults to
32; the Compose sandbox network leaves about 126 addresses.

### Mounts come from the turn's side

**Issue.** A container's mounts are fixed when it starts, and the plan
doesn't say what decides them: `shared/` read-write or read-only, and
whether `memory/` is mounted.

**Solution.** The turn's `Side`, on the agent's `Private` volume: the
owner's side gets `shared/` read-write and `memory/`, the public side (a
private task a non-owner asked for) `shared/` read-only and no `memory/`.
Every other volume mounts `shared/` read-write and no `memory/`. A turn
whose mounts differ from the warm container's stops the container, like a
credential kind or model change stops the process.

### How turns queue and survive their caller

**Issue.** The plan asks for a keyed queue in arrival order, and for
`turn_finished` to finish before the slot is released even when the caller
is cancelled. A task spawned per turn and then queued would queue in
whatever order tokio runs the tasks.

**Solution.** Each session's slot is a tokio mutex, which hands the lock
out in the order it was asked for. `run_turn` asks for it in the caller's
future, as its first await, and once it has it runs the turn in a spawned
task that owns the lock guard, so dropping the caller no longer cancels
anything; the lock is released only after `turn_finished` returns. A
caller that stops waiting while its turn is still queued leaves the queue
and its turn never runs. `reset` and `stop` queue the same way, so a reset
runs after the turns queued before it; turns queued after it fail with
`RunnerError::SessionReset`.

### Hook failures

**Issue.** The plan doesn't say what a failing hook does.

**Solution.** A failed `process_starting` fails the turn and stops the
container, and so does a process that fails to start. A failed
`turn_starting` fails it without sending it, and `turn_finished` is still
called, since the placeholder may have been pointed. A failed
`turn_finished` is logged and returned in `TurnReport::finished`, and the
process is stopped (with `process_stopping`), since the runner can't tell
whether the placeholder is still pointed. A failed `process_stopping` is
logged and the stop goes ahead.

A panic in `process_starting` is taken for its failure, and one in
`process_stopping` is logged and the stop goes ahead: otherwise a panic in
`process_stopping` left the process it was stopping running, taken out of
the session, and the next turn started a second process in the same
container. It would also have ended the event follower or the reaper for
good. A process that panics while starting has `process_stopping` called
and its container stopped, since it may have been started, and the turn
fails with `RunnerError::TurnTask`.

A panic in a turn is caught at the turn: one in `turn_starting` or the send
still has `turn_finished` called, and after any panic, `turn_finished`'s
included, the turn is recorded if it has an outcome and the process is
stopped as after a failed `turn_finished`. Only then does the panic resume,
failing the turn with `RunnerError::TurnTask`. Otherwise the next turn would
reuse a process whose placeholder may still be pointed, and the store would
keep `maybe_started` for a turn whose outcome was known. The slot's release
wakes waiters for an idle container from a drop guard, so a panic doesn't
leave a session waiting on the caps until the idle timeout.

### A container that fails to stop

**Issue.** When `Sandbox::stop` failed, the pool forgot the container and
freed its places under the caps, though it may still have been running with
its process in it. The next turn of the session then started another
container and another process on the same transcript, past the caps, and
nothing tried the stop again.

**Solution.** A container whose stop fails stays its session's, marked
dead, with its places under the caps. A turn that finds it, and `reset`,
try the stop again and fail with `RunnerError::Sandbox` if it still fails,
so no second process resumes the transcript. The reaper tries every dead
container each round, and eviction under a full cap tries it first. A
container's address is read when a process starts in it, after the
container is held, so a container whose address can't be read goes the
same way.

### A normal stop reported as a death

**Issue.** Stopping a container makes the sandbox report it dead, and the
event can arrive while `Sandbox::stop` is still returning. The pool forgot
the container only once the stop returned, so the event follower found it
still tracked and alive, and logged a container death, with a warning, on
nearly every normal stop.

**Solution.** A session marks its container dead before it stops it. The
follower takes a dead container's death as already handled, and a stop that
fails leaves the container marked dead, as before.

### The death-log test missed its own events

**Issue.** `a_normal_stop_is_not_logged_as_a_death` failed once in CI with
no "stopped a session container" line and nothing captured at all. It
captured logs with `tracing::subscriber::set_default`. With that one scoped
subscriber the only dispatcher registered, `tracing-core` works out a
callsite's interest, the first time the callsite is hit, from the dispatcher
of the thread that hits it. Another test of the binary, on its own thread
with no subscriber, that stopped a container first after this test had set
its subscriber registered the callsite with "never", for every thread, and
the test's own event was dropped. The same could turn off "a session
container died" and make the test pass whatever the pool logged. The pool
was right: the test failed only when another test's thread hit the
callsite first.

**Solution.** Every harness installs one global subscriber for the test
binary, once, before any test reaches the pool, and the test reads only the
lines naming its own session, as the egress log test does.

The other log captures, in agentd's sweeper, command and telemetry tests,
runner's log test and cred-proxy's logging and egress tests, had the same
flaw or the same ad hoc fix. Every capture now goes through the shared
`testkit::Logs`: one global subscriber per test binary (`Logs::global`, or
`Logs::install` with the binary's own, as agentd installs its JSON one),
read per test by a field only that test logs (`Logged::matching`) or by a
span it enters on its own thread (`Logs::tag`). agentd's capture formats only
events inside such a span, so tests that capture nothing aren't slowed down.
`Logged::assert_lacks` refuses an empty capture, and every absence check
sits next to a presence check on the line it expects. The telemetry tests
check subscribers themselves, so they still set one per test, through
`Logs::scoped` on the global capture: with the global subscriber registered
first there are always two dispatchers, and `tracing-core` then asks each of
them about a new callsite, whichever thread hits it.

A tag keeps only the lines whose span parents lead back to it: a line
inside a span made before the tag, such as a task's own `instrument` span,
or logged on another thread, such as by `spawn_blocking`, is missing from
it. So an absence check reads `Logged::matching` a unique id or the whole
snapshot, never a tag. `Logs::install` rebuilds the interest cache once
`set_global_default` has installed the subscriber, since `Dispatch::new`
rebuilt it before the global dispatcher was set and a callsite first hit in
between stays off; and it panics when a later call passes a different
`make`, which would otherwise be ignored. agentd's capture is the lib test
binary's global subscriber, so `telemetry::init` fails in a lib test that
reaches it; such a test runs `agentd` as a process, as `tests/binary.rs`
does.

### A refused `--resume` is known only on a resumed process

**Issue.** `TurnOutcome::resume_refused()` recognizes the CLI's refusal by
its shape: an `error_during_execution` error result before `system`/`init`.
Acting on that shape for any process would mark a session unstarted, and
run its turn again, after a `--session-id` start or a warm process's later
turn ended that way for some other reason.

**Solution.** The runner treats it as a refusal only on the first turn sent
to a process started with `SessionStart::Resume`. Any other turn with that
outcome is recorded like any turn without `init_seen`, which leaves
`maybe_started` as it was. The process remembers that nothing was sent to it
yet, and the send clears it: a resumed process whose first turn failed in
`turn_starting` is kept warm, and the refusal comes on the next turn, which
didn't start the process. Judged by whether the turn started the process,
that refusal came back as an error result, the turn didn't run again, and
its message was lost.

### What is durable

**Issue.** Queued and in-flight state must survive a restart if anything
reads it back.

**Solution.** What the runner reads back lives in `sessions`: the ids,
`started`, `maybe_started`, `last_turn_at` and `reset_at`. The queue of
waiting turns belongs to its callers' futures, and the warm pool to
running processes and containers, neither of which survives a restart:
agentd reaps every sandbox at startup (T17) and purges agentctl tokens
(T15), and placeholders live in memory (T18). They stay in memory.

## T22: router

### The plan and the design name no order for the checks

**Issue.** The design's flowchart has only the ignore and credential branches,
and T22 lists the refusals (paused, bans, deny rules, hop cap) without saying
where they fall, or how they rank against gating and linking. A refusal
placed before gating would make a paused or restricted agent answer every
channel message with a notice; a link prompt or community-key turn placed
before a refusal would be offered to someone who is banned.

**Solution.** The two documents don't disagree, so `route` takes the order
that is safe on both counts and documents it in the crate rustdoc: the
agent (unknown, deleted), the sender (unmanaged bot, own message), whose DM
it is, gating, attribution, then paused, banned, allow and deny, hop cap,
and last the credential. Every ignore precedes every refusal and every
refusal precedes the credential. The refusals keep the plan's listing order;
none spends anything, so it only picks the notice. `precedence_*` tests pin
each boundary.

### A DM didn't say whose DM it is

**Issue.** "Or DM" in the gating rule, and "owner in a DM" for the
`Private` scope, assumed the DM is with the agent being routed. Nothing in
`RouterView` could check that. If the pipeline made a mentioned agent a
candidate for the owner's DM with another bot, the manager bot included,
the router would have run it on the owner's side in a conversation that
isn't its own.

**Solution.** `RouterView::binding_agent(BindingId)` names the agent whose
binding received the event. A one-to-one DM has exactly one bot in it and
only that bot's binding receives it, on Slack (`message.im` to that app) and
on Rocket.Chat (one connection per bot, and only the agent's is in the
room). A DM that came in through any other binding is
`Ignore(NotThisAgentsDm)`, even if it mentions the agent. The plan's T22
list gains the query.

### `message_ref` needed the posting agent, and the requester's member may be stale

**Issue.** T22's `message_ref(msg) -> Option<(TurnId, Requester, Hop)>`
couldn't tell which agent agentd posted the message as, so a row recorded
for one agent would attribute another agent's message. The router doesn't
use the turn id. And the requester's `member` is recorded at posting time:
`None` for someone unlinked then, which would let a later ban by member be
sidestepped through a hop.

**Solution.** `message_ref` returns an `Attribution { agent, requester,
hop }`, and the router accepts it only when `agent` is the agent that sent
the message; otherwise the message is unattributed. A recorded member wins,
since it names who was billed; when none was recorded the router resolves
the key with `member_for`. `is_banned` takes the whole `Requester`, so the
view checks the member it names and the member its key belongs to.

### `LinkPrompt` didn't say whom to prompt

**Issue.** For a hop, the event's sender is a bot, so a bare `LinkPrompt`
left the pipeline to guess who should link. That happens when the inherited
requester unlinked, or the community key was cleared, mid-chain.

**Solution.** `Decision::LinkPrompt { requester }`. For a person's message it
is the sender; for a hop it is the inherited requester.

### The owner without a linked account

**Issue.** The flowchart sends the owner straight to "owner credential",
without asking whether the owner still has one. `/agent logout` can leave
an owner unlinked. Falling through to the community key would put the
community's key behind the owner's `Private` side in a DM.

**Solution.** The owner's turns run only on the owner's credential. An
unlinked owner gets a link prompt, in a DM and in a channel, and the
community key is never used for them. `design.md`'s Routing section says so.

### What allow and deny rules mean

**Issue.** T27 says "deny wins, and the default allows everyone". With a
default of allow, an allow rule could never change anything unless a
non-empty allow list restricts. T27 fills the rules, but T22 evaluates
them.

**Solution.** `AgentPolicy::permits`: a requester any deny rule covers is
refused. Otherwise an empty allow list allows everyone, and a non-empty one
allows only requesters one of its rules covers, by member or by room (see
[below](#member-rules-matched-one-surface-only)).
Rules apply to the requester, so a hop is checked against the inherited
requester. The owner is exempt, so `deny everyone` can't lock the owner out
of their own agent. `AgentPolicy` also carries `max_hops`, since T27's
per-agent `hops` limit lives in the same row; the view returns the effective
cap, the global one lowered by the agent's. Until T27, the default is
`DEFAULT_MAX_HOPS`, 3.

### Surface flags aren't trusted for managed agents

**Issue.** The router asked for the managed agent only when
`sender_is_bot` was true, per T22. A surface that failed to flag a managed
agent's post as a bot would have had it routed as a person's message:
billed to the bot's own key, most likely on the community key, and able to
loop.

**Solution.** The router asks `managed_bot(sender)` for every sender, and
a managed agent's post takes the agent path whatever the flags say. A sender
flagged as a bot whose `sender_bot_user` isn't `sender.user` (a Slack bot
known only by its bot id, or a surface bug) is an unmanaged bot without any
lookup, so a bot id never matches a binding even by accident. A
`sender_bot_user` alone marks the sender as a bot.

### A reply naming another agent ran two turns

**Issue.** A person's reply in agent A's thread counted as addressed to A
whatever it mentioned. A reply there that mentioned only agent B ran B, as
the mention asked, and also A, as a reply to A, so the person paid for two
turns and got an answer they didn't ask A for.

**Solution.** A reply to an agent counts only if it mentions no other
managed agent. Mentioning nobody, A itself, the manager bot or a person
still counts; mentioning B and not A is addressed to B alone. Mentioning
both runs both, since both were named. A DM still counts for the agent
whose DM it is, whatever it mentions, since no other agent can answer
there. `design.md`'s Routing section says so, and the invariant grid gains
a "mentions B" axis that asserts a message naming only B never engages A
outside A's DM.

### Member rules matched one surface only

**Issue.** `PolicyTarget::Member(MemberKey)` matched the requester's surface
identity only. Bans and the owner check go by `MemberId`, so a member
denied through their Slack identity could still use the agent through
their linked Rocket.Chat identity.

**Solution.** `PolicyTarget::Member { key, member }` holds the identity the
rule named and the member it belonged to when the rule was set.
`AgentPolicy::permits` takes the whole `Requester`: a member rule covers a
requester with the same key, or, when the rule has a member, a requester
with the same member on any surface. Key equality always counts, so a rule
set while the identity had no member still covers that identity after it
joins one. T27 stores the member with the rule.

### The manager bot had no identity in the view

**Issue.** `is_managed_bot` answered only for agents' bot users. A manager
bot post that the surface didn't flag as a bot, and that mentioned an agent,
was routed as a person's message: the manager bot's key has no member, so
it ran on the community key.

**Solution.** The lookup is `managed_bot(key) -> Option<ManagedBot>`, where
`ManagedBot` is `Agent(AgentId)` or `Manager`. The manager bot's posts are
`Ignore(ManagerBot)` whatever the flags say, and a mention of the manager
bot addresses no agent.

### A synchronous view over an asynchronous store failed open

**Issue.** `RouterView` is synchronous so that `route` stays pure, and the
store is asynchronous, so T23 has to load the view's answers before calling
`route`. Anything it forgot fell back permissively: `is_banned` answered
false and a missing policy was `AgentPolicy::default()`, which allows
everyone. A banned or denied requester would have been run.

**Solution.** The lookups that grant or withhold permission fail closed.
`is_banned` returns `Option<bool>` and `policy` returns
`Option<AgentPolicy>`; `None` means the view doesn't know, and `route`
refuses with `RefuseReason::PolicyUnavailable`, after the paused check and
a known ban, and before the credential. The owner is refused too, since
the policy also holds the hop cap. Every other lookup already withholds a
turn when it has no answer. The trait's rustdoc lists every lookup `route`
may make for an event, in order, so T23 knows what to load, and T23's and
T27's plan text say what they fill.

## T23: Turn pipeline end to end

### One row per session and one attribution per post

**Issue.** The plan made `message_refs` unique on `(surface, team_id,
conversation, platform_ref)` and gave every row a per-session short id. A
message then has at most one row, in one session, but an inbound message
mentioning two agents is shown to both agents' sessions, and a private
task's result, posted from the private session, is later shown to the
channel session. Only the first session could have given the model a short
id for it.

**Solution.** A row belongs to its session: `(session_id, short_id)` is the
key, and `(session_id, surface, team_id, conversation, platform_ref)` is
unique, so a message has at most one short id per session and keeps it.
What must be unique across sessions is the attribution, so a partial unique
index covers `(surface, team_id, conversation, platform_ref)` where
`agent_id` is set: agentd records each message it posts once, with the
agent, turn, requester and hop, and `Store::posted_message_ref` reads that
row. A session that is shown a message agentd posted elsewhere records its
own row without `agent_id`. A session shown its own post before recording
it as posted keeps that row and short id, and recording the post gives the
row the agent, turn, requester and hop in the same transaction rather than
returning it unchanged, which would lose the attribution; the partial index
still refuses it when another session's row attributes the message. An
inbound row's requester is the message's sender, with hop 0, so the columns
the plan lists stay required. The next
short id is taken inside the insert, in one `BEGIN IMMEDIATE` transaction;
a test with sixteen concurrent inserts on a file database gets 1 to 16.
`Store::posted_elsewhere` finds the agent's posts in a thread that the
session hasn't recorded yet, which is what the turn message builder (T23b)
shows once and then records.

### `process_stopping` revokes the process, not the session

**Issue.** The plan has `process_stopping` call `Registry::revoke_session`.
T21 gave every hook the process's own `Process` value so that a late call
for an old process, as when Docker reports an old container's death after
the session started a new one, never touches the new one.
`revoke_session` would revoke the new process's placeholder too.

**Solution.** `process_stopping` revokes the process's own placeholder with
`Registry::revoke(PlaceholderId)` and its own agentctl token. A session
runs one process at a time, so this is the session's last placeholder,
and `revoke` then drops the session's watch sender, which closes its
egress tunnels exactly as `revoke_session` would. A test stops an old
process after a new one started and the new placeholder still works. A
second call is harmless: both revocations find nothing.

### The hooks check the turn's side

**Issue.** The hooks took `TurnRequest::side` as given, but the owner's
side, which agentctl's target rules and the private volume's mounts grant
more to, belongs to the agent's private session only (the owner's DM and
the owner's private tasks). A wrong side from the pipeline would have
granted it in a channel.

**Solution.** `turn_starting` refuses `Side::Owner` unless the session's
scope is `ScopeKey::Private`, before it points the placeholder or records
the turn, so the turn fails and its `turn_finished` finds nothing to clear.
`turn_finished` unpoints the placeholder, which can't fail, before it
awaits `Ctl::end_turn`, so the credential stops being reachable first.

### A resumed process restores the session's total cost

**Issue.** T20 left open whether the first result of a `--resume`d process
reports a `total_cost_usd` counted from 0; the runner's per-turn `cost_usd`
assumed so, and T23's live check was to find out.

**Solution.** It doesn't. The native 2.1.285 build in a stand-in for the
sandbox image (Debian with `/opt/claude-code/bin/claude` copied in, since
the real image's download is blocked here), run by
`docker_real_claude_starts` against `fake_anthropic()`, reported 0.0001 for
a turn, and 0.0002 for the same usage on the first turn after the process
was stopped and the session resumed. The CLI appends a line
`{"type":"cost-state","totalCostUSD":…,"modelUsage":{…}}` to the transcript
when a process exits, none while it runs (a warm process's second turn left
none), and on `--resume` restores the total from the last one. A process
that is killed writes none, so the restored total is the session's total as
of its last clean exit.

The runner's `cost_usd` for such a turn therefore holds the restored total
too. Nothing reads it yet, so the runner only says so in its rustdoc, and
the Docker test pins the behavior: it fails if the resumed total stops
being the sum. T27, which meters cost, takes the restored total off, from
the transcript's last `cost-state` line (read without following links, with
its size capped: the transcript is agent-writable, but the CLI restores
from the same line, so the difference is still the turn's cost) or from a
total the runner keeps in `sessions` when a process exits cleanly. The
plan's T27 says so.

### The real `claude` test serves the proxy on the network's gateway

**Issue.** The plan's `docker_real_claude_starts` used a network that isn't
internal and `host-gateway`, so the container could reach the proxy in the
test process. `DockerSandbox` refuses a network that isn't internal unless
told otherwise for tests, and gives the container no `extra_hosts`.

**Solution.** The test creates an internal network without `inhibit_ipv4`,
on whose bridge the host holds the gateway address (T16's note on internal
networks), and serves the proxy there, as T19's Docker test does.
`ANTHROPIC_BASE_URL` names the gateway's address, and `NO_PROXY` names it
too, since the hooks set the egress proxy variables. Nothing else routes
out, so the test also shows that the CLI needs no direct traffic. The CLI
runs through the runner and agentd's hooks, with a community API key
placeholder, and the test asserts the read-only root, the transcript at
`$CLAUDE_CONFIG_DIR/projects/<id>/<id>.jsonl` holding both turns, a
`--resume` start for the second, and that every request the fake saw came
through the proxy with the swapped key. On CI the sandbox runs as the test
process's own uid, which has no entry in the image's `/etc/passwd`, and the
CLI ran there all the same; locally, as root, it ran as 10001.

### The native CLI honors `NO_PROXY`

**Issue.** T19 saw the npm build send `ANTHROPIC_BASE_URL` traffic straight
to the base URL when `NO_PROXY` named it, and left the native build to
T23's live check.

**Solution.** In the same stand-in image, with `HTTP_PROXY` pointing at the
unresolvable `cred-proxy.internal:8080`, the native 2.1.285 build reached
the base URL directly when `NO_PROXY` named its address, and when
`NO_PROXY` named only `cred-proxy.internal,agentctl.internal` it sent the
request to the proxy and retried until the turn timed out. So the proxy
variables reach it, and agentd's sandboxes, whose base URL is
`cred-proxy.internal`, work as T19 expects.

### Sandboxes reach agentd by name, not by configuration

**Issue.** `runner::ProcessConfig` takes `ANTHROPIC_BASE_URL`, and agentctl
reads `AGENTCTL_URL`. As configuration, a base URL whose host isn't in the
egress environment's `NO_PROXY` would send the CLI's API traffic to the
egress proxy, which refuses it.

**Solution.** `[runner]` has only `claude_bin`, `turn_timeout_secs` and the
pool's keys. Processes always get `cred_proxy::PROXY_URL`
(`http://cred-proxy.internal:8080`) and `pipeline::AGENTCTL_URL`
(`http://agentctl.internal:8081`), the names `NO_PROXY` lists, so the
deployment gives agentd those aliases on the sandbox network and keeps
those ports, as the Compose file does; the example configuration and the
README say so. Tests, which serve the listeners on port 0, build
`TurnSettings` themselves with the bound addresses and a `NO_PROXY` naming
them, and name `fake-claude`'s script and `agentctl`'s directory in
`TurnSettings::env`, which is added after the egress variables and holds
no secret. agentd sets nothing there.

With `[sandbox]` set, the ports are therefore not a choice:
`internal.proxy_listen` must use `cred_proxy::PROXY_URL`'s port and
`internal.ctl_listen` `pipeline::AGENTCTL_URL`'s, both read from the
constants, or the configuration is refused naming the key. Any other port,
0 included, would be one sandboxes never try and `isolate-sandbox.sh`
doesn't let through. Tests that bind port 0 have no `[sandbox]`.

### Plain HTTP upstreams only on loopback

**Issue.** `[proxy] upstream` took any `http://` URL, so a mistyped
upstream would send members' real credentials over the network
unencrypted.

**Solution.** `Upstream::parse`, behind both `CredProxy::new` and
`check_upstream`, takes `http` only when the host is a loopback IP address
(IPv4-mapped included), which is where tests' fakes listen, and `https`
otherwise. `localhost` is refused as a name that could resolve anywhere.
`Routers::new` logs a warning, with the upstream, when it isn't
`DEFAULT_UPSTREAM`, so a gateway in front of the API is never silent.

### `[sandbox]` is optional

**Issue.** The plan has agentd build a `DockerSandbox` and reap orphans at
startup, but most tests start agentd without Docker, and an operator may
run agentd for commands alone.

**Solution.** Without `[sandbox]`, `serve` logs a warning and runs no
turns. With it, `serve` connects to the Docker daemon, which must answer,
stops what a previous run of the same `instance` left (`reap_orphans`),
and starts the runner (`pipeline::Turns`) after the listeners are bound.
The example configuration has the section, with the image the Compose file
builds; the Compose README adds `host_data_dir`, which depends on where the
checkout is. `docker_startup_reaps_only_this_instances_sandboxes` plants a
container labeled with the configured instance and a session, and one
without labels, and checks that `connect_docker` removes only the first.

### A process sandbox gives every container one address

**Issue.** `ProcessSandbox::ip` answers `127.0.0.1` for every container,
and minting a placeholder for an address revokes other sessions'
placeholders there (T18). Tests that run turns on two sessions would find
the first session's warm process holding a revoked placeholder.

**Solution.** The tests set `global_container_cap = 1`, so a second
session's turn stops the first session's idle container, and with it its
placeholder, before minting. Docker gives each container its own address,
so production is unaffected.

### agentctl for scripted turns

**Issue.** `fake-claude` runs a script's commands from its `PATH`, and
agentd's tests need `agentctl` there, but cargo sets `CARGO_BIN_EXE_*`
only for a package's own tests.

**Solution.** `testkit::agentctl_path()` builds it the way
`fake_claude_path()` builds `fake-claude`, with the package named, and the
tests put its directory on the script's `PATH`.

### No community key until T26

**Issue.** `CredProxy::new` takes a `CommunityKey`, which T26 implements
over the store.

**Solution.** agentd's proxy uses `pipeline::NoCommunityKey`, which always
answers `NotConfigured`, so a placeholder pointed at the community key gets
401 from the proxy. The router never picks the community key before T26
either, since `community_key_configured` answers false until then.

### Surfaces take reactions back and say where the bot may post

**Issue.** The working indicator on Rocket.Chat is a reaction put up at
turn start and taken off at the end, but `Surface` could only add one.
And a reply there is a `chat.postMessage`, which joins the poster to a
public channel it isn't in, while Rocket.Chat delivers a message once for
every bot in the room, so a mention of an agent that isn't in the room can
arrive through another bot's connection (T14's note). The pipeline had no
way to tell before it ran the turn.

**Solution.** `Surface` gains `unreact` (Rocket.Chat's `chat.react` with
`shouldReact: false`, Slack's `reactions.remove`) and `can_post`. On
Rocket.Chat `can_post` asks `subscriptions.getOne?roomId=` (whose answer
is `{"subscription": null}` for a room the user isn't in, per
`@rocket.chat/rest-typings`) every time. A first version trusted a listing
of `subscriptions.get` for a minute, and a bot removed from a room in that
minute posted there and was added back by the post; asking room by room
also spares the full listing, and finds a DM the manager bot just opened.
`post` and `upload` refuse a
room the bot isn't in with `SurfaceError::Forbidden`, which also gives
agentctl's owner-side posts the refusal T15 left to the surface. Slack
never joins a poster to a conversation and refuses the post itself
(`not_in_channel`), so its `can_post` only checks the workspace. The
pipeline asks `can_post` before it runs a turn, so an agent whose bot isn't
in the room neither answers nor spends a turn. `MockSurface` records
`unreact` and has `keep_out_of` for a conversation the bot isn't in. The
design's trait is updated.

### The pipeline takes `Acknowledge`'s place only with turns

**Issue.** The plan has the pipeline replace T14's `Acknowledge` as every
Rocket.Chat connection's onward sender, but without `[sandbox]` agentd runs
no turns, and T14's tests watch for the `:eyes:` reaction.

**Solution.** `Server::run` passes messages to the pipeline when it has
`Turns` and to `Acknowledge` otherwise, so an agentd without sandboxes
still shows which bot a mention reached. The plan's T14 bullet says so.

### Short ids are `#` and a number

**Issue.** T15 accepts platform message ids, and T23 resolves the short
ids the turn message shows. A bare number would be ambiguous: nothing
stops a platform id from being all digits, and T15's own tests use `1`,
`2` and `3` as Slack-style ids.

**Solution.** The turn message shows `[#7]`, and `agentctl react` and
`agentctl history --before` take `#7`: `#` and one to nine digits, which no
platform id starts with, resolved in the calling token's session through
`Store::message_ref_by_short_id`. A short id of another conversation is
refused as `react`'s rule refuses any, and `--before` must name a message
in the turn's conversation. A short id the session doesn't have is
`not_found`. Anything else is read as a platform id, as before.

### The turn message shows what the session has no row for

**Issue.** "Thread messages since the agent's last reply" read as the
agent's bot's last message in the history. But a private task's result is
posted as the same bot from another session, and would then count as the
agent's reply, hiding both the result and what came before it from the
channel session. A first version started after the last message the
session itself posted instead, and that still hid what people said while a
turn ran: such a message comes before the turn's reply in the thread, so
the next turn skipped it. It also recorded what it showed before the turn
ran, so a turn that failed before reaching the model hid its own request
from every later turn.

**Solution.** The builder reads up to 50 messages of the thread before the
event and shows every one the session has no row for: a person's message,
said before or during an earlier turn, or the agent's own post from outside
the session, marked as such when it is attributed to a turn (see the next
note for the bot's posts that aren't). The session's own replies and what
it was shown have rows and are left out. Each message shown is recorded in
the session, which gives it its short id and keeps it out of the next turn;
the builder returns the short ids it recorded, and the pipeline deletes
those rows (`Store::forget_message_refs`, inbound rows only) when
`run_turn` fails, which covers every failure before the CLI read the
message, a `SessionReset` included, so the next turn shows them again. A
failed write the CLI still read would be shown twice, which is better than
never. Building that fails halfway forgets what it recorded too. Posts from
other sessions that the 50 messages didn't reach come from
`Store::posted_elsewhere`, listed by short id for `agentctl history`, since
`message_refs` keeps no text. The event's own message is shown last, with
the requester when it isn't the sender (a hop). Message text is kept to one
line in the context block, so a message can't forge its structure; the
event's own text keeps its line breaks, since a request often holds code,
but every line after the first is indented, so none of it starts where a
`[#N] name:` entry or a block would. A carriage return, a vertical tab, a
form feed, NEL and U+2028 and U+2029 break a line there as `\n` does, since
the model may read any of them as one; each becomes `\n`. A thread the
event starts has no history to read.

### Notices have no message ref

**Issue.** `message_refs` rows belong to a session, and a refused message
starts none. A turn that failed before reaching the model has a session,
but a row with `agent_id` set would attribute the notice to a turn that
never ran.

**Solution.** agentd's own notices are posted without a row: a refusal,
a failure before the turn reached the model (a second `SessionReset`
included), the busy line, the notice that part of a reply was lost, and
the one a shutdown posts. Nothing reads one: no turn is billed for it, and
a reply in its thread replies to the thread's root, not to the notice. A
turn's own failure message (a usage limit, a login that expired, a crash, a
timeout) comes from a turn that ran, and is recorded as its reply.

The files a turn uploads have no row either: `Surface::upload` returns no
message, and Slack's `files.completeUploadExternal` doesn't say which
message shares the files, so the trait wasn't changed for Rocket.Chat
alone. An attributed upload would also be a second message of one turn
that another agent's thread could answer. The next turn then shows a
notice or an upload of the agent's bot as `you`, and only an attributed
post as `you, outside this session`, since an unattributed one may be the
session's own. The attribution of an agent's post is waited for only when
the router reads it (see "An agent's post can arrive before its
attribution"), so an upload, which mentions no one, holds up no other
agent's lane.

### `SurfaceLookup` is asynchronous, and the pipeline posts through it

**Issue.** T15's `SurfaceLookup` was synchronous, but finding an agent's
surface means reading its binding and token from the store.

**Solution.** It is an `async_trait` now, and `StoreSurfaces` implements
it: each agent's active binding on the conversation's surface and team,
with a `RocketChatSurface` built from the manager's configuration and the
bot's token, or a `SlackSurface` over the manager app's `TeamDirectory`,
kept per binding. Each Slack lookup also gives the directory the
workspace's active agents' bot users with `set_managed_bots`. `App` builds
it and hands the same lookup to the agentctl API and the pipeline; tests
pass their own through `App::with_surfaces`. Slack agents' messages still
reach no pipeline until T31 routes them, as T30's note says, but their
replies would already go out through this lookup.

### Each agent answers a thread's messages in order, in the pipeline's tasks

**Issue.** A turn takes minutes, and a Rocket.Chat connection hands each
message to its onward sender and waits. A first version spawned a task per
message and per candidate: two messages in one thread could reach the
session's queue in either order, and since each built its turn message
before queueing, the later one could show the earlier as history and the
earlier then run too, answered twice. Nothing bounded the tasks, and
`Server::run` neither waited for them nor stopped them: a shutdown returned
at once mid-turn, and the late reply was posted with the store already
closed.

**Solution.** The sink looks the candidates up and queues the message for
each in a lane per agent and thread, whose task answers its messages one at
a time in arrival order, so the turn message is built only once the turn
before it has delivered. A lane holds at most 8 waiting messages, and the
pipeline at most 64 waiting or running; a person's message past either gets
one line in its thread saying the agent is busy, posted from the sink,
which also slows the connection down. A bot's message gets none, whether
the surface flags the bot or agentd knows it as an agent's or the manager
bot: the router ignores most of them anyway, and two bots could otherwise
answer each other's busy lines. The lanes' tasks run in a `JoinSet` of the
pipeline's own (`tokio-util`'s `TaskTracker` isn't a dependency), and a
panicking message doesn't stop its lane. The set is behind a
`std::sync::Mutex`, so queueing never waits: a sink cancelled mid-send, as
a Rocket.Chat connection's is on every reconnect, can't leave a lane
created without its task. Queueing checks that the pipeline is open under
that lock and never starts a task once it is closed. `drain` polls the set
under the lock without holding it across a wait, so a drain cut off by its
timeout leaves the tasks for `cut_short`, which takes the set and shuts it
down. On shutdown `Server::run` stops the public listener and the chat
connections, closes the pipeline, and gives the turns taken the drain
timeout while the proxy and ctl listeners, which a running turn's CLI and
agentctl need, still serve; only then do those stop, and the pipeline is
dropped before the store is closed. Turns still running or delivering their
reply at the timeout are aborted, their working emoji taken off and their
threads told to ask again, within five seconds: the guard that holds a
turn's working emoji is kept until its reply, or its failure notice, has
gone out, so a reply stuck on a slow post isn't lost without a word. That
is the simplest option that tells people: the turns and their queue stay in
memory rather than the store, so a crash, unlike a shutdown, still loses
them silently, and messages still waiting in a lane at the timeout are
dropped without a word, since no decision was made about them. The working
emoji is held by a guard, so a panicking turn takes it off too.

### An agent's post can arrive before its attribution

**Issue.** agentd records a post's `message_refs` row just after
`chat.postMessage` returns, and the platform may deliver the post to
another agent's connection first. The router then saw a managed bot's
message with no attribution and ignored it, dropping the hop.

**Solution.** When the sender is another agent's bot, the message mentions
the candidate, and it has no attribution yet, the view reads it again,
with pauses doubling from 25 ms, for up to two seconds before routing. Only
that candidate's lane waits. The router reads the attribution in that case
only, so any other post of an agent's bot, such as an upload, which never
gets one, is routed at once.

### Delivery goes on past a failed part

**Issue.** A failed post of the reply ended the delivery: the directives'
reactions, the outbox's reactions and the queued agentctl posts were lost,
and so were the chunks after a failed one. The reply had no size cap,
while `agentctl post` caps its text at `MAX_POST_BYTES`.

**Solution.** Each chunk, the upload, each reaction and each queued post is
tried whatever happened to the others; a chunk refused with a rate limit is
posted once more after the wait the platform asks for, up to five seconds.
If any part was lost, the thread gets one line saying so. The reply is cut
at `MAX_POST_BYTES` on a character boundary, with a note that it was cut; a
backtick or tilde code fence the cut leaves open is closed first, so the
note isn't rendered as code. Failures before the turn reached the model
(writing the persona, reading the plan's model, building the turn message,
starting the process) post the short failure notice too, once the bot is
known to be able to post; a link prompt waits for the same check. The
usage-limit and login texts name "the Claude account this request runs on"
rather than "your", since a turn may run on the community key.

### A turn whose start hook failed stops the process

**Issue.** When `turn_starting` fails, the runner keeps the process warm
(T21). A placeholder that can't be pointed, because it was revoked when a
new container took its address, would fail every later turn the same way.

**Solution.** The pipeline stops the session's process after a turn that
failed in `turn_starting`, so the next turn mints anew.

### Who counts as a managed bot

**Issue.** `RouterView::managed_bot` must know every bot user agentd made,
whatever the binding's state, but `Store::agent_for_bot` finds active
bindings only.

**Solution.** `Store::agent_of_bot_user` finds the agent of a bot user on a
binding in any state, and the view asks it for the sender and each
mention, besides the manager bots' identities from the configuration.
Candidates still come from active bindings.

### `fake-claude` still counts cost from 0 on resume

**Issue.** The real CLI restores a resumed session's total cost (see above),
and `fake-claude` counts each process from 0, as T04 wrote it.

**Solution.** Left as it is: the runner's tests rely on it, and changing
both belongs with T27's correction, which the plan's T27 now names.

## T24: Session commands

### Which sessions the commands act on

**Issue.** The plan lists "active and recent sessions" without saying which
rows those are. A reset marks the row reset and inserts its replacement at
once (T21), so every thread an agent ever answered keeps a live row, and a
thread reset once has a fresh row that never had a turn. Listing every live
row would show each thread ever answered, and resetting them would reset
rows that have no transcript, making yet another row each.

**Solution.** Both commands act on the live sessions in use: not reset, and
with a turn finished (`last_turn_at`), a turn gone to the CLI (`started` or
`maybe_started`), or a warm container. `Store::sessions_in_use` selects them
in SQL, most recently active first (the end of the last turn, or the
creation), with the ids warm on this instance
(`SessionManager::warm_sessions`) passed in as a JSON array for `json_each`,
so the rows a reset leaves behind are never read. `sessions` asks for at
most 20 (`commands::MAX_LISTED`), and only when it gets that many counts
them all (`Store::count_sessions_in_use`) to say how many there are. A
private task's session is listed and reset by `reset <name>`, but not by
`here`, since it isn't the conversation's own session. A reset session is
gone from the list; its replacement shows up again once it has a turn.

A session left out is one the CLI never read a message of, so it has no
transcript, and its next turn starts with `--session-id` whether it is
reset or not. That covers a session the pipeline has just looked up for a
turn it is still preparing: resetting it would change its id and nothing
else. A session that has run is reset even with such a turn pending, and
the turn then finds it reset and moves to the replacement (T23's retry on
`SessionReset`).

### `here` is the conversation, not the thread

**Issue.** A channel has one session per thread, so "the current
conversation's session" is one session only in a DM. A Slack slash command
names its channel but no thread (Slack doesn't offer slash commands in
threads), while an `!agent` message on Rocket.Chat may be sent in one.

**Solution.** `here` resets the agent's sessions of the conversation the
command was sent in: a DM's one session, or every thread of a channel, the
same on both surfaces. `Origin::SlackSlash` now carries the slash command's
conversation, and `Origin::conversation` gives it, or for `!agent` the room,
on the sender's team. In the manager bot's DM there is no conversation to
reset, so `here` is refused there with how to send it. A message there has
no conversation, but a Slack slash command sent there names the DM like any
other, so when a slash command's `here` finds no session, its conversation
is compared with the manager's DM with the owner (`Replies::dm_room`, a
`conversations.open`) and the command refused the same way. Only a slash
command is compared: on Rocket.Chat the manager bot's DM always arrives as
`Origin::RocketChatDm`, and opening it (`users.info` and `im.create`) would
only cost two calls for an answer known beforehand. A DM with the
agent's bot is a room like any other (T13), so `!agent reset <name> here`
there resets the owner's DM session, and in a room only the agent's bot is
in the agent's connection hears it, as T14 made every connection feed the
intake.

### Thread links

**Issue.** "A thread link where the surface can build one" needs a URL for
each surface, and `Surface` has no way to make one.

**Solution.** Two pure functions, used for the sessions of the surface and
team agentd serves:

- Slack: `surface_slack::surface::thread_link`, the web client's
  `https://app.slack.com/client/<team>/<channel>`, then
  `/thread/<channel>-<ts>`, which names the workspace by id and needs no
  Web API call (`chat.getPermalink` would, per message).
- Rocket.Chat: `RestClient::room_link`, the web client's routes
  `<base>/channel/<name>`, `<base>/group/<name>` or `<base>/direct/<room
  id>`, then `/thread/<root>`. A DM's route takes its id, so the owner's DM
  and group DMs need no call; a channel's type and name come from the
  manager's `rooms.info`, once per room and command, and a room the manager
  can't read has no link.

Another member's DM with the agent has no link, since the owner can't open
it, and neither has a private task's session, whose thread (where its
result goes) may be such a DM. A Rocket.Chat private group has no link
either: the manager may be in groups the owner isn't, and the link would
show them the group's name. Rocket.Chat has no cheap call for whether a
given user is in a room (`groups.members` pages through every member), so
the owner's membership isn't checked. Neither form was checked against a
live client.

### The commands reach the runner through a weak handle

**Issue.** `Commands` is built with `App`, before `serve` connects to Docker
and starts the runner (`pipeline::Turns`), and many tests start agentd with
no runner at all. A strong handle in `App` would also keep the runner's idle
reaper and event follower running after the pipeline is dropped at
shutdown, past the store's close.

**Solution.** `commands::SessionControl` is what the commands need
(`reset`, `warm_sessions`), implemented for `SessionManager`. `Turns::start`
hands its sessions to `app.commands()` as a `Weak`, so every path that
starts a runner for an app wires it, and dropping the runner ends it.
Without one (no `[sandbox]`, or after shutdown) `reset` marks the session
reset in the store alone, and nothing is warm. `warm_sessions` knows this
instance's containers only. A warm process on another instance keeps its
old session until its next turn there finds the session reset (T21's
`RunnerError::SessionReset`), which moves the turn to the replacement, and
the idle reaper stops the old container.

### A reset waits for the session's turns, the reply doesn't

**Issue.** `SessionManager::reset` runs after the turns queued before it,
which can take up to the turn timeout each, and it joins the session's
queue only when its future is first polled. Resetting a few sessions at a
time left the others out of their queues until an earlier reset ended, so a
message sent in one of them after `reset` ran on the old conversation and
was then wiped. Waiting for every reset before replying also held up the
owner's later commands, which the intake runs one at a time (T13), and
could outlast a Slack `response_url`, which expires after 30 minutes.

**Solution.** Every reset is issued at once and polled once before the
reply, so each is queued on its session before the owner reads
"Resetting". The first poll of `reset_all` polls every reset future itself,
in a plain loop, and only then awaits them all with `join_all`. That loop
runs under `tokio::task::unconstrained`: Tokio's cooperative budget allows
128 operations per task poll (tokio 1.53), each lock taken on an idle
session spends one, and once it is spent a lock returns `Pending` before
joining the mutex's queue, so a plain poll left every idle session past
about the 128th out of its queue until after the reply.

The loop polls every reset whatever the others do, so every reset has
joined its session's queue when the first poll returns, without exception.
`join_all` alone didn't promise that: it drives more than 30 futures
through `FuturesOrdered`, whose `FuturesUnordered` returns `Pending` once
two futures have woken themselves while being polled (`yielded >= 2`,
futures 0.3.34), and leaves the rest for the next poll, after the reply. A
reset is woken inside its own poll when its session's lock is handed over,
its spawned task ends, or a store permit is let go on another worker
between it registering its waker and returning `Pending`. The agentd
test's fake runner wakes itself once in every reset's first poll: with
`join_all` alone 2 of 200 resets held their session before the reply, and
without `unconstrained` 128. The first poll never yields: for 10,000
sessions it took 71-75 ms with a runner (a slot lock and a spawned task
each) and 18-19 ms without one, in a debug build on a loaded machine, two
runs each, holding one worker thread that long.

A reset's store write takes one of 2 permits (`store::RESETS_AT_ONCE`):
`Store::reset_session` waits for one before it takes a connection and lets
it go when its transaction ends. Without a cap, a reset of thousands of
sessions ran as many `BEGIN IMMEDIATE` transactions at once against the
store's pool of 10 connections: with 2,000 an unrelated `ping` waited 3
seconds, and every other agent's turns waited behind them, or past the
pool's 30-second acquire timeout. The semaphore lives in `Store` and is
shared by its clones, so it bounds every reset in the process, through the
runner or, without one, in the store alone, and neither `SessionControl`
nor `SessionManager` passes permits around. It covers only the write.
Stopping a container never used the pool and is bounded by
`global_container_cap`. An earlier version held the permit through the
stop as well, about 10 seconds or 120 with a degraded Docker daemon, which
held up every other reset, cold ones and other owners' too, each holding
its session meanwhile, so turns queued there filled the pipeline's
`max_pending` and `evict_idle` couldn't free their containers.
`SessionManager::reset` stops the container and then writes, with the
session held, so the reset is already queued while it waits for a permit,
and it waits on no other session while it holds one. A turn sent to a
session whose reset waits for a permit waits for it, as it would for the
reset itself.

SQLite has one writer, so more permits add no throughput and only park
more of the pool's connections in the busy handler. A throwaway probe
measured it: a file database in WAL mode, 2,000 sessions reset at once,
and a `ping` and an unrelated one-row `UPDATE` every 5 ms meanwhile, three
rounds of each cap in a debug build on 4 shared, loaded CPUs. The cap of 8
before this change was applied in the probe around each write, the cap of
2 is the store's own.

| Cap | Reset of 2,000 | `ping` p99 | `ping` max | Unrelated write p99 | Unrelated write max |
| --- | --- | --- | --- | --- | --- |
| None | 3.8-5.1 s | 12 ms-1.4 s | 3.0-3.6 s | 0.06-2.1 s | 2.8-3.7 s |
| 8 | 4.7-5.5 s | 1.0-5.5 ms | 4-34 ms | 0.63-1.04 s | 0.93-2.1 s |
| 2 | 4.4-5.0 s | 1.6-3.7 ms | 8-32 ms | 0.43-0.53 s | 0.63-2.0 s |

A reset takes as long with 2 as with 8. A `ping` reads, which WAL lets it
do while a write is open, so it waits only for a connection, and both caps
leave it some; 2 leaves 8 of the 10 free rather than 2. An unrelated write
still waits about half a second at p99: SQLite's busy handler retries after
sleeps of up to 100 ms and loses to resets that write back to back, which
no cap on resets alone makes fair. A single reset can queue for a permit
behind another owner's mass reset: at these rates one of about 10,000
sessions holds the permits for 20-25 seconds. That is accepted.

Waiting for the resets to end is the command's `FollowUp`: the intake
releases the member's command order once the reply is sent and then runs
the follow-up in the same task, so the owner's next command goes ahead. If
a reset fails (a container that can't be stopped isn't reset, T21), the
owner is told in a direct message from the manager bot, with the command
to send again. At shutdown the intake waits
for follow-ups as for commands, within the drain; one still waiting when the
drain ends is dropped with the intake's tasks. A reset still queued behind
its session's turns then leaves the queue without resetting. One that holds
its session goes on: `with_slot` runs the work in a task of its own, as for
a turn, and dropping the caller drops only its `JoinHandle`. It keeps the
runner's `Inner` alive until it ends, within one container stop, and it can
reach the store after `Store::close`. That is harmless: `close` waits for a
transaction in progress, and one begun after it fails at once, leaving the
session unreset with its container stopped, as any failed reset does, and
the process's exit ends the task anyway. Stopping the task with its caller
would cancel a container stop part way or thread a cancellation into the
write, for no gain. The follow-up lives only in memory: if the instance
dies, the queued resets die with it and nothing is reset, which the owner
sees in `sessions` and can send again.

The follow-up isn't polled while the reply is being sent. A reset queued on
a busy session whose turn ends in that window is handed the session's lock,
but runs only once the reply is sent, so the session's next turn waits for
the reply's round trip too. Driving the follow-up alongside the reply would
need the failure DM held back until the reply is out, and the resets task
aborted with the intake at shutdown, which isn't worth a delay of one
reply.

## T25: Skills and the agentctl skill

### Hosts are confirmed with a command of their own

**Issue.** The plan says the owner confirms a skill's `allowed-hosts` when
adding it, but `/agent` has no dialog: a reply can't ask and wait. Adding the
skill at once with its files but without its hosts would leave a skill that
fails when used, and asking the owner to run `skill add` again means
uploading or cloning twice.

**Solution.** A skill that declares hosts is fetched and checked once, and
kept outside what sandboxes mount (`<data>/skills-pending/<agent>/<name>/`)
with a `pending` row; the reply lists the hosts and asks for
`skill confirm <name> <skill>` within an hour (`PENDING_TTL`). Confirming
moves the files into the agent's skills and makes the row `active`, which is
when its hosts count. A confirmation after the hour finds the skill dropped;
the sweeper drops expired ones every minute, with their files, and startup
too. The parser gained `SkillCommand::Confirm`, and the design's command
table lists it.

### Skills are rows, their files are directories

**Issue.** The egress extension reads a session's hosts at every `CONNECT`,
and `skill rm` has to find what to remove, after restarts and on every
instance, so the hosts can't live in memory; parsing every agent's
`SKILL.md` files at each `CONNECT` would trust files over the store.

**Solution.** A migration adds `agent_skills` (`agent_id`, `name`, `state`
of `pending` or `active`, `source`, `hosts`, `added_by`, `added_at`), keyed
by agent, name and state, so a pending skill can wait next to the active
one it would replace. `Store::skill_hosts_for_session` joins `sessions` and
`agents` (deleted agents get none) and `SkillHosts` parses each host with
`HostRule` again, so a row that no longer parses allows nothing. Files stay
on disk, moved into place with a rename so a session sees a skill whole or
not at all. Replacing one moves the old directory aside first, so a session
starting between the two renames sees neither; `renameat2` with
`RENAME_EXCHANGE` would close that, but needs a fallback for file systems
without it, and the window is two renames.

The row and the files change in the order that never grants hosts to files
the owner didn't confirm them for. `skill add` records the row first: an
active row carries no hosts and replaces any row that did, and a pending
row's hosts don't count. `skill confirm` moves the files into place, then
makes the row active; a failed move leaves it pending, and the old skill is
moved back. `skill rm` deletes the rows, then the directories. A failure
between the two steps can leave directories no row records, so startup
removes work directories and skill directories, pending or live, whose name
has no row in either state (the bundled one aside). A row of either state
keeps both of its name's directories, so a confirmation moving one from
pending to live on another instance is never taken for left over. Startup
also leaves anything changed (by status change time) within `STALE_AFTER`,
the clone timeout and three minutes, since in a blue-green deploy the old
instance may still be cloning into a work directory. The 32-skill cap is
checked inside `put_skill`'s transaction.

`skill rm` refuses new connections to the skill's hosts at once, but the
egress proxy has no hook to close one agent's tunnels to one host, and
revoking the agent's sessions would restart its conversations. Tunnels
already open end on their own, within the 5-minute idle timeout or the
1-hour lifetime, and the reply says so.

### A clone reaches agentd's own network unless the host is checked

**Issue.** T08's parser keeps options, other transports and credentials out
of the source, but not its host: `https://169.254.169.254/…`,
`https://10.0.0.5/…` or `https://rocketchat:3000/…` (a single-label Compose
name) parse, and agentd clones from its own network, next to Rocket.Chat and
MongoDB. `git` would also follow a redirect anywhere, and resolve the name
again after any check.

**Solution.** The host must be a DNS name (`cred_proxy::normalize_host`:
two labels or more, no IP forms). agentd resolves it itself (5 seconds) and
refuses it if any address is one the egress proxy never reaches, reusing
`EgressPolicy::unreachable`, now public, with agentd's own addresses and the
sandbox subnet. `git` is then pinned to the checked addresses with
`http.curloptResolve` (Git 2.37 or later), follows no redirect
(`http.followRedirects=false`; a moved repository has to be given by its new
URL), may use only `https` (`protocol.allow=never`,
`protocol.https.allow=always`), and runs with an empty environment, no
system or global configuration, no credential helper or prompt, no hooks or
templates, no bundle URIs (`transfer.bundleURI=false`, `fetch.bundleURI=`)
or file system monitor, `transfer.fsckObjects`, and `core.symlinks=false`.
The empty environment also means an operator's `HTTPS_PROXY` never reaches
a clone: it connects directly, from agentd's own network. The clone is
`--depth=1 --single-branch --no-recurse-submodules --no-tags`, the ref only
as `--branch=<ref>` and the URL after `--`. It runs in its own process group,
killed whole after 2 minutes or once its directory passes 40 MB. Tests serve
a local repository through `Git::serving_prefix_from_directory_for_tests`,
which rewrites one `https://` prefix to a `file://` directory and skips the
lookup; it and the `file://` configuration exist only in test builds.

A clone runs inside the owner's command, and one member's commands run one
at a time, so a clone that takes its full 2 minutes holds that owner's
other commands for as long; other members aren't held up.

### The URL git gets names the host as the pin does

**Issue.** agentd checked and pinned the normalized host, but gave `git`
the URL as the owner wrote it. curl matches `http.curloptResolve` entries
by name, without dropping a trailing dot, so `https://evil.example./r`
missed the pin for `evil.example`, and curl resolved the name again,
reopening DNS rebinding into agentd's network.

**Solution.** `git` gets a URL rebuilt from what was checked,
`https://<normalized host>:<port><path>`, so its host and port are exactly
the pin's; a test runs a stand-in `git` and compares the two, `GitHub.com.`
included. The egress proxy has no such gap: it connects to the addresses it
checked, never to a name.

### Measuring a clone's directory can't stop one file

**Issue.** The 40 MB cap was checked by measuring the directory every
250 ms. A small pack of a highly compressible blob checks out hundreds of
megabytes between two measurements, onto the volume that also holds the
store; a test cloning an 8 MB blob of zeros under a 1 MB cap finished
before any measurement saw it.

**Solution.** No file `git` or its helpers write may pass 40 MB:
`RLIMIT_FSIZE`. The workspace forbids `unsafe`, so `pre_exec` can't set it,
and `prlimit` on the child's pid after `spawn` races `git` starting
`git-remote-https`, which would not inherit it. `git` starts through
`/bin/sh -c 'ulimit -f "$1" && shift && exec "$@"'`, which sets the limit
before `exec`, so every process of the clone has it; `compose-test.sh`
checks that the image runs `git` that way. A write past the limit kills
the writer with `SIGXFSZ`, and `git` killed by it is `TooLarge`. When a
helper such as `index-pack` is the one killed, `git` exits with an error
and removes the clone, and the owner gets the generic "Git couldn't clone
that" reply. The directory is still measured, for the total.

### The agentd image needs git

**Issue.** agentd clones skills itself, on the egress network, as the plan
says, and the distroless image has no `git`. A Rust Git client would be a
large dependency for one shallow clone.

**Solution.** The runtime stage is `debian:trixie-slim` (the digest the
sandbox image pins) with `git` and `ca-certificates`, and Debian's `/bin/sh`
for `ulimit -f`; it still runs as 10001. Trixie's Git is 2.47, above the 2.37 `http.curloptResolve` needs.
`compose-test.sh` checks that `git` runs in the image. The image couldn't be
built here (Debian's mirror is blocked); CI builds it.

### What a skill package may hold

**Issue.** The plan asks for a size cap and a `SKILL.md` with `name` and
`description`. An upload or a repository is the owner's, fetched from
anywhere, and ends up mounted into sandboxes.

**Solution.** `skills::package` checks every skill the same way, whatever
it came from: at most 10 MB of files, 1,000 files and directories, 16
levels and paths of 1,024 bytes (so a deep tree of long names is refused
before the file system answers `ENAMETOOLONG`); names without an empty, `.`
or `..` part, `\`, control or invisible formatting characters; only regular
files and directories (a symlink or a special file in a zip is refused; a
clone checks symlinks out as plain files holding their targets, and a
symlink found in any tree is refused); modes rewritten to
`0755` for directories and `0644`, or `0755` with an execute bit, for files.
A `.zip` is read with the `zip` crate (MIT) with only
`deflate-flate2-zlib-rs`, which adds `flate2`, `zlib-rs` (Zlib), `crc32fast`
and `typed-path`: stored or deflated entries, none encrypted, each counted
as it inflates against its declared size, so a small archive can't unpack
past the cap. macOS's `__MACOSX/` entries are skipped. `SKILL.md` is at the
top or in the only top-level directory (anything else, a lone file at the
top included, is "no SKILL.md"), at most 256 KB of UTF-8; its front
matter, at most 16 KB between `---` lines (YAML's `...` doesn't close it), is read with `serde_norway`
(now a normal dependency of agentd) into the three keys agentd needs, so
other keys are skipped rather than built: a "billion laughs" document under
a key agentd doesn't read parses at once, and one under `allowed-hosts` or
`description` fails on its type. The name follows Claude Code's
`[a-z0-9-]{1,64}` and can't be `agentctl`; the description has 1 to 1,024
characters; `allowed-hosts` is a list or a comma-separated line of at most
16 `HostRule`s, so `api.anthropic.com`, IP addresses and single labels are
refused before the owner is asked. A skill may not declare a wildcard: a
built-in list of public suffixes would always miss some (`*.github.io`,
`*.herokuapp.com` and the rest of the Public Suffix List's private
section), any agent's owner can add a skill, and naming each host costs a
skill little within 16 entries. Hosts on a port other than 443 are named
again in the reply that asks for confirmation. Error replies are fixed sentences that
never repeat the content. An agent has at most 32 skills besides
`agentctl`.

### The bundled skill and the mount

**Issue.** The bundled skill must always be present, and Docker refuses a
bind mount whose source doesn't exist, while the runner's own tests start
sessions for agents agentd never prepared.

**Solution.** The pipeline writes `<data>/skills/<agent>/agentctl/SKILL.md`
before every turn, next to the persona and with the same
write-only-when-changed helper (`runner::write_if_changed`, which
`write_persona` now uses), so an upgrade of agentd updates it. The runner
sets `SessionSpec::skills_dir` to `<data>/skills/<agent>` when that
directory exists as the container starts. A skill added, replaced or removed
reaches a conversation when its process next starts, as a persona does.

### Files from both manager DMs reach the handlers

**Issue.** T30 left the Slack DM's files unpassed and T14 read attachments
only in the Rocket.Chat DM, and `WebApi::download_file` reported a file over
the limit as `SurfaceError::Api`, where Rocket.Chat's download says
`TooLarge`.

**Solution.** `commands::slack::dm_command` returns the event's files and
the Slack inbound submits them with the command. `Commands::download` reads
an attachment from either manager's DM (and nowhere else), and both
`persona` and `skill add` use it, so `persona <name>` with a `persona.md`
attached works on Slack too. `download_file` answers `TooLarge` past its
limit.

### Skills reach the model only with the Skill tool

**Issue.** The launch flags gave `--tools "Bash,Read,Edit,Write,Glob,Grep"`.
Claude Code 2.1.285 lists the skills it finds in
`$CLAUDE_CONFIG_DIR/skills` to the model only when the `Skill` tool is
among the enabled tools; a probe against a fake API showed a mounted skill
never appeared in the request without it. Every skill, the bundled
`agentctl` one included, was mounted and never seen.

**Solution.** `runner`'s `TOOLS`, and the design's launch flags, add
`Skill`. A runner test fails if `--tools` lacks it, and the fakes and
tests that spell the flags out follow.

With `Skill` enabled, Claude Code also loads
`$CLAUDE_CONFIG_DIR/commands/*.md` as commands, and `CLAUDE_CONFIG_DIR`
(`sessions/<id>/claude`) is the session's to write, so an agent can plant
commands that last for that session; that grants nothing new, since it can
already write `CLAUDE.md` and `settings.json` there. Under
`--setting-sources user` a project's `.claude/skills` and `CLAUDE.md` in
the working directory are not loaded.

### Commands always have skills

**Issue.** `Commands` took its `Skills` through an optional
`with_skills`, only so that tests could build one without it, which needed
`Arc::make_mut` and a "not available" branch agentd never took.

**Solution.** `Commands::new` takes the `Skills`. Tests that never run a
skill command pass one over a data directory that doesn't exist, which
nothing reads until a skill command runs.

### Replacing a waiting skill drops the old one first

**Issue.** `skill add` of a skill with hosts upserted the pending row (the
new hosts and source) before moving the new files into
`skills-pending/<agent>/<name>`. If that move failed, or agentd died in
between, the owner got an error, not a prompt naming the new hosts, and a
later `skill confirm` meant for the first prompt made the new hosts active
on the old files. `confirm` also made active whatever pending row of the
name it found at the end, which another instance could have replaced since
`confirm` read it; and when that row was gone it removed the whole skill,
an unrelated active version included. The sweeper could drop a pending row
in the middle of a confirmation that began just before the hour was up.

**Solution.** A pending add first deletes the name's pending row and
removes its pending directory, then records the new row and moves the new
files in, so a failure anywhere leaves either nothing waiting or a row
without files, which `confirm` refuses. `Store::confirm_skill` takes the
row `confirm` read and makes it active only while its hosts and `added_at`
are unchanged. When it isn't, `confirm` undoes only its own move: the
active skill it set aside goes back, or, with none, the files it moved are
removed, unless something else has taken their place since (checked by
inode). `confirm` first renames the pending directory into its own work
directory, where no concurrent add can replace it, and takes the inode
there. It also reads the moved `SKILL.md` again and confirms only when its
hosts are the row's: two adds of one name racing can leave one's row with
the other's files, and those go back to wait, unconfirmable, until the
skill is added again or expires. A test confirms a stale copy of the row
(`added_at` a second earlier) while an active version exists: reverting
the undo to removing the skill by name fails it. The sweeper drops pending rows `PENDING_TTL` plus one
`SWEEP_INTERVAL` after they were added, while `confirm` still calls a skill
expired after `PENDING_TTL`. A test replaces a waiting skill with one
declaring another host while the pending directory can't be written, then
confirms: before the fix the confirmation made the new host active.

Startup's purge keeps both directories of a name that has a row in either
state, so it no longer depends on a rename updating the moved directory's
status change time. A pending directory an active add failed to remove is
then left until the skill is next added or removed.

### Tests stop the processes they hold

**Issue.** Declaring `child` before `stdin` in `ClaudeProcess` and
`ChildIo` stopped a drop from closing a process's input before killing
it, but a drop still sends `SIGKILL` to a child `try_wait` reports as
running. A child that is already exiting, as `fake-claude` is while it
writes its coverage profile, can still be cut short and leave a truncated
`.profraw`. Two runner tests dropped a warm `ClaudeProcess` at their end.

**Solution.** Every test that holds a `ClaudeProcess` stops it before
dropping it: `stop()` closes its stdin and waits up to `EXIT_GRACE`, so
`fake-claude` exits on end of input and writes its whole profile. The
runner reaps every exit it sees, a crash, a refused resume or a failed
write, before the turn returns. What is left for a drop to kill is a
warm process a `SessionManager` holds when a test ends, which waits for
input and has nothing to write; the tests wait for the stops they start,
and agentd's drain for the turns in flight. `ProcessChild::drop` keeps
killing at once: waiting there would block a runtime thread.

### A stand-in `git` is written by a child process

**Issue.** A `git` stand-in test wrote an executable script with
`std::fs::write` and then ran it, while other tests in the agentd library
binary spawn processes (`git` for fixture repositories, clones). A
process forked during the write inherits the descriptor open for writing
until it runs its own program, and running the script meanwhile fails with
`ETXTBSY`, as the runner's tests did on aarch64.

**Solution.** `stand_in` has `/bin/sh` write the script, so this binary
never holds a descriptor open for writing it and none can be inherited.
No lock is needed, in the tests or around production spawns. It is the
agentd tests' only file written and then run.

## T26: Requester-pays routing

### The community key lives in one sealed row, read on every request

**Issue.** The plan makes `/agent admin api-key set` the key's only
source and asks for it sealed, never logged and never reachable from a
sandbox but through the proxy's swap. It doesn't say how the proxy gets
it, how a change reaches a running agentd (or a second instance), or what
an operator can read back about it.

**Solution.** `community_settings` holds one row (`id` is 1 by a `CHECK`,
inserted by the migration) with `api_key_enc`, sealed with
`community_settings/api_key_enc/1` as its associated data, and
`api_key_changed_by` (a member key's string form) and
`api_key_changed_at`, which record who last set or cleared it; `me` shows
them to admins. `StoreCommunityKey`, the proxy's `CommunityKey`, reads and
opens the row on every request, with no cache, so a key set or cleared on
one instance applies at once on every instance sharing the store. The
router's view reads only whether the column is set
(`community_api_key_set`), without opening it or reading who changed it,
so a bad value in the audit columns breaks `me` for admins, not every
turn's routing.
The key reaches memory only in the proxy's request and in the `admin
api-key set` command, both as `SecretString`; commands are logged by name,
and a captured-log test at `trace` finds the key in neither the log nor
any reply. Sandboxes get an `agentd-key-…` placeholder, as T18 made it,
and a test shows the proxy swaps in the stored key, refuses with 401 once
it is cleared, and with 403 between turns.

### Admins are identities, matched exactly

**Issue.** "Admins are listed in configuration, by `MemberKey`" names no
section or key, and says nothing about members with identities on two
surfaces.

**Solution.** `[community] admins` is a list of member keys in their string
form, `<surface>:<team>:<user>`, checked at load with the key named in the
error. It is empty by default, so nobody is an admin until the operator
says so. An identity is matched exactly, surface and team included: the
same user id on another Rocket.Chat server or Slack workspace is not the
admin, and an admin who uses both surfaces is listed twice. A non-admin
gets one line saying only admins can change the key, and a key they sent
privately is dropped unstored; a key sent where others can read it is
refused first, admin or not, as T13 does. T27's `admin ban` and `unban`
should use the same list.

### A key is only checked for being a header value

**Issue.** Nothing said whether `admin api-key set` validates the key.

**Solution.** It must be 1 to 512 bytes of visible ASCII, which is what an
`x-api-key` header value can carry; anything else is refused without
being repeated. agentd doesn't try the key against Anthropic before
storing it: that would send it upstream outside any turn and make the
command depend on the network. A key Anthropic refuses shows up on the
first community turn, whose thread and requester are told the community
key was refused and an admin can set a working one.

### Whose account hit the limit, and who is told

**Issue.** T26 asks that usage-limit and auth errors be "shown to the
requester, never the owner, and name whose account hit the limit". T23b
posts a failure message in the thread, where everyone, the owner
included, reads it, and its texts said "the Claude account this request
runs on". `Surface::render` has no way to mention a member by id, so the
thread text can't name the requester as a mention.

**Solution.** Two messages. The thread gets the turn's recorded reply,
naming the account by whose it is: "the Claude account of the person who
asked" or "the community API key", for a usage limit or a refusal
(`USAGE_LIMIT_TEXT`, `LOGIN_EXPIRED_TEXT`, `COMMUNITY_USAGE_LIMIT_TEXT`,
`COMMUNITY_KEY_REFUSED_TEXT`). The requester, and only the requester,
also gets a direct message from the manager bot through the same
`Replies::dm` the link prompt uses, saying it was their account (or, with
no account linked, the community key) and what to do. The owner is never
messaged unless they asked; for a hop the requester is the inherited one.
In the requester's own DM with the agent the reply there is private
already, so no second message is sent. A refused login whose link is
already marked broken gets none either: the relink notice (T13) tells the
member once. A test refuses
bob's refresh mid-thread and sees the thread told, the link broken and no
message from the pipeline.

The direct message is also rate-limited, since a credential that keeps
failing without its link being marked broken (an upstream 401 on a token
the refresh didn't reject, a usage limit that holds for hours) would
otherwise message the requester on every turn. The thread is still told
every time. The requester gets at most one message per
`FAILURE_DM_INTERVAL` (an hour) for each kind of failure and whose
credential it was: `usage_limit/member`, `refused/member`,
`usage_limit/community` and `refused/community`. The last time is kept in
a `failure_notices` table keyed by the requester's identity, since a
community-key turn may have no member, and claimed with one conditional
upsert before sending, so it holds across restarts and instances. A
message that then fails to send releases the claim, deleting the row only
if it still holds the time claimed, so the next failure of that kind tries
again. If the claim itself fails, the requester is told anyway.

### The pipeline also refuses a private scope for anyone but the owner

**Issue.** The router never gives a non-owner `ScopeKind::Private` (its
invariant grid checks it), and the hooks refuse the owner's side outside
the private session, but the pipeline mapped whatever scope the decision
named to a `ScopeKey`, so a router change could put a non-owner's turn,
with the requester's own credential and the public side, on the agent's
`Private` volume, where the owner's `shared/` is.

**Solution.** `turn_scope` resolves the private scope only for the owner's
own turn on the owner's side and credential, answering the owner's own
message in a one-to-one DM: the event is a DM, its sender is the
requester's identity, and it isn't flagged as a bot's. A channel message
or another agent's hop never gets it, whatever the decision says. The
conversation's own scope is resolved only for a public-side turn; any
other combination fails the turn before a session is looked up, logged as
an error. A unit test walks every requester, credential, scope kind, side,
conversation kind, sender and bot flag, and finds exactly one combination
that resolves to the private scope. A pipeline test runs a non-owner's DM
on its `Dm` scope with no private volume created.

### A plan read at a refresh counts from the next turn

**Issue.** The model is picked from the requester's plan before the turn
starts, and `auth` reads the profile after it has handed the refreshed
token out (T09), so the turn whose request triggered the refresh can't
use a plan that refresh found.

**Solution.** As the plan says, the change takes effect on the next turn:
the pipeline reads `claude_links.plan` again for every turn, and the
runner restarts the process when the model differs. A test refreshes bob's
token mid-thread with a profile that moves him to Claude Max, and sees the
first turn on the refreshed token with the old model, the next on the new
model, in a new process.

### Seeing a process restart from a test

**Issue.** The acceptance wants a process restart between a linked
member's turn and the community key's, but nothing outside the runner
says which process ran a turn: every process of a session sends the same
`x-claude-code-session-id`.

**Solution.** The tests' scripted turns run `sh -c 'echo $PPID >> pids'`
through `fake-claude`, which records the pid of the `claude` process that
ran each turn. A change of credential kind or model shows as a new pid,
and a test with two linked members on one model shows the same pid for
both turns, with each turn's own bearer token upstream: the warm process's
placeholder follows the requester.

### A broken link asks for a new login, never the community key

**Issue.** The router's view counted a link as linked only when
`claude_links.broken_at` was empty, and the router sent anyone not linked
to the community key when one was set. So a member whose link broke was
silently billed to the community key, on the default model, and a failure
there told them they had no Claude account linked. The plan says only
that an unlinked member runs on the community key.

**Solution.** A member whose link broke is still a linked member, not an
unlinked one: nothing runs for them on the community key, and they are
asked to log in again. `RouterView::link_state` answers `Unlinked`, `Linked` or
`Broken` in place of `is_linked`, and `route` returns
`Decision::RelinkPrompt { requester }` for a broken link, the owner's
included, whether or not a community key is set. The pipeline sends it
through the same manager-bot DM as the link prompt and the relink notice
(T13), saying the link stopped working and to send `login` again; nothing
runs and the thread gets nothing. The community key is for a requester
with no link at all, or no member. The requester's failure messages no
longer say "you have no Claude account linked". Router tests, and the
invariant grid with an owner and a member whose links may be broken, check
that a broken link never runs, and a pipeline test sets the community key,
marks bob's link broken, and sees bob's channel message and DM make no
upstream request and each get the relink prompt, while carol, unlinked,
still runs on the community key.

## T27: Usage meter, limits, allow and deny

### The base branch lacked T24 to T26

**Issue.** T27 builds on T26 (community admins, the `admin` command group,
`failure_notices`), but `claude/slack-agent-apps` (T31), which T27 was to
be stacked on, and `claude/skills` (T25, on T26, on T24) both fork from
`claude/turn-pipeline-delivery`, so neither holds the other.

**Solution.** The branch merges `claude/skills` first, in a commit of its
own, resolving the conflicts: `Commands` keeps T25's `Inner`, which isn't
`Clone`, and carries T31's `SlackAgents` beside the admins; the pipeline
keeps T31's confirm-then-act split with T26's relink prompt; the Slack
inbound passes both T25's files and T31's in-flight place. The T27 commit
comes after it, so it reads on its own once the stack is chained again.

### The runner reads the restored total from the transcript

**Issue.** The plan offers two ways to take the total a `--resume`d
process restores off its first turn's cost: read the transcript's last
`cost-state` line, or keep a total in `sessions` when a process exits
cleanly. The second can't see what the CLI will actually restore: a
leftover process in the container can append its own `cost-state` line
after a clean exit, and the CLI restores that one.

**Solution.** The first. `SessionManager` reads the total before it
starts a `--resume`d process, off the async runtime, and hands it to
`ClaudeProcess::count_cost_from`; `TurnResult::cost_usd` is then the turn's
own on every turn. The transcript is agent-writable, so the read opens
`claude/projects/<id>/<id>.jsonl` below the session's directory one
component at a time with `O_NOFOLLOW` (rustix `openat`, a runner
dependency now), accepts only a regular file, opened `O_NONBLOCK` so a
FIFO can't hang it.

The runner must read the total the CLI restores, or none: a total it
reads but the CLI doesn't restore is billed, the difference up to
`MAX_TURN_COST_USD`, to the next requester per forced crash and resume.
Two versions tried to mirror the pinned CLI's loader and missed. The
first read a line with a derived serde struct, which accepts a JSON array
as the struct's fields in order, so `["cost-state", 0]` read as 0. The
second parsed lines as `JSON.parse` does and checked the CLI's zod schema
(read from 2.1.285's bundled source), and a review with the real CLI
against a stub API found two more paths: past 5,242,880 bytes (`gue` in
the source) the loader indexes lines by their first bytes before parsing
them, so `{"type":"artifact-autoreact-ledger","type":"cost-state",…}` is a
ledger line to the CLI and a `cost-state` line to `JSON.parse`; and the CLI
restores the `cost-state` line of the session its last message names,
so an appended message and `cost-state` line of another session id
restored that session's total.

So the runner no longer mirrors the loader; it fails closed. It reads a
total only when all of these hold, and otherwise the total is unknown:

- the file is at most 5,242,880 bytes, and ends its last line (the CLI
  ends every line it writes, and the size keeps the CLI off its index
  pass and its compaction, both only past that size);
- every line is blank or a JSON object with no key written twice at any
  depth (the CLI never writes one twice);
- every line with a `sessionId` carries the session's own id, and every
  message (a `user`, `assistant`, `system`, `attachment` or `progress`
  line, or any with a `uuid` or `parentUuid`) carries one, so the last
  message names this session;
- a `type` that is present is a string, and every `cost-state` line
  starts with the bytes the CLI writes,
  `{"type":"cost-state","sessionId":"<id>",`, holds only the keys the CLI
  writes, and passes its schema with a margin: every amount at most half
  the CLI's bound (so the two parsers' rounding can't fall on different
  sides of it), model names printable ASCII, the token sums in bounds.

The total is then the last `cost-state` line's, or 0 without one. The
unknown side is always the runner's: an agent can make its first turn's
cost unknown, never move it. The file is read whole, and no line has a
length cap: the CLI's own lines (tool results) run long. A missing
transcript (the CLI then refuses the `--resume`) or one it can't open this
way is unknown too.

This guarantees what the Docker test checks, not every path of a loader
the runner can't see all of: `docker_the_pinned_cli_restores_whatever_total_the_runner_reads`
(a runner unit test CI's Docker job runs) resumes the pinned CLI from the
sandbox image on crafted transcripts, against a stub API that answers 400
so the result's `total_cost_usd` is the restored total, and asserts that
whenever the runner reads a total the CLI restored exactly it. First it
has the CLI write a real session: one `-p` turn against a stub that
streams a `Write` call and then a text reply, so the transcript holds what
the CLI writes for a tool turn (queue operations, attachments, the
`tool_use` and `tool_result` lines, `last-prompt`, `atis-latch` and its
`cost-state` line), and the runner must read the total that turn reported
(0.0112), which the CLI then restores; no rule had to be relaxed for it.
The transcript is printed, and kept in
`testkit/fixtures/transcript/tool-turn.jsonl` (four long attachment lines
left out) for a unit test that needs no Docker. The CLI runs as the
session directory's owner, as agentd runs the sandbox when it isn't root,
so the runner opens the files with the modes the CLI gave them; run as the
image's user, it wrote a transcript the non-root CI runner could not
open. Then the hand-built
shapes (no cost line, one, two, a file of exactly 5,242,880 bytes) must
read, and the review's attacks (`ledger_big`, `attr_big`, `leaf2`,
`leaf3`, a duplicated key, a ledger-prefixed line) must be unknown. On the
code before this, the test read 0.25 where the CLI restored 5 for
`ledger_big` and `attr_big`, and 5 where it restored 7 for `leaf2` and
`leaf3`.

All of that holds only if nothing changes the file between the runner's
read and the CLI's, and a process the agent leaves running in the
container can: when the agent kills its own CLI, the exit is confirmed
and the container kept, so the next requester's `--resume` ran in a
container where a background loop could wait for `resume <id>` to appear
and append a `cost-state` line of 900 after the runner read 5, billing the
next requester about $895. `SessionManager::ensure_process` now reads the
restored total only when it started the container in the same call:
nothing of the agent's runs in a fresh one before the CLI (its command is
`sleep infinity` from the image, the exec wrapper is the image's `sh`, and
the credential proxy and agentctl token are agentd's), and a session's
earlier container is stopped before another starts. A `--resume` in a
container an earlier process ran in (after a crash, a kill, or a
credential kind or model change) counts its first turn's cost as unknown.
A runner test resumes in such a container and gets no cost; before this
it got the turn's cost. Stopping the container before every resume would
keep that cost known at a container start's price, and would also end
the agent's leftover processes (Deferred work's "Killing leftover
processes at turn end").

A turn's cost is unknown (an `Err` with a `CostUnknown` reason, and
billed as 0) when its result or the process's previous one has no
plausible total (so a result without one leaves the next turn unknown
too, and the one after is known again), when the total falls, or when it
rises by more than `MAX_TURN_COST_USD` ($1,000). An unknown restored total
makes only the first turn's cost unknown. When that first result has no
total either, `no_total` wins and the restored total's reason isn't
counted. Even so, the agent can write the transcript and the CLI's stdout
(it runs as the same user), so `cost_usd` is a record, never something a
limit is enforced with: no cap reads it. The same holds for the reasons:
an agent can move a turn from one to another, so their counts are a
lower-trust figure than the turns and tokens.

`fake-claude` now appends a `cost-state` line shaped as 2.1.285 writes
it, starting with the same bytes, when its input ends, and none when it
crashes, and restores the session's last one on `--resume`;
a runner test checks the resumed turn's cost after a clean stop and after
a crash, and the Docker test now checks the corrected cost against the real
CLI in CI.

### Turns of unknown cost are recorded by reason

**Issue.** A review measured about 146 KB of transcript per CLI process
start with 2.1.285: an ~82 KB `prompt_snapshot` line and a ~12 KB
`skill_listing` line are written on every start, whatever the turn. So a
long-lived thread's transcript passes `CLI_INDEX_BYTES` (5 MiB) after
about 36 process starts at that rate, and after at most about 56 from
those two lines alone. From then on every resumed process's first turn
has an unknown cost and is billed nothing. That fails safe, but only a
warning log said so, and nothing counted what went unbilled.

**Solution.** A turn's unknown cost carries its reason, a `CostUnknown`:
`no_result` (crashed or timed out), `no_total`, `total_out_of_range`,
`reused_container`, `transcript_too_large`, `transcript_unreadable` or
`transcript_unrecognized`. The `usage` table has a `cost_unknown` column
in its key, `''` for turns of known cost, so each member's day has a row
per reason with its turns and tokens and no cost. What went unbilled is
`SELECT cost_unknown, SUM(turns), SUM(input_tokens + output_tokens) FROM
usage WHERE cost_unknown != '' GROUP BY cost_unknown`, and `me` still
shows the member's day and month over every row. The warning logs name
the reason too. A store test and a pipeline test (a turn whose CLI the
agent killed is recorded as `no_result`) check it. Metering at the
credential proxy (Deferred work) would bill these turns; until then an
operator can price them from their tokens.

### Cache reads aren't tokens the meter counts

**Issue.** A result's `usage` has four counts. Every API call of a turn
reads the whole conversation from the prompt cache, so cache reads grow
with a thread's length, not its work: a token budget counting them would
stop a long thread after a few turns.

**Solution.** The meter's input tokens are uncached input plus cache
writes, and its tokens are those plus output, for `usage`, `thread_usage`
and `me`. `usage.cost_usd` keeps the CLI's own figure, which prices every
kind.

### A turn is billed at least what its messages used

**Issue.** Only a result line carries a turn's usage, so a turn that
crashed or timed out, one whose CLI the agent killed itself included,
counted as a turn with no tokens, and a loop of such turns never reached
the thread's token budget. A result line that reports less than the turn
used would undercut it too.

**Solution.** `TurnStats::message_usage` adds up the `message.usage` of
the turn's `assistant` lines. The CLI prints one line per content block of
an API message, each with that message's usage so far; in the captures
their output counts are lower than the result's, and their input counts
add up to it. Subagents print their messages as `assistant` lines too,
with a `parent_tool_use_id` (2.1.285's source emits each subagent
message's blocks as they stream), so the lines of Task subagents running
at once come interleaved: `s1, s2, s1, s2`. A first version counted a
message once only across consecutive lines, and so counted such a turn
twice. `TurnStats` now keeps the latest 64 message ids with what each has
counted, and a line of a known id adds only what it raises that by; an id
pushed out by 64 others counts again, which only more than 64 messages
streaming at once would reach. `TurnOutcome::usage` is that sum for a turn
that crashed or timed out, and for a finished one each count the larger
of the result's and the sum, so a forged low result can't undercut what
was streamed first. The meter bills it.

No count of a turn is more than `MAX_TURN_TOKENS` (100 million, far above
what a turn writes, as `MAX_TURN_COST_USD` is above its cost): a count
past it in a `usage`, or in what a turn's lines add up to, counts as the
cap. Each count is held on its own. A first version dropped a whole
`usage` with any count past the cap, cache reads included, which a long
turn over a million-token context passes honestly: the result's figures
were lost for the streamed ones, which are lower, and an agent could
print such a result to be undercounted. Held at the cap, a forged count
can only raise the turn's figure, never take a real one out of the
thread's budget.

The agent runs as the CLI's user, so it can write to the CLI's stdout as
well as its transcript, and print whatever lines it likes. Token counts
are therefore the CLI's only as long as the agent leaves them alone: they
stop agents that loop by mistake, not one that means to overspend. The
turn caps (per thread and hour, and per agent and day) and the hop cap
count turns agentd starts itself, so they are the hard bounds on a loop.
Metering at the credential proxy, which sees every API response, would
make tokens and cost a bound too (the plan's Deferred work). The same
stdout predates T27 with a worse problem, which this task leaves there
too: a forged `result` line ends the turn early with the agent's text as
the reply, and the CLI's real result for the turn is then read as the
next turn's, so the next requester gets this turn's reply and pays its
cost and tokens. Reading turns from a channel the agent can't write
closes both.

### One table counts threads and agents

**Issue.** The plan's `thread_usage` has a day but no hour and no agent,
while the caps are turns per thread per hour and turns per agent per day,
and nothing else counts an agent's turns.

**Solution.** `thread_usage` rows are keyed by thread, day, hour (UTC) and
agent. The thread's turns this hour and tokens today, and the agent's
turns for others today (`others_turns`), are sums over it (an index on
`(agent_id, day)` serves the latter). Rows older than two days are swept
with the other expired rows. `usage` keeps a member's days for good, for
`me`'s month.

### Counts are read before a turn and written after it

**Issue.** The router is pure, so the limits compare counts the view
loaded before the turn, and a turn's tokens are known only after it.

**Solution.** The pipeline meters each turn right after the runner returns
it, before delivering the reply, in one transaction (`usage` and
`thread_usage`). Turns of one agent in one thread run one at a time, so
their counts are exact; turns running at once elsewhere (other threads,
or other agents in the same thread) can each pass a cap the others are
about to reach, so a cap can be passed by the turns in flight when it is
reached. The overshoot is bounded: one turn per agent in a thread at a
time, at most the pipeline's 64 places (16 for one owner's agents) in
all, and no more than the runner's container cap can run. A turn is billed
to its requester's member, created from the identity if the store has
none yet (a community-key turn of someone never seen before). A failure to
meter is logged; the turn has run.

### What the caps count, and what they don't promise

**Issue.** Several edges of the caps follow from counting per UTC calendar
day and hour, per thread, and per member, and the plan doesn't settle them.

**Solution.** They are kept, and written down here:

- Windows are calendar hours and days (UTC), not sliding ones, so a thread
  can take up to twice `thread_turns_per_hour` across an hour's boundary,
  and an agent twice its `turns=N/day` across midnight.
- The thread caps count every turn in the thread, whoever asked, so any
  member who may use an agent there can use up the thread's hour for
  everyone, the owner included. A new thread starts afresh; a one-to-one
  DM isn't capped.
- A turn that ends with an error result (a credential at its usage limit,
  an unreachable API) counts: it is a turn agentd started, and a loop of
  failing turns has to stop as well. A turn that crashed or timed out
  counts too, with what its messages used. Only a message the router
  refuses, or a turn that never reached the CLI, counts nothing.
- A limit's refusal can change between the event's routing and the
  platform's copy's (T31's confirmation), as a turn ends or a window turns.
  The pipeline used to drop such a message silently; it now acts on the
  copy's decision when either is a limit's refusal (`limited`), since the
  copy is the message as the platform has it, but only if both decisions
  name the same requester identity (`Decision::requester`'s key): a limit
  can change between the two routings, and so can the member an identity
  belongs to, as one is made for it, but who asked can't, so a copy that
  names someone else is dropped as any other difference is.

### The owner is never capped by their own agent's limit

**Issue.** The plan doesn't say whether `turns=N/day` limits the owner.

**Solution.** It limits requests from anyone but the owner, as allow and
deny do, and counts only those: a first version counted the owner's own
turns too, so an owner who used their agent used up what they had allowed
others. `thread_usage.others_turns` counts the turns whose requester isn't
the owner, hops included, and `Store::capped_turns_on` sums it for the
router. `turns=0` leaves the agent to its owner. `limits` takes `off`
for either setting, which the parser now reads as `Setting::Off`; without
it an owner couldn't remove a cap once set.

### Thread caps count every agent and skip one-to-one DMs

**Issue.** Loop protection needs caps across every agent in a thread, but
an owner's long working session in their DM with their agent is no loop,
and a one-to-one DM can't hold another agent.

**Solution.** `RouterView::thread_budget` gives the thread's spend and the
caps, and the router asks it only outside one-to-one DMs; `None` refuses
as `PolicyUnavailable`. The caps apply to every requester, the owner
included, since a chain the owner started loops as well as anyone's.
Refusals come after the hop cap and the daily cap and before the
credential. `[limits] thread_turns_per_hour` (default 30) and
`thread_tokens_per_day` (default 2,000,000) turn off at 0, and
`max_hops` (default 3) is the global hop cap.

### A capped agent says so once per thread and window

**Issue.** "Past the daily cap, reply once per thread per day" needs a
record of the reply that other instances and a restart see, and the
thread caps would otherwise answer every message with the same line.

**Solution.** `limit_notices` holds one row per agent, thread, kind and
window (the UTC day for the daily cap and the token budget, the hour for
the turn cap), claimed with an insert before posting and released if the
post fails, as `failure_notices` does. A pause, the hop cap and an
unreadable policy still post each time, as before.

### A refusal of the requester is told to them alone, once a day

**Issue.** A banned member's message, or one an agent's rules deny, drew a
public line in the thread, every time: a banned member mentioning k agents
had k lines posted per message, and the thread learnt who was banned or
denied.

**Solution.** `Decision::Refuse` now carries the requester, as the link
prompts do (for a hop, the requester the turn inherited), and
`RefuseReason::is_personal` marks `Banned` and `Denied`. For those the
pipeline sends the line to the requester from the manager bot, like a link
prompt, and only if the agent's bot could have answered there. It claims a
`failure_notices` row first, so the requester is told at most once per
`REFUSAL_DM_INTERVAL` (a day): about a ban once for all agents
(`refusal/banned`), and about each agent's rules once for that agent
(`refusal/denied/<agent id>`); a DM that fails releases the claim. The
kinds don't start with T26's `refused/`, which names a refused
credential. Those rows are kept like the other failure notices, one per
requester and kind, and T26's failure DMs now read the pipeline's clock
too, so the table sees one clock.

Only a requester who sent the message is told. On a hop the requester is
inherited from the turn whose post named the agent, and they never
addressed it: a first version sent them the DM anyway, so an attacker's
agent naming k agents that deny everyone had the manager send its
requester k messages about agents they never asked. A hop's refusal of
its requester is logged and told to no one; the thread isn't told either,
since that would say who is banned or denied, and would give a spamming
agent a line per agent it names.

### Allow and deny undo each other

**Issue.** T22 fixed what the rules mean (deny wins; a non-empty allow
list restricts), but no command removes a rule, so the owner had no way
back from a mistake.

**Solution.** `deny <target>` puts the target on the deny list and leaves
the allow list alone. Taking the target off the allow list too, as a first
version did, widened access: after `allow helper @bob`, `deny helper @bob`
emptied the allow list and opened the agent to everyone else. A deny must
never grant access, and deny wins anyway, so the allow entry stays.
`allow` puts the target on the allow list, so the first `allow` limits the
agent to its targets, and lifts a deny of it. A first version only lifted
the deny of a denied target, so under an allow list that didn't hold it
`allow helper @bob` still left bob out, and took a second `allow`; now a
denied target is added to the allow list too when the list is not empty,
and only lifted when it is, so lifting a deny never limits an agent open
to everyone. A full allow list refuses the command whole, deny included.
Replies leave a denied target out of the allowed ones, and an allow list
whose every target is denied reads "Only you may use". `allow everyone`
empties the allow list and takes `everyone` off the deny list; denies by
name stay. An `allow` of a member or a channel while `everyone` is denied
changes nothing anyone can see, so its reply says to send `allow <name>
everyone` first. Each list holds at most 100 rules. Rules are JSON agentd
owns (`policy::Rule`), with the identity or conversation and how the owner
wrote it; replies name them in code spans, which neither surface turns
into a mention. A member is resolved as `list` does (Slack sends an id,
Rocket.Chat a username the manager looks up). A channel on Slack arrives
as an id token, whose name after `|` the reply shows. On Rocket.Chat the
manager looks it up with `rooms.info`, by `roomName`
(`RestClient::room_by_name`, and `FakeRest` answers it), or by `roomId`
for an id typed as a `<#…>` token, which is never taken as it is. The
manager can read private groups a member may not be in, so only a public
channel is found: a private group reads as unknown whether or not it
exists, and can't be named in a new rule on Rocket.Chat yet. A room the
lookup doesn't find is still matched against the agent's own room rules by
the name the owner wrote (`#secret`), so a channel denied or allowed while
public and made private since can be allowed or denied again; that tells
the owner only what their own rules hold. Checking the asker's membership
of the group would take a lookup of another user's rooms the manager
doesn't make yet. Rules that don't read refuse everyone, the owner too, as
`PolicyUnavailable`, since the same row holds the hop cap, and `allow
<name> everyone` clears them.

### An agent's settings change in one transaction

**Issue.** `limits`, `allow` and `deny` read an agent's settings, changed
them and wrote them back as separate queries, so two commands sent at once
could each write over the other's change: of eight `allow`s sent together,
one was kept.

**Solution.** `Store::update_agent_settings` reads the row, applies the
command's change, a plain function of the settings, and writes it in one
`BEGIN IMMEDIATE` transaction, and replaces `put_agent_limits` and
`put_agent_rules`. Resolving the target (a user or room lookup) happens
before it, outside the transaction. `Rules::write` puts the lists in the
settings; serializing a rule can't fail, and if it ever did the list
would be empty text, which doesn't read and so refuses everyone.

### A ban leaves what only takes away, and still warns about a leaked secret

**Issue.** A ban blocks a member's commands, but a
secret-bearing command sent in a channel is refused with advice to revoke
the secret, which a banned member needs as much as anyone.

**Solution.** `Commands::run` refuses a banned member's command before it
runs, except the commands that only take something away from them (`me`,
`logout`, and `pause` and `delete` of their own agents) and a
secret-bearing command sent where others can read it. `admin ban @member
[reason]` bans the member the identity belongs to, creating it if needed,
so every identity they link is covered; an identity of theirs they never
linked is another member to agentd, and isn't. The reason is at most 500
characters and shown to them by `me`, never logged. Admins, matched by
identity as T26 does, can't be banned, and neither `Commands::run` nor
the router holds an admin back (the pipeline's view never counts an
admin's identity as banned, as sender or as a hop's requester, and `me`
doesn't say they are), so a ban row left on an admin's member (made
before they were listed) can't lock the community out of `admin unban`
or silence the admin's requests. Deleting a
member deletes their ban (`ON DELETE CASCADE`). The router's view loads
bans for every member it knows (the sender's, the attributed requester's
and its key's) and fails closed if the store can't say.

A banned owner's agents go on answering others. A ban limits what the
member may ask for and change; each of their agents' turns runs on its own
requester's credential, and the agent stays as its owner left it, since
the owner can't change it while banned, only pause or delete it. An admin
who wants such an agent silent asks the owner to pause it, or removes its
bot on the platform.

### The usage migration was edited in place

**Issue.** `thread_usage.others_turns` and then `usage.cost_unknown`
were added to `20260930240000_usage.sql` after the branch's first push,
by editing the
migration rather than adding another, since T27 hadn't merged. sqlx keeps
each applied migration's checksum and refuses to start on a database whose
applied migration has since changed.

**Solution.** Nothing migrates such a database: one that ran an earlier
version of this branch's migration must be recreated (a development store
only; no release shipped it).

### The proxy's path allowlist stays deferred

**Issue.** T26 left an allowlist of Anthropic API paths for the credential
proxy open.

**Solution.** Nothing assigns it to T27: it is still in the plan's
Deferred work, waiting for a live capture of the paths the CLI uses, so
this task leaves it.

## T28: Slack ingress

### The manager binding needs a `BindingId`

**Issue.** `InboundEvent::binding` is a `BindingId`, a UUID, but the plan
gives the manager app the fixed path segment `manager`, and the manager has no
row in `agent_bindings` (its secret comes from configuration).

**Solution.** `surface_slack::BindingRef` is `Manager` or `Agent(BindingId)`,
parsed from the path: `manager`, or a binding id in canonical lowercase form;
anything else is 404 without a lookup. Manager events carry
`BindingRef::MANAGER_ID`, the nil UUID, which agentd never mints for an agent
(the parser refuses it as an agent path too). Deduplication sources use the
path form, so the manager's are `slack:manager…`.

### Signing secret and bot user come from one lookup

**Issue.** The plan's `SigningSecrets` trait returns a secret, but
normalization also needs the binding's bot user id to keep channel messages
that mention it, and T31 needs a binding in state `creating` to answer
`url_verification` before any secret exists.

**Solution.** `SigningSecrets::lookup(BindingRef)` returns
`Option<SlackApp { signing_secret: Option<SecretString>, bot_user:
Option<UserId> }>`. `None` is 404. A known binding without a secret answers
only the challenge, and everything else gets 401. Without a bot user, channel
messages pass only as thread replies; the manager has none until T30 reads it
with `auth.test`, which doesn't matter while it subscribes only to
`message.im`. agentd's `ConfigSigningSecrets` knows only the manager; T31
adds the store-backed agent bindings in front of it.

### Where the ack ends and processing begins

**Issue.** The plan says handlers enqueue and return 200 at once, and
deduplicate through the store. A store write before the ack could wait up to
SQLite's 5-second `busy_timeout` and miss Slack's 3 seconds, and the plan
doesn't say what a full queue does.

**Solution.** The handler does only what needs no I/O beyond the secret
lookup: read the body (at most 1 MiB), verify, parse, and `try_send` into a
bounded queue. A full or closed queue answers 503; Slack retries an event
that gets one, but not a slash command or an interaction, whose user sees
Slack's error. The handler never waits for the queue. `Queue::run` then deduplicates through the
`Dedup` trait (agentd's `StoreDedup` over `mark_event_processed`), normalizes,
and sends `SlackInbound` items, one at a time and in order, to a
`core_types::Sender`. A failed dedup write drops the request rather than risk
a duplicate turn. Until T29 and T30 consume it, agentd's sink (`Unrouted`)
logs each item's binding and kind and drops it. agentd runs the queue as a
`server::Worker` next to the listeners: `Routers` gained a `workers` field,
and the queue ends once the public listener's router is dropped, so every
acknowledged request is handled within the drain timeout. An acknowledged
request is lost if agentd dies before handling it; Slack won't retry it.

### Replays inside the five-minute window

**Issue.** Signature verification with a five-minute window still lets a
captured request be replayed within those minutes. Events are covered by
`event_id` deduplication, but slash commands and interactivity have no id.

**Solution.** Commands and interactions are deduplicated by their signature,
lowercased (the verifier accepts either hex case, so an uppercased copy would
otherwise pass), under `slack:<binding>:request`. Slack doesn't retry them, so
a second copy is never legitimate. Timestamps are also refused when more than
five minutes in the future, not only in the past.

### Current Slack apps post without a subtype

**Issue.** The plan ignores every subtype but `file_share` and
`thread_broadcast`, and describes bot events without a `user` field. In the
payloads of Slack's SDK test suites (`slackapi/bolt-python`
`tests/scenario_tests/test_message_bot.py`), a current app's bot post has no
subtype, with `bot_id`, `bot_profile` and its bot user in `user`; the
`bot_message` subtype, without `user`, is for classic integrations and
`response_url` posts.

**Solution.** Kept as the plan says: agent posts arrive with no subtype and a
`user`, and `bot_message` is ignored. The "no `user`" rule still applies to a
bot event that passes the subtype filter (a fixture covers one). T32 should
record which shape another agent's post has.

### Mentions typed inside a rich-text block

**Issue.** "Mentions come from `<@U…>` tokens in the text and in `blocks`"
could be read as scanning every string in the blocks. In a `rich_text` block,
a member who types `<@U123>` literally gets a `text` element holding it,
while a real mention is a `user` element (and the message `text` escapes the
literal as `&lt;@U123&gt;`).

**Solution.** Mentions are the tokens in `text`, the `user` elements of
`rich_text` blocks, and the tokens in `mrkdwn` text objects (section and
context blocks, which bots post). `plain_text` and rich-text `text` elements
are not scanned. Each user appears once, in order of first appearance; the
ids already seen are kept in a `HashSet`, since a 40,000-character message
can carry thousands of mentions.

### A misspelled manager secret went unnoticed

**Issue.** T10 accepts any `AGENTD_SLACK_MANAGER_<NAME>`, so a misspelled
`…_SIGNING_SECRET` would silently leave the manager binding unknown.

**Solution.** When any `AGENTD_SLACK_MANAGER_*` variable is set,
`AGENTD_SLACK_MANAGER_SIGNING_SECRET` must be too; the error asks whether one
is misspelled. The manager is known exactly when the secret is set, and
agentd logs at startup which it is.

T10's rule for other `AGENTD_*` variables still applies around it: near
misses of the prefix are refused, and names nobody reads land in
`Config::unknown_env`. Kubernetes service links are now recognized before
the Slack prefix, because a Service named `agentd-slack-manager` would set
`AGENTD_SLACK_MANAGER_PORT` and `…_SERVICE_HOST`; read as secrets, those
would fail this check (or become junk entries next to the signing secret).
No Slack secret's name ends like a service link.

### Slack's `ssl_check` is unsigned

**Issue.** The plan lets only `url_verification` skip the signature. Slack
also posts `ssl_check=1` (with the legacy verification token) to a slash
command's URL to check its certificate, unsigned; agentd answered it 401, or
400 when signed, since it isn't a command form. Bolt for JavaScript and for
Python answer it with 200 before verifying.

**Solution.** On `/commands`, a known binding answers a form whose
`ssl_check` is exactly `1` with an empty 200 before the signature check,
reading nothing else and queueing nothing, like the challenge echo. The
design's transport bullet and the plan name it as the second exception.

### Unaddressed messages cost a store write each

**Issue.** Agent apps receive every message in their channels, and
`Queue::run` recorded each event's `event_id` in `processed_events` (kept for
seven days) before normalization dropped the unaddressed ones: a store write
per channel message per agent.

**Solution.** `message` events are normalized first, which is pure, and a
dropped one costs no I/O. A kept message is deduplicated only by
`<channel>:<ts>` under `slack:<binding>:message`, which catches Slack's
retries as well as the event_id key did, so messages no longer write an
`event_id` row. Other events are still deduplicated by `event_id`.

### A slow body held up shutdown

**Issue.** Nothing bounded the secret lookup or the body read before the
ack. A client that sent headers and then trickled or withheld the body kept
its connection in flight, so a graceful shutdown waited the whole drain
timeout for it.

**Solution.** The handler every route goes through gives the lookup and the
body read one shared deadline, `PRE_ACK_TIMEOUT` (2 seconds, inside Slack's
3): 503 if the lookup is still running, 408 if the body hasn't arrived.

### Refusals before verification are throttled in the log

**Issue.** Anyone can send unsigned or forged requests and challenges, and
each was logged at warn or info, so a flood of them floods the log.

**Solution.** Like agentd's `RefuseSubnet`, the ingress logs such refusals
(bad signature, no secret yet, body refused or too slow) as a warning at most
once per `WARNING_INTERVAL` (a minute), with how many went quiet since, and
the rest at debug level. Answered challenges are throttled the same way at
info level. `ssl_check` is logged at debug level only.

### A trailing newline in a secret failed every request

**Issue.** T10's `secret()` refused only empty or all-white-space values. A
signing secret mounted from a file with a trailing newline was accepted, and
every Slack request then failed verification with 401.

**Solution.** Every secret read from the environment (`AGENTD_MASTER_KEY`,
`AGENTD_RC_MANAGER_TOKEN` and `AGENTD_SLACK_MANAGER_*`) is refused at startup
when it starts or ends with white space; the error names the variable, never
the value. That includes the master key, whose base64 decoding (T05) would
have ignored the newline: the rule is simpler kept the same for all secrets,
and `export AGENTD_MASTER_KEY="$(agentd gen-key)"` strips the newline anyway.

## T29: Slack Web API

Method parameters, response shapes and rate-limit tiers were read from
Slack's SDKs (`slackapi/python-slack-sdk` `slack_sdk/web/client.py` and
`internal_utils.py`, `slackapi/java-slack-sdk` `MethodsRateLimits.java` and
`MethodsRateLimitTier.java`), since Slack's own documentation site wasn't
reachable. None of it has run against real Slack yet.

### `Surface::render` has no directory to pass

**Issue.** T23 has the pipeline build a `MentionDirectory` snapshot from
agent bindings and the surface's member cache before rendering, but
`Surface::render(&self, markdown)` takes no directory, and the pipeline only
holds a `dyn Surface`, so it can reach neither Slack's member cache nor a
way to pass a snapshot in.

**Solution.** `SlackSurface::render` resolves `@Name` through its workspace's
member cache as last read. `users.list` lists bot users too, so agents'
names resolve without the bindings. `render` never blocks: when the cache is
older than its TTL (15 minutes by default) it starts a refresh on the
current Tokio runtime for the next call. agentd (T31) awaits
`SlackSurface::refresh_members` when it starts a binding, so the first reply
already has names, and gives each team's `TeamDirectory` its agents' bot
user ids with `set_managed_bots` (see below). `render_with` takes an
explicit directory for callers that have one. T23's delivery step, T29's
member-cache bullet and T31's receiver bullet say so.

### The ingress can't look bots up

**Issue.** A bot message without a `user` needs `bots.info` to fill
`sender.user` and `sender_bot_user`, but T28's `Queue` normalizes events for
every binding without any bot token.

**Solution.** `SlackSurface::fill_bot_sender(&mut InboundEvent)` does it with
the receiving binding's token, caching the answer per team and bot id
(`bot_not_found` and a bot without a user are cached as "no user"; other
errors are not cached and leave the event unchanged). The receiver of
`SlackInbound` calls it before routing; T31's deliverables in the plan now
say agentd's Slack receiver does. `history` names bot senders the same way.

### Request encoding and the upload flow

**Issue.** Slack accepts JSON bodies for some methods and only forms for
others, and the plan didn't say how the upload step of the external upload
flow authenticates.

**Solution.** As in the Python SDK: `chat.postMessage`, `chat.update` and
`chat.postEphemeral` send JSON (`application/json; charset=utf-8`); every
other method sends a form POST, which Slack accepts for read methods too.
The token is only ever in `Authorization: Bearer`. Posts send
`unfurl_links: false` and `mrkdwn: true`, never `link_names` or `parse`
(tested on the request bodies).

The upload is `files.getUploadURLExternal` (`filename`, `length`) per file,
a POST of the bytes to the returned `upload_url`, then one
`files.completeUploadExternal` with `files` as a JSON array of
`{id, title}`, `channel_id` and `thread_ts`. Both the Python and the Node
SDK post the file's raw bytes to `upload_url`, the Python one with no token
at all (Node adds one only for a per-call token), so the client posts raw
bytes as `application/octet-stream` without the bot token, and treats
`upload_url` as a secret since it is presigned. No multipart form is
needed, so the crate doesn't enable reqwest's `multipart` feature. If any file fails, nothing is completed, so
nothing is shared. Files are read into memory whole, which is fine for
staged attachments but not for very large files.

### Rate limits: tiers and 429s

**Issue.** The plan asks for a per-token limiter by tier and for honoring
`Retry-After`, without saying what a long `Retry-After` does to later calls.

**Solution.** Each method carries the Java SDK's tier: Tier 2 (20 per
minute: `users.list`, `reactions.remove`), Tier 3 (50: `conversations.*`,
`chat.update`, `reactions.add`, `bots.info`), Tier 4 (100:
`chat.postEphemeral`, `users.info`, `files.*`), `auth.test` (600) and
`chat.postMessage` (60 per minute per channel). The limiter keeps, per
token digest and method (and channel for `chat.postMessage`), the times of
the calls in the last minute, and a call waits while the quota is used up.
It is in memory and per process, which is enough to stay under Slack's
limits; Slack's 429 remains the authority.

A 429, or `ok: false` with `ratelimited`, blocks that bucket until
`Retry-After` has passed, so concurrent callers wait too. A call is retried
up to three times while the wait is at most `max_retry_wait` (60 s by
default); a longer wait fails at once with `SurfaceError::RateLimited`, and
so does any later call in that bucket while it stays blocked, instead of
sleeping silently. `Retry-After` is read as whole seconds and capped at a
day so it can't overflow a deadline.

### Error codes Slack answers with HTTP 200

**Issue.** Slack reports failures as `{"ok": false, "error": "<code>"}` with
HTTP 200, and some codes aren't failures for agentd.

**Solution.** `web::map_error`: `invalid_auth`, `not_authed`,
`token_revoked`, `token_expired` and `account_inactive` are `Unauthorized`;
`missing_scope` (with the scope from `needed`), `not_in_channel`,
`is_archived`, `cant_update_message`, `edit_window_closed`,
`restricted_action*`, `method_not_supported_for_channel_type` and similar
are `Forbidden`; `channel_not_found`, `message_not_found`,
`thread_not_found`, `user_not_found`, `bot_not_found`, `file_not_found` and
similar are `NotFound`; anything else is `Api` with the code. A code is
kept only if it is at most 64 lowercase letters, digits and underscores, so
an error never carries arbitrary response text. `already_reacted` from
`reactions.add` and `no_reaction` from `reactions.remove` count as success.
A non-2xx status other than 429 is `Api("HTTP <status>")`, an unreadable
body is `Transport`, and redirects are never followed. Transport errors drop
the request URL, so a `response_url` or upload URL can't leak through one.

### Names two members share

**Issue.** Display names aren't unique in Slack, and the plan didn't say
what an `@Name` shared by two members resolves to.

**Solution.** The member directory maps each active member's display name
and full name, compared ignoring case and runs of white space, and a bot
user's username too. A human's username is left out: it is often the local
part of their email address, and would let a human shadow an agent called
the same. A name that belongs to more than one member resolves to no one,
so it stays text: a missed mention is better than pinging the wrong person.
The exception is agents: agentd knows its agents' bot user ids from the
bindings and passes each team's to `TeamDirectory::set_managed_bots`, and a
shared name with exactly one managed agent among its members resolves to
that agent. The directory keeps every member id per name, so a new managed
set applies to the current list at once, without reading `users.list`
again. A managed bot the list lacks, such as an agent installed since the
last read, marks the list stale instead, so the next `refresh_members` or
render reads `users.list` even within the TTL (T31's `refresh_members` when
a binding starts would otherwise return the cached list without the new
agent for up to 15 minutes). That still goes through the one shared
refresh and honours the wait after a failed read. If the bot is set while
a read is running, the list that read produces stays stale too, since the
read may have started before the install. A bot still missing afterwards
causes no further reads until the managed set changes again. Deactivated
members are left out.

If a refresh fails, the next attempt waits a minute (or the TTL, if
shorter). Meanwhile an older list is kept; with none, `refresh_members`
returns the same error without calling Slack and `render` starts no
refresh, so a workspace whose `users.list` fails doesn't get one call per
rendered reply. `Debug` on `SlackSurface`, `TeamDirectory` and
`MemberDirectory` shows the team and counts, never member names or ids.

### Refresh cost in large workspaces

**Issue.** `users.list` is Tier 2 (20 calls a minute per token), and a
refresh reads every page, so a large workspace's refresh is slow.

**Solution.** `users.list` asks for 999 members a page, the largest size the
client asks any list method for; Slack's spec gives `users.list` no maximum
and says it may return fewer than asked. At a full 999 a page a refresh of
N members takes about N / 20,000 minutes of that token's `users.list` quota:
a 10,000-member workspace needs 11 calls, and 100,000 members about 101
calls, five minutes of waiting in the limiter. That stays well inside the
default 15-minute TTL, runs in the background after the first load, and
uses one binding's token per team, so other bindings' quotas are
untouched. If Slack returns much smaller pages, the refresh takes
proportionally longer; a very large workspace should raise the TTL with
`TeamDirectory::with_ttl`.

### Reading a thread's newest messages

**Issue.** `Surface::history` wants the newest `limit` messages before a
cursor, but `conversations.replies` pages from the thread's oldest message
(root first) and has no reverse order.

**Solution.** A thread read follows every page (with `latest` set to the
cursor and `inclusive=false`) and keeps only the last `limit` content
messages, filtering by `ts` on the client too, and skipping a `ts` it has
already kept, in case a later page repeats the root. `conversations.history`
pages from the newest, so a top-level read stops once it has `limit`. Both
always ask for 200 a page, never just the count still missing: skipped
joins and edits would otherwise cost a call each.
Content means no subtype, or `file_share`, `thread_broadcast` or
`bot_message`; joins, edits and tombstones are skipped.

### Smaller choices

- `SlackSurface::events` returns `Unsupported("events")`: Slack pushes
  events to T28's ingress, and no surface loop exists to run.
- A conversation or message from another workspace, or another surface, is
  refused before anything is sent.
- `auth.test` gives a bot token's team, bot user and `bot_id`, but not the
  app's id or name; T30's `/agent me` can get the app id and bot name from
  `bots.info` on that `bot_id`.
- `respond_ephemeral` posts `{"response_type": "ephemeral", "text": …}` to
  the `response_url` with no token. Slack answers `ok` (text or JSON) on
  success; `expired_url`, `used_url`, 404 and 410 are `NotFound`.

## T30: Slack manager app and configuration token

Slack's documentation site wasn't reachable, so the shapes below were read
from Slack's SDKs: `tooling.tokens.rotate` from `slackapi/python-slack-sdk`
(`slack_sdk/web/client.py`, which sends `refresh_token` as a form field) and
`slackapi/java-slack-sdk` (`ToolingTokensRotateResponse`, `RequestFormBuilder`,
`MethodsRateLimits`), and the manifest's keys from the Java SDK's
`AppManifest`. None of it has run against real Slack yet.

### One agentd serves one workspace

**Issue.** The plan says to install the manager app "once per workspace",
but its secrets are configuration: one signing secret, one bot token, and the
fixed binding `manager`. Nothing in the plan said which workspace that is,
and identities and replies need its team id.

**Solution.** agentd serves the workspace the bot token belongs to. The
signing secret now requires `AGENTD_SLACK_MANAGER_BOT_TOKEN` and the other
way round, and `App::open` asks Slack who the token is before serving:
`auth.test` gives the team, the bot user and the bot, and `bots.info` on the
bot gives the app's id and name (T29's note said `auth.test` names no app).
If Slack refuses or can't be reached, agentd doesn't start; the error names
the variable, never the token. A new, optional `[slack]` section has one key,
`api_url`, so tests can point agentd at a fake Web API. The ingress now keeps
channel messages that mention the manager's bot user, which no longer
matters to the manager app itself, since it subscribes only to `message.im`.

A signed request only proves it came through the manager app, which a
workspace admin elsewhere could install from the same manifest. So
`slack::Inbound` drops, with a debug line, any command, message or event
whose workspace (`SlackInbound::team`: the sender's, the conversation's or
the envelope's `team_id`) isn't the manager's, or that names none; before,
a slash command from another workspace could register a configuration
token there.

### A configuration token is checked by rotating it

**Issue.** The plan offers two checks, `auth.test` with the configuration
token or `tooling.tokens.rotate` at once. Whether `auth.test` accepts a
configuration token isn't in the SDKs, and it wouldn't show that the
refresh token works.

**Solution.** `/agent slack-token` rotates at once with the refresh token,
which proves it works and returns a fresh pair valid for 12 hours, and stores
that pair; the token the member typed is never used or stored. The call
sends no `Authorization` header and the refresh token only in the form body.
The answer's `team_id` and `user_id` must be the sender's own workspace and
user, since agentd would otherwise create apps as someone else or in another
workspace. A mismatch has already used up the refresh token, so the reply
says to generate a new one. Only a linked member on Slack may register a
token. `ConfigToken` and the store's token types keep both tokens as
`SecretString`, and a captured-log test at `trace` finds neither, nor the
`response_url`.

`tooling.tokens.rotate` is Tier 1 in the Java SDK ("special" per its own
comment). The limiter's new `Tier1` allows 5 calls a minute per bucket, and
a rotation's bucket is keyed by its refresh token, which is single use, so
the limiter never delays two different tokens.

### Rotation is leased and versioned

**Issue.** Refresh tokens are single use. Two instances rotating the same
token would leave one with a refused refresh token, and a rotation finishing
after the member registered a new token would overwrite it with the old
grant's successor, as a stale Claude refresh could (T09).

**Solution.** `slack_config_tokens` has the plan's columns plus `version`,
`updated_at`, `lease_until`, `broken_at`, `notified_at` and
`notice_attempts`. Every write of the tokens sets a new random `version`,
and every call that acts on a row read earlier takes the version it read
(`SlackConfigTokenRef`), so it changes nothing once the member registered
again. A random value rather than a counter, because a counter per row
starts over when the row is deleted and stored again. The rotator claims a
due token with a conditional `UPDATE` that sets a 5-minute lease
(`ROTATION_LEASE`), rotates, and stores the new pair, which ends the lease.
A process that dies after Slack rotated but before the store was updated
loses the new pair; the next rotation is refused, and the member is told to
register a new token. The encrypted columns' associated data is the row's
`member_id:team_id`.

Once Slack has rotated, the old refresh token is used up and the new pair
exists only in memory, so a store write that fails once would lose the
token. Both writers, `/agent slack-token` and the rotator, go through
`store_rotated`, which tries the write 4 times (`STORE_ATTEMPTS`), waiting
250 ms, then 500 ms, then 1 s. If the last try fails, the command tells the
member that the refresh token is used up and to generate a new one.

Nothing fences a write after the claim's lease ran out, so the rotator
keeps within it: `tooling.tokens.rotate` gets `ROTATE_TIMEOUT` (2 minutes,
rate-limit waits included), after which the claim counts as a failed
renewal. Should a rotation still finish after its lease, a second instance
has claimed the same row and, with the used refresh token, marked it broken
(the version doesn't change on a break). The late writer's pair is the one
that works, so `update_rotated_slack_config_token` also clears `broken_at`,
`notified_at` and `notice_attempts`, and the token is renewed again. A
rotation whose row was replaced or deleted meanwhile (`/agent logout`, a
`user_change`) stores nothing and drops its pair.

### Which failures a member hears about

**Issue.** The plan says the rotation loop "DMs the member on failure". Most
failures (a timeout, a 5xx, a rate limit) say nothing about the token, and a
DM for each would come every few minutes.

**Solution.** Only a refused refresh token breaks the token:
`invalid_refresh_token`, which now maps to `SurfaceError::Unauthorized`, or
any other code that does. The row is marked broken, and its member is owed
one DM from the manager app, claimed with a 10-minute lease
(`NOTICE_LEASE`) and tried at most 20 times, like the relink notice; a
member no manager bot reaches waits unclaimed. Any other failure keeps the
claim's lease, so the token is tried again when it ends, well within the
2 hours it still has. `/agent me` shows whether the token is registered,
renewed automatically, refused, or expired because renewing it keeps
failing; the last says agentd keeps trying and to register a new token if
it lasts.

### One command intake for every surface

**Issue.** T13's `CommandIntake` took commands only from Rocket.Chat events,
through a `CommandFeed` that knew the Rocket.Chat manager's binding, and
`Server::run` built it only with `[rocketchat]`. Slack's commands come from
the Slack queue, which `Routers::new` builds before `run`.

**Solution.** `commands::intake::CommandIntake` takes `(member, text,
origin)` through a `CommandSubmitter`, keeping T13's ordering per member and
its drain at shutdown. `CommandFeed::new(submitter, binding)` is the
Rocket.Chat side. `Routers` carries the intake and one submitter; the Slack
queue's `slack::Inbound` sink holds another. `Server::run` runs the intake
always, hands the submitter to the Rocket.Chat connection, and drops its own
copy when shutdown starts, so the intake finishes what it received once the
queue and the connection stop. `slack::Unrouted` is gone: `Inbound` passes
the manager's slash commands and DMs to the intake, deletes the token of a
member a `user_change` says was deleted, and drops everything else until
T31 routes agents' messages.

### Slack replies and entities

- `Origin::SlackDm { channel }` is new: a DM to the manager app is private
  and answered in the same DM, and replies there name commands bare
  (`login <code>`), as on Rocket.Chat.
- A slash command's reply is the rendered Markdown, each chunk sent to its
  `response_url` with T29's `respond_ephemeral`. Slack accepts five
  responses per URL; command replies are one chunk.
- Notices (relink, broken token) open the manager's DM with
  `conversations.open` (new in `WebApi::open_dm`, Tier 3, needs `im:write`),
  so relink notices now reach Slack-only members too.
- `surface_slack::normalize::unescape` decodes `&amp;`, `&lt;` and `&gt;`
  in one pass. It is applied to command text only (slash commands and
  manager DMs), before `commands::parse`, as T08's note expected. Message
  text for turns stays escaped: decoding it would make a literal `<@U…>`
  a member typed look like a mention token to anything that reads mentions
  from text later.
- A `user_change` deletes the token for the event's workspace only (the
  envelope's `team_id`, which must be the manager's), since the member may
  still be in another workspace; `/agent logout` deletes the
  member's tokens in every workspace. Neither revokes the token at Slack:
  the SDKs have no revoke method for configuration tokens, and whether
  `auth.revoke` accepts one is unverified.

### Files in the manager DM wait for their handlers

**Issue.** The plan says files attached to a manager DM feed `persona`
(T14's upload rule) and `skill add` (T25), downloaded with the manager's bot
token. Neither handler is in this stack yet; both commands answer "isn't
available yet".

**Solution.** T30 adds the download, `WebApi::download_file(file,
max_bytes)`: it sends the bot token only to an `https` URL on `slack.com` or
a subdomain (or the API URL's own origin, for tests), follows no redirects
(Slack redirects a request it refuses to its sign-in page), checks the
declared size and the `Content-Length` before reading, and stops reading
past the limit. Passing the DM's files to the handlers is left to T14 and
T25, whose plan text now says so.

### The manager's events URL before its secret is set

**Issue.** Slack verifies the events request URL when the app is created
from the manifest, but agentd knows the `manager` binding only once
`AGENTD_SLACK_MANAGER_SIGNING_SECRET` is set (T28), which Slack shows only
after creation, so the first challenge gets 404.

**Solution.** The README's install steps say to retry the verification
under "Event Subscriptions" once agentd runs with the secrets. Whether
Slack's "From a manifest" flow creates the app anyway and leaves the URL
unverified, as expected, is part of the live check.

### The manifest template

`deploy/slack/manager-manifest.yaml` uses `${PUBLIC_URL}` as its only
placeholder, which is a valid YAML plain scalar (a `{{…}}` placeholder would
start a flow mapping) and fills in with `envsubst '$PUBLIC_URL'`. Besides
the plan's scopes and events it turns the app home's messages tab on and
its read-only mode off, or members couldn't DM the app, and sets
`should_escape: true` on `/agent`, which delivers mentions as `<@U…|name>`
tokens that T08's parser reads. agentd's image build context leaves out
`deploy/`, so agentd doesn't embed the template; the tests read it with
`include_str!` and parse it with `serde_norway` (MIT OR Apache-2.0, a
maintained fork of the deprecated `serde_yaml`; with `unsafe-libyaml-norway`,
MIT, it is a dev-dependency of agentd only).

## T31: Slack agent apps from manifests

Slack's documentation site wasn't reachable, so the API shapes below were
read from Slack's SDKs: `apps.manifest.create` and `apps.manifest.delete`
from `slackapi/python-slack-sdk` (`slack_sdk/web/client.py`, whose
`params` end up in the form body, per `base_client.py`) and
`slackapi/java-slack-sdk` (`AppsManifestCreateResponse`, `AppCredentials`,
`MethodsRateLimits`), and `oauth.v2.access` from the same two
(`OAuthV2AccessResponse`, and the Python client's Basic authentication).
None of it has run against real Slack yet.

### What the app calls send

**Issue.** The plan names the methods but not how they authenticate or what
they return.

**Solution.** `SlackClient::create_app` and `delete_app` send a form, the
manifest as a JSON string in `manifest`, with the member's configuration
token only in `Authorization: Bearer`. `create_app` returns the `app_id`
and the `credentials` object's `client_id`, `client_secret` and
`signing_secret` (the legacy `verification_token` is ignored), with both
secrets as `SecretString`. An `ok: true` answer that names an app but can't
be read in full has the app deleted again with the same token, since
nothing could use it. `install_app` (`oauth.v2.access`) sends `code`
and `redirect_uri` in the form and the app's `client_id:client_secret` in
`Authorization: Basic`, as the Python SDK does, never in the body; it
refuses an answer whose `token_type` isn't `bot` or that lacks the token or
the bot user, and returns the granted scopes from its comma-separated
`scope`. `WebApi` now holds an `Auth` (none, bearer or basic) instead
of an optional bearer token. The Java SDK tags both manifest methods Tier 1
and `oauth.v2.access` Tier 4. `apps.manifest.delete` answers
`app_not_found` or `invalid_app_id` for an app that is already gone; for
that method both are `NotFound`, which counts as deleted.

Slack explains an `invalid_manifest` in an `errors` list of `message` and
`pointer` entries. Any failure that carries one has at most five entries,
each cut to 200 printable ASCII characters without backticks or angle
brackets, logged as a warning. An HTTP 5xx from Slack, on any call, is now
`SurfaceError::Transport`, which callers retry or report as "try again",
rather than `Api`, which read as Slack refusing the request.

### The install link's `state` is sealed, and used once through the binding

**Issue.** The plan asks for a signed `state` naming the binding, and for a
forged or replayed one to be refused, without saying with which key or how
a replay is noticed.

**Solution.** `Store::install_state` is `<binding id>.<base64url>`, where the
second part is a constant sealed with the master key (ChaCha20-Poly1305)
with the binding id in the associated data, so it can't be made without the
key or moved to another binding. The store already owns the key, so there
is no new key material. A replay is refused by the binding's state: the
callback acts only on a `pending_install` binding and makes it `active`,
conditionally, so a second callback, or two at once, installs once (409).
The answer of `oauth.v2.access` must also name the binding's app and the
workspace agentd serves. The state doesn't expire: the link stays good for
as long as the app waits, which an approval can make days. Anyone in the
workspace who has the link can complete the install, which gives agentd the
same bot token the owner's click would; the owner is told either way. The
installer (`authed_user`) isn't compared with the owner, since an admin may
install the app after approving it.

The link is posted as a Markdown link, `[Install <name>](<url>)`, which the
renderer turns into `<url|Install name>`, and `state` is its first query
parameter, with `redirect_uri` last. A bare link ending in `state` broke
about once in 64 installs: base64url ends in `_` or `-` that often, and the
renderer trims trailing punctuation off a bare URL, as qm-core does. The
link now ends in the letters of `/slack/oauth/callback`.

### The install link asks for the scopes and redirect URL the app was made with

**Issue.** `[slack] public_posting` adds `chat:write.public` to new apps. A
reminder built from the current configuration would ask for a scope the
app's manifest lacks once the switch changed; likewise a changed
`[slack] public_url` would break every pending install, since the redirect
URL must be one the manifest names.

**Solution.** A migration (`20260930230000`, after T26's two) adds
`agent_bindings.app_scopes` and `app_redirect_url`, the scopes and redirect
URL the manifest asked for, stored when the app is. Every link for the app
(the first and the reminder) asks for exactly those, and the callback's
`oauth.v2.access` repeats that redirect URL. The callback also refuses an
install whose granted scopes aren't all among `app_scopes`, storing
nothing. `public_url` is thus baked into each app at creation, which the
configuration example and the README say.

### A creation that stops halfway

**Issue.** Creating an agent app is a store write, a call to Slack, then
another store write; any of them can fail, or agentd can die in between.

**Solution.** As on Rocket.Chat (T14), the agent and its `creating` binding
are stored first, which reserves the name and lets the ingress answer
Slack's challenge. If `apps.manifest.create` fails, the creation is
abandoned: the binding is disabled and the agent deleted, which frees the
name, rather than deleting the rows. If the app was created but can't be
stored, because the store failed or the creation was abandoned meanwhile
(`/agent delete`, or the sweeper), the app is deleted again with the same
token. One `apps.manifest.*` request may take `MANIFEST_TIMEOUT` (2
minutes, where other calls get 30 seconds), since Slack sends the challenge
while it runs; the whole call, rate-limit waits included, gets
`APP_CALL_TIMEOUT` (3 minutes), well within `CREATION_LEASE` (10 minutes),
after which a new Slack sweeper abandons a `creating` Slack binding.

Two cases leave an app at Slack that agentd doesn't know: a crash after
Slack created the app and before it was stored, and a request that timed out
while Slack went on to create the app. Both are listed under the member's
apps at api.slack.com, and a retry of `/agent create` makes another app
([Deferred work](tasks-plan.md#deferred-work)).

### Deleting a Slack agent keeps its binding row

**Issue.** The plan says `/agent delete` deletes the binding. T23's router
recognizes every bot user agentd ever made through the bindings
(`Store::agent_of_bot_user`), so deleting the row would make the deleted
agent's old posts look like a stranger's.

**Solution.** `delete_agent` disables the binding, as for Rocket.Chat, and
now also forgets its Slack client and signing secrets with its bot token,
so the ingress no longer knows it (404). Then `apps.manifest.delete` runs
with the owner's configuration token, and the binding is marked retired.
Without a token that works (none, refused or expired), the app is left at
Slack and the owner is told to delete it at
`https://api.slack.com/apps/<app id>`; nothing retries it later
([Deferred work](tasks-plan.md#deferred-work)). A token Slack refuses is
marked broken, which also tells its owner to register a new one, as the
rotator does; so does one `apps.manifest.create` refuses.

Once `delete_agent` committed, nothing may fail the reply, or the owner
would lose the app id with nothing left to retry. The bindings are read
before the agent is deleted, and every later store error or Slack failure
is logged per binding and reported as `AppDeletion::Failed` with the app id
(when the store could read it), so the reply always says which app to
delete by hand. The bot token isn't revoked with `auth.revoke` first:
`apps.manifest.delete` revokes it with the app, and without a configuration
token the owner, who holds that token anyway, is told to delete the app.

### Agents' messages reach the pipeline built after the queue

**Issue.** `Routers::new` builds the Slack queue and its `slack::Inbound`
sink, but the pipeline is built later, from `Turns` (which needs the bound
listeners), and handed to `Server::with_pipeline`.

**Solution.** `Inbound` sends agents' messages to `slack::Messages`, which
`Routers` holds and `Server::with_pipeline` connects to the pipeline's sink
(with Slack's `Caps`, so `per_binding_delivery` makes the receiving
binding's agent the one candidate). Until then, and in an agentd without
`[sandbox]`, they are dropped with a debug line.

The Slack queue handles the manager's commands too, one request at a time,
so `Inbound` hands an agent's message to `Messages` without waiting. A
worker of its own, among the routers' workers, runs a lane for each binding
that has messages waiting: the lane looks the binding up, fills in a bot
sender with the binding's `SlackSurface::fill_bot_sender` (`bots.info`),
and hands the message to the pipeline, one message at a time in the order
they came, which keeps each thread's messages in order for the pipeline's
lanes. A message to a binding that isn't active (still waiting for its
install, or deleted) is dropped. The command intake likewise runs apart
from the queue. How many messages may wait is the ingress's business (see
"One agent's traffic denies no other agent service" below), so `Messages`
has no bound or drop of its own.

### One workspace for agent apps too

**Issue.** T30 serves the manager's workspace only. An agent binding's URL
is reachable from anywhere, and apps can be installed in other workspaces
when someone has the link.

**Solution.** `StoreSigningSecrets` knows an agent binding only in the
manager's workspace, `Inbound` drops a request to any binding whose
workspace isn't the manager's (the check T30 made for the manager), and the
OAuth callback refuses an install whose `team` isn't it, storing nothing.

### When a binding's first reply reads the member list

**Issue.** The plan has agentd await `refresh_members` when a binding
starts. The Slack queue handles requests one at a time, and a large
workspace's `users.list` can take minutes (T29).

**Solution.** `SlackBots` builds each binding's surface once, and nothing
waits for the member list. Every binding shares the workspace's one
`TeamDirectory`, so there is nothing to track per binding: each lookup of a
surface calls `SlackSurface::refresh_in_background`, which starts
`users.list` when the directory has no list or a stale one and no read is
running. A turn's reply renders with what the directory has then; the read
starts when the lane looks the surface up, before the turn, which takes
longer than `users.list` in all but the largest workspaces, so the first
reply after a restart normally resolves names. The busy line, which the
pipeline posts in a task of its own, and the refusals never wait for it.
`StoreSurfaces` takes Slack surfaces from `SlackBots`, each with its
binding's bot user, and still gives the directory the workspace's agents' bot
users on every lookup (`Store::active_bot_users`, which reads no token),
which also picks up agents another instance installed; an install and a
deletion do it too.

Whichever binding's lookup or render starts it, `users.list` is read with
the manager app's token (`SlackSurface::with_members_api`, which
`SlackBots` gives every agent's surface), never the agent's. The read
holds the directory's one refresh lock, and a failure sets the shared
retry wait and, before the first list, the error every caller gets; an
agent's owner holds its token and could revoke it or use up its Tier 2
quota, and so keep the list stale, or missing, for every agent. The
operators hold the manager's token.

### Owners can forge their agents' events

**Issue.** An agent's app is created with its owner's configuration token,
so the owner can read the app's signing secret at api.slack.com and sign a
`message` event with any content. The signature proves only that the event
came from someone holding the secret. A first fix read the message back and
compared its `ts`, sender and text, but the router decides on more than
that, so an owner could still bill any member V for a message V posted
where the bot can read it: add a `blocks` mention of the bot (V's real
message, unaddressed, was dropped at ingress before deduplication, so
nothing stopped the copy); claim `channel_type: im`; send the copy to a
second agent of theirs, which never deduplicated it; take another agent's
real post and add a mention, inheriting its recorded requester; attach
files; or point `thread_ts` at an agent's reply, since
`conversations.replies` with a reply's `ts` returns the whole thread.

**Solution.** Slack's copy is the source of truth, not the event.
`Surface::confirm` returns the platform's own copy of the message as an
`InboundEvent`, or `None`. `SlackSurface::confirm` reads it back with the
binding's bot token (`conversations.history` with `oldest` and `latest` at
its `ts`, `inclusive`, or `conversations.replies` in the thread the event
named), gets the conversation's kind from `conversations.info` (`is_im`,
`is_mpim`, else a channel; an id's prefix can't tell a group DM or a
private channel from a public one), and normalizes the raw message with
`normalize::read_back`, the same code the ingress runs on events: subtypes,
sender, mentions from `text` and `blocks`, `thread_ts`, files and the
addressing rule, with the binding's bot user, which `SlackSurface` now
carries. Only the binding, event id and arrival time come from the event.
A bot known by its bot id is named by `bots.info` as at ingress.
Rocket.Chat returns the event itself: its messages come over agentd's own
realtime login.

All of it happens in one place, `Pipeline::candidate`. It routes the
candidate's message once as it arrived. An ignore ends there. Anything
else, whoever the event names as its sender, waits for the copy. The copy
must have the event's conversation and `ts`, and be in the thread the
event's lane is for, which keeps each lane to one thread. Unless it equals
the event, as Rocket.Chat's always does, it is routed again with a view of
the store loaded for it, and the message is acted on only if that decision
is the one the event got, and then on the copy: its text and files are
what the turn sees. Anything else, a copy Slack doesn't have, one the
ingress wouldn't keep, one in another thread, or one that routes
differently (no longer addressed, another requester, another scope), is
dropped with a warning and no word to the thread. So the forged payload's
sender, conversation kind, thread, mentions and files decide nothing.
Confirming comes before link prompts and refusals too, so a made-up
message can't make the manager DM anyone or the agent post refusals.

A first version skipped the copy for what it judged only the owner could
gain from: a turn the owner asked for and paid for, a link prompt to the
owner, a refusal of the owner's own message. But that judgement was made
on the forged event, so the owner's turn then ran on the forged
conversation and thread, where other members' sessions live. Signing an
event from themselves with `thread_ts` at member V's thread root and a
mention resumed V's started channel session, its Claude transcript and
scope volume, and replied in V's thread. Claiming V's DM channel with
`channel_type: im` ran on the owner's `Private` scope there, so the lookup
reset V's DM session and replaced it with the owner's, and the owner's
turn saw V's DM history; a forged `channel_type: mpim` reset V's
thread session the same way. An unlinked owner could flood forged
messages that each made the shared manager bot open a DM and post a link
prompt, and an owner could claim the requester of an agent's recorded hop
was themselves. Confirming everything but an ignore closes all of them,
for one Tier 3 read on the owner's own bot token per message of theirs.

The router's rule that only the owner's own message in a one-to-one DM
with the agent runs on `Private` needs no extra check that the DM is the
owner's: with every decision confirmed, the conversation's kind comes from
`conversations.info` and the sender from Slack's copy, and an `im` the
agent's bot token can read is the bot's DM with one member, so a message
the owner sent there is in the owner's DM with the agent.

`TeamDirectory::conv_kind` also requires the `channel.id`
`conversations.info` answers with to be the event's channel exactly, and
refuses it otherwise (`SurfaceError::NotFound`, dropped without a word),
so a channel id spelled another way can't give V's message a second
deduplication key. An Enterprise Grid migration, if Slack then answered
for a channel with an id other than the one its events carry, would make
the check drop that channel's messages without a word until the two agree;
this fails closed, and hasn't been seen against real Slack.

The confirmation window, `CONFIRM_WINDOW` (15 minutes), refuses without a
lookup a message whose `ts` is older than that when its event arrived.
Slack retries a failed delivery three times, the last about five minutes
after the first, so a real event is always well inside it. Deduplication
keeps a message for an hour, and a message older than the bot's
membership never had one, so without the window either could be replayed.
The ingress now applies the window too, before it records anything (see
"What one owner can make agentd keep").

A lookup that fails without saying anything about the message (a Slack
5xx or an unreachable Slack, `SurfaceError::Transport`, or a rate limit,
`SurfaceError::RateLimited`) drops the message and tells the thread
"Sorry, I couldn't check this message. Try again in a moment."
(`UNCONFIRMED_TEXT`, worded for any surface, since the pipeline is), so a
real member isn't left without an answer. A refusal that is about the
message, such as `channel_not_found`, `thread_not_found` or
`not_in_channel` for a made-up conversation or thread, stays silent like
a mismatch.

The copy is read when the lane reaches the message, which may be minutes
after it arrived. An edit in between runs the turn once on the text as it
is then: the `message_changed` event is dropped at ingress, as before, and
the message's first event holds its place in the lane. A message deleted
meanwhile is gone from Slack, or a `tombstone`, and is dropped. A bot's
post that was edited isn't confirmed at all (`Skip::EditedByBot`, in the
normalization the ingress shares, where no plain event has `edited`):
agentd never edits its
agents' posts, so the edit came from someone else holding the bot's token,
such as the owner, who holds their agents' tokens and could otherwise edit
a mention into another agent's recorded post and inherit its requester.
That Slack marks a bot's `chat.update` with `edited` is read from the
SDKs' message shape, like the rest, and not yet seen against real Slack.

Forged events must not starve other agents while they wait for the copy,
since the lookups use the owner's own token, whose quota the owner can use
up at no cost:

- `SlackSurface::confirm` makes its lookups with
  `WebApi::without_waiting`: past the tier's quota in the client's limiter,
  or while a 429 holds the bucket, a call fails at once with
  `SurfaceError::RateLimited` without being sent, and a 429 is not
  retried. Before, a lane waited out the quota, and a 429 up to three
  times a minute, while its message held one of the pipeline's
  `max_pending` places. Now it gets the "try again" line at once, posted
  apart from the lane (below).
- A message also takes one of its agent's owner's `max_pending_per_owner`
  places (`DEFAULT_MAX_PENDING_PER_OWNER`, 16) besides one of the 64 shared
  `max_pending`. Before, about 50 forged addressed events a minute to one
  agent, spread over its threads, kept all 64 taken while they waited in
  the owner token's Tier 3 queue, and every other agent answered only with
  the busy line. A first fix gave each agent 16 places, but an owner with
  four agents still held all 64. Now one owner's agents together hold at
  most 16, and a message past its owner's places gets the busy line like
  one past the others.
- Notices, the busy line and the "try again" line, are posted in a task of
  their own, which holds none of the message's places, at most
  `MAX_NOTICES` (8) at once for one owner's agents; one more is dropped.
  The "try again" line used to be posted in the lane, through the waiting
  `chat.postMessage` client (a message a second per channel), so once the
  owner had used up the history quota, every forged event held its place
  for as long as that post waited. The post itself still waits for the
  quota; making it fail fast would need a way for the pipeline to post
  without waiting on every surface, and with the task holding no place
  and eight at most per owner, waiting costs no other agent anything.
- `SlackSurface::fill_bot_sender` looks the bot up without waiting too.
  Events with a made-up `bot_id` and no `user` each miss the cache and
  call `bots.info` (Tier 3) on the owner's token; past the quota the event
  goes on as it came, from a bot known only by its bot id, which the router
  ignores as an unmanaged bot. Within the quota each call is real and may
  take a while; it runs in its binding's lane in `slack::Messages`, so it
  holds up only that binding's messages. It used to run in one worker for
  every agent's messages: 50 forged events with a 300 ms `bots.info`
  delayed another agent's real message by 15 seconds.

Costs and limits:

- One `conversations.history` or `conversations.replies` call per message
  that isn't ignored, the owner's included, and one `conversations.info`
  per channel per `CONV_KIND_TTL` (an hour), cached in the workspace's
  `TeamDirectory` for every binding. All three are Tier 3 (about 50 a
  minute per app), and so is the thread's `conversations.replies` that
  `message::build` reads for a turn in a thread, from the same bucket. So
  an agent answers about 50 top-level messages a minute, but only about 25
  in threads, which cost two reads each. Past that, the thread gets the
  "try again" line at once, so an agent that really is asked more than
  that tells some of them to ask again. Agent apps need
  `channels:read`, `groups:read`, `im:read` and `mpim:read` for
  `conversations.info`, so the manifest asks for them beyond the plan's
  list. Slack lowered the history limits in 2025 for commercially
  distributed apps outside the Marketplace; agent apps are internal apps of
  their own workspace, which that change doesn't cover, but the live check
  should confirm the lookups aren't throttled.
- A group DM converted to a private channel keeps its cached kind for up
  to `CONV_KIND_TTL`, so its turns run on the group DM's scope for up to
  an hour after the conversion. It is the same conversation and the same
  members, so no one else's scope is reached.
- The event and the copy are routed with two separate reads of the store.
  A change in between (a link, a pause, a rule) makes the two decisions
  differ, and the message is dropped without a word, as a mismatch is.
- A real, addressed message that Slack never delivered (posted in a public
  channel before the bot joined, or during an outage past Slack's retries)
  can be delivered by the owner within the window. So can an edit that
  added a mention to a message that had none, since the ingress drops
  `message_changed` and the edited message was never delivered as
  addressed. Either runs as its sender asked, on their account, with its
  text as Slack has it.
- A lookup failure can't tell a real message from a forged one, so an
  owner who exhausts the app's rate limit can make the agent post the
  "try again" line in threads it can post in, at `chat.postMessage`'s pace
  and at most eight at once for the owner's agents. No turn runs and no one
  is billed.
- The owner holds the bot token, which reads every conversation the bot
  is in, other members' DMs with the agent included. Confirming doesn't
  hide those from the owner; what it guarantees is that no forged event
  runs a turn in another member's session, resets it, reaches their scope
  volume or bills them.
- The manager app isn't involved: its secret is the operators', and it
  starts no turns.
- Deduplication records a message's `<channel>:<ts>` at the ingress,
  before it is confirmed. A forged event carrying member V's real channel
  and `ts` that arrives before Slack's own delivery of V's message takes
  its place, and Slack's is dropped as a duplicate; the forged one is then
  confirmed and runs as V asked, with V's text, or is dropped if it routes
  differently from Slack's copy, and V's message goes unanswered.
  Deduplication is per binding, so this reaches only the owner's own
  agent, never another agent's delivery of the same message.
- `TeamDirectory` remembers which bot ids have no bot user apart from
  those that have one, each set bounded at 10,000 ids shaped like Slack's.
  Made-up bot ids are remembered as having none, so they only churn that
  set, never pushing out a real bot's user, which would cost other agents'
  `bots.info` calls.

The busy line, which a message past the queue bounds gets without being
confirmed, is posted in a task of its own, among the pipeline's tasks so a
shutdown drains it, and so is the "try again" line; the messages still
reach their lanes in the order they came. `Pipeline::handle` still waits
for both: a message's `done` is shared with the notices it starts. A
message past the bounds while its owner's agents have `MAX_NOTICES` being
posted gets none, which also caps what forged events can make the agents
say this way.

### One agent's traffic denies no other agent service

**Issue.** Confirming stops forged events from billing, resuming or
prompting anyone, but an owner can still sign as many events as they like
for each of their agents (up to 10), and a review found shared places
they could take from every other agent. Probed end to end: 3,000
concurrent signed events to one agent got 1,829 answers of 503, and
another agent's real message got 503 too and was never answered. The
ingress had one queue of 1,024 for every binding, the manager's included,
drained by one worker writing a deduplication row per event, and behind
it `slack::Messages` had one queue of 256, dropping silently after Slack
had its 200, and one worker; see also the `bots.info` and pipeline notes
above.

**Solution.** Each bound is on the unit an attacker controls:

- The ingress counts each binding's requests in flight, from the ack
  until they are handed on, in one place (`ingress::Places`). An agent's
  app may have `MAX_IN_FLIGHT_PER_AGENT` (32), agents' apps together
  `QUEUE_CAPACITY` (1,024), and the manager app `QUEUE_CAPACITY` of its
  own. The place is taken before the request is queued, so before its
  deduplication write, and a request past its binding's gets 503, which
  Slack retries, so nothing is dropped after Slack was told it arrived.
  The queue behind is unbounded, since the places bound it. A message
  keeps its place (`InFlight`, in `SlackInbound::Message`) through
  `slack::Messages` until its lane hands it to the pipeline; any other
  request gives it back once the queue has handed it on. The queue still
  handles requests one at a time, so the manager's may wait behind up to
  1,024 of agents', each one store write.
- `slack::Messages` runs a lane for each binding, so what holds one up
  (`bots.info`, the store) holds up that binding's messages only, which
  its places bound.
- The pipeline's shares are per owner, and notices hold no place (above).
- `core_types::MAX_MENTIONS` (100): each surface keeps an event's first 100
  different mentions. The router's view looks each one up in the store,
  and a signed body of up to a megabyte held 60,000 of them, 11 seconds of
  lookups in one lane. Slack's 40,000-character limit allows about 3,000
  mention tokens, so a real message mentioning the bot after 100 others
  isn't addressed to it; Rocket.Chat's are capped the same way.
- Places bound concurrency, not rate: a review signed 100 events with a
  900 KB `event_id` each in 1.6 seconds, which grew the shared SQLite file
  by 184 MB, kept for the seven days deduplication remembers a key, and
  each took about 16 ms of the one queue worker the manager's requests
  wait behind. Now the ids a key is made of must be shaped like Slack's,
  or the body gets 400 before the ack and writes nothing: `event_id` is
  `Ev` and 1 to 32 uppercase letters or digits, `team_id` `T` or `E` and
  1 to 20, a `message` event's `channel` `C`, `D` or `G` and 1 to 20, and
  its `ts` and `thread_ts` 10 digits, a dot and 6 digits. Every fixture,
  taken from Slack's SDK test suites, fits, and so do the ids in Slack's
  documentation. Only a message's `channel` is checked: other events'
  `channel` can be an object, and isn't a key. The manager app's requests
  are checked the same way, which costs it nothing. A signed slash command
  or interaction is keyed by its signature, whose shape verification
  already fixes.
- Each agent's app also has a token bucket (`ingress::Places`, beside its
  count): `AGENT_BURST` (100) requests at once, refilled at
  `AGENT_REQUESTS_PER_SECOND` (8), about Slack's own ceiling of 30,000
  events an hour for one app. Past it, 503, before the deduplication
  write; a request refused for its places takes no token. The bucket is in
  memory, since a restart only gives each app one more burst; the manager
  app has none. One owner's agents' apps together have a second bucket of
  twice that (see "What one owner can make agentd keep").
- One owner's agents' apps together have `MAX_IN_FLIGHT_PER_OWNER` (64)
  places, twice one app's, rather than 32 for each of up to
  `agents.max_per_owner` agents: with its default of 10, one owner held
  320 of the 1,024, and with 32 agents all of them. `slack_app_keys`
  returns the binding's owner with its secret, joined from `agents`, for
  the ingress to count by.
- The queue holds each request's body as it arrived (`Bytes`, at most a
  megabyte), not the parsed event: a `serde_json::Map` of a megabyte of
  `[0,0,…]` took 16 to 32 MB, about 5 to 10 GB for one owner's 320 places
  then. The handler still verifies the raw body, and parses only what it
  checks (the envelope's type and ids, and a message's ids, skipping the
  rest), then `Queue::run` parses the body again. Slash commands and
  interactions are queued the same way.
- The warnings a flood causes are logged at most once a minute for each
  binding or agent, the next one saying how many went quiet
  (`core_types::Throttle`, which the ingress's refusals moved to, now kept
  per binding too, so one app's flood hides no other's refusals): the
  ingress's 400s and `app_rate_limited` notices, the pipeline's "too many
  messages waiting" and "too many notices being posted", and the lanes'
  "couldn't look a bot sender up". The next section adds the rest.
- The pipeline reaps finished tasks when it spawns a notice, as it does
  when it spawns a lane; a flood of notices on an otherwise quiet
  pipeline kept every finished one until the next lane. A lane in
  `slack::Messages` checks whether the pipeline has closed
  (`Sink::is_closed`) before it looks the binding up or calls
  `bots.info`.
- The attribution wait (`ATTRIBUTION_WAIT`, 2 seconds) for a message
  claiming to be from another agent's bot and mentioning this one is taken
  on the event as it arrived, before confirming, so a forged event holds
  its place for up to 2 seconds for free. Its place is one of its owner's,
  so that stalls only their own agents.

Several owners flooding together could still take what other agents need:
four owners the pipeline's 64 places, 16 the ingress's 1,024. The queue
worker is still one, so the manager's requests may wait behind up to 1,024
of agents', each now one small store write.
End-to-end tests hold each bound with a gate or a held Slack call, not a
sleep: an agent's flood past its 32 places is refused while another
agent's message is answered; another agent is answered while a
`bots.info` of the first is held; one owner's three agents together leave
another owner's agent its place; and a message whose "try again" line is
held leaves its place to the next. Each of these failed before the
change. So did the ingress tests that an id not shaped like Slack's
(among them 900 KB ones) gets 400 with nothing recorded, and that an
agent's burst past its bucket gets 503 while another agent's app and the
manager's are answered.

### What one owner can make agentd keep

**Issue.** A sixth review found no way past the places, buckets or shapes,
but found what one owner could still make agentd hold in memory, in its
logs and on disk. A signed DM with no `user` and a 900 KB `bot_id` got 200:
`fill_bot_sender` called `bots.info` with it on the agent's token, and a
`bot_not_found` was cached in the workspace's shared set of bots without a
user, 10,000 ids but no bound in bytes, about 10 GB. `files` had no bound:
a 999 KB DM with 34,000 files became a 9.9 MB `InboundEvent`, and one
owner's apps may have 64 in flight. A body that passed the handler's
`IgnoredAny` check but not the queue's full parse (`"\ud800"`, `1e400`,
deep nesting) logged an unthrottled warning, and so did the pipeline's
warnings a forged message reaches when confirmed, Slack's retry header
(which isn't signed) at info, and a failed notice. Agents' apps' other
events, commands and interactions, which agentd drops, still wrote a
deduplication row each, and every row was kept seven days, 8 a second for
each of an owner's agents. The unsigned challenge probe parsed a
`challenge` of any JSON, so a megabyte of `[0,0,…]` took 16 to 32 MB before
verification.

**Solution.** One place bounds what is kept, and the ingress refuses what
it would refuse, before the ack:

- `normalize` holds every shape check (`is_user_id`, `is_bot_id`,
  `is_file_id`, `is_channel_id`, `is_team_id`, `is_event_id`, `is_ts`),
  each at most 20 characters after its prefix (32 for an `event_id`), and
  the ingress's pre-parse uses them. A message whose `channel`, `ts`,
  `thread_ts`, `user` or `bot_id` isn't shaped like Slack's is
  `Skip::Malformed`, whether it came as an event or was read back; the
  ingress answers such an event with 400 before the ack. Mentions are
  `U…`/`W…` ids of at most 21 characters, from `text` and `blocks` alike.
- A kept message is cut to Slack's limits: `text` to its first 40,000
  characters (`MAX_TEXT_CHARS`), files to the first 10 the bot can
  download (`MAX_FILES`), each with an `F…` id, a URL of at most 4 KB, a
  name cut to 255 characters and a MIME type of at most 255 bytes, and
  mentions to 100 as before. So an `InboundEvent` is at most about 220 KB,
  whatever the body held, and the 64 an owner's apps may have in flight
  at most about 14 MB. Files in history read back are cut the same way.
  An ignored subtype is carried, and logged, cut to 64 bytes.
- `TeamDirectory::bot_user` answers `None` for an id not shaped like a
  bot id, without calling `bots.info` or caching it, however it is
  reached (`fill_bot_sender`, `confirm` and history), so its caches hold
  only short ids.
- The ingress acknowledges and drops, without a deduplication row or a
  place, what an agent's app doesn't need: its events other than
  messages, its slash commands, its interactions, and a message whose
  `ts` is more than `CONFIRM_WINDOW` before it arrived, which `confirm`
  refuses anyway (`surface::within_window`, shared by both). The manager
  app's requests are handled as before.
- Slack's deduplication keys are kept an hour (`DEDUP_RETENTION`), not
  seven days: longer than Slack retries (about five minutes), than a
  signature is accepted (five minutes) and than the confirmation window
  (15 minutes), so a replay after it expires is refused by the window for
  an agent and by the signature for the manager. `processed_events` gains
  an `expires_at` column, set from the retention each caller passes to
  `mark_event_processed`, which the sweeper deletes by; Rocket.Chat keeps
  its week (`PROCESSED_EVENT_RETENTION`). The column, its backfill for
  existing rows and its index replacing `seen_at`'s are in T31's own
  migration.
- One owner's agents' apps together have a token bucket too, `OWNER_BURST`
  (200) then `OWNER_REQUESTS_PER_SECOND` (16), twice one app's like their
  places. So one owner adds at most 16 rows a second, each a few dozen
  bytes and kept an hour: about 60,000 rows, some 15 MB with the indexes,
  where before one owner's ten agents could add 80 a second for a week.
  A bucket that is full while its app or owner has nothing in flight is
  forgotten, being the same as none. The owner is no longer optional for
  an agent's seat: an agent's app whose lookup names no owner (the store
  always names one) gets 503 rather than be counted apart.
- Every log line the ingress writes for what a forger can repeat goes
  through one per-binding throttle, keyed by kind: refusals, including
  400s for a malformed signed body, which take no token since they write
  nothing; `app_rate_limited`; answered challenges, now counted per
  binding rather than all together; Slack's retry headers; and a queued
  body that no longer parses, which only a signed, crafted body can be.
  The pipeline's warnings for a message confirming dropped or couldn't
  check (older than the window, not at Slack, routing differently,
  refused, or Slack unreachable) share a new `Flood::Unconfirmed` kind, and
  a notice that fails to post shares `Flood::Notice`; `confirm`'s own line
  for a message older than the window is at debug level.
- The challenge probe reads `type` and `challenge` as strings only
  (`Cow<str>`), in two parses that skip everything else, so a non-string
  challenge is refused as malformed without being built, and the
  interactivity check reads the payload with `IgnoredAny` once it starts
  with `{`, allocating no keys.
- The pipeline reaps finished tasks through one helper that logs a task
  that panicked, as `drain` does, where it used to drop the error.

The ingress's burst test still runs on the clock, with bounds that allow
for the refill while it runs; the buckets' own tests take the time as an
argument, and one checks an owner's bucket at a fixed time.

### Real traffic meets no refusal of its own

**Issue.** Slack turns off an app's event subscriptions when its request
URL keeps failing, so a 400 or 503 for a real delivery costs far more than
a 200 that drops it: only forged bodies should meet a refusal the ingress
adds. A seventh review found three that real traffic could meet:

- The owner's bucket (`OWNER_BURST`, `OWNER_REQUESTS_PER_SECOND`) was
  charged at admission, for every request. Each agent's app gets every
  message in every channel it is in, so ten agents in busy channels at 1.6
  messages a second each make 16 deliveries a second; past the burst of
  200, a real mention got 503, and so did Slack's retries of it, and it
  was lost. Yet the bucket exists to bound deduplication rows, and only a
  kept message writes one. A new test, one owner's ten agents taking 40
  unaddressed channel messages each and then a mention, got 503 at round
  20 of 40 before the change.
- The ingress refused with 400 a message whose `user` or `bot_id` wasn't
  shaped like Slack's, the manager app's included. That bounded nothing,
  since `normalize` drops such a message as `Skip::Malformed` before any
  row, cache or lookup, and Slack says its ids "could grow longer in the
  future", so a longer real id would have turned the app off. The ids that
  make up a deduplication key had the same risk, at 20 characters after
  the prefix (32 for an `event_id`).
- A message to an agent's app older than the confirmation window was
  dropped with a debug line. While agentd's clock runs more than 15
  minutes fast, or Slack delivers a backlog late, every message is, and
  nothing said so.

**Solution.**

- The owner's bucket is charged where the row is written: in
  `process_event`, once `normalize` has kept a message and before its
  deduplication write (`InFlight::keep`, which knows the place's owner).
  A message past it is dropped after its 200, with no row, and a warning
  once a minute per binding (`Note::OwnerRate`). Admission keeps only each
  app's own bucket: Slack itself delivers at most 30,000 events an hour to
  one app, about 8.3 a second, and that bucket allows a burst of 100 then
  8 a second, so real traffic to one app meets it only in a burst of more
  than 100 events within a few seconds, which Slack's retries then
  deliver; moving it too would leave a forger's flood bound only by
  places, each request costing an HMAC and a parse before the ack. One
  owner's agents' apps together can now take `AGENT_REQUESTS_PER_SECOND`
  for each agent at admission, all of it CPU, since unaddressed messages
  write nothing and their places are given back as soon as the queue
  reaches them.
- The ingress no longer checks a message's sender, and `MessageIds` no
  longer reads it; `normalize` drops one not shaped like Slack's after the
  200. The shapes that make up a key, and `normalize`'s, allow up to 64
  uppercase letters or digits after the prefix (`normalize::MAX_ID_TAIL`,
  now for `event_id` too), and a `ts` of 10 to 20 digits, the first not a
  zero, a dot and 6 digits, so a message still has one spelling: a key is
  at most about a hundred bytes, and a mention at most 65. Rows are about
  twice as large as the 20-character bound allowed, so one owner's hour of
  rows is about 20 MB rather than 15.
- A message dropped for its age goes through the ingress's throttle as
  `Note::Stale`: a warning once a minute per binding, counting those
  dropped since, that asks whether agentd's clock is right. `confirm`'s
  own check stays at debug: an event reaches it only through the ingress,
  which has applied the same check to the same `ts` and arrival time.
- `text` is cut to 160,000 bytes (`MAX_TEXT_BYTES`) at a character
  boundary rather than to 40,000 characters. Slack sends text escaped, `&`
  as `&amp;`, so a character cap cut real messages of mostly `&` or `<` to
  a fifth of their length, while the byte cap is the same bound on memory.
  A mention past the cut is still read from `blocks`, where Slack's
  clients put each one too.
- `SigningSecrets` has a method for each kind of binding, `manager` and
  `agent`, and an agent's app (`AgentApp`) always carries its owner, so
  the seat of a request follows from the lookup and `Busy::Ownerless`,
  which guarded a case the store never produced, is gone.
- The test that a full bucket with nothing in flight is forgotten gives
  each place back at a time it chooses, not when the place is dropped, so
  it no longer reads the clock.

T31's migration (`20260930230000_slack_agent_apps.sql`) was edited in
place during review to add `processed_events.expires_at`. A database that
already ran an earlier version of it must be recreated: sqlx refuses a
migration whose checksum changed. `expires_at` keeps its `DEFAULT 0`,
backfilled at once from `seen_at`: SQLite refuses to add a `NOT NULL`
column without a default to a table that has rows, and rebuilding
`processed_events` for it isn't worth it, since every writer goes through
`mark_event_processed`, which always sets it.

### Thread replies under someone else's root aren't kept

**Issue.** An eighth review found that busy threads still drained the
owner's bucket. `normalize` kept every channel thread reply, whether or
not it could address the bot, so each of one owner's agents' apps kept,
and charged, every reply in every thread of every channel it was in; the
router then ignored nearly all of them, since it answers a person there
only for a mention of the agent or a reply under one of the agent's own
messages. A new test, one owner's ten agents taking a thread reply each
in 40 rounds and then a mention, kept 210 replies before the change and
dropped the mention after its 200. Group DMs were kept whole, though the
router applies the channel rule to them too (only a one-to-one DM is
addressed by being one), and a message `normalize` found malformed was
logged at debug only, so a change in Slack's ids would drop messages
without a word.

**Solution.**

- Outside a one-to-one DM, `normalize` drops a thread reply that doesn't
  mention the bot when its `parent_user_id`, which Slack sends on every
  thread reply, names a user other than the binding's bot user. A reply
  is kept when either is missing, and a `parent_user_id` that isn't a
  string shaped like a user id counts as missing rather than making the
  message malformed, since it is only compared, never kept. The router
  stays the judge of whether the root is the agent's own: `parent_user_id`
  drops only replies under a root the bot can't have posted, which the
  router would ignore, while hops need a mention and `reply_to` is still
  the thread root. `read_back` runs the same rule on Slack's copy, whose
  `conversations.replies` messages carry `parent_user_id` too.
- Group DMs follow the same rule as channels, top-level messages included.
- A kept message's owner's token is taken before deduplication, as before,
  and the module docs now say why: finding a retry a duplicate is a store
  write too.
- `Skip::Malformed` goes through the ingress's throttle as
  `Note::Malformed`: a warning once a minute per binding.
- The owner-rate test no longer needs its 270 messages sent within about
  4.3 seconds: it sends rounds until one of the owner's messages is
  dropped, and bounds what was kept by the burst plus what the measured
  time refilled.
- An agent whose app is replaced by one with another bot user would keep
  replies in the threads its old bot started only when they mention it.
  No flow replaces an agent's app today: a binding gets its bot user once,
  at install.

### Own posts and quiet bots aren't kept either

**Issue.** A ninth review found that `normalize` still kept, and charged
to the owner's bucket, two kinds of message the router always ignores,
in every kind of conversation: the agent's own posts (`OwnMessage`, and
the pipeline drops the sending agent from the candidates as well), and
bots' messages that don't mention the agent (`UnmanagedBot`, or
`NotMentionedByAgent` for another agent). Nothing else reads them at
ingress: `message_refs` rows for the agent's posts are written from
`chat.postMessage`'s answer, a read-back confirms only inbound messages
the router didn't ignore, and the manager app acts only on people's DMs.

**Solution.**

- `normalize` drops, as `Skip::NotAddressed`, a message whose sender is
  the binding's bot user, and a bot's message (`bot_id` or `bot_profile`)
  that doesn't mention the bot user, in DMs too. A bot's message is kept
  when the bot user isn't known. `read_back` inherits both.
- `parent_user_id` is read by a visitor that keeps a string only if
  `is_user_id` accepts it and skips anything else without copying it, so
  every other value still counts as missing.
- The owner-rate test gives up after 10 seconds of wall clock rather than
  2,000 rounds.
- New agentd tests run a turn for a member's reply under the agent's own
  root, with `parent_user_id` the agent's bot, and neither look up nor
  answer a reply under a member's root.

**Open.** Keeping a reply with no `parent_user_id` assumes Slack sends it
on every reply under a root with a user, and leaves it out only under
roots with none, such as incoming webhooks', Workflow Builder's and
`bot_message` posts. That hasn't been checked against live Slack; it
belongs in T32's live pass.

Two more gaps are left open, since the router ignores what they let
through or nothing makes them happen today:

- An own post that carries a `bot_id` and no `user` isn't recognized as
  the bot's own, since `normalize` compares the sender with the bot user,
  and is kept if it mentions the bot user. The router ignores it anyway.
- `normalize` matches only the current binding's bot user, while the
  router's `agent_of_bot_user` knows the bot users of an agent's bindings
  in any state. If an agent ever got a second Slack binding in the same
  team, mentions of its old bot user would be dropped at ingress.

### Bots don't join channels by posting

Slack refuses a post to a conversation the bot isn't in (T23b), and the
`message.*` events come only from conversations the bot is in, so a reply
always goes where the bot heard the message. `chat:write.public`, which
would let a bot post in any public channel, is off unless
`[slack] public_posting` is set. The manifest has the plan's
`channels:join`, but agentd never calls `conversations.join` for an agent.

### Smaller choices

- The app's name and its bot user's display name are the agent's name
  (names are 2 to 32 characters, within Slack's limits); the description is
  fixed. The messages tab is on and not read-only, so members can DM the
  agent. A name Slack refuses is reported with Slack's code.
- `[slack] public_url` must be `https`, since Slack takes nothing else as a
  request URL; its trailing slashes are dropped. Without it, `/agent create`
  on Slack says to set it.
- The install link carries `redirect_uri`, and `oauth.v2.access` repeats it.
- The owner's display name, for the default persona and `list`, comes from
  `users.info` through the manager app. `list` shows owners' names as code
  spans, so a display name like `[Admin](https://evil)` stays text, less
  backticks, control characters and what `ctl::is_invisible` (the check on
  agentctl's file names) lists: bidirectional overrides and isolates
  (U+202A to U+202E, U+2066 to U+2069), zero-width characters and the like,
  which could make a name read as another or reorder the line around it;
  and on Slack names each agent's bot by its user id, which the directory
  resolves to a mention for managed bots only (`MemberDirectory::lookup`): names are
  unique per owner only, so `@helper` can be ambiguous. The installed DM
  names the bot the same way, and the create reply, which comes before the
  bot exists, says that DM will.
- Agent names Slack reads as broadcasts (`here`, `channel`, `everyone`,
  `render::slack::BROADCASTS`) are refused on Slack. Rocket.Chat's list
  moved to `render::rocketchat::BROADCASTS` too, replacing agentd's copy.
- An expired configuration token that isn't broken is one the rotator will
  renew, as after downtime, so `/agent create` says to try again in a minute
  rather than to generate a new one.
- `invalid_manifest` usually means Slack couldn't reach the events URL during
  creation, so the reply asks the member to have `[slack] public_url`
  checked.
- The unsigned `url_verification` echo is answered only for a binding with no
  signing secret yet, one still being created; the manager app's and
  installed apps' challenges must verify, as Slack signs them.
- The callback's pages name the manager app when Slack gave it a name, and
  `agent-core` otherwise.
- The callback answers in plain text with `nosniff`, `no-store` and
  `no-referrer`, and never repeats what the query held. A cancelled install
  (`error=access_denied`) changes nothing, and the link still works.
- The reminder is claimed with a 10-minute lease, like the relink notice, and
  tried at most five times (the relink notice allows 20); an owner no
  manager DM reaches waits. Its link is built before the claim, so an
  attempt is never spent on a link that can't be made.
- Client and signing secrets, bot tokens, configuration tokens and OAuth
  codes are `SecretString`s; a captured-log test at `trace` through a whole
  create, install and delete finds none of them.

## T33: Consent cards and private tasks

### What `--file` names, and how files cross in

**Issue.** The plan says `--file` takes "paths in the calling session's
directory", but the CLI sees container paths (`/volume/sessions/<id>/…` in
Docker, host paths in the process sandbox), and the directory is the
agent's to write, so a path could point anywhere through a symlink.

**Solution.** agentctl turns each path, relative to its working directory
or absolute, into one relative to the session directory, the parent of
`$CLAUDE_CONFIG_DIR`, resolving `.` and `..` as written, and refuses a path
outside it; `PrivateRequest::files` now documents that form. agentd checks
every path again: only plain components, opened one by one from the
session directory's handle with `O_NOFOLLOW` (directories with `O_PATH`,
the file with `O_NONBLOCK` so a FIFO can't hold the request), regular files
of at most `[limits] attach_max_bytes` together, counted in the bytes
actually read, so hard links or sparse files can't multiply what is copied,
a plain file name (the check `agentctl attach` uses) that isn't a dotfile
or `CLAUDE.md`, which a session could read as its configuration, no two
files of one name, at most 10. The files
are copied at request time into `consents/<id>/` under the data directory
(`0700`, files `0600`), so what the owner approves is what the channel turn
had when it asked, and `attachments_json` holds their names. The task's
session gets them in its `work/` before its first turn, through
`SessionManager::work_dir`, which makes the new session's directory on its
volume with the sandbox's `ensure_volume`, and are given to the user the
sandbox's layout is configured to run agents as (`VolumeRef::owner`; the
process sandbox sets none, and they stay agentd's) so the task can change
them. They are deleted once the consent's work is done, with the
directories of the consent's private sessions (see "Every path ends in
`finish_consent`"). A sweep deletes staging directories no unfinished
consent owns after ten minutes, for requests that failed half way.

The task text is refused when empty or over 3000 UTF-16 code units, what a
Slack plain-text section holds as Slack counts it, and a task whose card
wouldn't fit (Slack's 3000 units per text object, or one Rocket.Chat message
at the server's default limit, rendered) is refused when it is asked for.

### Everything a consent owes is in its row

**Issue.** Consent can take a day, across deploys and instances, and the
card, the expiry, the task and the outcome must each happen once.

**Solution.** `consents` (`20260930250000`) holds the plan's columns and the
delivery state. The card is claimed like the relink notice: a conditional
`UPDATE` counts the attempt and sets a 10-minute lease. A failed send backs
off a minute, doubling up to 15 minutes, until the consent expires; a
deferral names its claim, so a stale claimer can't shorten a newer lease.
No manager bot on this instance reaching any of the owner's identities is
a failed send too, not a reason to expire: another instance, or this one
once configured, may reach them. Only a card that won't fit one message
expires at once. The thread is told the card couldn't reach the owner,
rather than that the owner didn't answer, whenever a consent expires with
its card never posted. A
card posted but not recorded is recorded again a few times before its claim
is left to lapse. A decision is one conditional `UPDATE` on a pending
consent whose expiry hasn't passed, and so is an expiry, so neither
overrides the other. The work a decided consent owes (run the task, or post
the outcome) is leased too, 10 minutes, renewed every third of that while
the task runs; only the latest claim can finish it. A try that fails is
counted (`work_failures`) and tried again a minute later, and after three
failures the thread is told it couldn't be run. A claim cut short isn't a
failure: the pipeline checks it is open before claiming, and a shutdown
releases the claims it cut before their turn started
(`release_consent_work`), so another instance takes them up at once. A
claim whose turn started is released only once its killed turn is known to
have ended (see "A task cut short is killed and billed"), and otherwise left
to lapse with its lease: until then the turn may still be on its way to
the CLI, and a release would let another instance run the task beside it. `claim_consent_work` returns the
row as claimed, so the claim reads the session an earlier claim recorded
then, not from the listing before it. The plan runs a task again "only if
it failed before reaching the model": a claim that finds the earlier
claim's session had a turn sent to the CLI (`started`, `maybe_started` or
a finished turn) tells the thread the task was interrupted and finishes,
so a task never runs twice. A claim that finds anything already posted
for the consent (a `message_refs` row naming it: its result, or any
outcome, each its last word) just finishes, as after a delivery whose
finish failed, so nothing is posted twice; posting an outcome checks the
same. The `private` map of claims a
shutdown releases drops an entry only for its own attempt, so an old
attempt can't drop a newer one's. A store error on the way, such as looking up the agent's
surface (`SurfaceLookup::surface` now returns the error rather than
`None`), is a failed try, never a silent finish.

`Consents::run` does all of it in one loop: at startup, on a `Notify` that
`agentctl private` and every decision wake, when the next thing a consent
owes falls due (`Store::next_consent_deadline`: a pending or approved
unfinished consent's expiry, a card's retry, a claim's retry or lease), and
at least every 30 seconds. Pausing, resuming or deleting an agent wakes it
too, so what the agent's new state owes doesn't wait; something that falls
due while a pass runs waits at most those 30 seconds. `Server::run` starts it only with a pipeline,
since without one no turn can ask. Consents read the system clock, never
the pipeline's, which tests pin.

### How the task runs

**Issue.** The plan fixes the session, credential, side and kind, but not
how the task shares the pipeline's capacity, how it is billed, or what its
failures say.

**Solution.** The pipeline does the work (`Pipeline::settle_consents`). An
approved task holds a place under `max_pending` and one among its owner's
agents' messages, like a message, and waits for the next pass when none is
free; it runs in the pipeline's task set, so shutdown drains or cuts it
like a turn. It is billed to the owner as the owner's own turn (it runs on
their account, and doesn't count toward the agent's daily cap for others),
and counts toward the thread's caps. A failure on the credential says it
was the owner's account (`PRIVATE_USAGE_LIMIT_TEXT`, `PRIVATE_LOGIN_TEXT`),
and nobody is told privately. The reply is headed ``*Private task `<id>`:*``
so the channel's next turn can match it to what it asked. Its container is
stopped as soon as the turn ends. A paused agent's approved task waits for the agent to be resumed, until the
consent's expiry, and then the thread is told it didn't run: a short pause
doesn't lose it. Its card says the agent is paused. A deleted agent's
pending consents expire at once, and nothing is posted for them.

### A task cut short is killed and billed

**Issue.** Review found the guard meant to stop a task cut short mid-turn
called `SessionManager::stop`, which waits for the session's slot, which
the running turn holds: it stopped the container only once the turn had
ended by itself. Meanwhile the turn ran on the owner's account, unbilled
(the bill was in the dropped future), outside the places it had held, and
still writing to `memory/` and `shared/`.

**Solution.** The runner gains `SessionManager::kill`, which stops the
session's tracked containers through the sandbox at once, without the
slot, as if they died: the turn ends as `Crashed`, and the session handles
the death as any other. The task's turn runs, with its bill, in a task of
its own (`TurnTask`), so it is billed when it ends whether or not the
private task still waits for it; a crash is billed as a turn of unknown
cost (T27's `CostUnknown`). Dropped before the turn ended, `TurnTask`
spawns a loop that kills the session's containers every 200 ms until the
turn's task has finished, for at most 30 seconds in all, its kills' own
waits included, covering a turn still starting its container. Once the
turn has ended the session is stopped and the claim released, so another
instance takes the task up at once and finds the session marked if the
turn reached the model; a turn that outlives the kills keeps its claim
until the lease lapses, and its container is left to the runner's idle
reaping rather than waited for. The loops run in a `JoinSet` the pipeline
holds, which `Pipeline::wait_for_kills` waits for, 35 seconds at most.
`Pipeline::cut_short` releases the other claims and tells the cut threads
to ask again first, so those notices aren't held up; `Server::run` then
waits for the kills, unless a second signal forces the shutdown, which
drops them at once as before, and closes the store only after, so a turn
cut by a shutdown is billed while the store is open. `Pipeline::drain`
waits for kills of taken-over tasks too. In the Docker sandbox the kill is
the sandbox's stop, which gives the process the daemon's grace period (10
seconds) before it is killed. The test runs `sleep 90` and asserts that
once `cut_short` and `wait_for_kills` return, within ten seconds, the
session is cold, the owner billed and the claim free.

### Every path ends in `finish_consent`

**Issue.** The private session's directory was deleted only after a
delivered result, so a failed hand-over (with partial copies of the
requester's files), a failed turn, a task cut short or taken over, and an
interrupted task left theirs on the owner's private volume, with whatever
an owner-side task copied out of `memory/`.

**Solution.** `Pipeline::finish_consent`, which every outcome reaches once
the consent's work is done, first renews its claim: a claim another one
took over touches nothing, since the newer claim's session may be running
(a test steals the claim mid-turn and checks the newer session's directory
survives the stale claim's delivery), and the renewal keeps any other claim
from taking over meanwhile. It then stops each of the consent's private sessions
(`Store::private_sessions_of`), which waits for a turn still running in it
here, and deletes its directory, then records the work finished and
deletes the staged files. The directories go before the record, so a crash
between the two leaves nothing: the next claim finishes again and finds
them gone. A private task's own turn also has its session stopped as soon
as `run_turn` returns, an error included, so a turn whose send failed
doesn't leave an owner-side container warm. The test fails the send with a
store trigger, and checks the container is stopped at once and the
directory gone once the next claim reports the task interrupted.

### Outcomes without a session

**Issue.** Declined and expired outcomes are "posted the same way", with a
`message_refs` row, but no session ran.

**Solution.** Their rows carry the consent's id as the session id (the
column has no foreign key), with the consent's requester and hop and no
turn, so `posted_elsewhere` shows them to the channel session like a
result.

### Slack's buttons are commands

**Issue.** The plan handles the buttons on `/slack/b/manager/interactivity`,
and only the owner may decide.

**Solution.** A `block_actions` payload whose action is the card's
(`block_id` `consent`, `consent_approve` or `consent_decline`, the consent
id as `value`) becomes `approve <id>` or `decline <id>` from whoever clicked
(`team.id` and `user.id` of the signed payload), answered through its
`response_url`, and goes to the command intake like a slash command. So a
click, a Slack DM and a Rocket.Chat DM all reach `Consents::decide`, which
maps the identity to its member and refuses anyone but the agent's owner
with the same answer as for an unknown id. Nothing else in the payload is
trusted: which message was clicked doesn't matter. Once decided or expired,
the card is updated once (`chat.update` with the outcome in place of the
buttons); on Rocket.Chat its one message is edited to the outcome.

The Slack card shows the task under the label "The task, exactly as
written:", in a `rich_text` block's `rich_text_preformatted` element: a
visible box of literal text, so mrkdwn and `<!channel>` in it show as typed
and nothing in it can pass for the card's own words. Both cards say what approving means: for
someone else's task, that it can read the owner's shared files but not
change them; for a task the owner's identity asked for outside their own
DM, that it runs on the owner's side, with `shared/` and memory. `WebApi` gains `post_blocks` and `update_blocks`, and `Replies`
`dm_rich` and `update_rich`; `Replies::with_slack` now takes the manager
app.

### Smaller choices

- `[limits] consent_ttl_secs` (default 86400, from 1 to 30 days) rather than
  the plan's `consent_ttl`, after the other `_secs` keys.
- A turn may ask for three private tasks (`ctl::MAX_PRIVATE_TASKS`), counted
  on its outbox like its posts, so a turn talked into a loop can't flood the
  owner with cards or the disk with copies. A refused request doesn't
  count.
- The card goes to the owner's identity on the task's surface and team, or
  else any a manager bot reaches; with none, it is retried until it
  expires.
- The card says whether the task is the owner's ("from you, as …") or
  "from someone other than you", since a Slack display name is the user's
  to choose and need not be unique, and names the requester by a handle
  that stays the same: their Slack mention with their user id, or their
  name on their surface (`OpenDm::name_of`: the Slack user's name, the
  Rocket.Chat username), looked up when the card is sent, with their id.
  In the name each control or invisible character, joiners included,
  shows as U+FFFD, so it can't make an exact copy of another name, and it
  is at most 80 characters (a task's joiners are dropped instead, below,
  only so the card and the model read the same text; a name needs no
  such match); agent listings show owners' names the same way
  (`commands::reply::shown_name`). When the card goes to another surface than the thread's,
  the requester and the thread are named for that surface: a Slack mention
  or channel link means nothing on Rocket.Chat. Text from elsewhere on a
  Slack card (names, ids) has `&`, `<` and `>` escaped
  (`render::slack::escape`), since Slack parses links and broadcasts even
  in inline code. The fit check counts the longest name and the paused
  line.
- The thread's caps (T27) are checked when a task starts, as the router
  checks them for any turn outside a DM: in a capped thread the thread is
  told why the task didn't run, and its work is done. Deferring it to the
  next window would leave a task to run hours after it was asked for. A
  task asked for in the owner's DM isn't capped, as DMs aren't.

### The owner's task skips the card only in the owner's own DM

**Issue.** Review found that a task asked for in any turn whose requester is
the owner was approved at once and ran on the owner's side. A hop turn
inherits its requester from another agent's post: in a thread the owner
started, another member's agent can mention the owner's agent with
instructions, and that agent's hop-1 turn asks for a task on the owner's
memory and shared files, with no card, posting the result where that
member reads it. Limiting it to hop 0 wasn't enough: a channel or group-DM
turn the owner started reads the thread's history, which anyone in it can
write, so another member's message could still steer a cardless task on
the owner's side.

**Solution.** A consent is approved at once only when the turn that asked is
on the owner's side (`CtlTurn::side`), which only the owner's own
one-to-one DM with the agent is. Skipping the card there grants nothing
that turn doesn't already have: it runs with `memory/` and read-write
`shared/`, and can post to any conversation. Whatever could steer it
(files, web results, the model's own output) could already use those
directly, so a card would only ask the owner about their own turn's
powers. The row records `approval = 'asked'`, and the
`CHECK` keeping it at hop 0 stays as a necessary condition the store can
see. Every other request waits for a card, the owner's in a channel
included. The owner's card says approving runs the task on the owner's
side, and that it was asked for outside their DM, or at a hop, where its
text may not be what the requester wrote. The side comes from how the
consent was approved, never from the requester alone: `Side::Owner` when
the owner asked in their DM, or when the owner approved on its card
(`approval = 'card'`) a task their own identity asked for, and
`Side::Public` otherwise.

### Characters the card wouldn't show

**Issue.** The card shows the task text, but Unicode tag characters,
bidirectional overrides, zero-width characters and variation selectors
render as nothing or reorder what is shown, long runs of blanks push the
rest of a line out of a Rocket.Chat code block's view, many blank lines
push it below Slack's fold, and stacked combining marks draw over the
card's own text: the text the owner approved could hide instructions the
model reads.

**Solution.** `agentctl private` first drops from the task the characters
that only choose how their neighbours are drawn (`ctl::without_joiners`):
the emoji and text presentation selectors U+FE0E and U+FE0F, which ⚠️
carries, and the zero-width non-joiner and joiner, which Persian and other
joining scripts, and emoji such as 👨‍💻, use. The text reads the same
without them, so the card and the model see the same stored text, and the
other variation selectors, the 256-value channel emoji smuggling uses,
stay refused. It then refuses, before anything is staged, a task with a
control character other than a newline or tab, a character
`ctl::is_invisible` matches, a line indented more than 32 columns, a run
of blanks wider than 16 columns after a line's first visible character
(enough for a table's alignment), more than 2 blank lines in a row, or
more than 4 combining diacritical marks in a row (the blocks of marks any
letter takes, so scripts whose letters carry their own marks aren't
refused). Columns count a tab as 8 and the ideographic space as 2, in an
indent and inside a line alike. Blank means whitespace, or U+2800 (the
Braille blank) or U+1D159 (the musical null notehead), which aren't
whitespace but draw as nothing. A line of nothing but blanks counts only
as a blank line, however wide. Indented code, YAML and
nested lists pass. File names are still refused, not changed, for any
invisible character: a name must match the file on disk and what the card
lists. Whether Rocket.Chat wraps a code block's long lines hasn't been
checked live; the limits hold either way, but a long line without blanks
could still run off the view if it doesn't. `ctl::is_invisible` is
Unicode's whole `Default_Ignorable_Code_Point` set
(`render::is_default_ignorable`, which the Slack renderer's link check
already used), and the line and paragraph separators and interlinear
annotation characters; skill file names used a copy of an older list and
now use it too. The card also says the files' contents aren't shown and
can direct the task like its text.

### The Rocket.Chat card fence

**Issue.** Rocket.Chat's message parser knows only code blocks fenced with
exactly three backticks, so a card that fenced the task with more showed no
block when the task held three: the task rendered as Markdown, and the
card could show something else than what runs. A card split in several
messages, and an update that edited only the first, could separate the task
from the commands that decide it.

**Solution.** The task is fenced with exactly three backticks, and a
zero-width space follows each backtick in it, so none of its own can close
the block; the card says so when it did. A test parses the card by the
parser's rule. File names are inline code. A card is one message:
`dm_rich` and `update_rich` refuse Markdown the surface would split
(`ReplyError::TooLong`), and a request whose card wouldn't fit is refused.

### Limits on what consents hold

**Issue.** Each consent copied up to ten capped files, so one turn could
stage gigabytes from hard links or sparse files, held for up to 30 days on
the data disk SQLite shares, and every turn could send the owner more cards.

**Solution.** The cap counts the files together, by the bytes read. Each
agent may have 10 unfinished consents (`MAX_OPEN_PER_AGENT`) and each
requester's identity 3 with one agent (`MAX_OPEN_PER_REQUESTER`), counted in
the insert's `BEGIN IMMEDIATE` transaction; past that the request is
refused (`Refused`). Unfinished, not only pending: an approved task's files
are held too until it runs. The owner's own consents don't count toward the
agent's limit, so others can't use it up to block the owner's tasks. The
same counts are read before the files are staged, so a request past them
copies nothing. Partial indexes serve the queries the worker
runs every pass.

### No hop from a private result

**Issue.** A private task's reply was recorded like any agent post, with
the consent's requester and hop, so a mention in it (on the owner's side,
written with `memory/` mounted) started another agent's turn on the
requester's credential.

**Solution.** Every row a private task's delivery records names its
consent (`message_refs.consent_id`), and the router's view gives such a
message no attribution, so the router ignores it as an unattributed managed
bot's post. Mentions are left as written: the thread still reads the reply
as the model wrote it, and T34, which feeds the router from `message_refs`,
finds the row at once rather than waiting for one, and can add a distinct
reason if it wants one.

### Open items

- An org-wide Slack install or Slack Connect can give an interaction a
  `team` the manager app doesn't serve; the click is dropped without a
  word. Worth a log line.
- agentctl resolves `..` lexically, so `work/link/../x` names `work/x`;
  agentd checks the path again, so this is only a surprise, not an escape.
- A staging request slower than agentctl's 30-second timeout is dropped
  with the client: its per-turn count isn't released and its directory waits
  for the sweep, and the agent may ask again.
- `consents` rows are never deleted; a retention sweep for finished rows is
  left for later.
- A claim that loses the race to `set_consent_session` leaves the session
  row it made unused (it has no directory yet).
- A capped thread loses an approved task for good: an approval that lands
  in a busy hour isn't deferred until the window frees.
- Only failed tries count toward giving up. A claim lost because its
  instance died counts nothing, so a task that kills agentd before
  reaching the model would be claimed again and again; a panic in the
  task's own task doesn't bring agentd down, so this is unlikely.
- Combining marks are counted only in the generic diacritical blocks, so
  stacked marks of a script's own (Thai tone marks, Hebrew and Arabic
  points, the Cyrillic enclosing signs) aren't limited; counting
  General_Category Mn and Me would need a Unicode table this workspace
  doesn't carry yet.
- A takeover's kill that outlives `KILL_TIMEOUT` (a turn that survives
  30 seconds of Docker stops) leaves the turn holding its session's slot,
  so a later `finish_consent` on the same instance waits in its
  `sessions.stop` until the turn ends. Very unlikely; bounding that stop,
  or killing instead and leaving the container to the idle reaper, would
  fix it.
- Whether a session still runs is known only on its own instance. If a
  claim's renewals keep failing while its turn runs, another instance can
  take the task over, report it interrupted and delete that session's
  directory under the running turn. The damage stays in that private
  session's own directory.
- With no manager bot anywhere reaching the owner, the thread hears that
  the card couldn't reach them only when the consent expires, a day by
  default. Each try counts toward the card's backoff, so in a deployment
  where only some instances reach the owner, one that can may wait up to
  15 minutes for its turn.

### Rust 1.99 deprecates `fetch_update`

- **Issue:** CI's lint jobs install the current stable, and Rust 1.99
  renamed `AtomicUsize::fetch_update` to `try_update`. A test mock's
  `fetch_update` then failed clippy's `-D warnings`. `try_update` is newer
  than the 1.98.1 MSRV, so it can't be used either.
- **Solution:** the mock decrements with a `compare_exchange` loop, which
  both toolchains accept. Clippy on 1.99 is otherwise clean.

## T34: Agent-to-agent hand-off

### agentd delivers its agents' mentions itself

**Issue.** The plan had the Slack half wait on T32's live check of whether
Slack delivers one app's bot post to another app, and T32 is blocked on a
live workspace. Rocket.Chat delivers such posts, but only through the
mentioned bot's own connection, after agentd's post returns.

**Solution.** The fallback T32 describes, on every surface. As a turn's
posts are recorded, each one hands off to the other managed agents it
mentions, and once the delivery is done `Pipeline::hand_off` queues each
hand-off as the posting bot's message, which then goes through routing
and the turn like any message. A mention hands off only to an agent whose
bot is active on the conversation's own surface and team
(`agent_for_bot`), never to the poster, and the router then requires that
agent's mention, the poster's attribution and the agent's rules for the
requester and conversation. A one-to-one DM hands off nothing: no other
agent answers there.

### Only a turn's posts in its own thread hand off

**Issue.** A turn can post elsewhere (`post --to` another channel), and
the platform delivers that post too. Attributing it would let one thread's
requester start a hop, with their hop count, in a conversation they never
wrote in; and agentd's copy needs the other conversation's kind, which it
doesn't know.

**Solution.** `message_refs` gains `hands_off` (migration
`20260930260000_hand_offs`): true only for a turn's post in the thread it
answers, when the turn's delivery may hand off (`Delivery::hand_offs`: not
a private task, not a one-to-one DM, and the posting bot's binding found
as the agent's). `StoreView` gives a post's attribution only when
`hands_off` is set, which replaces the private-task consent check there,
so a post anywhere else reaches other agents, by either delivery, as an
unattributed bot message, which the router ignores.

### Which users a post mentions

**Issue.** agentd needs the mentions of what it posted, as the platform
reads them, so that its own copy routes like the platform's. A review
found one place they differed: Markdown leaves backticks of unequal runs
as text (`` `a @writer`` ``), but Slack pairs any two backticks, so Slack
showed the mention as code while agentd counted it, and a hand-off ran
that the thread couldn't see. Two rounds of renderer fixes (zero-width
spaces around literal backticks, then around backticks inside inline code)
were each bypassed by another input: the splitter cutting a long code
span, and a cut placed by a backtick run the splitter paired differently
from Slack.

**Solution.** `Surface::post` returns `Posted { msg, mentions }`.
Rocket.Chat's `chat.postMessage` response carries the server's `mentions[]`,
read with the same helper the inbound path uses (no broadcasts, each once,
at most `MAX_MENTIONS`). Slack's response has no parsed mentions, so the
surface reads the `<@U…>` tokens of each chunk it sent, as `normalize`
reads an event's text; the renderer turns an `@<bot user id>` into one for
managed bots. `normalize::mentions`, which reads both agentd's own posts
and the platform's copies of them (and every other event), counts a
`<@U…>` token only when no backtick lies both before and after it in the
same text (`render::slack::without_code` drops all from the first
backtick to the last). Code, fenced or inline, has a backtick on each
side of what it holds, so whatever rule Slack pairs backticks by (any
two, no empty spans, not across lines, links first) and whatever the
renderer or the splitter left around a mention, a mention counted is one
Slack shows. An earlier version modelled Slack as pairing any two
backticks; a review showed plausible rules under which it counted a
mention shown as code, and that the platform's copy, read by the inbound
path, skipped the check. The cost, chosen to fail closed: a mention
between two backticks that Slack shows as text, as in `` `a` @writer
`b` ``, or between two code blocks, hands off to no one, which SKILL.md
tells agents. A bot's mentions are read from its `text` alone, as
agentd reads what it posted: Slack may attach `rich_text` blocks it makes
from an app's text-only post, whose `user` elements (or a code-styled one)
would otherwise count a mention agentd's own read dropped, running a hop
with no `hand_offs` row behind it. Agentd's agents post text, and the
router ignores every other bot, so nothing that routes is lost. A
person's mentions are still read from the blocks too, so a person who
writes `` `foo` @agent `bar` `` is addressed through the `user` element
their client sends; only a person's message without blocks loses it. The renderer
changes went back out: the scan makes them unneeded, and they made code
holding a backtick paste with invisible characters. A backtick in a link's
URL is percent-encoded (`%60`), so a URL never holds one; such a URL
copies as `%60`, the same address. `MockSurface` renders a post with
Slack's renderer (`render::slack::to_mrkdwn`, with the `name_user` names
as its directory) and reads its tokens the same way, so a test sees the
mentions Slack would: none in code, none for `@here`, and none for a
handle a word character follows.

### One hop for each posting turn and agent

**Issue.** When the platform delivers the post too, the mentioned agent
gets it twice, in either order; and a turn whose reply and queued posts
(or two chunks) mention the same agent would start it once per post.

**Solution.** A turn's delivery hands off to each agent once, from the
first of its posts that mentions it (`Delivery::mentioned` skips the
agents already handed to), so a turn writes one `hand_offs` row and queues
one job for each agent. `Pipeline::candidate` claims the hop in
`processed_events` (source `hop`, kept for `PROCESSED_EVENT_RETENTION`),
keyed by the mentioned agent and the posting turn read from the post's
`message_refs` row, after the decision is confirmed and just before
acting on it, for any decision on a hop: a requester other than the
sender, which only an attributed post gives. Every later copy, of that
post or of another post of the same turn, finds the claim and is dropped
without a word. Claiming only there keeps an ignored copy (one whose
attribution didn't come within `attribution_wait`) from blocking the
other. Refusals are claimed too, so one turn's posts give one refusal; a
refusal because the rules couldn't be read is not claimed, so another copy
may get past it. When the posting turn can't be read, or the claim can't
be recorded, the hop is left undone rather than risk running it twice,
and a hand-off keeps its row to be tried again; there is no fallback key.
The claim is checked read-only as well (`Store::event_processed`): in
`dispatch` for a bot's message, before it takes a place in a queue, in
`candidate` before the read-back, and before a hand-off is queued, so a
second copy costs neither.

### A hand-off skips the read-back

**Issue.** Each hand-off was read back like a platform event
(`conversations.replies` on Slack), though agentd made it from its own
post, and a duplicate the platform delivered paid one more read-back
before the claim dropped it. The read-back was also what told that the
mentioned agent's bot could see the conversation.

**Solution.** A job `hand_off` built holds its `hand_offs` row
(`Job::hand_off`) and skips `confirmed`: there is nothing to confirm in a
copy agentd made itself. Instead, before claiming the hop, it asks the
platform whether the bot may post there (`Surface::can_post_now`): a no
settles the hand-off without a word, and an error, or an agent with no
active binding there (no surface), leaves its row to be tried again, as
the replay does. The turn then doesn't ask again. Slack's `can_post` asks
`conversations.info` (the conversation asked about, not archived, and a
member, or a DM or group DM) and keeps a yes for `MEMBERSHIP_TTL` (five
minutes, at most `MAX_MEMBERSHIPS` conversations) for the messages it
read back; `can_post_now` always asks, and a no drops a kept yes, so a bot
removed from a channel takes no hop there. A `channel_not_found`, an auth
error (`invalid_auth`, `missing_scope`, ...) or a forbidden answer is a
no too, logged as a warning with Slack's error since a revoked token or a
missing scope looks like it, and drops the kept yes; a rate limit, a
transport failure or an error on Slack's side stays an error. Where a
message was read back with the bot's own access, a `can_post` that fails
that way is taken as a yes, so a rate-limited membership check doesn't
drop a person's message without a word; a refusal answers nothing. A
message the platform doesn't confirm from another bot gets no notice.

### Hand-offs are kept until settled

**Issue.** A hand-off was only a job in memory: a shutdown, a crash or a
full queue lost it. The first durable version then replayed every live
hand-off each five-minute lease, though a job can wait in a lane and run
for half an hour, so dead copies took queue places and people got busy
lines; it deleted rows whatever the outcome, so a store error lost the
hand-off; it wrote the rows only after the whole delivery; and a
shutdown left its rows due five minutes later. A later review found that
a drain longer than the lease stopped leasing the rows its jobs held, so
another instance ran them too; that a row was not held between its write
and its job, so a turn cut mid-delivery left it for a lease; that SQLite
could give a deleted row's id to a new row while a job still held the
old one; and that a post's ref and its hand-offs were two writes.

**Solution.** `Delivery::post_to` records a post's `message_refs` row and
a `hand_offs` row for each agent it hands off to (the event as JSON and
the agent), due after `HAND_OFF_LEASE` (five minutes), in one
transaction (`Store::record_post`), and holds each row at once
(`Holding`, from the pipeline's `Holder`: an in-memory set, a cache in
front of the rows, which refuses an id it holds already). The hold passes
to the job, which keeps it while it waits and runs. `hand_offs.id` is
`AUTOINCREMENT`, so an id is never given again. `candidate` says whether
it settled the hand-off: claimed and acted on, found claimed, ignored,
refused for a reason other than unreadable rules, or found unable to
post; and the lane deletes the row only then. The "hand-off worker" calls
`Pipeline::replay_hand_offs` every `HAND_OFF_SWEEP_INTERVAL` (30 s): it
leases this instance's held rows again, so neither it nor another
instance takes them, then leases up to 64 due rows, drops rows recorded
over an hour ago that are due (logging how many; a held row is never due,
whichever instance holds it, while its holder's worker runs), and
queues each again, unless its hop's claim is taken, when the row is done
with. A row that doesn't parse, whose agent is gone or has no active
binding there, or whose posting turn can't be read is left for later or
to age out, and each row is handled on its own. Once the pipeline is
closed, the worker, which now stops with the proxy and ctl listeners
after the drain or cut, only leases the held rows again, so a drain
longer than the lease keeps them. A hold let go while the pipeline is
closed, by a job that never settled its row or a delivery cut short, is
set aside, and the end of `drain` or `cut_short` makes those rows due at
once, so the next instance takes them on its first look, and the server
does it once more after its hand-off worker is joined or aborted
(`Pipeline::release_cut_hand_offs`), since the worker's last pass can let
go of a row after them. A row stays set aside until a release of it
succeeds, so one that failed or was cancelled is made again by the next,
and is then forgotten, so a later release can't take it from an instance
that holds it by then. A drain cut by its timeout while its release is in
flight may have committed it without forgetting the rows, and
`cut_short` then releases them again a few milliseconds later; a row
another instance took in between has its lease reset, which the claim
keeps from running twice. The server's last release gets a second on a
forced shutdown, and otherwise runs until a second signal; one cut short
leaves those rows to their lease. A pass whose re-lease of a row commits after the
cut released it leaves that row to its lease, five minutes at most. The
sweep interval is `PipelineSettings::hand_off_sweep` (30 s), so a test
drives the server's worker through a drain. A hand-off past
a full queue keeps its row and is taken again after the lease rather than
answered with a busy line, which no one would read. The record and the
replay read `PipelineSettings::now`.

A held row can still come due, and so be dropped as stale once it is an
hour old while its job runs on in memory, when its holder can't lease it
for more than about four and a half minutes: the instance frozen
(SIGSTOP, a paused VM, a blocked runtime), its store writes failing, or
two instances' clocks that far apart. That is the exposure the lease
already has for takes; such a job still runs from memory, but a cut or
an unsettled end then has no row to fall back on.

Delivery is at least once until the hop is claimed, and at most once
after: a crash between the claim and the turn loses that hop, and a cut
there before the working emoji is on posts no `RESTARTING_TEXT`. An
instance that stops leasing a row it holds, as a stuck one, lets another
take it; the claim then still runs the hop once, perhaps out of its lane's
order. A hand-off retried after a lease can run in the next hour's
`thread_turns_per_hour` budget, as any message waiting that long would.

### The hop-cap line once an hour

**Issue.** The hop-cap refusal was posted for every hop that hit it, and
said "1 hops" for a cap of one, and impl-notes claimed the claim made it
once, though each posting turn claims its own hop.

**Solution.** `HopCap` uses the limit notices' window
(`limit_window` gives `hop_cap`, one hour), so each agent says it once an
hour in a thread; a second chain stopped at the cap within the hour stops
without a word, which the skill tells agents to expect. It now reads
"{name} won't answer: it takes part in chains of at most {max} hand-offs",
with "1 hand-off" for a cap of one, since "this chain has reached its
limit" read as if the requester had done something. On a hop, the line for
rules that can't be read is throttled the same way (`policy_unavailable`,
one hour), since every post and copy of a chain meets it again and its
hand-off is retried; when that claim can't be recorded, the line isn't
posted, since a store failing is what it reports and each retry would
say it again. A link prompt on a hop is throttled per requester with
`claim_failure_notice` (`link_prompt/hop`, `FAILURE_DM_INTERVAL`) and
released when it can't be sent. The personal refusals (rules, ban, not in
the channel) stay silent on a hop, as T27 and T33 decided for any bot's
message.

### The ref is still recorded after the post

**Issue.** The plan asked to record the ref before posting, keyed by a
client-generated id where the platform supports one, so the platform's
copy finds its attribution at once.

**Solution.** Not done. agentd's own copy is made after the ref is
recorded, so it never races it; only a platform copy can arrive first, and
it still waits up to `PipelineSettings::attribution_wait` (two seconds by
default) for the row. If it gives up, it is ignored, and agentd's copy
runs the hop. A client id would change both surfaces' post calls for no
hop that isn't already delivered. The race test holds the post at a gate,
sends the platform's copy before the ref exists, and checks the hop runs
once, from that copy, while the turn's later post is still held; it waits
30 seconds for the attribution, so a slow machine can't fail it, and it
fails when the copy doesn't wait.

### `ask-agent` posts after the turn

**Issue.** The plan says to post the task, record it with the turn's
requester and hop, and return at once. A review found that matching
handles before names let an agent on Rocket.Chat, where a bot's handle is
its username and the first agent of a name takes it, take the tasks meant
for another owner's agent of that name.

**Solution.** The handler queues a post to `here` in the turn's outbox,
the handle and a colon alone on its first paragraph, then the task, so it
goes out after the turn with the other queued posts, recorded with the
turn's requester and hop, and hands off like any other post; the command
returns at once and its output says the other agent may answer. Nothing
of the task can run into the mention or turn it into code (a table row
would). The handle is the bot's user id on Slack and its username on
Rocket.Chat (`DirectoryEntry::handle`, which `list` uses too). Among the
agents with a bot on the conversation's surface and team that the turn's
requester may see (public ones and their own), a mention (`@x`, or
Slack's `<@…|…>`) names a handle only, and a bare word names a name or a
handle; a bare word that fits two agents, as one's name and another's
handle can, is refused with each one's handle, name, and whether it is
the requester's own or public, so the agent can tell which it meant. It
is refused outside a channel or group DM, for the calling agent itself,
for an agent the turn asked already (it would take one turn anyway; the
check and the queueing are one critical section on the outbox, so two
asks at once can't both pass), past the turn's ten queued posts, and, as
before, inside a private task. It doesn't check what the
router will decide (the other agent's rules, the hop cap, its limits):
those depend on the requester and the thread, and the router says them
when the hand-off runs, so the skill tells the agent not to promise an
answer.

### Smaller choices

- The hand-offs are queued once the whole delivery is done, after any
  failure notice, and before the working emoji comes off.
- The skill asks for `ask-agent` or a mention, not both. The first post
  that mentions an agent is the one it gets, which with both is the
  reply, posted before the queued task, so the task isn't in that
  agent's turn.
- A hand-off isn't read back, so a post a moderator deletes after it was
  made still hands off, and one retried after a shutdown can run up to an
  hour later.
- A background process left from an earlier turn (see "What remains" in
  the T18 notes) can also run `ask-agent` or post mentions in the next
  turn, starting hops attributed to that turn's requester. Killing the
  processes a turn leaves, the deferred fix there, removes this too.

### Open items

- Rocket.Chat's `mentions` in the `chat.postMessage` response is read as
  the realtime stream gives it; the response shape is not verified on a
  live server.
- Slack not resolving a mention inside code, and pairing any two
  backticks (the model `without_code` reads by), are not verified on a
  live workspace.

## T35a: Cloud hand-off: store and grammar

### A late answer is recorded once, on a row the pass marked

**Issue.** The plan records `fired`, `rejected` or `unknown` from
`sending`, and `fired` or `rejected` from `unknown`, marking the notice
done "unless a notice claim's lease is live". Taken literally:

- A late `unknown`, such as a reply that timed out and whose record was
  held up past the pass, was refused. Its notice then stayed owed, and the
  member got the "may have started" DM after the reply had said the same.
- A lease's exception changes nothing anyone sees. Every path that sends a
  notice needs `state = 'unknown'`, so a row a late answer made `fired` or
  `rejected` is never claimed again, live lease or not. The exception only
  left such a row's `notified_at` empty for good when the claim's send then
  failed. A first version added a `notice_leased_until` column to tell a
  lease from a backoff; it guarded against a case that can't send.
- An `unknown` the command's own reply recorded could still be turned into
  `fired` later, although only a row the pass marked waits on an answer.

A second version took a late answer only while the notice hadn't gone out.
Review showed that window is about one DM long: T35c's notifier marks rows
and sends their notices in the same tick, so a late `fired` (a blue-green
deploy whose instances' timeouts differ, say) would almost never be
recorded, leaving `cloud list` without the link of a session that exists,
while refusing the record prevented no message.

**Solution.** `finish_cloud_handoff` takes a row that is `sending`, or
`unknown` with `unknown_reason` `no_answer`, which only the pass sets, so a
reply's own `unknown` takes no later answer. It sets `notified_at` if it is
empty, since the reply tells the member, and keeps a notice's time. A late
`fired` or `rejected` replaces `unknown` and sets `answered_at`; a late
`unknown` keeps the row's time and state, fills in a status the row lacked
and replaces `no_answer` with its own reason. Either way the row no longer
has `no_answer`, so it takes one late answer. A notice a claim is sending at
that moment may still reach the member besides the reply, and the claim's
mark then returns false; avoiding that would need a two-phase send. There
is no lease column.

### Sealed values are bound to the member

**Issue.** Sealing a token with `cloud_routines/token_enc/<id>` stops a
ciphertext copied into another row, but not a row moved whole: setting
`member_id` to another member's id, by anyone who can write the database
without the master key, would let that member open the token and fire the
routine, and read moved tasks in `cloud list`.

**Solution.** A token's associated data key is
`<member>:<id>:<routine id>:<label>:<url origin>` and a task's
`<member>:<id>` (only the origin may hold `:`, since a label never does,
and it comes last). A row moved to another member, or given another
routine id, label or origin, fails with `SealError::Decrypt`, which tests
show for both tables; the label keeps two of a member's own rows from
swapping labels, which would fire one routine for the other's name. The
stored routine id and origin strings are the associated data, so
normalizing either later means re-sealing with the master key, not a SQL
`UPDATE`.

### The `url` crate changes what it parses

**Issue.** `url::Url::parse` removes `.` and `..` segments, decodes `%`
escapes in a host, reads `\` as `/` in `http` and `https` URLs, skips
extra slashes after the scheme (`https:///v1/…` gets the host `v1`), drops
a default port and accepts an empty user name (`https://@host`). A routine
URL checked only after parsing could name a path or a host other than the
one pasted.

**Solution.** `RoutineUrl` checks the text as pasted first: printable
ASCII without `%` anywhere, an `http` or `https` scheme, an authority
that isn't empty and holds no `@`, and a path, from the first `/`, `\`,
`?` or `#` after it, that is exactly `/v1/claude_code/routines/` + a
`RoutineId` + `/fire`. Since the id is only letters and digits, that one
match refuses dot segments, escapes, backslashes, a trailing slash, a
query and a fragment. Only then is the text parsed with `url`, for the
origin, and the parsed URL must agree: no user info, query or fragment,
and the same path. `commands` depends on `url` 2.5.8, which was already in
the lockfile through `reqwest`, so the origin is the same `url::Origin`
type that `reqwest::Url::origin()` returns for `[cloud] base_url`, and
T35c compares the two with `==` (a default port compares equal whether it
was typed or not).

### A routine keeps the origin it was registered with

**Issue.** The origin was to be compared with `[cloud] base_url` only at
`cloud add`. Each fire builds its URL from the current `base_url`, so an
operator who later points `base_url` at a mock or a loopback test server
would send every stored token there.

**Solution.** `cloud_routines.url_origin` keeps the origin, as
`url::Origin::ascii_serialization` writes it, and is part of the token's
associated data. `put_cloud_routine` takes it, and `CloudRoutineToken` and
`CloudRoutine` return it, so a fire (T35b's client, or T35c before
calling it) refuses a routine whose origin isn't `base_url`'s, and T35c
tells the member to `cloud add` it again. The comparison parses the stored
origin (`Url::parse(stored)?.origin() == base_url.origin()`) rather than
comparing strings, so a change in how `url` writes an origin can't lock
members out.

### A routine token is checked in one place

**Issue.** Any word was stored as a token, so one holding a control or
non-ASCII character would fail only when T35b built the `Authorization`
header, after a hand-off row was written; and the grammar, the store and
the fire client would each have had to agree on what a token is.

**Solution.** `core_types::RoutineToken` wraps a `SecretString` and is made
only by `RoutineToken::parse`: it starts with `sk-ant-` (the routine fire
reference says its tokens are prefixed `sk-ant-oat01-`; only the family is
required, in case the version changes), is printable ASCII and is at most
1024 bytes. Its `Debug` is redacted and it has no `Display` or serde form.
`cloud add`'s clap value parser makes one at once, so no `String` copy of
the token lives in the parsed arguments, and the refusal is a fixed
sentence. The store seals one (`NewCloudRoutine::token`) and opens one
(`CloudRoutineToken::token`), reporting a stored token that no longer passes
as `Corrupt`, which T35c answers by asking the member to `cloud add` the
routine again; T35b's `fire` takes `&RoutineToken`. `core-types`
now depends on `secrecy`, already a workspace dependency.

### Where the shared types live

**Issue.** T35b's fire client and T35a's store and grammar are built in
parallel, and T35c joins them. They need one routine id type, and the
store needs an outcome it can record without depending on `agentd`.

**Solution.** `core-types` has the ids: `RoutineId` (`trig_` and 1 to 64
ASCII letters and digits, checked by `FromStr` and serde), which
`RoutineUrl::routine_id` returns and the store takes, for T35b's `fire` to
take too, and the UUID ids `CloudRoutineId` and `CloudHandoffId`. `store`
has what its columns hold: `CloudOrigin` (`slack_slash`, `slack_dm`,
`rocketchat_dm`, named as `Origin::kind` names them), `CloudHandoffState`,
and `CloudOutcome` (`Fired { session_id, session_url }`,
`Rejected { status, error_type, retry_after_secs }` with an optional
status for a connection that failed before sending, and
`Unknown { status, reason }`). The reason is a `CloudUnknownReason`, one
per case of the plan's failure table: `server_error`, `other_status`,
`timeout`, `connection_lost`, `redirect`, `unreadable_answer`, and
`no_answer`, which only the pass sets: `finish_cloud_handoff` refuses it
with `StoreError::Refused`, since a caller recording it would leave a row
that takes a second answer and owes a second notice. It is kept in a column
of its own, `unknown_reason`, set exactly while the row is `unknown`, rather
than in `error_type`, which holds what the endpoint said. T35c maps T35b's
`FireOutcome` onto `CloudOutcome`. `retry_after_secs` is a `u32`: T35b
should read `Retry-After` into one, or T35c saturate into it.

Hand-off ids are minted by agentd in `begin_cloud_handoff` and never taken
from what a member types, so `finish_cloud_handoff` and the notice methods
take only the id and aren't scoped to a member. A command that ever took a
hand-off id from member text would have to check the member first.

### Tasks and tokens stay out of `Debug`

**Issue.** The plan keeps `cloud run`'s task a `String` that nothing logs,
but a derived `Debug` on the command, on a row read back from the store or
on the struct written to it would print it wherever someone logs the value
whole.

**Solution.** `CloudCommand` and `NewCloudHandoff` have hand-written
`Debug`s that leave out the task, and `CloudCommand`'s redacts the token as
`SecretString` does. The store opens a task only for
`recent_cloud_handoffs`, into a `SecretString` (`RecentCloudHandoff::task`);
`CloudHandoff`, which the pass and the notices return, has no task at all.
`RoutineUrl`'s `Debug` shows only the origin and the routine id, never the
URL as typed.

### A superseded claim may still mark its notice sent

**Issue.** `mark_cloud_handoff_notified(id, claim, now)` names its claim,
and `defer_cloud_handoff_notice` must ignore a stale claim so it can't
shorten a newer lease. Refusing a stale claim's mark as well would leave a
notice the member did get owed, so a failed send by the newer claim would
send it a third time.

**Solution.** A deferral needs the latest claim, as for consent cards. A
mark needs only a claim that was made (`notice_attempts >= claim`, and
`claim > 0`): any claim whose message went out may mark the notice sent.

### `cloud add` in a room needs its refusal already

**Issue.** agentd's refusal of a secret-bearing command sent where others
read it matches every command, so a new one doesn't compile until it has
its own advice (T13). `Command::Cloud` made it fail to compile, and
leaving `cloud add` in a room to "isn't available yet" would say nothing
about a token that is now public. A `cloud add` that fails to parse in a
room, the likeliest case (a trailing slash, a missing word), got only the
generic advice, which didn't say where a routine token is revoked.

**Solution.** `refuse_public_secret` gains the `cloud add` arm T35c
specifies: the token is no longer secret, nothing was stored, revoke it
with **Regenerate** or **Revoke** at claude.ai/code/routines. The generic
advice for secret-looking text that doesn't parse now names that too.
Every `cloud` command sent privately answers "isn't available yet" until
T35c's handlers land.

### Smaller choices

- Labels are case-sensitive, as agent names are. Registering a label
  again may also move it to another routine id, provided no other label
  holds that id; T35c's reply should name the new id. Routine ids are
  case-sensitive too, which T35c's live check is to confirm.
- `put_cloud_routine` returns `CloudRoutinePut`: `Added`, `Replaced`,
  `RoutineTaken { label }` naming the label that holds the routine, or
  `Full` past `MAX_CLOUD_ROUTINES` (20). The checks and the write are one
  `BEGIN IMMEDIATE` transaction; a test with 30 concurrent registrations on
  a file database stores exactly 20.
- `delete_cloud_routine` keeps the routine's hand-offs, which carry their
  own copy of the label and id; `delete_cloud_routines_of` deletes both in
  one transaction. Both tables also cascade from `members`.
- `stale_cloud_handoffs` is one `UPDATE … RETURNING`, so two passes never
  return the same row; it sorts what it returns by `created_at`, since
  `RETURNING` has no order. Due notices come oldest answer first. Its
  cut-off is the caller's, so in a blue-green deploy that changes
  `[cloud] timeout_secs`, the instance with the shorter timeout can mark a
  row the other still waits on; the member then gets the notice and the
  reply, and the late answer is still recorded. A per-row deadline would
  close that, at the cost of a column.
- A partial index on `created_at` where `state = 'sending'` serves the
  pass, which otherwise reads the whole history for its few `sending` rows;
  a plain one on `created_at` serves the purge; and one on
  `(member_id, created_at)` serves `recent_cloud_handoffs` (ties broken by
  `rowid`) and the member deletions.
- `purge_cloud_handoffs(before, now)` keeps a row whose notice is still
  owed at `now`, so a `retention_days` of 1 can't cut the notice's day
  short, and a `sending` row, so a purge that runs before the pass, after
  a long outage, doesn't lose its notice.
- The schema checks what each state implies: `answered_at` is set exactly
  when the row isn't `sending`, `session_id` (never empty) exactly when it
  is `fired`, a `session_url` only then, an error type and `Retry-After`
  only when `rejected`, an `unknown_reason` exactly when `unknown`,
  `notified_at` never while `sending`, an
  `http_status` from 100 to 999, and a routine id that starts with `trig_`
  in both tables.
- `recent_cloud_handoffs` fails whole when one task won't open, as other
  store reads do; T35c's `cloud list` should still show the routines when
  the hand-offs can't be read.
- The notice backoff after claim `n` is a minute times 2^(n-1), capped at
  an hour, so a day's notice gets about 24 tries after the first few.
- Parse errors for `cloud` commands use fixed sentences and never repeat a
  label, URL, token or task. Unparsed text counts as secret-bearing when a
  word `cloud` is followed by `add` and any value, wherever it stands
  (`help cloud add …` and `!agent cloud add …` in a DM included), with the
  same normalization as the other secret words.

### Open items

- A token typed where no command is read gets no advice: `cloud add …`
  without `!agent` in a Rocket.Chat room or an agent's DM is turn text, and
  a parsed `cloud run` or `persona` holding a token in its free text isn't
  secret-bearing. Command words garbled with invisible or look-alike
  characters fall back to the `sk-ant-` rule alone. Login codes have the
  same gaps.

## T35b: Cloud hand-off: fire client

No request reached claude.ai from the environment this was built in: the
client was tested against `wiremock` and hand-written local servers only.
The design's [Verified and assumed](design.md#verified-and-assumed) list
still holds, and T35c's live check covers it.

### The fire client takes the stored routine and returns its outcome

**Issue.** The plan gives `fire(routine_id, token, task)` returning a
`FireOutcome`. But T35c writes the hand-off `sending` before it fires, so
an error from `fire` would leave a row the pass later reports as possibly
started, after the reply said nothing was. T35a stores each routine with
the origin of the URL it was registered with, and a token checked as
`RoutineToken`, so a check of either here would be a second copy of a
rule. And `FireOutcome` restated the store's `CloudOutcome` field for
field, with a mapping to keep in step.

**Solution.** `fire(&CloudRoutineToken, task)` takes the row
`Store::cloud_routine` gives and returns the outcome alone. `FireOutcome`
is `store::CloudOutcome`, which T35c records as it is; every shape the
classifier gives is recorded and read back in
`every_outcome_is_recorded_as_it_is`, and none is `no_answer`, which only
the store's pass sets. A routine whose `url_origin` isn't
`FireClient::origin()` (an operator moved `[cloud] base_url` since it was
registered, so its token isn't for this endpoint) and a task `check_task`
refuses (empty, or over `MAX_TASK_BYTES`, 65,536 bytes) are not sent: the
outcome is `rejected` with no status, as for a connection that failed
first, and the log says why. T35c runs both checks before
`begin_cloud_handoff`, so it can say what is wrong; these are the
backstop. The token is T35a's `RoutineToken`, `sk-ant-` and printable
ASCII, so a header always carries it. The routine id is a `RoutineId`,
letters and digits after `trig_`, so the path can't be changed through
it. A reqwest builder error, should one still happen, counts as not sent
too. Building the client fails with `FireClientError` alone.

### reqwest retries some requests on its own

**Issue.** reqwest 0.13's client has a retry policy by default, which
resends a request the server refused at the protocol level (HTTP/2's
`REFUSED_STREAM` and `GOAWAY`), up to twice. The workspace builds reqwest
without `http2`, so it can't happen today, but a feature another crate
turns on would bring it back. A pooled connection the server closed while
it sat idle is a second way to end up unsure: a request written to it
fails after sending, and would be reported `unknown` though the server
never read it.

**Solution.** The client is built with `retry(reqwest::retry::never())`
and `pool_max_idle_per_host(0)`, so each fire is one request on a
connection of its own. That costs a TCP and TLS handshake per hand-off,
which a member's command can afford. `fire_never_retries` and the
hand-written server that hangs up after reading the request both check
that exactly one request, on one connection, was made, and
`each_fire_opens_a_connection_of_its_own` fires twice at a server that
keeps connections open and counts two.

### A connect timeout counts as not sent only when it fires first

**Issue.** reqwest reports its connect timeout through hyper-util's
connect error, so `is_connect()` holds and the fire is `rejected`: nothing
was sent. Its total timeout covers connecting too. With
`connect_timeout_secs` equal to `timeout_secs`, which the plan allows, the
two race, and when the total one wins, a connection that never opened is a
plain timeout, reported `unknown`. Probed once against an address that
drops packets: a 100 ms connect timeout under a 600 ms total gave
`rejected`, and 300 ms for both gave `unknown` with reason `timeout`.

**Solution.** `connect_timeout_secs` must be below `timeout_secs`, not at
most equal as the plan says, so a connection that never opened is always
`rejected`.

### No proxy reads a plain request or one to a loopback address

**Issue.** The client honors the system proxy settings, as the plan asks,
and reqwest's environment proxy has no exception of its own for loopback
addresses. With `HTTP_PROXY` set, a plain `http` request to a loopback
`base_url` went to the proxy in full, routine token included, and the
proxy reached `127.0.0.1` on its own host. A `NO_PROXY` listing
`127.0.0.1` doesn't cover all of `127.0.0.0/8` or `::ffff:127.0.0.1`,
which the configuration accepts. The credential proxy's upstream, the
OAuth client and the Slack and Rocket.Chat clients had the same gap with
members' and bots' tokens, and the Slack and Rocket.Chat clients accept
plain `http` to any host: Compose's `http://rocketchat:3000` sent the bot's
`X-Auth-Token` to the proxy in clear, and the proxy couldn't resolve
`rocketchat` either.

**Solution.** One rule, `core_types::skips_proxy`, decides it for all five
clients: a plain `http` base, or a loopback IP address over either
scheme, is called without a proxy; any other `https` base honors the
system settings. Skipping the proxy for every plain `http` base, rather
than only for loopback addresses with `NO_PROXY` advice for the rest, was
chosen because a proxy should never read a request that carries
credentials in clear, and an operator who needs one for such a host uses
`https`; it also needs no deployment to set anything. `auth` decides per
endpoint, with a proxied and a direct client, so a loopback token-endpoint
fake no longer takes the proxy away from an `https` profile endpoint. The
Slack client keeps one client chosen by `api_url`, which also carries file
transfers and `response_url` posts, so a plain `http` `api_url` (only
fakes use one) takes those off the proxy too; the example configuration
says so, rather than a second client for a case no deployment has.
Rocket.Chat's cross-origin redirects already use their own proxied client.
The example configuration and the Compose README tell operators the rule:
`https://` URLs honor `HTTPS_PROXY` (or `ALL_PROXY`, its fallback in
hyper-util's environment matcher) and `NO_PROXY`, each also in lowercase; `HTTP_PROXY` never
applies, since no plain `http://` URL is proxied. A plain `http://` URL or a
loopback IP address is always called directly, which `NO_PROXY` can't
change. Rocket.Chat's realtime connection (`tokio-tungstenite`) never used
a proxy, so a Rocket.Chat server has to be reachable directly anyway.
Where `http` is allowed is a separate rule, `core_types::is_loopback_ip_host`,
which `[cloud]`, `[proxy] upstream` and `[claude_oauth]` share;
`[claude_oauth]` now refuses `http://localhost` as the other two do. Each
client's builder takes a proxy that tests add as if the system had it,
and `testkit::proxy::assert_proxied_only_elsewhere` checks with a fake
proxy that the fake server's loopback address and each other direct base
go around it while `https://api.example.com` goes through it, without
setting any environment variable.

### `base_url` is checked as the `url` crate reads it

**Issue.** The plan asks for an origin with no path, query, fragment or
credentials. The `url` crate normalizes what it parses: a trailing slash,
a default port, upper case in the host, and a special scheme written
without slashes (`https:api.anthropic.com`) all give the same origin.

**Solution.** `CloudConfig::base_url` parses the value and requires the
path to be `/` and no query, fragment or user info, `https`, or `http`
only when the host is a loopback IP address (IPv4-mapped included;
`localhost` is a name that could resolve anywhere), as `[proxy] upstream`
does. Every request's URL is the parsed origin with the fire path set on
it, and `FireClient::origin()` is its ASCII serialization, the form T35a
stores a routine's in; T35c compares a pasted routine URL's origin with
it, so both sides go through the same normalization:
`cloud_config_is_checked` parses routine URLs with T35a's `RoutineUrl` and
finds the default's origin equal to one with `:443` typed, and different
from one with another port, scheme or host. Errors never repeat the value.

### What counts as each outcome

**Issue.** The plan names the documented statuses but leaves the rest of
the edges to the implementation.

**Solution.**

- Only a 200 can be `fired`. A 201, 202 or 204, though successful, is
  `unknown` with reason `other_status`, and so is every status but 200,
  400, 401, 403, 404, 429, a 3xx (`redirect`) and a 5xx (`server_error`),
  a 413 from a proxy in front of the endpoint among them.
- `claude_code_session_id` must be `session_` and 1 to 128 ASCII letters
  and digits exactly, with nothing trimmed. `claude_code_session_url` is
  kept only when it is byte for byte `https://claude.ai/code/` and that
  id, so a query, an extra segment or another host's look-alike loses the
  URL and keeps the id.
- `error.type` is kept from an error body only when it is 1 to 64
  lowercase ASCII letters, digits and `_`, so arbitrary text never reaches
  the store or a log. A body that isn't such an envelope gives none, and
  the status alone decides.
- `Retry-After` is kept only as digits, surrounding blanks aside; Rust's
  own parse would also take `+5`. A date or a fraction is ignored. Any
  number over a day, however long, is kept as a day
  (`MAX_RETRY_AFTER_SECS`), since the endpoint's documented limits are
  hourly and a member shouldn't be told to wait 136 years.
- A body is read up to 64 KiB, refused at once by its `Content-Length`
  when that says more, and otherwise counted as it arrives. A 200 whose
  body runs past it, breaks off or stalls past the timeout is `unknown`
  (`unreadable_answer`, `connection_lost` or `timeout`); an error status whose
  body does is still `rejected`, without an error type.
- Each fire logs one line, `fired a cloud routine`, with the routine id,
  the status, the outcome's state, and the session id, the error type, the
  `Retry-After` or the reason for `unknown`: at info for `fired` and at
  warn otherwise. A request that wasn't sent adds a warning saying why:
  the origin, the task, or the connect error and its causes without the
  URL (DNS, TLS, a refused connection), which never hold a header. The
  causes are joined by `core_types::error_chain`, which the Slack and
  Rocket.Chat clients' transport errors now use too.
- The answer's `Debug` shows a body's length only, since a body may echo
  what was sent; the classifier and its types are private to the module.

### Scopes outside profile and inference fail at startup

**Issue.** `OAuthConfig::validate` now refuses any `[claude_oauth]
scopes` entry but `user:profile` and `user:inference`. A deployment that
had widened them stops starting.

**Solution.** That is the point: the error names `claude_oauth.scopes`.
`auth::ALLOWED_SCOPES` lists the two.

### What was asked for doesn't bound what was granted

**Issue.** The scopes travel in the authorize URL agentd hands the member,
and the code exchange doesn't send them again, so a member who added
`user:sessions:claude_code` to the URL before approving got a linked token
with it, which the credential proxy would forward on every turn. A token
endpoint that grants its own defaults, or doesn't narrow a refresh, would
do the same silently, and members who linked while the configured scopes
were wider still hold such tokens. RFC 6749 lets the answer leave `scope`
out when it is what the request asked for, which for a login is the URL
the member may have changed.

**Solution.** `auth` reads a token response's `scope` as any JSON value
and splits every string in it, at any depth of arrays and objects and
objects' keys included, on blanks into scopes, so no shape hides a wider
one. It sorts the result into a `Grant`: wider if any scope is outside
`ALLOWED_SCOPES`, whatever the shape, so an object with any key that
isn't an allowed scope, a label such as `{"granted": ...}` included, is
wider, since a label can't be told from a scope name; for a
space-separated string, as RFC 6749 has it, or an array of strings,
allowed if it names a scope and unstated if not (absent, `null`, blank,
an empty array or an array of blank strings, since no scope is no grant
and a server using it for "as requested" would reopen the hole); and
unreadable for any other shape that names no wider scope, such as a
number, `true`, `[7]`, a nested array of allowed scopes, `{}` or
`{"user:profile": true}`.

- A login keeps only an allowed grant. A wider one is
  `AuthError::ScopeRefused`, whose reply tells the member to open the
  login link unchanged. An unstated or unreadable one is
  `AuthError::ScopeUnstated`, since every login then fails until the
  endpoint changes: its error line for the operator is logged once per
  login attempt, not again by the command handler, and the reply says so
  and to tell an admin rather than inviting retries.
- A refresh sent the scopes itself, so an unstated or unreadable grant
  keeps the link, and an unreadable one is logged at warn, once per
  process. A wider one, in any shape, marks the link broken, as a dead
  refresh token does, so the member gets the relink notice and the token
  isn't served again. Any string, or any object key at any depth, that
  isn't an allowed scope makes the grant wider, labelled objects such as
  `{"granted": ...}` included, even inside an array, since a label can't
  be told from a scope name. So a format change that adds such a string
  or key breaks every member's link and revokes every token at its next
  refresh, by design.
- A refused grant's refresh token is revoked, as `logout` revokes one,
  best effort: before the login's reply, and after a refresh releases the
  member's lock, the new refresh token if the answer had one and the
  link's old one otherwise. A refused login without a refresh token logs
  that its access token lives until it expires. A known gap: an answer
  that fails as `InvalidResponse` (one that doesn't parse, nests past
  serde_json's depth limit, has an empty `access_token` or has a bad
  `expires_in`) is never sorted
  into a `Grant`, so a wider grant in it isn't revoked, though nothing of
  it is stored or served.

That the endpoint names `scope` is observed, not documented: Claude Code
2.1.286's bundled JavaScript keeps `scopes: Hgn(e.scope)` in
`formatTokens`, where `Hgn` splits a string on spaces and gives `[]` for
anything else, and its save path `p8n` stores a login's tokens only when
those scopes include `user:inference` (`rU`), as its auth-source detection
and refresh eligibility also require. A login answer without `scope` would
leave Claude Code itself without a claude.ai login, so the endpoint names
it, at least for Claude Code's scope set; T35c's live check confirms it for
agentd's pair. The design's Verified and assumed list footnotes this, and
notes that `ALLOWED_SCOPES` is a constant, so a default scope the server
starts adding would refuse every login and break every link until a
release allows it.

Revoking a refused refresh's token can't hit one a concurrent refresh
stored: agentd runs one process, refreshes of a member are serialized,
and a broken link isn't refreshed again. Two processes on one database
aren't supported; if they become so, `update_claude_tokens` should also
require `broken_at IS NULL`, since it now clears a break with only a
generation check and could undo one the other process made. Whether
Anthropic revokes per grant or per member and client id, which would end
a member's healthy link when a later refused login is revoked, is
unverified, as it is for `logout`; T35c's live check asks.

## T36a: Slack Connect: who is outside

No Slack Connect payload was captured for this: T36e is blocked on a live
workspace, so everything below was built against the design's reading of
Slack's documentation, bolt-python's fixtures and the hand-written fixtures
in `testkit::slack` (the Slack Connect ones are listed in that module's
rustdoc). Each assumption T36e has to confirm lives in one place:

- the sender's team fields and their order: `MessageEvent::sender_teams`;
- that `authorizations[0].team_id` names the installation: the ingress's
  `Installation` reader;
- that `users.list` and `users.info` give an outside member's own
  `team_id`, and what they say of Grid members, deactivated accounts and
  strangers: `directory::is_home`;
- which `ok: false` codes mean Slack couldn't answer this time:
  `web::TRANSIENT_CODES`;
- the sharing flags and `connected_team_ids`: `web::Conversation::sharing`
  and `MAX_CONNECTED_TEAMS`;
- an interaction's `user.team_id`: the ingress's interaction reader.

### Only Slack's own `user_not_found` means "not home"

**Issue.** The plan has `home_user` answer `Ok(false)` for
`user_not_found` and pass every other error on. `WebApi::user_info` maps
a whole family of codes to `SurfaceError::NotFound` (`users_not_found`,
`channel_not_found`, `file_deleted`, …), so matching the variant would have
cached "not home" for answers that say nothing about the user.

**Solution.** Only `NotFound("user_not_found")` is a verdict; any other
`NotFound` is returned uncached like the rest. A `users.info` answer with no
`team_id`, or one not shaped like a team id, is `Ok(false)` and cached: it
is an answer, and it doesn't name the workspace.

### The answer cache numbers its entries

**Issue.** The cache keeps answers for an hour, at most 4,096, the oldest
dropped first. Keying the eviction queue by the time an answer was given
let it grow without bound when one user's answer was given again at the
same `Instant`, as a coarse clock or a test can do: every duplicate looked
current.

**Solution.** `HomeAnswers` numbers each answer it keeps and the queue holds
`(user, number)`, so only the newest entry per user is live; the queue is
compacted once it holds twice the capacity.

### The manager DM's check runs in `answer_text`

**Issue.** The plan puts the check "in `intake.rs`, before `answer_text`".
`answer_text` is the one place every DM command passes through (the
intake's task calls it, and `handle_text` wraps it), so a check before it
in `intake::start` alone would leave `handle_text` unchecked.

**Solution.** `Commands::answer_text` asks `Commands::admits` first, so the
check runs in the member's intake task, before the text is even parsed,
and nowhere else. It needs the Slack manager and a member of its workspace;
without them a `SlackDm` origin is dropped (it can't arise otherwise).
`Transport` and `RateLimited` post `pipeline::UNCONFIRMED_TEXT` into the DM
the event named; any other error drops the command, and a sender who isn't
home is dropped at debug. The warning for a failed lookup is the
directory's (below), not the command's. `Commands::dispatch`, which skips
both the parser and this check, is now `#[cfg(test)]`.

### Every bot is skipped, not only one with a bot user

**Issue.** The plan has `fill_sender_team` skip a sender with
`sender_bot_user` set. A bot post whose bot user isn't known
(`sender_is_bot` without `sender_bot_user`, after a failed or userless
`bots.info`) would then cost a `users.info` for a bot id or a made-up user.

**Solution.** It skips `sender_is_bot` too, as the design's "never a bot"
says: no bot is a requester, so its `outside` decides nothing.

### An unflagged `is_shared` is external

**Issue.** `Sharing` comes from `is_shared`, `is_org_shared` and
`is_ext_shared`, and Slack's documentation doesn't say what `is_shared`
alone, with neither of the others, means.

**Solution.** It is `External { teams: None }`, failing closed: a channel
that is shared with nobody says it is unknown, never home-only.
`is_org_shared` alone is `Org`; `is_ext_shared` wins over it. An
`is_shared` or `is_ext_shared` that is present and neither `false` nor
`null` counts as `true` (review round 1: reading it as `false` failed
open); an `is_org_shared` that isn't `true` is `false`, which leaves a
shared conversation external. `Sharing` lives in `core-types` beside
`Outside`, since T36c reads it outside `surface-slack`.

### No installation, no event

**Issue.** An `event_callback` without `authorizations[0].team_id` is
dropped with a throttled warning. The plan doesn't say how leniently the
list is read.

**Solution.** Only the first element is read, and the rest is skipped
unread. A missing, null, empty or non-array `authorizations`, a first
element that isn't an object, and a `team_id` that is absent, null or not
shaped like a team id all count as no installation: the event gets its 200,
nothing is recorded, and the ingress warns once per binding per
`WARNING_INTERVAL` (debug for the rest). The check comes after the agent
app's "not a message" ignore, so other events an agent's app gets stay
quiet as before.

### Outside drops in the Slack queue are debug lines

**Issue.** `slack::Inbound` drops a manager DM whose fields make the sender
outside and an interaction whose `sender_team` is absent or another team,
"with a debug line throttled per binding".

**Solution.** `Inbound` keeps a `Throttle<BindingId>` on `WARNING_INTERVAL`:
the first drop per binding and interval logs at debug with the count since,
the rest at trace. Neither touches the network.

### Deactivated accounts and an old member list vouch for no one

**Issue.** The first version kept every `users.list` entry whose `team_id`
was the workspace, deactivated ones too, on the grounds that a deactivated
user can't post. Review round 1 showed why that fails on Grid: user ids
are organization-wide, so a member deactivated here can stay active in a
sibling workspace and post in a shared channel. And a member list whose
refreshes keep failing was kept, and answered "home", with no age limit.

**Solution.** `directory::is_home` refuses a `deleted` account, from the
list or from `users.info` (cached like any answer), and `home_user`
answers from the list only while it is less than `HOME_ANSWER_TTL` old;
past that, each sender costs one cached `users.info`. A reactivated member
costs one lookup.

### `users.info` is read like the message's fields

**Issue.** `users.info` was trusted on `team_id` alone, the one field
Bolt's fixtures show Slack sometimes filling with the installing team for
an outside actor. Separately, on Grid a member of several of the
organization's workspaces may have a `team_id` naming another one, which
locked home members out of agents, DMs and consent clicks.

**Solution.** `directory::is_home`, used for both the list and
`users.info`: an active account, not `is_stranger`, whose `team_id` is the
workspace or whose `enterprise_user` is of the home organization and lists
the workspace in its `teams`; and every team the answer names
(`team_id`, `profile.team`, `enterprise_user.enterprise_id`) is the
workspace, the organization or one of those `teams`. A team field that
isn't a string, or an `enterprise_user` that isn't an object, is read as
naming no team, so it fails the rule. It is sound for outside members: a
member of another organization has its own `enterprise_id` or `team_id`,
and Slack lists only the workspaces a member belongs to. Two Grid cases
still fail closed, as the design now says: a message whose own fields name
another workspace of the organization, and a click whose `user.team_id`
does. `auth.test`'s `enterprise_id` now counts only when it starts with
`E`.

### Slack saying it couldn't answer is a transport error

**Issue.** `fatal_error`, `internal_error`, `request_timeout` and
`service_unavailable` come as `ok: false` with HTTP 200, so they were
`SurfaceError::Api`, and the home check took the sender as outside: the
copy's decision differed from the event's and the message was dropped
without the "try again" line, while an HTTP 5xx got it.

**Solution.** `web::map_error` maps them (`TRANSIENT_CODES`) to
`SurfaceError::Transport`, as an HTTP 5xx is, for every caller. The thread
and the manager DM get the "try again" line; nothing is cached.

### A failed home lookup is warned of once a minute

**Issue.** A revoked manager token or a missing `users:read` makes every
sender the member list doesn't vouch for outside, and `fill_sender_team`
logged that only at debug, while the manager DM's check had its own
warning.

**Solution.** `TeamDirectory::home_user` logs a failure that isn't a
transport error or a rate limit as a warning, at most once per
`LOOKUP_WARNING_INTERVAL` (a minute) for the workspace, whoever asked; the
commands' own throttle is gone.

### Nothing is posted for a sender the fields say is outside

**Issue.** `Pipeline::queue` posted the busy line for any sender who
wasn't a bot, before routing, so a member of another organization learned
whether the agent was busy.

**Solution.** A message whose `outside` is set is queued `quietly`, as a
bot's is: past the bounds it is dropped with the throttled warning. A
sender the fields leave `None` still gets the line, as a forged event
does.

### The store refuses outside requesters until T36b

**Issue.** `message_refs`, `consents` and `ctl_tokens` have no column for
`outside`, so a requester written with it would read back as home: a hop's
attribution, a consent or an `agentctl` turn could then run an outside
requester's work on a home requester's terms. Nothing writes one today,
since the router ignores outside requesters, but T36b has to remember all
three.

**Solution.** `record_post` (and so `record_message_ref`),
`create_consent` and `set_ctl_turn` refuse a requester with `outside` set
with `StoreError::Refused`, through one helper, `store::home_requester`.
The T36b plan says so, and that admitting a listed organization rests on
T36e: confirmation keeps the copy's own `outside`, which is `Outside { team:
None }` when the copy's fields don't name the organization, so
`copy_stands` drops an admitted outside message unless Slack's copy names
it.

### An event installed elsewhere is dropped before deduplication

**Issue.** An agent's app made public and installed in another workspace
gets that installation's deliveries. They were recorded under the message's
deduplication key and only then dropped by `slack::Inbound`, so the home
installation's delivery of the same message was taken for a retry.

**Solution.** `Queue::with_workspace` (which replaces `with_home_org`)
tells the ingress the workspace agentd serves; an event whose installation
is another one is dropped before it is deduplicated, with a warning at most
once per binding and `WARNING_INTERVAL`. `slack::Inbound` keeps its check.

### Smaller fixes from review round 1

- `fill_sender_team` sets a sender keyed by another surface or workspace
  outside rather than leaving them home.
- `member_who_left` needs the user's own `team_id` to be the workspace, so
  a member of another organization, or of another workspace of the
  organization, deactivated there deletes nothing here.
- `normalize::SENDER_TEAM_FIELDS` is gone: it restated
  `MessageEvent::sender_teams`, which is the one list.

### Left as they are

- A click passes on `user.team_id` alone, with no lookup: the payload is
  Slack's, signed with the manager app's secret only operators hold, and
  its only buttons are consent cards, which only the agent's owner can
  decide; the design says so.
- `home_user` has no single flight: concurrent confirmations of one new
  sender each ask `users.info` until the first answer is kept. It costs
  only the manager's Tier 4 quota for real senders, at most the places in
  flight (64 per owner), and a used-up quota gets "try again", never home.

## T35c: Cloud hand-off: commands

No request reached claude.ai from the environment this was built in: the
commands were tested against `wiremock` routine endpoints, a `MockSurface`
manager bot and a `wiremock` Slack. The plan's live check, and the design's
[Verified and assumed](design.md#verified-and-assumed) list, are still to be
done.

### Slack's tokens have to be read before its entities are decoded

**Issue.** Slack delivers command text with `&`, `<` and `>` as entities and
mentions, channels and links as `<…>` tokens, and `slash_command` and
`dm_command` decoded the entities before anything else read the text. After
that a `<div>` or `<T>` the member typed, which Slack sent as `&lt;div&gt;`,
can't be told from a token Slack made: rewriting tokens then would refuse
every task that mentions HTML or generics, and would read a typed
`<https://a|b>` as a link.

**Solution.** The Slack side hands on the text as Slack delivered it, and
`Commands::answer_text`, the one place every command passes through, decodes
it for Slack origins (`Origin::decoded`) before parsing, so every other
command reads as before. A `cloud run` from Slack parses the delivered text
again for its task: the command words and the label can't hold an entity or
a token, so it is the same command. Its tokens are rewritten there
(`slack_task`): `<@U…|name>` to `@name`, `<#C…|name>` to `#name`, `<url>`
and a `<url|label>` labelled with its URL to the URL, any other
`<url|label>` to `label (url)`, and the entities in the rest and inside the
tokens decoded. A link is a scheme, `:` and something without blanks, as
`https:`, `mailto:` and `tel:` links are. Anything else in brackets is
refused with one fixed line: broadcasts, user groups, dates, an unclosed
`<`, and a mention or channel without its name. Slack's `message` events,
which a DM with the manager app is, carry mentions as `<@U…>` without the
name, so a task with a mention is refused there and the member types the
name as plain text, or uses the slash command, whose tokens carry it.

### A routine's origin is compared parsed, in the fire client too

**Issue.** T35a decided that a routine's stored `url_origin` is parsed and
compared as an origin with `base_url`'s, so a change in how the `url` crate
writes an origin can't lock members out. T35b's backstop in
`FireClient::fire` compared the strings, and its test refused
`http://127.0.0.1:<port>/`, the same origin with a slash. With both rules
in place, a routine T35c's check let through would be written `sending`
and then refused unsent by the client.

**Solution.** One rule: `FireClient::fires_for(url_origin)` parses the
stored origin and compares it with `base_url`'s. `fire`, `cloud add`, `cloud
run` and `cloud list` (which marks a routine registered for another
endpoint) all use it. T35b's test now checks that a written-otherwise origin
is the same one and that other hosts, ports, schemes and unparsable text are
not.

### The checks on a task read for a card and for a cloud task

**Issue.** `consents::unshowable` gave its reasons in terms of "the owner's
card", which a `cloud run` member would have read in their refusal.

**Solution.** It is `pub(crate)`, and its reasons say what the characters do
wherever the task is shown ("which don't show where it is read", "which can
push the rest of a line out of view"). The agentctl error and the card's
tests match on the parts that stayed. `cloud run` drops joiners and
presentation selectors first, as `agentctl private` does, so an emoji such
as 👨‍💻 reaches the session as its parts.

### What `cloud list` shows of a task

**Issue.** The plan asks for each task's first line, cut to 60 characters,
as literal text, escaped on Slack. Command replies are Markdown rendered for
each surface, and Slack's renderer already escapes `&`, `<` and `>`
everywhere, so escaping the line again would show `&amp;lt;`, while plain
text would let the task's own Markdown, links and mentions format the reply.

**Solution.** The line is a code span, as agent listings show members'
names: nothing in it formats, links, mentions or broadcasts on either
surface, and the renderer escapes it on Slack. Slack can't show a backtick
in inline code, so backticks are left out; a line with nothing else shows
as "(nothing to show)". The cut is by characters and ends in `…`.

### Smaller choices

- `cloud rm` joins the commands a ban leaves, so the ban replies (`me`,
  the refusal and `admin ban`'s) now name it.
- A routine whose stored token no longer opens or parses
  (`StoreError::Seal` or `Corrupt` on `cloud_routines`) is answered by
  asking the member to `cloud add` it again, which replaces the row without
  reading the old token; other store failures are the usual "something went
  wrong".
- The reply never shows `error_type`, which the endpoint chooses: each
  status gets the failure table's fixed line, worded tentatively for 403
  and 404 as T35b's review asked. A 429 says when the limit resets, in
  whole minutes rounded up, from `Retry-After`.
- `cloud add` replies with the routine id, also when it replaces a label,
  and says which label already holds a routine registered twice.
- `logout` says to revoke the tokens only when it deleted routines; a
  member with only hand-offs left loses them without a word about tokens.
- The notifier sends a hand-off's notice to each of the member's identities
  a manager bot reaches, as the relink notice does, and leaves a member
  none reaches owed without a claim, so another instance or a later
  configuration can send it. It logs each row it marks `unknown` as a
  warning, with the hand-off, member and routine ids.
- `a_replayed_slack_command_fires_once` sends the same signed request
  twice, timestamp included, which is what a replay is; the ingress drops
  the second by its signature before the intake sees it.
