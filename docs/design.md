# agent-core design

Status: draft for review

## Context

A small community wants personal AI agents that live in its chat. Each member
runs one or more agents on their own Claude subscription. Anyone in a shared
channel can address an agent with `@name`, and agents can address each other
the same way. The community uses Slack (mostly one workspace per member or
small group) and Rocket.Chat (large shared workspaces).

The reference point is qm-core[^qm], a TypeScript agent platform with a mature
Slack surface. It runs one core deployment per organization, spawns a Claude
Code CLI process per turn on the core host, and bridges its tools to the CLI
through an in-process MCP server. That shape is heavy per agent: one
separately mentionable agent means one full core deployment. agent-core keeps
the parts of qm-core that work well (mention gating, scope separation, consent
flows, Markdown rendering, credential swapping at egress) and changes the shape
so that one small Rust process serves many agents.

## Goals

- Many separately `@mentionable` agents per community, owned by individual
  members.
- Members link their own Claude subscription with a PKCE login done entirely
  in chat.
- Manage agents with chat commands (`/agent ...`) instead of a web UI.
- Pure Rust core. No Node.js or npm dependencies in anything we build. The
  Claude Code CLI runs inside the sandbox as a separate executable.
- Slack and Rocket.Chat behind one platform-neutral core.
- Per agent and scope sessions that survive restarts, using Claude Code's own
  session persistence (`--session-id` / `--resume`).
- Requests are billed to the person who made them.

## Non-goals

- A web dashboard. A small HTTPS endpoint for OAuth callbacks and platform
  events is fine.
- Driving claude.ai through a browser or any other automated consumer UI.
  Consumer Terms section 3 prohibits automated access except through an API key
  or where explicitly permitted[^terms].
- Calling the Messages API directly with subscription OAuth tokens. Subscription
  tokens are only used by the Claude Code CLI.
- Slack Connect in the first releases. The design keeps it possible (see
  [Slack Connect](#slack-connect)).
- Microsoft Teams, Discord, email.

## Terminology

| Term | Meaning |
| --- | --- |
| Member | A person in a chat workspace. Identified by `(surface, team, user)`. |
| Linked member | A member who completed the Claude PKCE login. |
| Agent | A persona (prompt, skills, sandbox image) owned by one member, with one chat identity per surface binding. |
| Surface | A chat platform adapter: Slack or Rocket.Chat. |
| Scope | Where a conversation happens: a DM, a channel, or a group DM. Scopes decide sandboxes and what an agent may touch. |
| Session | One Claude Code conversation, resumed across turns. |
| Manager bot | The one bot per workspace that owns `/agent` commands, login DMs and consent cards. |

## Architecture

```mermaid
graph LR
    subgraph Chat
        SL["Slack workspaces"]
        RC["Rocket.Chat servers"]
    end
    subgraph agentd["agentd (one Rust binary)"]
        AD["Surface adapters"]
        CMD["Command handler"]
        RT["Router and turn policy"]
        AU["Auth: PKCE, token store, refresh"]
        RUN["Runner: per-session queue"]
        API["agentctl API"]
        PX["Credential proxy"]
        DB[("Store: SQLite, later Postgres")]
    end
    subgraph Sandboxes["Container per session, volume per agent and scope"]
        CC["claude CLI"]
        CTL["agentctl"]
        VOL[("Persistent volume")]
    end
    ANT["api.anthropic.com"]

    SL <--> AD
    RC <--> AD
    AD --> CMD
    AD --> RT
    RT --> RUN
    RUN --> CC
    CC --> PX
    PX --> ANT
    CTL --> API
    CMD --> AU
    AU --> DB
    RT --> DB
    CC --- VOL
```

agentd is a single process. Each surface adapter turns platform events into a
neutral `InboundEvent`. The router decides which agent, session and credential
a turn uses. The runner serializes turns per session and runs `claude -p` in the
right sandbox. Agents act on the world with ordinary CLIs and skills inside the
sandbox. The only core-facing CLI is `agentctl`, which calls back into agentd
with a short-lived token scoped to one turn.

### Why the CLI runs inside the sandbox

qm-core runs the CLI on the core host with every built-in tool disabled and
routes all execution through MCP tools into the sandbox[^qm-harness]. agent-core
wants Claude Code's built-in tools (Bash, Read, Edit) and skills instead of an
MCP schema. Those tools act on the machine the CLI runs on, so the CLI has to
run where the work happens. The sandbox becomes the security boundary, which
also makes `--permission-mode bypassPermissions` acceptable.

## Chat identities and mentions

On both platforms only a real account can be mentioned: a user or a bot user.
A mentionable agent therefore needs its own bot identity.

| | Slack | Rocket.Chat |
| --- | --- | --- |
| Mention in the message | `<@U…>` user id token, produced by autocomplete | `@username` text, parsed by the server into `mentions[]` |
| Agent identity | One Slack app with a bot user per agent | One user with the `bot` role per agent |
| How the bot hears it | `app_mention` event, bot must be a channel member[^slack-mention] | Realtime `stream-room-messages`, check `mentions[]` for the bot's `_id`[^rc-stream] |
| Bot-to-bot mentions | Expected but not yet verified for one app's bot user mentioning another's[^slack-botmention]. `app_mention` is never sent for DMs, which arrive as `message.im` | Delivered |
| Who creates the identity | The member installs the app. Admin approval only if "Require App Approval" is on[^slack-approval] | agentd's manager account with a custom role (`create-user` and token creation)[^rc-create] |
| Scaling limit | 10 app installs on the free plan[^slack-free] | None in practice |

### Slack

Each agent is its own Slack app, created from a manifest.

- **One-time setup per member.** App configuration tokens are only issued in
  the api.slack.com UI[^slack-manifest]. The member generates one there and
  hands the token and its refresh token to agentd with `/agent slack-token`
  (slash command text is not posted to the channel). agentd stores both
  encrypted and rotates them with `tooling.tokens.rotate` before the 12-hour
  expiry. Holding a member's configuration refresh token lets agentd create and
  edit apps as that member, so it is listed in the threat table.
- **Per agent.** `/agent create` calls `apps.manifest.create`. The response
  carries the new app's `app_id`, `client_id`, `client_secret` and
  `signing_secret`, which agentd stores with the binding. agentd DMs the member
  an install link. The member clicks Allow and the OAuth callback, using
  `client_id` and `client_secret`, delivers the bot token. Members can install
  apps without an admin by default. When the workspace requires app approval,
  the click becomes a request and agentd reports that the install is waiting for
  approval.
- **Transport.** All Slack apps, the manager bot included, use HTTPS: the
  Events API, interactivity and slash commands. Socket Mode needs an app-level
  `xapp` token that no API can create[^slack-socket], so it cannot be automated
  for agent apps, and one transport is simpler. agentd's public HTTPS endpoint is
  therefore a prerequisite for `/agent create` on Slack: the manifest's
  `request_url` must answer Slack's `url_verification` challenge when the app is
  created. Each app's requests are verified with its own `signing_secret`.
- **Acknowledge first.** Slack expects an acknowledgement within three seconds
  and retries otherwise. Turns take minutes, so agentd acknowledges every event,
  command and interaction immediately, processes it asynchronously, and drops
  retried deliveries by `event_id`.

Slash commands are neither namespaced nor unique. Two apps can both register
`/agent`, and Slack routes it to whichever was installed most recently, so a
later-installed app can silently take over the command. Only the manager bot
declares `/agent`, agent apps declare no commands, and `/agent me` shows the
manager app's name so members can notice a hijack.

Loop protection is mandatory because bots hear each other: a per-thread cap on
agent turns, a per-thread token budget, agents ignore bot messages that do not
mention them, and only mentions from agentd-managed agents are honored (see
[Agent-to-agent attribution](#agent-to-agent-attribution)).

### Rocket.Chat

The community admin installs agentd once and gives its manager account a custom
role. After that `/agent create` is self-service: agentd calls `users.create`
with the `bot` role, obtains a token for the new user, sets its name and avatar,
and joins the rooms the owner allows. Bot users bypass the REST rate limiter by
default[^rc-perms].

The custom role needs `create-user`, plus `edit-other-user-active-status` if
agentd passes `active` on create, and the permission for creating the bot's
token. It also needs `view-full-other-user-info`: messages don't carry
the sender's roles, and `users.info` shows another user's roles only with it,
which is how agentd tells bots from people. A reviewer's reading of the current server source is that `users.create`
with `roles: ["bot"]` checks only those, and that `assign-roles` is checked on
update only. That needs a test on the target server version.

Custom slash commands on Rocket.Chat require an Apps-Engine app written in
TypeScript[^rc-slash]. agent-core instead takes commands as DMs to the manager
bot or with a `!agent` prefix. Both feed the same parser, so the command set is
identical on both platforms.

## Account linking

Members link their Claude subscription with the OAuth PKCE flow that Claude Code
uses. The authorize page redirects to Anthropic's own callback page, which shows
a `code#state` string for the user to paste back. That fits chat and needs no
public callback URL.

```mermaid
sequenceDiagram
    participant M as Member
    participant B as Manager bot
    participant A as agentd auth
    participant C as claude.ai
    participant T as Token endpoint

    M->>B: /agent login
    B->>A: start_login(member)
    A->>A: random verifier, separate random state, expiry 10 min
    A-->>M: private link to authorize URL (S256 challenge)
    M->>C: sign in and approve
    C-->>M: page shows code and state
    M->>B: /agent login with pasted code (private)
    B->>A: complete_login(member, code, state)
    A->>A: match state, load verifier
    A->>T: authorization_code grant with verifier
    T-->>A: access token, refresh token, expiry
    A->>A: encrypt and store, delete pending login
    A-->>M: linked
```

Rules:

- The verifier never leaves agentd. `state` is a separate random value. qm-core
  sends the verifier as `state`[^qm-oauth], which puts the PKCE secret in the
  authorize URL. We do not copy that.
- Codes are only accepted in private channels: Slack slash command text is not
  posted to the channel, and on Rocket.Chat the code goes in a DM to the manager
  bot, which is already private. `!agent login <code>` in a channel is refused,
  the pending login is invalidated because the code is now public, and the member
  is told to start again. agentd does not delete members' messages. That would
  need `delete-message` or `force-delete-message`, which the `bot` role lacks.
- Tokens are encrypted at rest (ChaCha20-Poly1305, key from the environment or a
  KMS). Refresh is single-flight per member. A failed refresh DMs the member.
- Endpoint URLs, client id and scopes come from configuration. They are Claude
  Code's OAuth parameters, not a published API contract, and can change.

## Turn routing and billing

A member's Claude usage through Claude Code counts against a per-user monthly
Agent SDK credit that cannot be pooled or shared[^sdk-credit]. Consumer Terms
section 2 also prohibits making an account available to anyone else[^terms].
So a turn always runs on the credential of the person who caused it.

| Who starts the turn | Where | Runs on | What the agent can touch |
| --- | --- | --- | --- |
| The owner, in a DM | DM | Owner's credential | Everything the owner granted: repos, memory, cloud hand-off |
| The owner, in a channel | Channel thread | Owner's credential | Public side. A private task the agent requests runs without a consent card (see below) |
| Another linked member | Channel thread | Requester's credential | Public side only: persona, skills, thread context |
| A private task requested during a non-owner's turn | Owner's private sandbox | Owner's credential, after the owner approves a consent card | Owner's private resources for that one task. Only the result and attachments return to the thread |
| Unlinked member | Channel | Community API key if configured, otherwise a "link your account" reply | Public side only |
| Agent to agent | Thread | The requester of the turn whose message mentioned the agent | Public side only, hop-capped |

### Agent-to-agent attribution

A hop is billed to the requester of the turn that produced the mention, never to
whoever started the thread. agentd posts every agent message itself, so it
records `(platform message ref, turn, requester)` for each one. When an agent
message mentions another agent, the new turn inherits that requester and the hop
count. `agentctl ask-agent` carries the same information in its turn-scoped
token. Mentions from bot users that agentd does not manage are ignored. That
keeps an unmanaged or prompt-injected bot from spending anyone's subscription,
and the hop cap bounds what one request can cost its requester.

### Private tasks

Private resources never enter a channel sandbox. Channel volumes persist and
every later turn in that channel can read them with the CLI's built-in tools,
so a checked-out private repository or generated file left there would outlive
the consent it was granted under.

The router cannot tell from message text whether a task needs private
resources, so the agent asks during its turn, the same way qm-core's agents
issue ask-agent requests mid-turn[^qm-askagent]. The request is asynchronous,
like qm-core's. Consent can take hours, and a channel turn that waited would
hold its container, block the thread's session queue, keep its turn token alive
and outlast Claude Code's Bash tool timeout (2 minutes by default, with a
10-minute ceiling unless raised)[^cc-envvars]. The flow:

```mermaid
flowchart TD
    T["Channel turn running"] --> R["Agent runs agentctl private 'task text'"]
    R --> ID["agentctl returns a consent id at once.<br/>The turn tells the thread<br/>it asked the owner, then ends"]
    ID --> O{"Turn requester<br/>is the owner?"}
    O -- yes --> RUN["Fresh session in the<br/>owner's private sandbox"]
    O -- no --> CN["Consent card to owner<br/>shows the exact task text"]
    CN -- approved --> RUN
    CN -- "declined or expired" --> DN["agentd posts the<br/>outcome to the thread"]
    RUN --> RES["agentd posts the reply and<br/>attached files to the thread<br/>as a new agent message"]
```

- The same asynchronous path serves the owner as requester, with no consent
  card. A long private task would otherwise hold the channel turn the same way.
- `CONSENT` records the reply target: surface, conversation, thread root, the
  originating session, the requester and the hop count. Unanswered consent cards
  expire after a configurable time, 24 hours by default.
- Each private task gets a fresh session with a random v4 id on the owner's
  private volume. It never joins the owner's DM session, so a non-owner's task
  cannot read the owner's DM transcript and does not add to it.
- What crosses into the private turn is only the task text shown on the consent
  card and files the channel turn attached explicitly. The channel thread's
  transcript does not cross.
- What comes back is only the private turn's final reply and files it attached.
  agentd posts them to the thread as a new agent message whose `MESSAGE_REF` is
  attributed to the original requester and hop count, so a follow-up mention
  of that message inherits correctly.
- Inside a private task, `agentctl` offers only `attach`. `ask-agent` and
  `private` are refused, so private context cannot flow to other agents and no
  hops can chain on the owner's credential.

### Routing

```mermaid
flowchart TD
    E["InboundEvent"] --> B{"From a bot user?"}
    B -->|"yes, unmanaged"| X["Ignore"]
    B -->|"yes, managed agent"| M{"Mentions this agent?"}
    M -- no --> X
    M -- yes --> H["Inherit requester and<br/>hop count from the<br/>posting turn"]
    B -- no --> G{"Mentioned, reply to agent,<br/>or DM?"}
    G -- no --> X
    G -- yes --> O{"Requester is owner?"}
    H --> O
    O -- yes --> OC["Owner credential,<br/>conversation's scope"]
    O -- no --> L{"Requester linked?"}
    L -- yes --> RQ["Requester credential,<br/>channel scope"]
    L -- no --> K{"Community API key?"}
    K -- yes --> CK["Community key,<br/>channel scope"]
    K -- no --> LK["Reply: link your account"]
```

Response gating is deterministic: an explicit mention, a reply to the agent's
own message, or a DM. Messages from managed agents only count when they mention
this agent explicitly. Replying in a thread is not enough, or two agents in one
thread would answer each other indefinitely. qm-core runs a model call to decide whether to chime in on
unaddressed thread messages. With subscription credentials that would cost a
CLI run per message, so agent-core does not do it.

## Sessions and sandboxes

### Keys

- Volume: one per `(agent, scope)`. A DM transcript never lands on a channel's
  disk.
- Sandbox: one container per active session, with the scope's volume mounted.
  Sessions in one scope can run concurrently for different requesters, and a
  shared container would let one session read another's process environment
  (`/proc/<pid>/environ`), including its proxy placeholder and `agentctl` token.
  With one container per session, container identity and session identity are
  the same thing, which is what the credential proxy and `agentctl` rely on.
- Session: one row per conversation and thread root (DMs use one continuous
  session, channels one per thread), looked up by
  `(agent, surface, team, conversation, thread root)`. Its id is a random UUIDv4
  minted on create and again on `/agent reset`, because `--session-id` needs an
  id that has not been used.
- Directories on the volume: each session gets `sessions/<session id>/work` as
  its working directory and `sessions/<session id>/claude` as its
  `CLAUDE_CONFIG_DIR`, and only `sessions/<session id>/` is mounted read-write in
  its container. Keeping them apart keeps the agent's transcripts and
  `settings.json` out of its own Glob, Grep and `git` scope. `shared/` is mounted into every session of the
  scope and guarded by a scope-level lock that `agentctl` takes for writes.
  Skills are mounted read-only.
- The surface and team are part of every lookup key, so the same agent on two
  platforms, or in a Slack Connect channel seen from two workspaces, keeps
  separate sessions.

### Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Cold
    Cold --> Starting: first message
    Starting --> Running: start claude with session id or resume
    Running --> Warm: turn result
    Warm --> Running: next message (stdin, stream-json)
    Running --> Running: message during turn is queued or steered
    Warm --> Cold: idle timeout, process reaped
    Cold --> Starting: next message resumes from transcript
```

The runner keeps a warm container and `claude` process per active session in
stream-json mode and reaps both after an idle timeout. The next message resumes
from the transcript. Turns are serialized per session with a keyed queue. Two
concurrent `--resume` runs of one session would fork the transcript.

A warm process cannot change credential kind or model mid-flight. A thread can
be driven by a linked member on one turn (OAuth token, `Authorization: Bearer`
with the `oauth-2025-04-20` beta) and an unlinked member on the next (community
API key, sent by the CLI as `x-api-key` from `ANTHROPIC_API_KEY`). The process
environment is fixed at start, and plans differ in model access and rate limits.
The runner therefore restarts the process, resuming from the transcript, when
the next turn's credential kind or model differs from the running one. The model
is chosen per turn from what the requester's plan allows. The plan is read from
the account profile (the `user:profile` scope) at link time and on every token
refresh and stored in `CLAUDE_LINK`. Like the OAuth parameters, that profile is
Claude Code's, not a published contract. Switching the model
over the stream-json control channel instead of restarting is a later
optimization.

`claude` runs as a non-root user in the image. On Linux the CLI refuses
`bypassPermissions` as root or under sudo outside a recognized
sandbox[^cc-bypass].

### Persistence

- Each session's `CLAUDE_CONFIG_DIR` is its own `sessions/<session id>/claude`
  directory, and `CLAUDE_CODE_PROJECT_DIR_NAME` is set to the session id, so the
  transcript directory is named explicitly instead of being derived from the
  working directory (Claude Code 2.1.234 or later)[^cc-sessions].
  `claude --resume <id>` searches every project directory since 2.1.223, so the
  fixed layout is for predictability and backups, not a resume requirement.
- `cleanupPeriodDays` is raised from its 30-day default in each session's
  `settings.json` so idle threads keep their transcripts.
- Each turn's user message carries only what the transcript lacks: thread
  messages the agent did not answer, who is present, and surface hints. The
  system prompt stays byte-identical across turns so prompt caching keeps
  working.
- Volumes are snapshotted. Mirroring transcripts to the store is a later option
  for multi-host deployments.

## Credential proxy

The sandbox never holds a real Claude credential. It gets a placeholder token,
and the proxy swaps it for the real one.

```mermaid
sequenceDiagram
    participant CC as claude CLI in sandbox
    participant PX as Credential proxy
    participant ST as Token store
    participant AN as api.anthropic.com

    CC->>PX: POST /v1/messages, Bearer placeholder
    PX->>PX: authenticate source container
    PX->>ST: lookup (container, placeholder)
    ST-->>PX: real access token (refreshed if stale)
    PX->>AN: same request, Bearer real token
    AN-->>PX: streamed response
    PX-->>CC: streamed response
```

Sandbox environment for a subscription credential:

```text
CLAUDE_CODE_OAUTH_TOKEN=<placeholder>
ANTHROPIC_BASE_URL=http://cred-proxy.internal:8080
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
DISABLE_AUTOUPDATER=1
CLAUDE_CONFIG_DIR=/volume/sessions/<session id>/claude
CLAUDE_CODE_PROJECT_DIR_NAME=<session id>
```

For the community API key, `ANTHROPIC_API_KEY=<placeholder>` replaces
`CLAUDE_CODE_OAUTH_TOKEN`, and the CLI sends it as `x-api-key`.

Tested against Claude Code 2.1.283 with a local capture server:

- The CLI sends the subscription token to a custom `ANTHROPIC_BASE_URL` over
  plain HTTP, as `Authorization: Bearer` with the `oauth-2025-04-20` beta.
- By default it also opens direct connections to `api.anthropic.com` that
  bypass the base URL. With `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` and
  `DISABLE_AUTOUPDATER=1` there were none.
- It may send `HEAD /api/hello` to the base URL as a connectivity check.

Proxy rules:

1. Swap only the credential header, only for the configured upstream:
   `Authorization: Bearer` for a subscription placeholder, `x-api-key` for an
   API-key placeholder. A placeholder of one kind never receives a credential of
   the other kind. Never substitute in bodies or for other hosts. Otherwise the
   model could send the placeholder to an attacker's host and the proxy would
   attach the real token.
2. Mint one placeholder per `claude` process, which is also one per container
   and session. Several sessions in one scope can run at the same time for
   different requesters, and a shared placeholder would give the proxy no way to
   tell which member's credential a request belongs to. The runner points the
   process's placeholder at the current turn's credential when the turn starts.
   Turns within a process are serialized, so the mapping cannot change under a
   request in flight. The mapping is bound to the container's network identity,
   and is revoked when the container is reaped. Another session cannot read the
   placeholder because it runs in a different container.
3. Sandbox egress goes through the proxy and an allowlist only. Block cloud
   metadata endpoints. Direct `api.anthropic.com` is blocked so new side traffic
   fails loudly.
4. The same mechanism serves bearer-token CLIs in skills, for example
   `GH_TOKEN`. Signed-request schemes such as AWS SigV4 cannot be swapped and
   need short-lived credentials instead.

## Tools and skills

There is no MCP server. Agents use Claude Code's built-in tools plus skills in
`$CLAUDE_CONFIG_DIR/skills/`, loaded with `--setting-sources user`. Skills cost
only their name and description in context until used.

Core-facing actions go through `agentctl`, a small static Rust binary:

| Command | Effect |
| --- | --- |
| `agentctl attach <path>` | Stage a file to upload with this turn's reply |
| `agentctl post --to <target> <text>` | Post somewhere else the agent is allowed to post |
| `agentctl react <emoji> [message id]` | Add a reaction |
| `agentctl history [--before id]` | Pull more thread context than the turn included |
| `agentctl ask-agent <agent> <task>` | Hand a task to another agent through the policy engine. The hop is billed to this turn's requester. Refused inside a private task |
| `agentctl private <task>` | Ask for a task on the owner's private resources. Returns a consent id at once. Needs the owner's consent unless the owner is this turn's requester. agentd posts the result to the thread when the task finishes. Refused inside a private task |

One bundled skill documents `agentctl`. Its token is scoped to one agent, scope,
session, turn and requester, is bound to the session's container, and expires
with the turn.

Launch flags:

```text
claude -p --input-format stream-json --output-format stream-json --verbose \
  --tools "Bash,Read,Edit,Write,Glob,Grep" --strict-mcp-config \
  --setting-sources user --permission-mode bypassPermissions \
  --append-system-prompt-file /agent/persona.md \
  --session-id <uuid> | --resume <uuid>
```

## Rendering and delivery

The agent writes standard Markdown. Each surface renders it with a converter
built on a Markdown parse tree (`pulldown-cmark`), not regexes:

- Slack: mrkdwn. Tables become aligned text in a code block, `**bold**` becomes
  `*bold*`, links become `<url|label>`, and `@Name` becomes a real mention
  through the member directory. Mass mentions (`<!here>`, `<!channel>`) are
  neutralized. qm-core's `toSlackMrkdwn` is the behavioral reference[^qm-mrkdwn].
- Rocket.Chat: mostly pass-through Markdown. `@all` and `@here` are
  neutralized.
- Splitting respects each surface's message limit, never cuts inside a link,
  mention or surrogate pair, and closes and reopens code fences across chunks.
- Files staged with `agentctl attach` upload before the text reply.
- Text directives such as `[[react: eyes]]` are parsed and stripped before
  rendering. Short message ids shown to the model come from a per-session table
  in the store, not from encoding platform timestamps.

## Commands

| Command | Who | What it does |
| --- | --- | --- |
| `/agent login`, `/agent login <code>`, `/agent logout` | Anyone | Link or unlink a Claude account |
| `/agent me` | Anyone | Link status, usage meter, own agents, manager app name |
| `/agent slack-token <token> <refresh token>` | Linked member on Slack | Register the app configuration token used to create agent apps |
| `/agent create <name> [persona]` | Linked member | Create the identity and a default persona |
| `/agent persona <name> <text>` | Owner | Edit the system prompt, or upload `persona.md` in the DM |
| `/agent skill add <name> <source>`, `/agent skill rm <name> <skill>` | Owner | Manage skills |
| `/agent allow\|deny <name> <target>` | Owner | Who may mention the agent and where |
| `/agent limits <name> turns=N/day hops=N` | Owner | Per-agent limits |
| `/agent pause\|resume\|delete <name>` | Owner | Lifecycle. Delete deactivates the bot identity |
| `/agent sessions <name>`, `/agent reset <name> [here]` | Owner | Inspect or reset sessions |
| `/agent list [@user]` | Anyone | Agent directory |
| `/agent admin ...` | Community admin | Community API key, bans, Slack configuration |

Command replies are always private: Slack ephemeral responses through
`response_url`, and the manager bot's DM on Rocket.Chat. Slack slash commands
arrive over HTTPS like every other Slack request and are acknowledged within
three seconds, with the real reply sent later through `response_url`.

## Data model

```mermaid
erDiagram
    MEMBER ||--o{ SURFACE_IDENTITY : has
    MEMBER ||--o| CLAUDE_LINK : owns
    MEMBER ||--o{ AGENT : owns
    AGENT ||--o{ AGENT_BINDING : "exposed as"
    AGENT ||--o{ SESSION : runs
    SESSION ||--o{ MESSAGE_REF : shows
    AGENT ||--o{ VOLUME : uses
    MEMBER ||--o{ USAGE : accrues
    MEMBER ||--o{ SLACK_CONFIG_TOKEN : registers
    PENDING_LOGIN }o--|| MEMBER : for
    AGENT ||--o{ CONSENT : requests

    MEMBER {
        uuid id
        text display_name
    }
    SURFACE_IDENTITY {
        text surface
        text team_id
        text user_id
    }
    CLAUDE_LINK {
        bytes access_token_enc
        bytes refresh_token_enc
        timestamp expires_at
        text plan
    }
    AGENT {
        uuid id
        text name
        text persona
        text visibility
    }
    AGENT_BINDING {
        text surface
        text team_id
        text app_id
        text client_id
        bytes client_secret_enc
        bytes signing_secret_enc
        text bot_user_id
        bytes bot_token_enc
    }
    SLACK_CONFIG_TOKEN {
        text team_id
        bytes token_enc
        bytes refresh_token_enc
        timestamp expires_at
    }
    SESSION {
        uuid id
        text scope_key
        text thread_key
        timestamp last_turn_at
    }
    MESSAGE_REF {
        int short_id
        text platform_ref
        uuid turn_id
        uuid requester_id
        int hop
    }
    CONSENT {
        uuid id
        uuid requester_id
        int hop
        text task_text
        text state
        text reply_surface
        text reply_conversation
        text reply_thread_root
        uuid origin_session_id
        uuid private_session_id
        timestamp expires_at
    }
    VOLUME {
        text scope_key
        text path
    }
    USAGE {
        date day
        int turns
    }
    PENDING_LOGIN {
        text state
        bytes verifier_enc
        timestamp expires_at
    }
```

Identities are `(surface, team_id, user_id)`, never a bare user id. One member
can link several surface identities to one Claude login. `MESSAGE_REF` records
the turn and requester of every message agentd posts, which is what
agent-to-agent attribution reads. The Slack columns of `AGENT_BINDING` are empty
for Rocket.Chat bindings.

## Security

| Threat | Mitigation |
| --- | --- |
| Prompt injection from other members reaches the owner's secrets | Channel-scope sandboxes hold no owner secrets. Work on owner resources runs in the owner's private sandbox, and for non-owners only after a consent card. Persona prompt treats others' text as data. |
| Leaked placeholder token | One per CLI process and container, bound to the container's network identity, revoked when the container is reaped, swapped only for the configured upstream header of its own kind. |
| One session reads another session's placeholder or `agentctl` token | One container per session, so sessions share neither a PID namespace nor process environments. Tokens are bound to their container. |
| One member's request billed to another in a shared scope | Placeholders are per session container, and each mapping follows the current turn's requester. |
| Agent-to-agent hops billed to the wrong person | A hop inherits the requester of the turn that posted the mention. Mentions from unmanaged bots are ignored. |
| Private task leaks the owner's DM context to a non-owner | Each private task runs in a fresh session. Only the consented task text and explicit attachments cross in, only the reply and attachments cross out. Private tasks cannot call `ask-agent` or `private`. |
| A pending consent holds resources | `agentctl private` returns at once. The channel turn ends, and the result is posted later as a new message. Unanswered cards expire. |
| Private files left behind for later channel turns | Private resources only run in the owner's private sandbox. Channel sandboxes never mount them. |
| Concurrent threads corrupt a shared checkout | One working directory per session, a lock for the scope's shared paths. |
| Model exfiltrates the real token | The real token never enters the sandbox. |
| Agents loop on each other | Hop cap per thread, token budget per thread, ignore unmentioned bot messages. |
| PKCE code interception | Separate random state, verifier server-side, 10-minute expiry, private channels only. |
| Manager account compromise on Rocket.Chat | Custom role instead of admin. The manager token never enters sandboxes. |
| agentd holds members' Slack configuration refresh tokens | Encrypted at rest, used only to create and update that member's agent apps, deleted on `/agent logout` or when the member leaves. Compromise of agentd lets an attacker create or edit apps as those members, so agentd's store and key need the same protection as the Claude tokens. |
| A later-installed Slack app takes over `/agent` | Only the manager bot declares it. `/agent me` shows the manager app's name. |
| Forged or replayed Slack requests | Each app's requests are verified with its own `signing_secret`. Retried events are deduplicated by `event_id`. |
| One member's usage billed to another | Requester-pays policy. Owner credential only with owner action or approval. |

## Crate layout

| Crate | Purpose | Main dependencies |
| --- | --- | --- |
| `agentd` | Binary and wiring | `tokio`, `tracing` |
| `core-types` | IDs, events, keys, policy types | `serde`, `uuid` |
| `store` | Persistence | `sqlx` |
| `auth` | PKCE, exchange, refresh, encryption | `reqwest` (rustls), `sha2`, `base64`, `rand`, `chacha20poly1305` |
| `surface-slack` | Events API, interactivity, slash commands, Web API, manifest API | `reqwest`, `axum`, `hmac` |
| `surface-rocketchat` | DDP realtime, REST | `tokio-tungstenite`, `reqwest` |
| `render` | Markdown conversion and splitting | `pulldown-cmark` |
| `commands` | `/agent` parsing and handlers | `clap` |
| `runner` | Session queue, stream-json, resume | `tokio` |
| `sandbox` | Container per session: provision, exec, teardown | `bollard` |
| `cred-proxy` | Header swap | `hyper`, `axum` |
| `agentctl` | In-sandbox CLI, static musl build | `clap`, `reqwest` |

The core surface trait:

```rust
#[async_trait::async_trait]
pub trait Surface: Send + Sync {
    async fn events(&self, binding: &Binding, tx: Sender<InboundEvent>) -> Result<()>;
    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<MsgRef>;
    async fn edit(&self, msg: &MsgRef, text: &str) -> Result<()>;
    async fn react(&self, msg: &MsgRef, emoji: &str) -> Result<()>;
    async fn upload(&self, to: &ReplyTarget, files: &[OutFile]) -> Result<()>;
    async fn history(&self, thread: &ThreadKey, before: Option<Cursor>, limit: usize) -> Result<Vec<Msg>>;
    fn render(&self, markdown: &str) -> Vec<String>;
    fn caps(&self) -> Caps;
}
```

`history` reads one thread (or a DM's top level), because both of its
callers, the per-turn message and `agentctl history`, need a thread's
messages, and neither platform can list a thread's replies without its root.

Everything after `InboundEvent` is shared. Behavior differences go through
`caps()` (buttons, edits, message size), never through surface-name checks in
shared code. A `MockSurface` drives the shared core in tests.

## Milestones

1. Rocket.Chat adapter, manager bot DM commands (`login`, `create`, `persona`,
   `list`), PKCE linking.
2. One bot per agent, mention gating, a Docker container per session on a
   volume per agent and scope, non-root `claude`, `--session-id` / `--resume`
   through the credential proxy.
3. Requester-pays routing and the usage meter.
4. Slack adapter: public HTTPS endpoint, `/agent slack-token`, manifest-based
   agent apps with a one-click install, manager bot with `/agent`.
5. Consent cards and `agentctl private`, agent-to-agent hand-off with requester
   attribution, hop caps. Before this milestone, verify on a real workspace that
   one app's bot user mentioning another app's bot user produces `app_mention`.
6. Owner-initiated cloud hand-off (`claude --cloud`) for long PR work.
7. Slack Connect.

## Slack Connect

Every organization in a Slack Connect channel must be on a paid plan[^slack-connect].
Bot users work there, but a custom app's slash commands only work for its own
organization. The design keeps this possible by keying identities and sessions by
team, deduplicating events that arrive once per workspace, and routing consent
cards to the owner's own workspace. The audience policy is configurable rather
than refusing external members as qm-core does by default.

## Alternatives considered

**Keep qm-core's shape (CLI on the host, MCP tool bridge).** Rejected. Every tool
needs an MCP schema in context, and every mentionable agent needs its own core
deployment because qm-core supports one Slack installation per organization.

**One community bot that posts as each agent.** Slack's `chat:write.customize`
can change the display name and icon per message. It needs one app in total and
has no app limit, but agents are not separately mentionable. Kept as a fallback
for workspaces that cannot install more apps.

**Claude Managed Agents.** Anthropic hosts the loop and a per-session container,
with vault credentials substituted at egress, skills, custom tools and memory
stores[^cma]. It fits shared automation well and would remove the sandbox,
runner and proxy. It bills by API key, not subscription. Kept as an option for
channel agents funded by a community API key.

**Claude Code cloud sessions.** `claude --cloud` creates a session and
`claude -p "msg" --cloud <id>` queues a message, but there is no documented way
to read replies, and sessions are tied to a GitHub repository[^cloud]. Used only
for owner-initiated PR work, not as the chat backend.

**Browser automation of claude.ai.** Rejected. It violates Consumer Terms
section 3, and one suspension would take down every agent.

**Calling the Messages API from Rust with subscription tokens.** Rejected.
Subscription tokens are covered for use through Claude Code and the Agent SDK.
Direct calls would also need our own agent loop.

## Open questions and risks

- Claude Code's OAuth client id, scopes and endpoints are not a public contract.
  A change breaks linking until configuration is updated.
- The exact Rocket.Chat custom role. The current reading is `create-user`,
  `edit-other-user-active-status` when passing `active`, and token creation.
  An issue from 2017 reported that `users.create` also needed edit-user
  permissions[^rc-7351]. Needs a test on the target server version.
- Whether Slack delivers `app_mention` when one app's bot user mentions
  another's. Agent-to-agent turns on Slack depend on it.
- One container per active session costs more than one per scope. Idle reaping
  bounds it, but a busy channel with many threads needs a per-scope container
  cap and a queue.
- Whether Rocket.Chat's `__my_messages__` subscription delivers every joined
  room. Until confirmed, subscribe per room.
- The Agent SDK credit is per user and monthly. Agents need clear messages when
  a requester's credit runs out.
- Long-term transcript retention on volumes. Snapshots cover single-host
  deployments. Multi-host needs transcript mirroring.
- Terms interpretation for requester-pays in shared channels is a design
  judgment, not legal advice. Larger communities should confirm with Anthropic.

## References

[^qm]: qm-core, the reference TypeScript implementation (`agentsky/qm-core`). Slack surface in `src/slack/`, installation store in `src/surfaces/slack-installation.ts`.
[^qm-harness]: qm-core `src/harness/claude-harness.ts`: `tools: ["Agent"]`, `settingSources: []`, bridged tools through `createSdkMcpServer`.
[^qm-oauth]: qm-core `src/model/subscription-oauth.ts`, `startClaudeLogin`.
[^qm-mrkdwn]: qm-core `src/slack/mrkdwn.ts` and `src/slack/safe-cut.ts`.
[^qm-askagent]: qm-core `src/slack/agent-requests.ts`: an approved request runs as a DM-scoped turn for the target member and the result is posted back to the thread.
[^terms]: [Anthropic Consumer Terms](https://www.anthropic.com/legal/consumer-terms), sections 2 and 3.
[^sdk-credit]: [Use the Claude Agent SDK with your Claude plan](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan).
[^cloud]: [Use Claude Code in the cloud](https://code.claude.com/docs/en/claude-code-on-the-web.md).
[^cma]: Claude Managed Agents documentation, [quickstart](https://platform.claude.com/docs/en/managed-agents/quickstart).
[^slack-mention]: [app_mention event](https://docs.slack.dev/reference/events/app_mention/).
[^slack-botmention]: [slackapi/java-slack-sdk#1279](https://github.com/slackapi/java-slack-sdk/issues/1279) shows `app_mention` with the `bot_message` subtype for a message posted by a workflow, not for one app's bot user mentioning another's. To be verified on a real workspace.
[^cc-bypass]: [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes#skip-all-checks-with-bypasspermissions-mode): bypass mode is refused as root or under sudo on Linux and macOS outside a recognized sandbox.
[^cc-envvars]: [Claude Code environment variables](https://code.claude.com/docs/en/env-vars): `BASH_DEFAULT_TIMEOUT_MS` and `BASH_MAX_TIMEOUT_MS`.
[^cc-sessions]: [Claude Code sessions](https://code.claude.com/docs/en/sessions): `--resume <id>` searches every project since 2.1.223, and `CLAUDE_CODE_PROJECT_DIR_NAME` names the transcript directory since 2.1.234.
[^slack-approval]: [Manage app approval for your workspace](https://slack.com/help/articles/222386767-Manage-app-approval-for-your-workspace).
[^slack-free]: [Feature limitations on the free version of Slack](https://slack.com/help/articles/27204752526611-Feature-limitations-on-the-free-version-of-Slack).
[^slack-manifest]: [Configuring apps with app manifests](https://docs.slack.dev/app-manifests/configuring-apps-with-app-manifests/).
[^slack-socket]: [Using Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/). App-level tokens are generated in the app settings UI.
[^slack-connect]: [Slack Connect guide](https://slack.com/help/articles/115004151203-Slack-Connect-guide--Work-with-external-organizations).
[^rc-stream]: [stream-room-messages](https://developer.rocket.chat/api/realtime-api/subscriptions/stream-room-messages).
[^rc-create]: [Rocket.Chat Create User](https://developer.rocket.chat/reference/api/rest-api/endpoints/user-management/users-endpoints/create-user).
[^rc-perms]: [Rocket.Chat permissions](https://docs.rocket.chat/docs/permissions): `api-bypass-rate-limit` defaults to the admin, bot and app roles.
[^rc-slash]: [Rocket.Chat slash commands](https://docs.rocket.chat/docs/slash-command) are registered by Apps-Engine apps.
[^rc-7351]: [RocketChat/Rocket.Chat#7351](https://github.com/RocketChat/Rocket.Chat/issues/7351).
