#!/bin/sh
# Checks the images built from images/ and the development stack in
# deploy/compose (T16 in docs/tasks-plan.md):
#
# 1. The sandbox image: `claude --version` prints the pinned
#    CLAUDE_CODE_VERSION, it runs as uid 10001 in /volume, idles under
#    tini, has the tools agents use, and has no `node`. The agentd image
#    runs as uid 10001.
# 2. The Compose networks: a container on `sandbox` reaches agentd's proxy
#    and ctl ports (8080 and 8081), and not its public port (8443),
#    Rocket.Chat, MongoDB, the host, a cloud metadata address or the
#    internet. Each unreachable target is first shown reachable from the
#    `egress` network, so a check can't pass because its target is down or
#    the probe is broken.
#
# Build the images first, then run it from anywhere:
#
#   DOCKER_GID="$(stat -c %g /var/run/docker.sock)" \
#     docker compose -f deploy/compose/compose.yaml --profile sandbox build
#   sh scripts/ci/compose-test.sh
#
# It brings up its own Compose project, agent-core-test, with a throwaway
# data directory and master key, ignores deploy/compose/.env, and removes
# everything it created on exit. The networks' subnets are fixed, so a
# development stack must be down first. It needs python3 on the host, for a
# listener that stands in for a service on the host.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
compose_file=$root/deploy/compose/compose.yaml
project=agent-core-test
sandbox_image=agent-core/sandbox:dev
agentd_image=agent-core/agentd:dev

# The addresses compose.yaml and config/agentd.example.toml fix.
agentd_egress=172.31.0.2
agentd_sandbox=172.30.0.2
egress_gateway=172.31.0.1
sandbox_gateway=172.30.0.1
host_port=18765

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

echo "== Images"

version=$(sed -n 's/^ARG CLAUDE_CODE_VERSION=//p' "$root/images/sandbox/Dockerfile")
if [ -z "$version" ]; then
    echo "images/sandbox/Dockerfile has no ARG CLAUDE_CODE_VERSION=<version>" >&2
    exit 1
fi

check_output "claude --version prints the pinned $version" "$version (Claude Code)" \
    docker run --rm "$sandbox_image" claude --version
check_output "the sandbox runs as uid 10001" 10001 docker run --rm "$sandbox_image" id -u
check_output "the sandbox runs as gid 10001" 10001 docker run --rm "$sandbox_image" id -g
check_output "the sandbox starts in /volume" /volume docker run --rm "$sandbox_image" pwd
if docker run --rm "$sandbox_image" which claude agentctl git curl jq rg; then
    pass "claude, agentctl, git, curl, jq and rg are on PATH"
else
    fail "a tool is missing from PATH"
fi
if docker run --rm "$sandbox_image" agentctl --version; then
    pass "agentctl runs"
else
    fail "agentctl --version failed"
fi
if docker run --rm "$sandbox_image" which node; then
    fail "which node found node"
else
    pass "which node fails"
fi
idle=$(docker run -d --rm "$sandbox_image")
sleep 2
check_output "the sandbox idles under tini" "true /usr/bin/tini -- sleep infinity" \
    docker inspect -f '{{.State.Running}} {{.Path}} {{join .Args " "}}' "$idle"
docker rm -f "$idle" >/dev/null
check_output "the agentd image runs as 10001:10001" 10001:10001 \
    docker image inspect -f '{{.Config.User}}' "$agentd_image"

echo "== Compose networks"

tmp=$(mktemp -d)
listener=
cleanup() {
    status=$?
    if [ -n "$listener" ]; then
        kill "$listener" 2>/dev/null || true
    fi
    compose down --volumes --remove-orphans >/dev/null 2>&1 || true
    # agentd's files belong to uid 10001, so a container removes them.
    docker run --rm -v "$tmp:/scratch" busybox:1.37 rm -rf /scratch/data >/dev/null 2>&1 || true
    rm -rf "$tmp"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

AGENT_CORE_DATA=$tmp/data
AGENT_CORE_CONFIG=$root/config/agentd.example.toml
DOCKER_GID=$(stat -c %g /var/run/docker.sock)
AGENTD_MASTER_KEY=$(docker run --rm "$agentd_image" gen-key)
export AGENT_CORE_DATA AGENT_CORE_CONFIG DOCKER_GID AGENTD_MASTER_KEY
unset AGENTD_RC_MANAGER_TOKEN

python3 -m http.server "$host_port" --bind 0.0.0.0 >/dev/null 2>&1 &
listener=$!

if ! compose up --detach --no-build --wait --wait-timeout 600 mongodb rocketchat agentd; then
    compose logs >&2 || true
    echo "the stack didn't come up" >&2
    exit 1
fi

# /healthz answers 200 once agentd has opened its store and serves.
if docker run --rm --network "${project}_egress" "$sandbox_image" \
    curl -fsS -o /dev/null --max-time 5 --retry 30 --retry-all-errors --retry-delay 1 \
    "http://$agentd_egress:8443/healthz"; then
    pass "agentd answers /healthz on its egress address"
else
    fail "agentd never answered /healthz"
fi

egress_ip() {
    docker inspect -f "{{(index .NetworkSettings.Networks \"${project}_egress\").IPAddress}}" \
        "$(compose ps -q "$1")"
}
rocketchat=$(egress_ip rocketchat)
mongodb=$(egress_ip mongodb)

# Runs in a container with each argument a check, "<expected> <target>":
# "open <host> <port>" or "closed <host> <port>" for a TCP connection within
# five seconds, and "resolves <name>" or "unresolved <name>" for DNS. Exits
# 1 if any check fails.
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
if docker run --rm --network "${project}_egress" "$sandbox_image" bash -c "$probe" probe \
    "open $agentd_egress 8443" \
    "open $rocketchat 3000" \
    "open $mongodb 27017" \
    "open $egress_gateway $host_port" \
    "resolves example.com" \
    "open example.com 443" \
    "open 1.1.1.1 443"; then
    pass "every target is reachable from egress"
else
    fail "a control target is unreachable from egress"
fi

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
    "closed 169.254.169.254 80" \
    "unresolved example.com" \
    "closed 1.1.1.1 443"; then
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
