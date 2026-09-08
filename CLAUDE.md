# CLAUDE.md

Context for working in this repo. Read the design docs before changing
behavior — this file is a map to them and a summary of decisions already
made, not a replacement.

## What this is

`wg-server` publishes per-node `wg-quick` configs for a hub-and-spoke
WireGuard fleet to an S3-compatible bucket; `wg-client` runs on each node
and applies its own config. Full specs: [`docs/wg-server.md`](docs/wg-server.md),
[`docs/wg-client.md`](docs/wg-client.md). Testing plan and milestone
breakdown (M0–M6, this repo's actual commit history maps to it 1:1):
[`docs/milestones.md`](docs/milestones.md).

**The docs are authoritative.** If code and docs disagree, that's a bug in
one of them — figure out which before changing either. When a design
decision changes, update the doc in the same change, not as a follow-up.

## Architecture

- `wg-common/` — everything both binaries share: the strict `wg-quick`
  parser/renderer (also self-validates `wg-server`'s own output before
  upload), X25519/PSK key handling, topology schema + validation, the
  peer-selection algorithm, the Crockford Base32 encoding for IDs/digests
  (**not** RFC 4648 — see `wg-common/src/base32.rs`'s doc comment before
  touching this), and the `current.json` discovery-pointer schema.
- `wg-server/` — `apply`/`validate`. Stateless: the bucket itself is the
  only durable state (immutable generations, a `current.json` pointer
  updated via conditional writes). No local state file, no derived
  secrets — see `docs/wg-server.md` §8 for why and how key
  reuse/rotation works without one.
- `wg-client/` — `sync`. A `SystemOps` trait abstracts file I/O and
  `wg-quick` invocation so the local transaction/rollback state machine
  is unit-testable without root or a real interface; `RealSystemOps`
  shells out to the actual `wg-quick`/`wg` binaries.
- `docker-tests/` — the one place that exercises real kernel WireGuard
  interfaces end to end. See its own README. This is where a real bug
  was actually found (a panicking `aws-sdk-s3` credentials call that
  every mocked unit test was blind to) — don't skip it when touching the
  storage or key-application paths.

## Testing pattern

Both `wg-server` and `wg-client` put a trait between their core logic and
the outside world (`Storage`/`ReadStorage` for S3, `SystemOps` for
files/`wg-quick`), with an in-memory mock implementation under
`#[cfg(test)]`. Add new logic tests against the mock; only exercise the
real AWS SDK client construction and real `wg-quick` behavior through
`docker-tests/`. This is deliberate (see `docs/milestones.md` §3's
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

## Known simplifications (not bugs, but don't assume more coverage than exists)

- `docker-tests/` uses one shared MinIO admin key for both "read-write"
  and "read-only" access — real per-node credential scoping is a
  provider-IAM concern (`docs/wg-server.md` §12) and isn't exercised by
  this harness.
- `wg-client`'s live route-conflict preflight (checking a new config's
  routes against the host's actual routing table before teardown) is not
  implemented — only the structural/semantic validation in
  `wg-common`'s parser runs before applying.
- `r2_config.endpoint` HTTPS-only enforcement described in the docs isn't
  implemented in code (`docker-tests/` deliberately uses plain HTTP to
  talk to local MinIO).
- M6's fuzzing is a dependency-free PRNG mutation stress test
  (`wg-common/tests/fuzz_like.rs`), not real `cargo-fuzz` — this
  environment has no `rustup`/nightly toolchain. Swap in real fuzz
  targets if one becomes available; the seed corpus carries over.
- No CI workflow files exist yet (no `.github/workflows/`) — the commands
  above are what CI should eventually run, per `docs/milestones.md` §8.
