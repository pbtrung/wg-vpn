#!/usr/bin/env bash
# M4 network simulation: builds real wg-server/wg-client binaries into
# Docker images, publishes a topology through a local MinIO (S3-compatible)
# backend, applies it on 4 real kernel WireGuard interfaces (2 masters + 2
# spokes matching the wg-server.md worked example), and asserts the
# hub-and-spoke-with-meshed-hubs connectivity rules for real:
#   - every master reaches every other node and every other master
#   - a spoke never reaches another spoke directly
#
# Requires: docker with a running daemon, and a host that can create real
# WireGuard interfaces inside a --cap-add=NET_ADMIN container (verified in
# docs/milestones.md M4's "Supported execution mode").
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

REPO_ROOT="$(cd .. && pwd)"
FAILED=0
KEEP=${KEEP:-0}

log() { echo "[network-sim] $*"; }
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

if [ "$FAILED" = "0" ]; then
    log "ALL ASSERTIONS PASSED"
else
    log "SOME ASSERTIONS FAILED"
fi
exit "$FAILED"
