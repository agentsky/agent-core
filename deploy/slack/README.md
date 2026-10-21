# Slack live checks

The plan's Slack tasks were built and tested against `wiremock` and
hand-written payloads, because Slack's documentation and API were out of
reach while they were written. Request and response shapes come from
Slack's SDKs and Bolt's test fixtures. The plan keeps live checks manual
([Testing](../../docs/tasks-plan.md#testing)): each task names one, and its
pull request records what was run and what was seen. When this runbook was
written, no Slack task had run against a real workspace.

This runbook gathers those checks in one place and puts them in an order
that builds on itself:

- the setup they share;
- how to record and redact a result;
- every task's own live check;
- the open items the tasks' pull requests listed;
- the features no task's live check exercises on Slack.

[`deploy/compose/README.md`](../compose/README.md) is the same for
Rocket.Chat, and its stack is the one these checks run on.

## What you need

| What | For |
| --- | --- |
| A Slack workspace for development, on a plan whose limits you know: the free plan allows 10 app installs, the manager app included | every check |
| A public HTTPS hostname that stays the same across restarts, with a TLS terminator or tunnel to the host's `127.0.0.1:8443`. Slack bakes the URL into every agent's app when it is created, so a hostname that changes leaves those apps behind | every check |
| A second hostname, or a second path on the terminator, for the capture endpoint below | T32, T36d, T36e, and the interactivity check |
| A Docker host for the Compose stack, with `isolate-sandbox.sh`'s rules | every check that runs a turn |
| A Claude Pro or Max account to link | linking, and every turn on a member's account |
| A community API key for the Claude API | turns of a member with no linked account |
| Three people or test accounts in the workspace: the owner of the agents, who is also a community admin; a second member who never links an account; and, for the requester checks, a third member with an account of their own to link | C3, C4, D1 |
| A second paid workspace, of another organization, joined to the first by a Slack Connect channel, and an Enterprise Grid workspace if one is at hand | section F |

The checks run at the top of the plan's stack, the newest task branch,
until it merges. Some tasks' migrations were edited in place during
review, so a database left from an earlier run fails sqlx's checksum check
at startup: start from an empty `data/`, or reset it.

## 1. Bring agentd up for Slack

1. Bring the Compose stack up as in
   [`deploy/compose/README.md`](../compose/README.md) step 1, and configure
   `[sandbox]` as in its step 3. The Rocket.Chat parts can stay as they are:
   agentd serves Slack alongside, or alone with no `[rocketchat]` section.
2. Point the public hostname at `127.0.0.1:8443`, the public listener as
   `compose.yaml` publishes it, and check it from outside:
   `curl -fsS https://<public host>/healthz` prints `ok`.
3. `compose.yaml` passes agentd only `AGENTD_MASTER_KEY` and
   `AGENTD_RC_MANAGER_TOKEN`, so give it the Slack manager's secrets with a
   `compose.override.yaml` next to it, which Compose reads on its own; keep
   it out of Git, like `.env`:

   ```yaml
   services:
     agentd:
       environment:
         - AGENTD_SLACK_MANAGER_SIGNING_SECRET
         - AGENTD_SLACK_MANAGER_BOT_TOKEN
   ```

4. Add to `agentd.toml`:

   ```toml
   [slack]
   public_url = "https://<public host>"
   # So the install reminder (B2) comes within a minute or two.
   install_reminder_secs = 60

   [community]
   # The owner's identity: "slack:<team id>:<user id>". The user id is under
   # the profile's "Copy member ID"; the team id is the one auth.test gives
   # for the manager's bot token, once step 5 made it.
   admins = ["slack:T…:U…"]
   ```

   and, under `[server]`, change `log_filter` so the debug lines some
   checks read are written, keeping the rest at `info`:

   ```toml
   log_filter = "info,agentd::pipeline=debug"
   ```

5. Create the manager app from
   [`manager-manifest.yaml`](manager-manifest.yaml) as the main README's
   [Slack](../../README.md#slack) section says, put its signing secret and
   bot token in `.env` as `AGENTD_SLACK_MANAGER_SIGNING_SECRET` and
   `AGENTD_SLACK_MANAGER_BOT_TOKEN`, and `docker compose up -d`.
   `docker compose logs agentd` shows
   `Slack manager app: requests to /slack/b/manager/ are verified` once it
   reads them, and agentd doesn't start if Slack refuses the token. The
   team id for `[community] admins` is in

   ```bash
   curl -fsS https://slack.com/api/auth.test \
     -H "Authorization: Bearer $AGENTD_SLACK_MANAGER_BOT_TOKEN"
   ```

   after which `docker compose restart agentd` reads the admin list.

`docker compose logs -f agentd` is where most of what the checks record
shows up. agentd logs no message text, token or payload, only ids, kinds
and counts; a raw payload has to come from the capture endpoint.

## 2. Recording a result

Record each check in its task's pull request, or in a follow-up pull
request once that one has merged, and update the design or the plan where
the check's own text says to. For each check:

```markdown
### Live check: <check id>, <task>

- Date, agentd commit, Slack plan, Claude Code version in the sandbox image.
- What was run, step by step.
- What was seen: replies (as text), log lines, redacted payloads.
- Result: as the design says / differs (what, and the fix or the issue).
```

Redact everything that leaves the machine:

- Never paste a token or secret: `xoxb-`, `xoxe-`, `xoxp-`, refresh tokens,
  signing secrets, `sk-ant-` keys, OAuth codes, `response_url`s and
  `trigger_id`s, the `token` field of a payload.
- Replace every id (`T…`, `E…`, `U…`, `W…`, `B…`, `A…`, `C…`, `G…`, `D…`,
  `F…`, timestamps used as message ids) with a made-up one of the same
  shape, the same made-up one wherever the real one repeats, so a reader
  can still tell which ids are equal.
- Drop names, emails, avatars and other profile fields, and replace message
  text with a placeholder that keeps its `<@U…>` mentions, redacted.

### Capturing raw payloads

Some checks need what Slack sends, which agentd doesn't keep: a scratch
app, which is not one of agentd's, sends its requests to a small endpoint
that writes each one to a file. T36e makes this its first deliverable; until
then, this one does:

```python
#!/usr/bin/env python3
"""Writes every request a scratch Slack app sends to a file, unverified.

Usage: python3 capture.py <out dir> [delay seconds before answering]
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = sys.argv[1]
DELAY = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0


class Capture(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        record = {
            "at": time.time(),
            "path": self.path,
            "headers": {name.lower(): value for name, value in self.headers.items()},
            "body": body.decode("utf-8", "replace"),
        }
        with open(os.path.join(OUT, f"{time.time_ns()}.json"), "w") as file:
            json.dump(record, file, indent=1)
        reply = b""
        try:
            message = json.loads(body)
            if message.get("type") == "url_verification":
                reply = message["challenge"].encode()
        except (ValueError, AttributeError):
            pass
        if not reply:
            time.sleep(DELAY)
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


ThreadingHTTPServer(("127.0.0.1", 8444), Capture).serve_forever()
```

Run it with an empty directory, and point the second hostname at
`127.0.0.1:8444`. It checks no signature, so anyone who finds the URL can
write files: run it only while capturing, and delete the scratch apps
afterwards. Create each scratch app at <https://api.slack.com/apps> from a
manifest like an agent's, with its request URLs on the capture hostname:

```yaml
display_information:
  name: capture-a
features:
  bot_user:
    display_name: capture-a
    always_online: true
oauth_config:
  scopes:
    bot: [chat:write, channels:history, groups:history, im:history, mpim:history,
          channels:read, groups:read, im:read, mpim:read, users:read]
settings:
  event_subscriptions:
    request_url: https://<capture host>/events
    bot_events: [message.channels, message.groups, message.im, message.mpim,
                 channel_id_changed]
  interactivity:
    is_enabled: true
    request_url: https://<capture host>/interactivity
  org_deploy_enabled: false
  socket_mode_enabled: false
  token_rotation_enabled: false
```

Install it, note its bot token, and invite its bot where the check says.
Web API reads for a check are `curl` calls on that bot token, for example:

```bash
curl -fsS https://slack.com/api/conversations.replies \
  -H "Authorization: Bearer $SCRATCH_BOT_TOKEN" \
  --data-urlencode channel=C… --data-urlencode ts=1700000000.000100
```

## 3. The checks

They are grouped so each builds on the ones before it. A check that names
a task is that task's live check, or an item its pull request left open;
the ones marked *(not in the plan)* exercise a feature on Slack that no
task's live check covers.

### A. The manager app and accounts

- **A1, T30: the manager app.** Record whether Slack accepted the manifest
  as written, and whether "Event Subscriptions" showed the request URL
  verified on its own or needed "Retry" once agentd had its secrets (README
  step 5). T30's pull request left open whether Slack creates the app while
  the URL can't be verified yet.
- **A2, T13 and T30: linking.** `/agent login`, open the link, sign in with
  the Pro or Max account, and send `/agent login <code#state>` back.
  Confirm the authorization server accepts `user:profile user:inference`,
  `/agent me` shows the plan and names the app answering, a turn later runs
  on the account, and a refresh succeeds. Then `/agent logout`, and confirm
  the revocation at `revoke_url` succeeds and a refresh with the revoked
  token is refused. Link again for the checks that follow. If the T13 check
  already ran on Rocket.Chat, the Slack part is the reply forms and `me`.
- **A3, T30: the configuration token.** Generate an app configuration
  token under "Your App Configuration Tokens" and send
  `/agent slack-token <token> <refresh token>`: agentd logs
  `registered a Slack configuration token`. To see a rotation without
  waiting ten hours, stop agentd, move the token's expiry to an hour from
  now, and start it again; agentd renews a token with less than two hours
  left and looks every minute:

  ```bash
  docker compose stop agentd
  # A test-only edit of agentd's database, which is uid 10001's.
  sudo sqlite3 data/agentd.db \
    "UPDATE slack_config_tokens SET expires_at = strftime('%s','now') + 3600"
  docker compose start agentd
  sleep 90
  sudo sqlite3 data/agentd.db "SELECT expires_at - strftime('%s','now') FROM slack_config_tokens"
  ```

  The second query shows close to twelve hours left once it renewed; record
  that, and that B1's `/agent create` works with the renewed token.
- **A4, T30: a DM as a command.** Send `me` and `help` as direct messages
  to the manager app, and record that they answer like `/agent me` and
  `/agent help`, and that every slash command's reply is visible only to
  the member who sent it.
- **A5, T28: an overloaded slash command** *(opportunistic)*. agentd
  answers 503 when a binding has too many requests in flight or a binding's
  lookup takes too long. If one happens during the checks, record what
  Slack showed the member; T28 left open how Slack treats a 503 on a slash
  command.

### B. Agents as apps

- **B1, T31: creating an agent.** `/agent create helper` makes the app with
  the configuration token (`created an agent's Slack app` in the log).
  Record whether the app's events URL verified during
  `apps.manifest.create`, as T28 and T31 assumed, which the app's "Event
  Subscriptions" page shows. Then make an app named `twin` by hand at
  <https://api.slack.com/apps>, send `/agent create twin`, and record the
  error code Slack gives for the name conflict and the reply.
- **B2, T31: installing.** Open the install link the manager app DMs, choose
  "Allow", and record the callback page and the manager's message naming the
  bot to invite. For the reminder, create a third agent and don't install
  it: with `install_reminder_secs = 60` the manager reminds the member once
  (`sent an install reminder`). In a workspace that requires app approval,
  record what the click does instead.
- **B3, T31 and T29: the first turns.** Invite `helper`'s bot to a public
  channel and mention it. Record:
  - that it reacts with `[runner] working_emoji` while the turn runs, and
    answers in the mention's thread, as its own bot;
  - that a reply without a mention in that thread, and a DM to the bot,
    are answered too, and a channel message that doesn't mention it isn't;
  - T29's check: ask for a reply with the Markdown link
    `[a | b](https://x.io)`, which `render` sends as `<https://x.io|a | b>`,
    and confirm Slack shows the whole label `a | b` linking to
    `https://x.io`;
  - a turn that writes a file and attaches it with `agentctl attach`, for
    T29's upload flow (`files.getUploadURLExternal`, the raw upload to the
    URL it gives, `files.completeUploadExternal`).
- **B4, T31: two agents.** Create and install a second agent, invite both
  to the channel, mention each, and confirm each answers as its own bot.
  That completes design milestone 4.
- **B5, T31: threads in private channels.** Repeat a threaded exchange in a
  private channel the bot is in, three or more messages deep, and confirm
  the agent sees the thread: T29 reads it with `conversations.replies` on
  the agent's bot token, and a refusal would show as `Forbidden` in the log,
  not as an empty thread.
- **B6, T23 on Slack: restart and resume.** Mid-thread,
  `docker compose restart agentd`, then continue the thread: the next turn
  resumes the session. Record the cost `/agent me` shows before and after.
- **B7, T31 and T14: pause, resume, delete.** `/agent pause helper`: a
  mention is answered that `helper` is paused by its owner.
  `/agent resume helper`. Delete B2's never-installed agent's app by hand at
  <https://api.slack.com/apps>, then `/agent delete` that agent, and record
  Slack's error code and the reply, which T31 left open. Create and install
  a throwaway agent, `/agent delete` it, and confirm its app is gone.
- **B8: a persona from a file** *(not in the plan)*. Attach a `persona.md`
  to a DM `persona helper` to the manager app, and confirm the next turn
  follows it; this reads the file with the manager's `files:read`.

### C. Sessions, skills, paying and limits *(not in the plan)*

Nothing in the plan checks these on Slack; T37 checks some of them on
Rocket.Chat, in CI.

- **C1, T24: sessions.** `/agent sessions helper` lists the threads B3 to
  B6 used. In one of those threads' channel, `/agent reset helper here`
  resets only that conversation; `/agent reset helper` resets them all.
  `/agent reset helper here` sent in the manager's DM says no agent answers
  there and names the slash form to send instead.
- **C2, T25: skills.**
  - `/agent skill add helper <https Git URL of a repository with a SKILL.md>`
    adds it, and a turn uses it.
  - A skill whose `SKILL.md` lists `allowed-hosts` waits until
    `/agent skill confirm helper <skill>`. A turn reaches those hosts only
    after it, and only from `helper`'s sandboxes.
  - Attach a `.zip` to a DM `skill add helper` to the manager app.
  - `/agent skill rm helper <skill>` removes it.
- **C3, T26 and T21: who pays.**
  - With no community key, the member who never linked mentions `helper`:
    the manager DMs them how to link, and no turn runs.
  - The admin sends `/agent admin api-key set <key>`; the same mention now
    runs on the community key, and `/agent me` for the admin says a key is
    set.
  - In one thread, the owner, then the unlinked member, then the third
    member after linking their own account, each mention `helper`. Each
    turn runs on its requester's account or the key; `/agent me` shows each
    member's own turns.
  - `docker ps --filter label=agentd.instance` shows a new container
    whenever the requester changes, and the same one for a requester's next
    turn. That is T21's recycle, the fix for processes a turn leaves
    running.
- **C4, T27: limits, allow, deny, bans.**
  - `/agent limits helper turns=1/day`: the unlinked member's second
    request that day is refused, and the agent says so once in the thread.
  - `/agent deny helper @<member>`: their mention is refused, and the
    manager tells them privately, at most once a day.
  - `/agent allow helper #<channel>`, then `/agent allow helper everyone`.
    Each reply names the commands in their `/agent …` form.
  - `/agent admin ban @<member> testing`: they may run only `me`, `logout`,
    `cloud rm`, and `pause` or `delete` of their own agents, and
    `/agent me` says so. Then `/agent admin unban @<member>`.

### D. Private tasks and agents asking agents

- **D1, T33: private tasks.** Have the unlinked member ask `helper`, in a
  channel, for something only its owner's side holds, so the turn runs
  `agentctl private` with `--file`. Record:
  - the consent card the manager DMs the owner, as Block Kit: the exact
    task, the requester, the files, Approve and Decline;
  - that Approve runs it on the owner's account and posts the result in
    the thread, and the card then shows the outcome without buttons
    (`chat.update`);
  - a second request declined, and a third left to expire, with
    `[limits] consent_ttl_secs` lowered for the run;
  - T33's own check: the approved task can write a new file in `work/`
    beside the handed file, which relies on the sandbox's directory repair.
- **D2, T31: whether Slack retries an interaction.** T28 deduplicates an
  interaction by its signature, so a retry signed with a new timestamp
  would run twice
  ([impl-notes](../../docs/impl-notes.md#replays-inside-the-five-minute-window)).
  Run the capture endpoint with a delay, `python3 capture.py out 5`, have the
  scratch app post a button, and press it:

  ```bash
  curl -fsS https://slack.com/api/chat.postMessage \
    -H "Authorization: Bearer $SCRATCH_BOT_TOKEN" \
    -H 'Content-Type: application/json; charset=utf-8' \
    -d '{"channel":"C…","text":"retry probe","blocks":[{"type":"actions",
         "elements":[{"type":"button","text":{"type":"plain_text","text":"Press"},
         "action_id":"probe"}]}]}'
  ```

  Record how many requests reached the endpoint for one press, and their
  `x-slack-retry-num` and `x-slack-request-timestamp` headers.
- **D3, T34: hand-offs.** With `helper` and a second agent in one thread,
  ask `helper` to ask the other (`agentctl ask-agent`, or a mention in its
  reply). Record that the other agent answers once, on the same requester's
  account, and that a chain stops at the hop cap, with
  `/agent limits helper hops=1` to reach it quickly; the cap's notice is
  said once an hour per agent and thread.
- **D4, T32: whether Slack delivers one app's bot post to another.** During
  D3, agentd logs at debug level
  `the hop from this post's turn ran already; dropped this copy` when
  Slack's copy of an agent's post reaches the agent it mentions after
  agentd's own delivery ran the hop, and
  `a hop's other copy was acted on already; dropped this one` when Slack's
  copy came first. Neither line, for a hop that ran, means Slack didn't
  deliver the post to that agent's app. For the payload, invite a scratch
  app's bot to the same channel, have `helper` post a message that mentions
  the scratch app's bot, and record from the capture whether the scratch
  app received a `message.channels` event, with which `subtype`, `bot_id`
  and `user`, and whether it carries `blocks` Slack made of the text-only
  post. Do it at top level and in a thread. T32's pull request carries the
  redacted payloads and updates the design's table row, open question and
  footnote.

### E. Cloud hand-off on Slack

- **E1, T35c.** T35c's live check, on the routine endpoint, applies as
  written. On Slack, also record that `/agent cloud add <routine> <url>
  <token>` as a slash command leaves no message in any conversation, that
  `/agent cloud run` returns the session's link privately, that `/agent
  cloud list` and `/agent cloud rm` work in the slash command and in the
  manager's DM, and how a task with a mention, a channel and a link reaches
  the session.

### F. Slack Connect

These need the second workspace. Admitting anyone from another
organization waits for T36b, which waits for F3.

- **F1, T36d: channel ids that change.** Share a private channel holding an
  agent, and a scratch app's bot, with the other organization. Record the
  `channel_id_changed` payload from the capture, that `/agent me` listed any
  app still waiting for the event and an `apps.manifest.update` added it
  without a new install, and that a `deny #room` set before the share still
  applies after it.
- **F2, T36a: outside members are kept out.** Before T36b, record that an
  outside member's mention of an agent gets no turn, what their `/agent`
  and a DM to the manager app get (T36a drops a DM command from another
  workspace), and what agentd logged.
- **F3, T36e: the payloads the identity rules rest on.** Run T36e's capture
  as the plan lists it, items 1 to 9 and the Enterprise Grid part, with the
  capture endpoint and scratch apps above. Its redacted fixtures go under
  `crates/testkit/fixtures/slack/connect/`, and its pull request updates the
  design's Slack Connect "Verified and assumed".
- **F4, T36b and T36c**, once they exist: the live check under T36c, with
  `outside` allowed on one agent, completes design milestone 7.

## 4. Clean up

- `/agent delete` every agent left, and delete the scratch apps and any
  app that survived at <https://api.slack.com/apps>.
- `/agent admin api-key clear`, lift any ban, `/agent logout` each linked
  member, and revoke the configuration tokens.
- Stop the capture endpoint and remove its hostname.
