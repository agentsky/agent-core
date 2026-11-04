#!/bin/sh
# Runs agentd for the Slack live checks (deploy/slack/README.md) behind a
# Cloudflare tunnel for a set time, then removes everything it started. The
# `Slack live server` workflow (.github/workflows/slack-live.yml) runs it on
# a GitHub runner; it runs the same on any Docker host.
#
# 1. Writes agentd's configuration as the runbook's step 1 does: the
#    example's, with [slack] public_url on CF_URL_BASE and the install
#    reminder after a minute, the community admins from COMMUNITY_ADMINS,
#    [sandbox] on this project's network under an instance name of its own,
#    and the pipeline logging at debug level.
# 2. Starts agentd with a throwaway master key and data directory, and the
#    Slack manager app's secrets when they are set. compose.yaml doesn't pass
#    those, so a Compose file of its own adds them, as the runbook's
#    compose.override.yaml does.
# 3. Runs cloudflared on the tunnel CF_TOKEN names, in the host's network,
#    and waits for CF_URL_BASE/healthz to answer through it.
# 4. Stays up for WAIT_MINUTES, printing agentd's log as it is written when
#    SHOW_LOG is true, and fails if cloudflared stops.
#
# Variables:
#
#   CF_TOKEN       Required. The token of a Cloudflare tunnel managed from
#                  Cloudflare's dashboard, whose public hostname's service is
#                  http://localhost:8443, agentd's public listener as
#                  compose.yaml publishes it.
#   CF_URL_BASE    Required. That hostname as an https:// URL with no path,
#                  which becomes [slack] public_url.
#   WAIT_MINUTES   How long agentd stays up once the tunnel answers, from 1
#                  to 300. Default 60.
#   AGENTD_SLACK_MANAGER_SIGNING_SECRET, AGENTD_SLACK_MANAGER_BOT_TOKEN
#                  The manager app's secrets. Without them agentd serves no
#                  Slack requests, which is enough only to create the
#                  manager app (check A1).
#   COMMUNITY_ADMINS
#                  [community] admins, separated by spaces or commas, each
#                  slack:<team id>:<user id>. Default: nobody.
#   SHOW_LOG       true prints agentd's log while it is up. Default true.
#
# Build the images and add the iptables rules that let sandboxes reach
# agentd first:
#
#   DOCKER_GID="$(stat -c %g /var/run/docker.sock)" \
#     docker compose -f deploy/compose/compose.yaml --profile sandbox build
#   sudo sh deploy/compose/isolate-sandbox.sh
#   CF_TOKEN=... CF_URL_BASE=https://... sh scripts/ci/slack-live-server.sh
#
# It brings up its own Compose project, agent-core-slack, ignores
# deploy/compose/.env, and on exit removes what it created: the sandboxes,
# the containers and the data directory, with every agent, linked account,
# configuration token and key agentd stored. agentd is published on
# 127.0.0.1:8443 and the networks' subnets and the sandbox bridge's name are
# fixed, so a development stack must be down first. It needs curl and jq.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
compose_file=$root/deploy/compose/compose.yaml
project=agent-core-slack
agentd_image=agent-core/agentd:dev
cloudflared_image=cloudflare/cloudflared:2026.9.3
tunnel=$project-cloudflared
wait_minutes=${WAIT_MINUTES:-60}
show_log=${SHOW_LOG:-true}

die() {
    echo "FAIL $*" >&2
    exit 1
}

pass() {
    echo "ok   $*"
}

[ -n "${CF_TOKEN:-}" ] || die "CF_TOKEN is not set"
url=${CF_URL_BASE:-}
url=${url%/}
case $url in
    https://?*) ;;
    *) die "CF_URL_BASE must be an https:// URL" ;;
esac
case ${url#https://} in
    */* | *'?'* | *'#'*) die "CF_URL_BASE must have no path, query or fragment" ;;
esac
case $wait_minutes in
    '' | *[!0-9]*) die "WAIT_MINUTES must be a whole number of minutes" ;;
esac
[ "$wait_minutes" -ge 1 ] && [ "$wait_minutes" -le 300 ] ||
    die "WAIT_MINUTES must be from 1 to 300"
if [ -z "${AGENTD_SLACK_MANAGER_SIGNING_SECRET:-}" ] ||
    [ -z "${AGENTD_SLACK_MANAGER_BOT_TOKEN:-}" ]; then
    echo "warn neither or only one of the Slack manager app's secrets is set;" \
        "agentd serves no Slack requests without both" >&2
    unset AGENTD_SLACK_MANAGER_SIGNING_SECRET AGENTD_SLACK_MANAGER_BOT_TOKEN
fi
admins=$(jq -cn --arg admins "${COMMUNITY_ADMINS:-}" \
    '[$admins | splits("[ ,]+") | select(length > 0)]')
printf '%s' "$admins" | jq -e 'all(test("^slack:[A-Z0-9]+:[A-Z0-9]+$"))' >/dev/null ||
    die "COMMUNITY_ADMINS must be slack:<team id>:<user id> identities"

tmp=$(mktemp -d)
log_pid=
cleanup() {
    status=$?
    if [ -n "$log_pid" ]; then
        kill "$log_pid" 2>/dev/null || true
    fi
    if [ "$status" -ne 0 ]; then
        echo "== agentd's log, the last 200 lines" >&2
        compose logs --no-color --tail 200 agentd >&2 || true
        echo "== cloudflared's log, the last 50 lines" >&2
        docker logs --tail 50 "$tunnel" >&2 2>&1 || true
    fi
    docker rm -f "$tunnel" >/dev/null 2>&1 || true
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

compose() {
    docker compose --env-file /dev/null -f "$compose_file" -f "$tmp/compose.slack.yaml" \
        -p "$project" "$@"
}

# poll SECONDS COMMAND...: runs COMMAND every five seconds until it
# succeeds, for at most SECONDS. Fails if it never does.
poll() {
    poll_deadline=$(($(date +%s) + $1))
    shift
    until "$@"; do
        if [ "$(date +%s)" -ge "$poll_deadline" ]; then
            return 1
        fi
        sleep 5
    done
}

running() {
    [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = true ]
}

echo "== agentd"

AGENT_CORE_DATA=$tmp/data
AGENT_CORE_CONFIG=$tmp/agentd.toml
AGENT_CORE_SANDBOX_NETWORK=$project-sandbox
AGENT_CORE_EGRESS_NETWORK=$project-egress
DOCKER_GID=$(stat -c %g /var/run/docker.sock)
AGENTD_MASTER_KEY=$(docker run --rm "$agentd_image" gen-key)
if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::add-mask::$AGENTD_MASTER_KEY"
fi
export AGENT_CORE_DATA AGENT_CORE_CONFIG AGENT_CORE_SANDBOX_NETWORK AGENT_CORE_EGRESS_NETWORK \
    DOCKER_GID AGENTD_MASTER_KEY
unset AGENTD_RC_MANAGER_TOKEN

cat >"$tmp/compose.slack.yaml" <<'EOF'
services:
  agentd:
    environment:
      - AGENTD_SLACK_MANAGER_SIGNING_SECRET
      - AGENTD_SLACK_MANAGER_BOT_TOKEN
EOF

awk '/^\[/ {
        skip = ($0 == "[rocketchat]" || $0 == "[slack]" || $0 == "[community]" ||
                $0 == "[sandbox]")
    }
    !skip' "$root/config/agentd.example.toml" |
    sed 's/^log_filter = "info"$/log_filter = "info,agentd::pipeline=debug"/' >"$AGENT_CORE_CONFIG"
grep -q '^log_filter = "info,agentd::pipeline=debug"$' "$AGENT_CORE_CONFIG" ||
    die "config/agentd.example.toml has no log_filter = \"info\" line to change"
cat >>"$AGENT_CORE_CONFIG" <<EOF

[slack]
public_url = "$url"
install_reminder_secs = 60

[community]
admins = $admins

[sandbox]
image = "agent-core/sandbox:dev"
network = "$AGENT_CORE_SANDBOX_NETWORK"
host_data_dir = "$AGENT_CORE_DATA"
instance = "$project"
EOF

compose up --detach --no-build --wait --wait-timeout 120 agentd || die "agentd didn't start"
healthy() {
    curl -fsS -o /dev/null --max-time 5 "$1/healthz"
}
poll 60 healthy http://127.0.0.1:8443 || die "agentd never answered /healthz"
pass "agentd answers /healthz"

echo "== The tunnel"

TUNNEL_TOKEN=$CF_TOKEN docker run --detach --name "$tunnel" --network host \
    --cap-drop ALL --security-opt no-new-privileges \
    --env TUNNEL_TOKEN "$cloudflared_image" tunnel --no-autoupdate run >/dev/null
poll 120 healthy "$url" ||
    die "$url/healthz never answered through the tunnel; its public hostname's service must be http://localhost:8443"
pass "agentd answers /healthz at CF_URL_BASE"

echo "== Up for $wait_minutes minutes"

if [ "$show_log" = true ]; then
    compose logs --follow --no-color --no-log-prefix agentd &
    log_pid=$!
fi
deadline=$(($(date +%s) + wait_minutes * 60))
echo "agentd is up until $(date -u -d "@$deadline" '+%H:%M UTC'). Everything it stored" \
    "goes with it then: delete the agents you made before that, or their Slack apps stay behind."
while [ "$(date +%s)" -lt "$deadline" ]; do
    running "$tunnel" || die "cloudflared stopped"
    sleep 5
done
pass "stayed up for $wait_minutes minutes"
