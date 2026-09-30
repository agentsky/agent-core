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
`[proxy] allow` is the sandboxes' egress allowlist: the hosts they may open
HTTPS tunnels to through the proxy listener. It is empty by default, and
`api.anthropic.com`, IP addresses, and hosts that resolve to loopback,
link-local (cloud metadata), agentd's or private addresses are refused
whatever it says. `max_tunnels` and `max_session_tunnels` cap the open
tunnels in all and per sandbox. The same listener is the credential proxy,
sandboxes' `ANTHROPIC_BASE_URL`: it swaps the placeholder a sandbox holds for
the real credential and forwards to `[proxy] upstream`
(`https://api.anthropic.com`), which must be `https://`, or `http://` to a
loopback IP address for a test's fake; agentd logs a warning when it isn't
the default.
Turns run only with a `[sandbox]` section: agentd then connects to the Docker
daemon at startup, stops every container a previous run of the same
`[sandbox] instance` left, and runs one container per active session from
`[sandbox] image`, on the network `[sandbox] network` names. That network
must be an existing `internal` Docker network, named exactly, and sandboxes
must not reach each other on it: an internal network alone doesn't stop that,
so turn inter-container traffic off and let only agentd's ports 8080 and 8081
through, as `deploy/compose/compose.yaml` and `isolate-sandbox.sh` do.
`[runner]` sets the `claude` executable, the turn timeout, how long idle
containers stay warm, and how many run at once. Sandboxes reach agentd as
`cred-proxy.internal:8080` and `agentctl.internal:8081`, so give agentd those
names on the sandbox network and keep those ports: with `[sandbox]`,
`internal.proxy_listen` must use port 8080 and `internal.ctl_listen` 8081.
`GET /healthz` on the public listener answers 200 while the database does.
It also serves Slack's request URLs, `/slack/b/<binding>/events`,
`…/interactivity` and `…/commands`; the manager app's binding is `manager`,
and its requests are verified with `AGENTD_SLACK_MANAGER_SIGNING_SECRET`
(see [Slack](#slack) below).
The ctl listener serves the agentctl API that sandboxed agents call back
through; at startup agentd deletes every agentctl token and scope lock and
empties `ctl-outbox/` under `store.data_dir`, since the containers they
belonged to are gone.
With a `[rocketchat]` section and `AGENTD_RC_MANAGER_TOKEN` (the manager
account's personal access token), agentd connects as the manager bot and takes
commands: a direct message to it is a command as a whole (`login`, `me`), and
a message that starts with `!agent` is one too in any other room one of
agentd's bots is in. Replies always come as a direct message from the manager
bot. A login code or API key posted outside that direct message is refused and
the member is told to start again or revoke the key. Members whose Claude link
breaks get a direct message saying so, retried with a growing wait for about
three days if it can't be delivered. The manager posts every reply, so give its role
`api-bypass-rate-limit`, or Rocket.Chat's REST rate limiter will delay
replies when many members use commands at once.
A linked member creates an agent with `create <name> [persona]`: the manager
creates a bot user named `<name>` (or `<owner>.<name>` when that is taken),
which logs in once to create its own personal access token, so the `bot` role
needs `create-personal-access-tokens`, and the manager `create-user`. agentd
listens as every agent's bot from then on, and again after a restart. Owners
add a bot to a room with Rocket.Chat's own invite; `!agent create` in a room
the manager is in adds it there, which needs `add-user-to-joined-room`.
`persona <name> <text>` (or a `persona.md` attached to that command in the
manager's direct message, up to 64 KB), `pause`, `resume` and `delete` work
for the owner only, and `list [@member]` shows the agents. A member may have
`agents.max_per_owner` agents (default 10); deleted ones don't count. `delete`
deactivates the bot user, which needs `edit-other-user-active-status`; a
deactivation that fails is retried for about three days. A bot sets
`rocketchat.avatar_url` as its own avatar, if configured. With `[sandbox]`,
an agent answers a person who mentions it, replies in a thread it started,
or DMs it, and another agent's message that mentions it, in the thread, as
its own bot, on the Claude account of whoever asked, never the owner's
unless the owner asked, and on the model `[runner.models]` gives that
account's plan. Its bot reacts with `[runner] working_emoji` while the turn
runs, and answers only in rooms it was added to. A member without a linked
account runs on the community API key if a community admin set one, and
otherwise gets a direct message from the manager bot saying how to link an
account; a member whose link stopped working is asked to link it again
instead, and never runs on the community key. When the account or key a
turn ran on hits its usage limit or is refused, the thread is told whose it
was, and the member who asked gets a direct message from the manager bot,
at most once an hour for each kind of failure. Without `[sandbox]`, each agent reacts
with :eyes: to messages that mention it.
Every agent has a built-in `agentctl` skill, and its owner adds more with
`skill add <name> <https Git URL>[#ref]`, or with a `SKILL.md` or `.zip`
attached to `skill add <name>` in the manager bot's direct message, and
removes them with `skill rm <name> <skill>`. agentd clones with the `git`
program (2.37 or later, which the agentd image has) directly from agentd's
own network, only over `https` and only from a host whose addresses are all
public, and keeps skills in `skills/` under `store.data_dir`, mounted
read-only into the agent's sandboxes. A skill whose `SKILL.md` lists
`allowed-hosts` waits until the owner confirms them with
`skill confirm <name> <skill>`; those hosts, each named in full (no
wildcards), then extend `[proxy] allow` for that agent's sandboxes only.
The owner lists an agent's sessions with `sessions <name>` (where each is,
its last turn, and whether its container is warm), and starts them over
with `reset <name>`, or only the ones of one conversation with
`!agent reset <name> here` sent there; a reset stops the session's warm
container once its running turn ends.
Each turn is billed to whoever asked, and `me` shows the turns and tokens
billed to the member today and this month (UTC). The owner caps how many
requests an agent takes a day from others with `limits <name> turns=N/day`,
and how long a chain of agents may reach it with `hops=N` (`off` removes
either); `allow <name> <target>` and `deny <name> <target>` say who may use
it, a target being `@member`, `#channel` or `everyone`. The first `allow`
limits the agent to its targets, `deny` wins, `allow` of a denied target
lifts the deny, and `allow <name> everyone` opens it to everyone not denied
by name; the owner may always use their own agent. `[limits]` caps every
thread outside one-to-one DMs, whatever agents are in it:
`thread_turns_per_hour` (default 30) and `thread_tokens_per_day` (default
2,000,000), and chains of agents at `max_hops` (default 3). A capped agent
says so once per thread and window; when an agent refuses someone because
they are banned or denied, the manager bot tells them privately, at most
once a day.
Community admins are the member identities `[community] admins` lists, as
`<surface>:<team>:<user>`. An admin sets the community API key with
`admin api-key set <key>` in the manager bot's direct message (or with
`/agent admin api-key set <key>` on Slack) and removes it with
`admin api-key clear`. agentd stores it encrypted with the master key, never
logs it, and only the credential proxy uses it: sandboxes hold a placeholder.
`me` tells an admin whether a key is set. An admin bans a member with
`admin ban @member [reason]`, which covers every identity they linked:
agents refuse their requests, and `me` is the only command they may run,
until `admin unban @member`. Admins can't be banned.
On SIGTERM or SIGINT agentd stops accepting connections and messages and
gives running turns and in-flight requests `server.drain_timeout_secs` to
finish; a turn still running then is dropped, and its thread told to ask
again. A second signal drops them at once. Logs go to standard error,
human-readable on a terminal and one JSON object per line otherwise.

### Slack

agentd serves one Slack workspace through its manager app, the one app that
declares `/agent`. Install it once:

1. Put agentd's public listener behind a TLS terminator at a public HTTPS
   URL, such as `https://agentd.example.com`, and set it as
   `[slack] public_url`. Slack sends every event, command and interaction
   there, and agents' apps are created with request URLs under it. The URL
   is baked into each agent's app when it is created, so changing it later
   leaves existing apps, and their install links, on the old one.
2. Fill in the manifest template with that URL:

   ```bash
   PUBLIC_URL=https://agentd.example.com envsubst '$PUBLIC_URL' \
     < deploy/slack/manager-manifest.yaml > manager-manifest.yaml
   ```

3. At <https://api.slack.com/apps>, choose "Create New App", "From a
   manifest", pick the workspace and paste the result. Then install the app
   to the workspace ("Install App").
4. Give agentd the app's secrets: the "Signing Secret" under "Basic
   Information" as `AGENTD_SLACK_MANAGER_SIGNING_SECRET`, and the "Bot User
   OAuth Token" (`xoxb-…`) under "OAuth & Permissions" as
   `AGENTD_SLACK_MANAGER_BOT_TOKEN`. Restart agentd. At startup it asks
   Slack which workspace, bot user and app the token belongs to, and doesn't
   start if Slack refuses the token.
5. Slack checks the events URL when it creates the app, and agentd answers
   only once it has the signing secret. If "Event Subscriptions" says the
   request URL isn't verified, click "Retry" there now.

Members then send `/agent login` to link their Claude account; its reply,
like every command reply, is visible only to them. To let agentd create
their agents' apps, each member generates an app configuration token under
"Your App Configuration Tokens" at <https://api.slack.com/apps> and sends
`/agent slack-token <token> <refresh token>`. agentd renews it before its 12
hours run out and stores it encrypted; `/agent logout` deletes it, and so
does leaving the workspace. A direct message to the manager app works as a
command too, like on Rocket.Chat, and `/agent me` names the app answering,
so members notice if another app takes `/agent` over.

On Slack every agent is an app of its own. `/agent create <name> [persona]`
creates it from a manifest with the member's configuration token: its bot
user is named after the agent, it declares no slash command, and it hears
every message in the channels, DMs and group DMs its bot is in (but answers
only when mentioned, in its own threads, or in a DM). The manager app then
DMs the member a link that installs it; after "Allow", Slack sends the member
back to `<public_url>/slack/oauth/callback`, which stores the app's bot token
and tells the member. In a workspace that requires app approval the click
sends an admin a request instead, and agentd reminds the member once if the
app still isn't installed after `[slack] install_reminder_secs` (default an
hour). Once it is installed, the manager app tells the member which bot
user to invite to a channel (names are unique per owner only, so it names the
bot by mention), and they mention it there. Before it acts on any message it
doesn't ignore, agentd reads the message back from Slack with the agent's
bot token and routes Slack's copy, not the event, since the owner holds the
app's signing secret and could otherwise forge messages, their own included,
in other members' conversations and threads. `/agent delete` deletes the app with the member's
configuration token, or, without a working one, stops answering as it and
says to delete the app at <https://api.slack.com/apps>. Apps ask for
`chat:write.public` only with `[slack] public_posting = true`. On the free
plan a workspace allows 10 app installs, the manager app included.

## Development stack

[`deploy/compose`](deploy/compose/README.md) runs Rocket.Chat, MongoDB and
agentd with Docker Compose, on the two networks the plan describes, and
builds the sandbox image agentd starts sessions from. Its README walks
through bringing it up, setting up the Rocket.Chat manager, configuring
agentd, and the manual live checks. The images are built from
[`images/sandbox`](images/sandbox/Dockerfile), which pins the Claude Code
version with the `CLAUDE_CODE_VERSION` build argument and checks the
download against a pinned SHA-256, and
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
containers. `docker_real_claude_starts` runs the real `claude` from the
sandbox image, which the job builds first: build it as
`docker build -f images/sandbox/Dockerfile -t agent-core/sandbox:dev .`, or
name another tag in `AGENT_CORE_SANDBOX_IMAGE`.

The `images` job builds both images through the Compose file, without
pushing them, adds the iptables rules from
`deploy/compose/isolate-sandbox.sh`, and runs `scripts/ci/compose-test.sh`:
the sandbox image prints the pinned `claude --version`, runs as uid 10001
and has no `node`, and a container on the Compose `sandbox` network reaches
agentd's ports 8080 and 8081 but not its public port, Rocket.Chat, MongoDB,
the host, another sandbox or the internet. `deploy/compose/README.md` says
how to run it locally.

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
