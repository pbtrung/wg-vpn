# `wg-server` as a cloud cron job

This image runs `wg-server apply` once and exits — no in-container
scheduler. Point your cloud platform's cron/scheduled-job feature at it
(GCP Cloud Run Jobs + Cloud Scheduler, AWS ECS Scheduled Tasks / EventBridge
Scheduler, a Kubernetes `CronJob`, ...); `docs/wg-server.md` §1 assumes
exactly this shape (`0 0 * * *` UTC, no persistent filesystem between
runs).

## Build

```sh
docker build -f docker/Dockerfile -t wg-server .
```

## Configure

The topology config path comes from the `WG_SERVER_CONFIG` environment
variable, not a baked-in path or CLI flag — set it to wherever your
platform mounts the config as a file. **The config file contains
read-write bucket credentials (`docs/wg-server.md` §12) — mount it from
your platform's secret manager (GCP Secret Manager, AWS Secrets Manager,
a Kubernetes `Secret` volume, ...), never bake it into the image or set
its *contents* as a plain environment variable.**

```sh
docker run --rm \
  -e WG_SERVER_CONFIG=/etc/wg-server/config.json \
  -v /path/to/your/config.json:/etc/wg-server/config.json:ro \
  wg-server
```

## Extra flags

The image's `ENTRYPOINT` is `wg-server apply`; anything you pass as
container args is appended, so a one-off dry run or rotation looks like:

```sh
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server --dry-run

# Rotate one node's key (repeatable), or every node with a bare --rotate.
# Either form always cleans up immediately (skips the usual ~15-minute
# retention grace period), since rotation is already a deliberate,
# manual action rather than the routine unattended cron invocation.
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server --rotate workstation-01
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server --rotate

# More verbose logs (-v info, -vv debug, -vvv trace) -- or set
# -e RUST_LOG=debug instead, which takes priority if both are given.
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server -v
```

Flags combine normally, e.g. `wg-server --rotate -v` for a verbose
full-fleet rotation (`--prune` is redundant here since `--rotate` already
implies it, but harmless to add).

## Notes

- The runtime image is `alpine:edge`, rebuilt from source in an
  `alpine:edge` builder stage too (matching musl libc — a binary built
  against glibc, e.g. from a Debian-based builder, will not run here).
  Both stages run `apk update && apk --no-cache upgrade` before
  installing anything, so rebuilding regularly picks up upstream
  security patches rather than pinning to whatever was current when the
  image was last built. `edge` (Alpine's rolling branch) rather than a
  numbered release so those upgrades actually have newer packages to
  pull from.
- Debug interactively with `docker run --rm -it --entrypoint sh
  wg-server` (Alpine's `sh` is available, unlike a distroless image).
- Nothing in this image persists between runs by design
  (`docs/wg-server.md` §8) — the storage bucket is the only durable
  state, so a fresh container on every scheduled invocation is correct,
  not just tolerated.
- `--rotate`/`--dry-run`/`--prune` are documented in the root
  [`README.md`](../README.md#use) and `docs/wg-server.md` §4; the normal
  scheduled invocation should pass none of them.
