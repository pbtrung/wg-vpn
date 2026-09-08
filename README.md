# wg-vpn

A small hub-and-spoke WireGuard fleet manager: `wg-server` generates keys
and publishes per-node `wg-quick` configs to an S3-compatible bucket
(Cloudflare R2 in the reference deployment); `wg-client` runs on every
node, downloads its own config, and brings the tunnel up.

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
└── docker-tests/   # Docker Compose harness with real kernel WireGuard
                    # interfaces (see docker-tests/README.md)
```

## Requirements

- Rust (edition 2024; a recent stable toolchain — this was built and
  tested against 1.98).
- `wg-client` shells out to the real `wg-quick`/`wg`/`ip` tools at
  runtime and needs root (or `CAP_NET_ADMIN` plus the rest of
  `wg-quick`'s prerequisites) to manage an interface.
- An S3-compatible bucket (Cloudflare R2, or anything else that supports
  conditional writes — see [`docs/wg-server.md` §10](docs/wg-server.md)).
- Docker, only for `docker-tests/` (an end-to-end harness against real
  WireGuard interfaces, not required to build or unit-test the project).

### Arch Linux packages

`wg-server` only needs a Rust toolchain to build and run. `wg-client`
additionally needs the real WireGuard/networking tools on any host it
manages an interface on:

```sh
# Build toolchain (either binary):
sudo pacman -S rust

# Runtime, wg-client hosts only:
sudo pacman -S wireguard-tools iproute2 iputils bash

# Only for docker-tests/:
sudo pacman -S docker docker-compose
```

`wireguard-tools` provides `wg`/`wg-quick`; `iproute2` provides `ip`;
`iputils` provides `ping` (used by `docker-tests/`, not by the binaries
themselves); `bash` is required because `wg-quick` itself is a Bash
script. The `wireguard` kernel module ships in the mainline Linux kernel
on Arch, so no separate DKMS package is needed — just confirm it loads
(`sudo modprobe wireguard`).

## Build

```sh
cargo build --release
# binaries land in target/release/wg-server and target/release/wg-client
```

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
# every node.
wg-server apply --config topology.json --rotate workstation-01

# Cleanup of old generations always runs automatically (keeping only the
# current and one previous generation), but waits out a 15-minute grace
# period by default. --prune forces it immediately -- useful when you're
# testing/iterating and don't want to wait, or know no other apply is
# running concurrently.
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

MIT — see [`LICENSE`](LICENSE).
