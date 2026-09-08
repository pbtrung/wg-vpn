# `wg-server` as a cloud cron job

This image runs `wg-server apply --rotate -v` once and exits — no
in-container scheduler. Point your cloud platform's cron/scheduled-job
feature at it (GCP Cloud Run Jobs + Cloud Scheduler, AWS ECS Scheduled
Tasks / EventBridge Scheduler, a Kubernetes `CronJob`, ...); `docs/wg-server.md`
§1 assumes this general shape (no persistent filesystem between runs),
but see "Rotation on every run" below for how this image's schedule
differs from the docs' general recommendation.

## Rotation on every run

The `ENTRYPOINT` bakes in a bare `--rotate` (every node) and `-v`, so
**every scheduled invocation force-rotates every node's key and
immediately prunes down to current + previous** (`--rotate` implies
`--prune` — see `docs/wg-server.md` §4). This is a deliberate choice for
this packaged deployment, not the general guidance: `docs/wg-server.md`
§10–11 describes the *default-assumed* usage as a non-rotating
`apply` on a schedule, precisely because a rotation can interrupt a live
tunnel until each node's `wg-client` next syncs. Rotating on every
scheduled run means:

- Every node must run `wg-client sync` frequently enough relative to
  this image's own schedule that it reliably picks up each new
  generation before the *next* rotation supersedes it — an offline or
  infrequently-syncing node can permanently miss a generation once both
  `current` and `previous` have rotated past it (`docs/wg-server.md`
  §10's "Retention is independent of client schedules" caveat).
- There is no stable, long-lived key for any node; treat rotation as the
  normal condition rather than an exceptional operator action.

If you want the general (non-rotating, occasional manual rotation)
usage `docs/wg-server.md` describes instead, override the entrypoint:

```sh
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... \
  --entrypoint /usr/local/bin/wg-server \
  wg-server apply --config /etc/wg-server/config.json
```

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

The image's `ENTRYPOINT` is `wg-server apply --rotate -v`; anything you
pass as container args is appended to that, not a replacement for it:

```sh
# Plan the rotation without publishing or deleting anything.
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server --dry-run

# -v accumulates (Count-based), so this bakes in -vv (debug) for this
# run only -- or set -e RUST_LOG=debug instead, which takes priority
# over any number of -v's if both are given.
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server -v
```

Because a bare `--rotate` is already baked in, you **cannot** append
`--rotate <hostname>` to rotate only specific node(s) — that mixes a bare
and named `--rotate` in one invocation, which `wg-server` rejects. To
rotate only specific nodes (or to not rotate at all), override the
entrypoint as shown in "Rotation on every run" above.

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
  [`README.md`](../README.md#use) and `docs/wg-server.md` §4. This
  image's baked-in `--rotate -v` is specific to this packaged
  deployment — see "Rotation on every run" above.
