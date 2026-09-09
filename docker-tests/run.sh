#!/usr/bin/env bash
# M4 network simulation: builds real wg-server/wg-client binaries into
# Docker images, publishes a topology through a local MinIO (S3-compatible)
# backend, applies it on 4 real kernel WireGuard interfaces (2 masters + 2
# spokes matching the wg-server.md worked example), and asserts the
# hub-and-spoke-with-meshed-hubs connectivity rules for real:
#   - every master reaches every other node and every other master
#   - a spoke never reaches another spoke directly
#
# Also runs two chaos scenarios: SIGKILL-mid-sync recovery, and (M4's
# planned "one route-conflict preflight rejection" PR-core case) a real
# routing-table conflict that wg-client's live preflight must reject
# before tearing down the working interface.
#
# Requires: docker with a running daemon, and a host that can create real
# WireGuard interfaces inside a --cap-add=NET_ADMIN container (verified in
# docs/milestones.md M4's "Supported execution mode").
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

REPO_ROOT="$(cd .. && pwd)"
FAILED=0
KEEP=${KEEP:-0}

log() { echo "[docker-tests] $*"; }
pass() { echo "  PASS: $*"; }
fail() { echo "  FAIL: $*"; FAILED=1; }

cleanup() {
    if [ "$KEEP" = "1" ]; then
        log "KEEP=1 set, leaving containers running for inspection (docker compose down -v to clean up)"
        return
    fi
    log "tearing down"
    docker compose down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

log "building release binaries on host"
(cd "$REPO_ROOT" && cargo build --workspace --release)

log "building docker images"
docker compose build --quiet

log "starting storage backend and publishing the topology"
docker compose up -d minio
docker compose up bucket-init
docker compose up server

server_exit=$(docker compose ps -a server --format '{{.ExitCode}}' 2>/dev/null || echo 1)
if [ "$server_exit" != "0" ]; then
    fail "wg-server apply did not exit 0 (exit=$server_exit); see: docker compose logs server"
    exit 1
fi
pass "wg-server apply published a generation"

log "starting node containers"
docker compose up -d master-us master-eu workstation-01 mobile-01
sleep 2

sync_node() {
    local svc="$1"
    log "wg-client sync on $svc"
    docker compose exec -T "$svc" wg-client sync --config /etc/wg-client/config.json --once --hostname "$svc"
}
for svc in master-us master-eu workstation-01 mobile-01; do
    sync_node "$svc"
done

ping_ok() {
    docker compose exec -T "$1" ping -c 3 -W 2 -i 0.3 "$2" >/dev/null 2>&1
}

# Warm-up: spokes have no configured Endpoint, so they must send the first
# packet before a master can reach them back (WireGuard roaming).
log "warm-up: spokes initiate to masters first"
docker compose exec -T workstation-01 ping -c 3 -W 2 10.10.0.1 >/dev/null 2>&1 || true
docker compose exec -T workstation-01 ping -c 3 -W 2 10.10.0.2 >/dev/null 2>&1 || true
docker compose exec -T mobile-01 ping -c 3 -W 2 10.10.0.1 >/dev/null 2>&1 || true
docker compose exec -T mobile-01 ping -c 3 -W 2 10.10.0.2 >/dev/null 2>&1 || true
sleep 1

log "assertions"
ping_ok master-us 10.10.0.2 && pass "master-us -> master-eu" || fail "master-us -> master-eu"
ping_ok master-eu 10.10.0.1 && pass "master-eu -> master-us" || fail "master-eu -> master-us"
ping_ok master-us 10.10.0.100 && pass "master-us -> workstation-01" || fail "master-us -> workstation-01"
ping_ok master-us 10.10.0.101 && pass "master-us -> mobile-01" || fail "master-us -> mobile-01"
ping_ok master-eu 10.10.0.100 && pass "master-eu -> workstation-01" || fail "master-eu -> workstation-01"
ping_ok master-eu 10.10.0.101 && pass "master-eu -> mobile-01" || fail "master-eu -> mobile-01"
ping_ok workstation-01 10.10.0.1 && pass "workstation-01 -> master-us" || fail "workstation-01 -> master-us"
ping_ok workstation-01 10.10.0.2 && pass "workstation-01 -> master-eu" || fail "workstation-01 -> master-eu"
ping_ok mobile-01 10.10.0.1 && pass "mobile-01 -> master-us" || fail "mobile-01 -> master-us"
ping_ok mobile-01 10.10.0.2 && pass "mobile-01 -> master-eu" || fail "mobile-01 -> master-eu"

# The critical negative case: no direct spoke-spoke edge exists.
if ping_ok workstation-01 10.10.0.101; then
    fail "workstation-01 -> mobile-01 succeeded (should be unreachable: no spoke-spoke edge)"
else
    pass "workstation-01 -> mobile-01 correctly unreachable"
fi
if ping_ok mobile-01 10.10.0.100; then
    fail "mobile-01 -> workstation-01 succeeded (should be unreachable: no spoke-spoke edge)"
else
    pass "mobile-01 -> workstation-01 correctly unreachable"
fi

# M5 chaos scenario: publish a real config change, then SIGKILL
# wg-client partway through applying it on a real container, and verify
# the node recovers to a working state on the next scheduled pass rather
# than being left with a broken or half-installed tunnel.
log "chaos: publishing a config change (rotate master-us)"
docker compose run --rm server apply --config /etc/wg-server/topology.json --rotate master-us >/dev/null

log "chaos: killing wg-client on workstation-01 mid-sync"
docker compose exec -T workstation-01 sh -c \
    'wg-client sync --config /etc/wg-client/config.json --once --hostname workstation-01 & pid=$!; sleep 0.05; kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; true'

log "chaos: recovery pass"
if docker compose exec -T workstation-01 wg-client sync --config /etc/wg-client/config.json --once --hostname workstation-01; then
    pass "recovery pass completed without error"
else
    fail "recovery pass reported an error (see logs above)"
fi
# master-us's own interface also needs the rotated key it just published;
# only workstation-01's crash/recovery was under test above.
sync_node master-us

docker compose exec -T workstation-01 ping -c 3 -W 2 10.10.0.1 >/dev/null 2>&1 || true
sleep 1
if ping_ok master-us 10.10.0.100; then
    pass "tunnel still works after the killed transaction and recovery"
else
    fail "tunnel broken after the killed transaction and recovery"
fi

# Planned M4 "PR core" scenario (docs/milestones.md §4): one
# route-conflict preflight rejection. Replace the route for one of
# mobile-01's peer AllowedIPs (master-us's tunnel address, 10.10.0.1/32)
# with one via eth0 -- a real interface distinct from wg0 -- so the live
# routing-table preflight (wg-client.md §6, wg-client/src/preflight.rs)
# must reject the next apply before tearing down the working tunnel.
# (`replace`, not `add`: wg0 already owns this destination from the
# initial sync, and a plain `add` would just fail with "File exists".)
log "chaos: injecting a conflicting route to trigger the route-conflict preflight"
docker compose exec -T mobile-01 ip route replace 10.10.0.1/32 dev eth0
wg0_ifindex_before=$(docker compose exec -T mobile-01 cat /sys/class/net/wg0/ifindex)

reject_output=$(docker compose exec -T mobile-01 wg-client sync --config /etc/wg-client/config.json --once --hostname mobile-01 --force 2>&1) && reject_exit=0 || reject_exit=$?
if [ "$reject_exit" != "0" ] && echo "$reject_output" | grep -q "route preflight failed"; then
    pass "forced apply was rejected by the route-conflict preflight"
else
    echo "$reject_output"
    fail "expected a route-conflict preflight rejection (exit=$reject_exit)"
fi

# A torn-down-and-recreated wg0 would get a new kernel ifindex; the
# preflight must reject before touching the interface at all, so this
# must be unchanged (wg-client.md §6: "Keep the current configuration on
# any preflight error").
wg0_ifindex_after=$(docker compose exec -T mobile-01 cat /sys/class/net/wg0/ifindex)
if [ "$wg0_ifindex_after" = "$wg0_ifindex_before" ]; then
    pass "wg0 was never torn down by the rejected apply (preflight kept the existing configuration untouched)"
else
    fail "wg0 was recreated despite the preflight rejection (ifindex changed: $wg0_ifindex_before -> $wg0_ifindex_after)"
fi

log "chaos: removing the injected conflicting route"
docker compose exec -T mobile-01 ip route del 10.10.0.1/32 dev eth0

if docker compose exec -T mobile-01 wg-client sync --config /etc/wg-client/config.json --once --hostname mobile-01 --force; then
    pass "apply succeeds once the conflicting route is removed"
else
    fail "apply still failing after removing the conflicting route"
fi

docker compose exec -T mobile-01 ping -c 3 -W 2 10.10.0.1 >/dev/null 2>&1 || true
sleep 1
if ping_ok mobile-01 10.10.0.1; then
    pass "tunnel to master-us reachable again once the conflicting route is removed and reapplied"
else
    fail "tunnel to master-us still unreachable after removing the conflicting route and reapplying"
fi

if [ "$FAILED" = "0" ]; then
    log "ALL ASSERTIONS PASSED"
else
    log "SOME ASSERTIONS FAILED"
fi
exit "$FAILED"
