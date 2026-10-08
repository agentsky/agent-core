#!/bin/sh
# The Rocket.Chat end-to-end test (T37 in docs/tasks-plan.md): the real
# Rocket.Chat, agentd and sandbox images from deploy/compose and the real
# Claude Code CLI, with only Anthropic faked.
#
# 1. Starts MongoDB and Rocket.Chat, and waits until the admin can post. A
#    Community Edition workspace answers `restricted-workspace` to posts
#    until it has reported statistics to Rocket.Chat Cloud, which it does at
#    startup when it can reach it (docs/impl-notes.md, "The live check
#    against 7.13.9").
# 2. Does step 2 of deploy/compose/README.md through the REST API: the `bot`
#    and `app` permissions, the manager holding those roles with a personal
#    access token that bypasses two-factor authentication, and two members,
#    alice and bob.
# 3. Starts agentd with the example configuration, a [rocketchat] section
#    for the manager, the admin as community admin, [proxy] upstream and the
#    [claude_oauth] URLs on fake-anthropic, [sandbox] on this project's
#    network under an instance name of its own, and the Rocket.Chat surface
#    logging at debug level. agentd's realtime connections don't fetch a
#    message posted in a room before they subscribe to it, and a new direct
#    message with the manager is a room no connection is in yet, so the
#    members' direct messages with the manager are opened before agentd
#    starts, and the test waits for the debug line saying the manager's
#    connection has sent its subscriptions before posting. A mention in
#    #general needs no such wait: the manager is in #general, and on
#    Rocket.Chat whichever connection hears a message delivers it for every
#    agent it mentions.
# 4. Runs fake-anthropic (crates/testkit/src/bin/fake-anthropic.rs) in
#    agentd's network namespace, from the agentd image: the upstream and the
#    OAuth URLs may only be plain HTTP to a loopback address.
# 5. As alice, in a direct message to the manager, `login`, then
#    `login <code>#<state>` with the state from the login link, which the
#    fake's token endpoint accepts. In #general, `!agent create helper`, then
#    a mention of its bot, which must answer in the mention's thread with
#    fake-anthropic's reply. The admin sets the community API key, and bob,
#    who has linked no account, mentions the bot and gets the same answer.
# 6. Checks fake-anthropic's log: a /v1/messages request on the access token
#    it issued, one on the community key, and no request carrying any other
#    value, so every turn went through the credential proxy's swap.
#
# Build the images and fake-anthropic first, add the iptables rules that let
# sandboxes reach agentd, then run it from anywhere:
#
#   DOCKER_GID="$(stat -c %g /var/run/docker.sock)" \
#     docker compose -f deploy/compose/compose.yaml --profile sandbox build
#   cargo build --locked -p testkit --bin fake-anthropic
#   sudo sh deploy/compose/isolate-sandbox.sh
#   sh scripts/ci/rocketchat-e2e.sh
#
# FAKE_ANTHROPIC names the binary when it isn't target/debug/fake-anthropic.
# It runs on the agentd image's Debian, so the host's glibc must be no newer
# than that one's. The script needs curl and jq on the host, and Rocket.Chat
# needs to reach Rocket.Chat Cloud. alice and bob lack Rocket.Chat's
# api-bypass-rate-limit, so their polling is spaced out, and a wait's budget
# covers a rate-limited minute.
#
# It brings up its own Compose project, agent-core-e2e, with its own network
# names, a throwaway data directory, master key and passwords, ignores
# deploy/compose/.env, and removes everything it created on exit, the
# sandboxes agentd started included. Rocket.Chat is published on
# 127.0.0.1:3000 and agentd on 127.0.0.1:8443, and the networks' subnets and
# the sandbox bridge's name are fixed, so a development stack must be down
# first. When a step fails it prints the logs of agentd, fake-anthropic and
# Rocket.Chat.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
compose_file=$root/deploy/compose/compose.yaml
project=agent-core-e2e
agentd_image=agent-core/agentd:dev
fake_bin=${FAKE_ANTHROPIC:-$root/target/debug/fake-anthropic}
fake=$project-fake
fake_url=http://127.0.0.1:18080
rc=http://127.0.0.1:3000
team=e2e
manager=agent-manager
community_key=sk-ant-api03-e2e-community-key

die() {
    echo "FAIL $*" >&2
    exit 1
}

pass() {
    echo "ok   $*"
}

secret() {
    od -An -N16 -tx1 /dev/urandom | tr -d ' \n'
}

compose() {
    docker compose --env-file /dev/null -f "$compose_file" -p "$project" "$@"
}

reply=$(sed -n 's/^pub const DEFAULT_REPLY: &str = "\(.*\)";$/\1/p' "$root/crates/testkit/src/anthropic.rs")
[ -n "$reply" ] || die "crates/testkit/src/anthropic.rs has no DEFAULT_REPLY"
[ -x "$fake_bin" ] ||
    die "no fake-anthropic at $fake_bin: cargo build --locked -p testkit --bin fake-anthropic"

tmp=$(mktemp -d)
cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        echo "== agentd's log" >&2
        compose logs --no-color agentd >&2 || true
        echo "== fake-anthropic's log" >&2
        docker logs "$fake" >&2 2>&1 || true
        echo "== Rocket.Chat's log, the last 200 lines" >&2
        compose logs --no-color --tail 200 rocketchat >&2 || true
    fi
    docker rm -f "$fake" >/dev/null 2>&1 || true
    compose stop agentd >/dev/null 2>&1 || true
    docker ps -aq --filter "label=agentd.instance=$project" 2>/dev/null |
        xargs -r docker rm -f >/dev/null 2>&1 || true
    compose down --volumes --remove-orphans >/dev/null 2>&1 || true
    # agentd's files belong to uid 10001, so a container removes them.
    docker run --rm -v "$tmp:/scratch" busybox:1.37 rm -rf /scratch/data >/dev/null 2>&1 || true
    rm -rf "$tmp"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# poll SECONDS COMMAND...: runs COMMAND every six seconds until it
# succeeds, for at most SECONDS. Fails if it never does.
poll() {
    poll_deadline=$(($(date +%s) + $1))
    shift
    until "$@"; do
        if [ "$(date +%s)" -ge "$poll_deadline" ]; then
            return 1
        fi
        sleep 6
    done
}

# api USER_ID TOKEN METHOD PATH [BODY]: one REST call as that user. Prints
# the answer, and fails on an HTTP error.
api() {
    api_user=$1
    api_token=$2
    api_method=$3
    api_path=$4
    shift 4
    if [ "$#" -gt 0 ]; then
        set -- --data "$1"
    fi
    curl -sS --fail-with-body --max-time 30 -X "$api_method" \
        -H "X-User-Id: $api_user" -H "X-Auth-Token: $api_token" \
        -H 'Content-Type: application/json' "$@" "$rc/api/v1/$api_path"
}

# login USER PASSWORD: prints "<user id> <auth token>".
login() {
    curl -sS --fail-with-body --max-time 30 -H 'Content-Type: application/json' \
        --data "$(jq -cn --arg user "$1" --arg password "$2" '{user: $user, password: $password}')" \
        "$rc/api/v1/login" | jq -er '.data.userId + " " + .data.authToken'
}

# create_user USERNAME PASSWORD ROLES: creates the user with the roles (a
# JSON array) and an unverified address, so email two-factor authentication
# never applies. Prints its id.
create_user() {
    api "$admin_id" "$admin_token" POST users.create "$(jq -cn \
        --arg username "$1" --arg password "$2" --argjson roles "$3" \
        '{username: $username, name: $username, email: ($username + "@e2e.invalid"),
          password: $password, roles: $roles, verified: false,
          requirePasswordChange: false, joinDefaultChannels: true}')" |
        jq -er .user._id
}

# say USER_ID TOKEN ROOM_ID TEXT: posts TEXT in the room. Prints its id.
say() {
    api "$1" "$2" POST chat.postMessage \
        "$(jq -cn --arg room "$3" --arg text "$4" '{roomId: $room, text: $text}')" |
        jq -er .message._id
}

# dm USER_ID TOKEN USERNAME: opens the user's direct message with USERNAME.
# Prints its room id.
dm() {
    api "$1" "$2" POST im.create "$(jq -cn --arg username "$3" '{username: $username}')" |
        jq -er --arg username "$3" 'select(.room.usernames | index($username)) | .room._id'
}

# manager_said USER_ID TOKEN ROOM_ID TEXT: whether the manager has said
# something containing TEXT in the direct message. Sets `found` to it.
manager_said() {
    found=$(api "$1" "$2" GET "im.history?roomId=$3&count=50" |
        jq -r --arg from "$manager" --arg text "$4" \
            '[.messages[] | select(.u.username == $from and ((.msg // "") | contains($text)))][0].msg // empty') ||
        true
    [ -n "$found" ]
}

# wait_manager USER_ID TOKEN ROOM_ID TEXT: waits for the manager to say
# TEXT there, and fails showing the direct message if it doesn't.
wait_manager() {
    if ! poll 120 manager_said "$@"; then
        echo "The direct message with @$manager holds, newest first:" >&2
        api "$1" "$2" GET "im.history?roomId=$3&count=20" |
            jq -r '.messages[] | "  @\(.u.username): \(.msg)"' >&2 || true
        die "@$manager never said '$4'"
    fi
}

# logged MESSAGE USER_ID: whether agentd has logged MESSAGE for that user.
logged() {
    compose logs --no-color --no-log-prefix agentd 2>/dev/null |
        jq -Rc --arg message "$1" --arg user "$2" '
            fromjson? | .fields | select(.message == $message and .user == $user)' |
        grep -q .
}

# bot_answered USER_ID TOKEN ROOT_ID: whether the bot has answered with
# fake-anthropic's reply in the thread of ROOT_ID.
bot_answered() {
    found=$(api "$1" "$2" GET "chat.getThreadMessages?tmid=$3&count=50" |
        jq -r --arg from "$bot" --arg text "$reply" \
            '[.messages[] | select(.u.username == $from and ((.msg // "") | contains($text)))][0].msg // empty') ||
        true
    [ -n "$found" ]
}

# wait_answer USER_ID TOKEN ROOT_ID WHO: waits for the bot's answer to WHO's
# mention, and fails showing the thread if it doesn't come.
wait_answer() {
    if ! poll 300 bot_answered "$1" "$2" "$3"; then
        echo "The thread of $4's mention holds:" >&2
        api "$1" "$2" GET "chat.getThreadMessages?tmid=$3&count=50" |
            jq -r '.messages[] | "  @\(.u.username): \(.msg)"' >&2 || true
        die "@$bot didn't answer $4's mention in its thread"
    fi
    pass "@$bot answered $4's mention in its thread"
}

echo "== Rocket.Chat"

AGENT_CORE_DATA=$tmp/data
AGENT_CORE_CONFIG=$tmp/agentd.toml
AGENT_CORE_SANDBOX_NETWORK=$project-sandbox
AGENT_CORE_EGRESS_NETWORK=$project-egress
DOCKER_GID=$(stat -c %g /var/run/docker.sock)
AGENTD_MASTER_KEY=$(docker run --rm "$agentd_image" gen-key)
RC_ADMIN_USERNAME=admin
RC_ADMIN_PASS=$(secret)
export AGENT_CORE_DATA AGENT_CORE_CONFIG AGENT_CORE_SANDBOX_NETWORK AGENT_CORE_EGRESS_NETWORK \
    DOCKER_GID AGENTD_MASTER_KEY RC_ADMIN_USERNAME RC_ADMIN_PASS
unset AGENTD_RC_MANAGER_TOKEN RC_ADMIN_EMAIL

compose up --detach --no-build --wait --wait-timeout 600 mongodb rocketchat ||
    die "MongoDB and Rocket.Chat didn't come up"

admin_login() {
    admin=$(login "$RC_ADMIN_USERNAME" "$RC_ADMIN_PASS" 2>/dev/null) || return 1
    admin_id=${admin% *}
    admin_token=${admin#* }
}
poll 120 admin_login || die "Rocket.Chat's admin never logged in"
pass "Rocket.Chat's admin logs in"

can_post() {
    posted=$(api "$admin_id" "$admin_token" POST chat.postMessage \
        '{"channel":"#general","text":"The end-to-end test starts."}' 2>&1)
}
if ! poll 300 can_post; then
    echo "Rocket.Chat answered: $posted" >&2
    die "Rocket.Chat refuses posts; a Community Edition workspace that can't report to Rocket.Chat Cloud answers restricted-workspace"
fi
pass "the admin can post"

echo "== The manager and the members"

permissions=$(api "$admin_id" "$admin_token" GET permissions.listAll) ||
    die "permissions.listAll failed: $permissions"
grant=$(printf '%s' "$permissions" | jq -c '
    def grant($role; $ids):
        [.update[] | select(._id as $id | $ids | index($id))
         | {_id, roles: (.roles + [$role] | unique)}];
    {permissions: (
        grant("app"; ["create-user", "view-full-other-user-info",
                      "edit-other-user-active-status", "add-user-to-joined-room"])
        + grant("bot"; ["create-personal-access-tokens"]))}')
[ "$(printf '%s' "$grant" | jq '.permissions | length')" = 5 ] ||
    die "permissions.listAll lacks a permission the manager needs: $grant"
api "$admin_id" "$admin_token" POST permissions.update "$grant" >/dev/null
pass "the app and bot roles have the manager's permissions"

manager_pass=$(secret)
alice_pass=$(secret)
bob_pass=$(secret)
manager_id=$(create_user "$manager" "$manager_pass" '["bot","app"]')
create_user alice "$alice_pass" '["user"]' >/dev/null
create_user bob "$bob_pass" '["user"]' >/dev/null
manager_session=$(login "$manager" "$manager_pass")
AGENTD_RC_MANAGER_TOKEN=$(curl -sS --fail-with-body --max-time 30 \
    -H "X-User-Id: ${manager_session% *}" -H "X-Auth-Token: ${manager_session#* }" \
    -H "x-2fa-code: $(printf '%s' "$manager_pass" | sha256sum | cut -d' ' -f1)" \
    -H 'x-2fa-method: password' -H 'Content-Type: application/json' \
    --data '{"tokenName":"agentd","bypassTwoFactor":true}' \
    "$rc/api/v1/users.generatePersonalAccessToken" | jq -er .token)
export AGENTD_RC_MANAGER_TOKEN
pass "@$manager has a personal access token"

alice=$(login alice "$alice_pass")
alice_id=${alice% *}
alice_token=${alice#* }
alice_dm=$(dm "$alice_id" "$alice_token" "$manager")
admin_dm=$(dm "$admin_id" "$admin_token" "$manager")
general=$(api "$alice_id" "$alice_token" GET "rooms.info?roomName=general" | jq -er .room._id)
pass "alice and the admin have direct messages with @$manager"

echo "== agentd and fake-anthropic"

awk '/^\[/ {
        skip = ($0 == "[proxy]" || $0 == "[claude_oauth]" || $0 == "[rocketchat]" ||
                $0 == "[community]" || $0 == "[sandbox]")
    }
    !skip' "$root/config/agentd.example.toml" |
    sed 's/^log_filter = "info"$/log_filter = "info,surface_rocketchat=debug"/' >"$AGENT_CORE_CONFIG"
grep -q '^log_filter = "info,surface_rocketchat=debug"$' "$AGENT_CORE_CONFIG" ||
    die "config/agentd.example.toml has no log_filter = \"info\" line to change"
cat >>"$AGENT_CORE_CONFIG" <<EOF

[proxy]
upstream = "$fake_url"

[claude_oauth]
token_url = "$fake_url/v1/oauth/token"
revoke_url = "$fake_url/v1/oauth/token/revoke"
profile_url = "$fake_url/api/oauth/profile"

[rocketchat]
base_url = "http://rocketchat:3000"
team = "$team"
manager_user_id = "$manager_id"

[community]
admins = ["rocketchat:$team:$admin_id"]

[sandbox]
image = "agent-core/sandbox:dev"
network = "$AGENT_CORE_SANDBOX_NETWORK"
host_data_dir = "$AGENT_CORE_DATA"
instance = "$project"
EOF

compose up --detach --no-build --wait --wait-timeout 120 agentd || die "agentd didn't start"
healthy() {
    curl -fsS -o /dev/null --max-time 5 http://127.0.0.1:8443/healthz
}
poll 60 healthy || die "agentd never answered /healthz"
pass "agentd answers /healthz"
poll 120 logged "realtime connection ready" "$manager_id" ||
    die "@$manager's realtime connection never got ready"
pass "@$manager's realtime connection is ready"

docker run --detach --name "$fake" --network "container:$(compose ps -q agentd)" \
    --read-only --cap-drop ALL --security-opt no-new-privileges \
    --volume "$fake_bin:/usr/local/bin/fake-anthropic:ro" \
    --entrypoint /usr/local/bin/fake-anthropic \
    "$agentd_image" --listen "${fake_url#http://}" --community-key "$community_key" >/dev/null
listening() {
    docker logs "$fake" 2>&1 | grep -q '^listening on '
}
poll 30 listening || die "fake-anthropic never listened"
pass "fake-anthropic listens in agentd's network namespace"

echo "== Linking, creating and mentioning"

say "$alice_id" "$alice_token" "$alice_dm" login >/dev/null
wait_manager "$alice_id" "$alice_token" "$alice_dm" "open the Claude login page"
state=$(printf '%s' "$found" | grep -o 'state=[A-Za-z0-9_-]*' | head -n 1 | cut -d= -f2)
[ -n "$state" ] || die "the login link has no state: $found"
say "$alice_id" "$alice_token" "$alice_dm" "login e2e-code#$state" >/dev/null
wait_manager "$alice_id" "$alice_token" "$alice_dm" "Your Claude account is linked."
pass "alice linked her Claude account"

say "$alice_id" "$alice_token" "$general" "!agent create helper" >/dev/null
wait_manager "$alice_id" "$alice_token" "$alice_dm" "Created \`helper\`."
bot=$(printf '%s' "$found" | sed -n 's/.*Its bot user is @\([A-Za-z0-9._-]*\).*/\1/p')
bot=${bot%.}
[ -n "$bot" ] || die "the manager named no bot user: $found"
case $found in
*"I added it to the room you asked in."*) ;;
*) die "the manager didn't add @$bot to #general: $found" ;;
esac
pass "alice created helper, whose bot @$bot is in #general"

root_id=$(say "$alice_id" "$alice_token" "$general" "@$bot what is two plus two?")
wait_answer "$alice_id" "$alice_token" "$root_id" alice

say "$admin_id" "$admin_token" "$admin_dm" "admin api-key set $community_key" >/dev/null
wait_manager "$admin_id" "$admin_token" "$admin_dm" "The community API key is set."
pass "the admin set the community API key"

bob=$(login bob "$bob_pass")
root_id=$(say "${bob% *}" "${bob#* }" "$general" "@$bot and what is three plus three?")
wait_answer "${bob% *}" "${bob#* }" "$root_id" bob

echo "== The credentials fake-anthropic saw"

# fake-anthropic prints the requests it recorded every 250 ms, so its log
# can trail the answer the test saw. A log that holds still for a second
# has printed them all.
fake_requests() {
    docker logs "$fake" 2>&1 | grep -v '^listening on ' || true
}
settled() {
    printed=$(fake_requests | wc -l)
    sleep 1
    [ "$(fake_requests | wc -l)" = "$printed" ]
}
poll 30 settled || die "fake-anthropic's log never stopped growing"
requests=$(fake_requests)
printf '%s\n' "$requests" | sort | uniq -c
printf '%s\n' "$requests" | grep -qx 'POST /v1/messages authorization=issued x-api-key=absent' ||
    die "no turn ran on the access token alice linked"
pass "alice's turn ran on her linked account"
printf '%s\n' "$requests" | grep -qx 'POST /v1/messages authorization=absent x-api-key=community' ||
    die "no turn ran on the community key"
pass "bob's turn ran on the community key"
if printf '%s\n' "$requests" | grep -q '=other'; then
    die "a request reached fake-anthropic with a credential the proxy didn't swap"
fi
pass "every credential fake-anthropic saw was one the proxy swapped in"
if printf '%s\n' "$requests" | grep -Ev '^POST /v1/oauth/token(/revoke)? ' |
    grep -q 'authorization=absent x-api-key=absent'; then
    die "a request reached fake-anthropic with no credential"
fi
pass "every request but the OAuth token endpoints carried a credential"

echo "All checks passed"
