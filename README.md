# wg-vpn

A small hub-and-spoke WireGuard fleet manager: `wg-server` generates keys
and publishes per-node configs (in `wg-quick`-compatible INI syntax, used
only as a storage format) to an S3-compatible bucket (Cloudflare R2 in
the reference deployment); `wg-client` runs on every node, downloads its
own config, and brings the tunnel up by configuring the kernel interface
directly over netlink/UAPI — the way [innernet](https://github.com/tonarino/innernet)
does — rather than shelling out to `wg`/`wg-quick`.

Topology rule: every master reaches every node and every other master;
non-master nodes only reach masters, never each other.

Full design docs live in [`docs/`](docs/) — start with
[`docs/wg-server.md`](docs/wg-server.md) and
[`docs/wg-client.md`](docs/wg-client.md); [`docs/milestones.md`](docs/milestones.md)
has the testing plan and maps to this repo's commit history.

## Layout

```
wg-vpn/
├── wg-common/      # shared: parser/renderer, key handling, topology
│                   # validation, Base32 encoding, discovery schema
├── wg-server/      # control-plane binary: generate/rotate keys, publish
├── wg-client/      # per-node binary: download and apply the config
├── docker/         # production image for running wg-server as a cloud
│                   # cron job (see docker/README.md)
├── docker-tests/   # Docker Compose harness with real kernel WireGuard
│                   # interfaces (see docker-tests/README.md)
└── package/        # Arch Linux packaging: PKGBUILD, systemd units,
                    # and the cross-build/release tooling behind it
                    # (see "Install via package" below)
```

## Requirements

- Rust (edition 2024; a recent stable toolchain — this was built and
  tested against 1.98).
- `wg-client` configures the kernel WireGuard interface itself over
  netlink/UAPI and needs `CAP_NET_ADMIN` to do so (root is the simplest
  way to get that; see [`docs/wg-client.md` §8](docs/wg-client.md)). No
  `wg`/`wg-quick`/`ip` binary or Bash is required at runtime.
- An S3-compatible bucket (Cloudflare R2, or anything else that supports
  conditional writes — see [`docs/wg-server.md` §10](docs/wg-server.md)).
- Docker, only for `docker-tests/` (an end-to-end harness against real
  WireGuard interfaces, not required to build or unit-test the project).

### Arch Linux packages

Both binaries only need a Rust toolchain to build and run — `wg-client`
included, since it needs no external WireGuard/networking tools at
runtime:

```sh
# Build toolchain (either binary):
sudo pacman -S rust

# Optional, wg-client hosts only, for manual inspection:
sudo pacman -S wireguard-tools iproute2

# Only for docker-tests/:
sudo pacman -S docker docker-compose
```

`wireguard-tools` provides `wg show`; `iproute2` provides `ip addr`/`ip
route` — both purely for an operator inspecting the interface by hand
(`package/PKGBUILD`'s `optdepends`), not required by `wg-client` itself.
The `wireguard` kernel module ships in the mainline Linux kernel on Arch,
so no separate DKMS package is needed — just confirm it loads
(`sudo modprobe wireguard`).

## Build

```sh
cargo build --release
# binaries land in target/release/wg-server and target/release/wg-client
```

## Install via package (Arch Linux)

Prebuilt `x86_64`/`aarch64` binaries plus `systemd` service/timer units
are also available as an Arch package instead of building from source:

```sh
cd package
makepkg
```

This installs `wg-server`/`wg-client` to `/usr/bin`, and
`wg-server.{service,timer}`/`wg-client.{service,timer,-boot.timer}` to
run them on the daily schedule `docs/wg-server.md` assumes (`wg-server`
at 00:00 UTC, `wg-client` at 00:30 UTC). `wg-server.service` rotates
every node's key on every run — see `docker/README.md`'s "Rotation on
every run" for what that means operationally; edit the unit's `ExecStart`
if you want the non-rotating schedule the docs describe instead. After
installing, drop your config into `/etc/wg-server/config.json` and/or
`/etc/wg-client/config.json` (see Configure below) and enable the
relevant timer(s):

```sh
sudo systemctl enable --now wg-server.timer        # server hosts
sudo systemctl enable --now wg-client.timer        # every node
sudo systemctl enable --now wg-client-boot.timer   # every node, see below
```

Do **not** also enable `wg-quick@wg0` (or any other tunnel controller)
for the same interface — `wg-client` configures the kernel WireGuard
device itself over netlink/UAPI, so a second controller managing the
same interface would race it (`docs/wg-client.md` §3, §8).

Enable both client timers, not just `wg-client.timer`: that one is the
daily 00:30 UTC slot with a `RandomizedDelaySec` (see
[`package/wg-client.timer`](package/wg-client.timer) for the current
value) so a fleet doesn't hit the bucket in the same second;
`wg-client-boot.timer` runs a sync pass shortly after boot instead,
deliberately with **no** randomized delay. They're separate units because
`RandomizedDelaySec` applies to every trigger in a `[Timer]` section — if
`OnBootSec` were combined into `wg-client.timer`, that same fleet-spreading
delay would also jitter boot recovery by just as much. The boot pass does
two things, in order:
first it checks the interface directly against the kernel and, if
missing/down, restores it from the last-good local config with no
network involved at all — so the tunnel comes back immediately even if
storage happens to be unreachable at that instant. It then still runs
the normal remote discovery/fetch (`current.json`, this node's config,
digest verify) like any other pass, and applies a newer generation on
top if one is published (`wg-client/src/sync.rs`, `docs/wg-client.md`
§6, §7).

See [`package/PKGBUILD`](package/PKGBUILD) for the package itself,
[`package/Dockerfile.build`](package/Dockerfile.build) for how the
release binaries are cross-built for both architectures from a single
host, and [`package/release.sh`](package/release.sh) for the release
process that ties the two together.

## Configure

`wg-server` reads one JSON topology file (read-write bucket credentials,
every node's `wg_config`, and the `master` list) and `wg-client` reads one
small JSON file per node (read-only bucket credentials plus `conf_path`).
Full schemas, validation rules, and worked examples are in
[`docs/wg-server.md` §5](docs/wg-server.md) and
[`docs/wg-client.md` §4](docs/wg-client.md). A minimal topology looks
like:

```json
{
  "r2_config": {
    "endpoint": "https://<accountid>.r2.cloudflarestorage.com",
    "read_write_access_key_id": "...",
    "read_write_secret_access_key": "...",
    "region": "auto",
    "bucket": "wg-confs"
  },
  "nodes": [
    {
      "hostname": "master-us",
      "wg_config": {
        "tunnel_address": "10.10.0.1/32",
        "endpoint": "203.0.113.10:51820"
      }
    },
    {
      "hostname": "workstation-01",
      "wg_config": {
        "tunnel_address": "10.10.0.100/32"
      }
    }
  ],
  "master": ["master-us"]
}
```

Each node then gets its own small client config, e.g. for `workstation-01`:

```json
{
  "r2_config": {
    "endpoint": "https://<accountid>.r2.cloudflarestorage.com",
    "read_only_access_key_id": "...",
    "read_only_secret_access_key": "...",
    "region": "auto",
    "bucket": "wg-confs"
  },
  "conf_path": "/etc/wireguard/wg0.conf"
}
```

The node's own hostname (`workstation-01` here) is resolved from
`--hostname`, the `WG_CLIENT_HOSTNAME` environment variable, or the
system hostname, in that order — it isn't part of this file, so the same
config can be reused across hosts (see
[`docs/wg-client.md` §5](docs/wg-client.md)).

## Use

```sh
# Validate a topology file with no key generation and no network calls.
wg-server validate --config topology.json

# Generate/reuse keys, render configs, and publish a new generation if
# anything changed. Add --dry-run to see the plan without writing.
wg-server apply --config topology.json

# Force a fresh key for one node (repeatable), or --rotate alone for
# every node. --rotate always cleans up immediately (see below) since
# it's already a deliberate, manual action -- never part of the routine
# unattended cron invocation.
wg-server apply --config topology.json --rotate workstation-01

# Cleanup of old generations always runs automatically (keeping only the
# current and one previous generation), but waits out a 15-minute grace
# period by default. --prune forces it immediately -- useful when you're
# testing/iterating a plain (non-rotating) apply and don't want to wait,
# or know no other apply is running concurrently.
wg-server apply --config topology.json --prune

# --config can be omitted if WG_SERVER_CONFIG is set instead -- this is
# what lets the same binary run unmodified as a cloud cron job (see
# docker/README.md).
WG_SERVER_CONFIG=topology.json wg-server apply

# On each node: download and apply this node's current published config.
sudo wg-client sync --config client.json --once

# Or run as a daemon that re-syncs on an interval.
sudo wg-client sync --config client.json --daemon --interval 24h
```

Both binaries default to warn-level logging; add `-v`/`-vv`/`-vvv` (info/
debug/trace) anywhere on the command line, or set `RUST_LOG` for full
control (it takes priority over `-v` when set).

Exact flag reference, the local transaction/rollback model, and locking
behavior are in [`docs/wg-client.md`](docs/wg-client.md).

## Test

```sh
cargo test --workspace          # unit + property/mutation tests, <5s
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all

# Scale benchmark (informational, not a per-commit gate — see
# docs/milestones.md §6):
cargo test --release -p wg-server -- --ignored

# End-to-end against real kernel WireGuard interfaces in Docker:
cd docker-tests && ./run.sh
```

## License

MIT — see [`LICENSE`](LICENSE). The compiled `wg-client` binary also
links an LGPL-2.1-or-later component; see [`NOTICE.md`](NOTICE.md).
