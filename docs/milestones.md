# Testing Plan & Milestones

Status: design only, no implementation yet. Companion to
[wg-server.md](./wg-server.md) and [wg-client.md](./wg-client.md).

## 1. Testing philosophy

Four layers, cheapest/fastest first. A change should be caught at the
lowest layer that can catch it — the Docker network simulation exists for
the handful of properties that genuinely require a real kernel/userspace
WireGuard data plane, not as the default place to test parsing or
validation logic.

1. **Unit** — pure functions, no filesystem/network/subprocess. Runs on
   every commit, seconds.
2. **Integration** — `wg-server`/`wg-client` against a real S3-compatible
   backend (§3), still no WireGuard interfaces. Runs on every PR, low
   minutes.
3. **Network simulation** — multiple Docker containers running real
   WireGuard peers, driven by the actual binaries (§4). Runs on every PR
   for the core scenarios, nightly for the full matrix.
4. **Fuzzing & scale** — parser fuzzing and near-limit fleet sizes (§5–6).
   Runs nightly/on a schedule, not on every PR.

## 2. Unit testing

All in `wg-common` unless noted — both binaries call the same
implementation, so this is where correctness actually needs to be pinned.

- **Crockford Base32 encoding** ([wg-server.md §10](./wg-server.md#10-upload-to-storage)):
  pin the big-integer encoding against known values (all-zero input,
  all-`0xff` input, a single low-order bit set) so a future refactor
  can't silently drift onto RFC 4648 grouping — this is exactly the
  failure mode flagged in that section, and it's cheap to make
  structurally impossible to reintroduce.
- **Key validation**: reject a zero private/public/preshared key, reject
  non-canonical Base64, reject a key that doesn't decode to exactly 32
  bytes, reject duplicate public keys across nodes in one generation.
- **Hostname validation**: accept every valid boundary case (1 char, 63
  chars), reject uppercase, underscores, leading/trailing hyphen, empty
  string, and an FQDN suffix.
- **Topology / peer-selection algorithm** ([wg-server.md §7](./wg-server.md#7-topology--peer-selection-algorithm)):
  property-test the edge-generation function directly — for randomly
  generated `(nodes, master)` inputs, assert every master-master and
  master-spoke pair has exactly one edge, no spoke-spoke edge exists, and
  edge count equals `C(masters, 2) + masters × spokes` exactly. Also
  cover the degenerate cases explicitly: zero masters (no edges, and the
  documented warning fires), one node total, all nodes are masters.
- **Config parser / accepted subset** ([wg-client.md §6](./wg-client.md#accepted-configuration-subset)):
  table-driven accept/reject cases for every rule in that section —
  `PreUp`/`PostUp`/`PreDown`/`PostDown`/`SaveConfig`/`Table`, unknown
  sections, duplicate directives, duplicate peer keys, a peer matching
  the local public key, control characters (CR/LF/NUL) embedded in a
  value, a zero-peer configuration (must be *accepted*, not rejected —
  this is how node removal takes effect), overlapping `AllowedIPs`, and a
  `0.0.0.0/0` route.
- **Rendering determinism**: render the same input topology twice and
  assert byte-identical output (this is load-bearing for the
  skip-if-unchanged / no-new-generation path in
  [wg-server.md §8](./wg-server.md#8-key-management)); render with peers
  supplied in different input orders and assert identical output (sort
  order must not depend on input order).
- **Reuse/rotation resolution** ([wg-server.md §8](./wg-server.md#8-key-management)):
  given a fake "current generation" fixture, verify: an untouched node's
  key is byte-identical to its input; a `--rotate <hostname>` node gets a
  new key and every edge touching it gets a new PSK while untouched
  edges keep their old PSK; bare `--rotate` changes every key; a
  one-sided edge record (present in one endpoint's fixture, absent from
  the other's) is rejected as an integrity error rather than silently
  resolved.
- **`current.json` schema validation**: reject unknown fields, duplicate
  keys, malformed IDs, `current: null` with non-null `previous`, and
  equal `current`/`previous` IDs.
- **wg-client local state machine** ([wg-client.md §6](./wg-client.md#local-application)):
  drive the pending-transaction logic with an in-memory filesystem double
  — verify a "pending" marker left from a simulated crash triggers backup
  restoration on the next run *before* any hash comparison; verify a
  first-install (no prior backup) never fabricates a rollback; verify
  drift comparison ignores WireGuard's learned roaming endpoint and
  handshake counters but catches a changed key/address/route.

## 3. Integration testing against storage

### Tooling: what to actually run in CI

The local S3-compatible test-server landscape has been unstable —
**MinIO's OSS repo was archived in April 2026** and **LocalStack's in
March 2026**; neither should be assumed available or maintained going
forward. Recommendation:

- **Primary: a real R2 bucket dedicated to CI**, using scoped read-write
  and read-only test credentials. This is the only way to actually verify
  the conditional-write behavior the whole publication-commit protocol
  depends on ([wg-server.md §10](./wg-server.md#10-upload-to-storage)),
  since it's the real target provider. Budget for R2's actual latency in
  test timeouts rather than mocking it away.
- **Secondary, for fast/offline unit-adjacent runs: a lightweight
  self-hosted S3-compatible server** (e.g. Garage, currently the most
  commonly recommended self-hosted option post-MinIO) for tests that
  don't depend on exact conditional-write semantics. **Before relying on
  any such server for the atomic-publish test cases specifically, verify
  it supports `If-None-Match: *` with the wildcard** — MinIO itself never
  did (it required an exact ETag, unlike AWS/R2's wildcard support), so
  "S3-compatible" is not sufficient evidence that this particular
  primitive works. If the chosen server doesn't support it, keep those
  specific test cases on the real R2 bucket and use the local server only
  for everything else (discovery, digest verification, retention listing,
  etc.).
- **Fault injection: a proxy layer (e.g. Toxiproxy) in front of whichever
  backend**, for retry/backoff/timeout tests. This layer doesn't need
  correct conditional-write semantics — it's testing `wg-server`'s and
  `wg-client`'s own reaction to transport failures (timeouts, resets,
  5xx, throttling), which is independent of the backend's correctness.

### Scenarios

- **`wg-server apply` against an empty bucket**: initializes `current.json`
  per the `If-None-Match: *` rule; a second concurrent "initializer" (two
  processes racing the same empty-bucket init) must have exactly one
  winner and the other must detect and reconcile rather than error
  uselessly.
- **Reuse path**: run `apply` twice with no config changes; assert zero
  `PutObject` calls on the second run (beyond the revision-only pointer
  refresh) and that `current`/`previous` are unchanged.
- **Rotation path**: `--rotate <hostname>` and bare `--rotate`, asserting
  exactly the expected set of objects changes each time.
- **All-or-nothing upload failure and rollback**
  ([wg-server.md §10](./wg-server.md#10-upload-to-storage)): inject a
  failure (via the fault-injection proxy) partway through a multi-node
  upload batch and assert every object touched in that batch is restored
  to its pre-run content (or deleted, if newly created), `current.json`
  is untouched, and the run exits non-zero.
- **Conditional-publish conflict**: simulate two overlapping `apply` runs
  against the same bucket (deliberately violating the scheduler's
  serialization guarantee, to test the defense-in-depth layer, not to
  claim the scheduler doesn't need to work) and assert one run's pointer
  write is rejected and correctly reconciled per the three outcomes in
  [wg-server.md §10 "Publication commit"](./wg-server.md#10-upload-to-storage)
  (confirmed-succeeded / safe-to-retry / real-conflict).
- **Retention grace period**: upload an object with a fresh timestamp
  that isn't part of the retained pair and assert cleanup skips it; assert
  it's collected once the grace period elapses (use a short grace period
  in test config to keep this fast).
- **Client discovery/rediscovery**: publish a generation, start a
  download, delete that generation and publish a new one mid-flight
  (simulating the exact race in
  [wg-server.md §10 "Retention"](./wg-server.md#10-upload-to-storage)),
  and assert the client's bounded rediscovery loop recovers within its
  three-round budget rather than failing outright.
- **Retry/backoff behavior**: via the fault-injection proxy, assert
  exactly 5 attempts on a sustained transient failure, assert exponential
  backoff timing (within tolerance) between attempts, and assert a 404 or
  auth error consumes zero retries.

## 4. Docker-based network simulation

### Why userspace WireGuard, not the kernel module

CI runners frequently can't load kernel modules (rootless containers,
shared/managed CI hosts) and Docker's default settings don't grant that
capability. Run **`boringtun`** (Cloudflare's userspace WireGuard
implementation) inside each container instead of relying on `wg-quick`'s
kernel path — it only needs a TUN device and `CAP_NET_ADMIN`, which a
container can be granted (`--cap-add=NET_ADMIN --device /dev/net/tun`)
without touching the host kernel. `wg-quick` itself is still what
`wg-client` invokes (per its own design); the container just needs a
WireGuard implementation present that `wg-quick`'s `wg` calls resolve to,
whether that's the kernel module (on hosts that support it) or a
userspace daemon satisfying the same interface. Document which mode a
given CI environment uses so a failing network-simulation test can be
triaged against the right implementation.

### Topology fixtures

Build small Docker Compose fixtures mirroring the worked example already
in [wg-server.md §5](./wg-server.md#5-input-configuration-schema) and
[§7](./wg-server.md#7-topology--peer-selection-algorithm) (2 masters + 2
spokes), plus:

- a single-master, single-spoke minimal case,
- a spoke with `extra_allowed_ips` (gateway/site-subnet role),
- a NAT-simulated spoke (container with no inbound port published, only
  outbound reachability to the masters, to exercise the
  `PersistentKeepalive` / endpoint-omitted path for real).

### Assertions

For each fixture, after both `wg-server apply` (against the real or
Garage-backed test bucket) and `wg-client sync` on every node container
have run:

- every master can `ping` every other node (master or spoke) over its
  tunnel address;
- masters can `ping` each other;
- **a spoke cannot reach another spoke** directly over the tunnel — this
  is the one property that's easy to get backwards in an implementation
  and impossible to catch without an actual second data path to test
  against;
- a gateway spoke's advertised `extra_allowed_ips` subnet is reachable
  from a master, and a non-gateway spoke's traffic to that subnet is not
  captured by a route it shouldn't have.

### Lifecycle & chaos scenarios

- **Add a node**, re-`apply`, re-`sync` only the affected nodes (not the
  whole fleet), assert connectivity updates correctly and unrelated nodes
  were never re-uploaded (cross-check against the integration-layer
  assertion in §3 that unrelated objects don't change).
- **Promote a spoke to master**: assert new edges appear to every other
  master and that the promoted node's *identity key is unchanged* (only
  new edges/PSKs are created — per
  [wg-server.md §8](./wg-server.md#8-key-management), promotion alone
  must not force a rotation).
- **Remove a node**: assert its former peers drop it after their next
  sync, and that its own container (still holding stale keys/routes)
  cannot reach anyone.
- **Kill `wg-client` mid-apply** (`SIGKILL` between the teardown and
  install steps in [wg-client.md §6](./wg-client.md#local-application)):
  on the next invocation, assert it recovers from the pending-transaction
  marker and restores the last-good config *before* considering a
  content-hash no-op, matching the documented recovery ordering.
- **Partition a node from storage mid-sync** (via the fault-injection
  proxy or a Docker network disconnect): assert the existing tunnel is
  left running and the node retries on its next scheduled pass.
- **Kill `wg-server` mid-upload-batch** (partition it from storage after
  some but not all `PutObject` calls land): assert the rollback described
  in §3's "all-or-nothing" scenario holds even when triggered by a real
  process kill, not just an injected error return.

## 5. Security testing

- **Fuzz the config parser** (`cargo-fuzz`, `wg-common`'s parser as the
  target) with the accepted-subset grammar as a seed corpus. This is the
  component most exposed to adversarial input (a compromised or buggy
  storage object is untrusted input by design — see
  [wg-client.md §10](./wg-client.md#10-security-considerations)), so it's
  the highest-value fuzz target in the project. Run against a persisted
  corpus in CI, not just ad hoc.
- **Limit enforcement**: configs/generations at exactly and one-over each
  documented limit (4096 nodes, 16 MiB discovery JSON, 1 MiB per rendered
  config) — assert the boundary is rejected with a clear error, not an
  off-by-one silent acceptance or a panic.
- **Master fan-out file-size case**
  ([wg-server.md §6](./wg-server.md#6-hostname--topology-validation)):
  construct a fleet sized so a master's rendered file lands just under,
  at, and just over 1 MiB, and assert `apply` fails cleanly on the
  over-limit case, naming the offending hostname.
- **Credential scoping (manual/documented, not fully automatable)**: this
  depends on real provider IAM behavior, not just the binaries' own
  logic. Maintain a deployment checklist item to periodically verify
  (against a real R2 test bucket + a real per-node scoped token) that a
  spoke's read-only credentials genuinely cannot `GetObject` another
  hostname's file when prefix-scoped per
  [wg-server.md §12](./wg-server.md#12-security-considerations).
- **Injection attempts**: control characters, `PreUp`/hook directives,
  and oversized values injected into every string field the renderer
  touches, asserting rejection at validation rather than appearing in
  rendered output.

## 6. Performance / scale testing

- Run `wg-server apply` against a synthetic ~4000-node fixture (mixed
  master/spoke ratio, and separately an all-masters worst case for edge
  count) and measure wall-clock time against the 8-minute job deadline
  ([wg-server.md §10](./wg-server.md#10-upload-to-storage)) under
  realistic (not zero) simulated network latency to the test bucket.
- Run `wg-client sync` against the largest single-node config the fixture
  above produces and measure against the 320s total pass budget
  ([wg-client.md §6](./wg-client.md#retry-policy)).
- Track both numbers over time (a simple CI-reported benchmark, not a
  hard gate initially) so a regression is visible before it silently
  eats the deadline's margin.

## 7. Milestones

| Milestone | Scope | Exit criteria |
|---|---|---|
| M0 — `wg-common` core | Parser/renderer, key validation, Base32 encoding, topology algorithm | §2 unit suite green; Base32 vectors pinned; property tests for topology algorithm passing |
| M1 — `wg-server` apply (no rotation) | Init, reuse, render, upload, `--dry-run` | §3 integration scenarios for init/reuse green against real R2 test bucket |
| M2 — `wg-server` rotation, all-or-nothing, retention | `--rotate`, rollback, conditional publish, cleanup + grace period | §3 rotation/rollback/conflict/retention scenarios green |
| M3 — `wg-client` sync | Discovery, digest verification, local apply/rollback, locking | §2 state-machine unit tests + §3 discovery/rediscovery scenarios green |
| M4 — Network simulation harness | Docker Compose fixtures, boringtun-based containers, connectivity assertions | §4 topology fixtures and lifecycle scenarios green in CI |
| M5 — Chaos & fault injection | Process kills, storage partitions, overlapping server runs | §4 chaos scenarios green; §3 fault-injection retry/backoff scenarios green |
| M6 — Security & scale | Fuzzing corpus, limit boundaries, scale benchmarks | §5 fuzz target running nightly with no crashes; §6 benchmarks recorded and under budget |

M0–M3 gate normal development (should stay green on every PR). M4–M6 can
start once M1–M3 are stable and are expected to run nightly/on-schedule
rather than on every commit, given their cost (real containers, a real
storage backend, fuzzing time).

## 8. CI notes

- `cargo nextest` for the Rust test runner (parallelism, better output
  for the table-driven §2 suites).
- Persist the `cargo-fuzz` corpus between runs (cache it) rather than
  starting from the seed corpus every time.
- The real-R2 test bucket's credentials are CI secrets like any other —
  scope them to a bucket used for nothing else, and never point CI at a
  production fleet's bucket.
- Docker-in-CI network simulation jobs should be tagged/skippable
  separately from the fast unit/integration jobs, so a contributor
  iterating on `wg-common` isn't stuck waiting on container startup time
  for every push.
