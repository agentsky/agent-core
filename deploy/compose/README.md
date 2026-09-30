# Development stack

[`compose.yaml`](compose.yaml) runs Rocket.Chat 7.x with MongoDB, and
agentd, on the two networks from the plan's
[Network and deployment shape](../../docs/tasks-plan.md#network-and-deployment-shape):

| Network | Kind | Members |
| --- | --- | --- |
| `egress` (`172.31.0.0/24`) | normal bridge | Rocket.Chat, MongoDB, agentd at `172.31.0.2` |
| `sandbox` (`172.30.0.0/24`) | internal, and the host has no address on it | agentd at `172.30.0.2` (`cred-proxy.internal`, `agentctl.internal`), sandboxes |

agentd's public listener is `172.31.0.2:8443`, the credential proxy
`172.30.0.2:8080` and the agentctl API `172.30.0.2:8081`, as in
[`config/agentd.example.toml`](../../config/agentd.example.toml). A sandbox
reaches those two ports and nothing else; `scripts/ci/compose-test.sh`
checks that in CI. Docker names the networks `agent-core_egress` and
`agent-core_sandbox`.

On the host, Rocket.Chat is at <http://localhost:3000> and agentd's public
listener at `127.0.0.1:8443`. Both bind the loopback address only.

The commands below run in this directory. They need Docker with the Compose
v2 plugin, and on Linux a user allowed to use the Docker socket.

## 1. Bring the stack up

Settings and secrets live in `.env` here, which Compose reads and Git
ignores. [`compose.yaml`](compose.yaml)'s header lists every variable.

```bash
# The group owning the Docker socket, as containers see it. agentd, which
# runs as uid 10001, joins it to start sandboxes.
echo "DOCKER_GID=$(docker run --rm -v /var/run/docker.sock:/sock busybox:1.37 stat -c %g /sock)" > .env

# Build agentd's image and the sandbox image agentd starts sessions from.
docker compose --profile sandbox build

# The master key that encrypts agentd's stored secrets. Keep a copy: losing
# it makes every stored secret unreadable.
echo "AGENTD_MASTER_KEY=$(docker compose run --rm --no-deps agentd gen-key)" >> .env

# Rocket.Chat creates this admin on its first start.
echo "RC_ADMIN_PASS=$(openssl rand -base64 18)" >> .env

docker compose up -d
docker compose ps
curl -fsS http://127.0.0.1:8443/healthz   # "ok" once agentd serves
```

Rocket.Chat takes a minute or two to start the first time. `docker compose
logs -f rocketchat` shows when it is up.

agentd keeps its database and, later, the agents' volumes under `./data`
(`AGENT_CORE_DATA`), owned by uid 10001 with mode 0700, so reading it on
the host needs `sudo`. `docker compose down` stops the stack and keeps
everything; `docker compose down -v` also deletes Rocket.Chat's database.

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
   what agentd needs. From the server source
   ([impl-notes](../../docs/impl-notes.md#what-the-server-source-says-about-the-managers-custom-role)),
   that is:

   | Permission | For |
   | --- | --- |
   | `create-user` | `/agent create` |
   | `view-full-other-user-info` | telling bots from people ([impl-notes](../../docs/impl-notes.md#messages-dont-carry-the-senders-roles)) |
   | `edit-other-user-active-status` | `/agent delete` |
   | `edit-other-user-info` | renaming a bot |
   | `edit-other-user-avatar` | setting a bot's avatar |
   | `add-user-to-joined-room` | inviting a bot where the manager is a member |
   | `api-bypass-rate-limit` | the manager's own REST calls |
   | `create-personal-access-tokens` | the manager's own token |

   Creating a role is an Enterprise feature: `roles.create` needs the
   `custom-roles` license module
   ([impl-notes](../../docs/impl-notes.md#custom-roles-need-a-rocketchat-enterprise-license)).
   With a license, create a global role `agent-manager` in **Administration >
   Workspace > Permissions > Roles** and tick the permissions above. On the
   Community Edition this stack runs by default, there is no least-privilege
   setup yet; for development, give the manager the `admin` role.
4. Create the manager in **Administration > Workspace > Users > New user**:
   username `agent-manager`, any email, a password, roles `user` and the role
   from step 3.
5. Log in as the manager and, in **My account > Personal access tokens**,
   create a token with **Ignore Two Factor Authentication** ticked (the
   endpoint for renaming a bot requires two-factor authentication otherwise).
   Note the token and the user id shown with it.

## 3. Configure agentd

agentd reads [`config/agentd.example.toml`](../../config/agentd.example.toml)
by default, whose addresses match `compose.yaml`. To change it, copy it here
and point `AGENT_CORE_CONFIG` at the copy:

```bash
cp ../../config/agentd.example.toml agentd.toml
echo "AGENT_CORE_CONFIG=./agentd.toml" >> .env
```

Keep the `[server]` and `[internal]` addresses unless you change
`compose.yaml` to match. Add the manager's token from step 2 to `.env`:

```bash
echo "AGENTD_RC_MANAGER_TOKEN=<token>" >> .env
```

The manager's user id and Rocket.Chat's address (`http://rocketchat:3000` on
the `egress` network) go in the `[rocketchat]` section once agentd reads
one. For sandboxes, the `[sandbox]` section, once agentd reads it, takes:

```toml
[sandbox]
image = "agent-core/sandbox:dev"
network = "agent-core_sandbox"
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
  each mention arrives at its own bot.
- **T23, turn pipeline.** With a linked Claude account (`login` in a DM to
  the manager bot), mention an agent in a channel, run a turn that uses Bash
  and returns a file, `docker compose restart agentd`, and continue the
  thread: the next turn resumes the session with `--resume`.

## Checking the networks

`scripts/ci/compose-test.sh` checks the sandbox image and what a container
on the `sandbox` network reaches. It runs its own Compose project,
`agent-core-test`, but the subnets are fixed, so stop this stack first:

```bash
docker compose down
sh ../../scripts/ci/compose-test.sh
```

`docker compose run --rm sandbox bash` opens a shell in the sandbox image on
the `sandbox` network, confined the way agentd confines sessions, to try
things by hand.
