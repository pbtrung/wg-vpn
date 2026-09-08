# `wg-server` — Design Document

Status: design only, no implementation yet.

## 1. Purpose

`wg-server` is an offline/control-plane CLI tool. An operator (human or CI job)
runs it against a JSON topology file. The tool:

1. Validates the topology (unique hostnames, valid master list).
2. Generates WireGuard key material (private/public keypair per node,
   preshared key per allowed connection) — all randomly generated, never
   supplied by the operator.
3. Renders one `wg-quick`-format `.conf` file per node, encoding the correct
   set of peers for that node.
4. Uploads each file to an S3-compatible bucket (Cloudflare R2 in the
   reference deployment) as `{hostname}.conf`, to be pulled later by
   `wg-client` (see [docs/wg-client.md](./wg-client.md)).

`wg-server` never touches a live WireGuard interface itself — it only
produces and publishes configuration.

## 2. Non-goals

- Does not manage the wg interface on any host (that's `wg-client`).
- Does not provision the storage bucket or IAM/API tokens — those are
  created by the operator ahead of time (see §12, Security considerations).
- Does not rotate keys on a timer; rotation is a manual, operator-triggered
  action (see §8, Key management).

## 3. Project layout (proposed workspace)

```
wg-vpn/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── wg-common/              # shared: S3 client wrapper, wg-quick renderer,
│   │                           # config types, error types
│   ├── wg-server/              # this binary
│   └── wg-client/              # sibling binary — docs/wg-client.md
└── docs/
    ├── wg-server.md
    └── wg-client.md
```

## 4. CLI

```
wg-server apply    --config <path> [--rotate [<hostname>]]... [--prune] [--dry-run]
wg-server validate --config <path>
```

| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | required | Path to the topology JSON (§5). |
| `--rotate [<hostname>]` | none | Force a brand-new random key for the given node(s) instead of reusing what's currently published (§8). Two forms: `--rotate <hostname>` (repeatable) rotates only the named node(s); bare `--rotate` with no value rotates **every** node in `--config`. Passing both a bare `--rotate` and one or more `--rotate <hostname>` in the same invocation is a validation error — pick one form. |
| `--prune` | off | Delete `.conf` objects from the bucket whose hostname is no longer present in `--config` (determined by listing the bucket — see §10). Without this flag, removed nodes are simply not regenerated; their old `.conf` object is left in place until an operator cleans it up. |
| `--dry-run` | off | Compute the full plan (which keys are reused vs. freshly generated, rendered files, upload/skip/prune decisions) and print a summary; do not touch the bucket. |

`validate` only runs schema + topology validation (§5–6), no key handling, no network calls.

## 5. Input configuration schema

```json
{
  "r2_config": {
    "endpoint": "https://<accountid>.r2.cloudflarestorage.com",
    "read_write_access_key_id": "",
    "read_write_secret_access_key": "",
    "region": "auto",
    "bucket": "wg-confs"
  },
  "nodes": [
    {
      "hostname": "master-us",
      "wg_config": {
        "tunnel_address": "10.10.0.1/32",
        "listen_port": 51820,
        "endpoint": "203.0.113.10:51820",
        "dns": "1.1.1.1",
        "mtu": 1420,
        "extra_allowed_ips": ["10.10.0.0/16"],
        "persistent_keepalive": null
      }
    },
    {
      "hostname": "master-eu",
      "wg_config": {
        "tunnel_address": "10.10.0.2/32",
        "listen_port": 51820,
        "endpoint": "198.51.100.20:51820"
      }
    },
    {
      "hostname": "workstation-01",
      "wg_config": {
        "tunnel_address": "10.10.0.100/32",
        "persistent_keepalive": 25
      }
    }
  ],
  "master": ["master-us", "master-eu"]
}
```

### Top-level fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `r2_config` | object | yes | Storage credentials — see below. |
| `nodes` | array | yes | See below. |
| `master` | string[] | yes | Hostnames of master nodes — see below. |

### `r2_config`

| Field | Type | Required | Notes |
|---|---|---|---|
| `endpoint` | string (URL) | yes | S3-compatible endpoint. Works with any S3-compatible provider, not just R2. |
| `read_write_access_key_id` | string | yes | Must have `PutObject`/`GetObject`/`ListBucket`/`DeleteObject` on `bucket`. |
| `read_write_secret_access_key` | string | yes | Secret half of the above. |
| `region` | string | yes | `"auto"` for R2; a real region for AWS S3/other providers. |
| `bucket` | string | yes | Destination bucket for `{hostname}.conf` objects. |

### `nodes[].wg_config`

| Field | Type | Required | Meaning |
|---|---|---|---|
| `tunnel_address` | string (CIDR, e.g. `10.10.0.1/32`) | yes | This node's address on the wg interface. Also the default route (`/32`) other nodes use to reach it as a peer. |
| `listen_port` | u16 | no (default `51820`) | UDP port wg listens on. |
| `endpoint` | string (`host:port`) | no | Publicly reachable address for this node. Omit for NAT'd/roaming nodes that only ever dial out (their peers won't get an `Endpoint =` line pointing here; WireGuard will still learn their address once they initiate a handshake). |
| `dns` | string | no | Written as `DNS =` in this node's own `[Interface]`. |
| `mtu` | u16 | no | Written as `MTU =` in this node's own `[Interface]`. |
| `extra_allowed_ips` | string[] (CIDR) | no | Additional routes advertised through this node when it appears as a peer (e.g. it's a gateway for a site subnet). Merged with `tunnel_address` in peers' `AllowedIPs =`. |
| `persistent_keepalive` | u16 or null | no | Seconds. If unset, `wg-server` defaults to `25` for any node whose `endpoint` is null (it's the side that needs the keepalive to hold a NAT mapping open), and omits the line otherwise. |

### `master`

Array of hostnames. Every entry must exist in `nodes`. Duplicates are a
validation error.

## 6. Hostname & topology validation

Run for both `apply` and `validate`, before any key handling:

- Hostname must match `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$` (DNS-label-safe,
  and safe as an S3 key / filename).
- Hostnames must be unique across `nodes`.
- `nodes` must be non-empty.
- Every entry in `master` must reference an existing hostname in `nodes`,
  and must not repeat.
- Warn (not error) if `master` is empty — no peer edges will be generated at
  all, which is very likely a mistake.
- `tunnel_address` values should not overlap (best-effort check: reject if
  two nodes have the exact same address; full CIDR-overlap checking is a
  nice-to-have, not required for v1).

## 7. Topology / peer-selection algorithm

Rules from the spec:

- Every master can reach every other node (master or not).
- Masters fully mesh with each other.
- Non-master nodes cannot reach each other directly — only masters.

This is a hub-and-spoke topology with a fully-meshed hub set:

```mermaid
graph LR
    subgraph Masters (full mesh)
        M1[master-us]
        M2[master-eu]
    end
    M1 --- M2
    M1 --- S1[workstation-01]
    M2 --- S1
    M1 --- S2[mobile-01]
    M2 --- S2
```

Edge set, pseudocode:

```
masters = set(config.master)
all_hosts = set(n.hostname for n in config.nodes)
spokes = all_hosts - masters

edges = {}
for (a, b) in combinations(masters, 2):        # master <-> master, full mesh
    edges.add(unordered_pair(a, b))
for m in masters:                              # master <-> every spoke
    for s in spokes:
        edges.add(unordered_pair(m, s))
# no spoke <-> spoke edges
```

Each edge becomes one `[Peer]` stanza in *both* endpoints' `.conf` files.

## 8. Key management

`wg-server` keeps no local database or key store between runs. The bucket
itself is the durable record of "what a node's current key is": on every
run, `wg-server` reads back whatever is already published and reuses it,
and only ever generates brand-new random key material for a node that
either has nothing published yet or was explicitly asked to be rotated.
There is nothing else to back up, protect, or lose track of beyond the
`.conf` objects that already have to exist for `wg-client` to consume.

### Key types

| Key | Algorithm | Scope | Source |
|---|---|---|---|
| Private key | Curve25519 (X25519), 32 random bytes, base64 | one per **node** | reused from the currently-published `{hostname}.conf` if present and not rotating; otherwise freshly generated via a CSPRNG |
| Public key | derived from private key | one per **node** | recomputed from whichever private key was used (reused or fresh) |
| Preshared key | 32 random bytes, base64 | one per **edge** (unordered pair) | reused from either endpoint's currently-published `.conf` (whichever already has a `[Peer]` stanza for the other) if present and neither endpoint is rotating; otherwise freshly generated via a CSPRNG |

Preshared keys are symmetric in usage (the same value is written into both
endpoints' `PresharedKey =` line), but there is nothing to derive — the
value is simply copied from wherever it already exists, or generated once
and written to both sides.

### Reuse and rotation rules

For each desired node (§7):

- If `--rotate` was passed bare, or `--rotate <hostname>` named this node:
  generate a fresh keypair. Ignore whatever is currently published.
- Else, if `{hostname}.conf` already exists in the bucket: parse its
  `[Interface] PrivateKey =` line and reuse it (recompute the public key
  from it).
- Else (brand-new node, nothing published yet): generate a fresh keypair.

For each desired edge `(a, b)`:

- If either `a` or `b` is being freshly keyed (per the rules above):
  generate a fresh preshared key for this edge. A node getting a new
  identity key should not keep sharing old preshared keys with its
  peers — this also naturally covers "new edge, never existed before"
  since a never-before-seen node is always freshly keyed.
- Else: look for an existing `[Peer]` stanza for `b` in `a`'s
  currently-published `.conf` (or for `a` in `b`'s) and reuse its
  `PresharedKey =` value.
- Else (neither side has a record of this edge — e.g. a spoke was just
  promoted to master, creating new mesh edges): generate a fresh preshared
  key.

This means an unrelated config change (e.g. editing one node's `dns`) never
perturbs any key material anywhere else in the fleet, with no separate
state to consult beyond the bucket contents `wg-server` already has to read
for the skip-if-unchanged upload check (§10) — reading existing objects
back serves both purposes in the same pass.

### `apply` algorithm

1. Compute the desired node set and edge set from `--config` (§7).
2. Fetch every currently-published `{hostname}.conf` for hostnames in the
   desired set (parallel `GetObject` calls; missing objects are not an
   error, just "nothing to reuse yet").
3. Resolve rotation targets: bare `--rotate` → all desired nodes;
   `--rotate <hostname>` → the named node(s) (error if any name isn't in
   the desired set).
4. For each desired node, resolve its keypair per the reuse/rotation rules
   above.
5. For each desired edge, resolve its preshared key per the reuse/rotation
   rules above.
6. Render `.conf` for every desired node (§9).
7. Upload changed files, skipping ones that are byte-identical to what was
   fetched in step 2 (§10). Anything freshly keyed will always differ and
   will always be uploaded — along with every peer that references it,
   since their rendered `AllowedIPs`/`PublicKey`/`PresharedKey` lines
   changed too.
8. `--prune`: delete `.conf` objects in the bucket for hostnames no longer
   in the desired set (via a `ListObjectsV2` pass — not related to the
   per-node fetches in step 2).

## 9. Rendered `wg-quick` format

Peers are emitted in sorted hostname order for stable, diff-friendly
output. Example for `master-us` (a master, so it peers with `master-eu` and
every spoke):

```ini
# Managed by wg-server — do not edit manually.
# Hostname: master-us

[Interface]
PrivateKey = <master-us private key>
Address = 10.10.0.1/32
ListenPort = 51820
DNS = 1.1.1.1
MTU = 1420

[Peer]
# master-eu
PublicKey = <master-eu public key>
PresharedKey = <master-us|master-eu psk>
AllowedIPs = 10.10.0.2/32
Endpoint = 198.51.100.20:51820

[Peer]
# workstation-01
PublicKey = <workstation-01 public key>
PresharedKey = <workstation-01|master-us psk>
AllowedIPs = 10.10.0.100/32
PersistentKeepalive = 25
```

Notes:

- No generation timestamp is embedded in the file — content is a pure
  function of resolved keys + topology, which keeps the skip-if-unchanged
  comparison in §10 meaningful (no line changes on every run just because
  time passed). Use the object's S3 `LastModified` metadata if "when was
  this last published" is needed.
- `Endpoint =` is only emitted for a peer if that peer's own `wg_config.endpoint`
  is set.
- `PersistentKeepalive =` is only emitted if the *peer* has no endpoint (it's
  the NAT'd side) or the peer explicitly set `persistent_keepalive`.
- `AllowedIPs =` is the peer's `tunnel_address` plus its `extra_allowed_ips`,
  comma-joined.

## 10. Upload to storage

- One `PutObject` per node, key `{hostname}.conf`, `Content-Type: text/plain; charset=utf-8`.
- Set `Cache-Control: no-store` on the object — if the bucket/endpoint is
  ever fronted by a CDN cache, clients must never get a stale copy.
- The `GetObject` fetches already done in §8 step 2 double as the
  skip-if-unchanged check: compare the freshly rendered content for a node
  to what was fetched for it; skip the `PutObject` call if identical.
- `--prune`: `ListObjectsV2` the bucket, and delete any `{hostname}.conf`
  object whose hostname is not in the current desired node set (§7).
- There is no cross-object transaction: if `apply` uploads 5 files and
  fails on the 3rd, the first two are already live. This is safe by
  design — an uploaded `.conf` for one node never depends on another
  node's `.conf` having already been uploaded (each file is
  self-contained), so a partial run just means some nodes haven't picked
  up the latest topology yet. Re-running `apply` is idempotent: nodes that
  already got their new key will simply be read back and reused on the
  retry, not re-rotated.
- On completion, print/log a summary: nodes uploaded, nodes skipped
  (unchanged), nodes pruned, nodes/edges freshly keyed.

## 11. Error handling

- Schema/topology validation errors (§6) abort before any key handling or
  network call — fail closed.
- A storage error on a given `GetObject`/`PutObject`/`DeleteObject` call is
  logged with the hostname and does not abort the rest of the run;
  `apply` exits non-zero at the end if anything failed, listing which
  hostnames still need a retry.
- If a currently-published `.conf` can't be parsed (corrupted object,
  manual edit, etc.) while trying to reuse its key, treat that node as if
  nothing were published — generate a fresh keypair for it, log a warning
  naming the hostname so the operator knows a rotation happened
  incidentally, not because it was requested.
- Because `wg-server` keeps no state of its own, a crash or interrupted run
  leaves nothing to reconcile beyond the bucket's own contents — simply
  re-run `apply`.

## 12. Security considerations

- The input config file contains **read-write** bucket credentials —
  treat it like any other secret (inject via env/secrets manager in CI,
  restrict file permissions, never commit it).
- Read-only client credentials (see `wg-client`) are typically scoped to
  the whole bucket by the storage provider, not to individual objects. That
  means any client holding valid read-only credentials can, in principle,
  download *any* hostname's `.conf` — including nodes it should not be able
  to reach at the network layer — and obtain that node's private key and
  preshared keys. This is a **residual risk** of the flat `{hostname}.conf`
  naming scheme, not something `wg-server` can fix by itself. Mitigations,
  if the storage provider supports them:
  - Issue one read-only API token per node, scoped to a `GET` on a single
    object or a per-node prefix (e.g. rename objects to
    `{hostname}/wg0.conf` so a token can be scoped to that prefix, if the
    provider supports prefix-scoped tokens).
  - Alternatively, encrypt each `.conf` at rest with a per-node key
    distributed out of band before upload — listed under Future
    Enhancements (§15) since it introduces its own key-distribution
    problem and is out of scope for v1.
- Always use `https://` endpoints (the SDK will refuse to send credentials
  over plaintext HTTP by default — do not override that).
- Because reuse is read back from the bucket itself, anyone with
  **write** access to the bucket could plant a malicious `.conf` for a
  node ahead of an `apply` run and have `wg-server` treat its (attacker-
  chosen) `PrivateKey` as legitimate to reuse. This is not a new exposure
  in practice — anyone with write access to the bucket could simply upload
  a malicious `.conf` directly and skip `wg-server` entirely — but it's
  worth stating plainly: the read-write credentials in `--config` are the
  actual trust boundary for the whole fleet, not `wg-server`'s logic.

## 13. Logging & observability

- Structured logs (`tracing`), one line per hostname processed
  (`reused` / `rotated` / `uploaded` / `skipped-unchanged` / `pruned`),
  plus a final summary line.
- `--dry-run` prints the same plan without the "uploaded" / "pruned"
  side effects, for review in a PR/CI pipeline before an operator approves
  the real `apply`.

## 14. Dependencies (planned crates)

| Crate | Purpose |
|---|---|
| `clap` | CLI parsing |
| `serde`, `serde_json` | Config (de)serialization |
| `tokio` | Async runtime (required by the S3 SDK) |
| `aws-sdk-s3`, `aws-config` | S3-compatible storage client, with custom endpoint + `auto`/path-style support for R2 |
| `x25519-dalek`, `rand` | X25519 keypair generation and random preshared keys |
| `base64` | Key encoding (WireGuard's on-the-wire key format) |
| `sha2` | Content-hash comparison for skip-if-unchanged uploads |
| `thiserror` / `anyhow` | Error types |
| `tracing`, `tracing-subscriber` | Logging |

## 15. Future enhancements

- Scheduled/automated key rotation (e.g. quarterly `apply --rotate` run)
  driven by CI cron.
- Per-node scoped storage tokens generated and rotated by `wg-server`
  itself, if the provider exposes a token-management API.
- Optional per-node encryption of `.conf` contents at rest.
- IPv6 tunnel addresses / dual-stack `AllowedIPs`.
- Webhook/notification on publish (e.g. notify a chat channel of topology
  changes).
- Terraform/Ansible provider wrapping `wg-server apply` for GitOps-style
  fleet management.
