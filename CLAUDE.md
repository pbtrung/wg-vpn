# CLAUDE.md

Context for working in this repo. Read the design docs before changing
behavior — this file is a map to them and a summary of decisions already
made, not a replacement.

## What this is

`wg-server` publishes per-node WireGuard configs (rendered in
`wg-quick`-compatible INI syntax, used only as a storage/interchange
format) for a hub-and-spoke fleet to an S3-compatible bucket; `wg-client`
runs on each node and applies its own config directly to the kernel over
netlink/UAPI — the way [innernet](https://github.com/tonarino/innernet)
does — rather than shelling out to `wg`/`wg-quick`. Full specs: [`docs/wg-server.md`](docs/wg-server.md),
[`docs/wg-client.md`](docs/wg-client.md). Testing plan and milestone
breakdown (M0–M6, this repo's actual commit history maps to it 1:1):
[`docs/milestones.md`](docs/milestones.md).

**The docs are authoritative.** If code and docs disagree, that's a bug in
one of them — figure out which before changing either. When a design
decision changes, update the doc in the same change, not as a follow-up.

## Architecture

- `wg-common/` — everything both binaries share: the strict
  `wg-quick`-format parser/renderer (a storage/interchange format only —
  see `docs/wg-server.md` §9; also self-validates `wg-server`'s own output
  before upload), X25519/PSK key handling, topology schema + validation,
  the peer-selection algorithm, the Crockford Base32 encoding for
  IDs/digests (**not** RFC 4648 — see `wg-common/src/base32.rs`'s doc
  comment before touching this), and the `current.json` discovery-pointer
  schema.
- `wg-server/` — `apply`/`validate`. Stateless: the bucket itself is the
  only durable state (immutable generations, a `current.json` pointer
  updated via conditional writes). No local state file, no derived
  secrets — see `docs/wg-server.md` §8 for why and how key
  reuse/rotation works without one.
- `wg-client/` — `sync`. A `SystemOps` trait abstracts file I/O and
  WireGuard interface configuration so the local transaction/rollback
  state machine is unit-testable without root or a real interface;
  `RealSystemOps` configures the kernel device directly via the
  `wireguard-control` crate's netlink/UAPI calls — the way
  [innernet](https://github.com/tonarino/innernet) does — instead of
  shelling out to `wg`/`wg-quick` (see `docs/wg-client.md` §8 for the
  resulting `CAP_NET_ADMIN`-vs-root privilege story).
- `docker-tests/` — the one place that exercises real kernel WireGuard
  interfaces end to end. See its own README. This is where a real bug
  was actually found (a panicking `aws-sdk-s3` credentials call that
  every mocked unit test was blind to) — don't skip it when touching the
  storage or key-application paths.
- `docker/` — a *different* thing from `docker-tests/`: the production
  image for running `wg-server apply` as a scheduled cloud cron job
  (`alpine:edge` for both build and runtime stages — musl libc, so don't
  copy in a glibc-built binary; config path from `WG_SERVER_CONFIG`).
  Don't confuse the two directories.
- `package/` — Arch Linux packaging: `PKGBUILD` installs both binaries
  plus `systemd` service/timer units (`wg-server` at 00:00 UTC,
  `wg-client` at 00:30 UTC). Like `docker/`, `wg-server.service` bakes
  in `--rotate -v`, rotating the whole fleet on every scheduled run — an
  intentional deviation from the docs' general "routine jobs don't
  rotate" guidance (see `docker/README.md`'s "Rotation on every run" for
  the tradeoffs this accepts). `Dockerfile.build` cross-builds both
  architectures (`x86_64`, `aarch64`) from one amd64 host via Arch's
  official `aarch64-linux-gnu-gcc` cross toolchain — no buildx/QEMU
  emulation, no OpenSSL to cross-build (rustls). `release.sh` builds,
  uploads GitHub release assets via `gh`, and rewrites `PKGBUILD`'s
  `pkgver`/`sha256sums` to match.

## Testing pattern

Both `wg-server` and `wg-client` put a trait between their core logic and
the outside world (`Storage`/`ReadStorage` for S3, `SystemOps` for
files/interface configuration), with an in-memory mock implementation
under `#[cfg(test)]`. Add new logic tests against the mock; only exercise
the real AWS SDK client construction and real kernel netlink/WireGuard
behavior through `docker-tests/`. This is deliberate (see
`docs/milestones.md` §3's
"explicit host-operation test adapter") — don't reach for `docker-tests/`
to test something the mock can express, but don't assume the mock alone
proves a change works, either — it doesn't touch the real SDK config
path or a real interface.

## Commands

```sh
cargo build --release
cargo test --workspace                                  # <5s, no network/root needed
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
cargo test --release -p wg-server -- --ignored           # scale benchmark, slow, informational only
cd docker-tests && ./run.sh                              # real kernel WireGuard end-to-end, needs Docker
```

`/commit` (`.claude/commands/commit.md`) runs fmt/clippy/test on touched
Rust files before committing, writes a detailed message, and pushes. Use
it rather than committing by hand.

## Decisions worth knowing before you change something

- **No local state file anywhere.** `wg-server` derives everything from
  the bucket's current contents each run; `wg-client` persists only a
  small per-node state file (`conf_path + ".state.json"`) tracking the
  last applied digest and a pending-transaction flag. Don't add a new
  local cache/state file without a strong reason — it was deliberately
  designed out (`docs/wg-server.md` §8).
- **Generations are immutable and content-addressed.** A node's file
  lives at `nodes/{hostname}/{generation-id}.conf`; `current.json` points
  at the current and (at most one) previous generation by ID. Never
  overwrite a generation file in place.
- **All key material is either reused byte-for-byte from what's
  currently published, or freshly random.** There is no deterministic
  derivation (an earlier design considered HKDF-from-a-root-secret and
  it was deliberately dropped — don't reintroduce it without checking
  why it was rejected in the docs' history).
- **Rotation (`--rotate`) forces a fresh key** for the named node(s) (or
  every node, bare) and, as a consequence of the reuse rule above,
  forces fresh preshared keys on every edge touching a rotated node.
- **Retention keeps current + previous, plus a grace period** before
  deleting anything else, as defense-in-depth under the assumption that
  the scheduler serializes `wg-server` runs (it can't fully prevent a
  rare concurrent-cleanup race, and the docs say so explicitly rather
  than papering over it).
- **wg-client never tears down a working tunnel before the replacement
  is downloaded, digest-verified, and parsed.** The pending-transaction
  flag is set before teardown and only cleared after the new config is
  confirmed up (or a rollback to the backup succeeds) — see
  `wg-client/src/sync.rs`.
- **Boot/restart recovery checks the kernel interface first, before any
  network call, on every sync pass — not just when a transaction was left
  pending.** If the interface is missing (typically: first pass after a
  reboot) and `conf_path` still parses, `wg-client` reapplies it directly
  from that local file, the same way
  [innernet](https://github.com/tonarino/innernet)'s daemon re-checks and
  restores interface state from local config on every loop iteration
  before contacting its server. This is why the boot-recovery trigger
  exists at all, and why enabling a separate `wg-quick@<iface>` unit for
  the same interface is wrong, not just redundant (`docs/wg-client.md`
  §6–§7).
- **The boot-recovery trigger (`OnBootSec=5s`) lives in its own
  `wg-client-boot.timer`, separate from `wg-client.timer`'s daily
  `OnCalendar` slot.** `RandomizedDelaySec` (used on `wg-client.timer` to
  spread a fleet's daily bucket hits) applies to *every* trigger in a
  `[Timer]` section, `OnBootSec` included — combining them would jitter
  boot recovery by the same delay, silently defeating "shortly after
  boot." Don't merge these two units back into one without re-solving
  that (`docs/wg-client.md` §7, `package/wg-client-boot.timer`).

## Known simplifications (not bugs, but don't assume more coverage than exists)

- `docker-tests/` uses one shared MinIO admin key for both "read-write"
  and "read-only" access — real per-node credential scoping is a
  provider-IAM concern (`docs/wg-server.md` §12) and isn't exercised by
  this harness.
- `wg-client`'s live route-conflict preflight is implemented
  (`wg-client/src/preflight.rs`, wired into `sync::run_once` via
  `SystemOps::preflight`, called before any backup/pending-state/teardown
  mutation once a pass has decided to apply): it dumps the host's IPv4
  routing table over netlink (`wg-client/src/netlink.rs::list_ipv4_routes`)
  and rejects a candidate whose peer routes overlap another interface's
  existing non-default route, or would capture the storage endpoint,
  this tunnel's configured DNS, or a peer's transport endpoint (all
  resolved via DNS with the bounded 20s budget docs/wg-client.md §6
  specifies). Confirmed against real kernel interfaces by
  `docker-tests/`, which exercises this path on every sync and also runs
  a dedicated rejection scenario (docs/milestones.md §4's planned "one
  route-conflict preflight rejection" PR-core case): injects a real
  conflicting route and verifies `wg-client` rejects the apply without
  ever tearing down the working interface. Still not implemented:
  DNS/`resolvconf` integration itself (see below) — the preflight only
  protects the *configured* DNS resolver address, it doesn't apply one.
- `r2_config.endpoint`/credentials get real validation now
  (`wg_common::topology::validate_r2_endpoint`/`validate_r2_credentials`,
  called from both `wg-server`'s `topology::validate()` and `wg-client`'s
  `config::parse()`): non-`http`/`https` scheme, URL userinfo, query
  strings, fragments, and empty credential fields are all rejected. Full
  HTTPS-only enforcement (rejecting plain `http://` specifically) is
  still deliberately not implemented, since `docker-tests/` talks to
  local MinIO over plain HTTP — closing that needs a TLS-enabled test
  harness, not just a stricter check.
- `wg-client/src/system.rs`'s `RealSystemOps` and `wg-client/src/sync.rs`
  are migrated to the netlink/`wireguard-control` design described above
  and in `docs/wg-client.md` (no `wg`/`wg-quick`/`ip` subprocess; the
  interface-missing check runs unconditionally at the start of every pass,
  not only inside `state.pending`), and `docker-tests/` has confirmed this
  against real kernel interfaces: device creation, full peer replacement
  on rotation, address/route installation, hub-and-spoke connectivity,
  and the local-first restore firing correctly during the SIGKILL/recovery
  chaos scenario. Not yet covered by that harness: running as a truly
  unprivileged `CAP_NET_ADMIN`-only user (containers there still run as
  root), and DNS/`resolvconf` integration remains unimplemented
  regardless (see the bullets above/below) — route-conflict preflight is
  covered separately above.
  `wireguard-control`/`netlink-request` are git-pinned to a specific
  innernet commit rather than a crates.io release — see the comment in
  `wg-client/Cargo.toml` for why, and re-pin by hand if innernet cuts a
  matching release or moves its `main` branch further.
- M6's fuzzing is a dependency-free PRNG mutation stress test
  (`wg-common/tests/fuzz_like.rs`), not real `cargo-fuzz` — this
  environment has no `rustup`/nightly toolchain. Swap in real fuzz
  targets if one becomes available; the seed corpus carries over.
- No CI workflow files exist yet (no `.github/workflows/`) — the commands
  above are what CI should eventually run, per `docs/milestones.md` §8.
