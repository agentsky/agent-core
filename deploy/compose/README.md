# Development stack

[`compose.yaml`](compose.yaml) runs Rocket.Chat 7.x with MongoDB, and
agentd, on the two networks from the plan's
[Network and deployment shape](../../docs/tasks-plan.md#network-and-deployment-shape):

| Network | Kind | Members |
| --- | --- | --- |
| `egress` (`172.31.0.0/24`) | normal bridge | Rocket.Chat, MongoDB, agentd at `172.31.0.2` |
| `sandbox` (`172.30.0.0/24`, bridge `br-agent-sbx`) | internal, the host has no address on it, and its containers can't reach each other | agentd at `172.30.0.2` (`cred-proxy.internal`, `agentctl.internal`), sandboxes |

agentd's public listener is `172.31.0.2:8443`, the credential proxy
`172.30.0.2:8080` and the agentctl API `172.30.0.2:8081`, as in
[`config/agentd.example.toml`](../../config/agentd.example.toml). A sandbox
reaches those two ports and nothing else: not the host, not the internet,
and not another sandbox. `scripts/ci/compose-test.sh` checks that in CI.
Docker knows the networks as `egress` and `sandbox`
(`AGENT_CORE_EGRESS_NETWORK` and `AGENT_CORE_SANDBOX_NETWORK` change that).

Keeping sandboxes apart takes two parts. `compose.yaml` turns
inter-container traffic off on `sandbox`, so Docker drops everything
between its containers, and that includes a sandbox's connections to
agentd. [`isolate-sandbox.sh`](isolate-sandbox.sh) then adds iptables rules
to Docker's `DOCKER-USER` chain that let new connections to
`172.30.0.2:8080` and `:8081` through, and drop everything else on the
bridge. Without the rules sessions can't reach the credential proxy, so a
missing rule fails loudly rather than letting sandboxes talk to each other.
The rules need root, and don't survive a reboot of the host.

On the host, Rocket.Chat is at <http://localhost:3000> and agentd's public
listener at `127.0.0.1:8443`. Both bind the loopback address only.

The commands below run in this directory. They need Docker with the Compose
v2 plugin, and on Linux a user allowed to use the Docker socket.

## 1. Bring the stack up

Settings and secrets live in `.env` here, which Compose reads and Git
ignores. [`compose.yaml`](compose.yaml)'s header lists every variable.

Each line below adds its key to `.env` only when the key isn't there yet,
so running the block again keeps the master key and the admin password.

```bash
touch .env

# The group owning the Docker socket, as containers see it. agentd, which
# runs as uid 10001, joins it to start sandboxes.
grep -q '^DOCKER_GID=' .env ||
  echo "DOCKER_GID=$(docker run --rm -v /var/run/docker.sock:/sock busybox:1.37 stat -c %g /sock)" >> .env

# Build agentd's image and the sandbox image agentd starts sessions from.
docker compose --profile sandbox build

# The master key that encrypts agentd's stored secrets. Keep a copy: losing
# it makes every stored secret unreadable.
grep -q '^AGENTD_MASTER_KEY=' .env ||
  echo "AGENTD_MASTER_KEY=$(docker compose run --rm --no-deps agentd gen-key)" >> .env

# Rocket.Chat creates this admin on its first start.
grep -q '^RC_ADMIN_PASS=' .env ||
  echo "RC_ADMIN_PASS=$(openssl rand -hex 16)" >> .env

# agentd's configuration: the example, whose addresses match compose.yaml,
# without its [rocketchat] section, which names a placeholder server.
[ -e agentd.toml ] ||
  awk '/^\[/ { skip = ($0 == "[rocketchat]") } !skip' ../../config/agentd.example.toml > agentd.toml
grep -q '^AGENT_CORE_CONFIG=' .env || echo "AGENT_CORE_CONFIG=./agentd.toml" >> .env

# Let sandboxes reach agentd's 8080 and 8081, and nothing else on their
# network. Again after every reboot of the host.
sudo sh isolate-sandbox.sh

docker compose up -d
docker compose ps
curl -fsS http://127.0.0.1:8443/healthz   # "ok" once agentd serves
```

Rocket.Chat takes a minute or two to start the first time. `docker compose
logs -f rocketchat` shows when it is up.

The Community Edition must be able to reach Rocket.Chat Cloud over HTTPS to
report its statistics. A workspace that never has, or hasn't for ten days,
is restricted: posts and edits, through the REST API and DDP, and the
confirm step of uploads answer `restricted-workspace` until it reports or
gets a license
([impl-notes](../../docs/impl-notes.md#the-live-check-against-7139)). Reads,
reactions, invites and user management still work, so a restricted
workspace looks healthy until an agent tries to answer.

agentd keeps its database and, later, the agents' volumes under `./data`
(`AGENT_CORE_DATA`), owned by uid 10001 with mode 0700, so reading it on
the host needs `sudo`. `docker compose down` stops the stack and keeps
everything; `docker compose down -v` also deletes Rocket.Chat's database.
`sudo sh isolate-sandbox.sh remove` takes the iptables rules out again.

The Docker socket is mounted into agentd so it can start sandboxes. That is
a development-only shortcut: whoever controls agentd then controls the
Docker daemon, which is root on the host. In production, put a socket proxy
in front of the daemon that allows only the container, exec and event calls
agentd makes, and give agentd that instead.

## 2. Rocket.Chat admin, manager user and role

1. Log in at <http://localhost:3000> as `admin` (`RC_ADMIN_USERNAME`) with
   the `RC_ADMIN_PASS` from `.env`. Compose marks the setup wizard as
   completed. If the stack first started without `RC_ADMIN_PASS`, Rocket.Chat
   created no admin, and the first user to register becomes one.
2. Let bots create their own tokens: agentd logs in as each new bot once and
   creates a personal access token for it. In **Administration >
   Workspace > Permissions**, find `create-personal-access-tokens` and tick
   the `bot` role.
3. Give the manager its permissions. The design asks for a role with only
   what agentd needs. T11's live check against 7.13.9
   ([impl-notes](../../docs/impl-notes.md#the-live-check-against-7139))
   found that is:

   | Permission | For |
   | --- | --- |
   | `create-user` | `/agent create` |
   | `view-full-other-user-info` | telling bots from people ([impl-notes](../../docs/impl-notes.md#messages-dont-carry-the-senders-roles)) |
   | `edit-other-user-active-status` | `/agent delete` |
   | `add-user-to-joined-room` | `!agent create` in a room, which adds the bot there |
   | `api-bypass-rate-limit` | the manager's own REST calls |
   | `create-personal-access-tokens` | the manager's own token |

   A new bot sets its own avatar, and agentd renames no bot, so the manager
   needs neither `edit-other-user-avatar` nor `edit-other-user-info`
   ([impl-notes](../../docs/impl-notes.md#a-bot-sets-its-own-avatar)).

   Creating a role is an Enterprise feature: `roles.create` needs the
   `custom-roles` license module
   ([impl-notes](../../docs/impl-notes.md#custom-roles-need-a-rocketchat-enterprise-license)).
   On the Community Edition this stack runs by default, the manager holds
   the built-in `bot` and `app` roles instead. In **Administration >
   Workspace > Permissions**, tick the `app` role for `create-user`,
   `view-full-other-user-info`, `edit-other-user-active-status` and
   `add-user-to-joined-room`. `app` already has
   `api-bypass-rate-limit`, and step 2 gave `bot`
   `create-personal-access-tokens`. `app`'s only other holders are
   Apps-Engine app users, which can't log in; ticking these for `bot` would
   give them to every bot in the workspace. With a license, create a global
   role `agent-manager` in **Administration > Workspace > Permissions >
   Roles** and tick the permissions above instead.
4. Create the manager in **Administration > Workspace > Users > New user**:
   username `agent-manager`, any email, a password, and the roles `bot` and
   `app` on the Community Edition, or `user` and `agent-manager` with a
   license.
5. Log in as the manager and, in **My account > Personal access tokens**,
   create a token with **Ignore Two Factor Authentication** ticked (the
   endpoint for renaming a bot requires two-factor authentication otherwise).
   Note the token and the user id shown with it.

## 3. Configure agentd

agentd reads `agentd.toml` here, which step 1 copied from
[`config/agentd.example.toml`](../../config/agentd.example.toml) without its
`[rocketchat]` section.

Keep the `[server]` and `[internal]` addresses unless you change
`compose.yaml` and `isolate-sandbox.sh` to match. Put the manager's token
from step 2 in `.env` as `AGENTD_RC_MANAGER_TOKEN=<token>`, replacing the
line if there is one already.

Then add the example's `[rocketchat]` section, with `base_url =
"http://rocketchat:3000"` (Rocket.Chat on the `egress` network), a `team`
name, and the manager's user id as `manager_user_id`. agentd refuses to
start with the section and no `AGENTD_RC_MANAGER_TOKEN`. For sandboxes, the
`[sandbox]` section, once agentd reads it, takes:

```toml
[sandbox]
image = "agent-core/sandbox:dev"
# The default; the name compose.yaml gives the network.
network = "sandbox"
# agentd runs in a container, and Docker resolves bind mounts on the host,
# so this is ./data as the Docker daemon sees it: `realpath data`.
host_data_dir = "/absolute/path/to/deploy/compose/data"
```

Apply changes with `docker compose up -d`, which recreates agentd when
`.env` changed, or `docker compose restart agentd` after editing
`agentd.toml`. `docker compose logs -f agentd` shows configuration errors,
which name the key at fault.

## 4. Live checks

The plan's live checks run against this stack. Record in the pull request
what was run and what was seen, with tokens and ids redacted.

- **T11, Rocket.Chat REST.** Using a manager with only the role from step
  3, create a bot user and obtain its token, and record the exact
  permissions that needed. By hand, with the manager's id and token in
  `MANAGER_ID` and `TOKEN`:

  ```bash
  curl -fsS http://localhost:3000/api/v1/users.create \
    -H "X-User-Id: $MANAGER_ID" -H "X-Auth-Token: $TOKEN" -H 'Content-Type: application/json' \
    -d '{"username":"probe-bot","name":"Probe","email":"probe-bot@agent-core.invalid",
         "password":"<random>","roles":["bot"],"verified":false,
         "joinDefaultChannels":false,"requirePasswordChange":false}'
  curl -fsS http://localhost:3000/api/v1/login -H 'Content-Type: application/json' \
    -d '{"user":"probe-bot","password":"<random>"}'   # gives the bot's userId and authToken
  curl -fsS http://localhost:3000/api/v1/users.generatePersonalAccessToken \
    -H "X-User-Id: <bot userId>" -H "X-Auth-Token: <bot authToken>" \
    -H 'x-2fa-method: password' -H "x-2fa-code: $(printf %s '<random>' | sha256sum | cut -d' ' -f1)" \
    -H 'Content-Type: application/json' -d '{"tokenName":"agentd","bypassTwoFactor":true}'
  ```

  Update the Rocket.Chat section of `docs/design.md` with the result.
- **T14, agent lifecycle.** Create two agents by DMing the manager bot
  `create <name>`, invite both to a channel, mention each, and record that
  each mention arrives at its own bot: until turns exist (T23), the bot
  reacts with :eyes:. Then `pause` one and check it no longer reacts,
  `delete` it and check its user is deactivated, and restart agentd and
  check the other still reacts.
- **T23, turn pipeline.** With a linked Claude account (`login` in a DM to
  the manager bot), mention an agent in a channel, run a turn that uses Bash
  and returns a file, `docker compose restart agentd`, and continue the
  thread: the next turn resumes the session with `--resume`.

## Checking the networks

`scripts/ci/compose-test.sh` checks the sandbox image and what a container
on the `sandbox` network reaches, another sandbox included. It runs its own
Compose project, `agent-core-test`, with its own network names, but the
subnets and the bridge name are fixed, so stop this stack first. It needs
the rules from `isolate-sandbox.sh`:

```bash
docker compose down
sudo sh isolate-sandbox.sh
sh ../../scripts/ci/compose-test.sh
```

`docker compose run --rm sandbox bash` opens a shell in the sandbox image on
the `sandbox` network, to try things by hand. It has the network, init,
read-only root and dropped capabilities that agentd gives sessions, but
not their mounts, environment or resource limits.
