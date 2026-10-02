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

### Tests wait for what they assume, not for time

**Issue.** A fixed sleep before a step that assumes something has happened
passes on an idle machine, then fails or stops testing anything under load.
Auth tests slept 100 ms and assumed a token response delayed 300 ms was
still on its way. `concurrent_callers_share_a_failed_refresh_of_an_expired_token`
gave its five callers the 503's 200 ms delay to join one refresh; a caller
that joins after it lands starts its own, since an expired token is always
retried, and sends a second request. Adding 250 ms before callers 2 to 5
start failed it. The plan-after-lock test slept 1 s, then checked a profile
that was delayed 1.5 s, so it never saw the profile land.

**Solution.** `testkit::Held` is a wiremock responder that holds its
response until the test releases it. A test waits, with a 30 s bound, for
the request to arrive, acts, then releases it. Holding blocks the mock
server's thread, so the `Hold` is declared after the server and released
before anything else asks the server. The expired-token test also waits
until all five callers have joined: the in-flight map holds the refresh's
`watch::Sender`, and a doc-hidden `Auth::refresh_waiters(member)` returns
its receiver count. That sender keeps the channel open, so the guard that
removes the map entry goes into the refresh task when it is spawned, and a
task dropped before its first poll still frees it. The plan-after-lock test
waits for the profile request and checks 2 s after it. A sleep stays only
as a settle before asserting that nothing more happened.

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
where it lives in `apps/meteor/app/api/server/`. On 2026-10-02 it was checked
against a live Rocket.Chat 7.13.9 Community Edition server; see
[The live check against 7.13.9](#the-live-check-against-7139) for what held,
what didn't, and the roles the manager needs.

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

**Solution.** Use the other route, which works for a manager with only the
roles the design gives it: `RestClient::issue_bot_token` logs in as the bot
with its random password (`POST login`), calls
`users.generatePersonalAccessToken`, then
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
(`<name>@<something>.invalid` passes the default checks). With
`Accounts_EmailVerification` on (off by default), password login refuses
unverified emails (`validateLoginAttempt` in `startup.js`; live, HTTP 401
`error-invalid-email`, which the client reports as `Unauthorized`, though the
saved logs don't keep that answer), so such a server needs email 2FA auto
opt-in off and `verified: true`.

### What the server source says about the manager's custom role

**Issue.** The design leaves the custom role open. The source narrows it
down, and the live check confirmed the rows agentd relies on (see
[The live check against 7.13.9](#the-live-check-against-7139)). It didn't
exercise `users.create` with `active` (agentd never sends it), the
`manage-moderation-actions` alternative for `users.setActiveStatus`,
`assign-admin-role` or `Accounts_AllowUserAvatarChange`; those rows rest on
the source alone:

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
`api-bypass-rate-limit` once the manager bot posts DMs (T13). The live check
settled the design's open question.

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
`with_max_retry_wait` (61 s by default, the server's default window plus a
second; see [Clock skew defeated the 429 retry](#clock-skew-defeated-the-429-retry));
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
shorter. The default maximum is therefore 61 seconds, not the window's 60:
live, a burst that hits the limit sees a reset about 60 s away, and with
`Date` truncated the wait measured 60.13 s, which a 60 s maximum refused.
`FakeRest::rate_limit_at` sends a 429 from a skewed server clock, with a
`Date` in whole seconds and a reset measured from it.

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

### The live check against 7.13.9

**Setup.** On 2026-10-02, `rocketchat/rocket.chat:7.13.9` and `mongo:7.0` as
a one-node replica set, started like the T16 Compose file but with only those
two services, the admin created from `ADMIN_USERNAME`/`ADMIN_PASS`. The server
is the Community Edition: `licenses.info` lists no active modules. An ignored
integration test, not committed, drove `RestClient` for every method; the
admin's steps (`permissions.update`, rooms, integrations) and the raw bodies
below were plain REST calls.

**Custom roles need an Enterprise license.** `roles.create` answers HTTP 400
`{"success":false,"error":"This is an enterprise feature [error-action-not-allowed]","errorType":"error-action-not-allowed"}`,
and so does `roles.update` on the built-in `bot` role. `users.create` with a
role id that doesn't exist fails with `The field Roles consist invalid role id
[error-action-not-allowed]` (`details.action: "Assign_role"`). On the
Community Edition the only lever is which built-in roles hold a permission.

**The role.** The manager holds the built-in roles `bot` and `app`, the
manager's extra permissions are added to `app`, and `create-personal-access-tokens`
is added to `bot`. `app` is a global built-in role whose other holders are
Apps-Engine app users, which have no password or token to call the REST API
with. Nothing in the 7.13.9 server treats the role specially beyond its default
permissions, which already include all of `bot`'s, `api-bypass-rate-limit`
among them. Adding the permissions to `bot` instead would give them to every
bot in the workspace, agentd's agents and other integrations alike, and the
`livechat-*`, `guest` and `anonymous` roles carry omnichannel or guest
behavior. With an Enterprise license the same permissions go on a custom role.
Every row below was run with the permission missing, then present:

| What agentd does | Endpoint | Permission, on the manager unless noted | Without it (HTTP, body, client error) |
| --- | --- | --- | --- |
| The manager's own token | `users.generatePersonalAccessToken` | `create-personal-access-tokens` on a role it holds (`bot`) | 400 `Not Authorized [not-authorized]`, `Forbidden("not-authorized")` |
| Create a bot | `users.create` with `roles: ["bot"]` | `create-user` only: no `assign-roles`, no `edit-other-user-info` | 400 `Adding user is not allowed [error-action-not-allowed]`, `Forbidden` |
| The bot's token | `login`, `users.generatePersonalAccessToken`, `logout` | none on the manager; `create-personal-access-tokens` on `bot` | 400 `not-authorized`, `Forbidden`; the bot user stays behind |
| Deactivate, reactivate | `users.setActiveStatus` | `edit-other-user-active-status` | 403 `User does not have the permissions required for this action [error-unauthorized]`, `Forbidden("error-unauthorized")` |
| Invite into a room the manager is in | `channels.invite`, `groups.invite` | `add-user-to-joined-room` (default: `owner`, `moderator`, not a plain member) | 400 `Not allowed [error-not-allowed]`, `Forbidden` |
| Invite into another room | same | `add-user-to-any-c-room`, `add-user-to-any-p-room` | the same |
| Read another user's roles | `users.info` | `view-full-other-user-info` | 200 with no `roles` field (only `_id`, `active`, `avatarETag`, `canViewAllInfo`, `name`, `status`, `type`, `username`, `utcOffset`) |
| Rename another user | `users.update` | `edit-other-user-info`, and a token that bypasses 2FA | 400 `Editing user is not allowed [error-action-not-allowed]`, `Forbidden` |
| Another user's avatar | `users.setAvatar` | `edit-other-user-avatar` | 403 `{"error":"unauthorized"}`, `Forbidden("unauthorized")` |
| Room details | `rooms.info` | none for a public room or one it is in | private room it isn't in: 400 `error: "not-allowed"`, `Forbidden`; unknown: `NotFound("error-room-not-found")` |
| Open a DM | `im.create` | `create-d` (`bot` has it) | |
| Not be rate limited | every call | `api-bypass-rate-limit` (`bot` and `app` have it) | twelve `me` calls in a row passed |

A bot renames itself and sets its own avatar with its own token and no
permission, so agentd needs `edit-other-user-info` and `edit-other-user-avatar`
only if the manager edits bots. The Community Edition recipe is therefore: the
manager holds `bot` and `app`; `app` gains `create-user`,
`edit-other-user-active-status`, `add-user-to-joined-room` and
`view-full-other-user-info`; `bot` gains `create-personal-access-tokens`.
`users.create`'s answer shows the new user's `roles` only to a caller with
`view-full-other-user-info` (otherwise `[]`), so nothing should read the
role from it.

**Two-factor authentication.** A personal access token made without "Ignore
Two Factor Authentication" passes 2FA-gated endpoints only during the
registration grace (`Accounts_TwoFactorAuthentication_RememberFor`, 1800 s
after the user's creation). With the grace cut to one second, `users.update`
and `users.generatePersonalAccessToken` answered 400 `TOTP Required
[totp-required]` (`details.method: "password"`) for such a token and for a
login session without the `x-2fa-*` headers, while a token with the bypass
passed. The manager's token must be created with the bypass. With a verified
email and default settings, the password login itself answers 401
`totp-required` with `method: "email"`, which is why bots are created with
`verified: false`. These refusals were seen during the run but the saved
logs don't keep them: they hold only the within-grace success
(`users.update name (manager PAT without bypass): ()`). They match the
server source, and a rerun should save them.

**A workspace that can't reach Rocket.Chat Cloud can't post.** 7.13.9 restricts
a Community Edition workspace that hasn't reported statistics to
`collector.rocket.chat` within ten days, and one that never has (no stats
token) from its first start (`AirGappedRestriction` in `@rocket.chat/license`,
applied by `ee/server/patches/airGappedRestrictionsWrapper.ts`; any valid
license lifts it). The test container's egress allowed no Rocket.Chat Cloud
host, so `Cloud_Workspace_AirGapped_Restrictions_Remaining_Days` read 0 and
`chat.postMessage`, `chat.update` and `rooms.mediaConfirm` answered 400
`{"success":false,"error":"restricted-workspace"}`, which the client reports
as `Api("restricted-workspace")`. Reads, reactions, invites and user
management were unaffected. A deployment on the Community Edition needs
outbound HTTPS to Rocket.Chat Cloud's statistics collector, or a license.
Posting, editing and the upload's confirm step were therefore not run end to
end. `rooms.media` itself accepted the client's multipart upload (stored
complete, expiring after 24 hours unless confirmed). The messages the read
paths were checked against were posted as the bot and the manager through
incoming webhooks, which 7.13.9 doesn't restrict and which run the same
`sendMessage` as `chat.postMessage`.

**What held.** `chat.react` with `shouldReact: true` twice left one `:eyes:`
reaction, with and without colons. `chat.getThreadMessages` returned the
replies newest first without the root; an unknown root is
`NotFound("error-invalid-message")`. `channels.history`, `groups.history` and
`im.history` returned top-level messages only, newest first, and `latest` set
to a message's `ts` left that message out. A room the bot isn't in is
`Forbidden("unauthorized")` (public) or `NotFound("error-room-not-found")`
(private); `chat.getMessage` there is `Forbidden("error-not-allowed")`, and
`chat.react` `Forbidden("not-authorized")`. A wrong token is 401 `You must be
logged in to do this.`, `Unauthorized`. A taken username is
`Api("… is already in use :( [error-field-unavailable]")`. REST messages
carry `ts` as ISO 8601 with milliseconds, `u` with `_id`, `username` and
`name`, `mentions[]` with `_id`, `username`, `name` and `type`, and system
messages `t` (`au` with the added username as `msg`).

**What didn't, and was fixed.**

- `im.create` with a username it doesn't know, including one that differs
  only in case, answers success with the caller's self-DM
  (`{"room":{"t":"d","usernames":["admin"],…},"success":true}`), so
  `create_dm` returned a room that wasn't the requested DM. It now checks that
  the room's `usernames` include the name and otherwise fails with
  `NotFound("error-invalid-user")`. `FakeRest` answers the same way.
- `chat.getMessage` with an unknown id answers a bare `{"success":false}`
  (HTTP 400, `API.v1.failure()`), which mapped to `Api("HTTP 400")`.
  `get_message` now maps it to `NotFound("message")`, and `FakeRest` sends that
  body.
- The 429 retry. With the default 60 s maximum, the client gave up on the first
  429 of a burst: the reset was 59.9 s away and the wait measured against the
  whole-second `Date` came to 60.13 s. The default is now 61 s; live, the same
  call then waited out the window and succeeded.

**Other shapes, for T12 to T14.**

- Every response to a caller without `api-bypass-rate-limit` carries
  `x-ratelimit-limit` (10), `x-ratelimit-remaining` and `x-ratelimit-reset` (an
  epoch time in milliseconds). The 429 body is
  `{"success":false,"error":"Error, too many requests. Please slow down. You must wait 60 seconds before trying this endpoint again. [error-too-many-requests]"}`,
  with no `errorType`. `login` has no caller to exempt, so it is limited per
  client address like any route: a burst of logins from one host, the bots'
  token logins included, gets the same 429.
- A message from a user without `mention-all` or `mention-here` (`bot` has
  neither) that contains `@all` or `@here` is refused whole with
  `error-action-not-allowed`, not stripped, so the neutralizing in `render`
  is required, not cosmetic.
- `users.setAvatar` from a URL refuses private addresses
  (`checkUrlForSsrf`): `http://172.18.0.1:8765/avatar.png` got
  `Api("Invalid avatar URL: … [error-avatar-invalid-url]")`, while a public
  `https` image worked. The server also refuses redirects and anything not
  `image/*`. T14 needs a public avatar URL.
- The messages posted through integrations carried `bot` (`{"i":
  "<integration id>"}`). Whether a bot user's own `chat.postMessage` does
  could not be checked, so the sender's `bot` role stays the reliable signal.
- Realtime, over DDP on 7.13.9: `login` with `resume` returns `id`, `token`,
  `tokenExpires` and `type: "resume"`; the server pings every 30 s;
  `stream-room-messages` sends `ts` and `_updatedAt` as `{"$date": ms}`; a
  thread reply makes the server send the root again, same `_id`, with
  `tcount`, `replies` and `tlm` and no `editedAt`. `__my_messages__` delivered
  messages from a joined channel and a DM, each with a second argument such as
  `{"roomParticipant":true,"roomType":"c","roomName":"…"}`.
  `<uid>/subscriptions-changed` sent `inserted` when a DM or channel was
  created with the user and `updated` on new messages.

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
between every surface, so the manager's roles also need
`view-full-other-user-info`; on the Community Edition the admin adds it to
`app` ([T11's live check](#the-live-check-against-7139)), and the design's
Rocket.Chat section says so. A sender is a bot when the message has a
non-false `bot` field or the sender has the `bot` role. `RestClient` gains `user_info`, and `FakeRest` shows roles
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
