# Testing Plan & Milestones

Status: design only, no implementation yet. Companion to
[wg-server.md](./wg-server.md) and [wg-client.md](./wg-client.md).

## 1. Testing philosophy

Four layers, cheapest/fastest first. Catch a defect at the lowest layer
that can establish the relevant property. Use real WireGuard interfaces
for host application and packet-flow checks; parsing and publication
decisions do not require them. This plan tests the companion designs and
does not add new CLI flags or broaden v1 platform support.

1. **Unit** — pure functions, no filesystem/network/subprocess. Runs on
   push and PR, seconds.
2. **Integration** — storage operations, real files, locks, and subprocess
   handling (§3), without WireGuard interfaces. Local and deterministic
   cases run on every PR; real-R2 conformance runs on trusted PR revisions
   and nightly (§8).
3. **Network simulation** — multiple Docker containers running real
   WireGuard peers, driven by the actual binaries (§4). Runs on every PR
   for the explicitly listed core scenarios, nightly for the full matrix.
4. **Fuzzing & scale** — parser fuzzing and near-limit fleet sizes (§5–6).
   Runs nightly/on a schedule, not on every PR.

## 2. Unit testing

### Shared core (M0)

Test the common parser, renderer, encoding, and validation in `wg-common`.
Test binary-specific decisions in their owning crates as listed below.

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
- **Typed topology validation**: cover strict JSON fields/types/nulls,
  duplicate hostnames/master entries, empty nodes, unknown masters,
  endpoint syntax/ranges, an edge with neither endpoint configured,
  unique unicast `/32` addresses, and disjoint canonical extra routes.
  Reject routes covering any tunnel address and IPv6 tunnel routes;
  accept valid IPv6 transport endpoints. Pin keepalive defaults and
  explicit zero, and DNS/MTU parsing and ranges.
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
  this lets a remaining node drop its final peer), overlapping `AllowedIPs`,
  and a `0.0.0.0/0` route.
- **Rendering determinism**: render the same input topology twice and
  assert byte-identical output (this is load-bearing for the
  skip-if-unchanged / no-new-generation path in
  [wg-server.md §8](./wg-server.md#8-key-management)); render with peers
  supplied in different input orders, including extra-route order, and
  assert identical output. Round-trip generated valid configurations
  through the shared parser and compare typed values, including defaults,
  zero peers, endpoint omission, and locally determined keepalives.
- **`current.json` schema validation**: reject unknown fields, unsupported
  versions, duplicate keys, malformed IDs/revisions/digests, invalid or
  empty node maps, `current: null` with non-null `previous`, and equal
  `current`/`previous` IDs. Accept an initialized empty pointer and a first
  publication with `previous: null`. Pin canonical ID/digest rejection
  cases, including uppercase, aliases, and nonzero unused high bits.

### Server decisions (M1–M2)

- **Reuse/rotation resolution** ([wg-server.md §8](./wg-server.md#8-key-management)):
  given a fake "current generation" fixture, verify: an untouched node's
  key is byte-identical to its input; a `--rotate <hostname>` node gets a
  new key and every edge touching it gets a new PSK while untouched
  edges keep their old PSK; bare `--rotate` changes every key; a
  one-sided edge record, conflicting reciprocal PSKs, or duplicate peer
  key is an integrity error even when rotation is requested. A new edge
  gets one shared PSK without changing existing identities. Reuse and
  integrity cases belong to M1; explicit rotation cases belong to M2.
- **Publication and retention decisions**: model all commit outcomes in
  §3. M1 covers conditional publication; M2 covers revision fencing and
  cleanup selection. With a fake clock, test grace-period age just below,
  at, and above the threshold; retained IDs are protected regardless of
  age. A future `LastModified` must not make an object eligible early.

### Client decisions (M3)

- **Hostname resolution and CLI**: check flag/environment/system-source
  precedence, ASCII lowercasing before shared hostname validation, and
  rejection of an explicitly empty source without fallback. Cover
  interface-name/path boundaries, mode conflicts, and positive intervals.
- **wg-client local state machine** ([wg-client.md §6](./wg-client.md#local-application)):
  drive the pending-transaction logic with an in-memory filesystem double
  — verify a "pending" marker left from a simulated crash triggers backup
  restoration on the next run *before* any hash comparison; verify a
  first-install (no prior backup) never fabricates a rollback; verify
  drift comparison ignores WireGuard's learned roaming endpoint,
  handshake timestamps, and counters but catches changes in every managed
  field. Identical content with a new generation may update metadata but
  must preserve the last-good backup and avoid down/up unless forced or
  running state needs repair. Reject a changed owning hostname/interface
  and ambiguous untracked local state. Real filesystem and process checks
  are separate integration tests (§3).

## 3. Integration testing

### Tooling: what to actually run in CI

- **Provider conformance**: use dedicated real-R2 test buckets to verify
  the target provider's behavior. Before qualifying any pinned local
  S3-compatible backend, test `If-None-Match: *` creation, rejection of
  overwrites, `If-Match` replacement with the current ETag, rejection of
  stale ETags, and read/list visibility after writes and deletes. Race
  writers using the same initial precondition and check exactly one
  mutation wins. Preserve ETags exactly as returned. A compatibility label
  alone is insufficient.
  MinIO did support wildcard conditional creation in later releases;
  the [upstream discussion](https://github.com/minio/minio/discussions/20318)
  confirms it after an upgrade to the September 2024 release. However,
  **MinIO's OSS repository was archived in April 2026, and LocalStack's in
  March 2026** — neither has an ongoing maintenance/security-fix path
  going forward, regardless of which historical release passed
  conformance. Treat "passed conformance on a pinned version" and "still
  maintained" as two separate, both-required conditions before adopting a
  local backend; re-check the current archival/fork status of whichever
  backend is chosen at adoption time, since this landscape has shifted
  more than once in a single year.
- **Local runs**: use a pinned local backend for the cases it qualifies for.
  If it lacks required conditionals, use it only for independent read/list
  cases; deterministic transport tests cover application protocol decisions
  offline, and the real-R2 gate supplies provider evidence. Serve local
  endpoints over HTTPS with a trusted test CA; retain certificate validation.
- **Isolation**: reserve a separate bucket for each concurrently executing
  scenario, or exclusively lease a bucket and serialize its use across
  test processes and CI runs. Different hostnames or generation IDs do not
  isolate the fixed `current.json` or cleanup's `nodes/` scan. Intentional
  writer races share only their own isolated bucket. Clean up test buckets
  through the harness, including after failed runs; use no production data.
- **Fault injection**: use [Toxiproxy](https://github.com/Shopify/toxiproxy)
  for TCP delay, resets, and interrupted streams. Use an HTTP-aware test
  endpoint or instrumented SDK transport for 5xx/throttling and controlled
  response loss before or after a write lands. Preserve TLS verification
  and request-signing behavior in real-provider tests; a TCP proxy cannot
  synthesize HTTPS status codes or count individual encrypted requests.
- **Harness boundaries**: execute the actual server binary for storage/CLI
  scenarios. Exercise client sync logic without interfaces through an
  explicit host-operation test adapter; separately test production CLI
  validation and failure paths that stop before interface mutation. Use
  real temporary files, `flock`, and controlled child processes for local
  integration. The test adapter is not a production preflight bypass.

### Server publication (M1)

- **Initialization**: initialize an absent pointer only after a successful
  listing proves the reserved namespace empty. Distinguish a missing
  object, missing bucket, and denied listing. Existing generation objects
  without a pointer must fail closed. In a synchronized initializer race,
  only one conditional creation succeeds; the loser rereads state and
  safely reports a conflict or replans, without overwriting the winner.
- **Reuse and complete generations**: an unchanged pass creates no new
  generation and performs no configuration PUTs. A changed hostname set
  or rendered file requires a new generation containing every desired
  node, including byte-identical files. Verify all digests and that older
  generation objects remain unchanged. M2 additionally checks the
  revision-only refresh and cleanup on unchanged passes.
- **Published-state integrity**: missing/corrupt referenced files, digest
  mismatches, invalid configs, and inconsistent reusable key records abort
  publication without incidental rotation. Unreferenced uploads are never
  a source of reusable identities. Apply these checks with rotation in M2.
- **Upload failure**: fail a batch after some candidate PUTs land. The
  committed pointer and retained configurations remain unchanged, the run
  exits nonzero, and uploaded candidates remain unreferenced for later
  cleanup. An interrupted first publication may leave the initialized
  empty pointer. There is no compensating overwrite of live files.
- **Upload retry**: lose a successful configuration PUT response. Retry
  with the same generation ID and bytes; an already-present identical
  object counts as complete after verification, while differing content
  is an integrity error. Never publish until every upload is confirmed.
- **Publication outcomes**: independently force all four outcomes in
  [wg-server.md §10](./wg-server.md#publication-commit): intended pointer
  already stored (success), original pointer/ETag still present (bounded
  retry), another pointer present (conflict), and failed reconciliation
  read (unknown). Reuse intended bytes/revision within the invocation;
  preserve candidates and skip cleanup while the outcome is unknown.
  Synchronize overlapping writers after they read the same pointer to
  test stale-ETag rejection; that race alone does not cover response loss.
- **CLI contracts**: `validate` performs no key generation or network
  calls. Invalid input/rotation targets fail before network access.
  `--dry-run` on empty, unchanged, and changed state performs no writes or
  deletes and emits no secrets. Extend it to rotation/cleanup plans in M2.

### Server rotation and retention (M2)

- **Rotation**: test named, repeated named, and bare `--rotate`; reject
  mixing bare and named forms. Check changed identity keys and incident
  PSKs, preserved unrelated keys, and complete uploads under the new ID.
  Reissuing the command after a confirmed commit is a new rotation, not
  an idempotent replay. Corrupt source state must still fail validation.
- **Retention and grace**: exercise multiple listing pages, old committed
  generations, and abandoned uploads. Protect current/previous regardless
  of age, `current.json`, unrelated keys, and unexpected object names.
  Fresh orphans remain until their grace period expires. Use an injected
  clock in deterministic tests and genuinely aged fixtures in scheduled
  provider tests; no undocumented production grace-period override is
  required. An unchanged pass still refreshes revision and attempts cleanup.
- **Delayed publication fencing**: hold an old conditional pointer request,
  terminate its originating job, then run an unchanged successor. After
  the successor refreshes revision, release the delayed request and assert
  it cannot commit. Do not overlap live cleanup/upload jobs outside the
  deliberate conflict fixtures; scheduler serialization remains required.
- **Cleanup failures**: failed/unknown revision refresh or changed pointer
  verification skips cleanup. A list/delete failure after confirmed
  publication exits nonzero with `publication=committed` or `unchanged`
  and `cleanup=incomplete`; current remains usable and the next serialized
  pass retries cleanup, including when content is unchanged.

### Client discovery and local integration (M3)

- **Discovery and integrity**: missing/empty publication, missing hostname,
  invalid metadata, digest mismatch, or invalid downloaded config leaves
  the existing/restored tunnel untouched. Never fetch `previous` for a
  removed node, list generations, or select IDs chronologically. A fully
  verified download remains usable after its remote generation is removed.
- **Retention race**: publish G0, let its objects age past the grace period,
  then pause a client after discovery but before its config GET. Publish
  G1 and G2 serially so cleanup can legitimately delete G0. The client
  must rediscover G2 after G0's missing-object response. G1 alone cannot
  retire G0, which would still be previous. Exercise the same sequence with
  deterministic fixtures on PRs and aged provider fixtures nightly.
- **Rediscovery boundaries**: an unchanged current ID with a missing
  referenced file is an integrity error. A revision-only refresh does not
  count as a new generation. Repeated generation churn exhausts at most
  three discovery rounds, preserving local state on failure. Test both
  configuration 404s and ambiguous 403s: reread the pointer within that
  budget, retry a changed generation, and fail on unchanged state. A
  pointer 403 is terminal, with no credential broadening or same-key retry.
- **Files and locking**: check root-owned `0600` config, backup, metadata,
  and temporary files from creation; reject symlinks and non-regular files.
  Check trusted parent directories and missing `/run/wg-client` creation
  with mode `0700`. Test wrong ownership, unsafe path replacement, and
  preservation of unrelated files. Two processes targeting one interface
  must share the fixed lock even with different config directories; the
  second exits immediately with 1. Never unlink the lock or let children
  inherit its descriptor; release it during daemon sleep.
- **Transaction failures**: inject stage/backup/pending-write, teardown,
  install, startup, verification, and rollback errors. Check durable
  ordering, file/directory fsync calls, and atomic metadata transitions.
  Stop candidate application after failed teardown, preserve the verified
  backup while pending, and retain recovery information if rollback fails.
  First-install failure cleans partial state without claiming restoration
  of a nonexistent backup. Process-kill cases on real interfaces are in §4;
  a process kill alone does not prove power-loss durability.
- **Credential reload and daemon behavior**: reload explicitly supplied
  credentials/session token each pass, including renewal after expiry;
  never fall back to environment or instance credentials. Reject changed
  owning hostname/interface or invalid reload without changing the tunnel.
  Test fresh pass budgets, continuation after transient failure,
  interruptible sleep, and coordinated `SIGTERM`/`SIGINT` with controlled
  child processes.

### Retry and deadline checks (M1 server, M3 client)

- Fast sustained transient responses can exhaust exactly five attempts
  within the 30s call limit. Slow failures must stop at the deadline even
  with attempts remaining. Check 10s attempt limits, exponential backoff
  with jitter and a 5s cap, and no retry of permanent errors against the
  same resource/credentials. Client rediscovery is separately bounded above.
- Mix SDK failures with interrupted streamed bodies and assert a single
  five-attempt budget per call. Include bodies that stall after successful
  headers and success on the final permitted attempt. Use controlled time
  and randomness for exact assertions; real transport checks use tolerances.
- Enforce at most eight concurrent server storage calls and the 8-minute
  job limit. Check the client's 180s discovery/download, 20s preflight,
  shared 120s recovery/apply/rollback, 30s subprocess, and 320s total pass
  budgets. Cancel and reap timed-out children before recovery commands;
  exhausting a budget cannot discard incomplete recovery state. These
  deadline checks gate correctness independently of scale benchmarks.

## 4. Docker-based network simulation

### Supported execution mode

Use rootful Linux Docker on runners with WireGuard kernel support already
available. Run node containers with `CAP_NET_ADMIN` and isolated network
namespaces, per [wg-client.md §8](./wg-client.md#8-privileges) — `wg-client`
itself needs only `CAP_NET_ADMIN` (root remains the default packaged
deployment, but is not required by the interface-configuration path
anymore) since it configures the kernel device directly over netlink/UAPI
via `wireguard-control`, the same way
[innernet](https://github.com/tonarino/innernet) does, rather than
shelling out to `wg`/`wg-quick`. Containers therefore need no
`wireguard-tools` package or Bash for the client to function; keep `wg`
installed anyway for manual inspection (`wg show`, etc., per
`docker-tests/README.md`) and iproute2 for fixture setup/debugging outside
the client's own netlink calls. Fixtures with `DNS` also need a compatible
`resolvconf` integration. Preflight runner support before the suite;
missing infrastructure is not a passing network test. Containers need no
module-loading privilege.

BoringTun is not the v1 acceptance harness: the client requires kernel
support. Any future userspace matrix requires an explicit client support
decision first — `wireguard-control`'s netlink/UAPI path goes straight to
the kernel device with no automatic userspace fallback (unlike stock
`wg-quick`, which tries the kernel before falling back to
`WG_QUICK_USERSPACE_IMPLEMENTATION`); adding userspace support here would
mean explicitly wiring in a userspace backend (e.g. embedding BoringTun),
not installing a daemon that gets picked up implicitly. Record kernel,
tools, image versions, and actual interface implementation with test
results.

### Topology fixtures

Build a two-master/two-spoke fixture matching the diagram in
[wg-server.md §7](./wg-server.md#7-topology--peer-selection-algorithm), plus:

- a single-master/single-spoke minimal case;
- a gateway spoke with `extra_allowed_ips`, a separate site host/network,
  and explicitly provisioned forwarding, firewall rules, and return routes
  or NAT. The advertised subnet must not be reachable through a direct
  Docker shortcut;
- a NAT spoke on a private network behind a router container, with masters
  on a separate external network. Configure SNAT and stateful filtering:
  allow outbound initiation and return traffic, deny unsolicited inbound
  UDP, and provide no direct master-to-private-network route. Unpublished
  ports alone do not isolate containers on a shared bridge
  ([Docker documentation](https://docs.docker.com/engine/network/drivers/bridge/)).

Keep storage and resolver access on an underlay management path independent
of the VPN. Use disjoint underlay, tunnel, and advertised subnet ranges so
valid fixtures pass the client's host-route preflight.

### Assertions

After `wg-server apply` against an isolated qualified test bucket and
`wg-client sync` on each participating node, use bounded active probes.
Verify routes/interface selection and WireGuard packet counters so a
successful probe cannot be satisfied by an underlay shortcut.

- Every master reaches every other node over its tunnel address, and
  spokes reach their masters. Positive controls establish working tunnels
  before evaluating negative probes.
- Spokes have no direct spoke peers or generated routes for other spokes.
  With hub forwarding disabled or explicitly filtered in the fixture,
  spoke-to-spoke probes fail. Absence of a direct edge is not a firewall
  guarantee; operator-enabled hub forwarding is a separate policy.
- A master's route to the gateway's site subnet uses the gateway peer;
  other spokes acquire no unintended route for that advertisement.
- The NAT spoke initiates without an advertised endpoint; masters learn
  its endpoint. Test keepalive across an idle interval exceeding a known
  NAT expiry, with no active probes that refresh the mapping. Compare
  enabled keepalive against explicit zero. Observe router state/packets
  before spontaneous WireGuard traffic can reopen an expired mapping;
  configure the test expiry accordingly and bound the subsequent probe.

### Lifecycle and host application (M4)

- **No-op and repair**: unchanged bytes with matching running state cause
  no down/up, including a revision-only pointer refresh or a new generation
  with identical node bytes. `--force` reapplies. A missing/down interface
  or managed drift is repaired; an unrelated non-WireGuard device using
  the requested name is rejected without mutation.
- **Add a node**: publish a complete generation, asserting unchanged bytes
  and key material for unaffected nodes. Sync affected nodes first and
  verify new connectivity; then sync unaffected nodes and assert no flap.
- **Promote a spoke to master**: new edges connect it to the remaining
  spokes; its edges to existing masters already existed. In the
  two-master/two-spoke fixture, promotion adds exactly one edge. Preserve
  all existing identity keys and existing-edge PSKs; only new edges get
  fresh PSKs. Check connectivity after affected nodes sync.
- **Remove a node/final peer**: revoke its test storage credentials and
  publish its removal. Once all former peers sync, its stale local keys
  cannot reach those peers over the VPN. Before that convergence, removal
  is not immediate revocation. Separately remove the sole master while
  retaining a spoke: the spoke installs a valid zero-peer configuration.
  In a separate case with still-valid credentials, a removed client's sync
  reports not registered, preserves local state, and never falls back to
  `previous`.
- **Staggered rotation**: sync one endpoint, then the other. Temporary
  incompatible peer keys must not trigger rollback solely for missing
  handshakes while local and management checks pass. After both updates,
  actively verify connectivity. Also test skipping intermediate generations.
- **Boot recovery and management protection**: restart a node with durable
  config/state but no interface or `/run` contents and unavailable storage;
  restore its last-good tunnel before discovery. Reject candidate routes
  capturing DNS, storage, transport endpoints, or another interface's
  nondefault routes before teardown. An ordinary underlay default route
  is allowed. Missing utilities/DNS integration also fail preflight.
  Inject a post-start management failure and verify local rollback.

### Chaos scenarios (M5)

- **Kill `wg-client` during a transaction**: use synchronized `SIGKILL`
  points after pending-state persistence, after teardown, after candidate
  installation, and after startup but before success is recorded. Restart
  and verify backup recovery precedes any content-hash no-op, including
  when installed bytes match storage. Cover first installation without a
  backup, failed recovery, and startup subprocesses that outlive the parent.
  Ensure no surviving child can race recovery in the tested deployment.
- **Graceful stop and child timeouts**: send `SIGTERM`/`SIGINT` during
  download, sleep, and application; exercise the documented systemd and
  process-group behavior. Cancel/reap children before recovery and respect
  the remaining shared local-operation budget. Preserve pending state
  when recovery cannot finish.
- **Storage partition**: interrupt only the management path during
  discovery/download, after startup recovery. The existing tunnel remains
  running and a later pass gets a fresh budget. A partition discovered by
  the post-apply management check instead triggers rollback. Preserve VPN
  underlay links so the test does not create an unrelated transport outage.
- **Kill `wg-server` mid-upload**: send `SIGKILL` after some candidate PUTs
  land and before any pointer commit. The committed generation remains
  discoverable; partial candidate files may remain. Start a serialized
  successor with no runner-local state and verify recovery from the bucket,
  preservation of current keys, and orphan cleanup after grace eligibility.
  Test storage partition separately as an error-return path. A kill near
  pointer commit must use §3's reconciliation expectations, not assume the
  write failed merely because its process died.
- **Real transport races**: repeat §3's response-loss, delayed-write,
  overlapping-writer, and streamed-body cases through the proxy/provider
  harness. These extend the deterministic M1–M3 cases rather than deferring
  publication safety and retry correctness until M5.

The **PR core** is the minimal and two-master/two-spoke fixtures with
baseline connectivity/peer-route assertions, no-op/force/repair, node-add
convergence, and one route-conflict preflight rejection. Run the gateway,
NAT/idle, other lifecycle cases, and full chaos matrix nightly. M4 requires
the full topology/lifecycle suite to pass at completion; M5 adds chaos.

## 5. Security testing

- **Fuzz the config parser** (`cargo-fuzz`, `wg-common`'s parser as the
  target) with the accepted-subset grammar as a seed corpus. This is the
  component most exposed to adversarial input (a compromised or buggy
  storage object is untrusted input by design — see
  [wg-client.md §10](./wg-client.md#10-security-considerations)), so it's
  the highest-value fuzz target in the project. Run against a persisted
  corpus in CI, not just ad hoc.
- **Limit enforcement**: for otherwise valid inputs, accept exactly each
  documented maximum and reject one-over with a clear error: 4096 nodes,
  16 MiB topology/discovery JSON, and 1 MiB per configuration. MiB means
  `1024 × 1024` bytes. Isolate limits so an unrelated file-size or schema
  error does not masquerade as the tested boundary. Test streamed download
  limits with absent or misleading `Content-Length`, including oversized
  bodies after valid headers, without allocating unbounded memory.
- **Master fan-out file-size case**
  ([wg-server.md §6](./wg-server.md#6-hostname--topology-validation)):
  construct a fleet sized so a master's rendered file lands just under,
  at, and just over 1 MiB. Accept the otherwise valid under/at cases and
  reject the over-limit case before uploading any configuration, naming
  the hostname and rendered size. Never truncate output to meet the cap.
- **Credential scoping**: automate provider checks using a real per-node
  scoped token in an isolated R2 bucket. Positive controls must read
  `current.json` and that node's files across generations; negative controls
  must deny reading another existing node's file, listing, writing, and
  deleting. Check session-token expiry/renewal separately. Provider IAM
  cannot be established by mocks; keep an equivalent documented deployment
  check where CI lacks a trusted credential issuer. Parent/issuer tokens
  must remain outside node containers, per
  [wg-server.md §12](./wg-server.md#12-security-considerations).
- **Injection attempts**: control characters, `PreUp`/hook directives,
  and oversized values injected into every string field the renderer
  touches, asserting rejection at validation rather than appearing in
  rendered output.
- **Secret handling**: capture sanitized logs, parser errors, dry-run
  summaries, and failed subprocess output with seeded test secrets; assert
  no private key, PSK, credential, signing header, or full config leaks.
  Verify HTTPS/certificate failures are rejected and downloaded metadata
  cannot redirect requests or choose local paths. Exercise the file and
  lock protections in §3 as required M3 checks, not just nightly fuzzing.

Deterministic rejection, limit, and secret-handling cases run on every PR
as their owning components land. Continuous fuzzing and provider credential
checks run on the scheduled/trusted lanes in §8. Persist minimized failures
as regression cases; a crash-free fuzz run alone does not establish parser
correctness.

## 6. Performance / scale testing

- Run `wg-server apply` against a synthetic ~4000-node fixture (mixed
  master/spoke ratio, and separately an all-masters worst case for edge
  count) and measure wall-clock time against the 8-minute job deadline
  ([wg-server.md §10](./wg-server.md#10-upload-to-storage)) under
  a recorded latency/bandwidth profile. Keep benchmark inputs within all
  validation limits; oversized fixtures belong to rejection testing in §5.
  Measure initial publication, unchanged reuse, changed publication, and
  retention cleanup separately; record requests, concurrency, peak memory,
  node/edge counts, and per-config sizes alongside elapsed time.
- Run `wg-client sync` with a real kernel interface against the largest
  valid single-node config the fixture above produces and measure against
  the 320s total pass budget
  ([wg-client.md §6](./wg-client.md#retry-policy)).
- Pin build profile, runner resources, fixture parameters, and tool/backend
  versions so results can be compared. Report completion versus timeout,
  phase timings, and remaining deadline margin; reaching the timeout is
  not a successful benchmark completion.
- Benchmarks are informational initially, including at M6: record and
  investigate regressions without requiring every scale fixture to finish
  within budget as an exit gate. The hard runtime deadlines still apply
  and are tested in §3. A future performance gate needs an agreed workload
  and environment before a numerical threshold is enforced.

## 7. Milestones

| Milestone | Scope | Exit criteria |
|---|---|---|
| M0 — `wg-common` core | Shared parsing/rendering, key and topology validation, Base32, discovery schema | §2 Shared core suite green, including vectors, topology properties, round-trips, and applicable §5 rejection/limit tests |
| M1 — Safe `wg-server` publication | Init, reuse, complete immutable generations, conditional commit, `validate`, `--dry-run` | §2 M1 server decisions, §3 Server publication and server retry/deadline cases green; real-R2 conformance and publication suite passed on the trusted revision |
| M2 — Server rotation and retention | `--rotate`, revision refresh/fencing, paginated cleanup and grace | §2 M2 server decisions and §3 Server rotation and retention cases green; dry-run and integrity cases extended to rotation/cleanup; provider retention run passed |
| M3 — `wg-client` sync | Discovery, digest verification, local transaction/recovery, files/locking, daemon reload/signals | §2 Client decisions, §3 Client discovery and local integration, client retry/deadline cases, and applicable §5 deterministic security checks green |
| M4 — Kernel network harness | Rootful Docker fixtures, real interfaces, connectivity and host lifecycle | §4 topology/assertion and Lifecycle and host application suites passed; PR core required and full lifecycle scheduled nightly |
| M5 — Full chaos matrix | Process kills, partitions, real transport response loss and writer races | §4 Chaos scenarios passed on the supported kernel/provider harness and scheduled nightly; deterministic failure cases remain M1–M3 gates |
| M6 — Scheduled security and scale | Persisted fuzzing corpus, provider credential checks, reproducible benchmarks | §5 deterministic cases green, nightly fuzzing configured with no unresolved crashes, provider scoping checks passed, and §6 benchmark outcomes/baselines recorded; scale timings remain informational |

Milestones describe delivery scope; §8 defines ongoing CI frequency. M1
already includes publication safety, and M3 includes local recovery safety.
M4–M6 can build on stable M1–M3 work; their deterministic prerequisites
remain required PR checks. Full integration/acceptance runs must actually
pass before marking the corresponding milestone complete. M6's scale
measurements are the explicit informational exception.

## 8. CI notes

| Lane | Frequency | Gate |
|---|---|---|
| Unit, deterministic protocol/security/limit cases, local storage/files/process integration | Every PR; unit suite also on pushes | Required for implemented M0–M3 scope |
| Kernel network PR core explicitly listed in §4 | Every PR once M4 lands | Required; missing runner capability is an infrastructure failure |
| Real-R2 conformance and publication/retry checks | Trusted PR revisions and nightly | Required provider evidence; unavailable credentials mean not run, not passed |
| Aged retention/rediscovery, gateway/NAT, full host lifecycle and chaos, provider scoping | Nightly and before completing the owning milestone | Required acceptance cases; failures need triage |
| Fuzzing with persisted corpus | Nightly | Crashes fail the lane and become regression cases |
| Scale benchmarks | Scheduled | Informational outcomes and deadline margin, as in §6 |

- Use `cargo nextest` with separate named suites for fast tests, provider
  integration, and kernel networking. Developers can run only a chosen
  suite locally; that does not make its required PR checks optional.
- Pin Rust/tools/container/backend versions. Record scenario, fixture
  seed, sanitized request outcomes, and implementation mode for failures.
  Use explicit synchronization points for races and crash injection rather
  than sleeps that depend on runner load.
- Apply §3's bucket isolation across test-runner parallelism, workflow
  retries, and separate PR jobs. Provision unversioned, private test buckets
  without retention locks or lifecycle expiry of the reserved namespace.
  Scope credentials to those buckets and avoid production configuration.
- Run fork/untrusted PR code without provider secrets. Use the local and
  deterministic suites there; run provider checks on a trusted revision
  with scoped credentials and report the tested revision. Do not treat
  skipped provider checks as successful coverage. GitHub Actions normally
  withholds secrets from fork PR workflows
  ([GitHub documentation](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request)).
- Persist the fuzz corpus between scheduled runs and retain minimized
  regression inputs in the repository. Keep generated private configs,
  credentials, and raw secret-bearing subprocess output out of caches and
  uploaded diagnostics; synthetic fuzz inputs must contain no real secrets.
