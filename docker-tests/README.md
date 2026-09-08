# Network simulation (M4)

Builds real `wg-server`/`wg-client` binaries into Docker images, runs the
2-master/2-spoke fixture from `docs/wg-server.md` §5/§7 against a local
MinIO (S3-compatible) backend, applies it on four real kernel WireGuard
interfaces, and asserts the hub-and-spoke-with-meshed-hubs connectivity
rules for real — including the one property that's easy to get backwards
and impossible to catch with mocks: a spoke never reaches another spoke
directly.

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
- Chaos scenarios (M5) — process kills and storage partitions aren't
  exercised here.
