#!/bin/sh
# Keeps sandboxes on the Compose `sandbox` network from reaching each other.
#
# compose.yaml turns inter-container traffic off on that network
# (enable_icc), so Docker drops everything between its containers,
# sandbox to agentd included. This script adds the one exception, in
# Docker's DOCKER-USER chain, which Docker evaluates first and never
# rewrites: new TCP connections to agentd's sandbox address on ports 8080
# (the credential proxy) and 8081 (the agentctl API), and agentd's replies
# on them. Everything else between containers on the bridge is dropped
# here as well, so the isolation holds even where Docker's own inter-
# container rule is missing.
#
#   sudo sh deploy/compose/isolate-sandbox.sh          # add, or refresh
#   sudo sh deploy/compose/isolate-sandbox.sh remove   # take them out
#
# It needs root, the iptables Docker uses, and a running Docker daemon
# (which creates DOCKER-USER). Adding is idempotent: the rules live in a
# chain of their own, AGENT-CORE-SANDBOX, which is rebuilt each time and
# jumped to once from DOCKER-USER. The rules name the bridge, which
# compose.yaml fixes (com.docker.network.bridge.name), so they can be added
# before the network exists. They don't survive a reboot of the host; run
# this again after one, before starting sandboxes.
set -eu

bridge=br-agent-sbx
agentd=172.30.0.2
ports=8080,8081
chain=AGENT-CORE-SANDBOX

jump() {
    iptables "$1" DOCKER-USER -i "$bridge" -o "$bridge" -j "$chain"
}

remove() {
    while jump -C 2>/dev/null; do
        jump -D
    done
    if iptables -n -L "$chain" >/dev/null 2>&1; then
        iptables -F "$chain"
        iptables -X "$chain"
    fi
}

add() {
    if ! iptables -n -L DOCKER-USER >/dev/null 2>&1; then
        echo "iptables has no DOCKER-USER chain: is the Docker daemon running, with its iptables firewall backend?" >&2
        exit 1
    fi
    iptables -N "$chain" 2>/dev/null || iptables -F "$chain"
    iptables -A "$chain" -d "$agentd" -p tcp -m multiport --dports "$ports" \
        -m conntrack --ctstate NEW,ESTABLISHED -j ACCEPT
    iptables -A "$chain" -s "$agentd" -p tcp -m multiport --sports "$ports" \
        -m conntrack --ctstate ESTABLISHED -j ACCEPT
    iptables -A "$chain" -j DROP
    jump -C 2>/dev/null || jump -I
}

case ${1:-add} in
add)
    add
    ;;
remove)
    remove
    ;;
*)
    echo "usage: $0 [add | remove]" >&2
    exit 2
    ;;
esac
