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
keeps combining marks, joiners, skin-tone modifiers and flag pairs with the
character before them. A property test renders generated Markdown with
`to_mrkdwn` and checks that no cut lands inside a token or an entity. The
crate docs state the order.

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
renderer only accepts a username made of letters, digits, `.`, `_` and `-`
that isn't `all` or `here`, so a directory entry can't turn a mention into a
broadcast. `@all` and `@here` get the same zero-width space as Slack's
broadcasts. Code spans, code blocks, link destinations, autolinks and bare
URLs are left alone, found with the same `pulldown-cmark` parse
(`render::verbatim`), so `https://x.io/@all` keeps working. The bare URL
scanner moved from `slack.rs` to `render::url` to be shared.

### Directive details qm-core decides and T07 doesn't

**Issue.** T07 names only `[[react: <emoji>]]`. qm-core's `extractReactions`
also accepts several names per directive, names with colons, literal emoji
characters (through a generated table of about 1,800 entries), and a target
message after `@`, caps a reply at five reactions, strips an unclosed
`[[react:` running to the end of the text, and trims every line of the reply,
code included, whenever it removed something.

**Solution.** `directives::extract` accepts several names separated by spaces
or commas, strips colons, lowercases, keeps only valid short names
(`[a-z0-9_+'-]+` with an optional `::skin-tone-2` to `-6`), drops duplicates
and returns at most `MAX_REACTIONS` (5). Literal emoji characters are dropped:
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
channel rule to both.

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

**Solution.** Only a refusal marks the link broken: HTTP 400 (`invalid_grant`
and the like), 401 or 403 from the token endpoint. Any other failure leaves
the link as it is. If the current access token hasn't expired yet (a refresh
starts 5 minutes early) it is returned; otherwise the error is. A broken link
returns `RelinkRequired` at once, without calling the endpoint again, until
the member logs in. `RelinkRequired { newly_broken }` carries what
`mark_claude_link_broken` returned, so T13 sends one notice per failure.

### `put_claude_link` would let a refresh undo a logout

**Issue.** `put_claude_link` is an upsert. A refresh that read the link, then
waited on the token endpoint while the member logged out, would store its
result and link the member again.

**Solution.** Two layers. Every write of a member's link in `auth` (refresh,
login, logout) holds that member's lock from the `KeyedLocks` the
single-flight refresh uses, so a logout waits for a refresh in flight and then
deletes its result. And a refresh stores through a new store method,
`update_claude_link`, a plain `UPDATE` that returns false when there is no
row, so even a delete that bypasses `auth` isn't undone; `auth` then revokes
the orphaned refresh token and returns `NotLinked`. A login still upserts,
since it is meant to create the link.

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
  repeated `login` commands.
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
unspecified listen address, a public address inside the sandbox subnet, the
drain timeout bound, the log filter, a non-SQLite URL, every environment
variable) produce the same `key: message` form. Values are never repeated,
except the offending value of a known, non-secret key in a type error.

### The public listener's subnet guard needed a key

**Issue.** The plan's network section says the public listener also refuses
connections from the sandbox subnet, but no key said what that subnet is, and
no task was named for it.

**Solution.** T10 builds the public listener, so it adds
`internal.sandbox_subnet` (CIDR, required) and `net::RefuseSubnet`, which
closes such connections as soon as they are accepted, before any byte is
read. Validation refuses a `server.listen` inside the subnet. A Linux-only
test binds the public listener to `127.0.0.2`, sets the subnet to
`127.0.0.1/32`, and checks that a client bound to `127.0.0.1` gets no answer
while one bound to `127.0.0.3` gets 200.

### `migrate` needs the master key

**Issue.** `agentd migrate` only needs the database, but `Store::open` takes
a `Sealer`, so the key has to be present.

**Solution.** `migrate` loads the full configuration, key included, like
`serve`. That also means a migration job checks the configuration it will be
served with. The store gained `ping` (for `/healthz`) and `close` (so
shutdown closes the pool after the drain instead of leaving it to drop).

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

**Solution.** The wait is the header minus the local clock, floored at zero,
and one second when the header is missing or unreadable. A 429, or either
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
bounded queue. A full or closed queue answers 503, so Slack retries; the
handler never waits for it. `Queue::run` then deduplicates through the
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
are not scanned. Each user appears once, in order of first appearance.

### A misspelled manager secret went unnoticed

**Issue.** T10 accepts any `AGENTD_SLACK_MANAGER_<NAME>`, so a misspelled
`…_SIGNING_SECRET` would silently leave the manager binding unknown.

**Solution.** When any `AGENTD_SLACK_MANAGER_*` variable is set,
`AGENTD_SLACK_MANAGER_SIGNING_SECRET` must be too; the error asks whether one
is misspelled. The manager is known exactly when the secret is set, and
agentd logs at startup which it is.

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
already has names. `render_with` takes an explicit directory for callers
that have one. T23's delivery step and T29's member-cache bullet say so.

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

**Solution.** The member directory maps each active member's display name,
full name and username, compared ignoring case and runs of white space. A
name that belongs to more than one member resolves to no one, so it stays
text: a missed mention is better than pinging the wrong person. Deactivated
members are left out. If a refresh fails while an older list exists, the
older list is kept and the next attempt waits a minute.

### Reading a thread's newest messages

**Issue.** `Surface::history` wants the newest `limit` messages before a
cursor, but `conversations.replies` pages from the thread's oldest message
(root first) and has no reverse order.

**Solution.** A thread read follows every page (with `latest` set to the
cursor and `inclusive=false`) and keeps only the last `limit` content
messages, filtering by `ts` on the client too. `conversations.history`
pages from the newest, so a top-level read stops once it has `limit`.
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
