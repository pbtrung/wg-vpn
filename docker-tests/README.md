# Network simulation (M4)

Builds real `wg-server`/`wg-client` binaries into Docker images, runs the
2-master/2-spoke fixture from `docs/wg-server.md` §5/§7 against a local
MinIO (S3-compatible) backend, applies it on four real kernel WireGuard
interfaces, and asserts the hub-and-spoke-with-meshed-hubs connectivity
rules for real — including the one property that's easy to get backwards
and impossible to catch with mocks: a spoke never reaches another spoke
directly.

It also runs two chaos scenarios against real containers: killing
`wg-client` mid-sync and verifying backup recovery on the next pass, and
(`docs/milestones.md` §4's planned "one route-conflict preflight
rejection" PR-core case) injecting a real routing-table conflict and
verifying `wg-client`'s live preflight (`wg-client/src/preflight.rs`,
`docs/wg-client.md` §6) rejects the apply *before* tearing down the
working interface, leaving it running untouched.

## Requirements

- Docker with a running daemon.
- A host where a `--cap-add=NET_ADMIN` container can create a real
  WireGuard interface (verified for this environment; see
  `docs/milestones.md` §4 "Supported execution mode" — this needs the
  `wireguard` kernel module already loaded on the host, not inside the
  container).
- `docker-tests/Dockerfile.{server,client}` copy a **host-built** release
  binary rather than compiling inside Docker, and use an `archlinux`
  runtime image to match the host's glibc. If your host isn't
  Arch-compatible, switch the base image and drop the host-binary copy
  in favor of a proper multi-stage Rust build.

## Run it

```sh
./run.sh
```

Tears down the containers on exit. Set `KEEP=1` to leave them running for
manual inspection (`docker compose exec master-us wg show`, etc.) —
remember to `docker compose down -v` afterward.

## What it does not cover

- Credential scoping (all nodes here share one MinIO admin key) — that's
  a provider-IAM concern, tracked separately in `docs/milestones.md` §5.
- The gateway/NAT/idle-keepalive fixtures from `docs/milestones.md` §4 —
  this is the minimal two-master/two-spoke fixture only.
- Most of M5's chaos matrix: graceful stop/`SIGTERM` handling, storage
  partitions, killing `wg-server` mid-upload, and real transport races
  aren't exercised here — only the `wg-client` `SIGKILL`-mid-sync
  recovery and route-conflict preflight rejection scenarios are.
