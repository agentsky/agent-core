#!/bin/sh
# Checks the images built from images/ and the development stack in
# deploy/compose (T16 in docs/tasks-plan.md):
#
# 1. The sandbox image, run the way the sandbox crate runs it (Docker's
#    init, read-only root, a tmpfs /tmp, no capabilities, a HOME of its
#    own): `claude --version` prints the pinned CLAUDE_CODE_VERSION, it runs
#    as uid 10001 in /volume, has no entrypoint and idles under Docker's
#    init, has the tools agents use, and has no `node`. The agentd image
#    runs as uid 10001 and has `git`, which clones skills, and a `/bin/sh`
#    whose `ulimit -f` caps what a clone writes.
# 2. The stack: Rocket.Chat's first admin, from RC_ADMIN_PASS, can log in.
# 3. The Compose networks: a container on `sandbox` reaches agentd's proxy
#    and ctl ports (8080 and 8081), and not its public port (8443),
#    Rocket.Chat, MongoDB, the host, another container on `sandbox`, a
#    cloud metadata address or the internet, and has IPv6 off on eth0 with
#    no IPv6 address but loopback's. The host has no address on the sandbox
#    bridge but the IPv6 link-local one. Every unreachable target that
#    exists is first shown reachable from the `egress` network, so a check
#    can't pass because its target is down or the probe is broken: agentd's
#    8443, Rocket.Chat and MongoDB by address and by name, a listener on the
#    host's wildcard address, the peer container's listener before it moves
#    to `sandbox`, and the internet.
#    Three targets have no control because nothing answers there by
#    construction: agentd's sandbox address on 8443 (the public listener
#    binds its egress address only), the sandbox gateway 172.30.0.1 (the
#    host has no IPv4 address on the bridge, which is checked), and
#    169.254.169.254 (CI and most hosts have no metadata service; the
#    sandbox has no route to it, as to the internet).
#
# Build the images first, add the iptables rules that let sandboxes reach
# agentd (deploy/compose/isolate-sandbox.sh), then run it from anywhere:
#
#   DOCKER_GID="$(stat -c %g /var/run/docker.sock)" \
#     docker compose -f deploy/compose/compose.yaml --profile sandbox build
#   sudo sh deploy/compose/isolate-sandbox.sh
#   sh scripts/ci/compose-test.sh
#
# It brings up its own Compose project, agent-core-test, with its own
# network names, a throwaway data directory, master key and Rocket.Chat
# admin password, ignores deploy/compose/.env, and removes everything it
# created on exit. The networks' subnets and the sandbox bridge's name are
# fixed, so a development stack must be down first. It needs python3 on the
# host, for a listener that stands in for a service on the host.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
compose_file=$root/deploy/compose/compose.yaml
project=agent-core-test
sandbox_image=agent-core/sandbox:dev
agentd_image=agent-core/agentd:dev
sandbox_net=$project-sandbox
egress_net=$project-egress
peer=$project-peer

# The addresses and names compose.yaml and config/agentd.example.toml fix.
agentd_egress=172.31.0.2
agentd_sandbox=172.30.0.2
egress_gateway=172.31.0.1
sandbox_gateway=172.30.0.1
sandbox_bridge=br-agent-sbx
host_port=18765
peer_port=9000

failures=0

pass() {
    echo "ok   $*"
}

fail() {
    echo "FAIL $*"
    failures=$((failures + 1))
}

# check_output DESCRIPTION EXPECTED COMMAND...: passes when COMMAND succeeds
# and prints exactly EXPECTED.
check_output() {
    description=$1
    expected=$2
    shift 2
    if actual=$("$@" 2>&1) && [ "$actual" = "$expected" ]; then
        pass "$description"
    else
        fail "$description: expected '$expected', got '$actual'"
    fi
}

compose() {
    docker compose --env-file /dev/null -f "$compose_file" -p "$project" "$@"
}

# docker run with the confinement the sandbox crate's container_config
# gives a session, less its mounts and limits.
confined() {
    docker run --init --read-only --tmpfs /tmp:exec --cap-drop ALL \
        --security-opt no-new-privileges -e HOME=/tmp/h "$@"
}

sandbox() {
    confined --rm "$sandbox_image" "$@"
}

echo "== Images"

version=$(sed -n 's/^ARG CLAUDE_CODE_VERSION=//p' "$root/images/sandbox/Dockerfile")
if [ -z "$version" ]; then
    echo "images/sandbox/Dockerfile has no ARG CLAUDE_CODE_VERSION=<version>" >&2
    exit 1
fi

check_output "claude --version prints the pinned $version" "$version (Claude Code)" \
    sandbox claude --version
check_output "the sandbox runs as uid 10001" 10001 sandbox id -u
check_output "the sandbox runs as gid 10001" 10001 sandbox id -g
check_output "the sandbox starts in /volume" /volume sandbox pwd
if sandbox which claude agentctl git curl jq rg; then
    pass "claude, agentctl, git, curl, jq and rg are on PATH"
else
    fail "a tool is missing from PATH"
fi
if sandbox agentctl --version; then
    pass "agentctl runs"
else
    fail "agentctl --version failed"
fi
if sandbox which node; then
    fail "which node found node"
else
    pass "which node fails"
fi
check_output "the sandbox image has no entrypoint and idles with sleep infinity" \
    'null ["sleep","infinity"]' \
    docker image inspect -f '{{json .Config.Entrypoint}} {{json .Config.Cmd}}' "$sandbox_image"
idle=$(confined -d --rm "$sandbox_image")
sleep 2
check_output "the sandbox idles under Docker's init" "/sbin/docker-init -- sleep infinity" \
    docker exec "$idle" sh -c 'tr "\0" " " </proc/1/cmdline | sed "s/ $//"'
docker rm -f "$idle" >/dev/null
check_output "the agentd image runs as 10001:10001" 10001:10001 \
    docker image inspect -f '{{.Config.User}}' "$agentd_image"
if docker run --rm --entrypoint /bin/sh "$agentd_image" \
    -c 'ulimit -f 81920 && exec git --version'; then
    pass "the agentd image has git for skill clones, under ulimit -f"
else
    fail "git doesn't run under ulimit -f in the agentd image"
fi
check_output "ulimit -f counts 512-byte blocks in the agentd image" 1024 \
    docker run --rm --read-only --tmpfs /tmp --entrypoint /bin/sh "$agentd_image" \
    -c 'ulimit -f 2 && { head -c 4096 /dev/zero >/tmp/f; } 2>/dev/null; wc -c </tmp/f'

echo "== Compose stack"

tmp=$(mktemp -d)
listener=
cleanup() {
    status=$?
    if [ -n "$listener" ]; then
        kill "$listener" 2>/dev/null || true
    fi
    docker rm -f "$peer" >/dev/null 2>&1 || true
    compose down --volumes --remove-orphans >/dev/null 2>&1 || true
    # agentd's files belong to uid 10001, so a container removes them.
    docker run --rm -v "$tmp:/scratch" busybox:1.37 rm -rf /scratch/data >/dev/null 2>&1 || true
    rm -rf "$tmp"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

AGENT_CORE_DATA=$tmp/data
AGENT_CORE_CONFIG=$tmp/agentd.toml
awk '/^\[/ { skip = ($0 == "[rocketchat]") } !skip' "$root/config/agentd.example.toml" >"$AGENT_CORE_CONFIG"
AGENT_CORE_SANDBOX_NETWORK=$sandbox_net
AGENT_CORE_EGRESS_NETWORK=$egress_net
DOCKER_GID=$(stat -c %g /var/run/docker.sock)
AGENTD_MASTER_KEY=$(docker run --rm "$agentd_image" gen-key)
RC_ADMIN_USERNAME=admin
RC_ADMIN_PASS=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
export AGENT_CORE_DATA AGENT_CORE_CONFIG AGENT_CORE_SANDBOX_NETWORK AGENT_CORE_EGRESS_NETWORK \
    DOCKER_GID AGENTD_MASTER_KEY RC_ADMIN_USERNAME RC_ADMIN_PASS
unset AGENTD_RC_MANAGER_TOKEN RC_ADMIN_EMAIL

# Dual-stack where the host has IPv6, so the listener answers on every
# address the host has.
if [ -e /proc/net/if_inet6 ]; then
    host_bind=::
else
    host_bind=0.0.0.0
fi
python3 -m http.server "$host_port" --bind "$host_bind" >/dev/null 2>&1 &
listener=$!

if ! compose up --detach --no-build --wait --wait-timeout 600 mongodb rocketchat agentd; then
    compose logs >&2 || true
    echo "the stack didn't come up" >&2
    exit 1
fi

# /healthz answers 200 once agentd has opened its store and serves.
if docker run --rm --network "$egress_net" "$sandbox_image" \
    curl -fsS -o /dev/null --max-time 5 --retry 30 --retry-all-errors --retry-delay 1 \
    "http://$agentd_egress:8443/healthz"; then
    pass "agentd answers /healthz on its egress address"
else
    fail "agentd never answered /healthz"
fi

# README.md's step 2 logs in as this admin.
login=$(docker run --rm --network "$egress_net" "$sandbox_image" \
    curl -sS --max-time 10 --retry 10 --retry-all-errors --retry-delay 3 --fail \
    -H 'Content-Type: application/json' \
    -d "{\"user\":\"$RC_ADMIN_USERNAME\",\"password\":\"$RC_ADMIN_PASS\"}" \
    http://rocketchat:3000/api/v1/login 2>&1) || true
if printf '%s' "$login" | grep -q '"status": *"success"'; then
    pass "Rocket.Chat's admin from RC_ADMIN_PASS logs in"
else
    fail "Rocket.Chat's admin couldn't log in: $(printf '%s' "$login" | head -c 300)"
fi

echo "== Compose networks"

# The host's addresses on the sandbox bridge, as a container in the host's
# network namespace lists them: no IPv4 address, and no IPv6 address but
# the link-local one the kernel gives every interface. Sandboxes have IPv6
# off (the no-ipv6 check below), so they can't reach that one.
if bridge_addresses=$(docker run --rm --network host busybox:1.37 ip addr show dev "$sandbox_bridge" 2>&1); then
    unexpected=$(printf '%s\n' "$bridge_addresses" | grep -E '^ *inet' | grep -Ev '^ *inet6 fe80:[^ ]* scope link') || true
else
    unexpected="no bridge $sandbox_bridge: $bridge_addresses"
fi
if [ -n "$unexpected" ]; then
    fail "the host has an address on $sandbox_bridge: $unexpected"
else
    pass "the host has no IPv4 address and only a link-local IPv6 address on $sandbox_bridge"
fi

egress_ip() {
    docker inspect -f "{{(index .NetworkSettings.Networks \"$egress_net\").IPAddress}}" "$1"
}
rocketchat=$(egress_ip "$(compose ps -q rocketchat)")
mongodb=$(egress_ip "$(compose ps -q mongodb)")

# Another container, which will move to `sandbox`, stands in for a second
# session with a listener. The sandbox image has no tool that listens, so
# it is busybox's nc.
docker run -d --name "$peer" --network "$egress_net" busybox:1.37 \
    nc -lk -p "$peer_port" -e echo hi >/dev/null
peer_egress=$(egress_ip "$peer")

# Runs in a container with each argument a check, "<expected> <target>":
# "open <host> <port>" or "closed <host> <port>" for a TCP connection within
# five seconds, "resolves <name>" or "unresolved <name>" for DNS, and
# "no-ipv6 eth0" for IPv6 turned off on eth0 and no IPv6 address but
# loopback's. Exits 1 if any check fails.
probe='
failed=0
for check in "$@"; do
    expected=${check%% *}
    target=${check#* }
    case $expected in
    open | closed)
        if timeout 5 bash -c "exec 3<>/dev/tcp/\$0/\$1" ${target% *} ${target#* } 2>/dev/null; then
            actual=open
        else
            actual=closed
        fi
        ;;
    resolves | unresolved)
        if getent hosts "$target" >/dev/null; then
            actual=resolves
        else
            actual=unresolved
        fi
        ;;
    no-ipv6)
        actual=no-ipv6
        switch=/proc/sys/net/ipv6/conf/$target/disable_ipv6
        if [ -e "$switch" ] && [ "$(cat "$switch")" != 1 ]; then
            actual="IPv6 is on for $target"
        fi
        if [ -e /proc/net/if_inet6 ] && grep -v " lo$" /proc/net/if_inet6 | grep -q .; then
            actual="IPv6 addresses on $(grep -v " lo$" /proc/net/if_inet6 | tr -s " " | cut -d" " -f6 | sort -u | tr "\n" " ")"
        fi
        ;;
    *)
        actual="an unknown check"
        ;;
    esac
    if [ "$actual" = "$expected" ]; then
        echo "ok   $check"
    else
        echo "FAIL $check: $actual"
        failed=1
    fi
done
exit $failed
'

echo "-- From the egress network (controls)"
if docker run --rm --network "$egress_net" "$sandbox_image" bash -c "$probe" probe \
    "open $agentd_egress 8443" \
    "open $rocketchat 3000" \
    "open $mongodb 27017" \
    "resolves rocketchat" \
    "resolves mongodb" \
    "open $egress_gateway $host_port" \
    "open $peer_egress $peer_port" \
    "resolves example.com" \
    "open example.com 443" \
    "open 1.1.1.1 443"; then
    pass "every target is reachable from egress"
else
    fail "a control target is unreachable from egress"
fi

docker network connect "$sandbox_net" "$peer"
docker network disconnect "$egress_net" "$peer"
peer_sandbox=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$sandbox_net\").IPAddress}}" "$peer")

echo "-- From the sandbox network"
if compose run --rm --no-deps -T sandbox bash -c "$probe" probe \
    "open $agentd_sandbox 8080" \
    "open $agentd_sandbox 8081" \
    "open cred-proxy.internal 8080" \
    "open agentctl.internal 8081" \
    "closed $agentd_sandbox 8443" \
    "closed $agentd_egress 8443" \
    "closed $rocketchat 3000" \
    "closed $mongodb 27017" \
    "unresolved rocketchat" \
    "unresolved mongodb" \
    "closed $sandbox_gateway $host_port" \
    "closed $egress_gateway $host_port" \
    "closed $peer_sandbox $peer_port" \
    "closed 169.254.169.254 80" \
    "unresolved example.com" \
    "closed 1.1.1.1 443" \
    "no-ipv6 eth0"; then
    pass "a sandbox reaches agentd's 8080 and 8081 only"
else
    fail "the sandbox network's reachability is wrong"
fi

if [ "$failures" -gt 0 ]; then
    compose logs agentd >&2 || true
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "All checks passed"
