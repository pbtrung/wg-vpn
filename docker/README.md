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
docker run --rm -e WG_SERVER_CONFIG=... -v ...:... wg-server --rotate workstation-01
```

Set `-e RUST_LOG=info` (or `-e RUST_LOG=debug`) for more verbose logs —
see the root [`README.md`](../README.md#use) for the logging flags this
binary also accepts directly (`-v`/`-vv`/`-vvv`), which work the same way
here as `docker run ... wg-server -v --dry-run`.

## Notes

- The image is `distroless` (no shell, no package manager) — if you need
  to debug interactively, use `docker run --rm -it --entrypoint sh
  <a non-distroless build>` instead, or add a debug stage.
- Nothing in this image persists between runs by design
  (`docs/wg-server.md` §8) — the storage bucket is the only durable
  state, so a fresh container on every scheduled invocation is correct,
  not just tolerated.
- `--rotate`/`--dry-run`/`--prune` are documented in the root
  [`README.md`](../README.md#use) and `docs/wg-server.md` §4; the normal
  scheduled invocation should pass none of them.
