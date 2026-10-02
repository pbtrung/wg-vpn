# `wg-server` — Design Document

Status: design only, no implementation yet.

> **Encrypted, signed publication (discovery schema v2) is designed but
> not implemented yet.** Sections that describe per-node age encryption,
> the signed `current.json` envelope, `publication_keys`,
> `nodes[].age_recipient`, `keygen`, and `restore-previous` (§4–§5, §8,
> §10–§12) are the target design. The current code still implements
> schema v1: plaintext `.conf` objects and an unsigned pointer. See
> [§10 "Encryption and signing"](#encryption-and-signing-schema-v2) and
> "Migrating from schema v1" for the plan.

## 1. Purpose

`wg-server` is a control-plane CLI tool run by an ephemeral cloud cron job
against a JSON topology file **daily at 00:00**.
The runner has no persistent filesystem, database, or local key store.
The storage bucket holds all durable publication state. The tool:

1. Validates the topology (unique hostnames, valid master list).
2. Generates WireGuard key material (private/public keypair per node,
   preshared key per allowed connection) — all randomly generated, never
   supplied by the operator.
3. Renders one `.conf` file per node in `wg-quick`-compatible INI syntax,
   encoding the correct set of peers for that node. This is purely an
   interchange/storage format — `wg-client` never runs it through
   `wg-quick`; it parses the file and pushes the result to the kernel
   directly over netlink/UAPI (see
   [wg-client.md §1](./wg-client.md#1-purpose)).
4. Encrypts each node's file to that node's own post-quantum hybrid age
   recipient (plus the server's own recipient, so later runs can reuse
   keys), uploads a complete immutable generation to an S3-compatible
   bucket (Cloudflare R2 in the reference deployment), then publishes it
   through a signed `current.json` (§10). The bucket holds only
   ciphertext and signed metadata, never plaintext key material.
   Independent `wg-client` jobs pull the latest
   generation **daily at 00:30** (see
   [docs/wg-client.md](./wg-client.md)).

`wg-server` never touches a live WireGuard interface itself — it only
produces and publishes configuration.

Use the same scheduler timezone for both jobs; these examples use UTC.
The nominal client slots are thirty minutes after the server slots. This
gives a healthy server run time to publish before the next client pass,
while both jobs still execute independently.

Keep the current generation and **one previous committed generation**.
Unchanged runs create no generation. Clients may skip generations and do
not acknowledge publication; neither publication nor retention waits for
them. A configuration or key change takes effect on each node at its next
successful sync, so publication does not imply simultaneous activation.

The cloud scheduler must serialize server jobs, including retries, manual
invocations, and retention cleanup. Skip or queue an overlapping trigger;
start a successor only after the previous process has terminated. This
coordination is provided by the scheduler, with no local lock or persistent
runner state. Conditional publication (§10) provides an additional conflict
check, but cannot by itself prevent concurrent cleanup from deleting an
active uploader's files.

## 2. Non-goals

- Does not manage the wg interface on any host (that's `wg-client`).
- Does not provision the storage bucket or IAM/API tokens — those are
  created by the operator ahead of time (see §12, Security considerations).
- Does not rotate keys on a timer; rotation is a manual, operator-triggered
  action (see §8, Key management).
- Does not provide full-tunnel/default-route VPN service, arbitrary shell
  hooks (`PreUp`/`PostUp`-style directives — rejected outright regardless
  of applier, see [wg-client.md §6](./wg-client.md#6-sync-algorithm)), or
  automatic gateway firewall/NAT configuration in v1. Gateway forwarding
  and return routes are operator prerequisites.

## 3. Project layout (proposed workspace)

```
wg-vpn/
├── Cargo.toml         # workspace root
├── wg-common/         # shared: S3 client wrapper, wg-quick-format renderer/
│                      # parser (storage format only, not an applier),
│                      # config types, error types
├── wg-server/         # this binary
├── wg-client/         # sibling binary — docs/wg-client.md
└── docs/
    ├── wg-server.md
    └── wg-client.md
```

## 4. CLI

```
wg-server apply    --config <path> [--rotate [<hostname>]]... [--dry-run] [--prune] [--migrate-v1]
wg-server validate --config <path>
wg-server keygen
wg-server restore-previous --config <path> [--dry-run]
```

| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | required | Path to the topology JSON (§5). Falls back to the `WG_SERVER_CONFIG` environment variable if omitted. |
| `--rotate [<hostname>]` | none | Force a brand-new random key for the given node(s) instead of reusing what's currently published (§8). Two forms: `--rotate <hostname>` (repeatable) rotates only the named node(s); bare `--rotate` with no value rotates **every** node in `--config`. Passing both a bare `--rotate` and one or more `--rotate <hostname>` in the same invocation is a validation error — pick one form. Implies `--prune` (see below). |
| `--dry-run` | off | Read published state and compute the plan (key reuse/rotation, generation publication, retention cleanup); print a summary without secrets. Perform no bucket writes or deletes. |
| `--prune` | off | Skip the retention grace period (§10) and delete anything outside `current`/`previous` immediately, instead of waiting the usual ~15 minutes. Automatically implied by `--rotate` in either form — see below. |

| `--migrate-v1` | off | One-time upgrade from an unsigned schema v1 pointer with plaintext `.conf` generations to schema v2 (§10 "Migrating from schema v1"). Without it, `apply` refuses a v1 pointer. Implies `--prune`. |

`validate` only runs schema + topology validation (§5–6), including
parsing `publication_keys` and every `age_recipient`, with no network calls.

`keygen` makes no network call and reads no config. It generates a fresh
server age identity (`mlkem768x25519`) and a fresh hybrid signing key
pair (Ed25519 + ML-DSA-87), and prints one JSON object to stdout. The
object holds the `publication_keys` block for this config (§5), plus the
derived `server_recipient` and `server_verify_key` for operators to
distribute. `server_verify_key` is an object holding both public keys;
it goes into every client's `server_verify_keys` (see
[wg-client.md §4](./wg-client.md#4-client-configuration-schema)). Run it
with a restrictive umask and redirect stdout straight into the secret
store; it never writes a file itself.

`restore-previous` is the signed replacement for hand-editing the pointer
during recovery (§11). It verifies the current pointer's signature,
decrypts and validates every file of the retained `previous` generation,
and then conditionally writes a new signed pointer:
`current` = the old `previous`, `previous: null`, a fresh `revision`, and
a new `sequence` (§10). `--dry-run` reports what it would restore without
writing.

The normal server command is `wg-server apply --config <path>`, on cloud
cron expression `0 0 * * *` in UTC.
It reuses existing keys; scheduling `apply` does not schedule rotation.
Retention cleanup is automatic and always runs, but by default waits out
the grace period described in §10. `--rotate` always forces immediate
cleanup (as if `--prune` were also given): rotation is already a
deliberate, manual/operator-triggered action, never part of the routine
unattended cron invocation, so there is no routine-job race for the grace
period to protect against, and there is no reason to leave a just-rotated
generation's old (possibly compromised) key sitting around for the
grace period. `--prune` is also available standalone (e.g. to force
immediate cleanup of a plain, non-rotating `apply`, such as when
testing/iterating and certain no concurrent apply is running). Node
removal takes effect by omitting the node and its peer edges from the next
generation, while the previous generation remains available under the
same one-generation retention policy.

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
  "publication_keys": {
    "age_identity": "AGE-SECRET-KEY-PQ-1...",
    "retired_age_identities": [],
    "signing_keys": {
      "ed25519": "<Base64 32-byte Ed25519 seed>",
      "ml_dsa_87": "<Base64 32-byte ML-DSA-87 seed>"
    }
  },
  "nodes": [
    {
      "hostname": "master-us",
      "age_recipient": "age1pq1...",
      "wg_config": {
        "tunnel_address": "10.10.0.1/32",
        "listen_port": 51820,
        "endpoint": "203.0.113.10:51820",
        "dns": "1.1.1.1",
        "mtu": 1420,
        "extra_allowed_ips": ["10.20.0.0/16"],
        "persistent_keepalive": null
      }
    },
    {
      "hostname": "master-eu",
      "age_recipient": "age1pq1...",
      "wg_config": {
        "tunnel_address": "10.10.0.2/32",
        "listen_port": 51820,
        "endpoint": "198.51.100.20:51820"
      }
    },
    {
      "hostname": "workstation-01",
      "age_recipient": "age1pq1...",
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
| `publication_keys` | object | yes | Server age identity and pointer-signing keys — see below. |
| `nodes` | array | yes | See below. Each entry has `hostname`, `age_recipient`, and `wg_config`. |
| `master` | string[] | yes | Hostnames of master nodes — see below. |

### `r2_config`

| Field | Type | Required | Notes |
|---|---|---|---|
| `endpoint` | string (URL) | yes | HTTPS S3-compatible endpoint. The provider must support the consistency and conditional-write requirements in §10. Reject a non-`http`/`https` scheme, URL userinfo, query strings, and fragments. HTTPS-only rejection of plain `http://` is not yet enforced — see §12. |
| `read_write_access_key_id` | string | yes | Must have `PutObject`/`GetObject`/`ListBucket`/`DeleteObject` on `bucket`. |
| `read_write_secret_access_key` | string | yes | Secret half of the above. |
| `session_token` | string | no | Required when the supplied access-key pair represents temporary credentials. Load fresh credentials for every cron invocation. |
| `region` | string | yes | `"auto"` for R2; a real region for AWS S3/other providers. |
| `bucket` | string | yes | Bucket containing `current.json` and `nodes/` (§10). Reserve these names exclusively for this fleet. |

### `publication_keys`

All three values are secrets. Supply them the same way as the read-write
credentials (§12). `wg-server keygen` (§4) generates them.

| Field | Type | Required | Notes |
|---|---|---|---|
| `age_identity` | string | yes | The server's own age identity: `AGE-SECRET-KEY-PQ-1…` (the `mlkem768x25519` type, §10). Every published file is also encrypted to its recipient, so the next `apply` can decrypt the current generation and reuse its keys (§8). |
| `retired_age_identities` | string[] | no (default `[]`) | Earlier server identities. Used **only to decrypt** the current generation while the server identity is being rotated; never used to encrypt. At most 2 entries. Remove an entry once a run under the new identity has committed. |
| `signing_keys.ed25519` | string | yes | Canonical Base64 of a 32-byte Ed25519 secret seed (RFC 8032). |
| `signing_keys.ml_dsa_87` | string | yes | Canonical Base64 of the 32-byte ML-DSA-87 key-generation seed ξ (FIPS 204 `ML-DSA.KeyGen_internal`). The expanded secret key is derived in memory and never stored. |

The two signing keys always work as a pair. Every `current.json` write
carries both signatures (§10), and the pair's two public keys together
are what clients pin as one `server_verify_keys` entry. They are rotated
together, never independently.

### `nodes[].age_recipient`

| Field | Type | Required | Notes |
|---|---|---|---|
| `age_recipient` | string | yes | This node's age recipient, `age1pq1…` (`mlkem768x25519` only). Each node generates its own identity locally (`wg-client keygen`, [wg-client.md §3](./wg-client.md#3-cli)); only this public half is ever given to the server. |

### `nodes[].wg_config`

| Field | Type | Required | Meaning |
|---|---|---|---|
| `tunnel_address` | string (IPv4 CIDR, e.g. `10.10.0.1/32`) | yes | Unique IPv4 unicast address with an explicit `/32` prefix. Written to this node's interface and advertised as a host route to its peers. |
| `listen_port` | integer | no (default `51820`) | UDP listening port, `1`–`65535`; randomized port `0` is unsupported. |
| `endpoint` | string or null | no | Reachable `IPv4:port`, `DNS-name:port`, or `[IPv6]:port`; port is `1`–`65535`. Omit/null for NAT'd nodes that initiate outbound handshakes. IPv6 transport is allowed even though v1 tunnel addresses are IPv4. |
| `dns` | string | no | One resolver IP address, parsed and rendered canonically as `DNS =` in this node's own interface. No arbitrary text or search-domain directives. |
| `mtu` | integer | no | `576`–`65535`, additionally limited by the host/underlay. Omit to leave the interface at `wg-client`'s platform default MTU (there is no `wg-quick`-style automatic path-MTU probing once the interface is configured directly over netlink); setting it is an operator choice, not proof that the path supports it. |
| `extra_allowed_ips` | string[] (IPv4 CIDR) | no (default `[]`) | Canonical site-subnet routes through this node. Subject to the non-overlap and default-route restrictions in §6. |
| `persistent_keepalive` | integer or null | no | Local node's outbound keepalive interval, emitted in its own peer stanzas. `1`–`65535` seconds enables it; explicit `0` disables it. Missing/null defaults to `25` if the local endpoint is missing/null, and omits the line otherwise. |

### `master`

Array of hostnames. Every entry must exist in `nodes`. Duplicates are a
validation error.

## 6. Hostname & topology validation

Run for both `apply` and `validate`, before any key handling or network call:

- Parse JSON strictly: reject unknown fields, duplicate object keys,
  invalid types/ranges, empty required credentials, and invalid URLs.
  Missing optional fields use their documented defaults; only fields
  explicitly permitting null accept it. Reject CR, LF, NUL, and other
  control characters in all rendered string values. Render typed values,
  never concatenate raw operator text as config directives.
- Hostname must match `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$` (DNS-label-safe,
  and safe as an S3 key / filename).
- Every `age_recipient` must parse as a canonical `mlkem768x25519`
  recipient (`age1pq1…`). Reject every other recipient type, including
  classic X25519 `age1…`, SSH, and plugin recipients. age says a file
  SHOULD NOT mix post-quantum and non-post-quantum recipients (§10), and
  this fleet allows only the hybrid type. Reject duplicate recipients
  across nodes, since a shared identity would let one node decrypt
  another's file. Also reject any node recipient equal to the server's own.
- `publication_keys.age_identity` and each `retired_age_identities` entry
  must parse as `mlkem768x25519` identities with distinct recipients.
  Each of `signing_keys.ed25519` and `signing_keys.ml_dsa_87` must decode
  as canonical Base64 to exactly 32 bytes.
- Hostnames must be unique across `nodes`.
- `nodes` must be non-empty.
- Every entry in `master` must reference an existing hostname in `nodes`,
  and must not repeat.
- Warn (not error) if `master` is empty — no peer edges will be generated at
  all, which is very likely a mistake.
- Parse `tunnel_address` as an IPv4 unicast `/32`, and compare numeric IPs
  for uniqueness. Reject broader prefixes and IPv6 tunnel addresses in v1.
- Parse each extra route as a canonical IPv4 network. Reject host bits
  outside its prefix, duplicate/overlapping extra routes, `0.0.0.0/0`, and
  any extra route covering a node's tunnel address. Each advertised subnet
  has exactly one gateway; overlapping advertisements and automatic
  gateway failover are outside v1. Recheck disjoint prefix ownership on
  every rendered interface so peer selection cannot introduce ambiguity.
- Every generated edge must have a configured endpoint on at least one
  side. Hostname/port syntax is validated offline; DNS resolution, UDP
  reachability, forwarding, and NAT mappings are deployment checks (§7).
- The shared limits are 4096 nodes, 16 MiB for topology/discovery JSON,
  and 1 MiB for each rendered configuration. Each is an inclusive maximum:
  exactly 4096 nodes, a discovery JSON of exactly 16 MiB, or a rendered
  file of exactly 1 MiB is valid; the first byte/node past that is what's
  rejected. MiB means `1024 × 1024` bytes. Enforce limits before
  publication, so clients can use the same bounded parsers/downloads.
- A master's file holds one `[Peer]` stanza per *other* node in the fleet
  (§7), so its size scales with total node count, not master count: at
  roughly 200–300 bytes per stanza (more with long hostnames or
  `extra_allowed_ips`), a fleet near the 4096-node limit can push a
  master's own rendered file close to or past the 1 MiB cap even with as
  few as one master. Enforce the 1 MiB cap per rendered file as a hard
  `apply`-time validation error naming the offending hostname and its
  rendered size — never truncate or upload an oversized file. Operators
  approaching this limit should keep the number of nodes any single
  master must peer with well under the level implied by 1 MiB ÷
  (bytes per stanza), rather than relying on the nominal 4096-node ceiling
  in isolation.

## 7. Topology / peer-selection algorithm

Rules from the spec:

- Every master has a direct WireGuard peer relationship with every other
  node (master or not), subject to endpoint and host-network prerequisites.
- Masters fully mesh with each other.
- Non-master nodes cannot reach each other directly — only masters.

This is a hub-and-spoke topology with a fully-meshed hub set:

```mermaid
graph LR
    subgraph masters_group["Masters (full mesh)"]
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

edges = set()
for (a, b) in combinations(masters, 2):        # master <-> master, full mesh
    edges.add(unordered_pair(a, b))
for m in masters:                              # master <-> every spoke
    for s in spokes:
        edges.add(unordered_pair(m, s))
# no spoke <-> spoke edges
```

Each edge becomes one `[Peer]` stanza in *both* endpoints' `.conf` files.

Peering is not a firewall policy. For a site gateway, the operator must
enable forwarding, permit the intended traffic, and install return routes
or appropriate NAT on the site network. Hosts must permit the WireGuard
UDP ports, and NAT'd nodes must initiate traffic toward a reachable peer.
Any spoke-to-spoke forwarding policy must be enforced explicitly on hubs;
absence of a direct spoke edge is not itself traffic isolation.

Storage access, endpoint DNS, and any configured resolver must remain
reachable over the node's underlying network when WireGuard is stopped
or peers have mismatched keys. Avoid advertising networks that would
capture this traffic. Clients check host-specific route conflicts before
application (§6 of wg-client); static topology validation cannot verify
every node's local LAN or changing DNS answers.

## 8. Key management

`wg-server` keeps no local database or key store between runs. On every
run it reads `current.json` and verifies both of its signatures against
its own `signing_keys` (§10). It then gets reusable keys by decrypting the
complete generation named by `current` with its own age identity (or a
retired one, §5). The signed pointer and retained `.conf.age` objects are
the only durable state; an interrupted job's memory or temporary files are
never needed for recovery. The `publication_keys` secrets come from the
job's secret store on every run, the same as the storage credentials, so
they add no local state either.

A hostname absent from the current generation is a new node. An object
listed in that generation that is missing, corrupt, undecryptable, or
unparsable is a state-integrity error, not permission to silently generate
a new identity. A pointer whose signature does not verify is also an
integrity error, as is one with an unsupported schema version (except a
v1 pointer under `--migrate-v1`, §10).
Abort and report the affected node. Only a new node or an explicit
`--rotate` request gets freshly generated identity keys.

### Key types

| Key | Algorithm | Scope | Source |
|---|---|---|---|
| Private key | X25519 scalar generated from 32 CSPRNG bytes and clamped as required by WireGuard, Base64 | one per **node** | reused from the node's decrypted `.conf.age` in the current generation if registered and not rotating; otherwise freshly generated |
| Public key | derived from private key | one per **node** | recomputed from whichever private key was used (reused or fresh) |
| Preshared key | 32 CSPRNG bytes, Base64 | one per **edge** (unordered pair) | reused from both endpoints' matching current stanzas if neither endpoint is rotating; otherwise generated once per edge |
| Node age identity | `mlkem768x25519` (ML-KEM-768 + X25519 hybrid) | one per **node** | generated **on the node** (`wg-client keygen`); the server only ever sees its recipient (`nodes[].age_recipient`) and never generates, stores, or publishes it |
| Server age identity | `mlkem768x25519` | one per **fleet** | `publication_keys.age_identity`, generated by `wg-server keygen` and supplied as a job secret |
| Pointer signing key pair | Ed25519 + ML-DSA-87 (hybrid; both signatures required) | one pair per **fleet** | `publication_keys.signing_keys`, generated by `wg-server keygen` and supplied as a job secret |

The age and signing keys are long-lived configuration, not per-run key
material. `--rotate` never touches them; rotating them is the staged,
operator-driven procedure in "Rotating publication keys" below.

Preshared keys are symmetric in usage (the same value is written into both
endpoints' `PresharedKey =` line), but there is nothing to derive — the
value is reused after consistency validation, or generated once and
written to both sides. Validate canonical Base64 decoding to exactly 32
bytes, reject zero keys (which can disable key/PSK settings), and reject
duplicate node public keys. Use the same key validation in both binaries.

### Reuse and rotation rules

For each desired node (§7):

- If `--rotate` was passed bare, or `--rotate <hostname>` named this node:
  generate a fresh keypair after validating published state.
- Else, if the node is listed in the current generation: parse its
  `[Interface] PrivateKey =` line and reuse it (recompute the public key
  from it).
- Else (node absent from the current generation): generate a fresh keypair.

For each desired edge `(a, b)`, first validate its existing records when
both endpoints appear in the current generation. Derive their old public
keys from their current private keys, even if rotation is requested.
Match peers by those public keys, not hostname comments or mutable
addresses. If both reciprocal stanzas exist, require the same valid PSK.
A one-sided edge, conflicting PSKs, or duplicate peer public keys is an
integrity error; abort instead of choosing one endpoint arbitrarily.
Rotation must not hide inconsistent source state.

Then select the edge's PSK:

- If either endpoint is being freshly keyed, generate one fresh PSK for
  both endpoints. New identities must not reuse old PSKs.
- Otherwise, reuse the validated reciprocal PSK when the edge exists.
- If neither side records this edge, generate a fresh PSK. For example,
  promoting an existing spoke to master can create new edges without
  changing either endpoint's identity key.

This means an unrelated config change (e.g. editing one node's `dns`) never
perturbs any key material anywhere else in the fleet, with no separate
state to consult beyond the bucket contents `wg-server` already has to read
for the skip-if-unchanged upload check (§10) — reading existing objects
back serves both purposes in the same pass.

### Rotating publication keys

None of these steps require fleet downtime. Each is a sequence of normal
`apply` runs plus out-of-band config pushes.

- **A node's age identity** (e.g. the node was reimaged, or its identity
  file leaked):
  1. Generate a new identity on the node.
  2. Add it to the node's `age_identities` *alongside* the old one
     ([wg-client.md §4](./wg-client.md#4-client-configuration-schema)).
  3. Replace that node's `age_recipient` in the topology and run `apply`.
     The recipient fingerprint changes (§10), so this publishes a new
     generation even when nothing else changed.
  4. Once the node has applied it, remove the old identity from the
     node's config.

  If the old identity leaked, also `--rotate` that node. Its WireGuard
  private key and PSKs were readable to whoever held the old identity.
- **The server age identity**:
  1. Move the old value into `retired_age_identities`, put a fresh one in
     `age_identity`, and run `apply`. Recipient sets change, so the run
     re-encrypts every file to the new server recipient.
  2. After that commit, drop the retired entry.

  The retired identity also stays able to decrypt the retained
  `previous` generation, which still names the old recipient. Removing it
  is what retires that copy from the server's view; the objects
  themselves go at the next cleanup that drops them.
- **The signing key pair** (both halves together):
  1. Add the new verify-key entry to every client's `server_verify_keys`
     and wait until all clients have it.
  2. Switch `signing_keys` to the new pair and run `apply`. This forces a
     pointer write but not a new generation.
  3. Remove the old entry from clients.

  Changing the signing keys before every client trusts the new entry
  leaves those clients rejecting every pointer until they do. They fail
  closed: their tunnel stays up.
- **Loss of the server age identity** with no retired copy means the
  current generation can't be decrypted, so its keys can't be reused.
  Recover with bare `--rotate`, which rekeys the whole fleet. The
  integrity pass that normally decrypts existing records first (§8 above)
  is skipped only in this case, and the run reports it explicitly as
  `reuse=unavailable-rotating-all`.

### `apply` algorithm

1. Validate input and rotation targets before any network call. Compute
   the desired node set and edge set (§7).
2. Fetch `current.json`, retaining its ETag for conditional publication.
   Verify its signature and validate its payload (§10). Initialize an
   empty signed pointer for a new fleet as described in §10; never infer
   published state from orphan uploads. A dry run only plans
   initialization.
3. Fetch the current generation's files for desired nodes already listed
   there, with bounded parallelism. Verify their SHA-256 digests (over the
   ciphertext), decrypt them with the server identity, and only then parse
   keys. Nodes not listed there have no reusable current identity.
4. Resolve node keypairs and edge preshared keys using the rules above.
5. Render every desired node's complete plaintext configuration (§9).
   age ciphertext is randomized, so compare plaintext, never ciphertext.
   A node is unchanged only when all of these match the current generation:
   - its decrypted plaintext bytes;
   - its recipient fingerprint (§10);
   - the server recipient its file was encrypted to.

   If the desired hostname set and every node match, keep the
   current/previous generation entries unchanged and proceed to the
   conditional revision refresh and retention cleanup in §10. No new
   generation is created.
6. Otherwise, generate a unique generation ID. Encrypt **every** desired
   node's configuration to exactly two recipients — that node's
   `age_recipient` and the server's own — and upload each under the new
   ID, including nodes whose plaintext didn't change. Each generation is
   self-contained; it never depends on files belonging to an older
   generation.
7. After every upload succeeds, conditionally replace `current.json` with
   a newly signed pointer naming the new generation as `current` and the
   former `current` as `previous` (§10). On failure, leave the published pointer untouched or reconcile
   an uncertain result before doing any cleanup.
8. Automatically remove generations outside `current` and `previous`,
   including abandoned uploads from terminated jobs (§10). This also runs
   on unchanged passes. `--dry-run` reports these decisions without
   executing steps 6–8's writes or deletes.

## 9. Rendered configuration format (`wg-quick`-compatible INI)

This section describes the storage/interchange syntax only. Nothing in
this repo executes it through `wg-quick`; `wg-client` parses it with
`wg-common` and applies it directly to the kernel over netlink/UAPI (see
[wg-client.md §1](./wg-client.md#1-purpose) and
[§6](./wg-client.md#6-sync-algorithm)). It stays `wg-quick`-compatible
syntax anyway: it's a well-understood, human-readable INI format, and
keeping it lets an operator inspect a downloaded file — once decrypted
with the node's or server's identity, e.g. Go `age -d` — with any
existing WireGuard tooling if they ever need to.

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
```

In `workstation-01`'s own configuration, its entry for `master-us` is:

```ini
[Peer]
# master-us
PublicKey = <master-us public key>
PresharedKey = <workstation-01|master-us psk>
AllowedIPs = 10.10.0.1/32, 10.20.0.0/16
Endpoint = 203.0.113.10:51820
PersistentKeepalive = 25
```

The workstation also sends keepalives to `master-eu`. The masters do not
inherit the workstation's keepalive setting: the NAT'd node sends packets
to maintain its own mapping
([WireGuard guidance](https://www.wireguard.com/quickstart/#nat-and-firewall-traversal-persistence)).

Notes:

- No generation timestamp is embedded in the file — content is a pure
  function of resolved keys + topology, which keeps the skip-if-unchanged
  comparison in §10 meaningful (no line changes on every run just because
  time passed). Use the object's S3 `LastModified` metadata if "when was
  this last published" is needed.
- `Endpoint =` is only emitted for a peer if that peer's own `wg_config.endpoint`
  is set.
- `PersistentKeepalive =` is determined from the **local node** and emitted
  in each of that node's peer stanzas using §5's default/override rules.
- `AllowedIPs =` is the peer's `tunnel_address` plus its `extra_allowed_ips`,
  comma-joined.
- Render UTF-8 with LF line endings and a final newline. Sort peers by
  hostname and extra routes by numeric network/prefix; normalize addresses,
  key encodings, and optional defaults for deterministic comparison.
- Emit only the directive subset accepted by wg-client §6. Never generate
  `PreUp`, `PostUp`, `PreDown`, `PostDown`, `SaveConfig`, or arbitrary shell
  text. Validate every rendered file with the shared parser before upload.
- This plaintext is what gets encrypted (§10); it never reaches the
  bucket unencrypted. The 1 MiB per-file limit (§6) applies to this
  plaintext. Self-validation runs on the plaintext before encryption. As
  an extra check, the server decrypts each ciphertext it produced with its
  own identity and compares the result byte-for-byte before uploading.

## 10. Upload to storage

### Storage layout and generation discovery

The configured bucket has two reserved names:

```text
current.json
nodes/<hostname>/<generation-id>.conf.age
```

`current.json` is the fixed discovery object fetched by both the server
and every client. It is a signed envelope (see "Encryption and signing"
below) around a payload. The payload contains the current generation and
at most one previous committed generation. Each generation entry contains:
- the complete hostname set;
- for each node, the SHA-256 digest of its uploaded ciphertext and the
  fingerprint of the recipient that file was encrypted to;
- the fingerprint of the server recipient used for that generation.

The stored object:

```json
{
  "schema_version": 2,
  "payload": "<canonical Base64 of the exact payload JSON bytes>",
  "signatures": {
    "ed25519": "<canonical Base64 of a 64-byte Ed25519 signature>",
    "ml_dsa_87": "<canonical Base64 of a 4627-byte ML-DSA-87 signature>"
  }
}
```

with a decoded payload such as:

```json
{
  "schema_version": 2,
  "sequence": 1790812800123,
  "revision": "0eadndcbb7ved8xd0x34e6229z4ecbhqd5nmr6krre4bk4kp32xm",
  "current": {
    "id": "189c8r7s5x4dcvhq7acet5dr0zpc592e13rxd8xgk3nqe1hxj9d4",
    "server_recipient": "1d0k7ckr5pwsg4ab3nvkfh2c3y8a91yq2g1wq4nqk4m7ezm5bfa0",
    "nodes": {
      "master-us": {
        "digest": "17670h05q0z785jbt19b4vsyqrggq6rkna7s21764kzas2p5dgs2",
        "recipient": "0w2k6q5d7xk1s9d1c4e5nm0cg9ah1x8w3c1rzm6k3t7de8v2yy0s"
      },
      "workstation-01": {
        "digest": "0m205js6jn658mf9j5y9k9wee9yvgv14bsdmt4ne8k2nexyx10j1",
        "recipient": "1ac4f5r2y9v6mx0t8s3hq4e7nm2b5kd1c7a4w9zg3pj6r8t0e2mn"
      }
    }
  },
  "previous": {
    "id": "0j530zh3377p9035zg0jbex7hpf30g8zmt476nwxr0g41kjs31nf",
    "server_recipient": "1d0k7ckr5pwsg4ab3nvkfh2c3y8a91yq2g1wq4nqk4m7ezm5bfa0",
    "nodes": {
      "master-us": {
        "digest": "0eadndcbb7ved8xd0x34e6229z4ecbhqd5nmr6krre4bk4kp32xm",
        "recipient": "0w2k6q5d7xk1s9d1c4e5nm0cg9ah1x8w3c1rzm6k3t7de8v2yy0s"
      }
    }
  }
}
```

The IDs, digests, and fingerprints above are illustrative. A recipient
fingerprint is SHA-256 over the canonical recipient string's ASCII bytes
(`age1pq1…`), in the encoding below. Generate each ID from **32
CSPRNG bytes**. Compute each configuration digest using **SHA-256 over the
exact uploaded bytes**, yielding 32 digest bytes. Both use the same text
encoding: interpret the bytes as an unsigned big-endian 256-bit integer,
encode in **lowercase Crockford Base32**, and left-pad with `0` to exactly
**52 characters**. Use alphabet `0123456789abcdefghjkmnpqrstvwxyz`, with
no separators, `=` padding, or check symbol. The four unused high bits are
zero, so the first character is `0` or `1`. The canonical validation rule
for both IDs and digests is `^[01][0-9abcdefghjkmnpqrstvwxyz]{51}$`.
Reject uppercase and the usual human-input aliases (`i`, `l`, `o`). This
uses the [Crockford alphabet and numeric
encoding](https://www.crockford.com/base32.html) with lowercase output.

**This is not RFC 4648 Base32.** RFC 4648 groups input into fixed 5-byte
blocks and encodes each block's bits independently (with `=` padding on a
short final block); Crockford's encoding treats the entire input as one
big-endian integer. For a 32-byte input the two schemes produce
**different, same-length strings** for the same bytes. A stock Base32
crate (e.g. Rust's `base32` or `data-encoding`, which implement RFC 4648)
will silently produce the wrong value if reached for here — this
encoding must be implemented as a big-integer conversion (e.g. via
`num-bigint`, or a manual repeated divide-by-32 loop) in the shared
`wg-common` module, not via a generic Base32 dependency, with unit tests
that pin its output against known values derived from this specification
(see [milestones.md](./milestones.md)) rather than trusting a generic crate.

Use this encoding for **all application-defined digests**, including
publication metadata, recipient fingerprints, logged hashes, and clients'
persisted applied hashes. Hash computation remains deterministic; generation IDs and pointer
revisions are random. Storage ETags are opaque provider values and must be passed back
unchanged, and WireGuard keys retain their required Base64 format. IDs
contain no timestamp and must never be sorted to discover the latest
generation.

`revision` is a fresh 32-byte CSPRNG nonce using the same canonical
encoding as generation IDs. Change it on every pointer mutation, including
initialization, recovery, and the refresh before unchanged-pass cleanup.
It fences delayed conditional writes from earlier invocations without
creating another configuration generation. Clients use `current.id` for
discovery; a revision-only update does not trigger local application.

`sequence` is a nonnegative JSON integer no larger than `2^53 − 1`. Every
pointer write sets it to `max(old.sequence + 1, now_unix_millis)`, where
initialization uses `now_unix_millis` alone. That covers publication,
revision refresh, initialization, migration, and `restore-previous`. It is
strictly increasing across writes, so clients can reject a replayed older
pointer (see [wg-client.md §6](./wg-client.md#6-sync-algorithm)). The
wall-clock term lets the sequence recover past any value a client has
seen even if an attacker rolled the stored pointer back. The `+ 1` term
keeps it increasing if the server's clock steps backward. Clients never
compare `sequence` with their own clock.

Node names follow §6. Reject unknown fields, unsupported schema versions,
duplicate JSON keys, malformed entries, and equal current/previous IDs, in
both the envelope and the payload. The two `schema_version` values must
match. Each non-null generation contains a valid `server_recipient`
fingerprint and a nonempty hostname map within the shared limits. Each
map entry has exactly a `digest` and a `recipient` fingerprint.
`current: null` requires `previous: null`. Derive object paths from these
validated values; the discovery object supplies no arbitrary URL or
filesystem path. Parse the payload only after its signature verifies.

`previous` is `null` until a second generation is committed. Before the
first publication, both `current` and `previous` may be `null`; clients
then report that no generation has been published. An absent pointer may
be initialized to this empty state with a fresh revision and
`If-None-Match: *` only when listing confirms the reserved `nodes/`
namespace is empty. Distinguish `NoSuchKey` from a missing bucket or denied
access; initialization requires a successful listing in the intended
existing bucket. Existing generation objects with no pointer indicate
lost or inconsistent state and require explicit recovery. Initializing
the pointer before staging files lets the next
cron job recognize an interrupted first publication.

A client named `workstation-01` reads `current.json` and verifies its
signature. It selects `current`, looks up its own entry, and downloads
`nodes/workstation-01/189c8r7s5x4dcvhq7acet5dr0zpc592e13rxd8xgk3nqe1hxj9d4.conf.age` in this
example. It verifies the digest, decrypts the file with its own age
identity, and validates the plaintext before local application. It needs
no bucket listing, synchronized clock, server notification, or remembered
remote generation ID. See [wg-client.md §6](./wg-client.md#6-sync-algorithm).

Configuration objects use `Content-Type: application/octet-stream`; the
pointer uses `Content-Type: application/json`. Set `Cache-Control: no-store`
on both and use the authenticated storage endpoint directly. Each new
generation includes every node's full file, even if only one file changed.
An entirely unchanged topology and rendered content leave both retained
generation entries unchanged; the revision may refresh to fence cleanup.

### Encryption and signing (schema v2)

**Configuration files** are [age v1](https://age-encryption.org/v1)
files in the binary format (not ASCII armor). Each file has exactly two
recipient stanzas, both of the native hybrid post-quantum type
`mlkem768x25519`: one for the node's `age_recipient` and one for the
server's own recipient. That type combines ML-KEM-768 and X25519 through
HPKE ([C2SP age spec](https://github.com/C2SP/C2SP/blob/main/age.md);
recipients `age1pq1…`, identities `AGE-SECRET-KEY-PQ-1…`). An attacker has
to break both ML-KEM-768 and X25519 to read a file. The WireGuard PSKs
inside are 32 random bytes, so recorded tunnel traffic stays protected
against a future quantum attacker as long as this delivery path is too.
age says a file SHOULD NOT mix this type with non-post-quantum
recipients, and this fleet makes that a hard rule.

- Both readers reject a file whose header contains any stanza type other
  than `mlkem768x25519`, or a stanza count other than two. This stops a
  writer from downgrading a file to a classical-only recipient. age
  stanzas don't reveal which recipient they're for, so readers check the
  count and type, not the recipient's identity.
- Bound reads on both sides. The ciphertext download limit is 1 MiB +
  64 KiB (inclusive), which covers the age header and per-chunk tags on a
  1 MiB plaintext. Decrypt into a buffer capped at 1 MiB (inclusive) and
  reject the first byte past it.
- The per-node `recipient` fingerprint and generation-level
  `server_recipient` fingerprint in the pointer exist for two reasons:
  - The server can detect a recipient change (§8 step 5) without
    decrypting anything with the node's key.
  - A client can report "published for a different identity" clearly,
    instead of a generic decryption failure.

  They are metadata, not an access control. Decryption is the access
  control.

**The pointer** carries a hybrid signature: one Ed25519 signature and
one ML-DSA-87 signature, made with `publication_keys.signing_keys`.
Both sign the same message M: the ASCII domain-separation prefix
`wg-vpn/current.json/v2\n` followed by the exact decoded payload bytes.
Signing raw bytes avoids JSON canonicalization rules.
- Ed25519 is plain RFC 8032 Ed25519 (not Ed25519ph/ctx).
- ML-DSA-87 is FIPS 204 pure ML-DSA (not HashML-DSA), hedged signing,
  and an empty context string. The domain prefix inside M already does
  the context's job.

A pointer is valid only when **both** signatures verify under the **same**
`server_verify_keys` entry. A forgery then needs both a classical break of
Ed25519 and a break of ML-DSA (for example, by a quantum computer, or
through a flaw in this young scheme or its implementations). Accepting
either signature alone would give up exactly that property, so neither
reader does.

ML-DSA-87 is FIPS 204's highest parameter set (NIST category 5). Its
costs are negligible here: a 2592-byte public key in each client's
config, and a 4627-byte signature on one small object per write. The
encryption side stays at ML-KEM-768 (category 3), because `mlkem768x25519`
is the only native post-quantum hybrid age type. Raising it would mean a
non-standard age type that Go age couldn't decrypt, losing the interop
check that gates the implementation.

Verification happens before the payload is parsed. Both the server
(against its own keys) and every client (against `server_verify_keys`)
reject a pointer that doesn't verify. A configuration file's integrity
follows from its SHA-256 digest in the signed payload. Files are not
signed separately. Pinning a list of verify-key entries, rather than
one, is what makes signing-key rotation possible without a flag day
(§8).

**Implementation status.** The Rust `age` crate (0.12.1, the latest
release checked) supports the age file format and the hardware-token
`mlkem768p256tag` type. It does not yet ship the software
`mlkem768x25519` recipient/identity. Until it does, implement that type in
`wg-common` against the `age` crate's public `Recipient`/`Identity` traits,
using [libcrux](https://github.com/cryspen/libcrux)'s X-Wing as the KEM.

The age spec's KEM is HPKE `MLKEM768-X25519` (KEM ID `0x647a`,
draft-ietf-hpke-pq). draft-irtf-cfrg-concrete-hybrid-kems defines that KEM
and states it "is identical to the X-Wing construction". The parameters
match what libcrux-kem implements as `XWingKemDraft06`:
- 32-byte private seed, expanded with SHAKE-256 into the ML-KEM-768 and
  X25519 keys;
- 1216-byte public key and 1120-byte encapsulation;
- SHA3-256 combiner with the `\.//^\` label.

So the only custom code is the glue: the HPKE base-mode layer, through
the `hpke` crate's KEM trait the same way the `age` crate's own `tagpq`
module plugs in its KEM; the age stanza encoding; and the
`age1pq1…`/`AGE-SECRET-KEY-PQ-1…` encodings. No hand-assembled ML-KEM or
X25519 combination. The ML-KEM inside libcrux is formally verified (hax);
treat the X-Wing combiner and HPKE glue as ordinary, test-gated code.

Do not treat this module as trusted until it passes both checks:
- it decrypts files produced by the Go reference implementation
  (`filippo.io/age`), and Go age decrypts files it produced;
- it passes the spec's published test vectors where available.

Replace it with the upstream implementation as soon as one is released.
No other part of this design depends on which implementation is used.

### Migrating from schema v1

Schema v1 has an unsigned pointer and plaintext `.conf` objects. Migration
is a single, operator-run `apply --migrate-v1`. Rollout order:

1. Upgrade every client binary. Generate each node's age identity
   (`wg-client keygen`), and give each client its `age_identities` plus
   the `server_verify_keys` from `wg-server keygen`. Upgraded clients
   reject the v1 pointer and leave their tunnel up, so this step is safe
   to do gradually.
2. Add `publication_keys` and every node's `age_recipient` to the
   topology, then run `wg-server apply --migrate-v1`. This:
   1. accepts the v1 pointer (only with this flag) and validates it with
      v1 rules;
   2. reads reusable keys from its plaintext current generation;
   3. publishes an encrypted generation;
   4. conditionally replaces the pointer with a signed v2 pointer, using
      `previous: null` (a plaintext generation is never retained under
      v2);
   5. deletes every plaintext `.conf` object immediately (the flag
      implies `--prune`).
3. Later runs omit the flag. `apply` without it rejects a v1 pointer, and
   `--migrate-v1` rejects a v2 pointer, so a stray re-run can't downgrade
   anything.

The v1 private keys and PSKs sat in plaintext in the bucket. Unless
per-node read scoping (§12) was enforced the whole time, combine
`--migrate-v1` with bare `--rotate` so no key that was ever stored in
plaintext survives into v2. Clients not upgraded before step 2 reject
the v2 pointer and keep their old tunnel. If the migration also rotated
keys, their links to rotated peers break until they're upgraded.

### Retry policy

Every individual storage call — `GetObject`, `PutObject`, `DeleteObject`,
`ListObjectsV2` — is retried up to **5 attempts total** before being
treated as failed:

- Only transient errors are retried: network timeouts/connection resets,
  retryable 5xx responses, and throttling (429 or provider throttling error
  codes). Auth failures (401/403) and other
  non-retryable errors fail immediately without consuming retries.
- A 404 for a configuration referenced by `current.json` is an integrity
  error. Node absence is determined from the pointer's hostname set, not
  from a missing object. Handle an absent pointer using the initialization
  rules above.
- Backoff between attempts is exponential with jitter (1000ms base,
  doubling, capped at 5s). Configure both the attempt limit and the
  backoff cap explicitly in the SDK.
- A streamed-body failure restarts the whole download only within the
  remaining per-call attempt budget. SDK and body retries share the same
  five attempts; never multiply nested retry budgets.
- Set finite request/attempt and body-download deadlines, plus an overall
  job deadline within the scheduler's execution limit. Use at most eight
  concurrent storage calls, a 10s request-attempt timeout, a 30s total
  deadline per call including body consumption, and an 8-minute overall
  job deadline. SDK request timeouts alone do not cover consuming a
  returned response stream
  ([AWS timeout documentation](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/timeouts.html)).
- A conditional-write conflict is not a transient error to blindly retry.
  Reconcile it using the publication rules below. A timed-out write has
  an unknown outcome, not a confirmed failure to store data.

### Publication commit

1. Upload each node's file using the new generation ID. Use
   `If-None-Match: *` to prevent overwriting an existing object. Reuse the
   same ID and bytes for retries
   within this invocation. If a retry finds the object already present,
   verify its exact content; an identical upload is complete, while
   different content is an integrity error.
2. Await all uploads. Only after every file is confirmed stored, replace
   `current.json` using `If-Match` with the ETag read before planning.
   Set `current` to the complete new entry and `previous` to the former
   `current` entry, with a fresh revision and a new `sequence`, and sign
   the payload. All change in the same object write. Retries reuse the
   same revision, sequence, and signed bytes; never re-sign for a retry.
3. If the pointer write times out or returns a conflict, reread it. If it
   exactly matches this invocation's intended pointer, the commit succeeded.
   If it still matches the original pointer and ETag, the same conditional
   write can be retried within the bounded budget. If another value is
   present, report a conflict and stop. If its state cannot be read, report
   an unknown commit outcome. Do not delete any candidate generation while
   the outcome is unknown.
4. A failed upload leaves the published pointer and its configurations
   unchanged. Abandon the candidate generation for subsequent cleanup;
   no compensating overwrite of live configurations is necessary.

This requires strong read-after-write consistency and conditional writes
from the storage provider. R2 supports these primitives
([consistency model](https://developers.cloudflare.com/r2/reference/consistency/),
[S3 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/)).
The pointer commits discovery of a complete snapshot; it does not switch
live WireGuard interfaces together. All readers must follow `current.json`
instead of listing the bucket or choosing a generation by name or age.

### Retention: current plus one previous generation

After a confirmed commit, reread and validate that the pointer still
matches this invocation's committed revision and retained entries. On an
unchanged pass, first conditionally refresh only `revision` using the
observed ETag and verify that write using the same reconciliation rules as
publication. If this fails or its result is unknown, skip cleanup.

This revision change invalidates an older job's in-flight conditional
commit even if that job's process has already terminated. Otherwise a
delayed commit could publish candidate files after the next unchanged run
deleted them. Retained configuration generations do not change during a
revision refresh. The scheduler must still prevent live overlapping jobs;
revision refresh alone does not protect concurrent staging/cleanup.

Under the scheduler's exclusive execution guarantee (§1), list
all pages beneath the reserved `nodes/` prefix and delete recognized
generation objects whose ID is neither `current.id` nor `previous.id`.
This removes older committed generations and abandoned uploads. Never
delete either retained generation, the pointer, or unrelated bucket keys.
Recognize only `nodes/<valid-hostname>/<valid-generation-id>.conf.age`
and, as leftovers from schema v1, `nodes/<valid-hostname>/<valid-generation-id>.conf`.
A v1 `.conf` object is never retained under v2, whatever its ID. Skip
and report unexpected names. Compare IDs for membership in the retained
pair, never for chronological order. When an entry is null, it contributes
no retained ID.

As a defense-in-depth layer under the scheduler's guarantee — not a
replacement for it — skip deleting any candidate object whose
`LastModified` is younger than a fixed grace period (e.g. 15 minutes).
This bounds the exact race named in §1 (conditional publication cannot by
itself stop concurrent cleanup from deleting an active uploader's files):
a legitimate in-flight upload from an overlapping job is always younger
than its own upload started, so a grace period comfortably longer than
the 8-minute job deadline ensures cleanup never deletes a file that could
still belong to a run still in progress, while still reclaiming genuinely
abandoned uploads on the next successful cleanup pass. This does not
change what counts as *retained* — that remains pointer membership, never
age — it only delays when an already-orphaned object becomes eligible for
deletion.

After successful cleanup the bucket contains the current generation, at
most one previous committed generation, and possibly a handful of
recently-orphaned objects still inside the deletion grace period above. A
newly initialized fleet has no previous generation. Upload staging
temporarily uses additional space; a crash or failed delete can leave
extra objects until the next successful cleanup. Report cleanup failures
separately from publication success and retry them on the next cron run,
including runs with unchanged content. There is no retained archive
beyond the one previous generation and whatever the grace period is
still holding.

Provision unversioned storage without retention locks for these objects.
S3 versioned or versioning-suspended buckets are outside this v1 cleanup
protocol: a normal delete can leave older versions and delete markers
([S3 object-version deletion](https://docs.aws.amazon.com/AmazonS3/latest/userguide/DeletingObjectVersions.html)).
Do not configure age-based lifecycle expiry for the reserved namespace;
an unchanged current generation may remain active indefinitely. Retention
must follow pointer membership rather than object age.

Retention is independent of client schedules. For example, suppose changes
produce G0 on day 1, G1 on day 2, and G2 on day 3, each at 00:00. A client
that applies G0 at 00:30 on day 1 but misses day 2 goes directly to G2 at
00:30 on day 3. Only G2 and G1 are then kept in the bucket; the client's
existing local G0 file is unaffected. If cleanup removes an object after
a client reads its generation ID but before it downloads the file, the
client rereads the pointer and retries
the latest generation with a bounded retry budget (§6 of wg-client).

Keeping one previous generation provides recovery data. It does not make
old and new WireGuard keys concurrently valid or require clients to replay
intermediate generations. With healthy daily client scheduling and a
stable current generation, adoption occurs at each client's next run,
plus scheduling jitter and download/apply time. Offline clients have no
bounded adoption time. Key rotations and incompatible address/topology
changes can interrupt affected links during this transition; routine
scheduled server jobs must not request key rotation. The 8-minute server
deadline normally fits within the thirty-minute offset to the next client
slot, but delayed/failed jobs can miss that slot; the schedule is not an
activation barrier or a zero-downtime guarantee.

## 11. Error handling

- Schema/topology validation errors (§6) abort before any key handling or
  network call — fail closed.
- Invalid discovery metadata, a pointer signature that fails to verify,
  missing referenced files, digest mismatches, undecryptable files, or
  unparsable current keys abort publication without incidental rotation.
- An upload failure leaves the current generation published. A pointer
  write with an uncertain result is reconciled by reading `current.json`
  (§10), never by assuming the write failed and deleting its objects.
- A cleanup failure exits non-zero with `publication=committed` or
  `publication=unchanged` and `cleanup=incomplete` in the summary. The
  current generation remains usable and the next serialized job retries
  cleanup from bucket state.
- After a crash, a normal scheduled `apply` resumes from the pointer,
  reuses its current keys, and collects abandoned data after a successful
  pass. No local transaction journal survives or is needed. Integrity
  errors require explicit recovery instead of blind repeated rotation.
- Reissuing `apply --rotate` creates another rotation, even if a prior
  invocation already committed but lost its response. Inspect publication
  state before manually repeating a rotation; the normal cron command
  contains no `--rotate` flag. (The packaged `docker/` cron image and
  `package/`'s `wg-server.service` are intentional exceptions — both
  bake in a bare `--rotate`, so every scheduled run there does rotate
  the whole fleet; see `docker/README.md`'s "Rotation on every run" for
  the operational tradeoffs that choice accepts.)

For recovery from damaged published state, stop scheduled publication and
confirm the previous job has terminated. Verify the retained previous
snapshot's digests and configuration consistency before using it.
`wg-server restore-previous` (§4) does that check and then conditionally
restores the pointer to the verified snapshot, with `previous: null`, a
fresh revision, a new sequence, and a fresh signature. A hand-edited
pointer would fail signature verification on every client, so don't
write one with generic storage tooling. Then correct the topology input
and resume normal `apply`. Do not infer
trusted keys from unreferenced files or retain a damaged snapshot as the
fallback. If neither retained snapshot is usable, explicit re-enrollment
is required. Recovery
is a new publication decision and still propagates on client schedules.

## 12. Security considerations

- Supply the input config and **read-write** credentials through the cloud
  job's secret/configuration mechanism. Use owner-only temporary files,
  restrictive creation permissions, and no persistent runner storage.
  The input JSON is supplied afresh on each invocation; environment-based
  secret injection belongs to the job wrapper, not implicit SDK credential
  fallback. Never commit real credentials or rendered private configs.
  `publication_keys` gets the same treatment, with one difference: the
  storage credentials and the publication keys are separate secrets, and
  the publication keys are the more important pair (see the trust
  boundary below).
- **Trust boundary (schema v2).** Bucket access is no longer the fleet's
  trust boundary; `publication_keys` is. Concretely:
  - **Read access** to the bucket reveals hostnames, digests, recipient
    fingerprints, generation IDs, and ciphertext — no private key or PSK.
  - **Write access** without the signing keys can delete objects or upload
    junk (denial of service). It cannot plant a configuration any client
    or the server will accept: a forged or edited pointer fails signature
    verification, and a file swapped under a valid pointer fails its
    signed digest.
  - It also cannot replay an older genuine pointer to a client that has
    already seen a newer one (wg-client §6). A client with no stored
    state (fresh install, lost state file) accepts any correctly signed
    pointer, including an older one. That's trust-on-first-use, bounded
    by the retention policy.
  - **The signing key pair** lets its holder publish anything to every
    node. Both halves are needed: one leaked half alone forges nothing,
    but rotate the pair anyway.
    **The server age identity** lets its holder read every node's
    WireGuard keys.

  Keep both in the scheduler's secret store only, never on a node or in
  an image, and treat exposure of either as a fleet compromise: rotate it
  (§8) and run bare `--rotate`.
- Keep the bucket private, with public endpoints/CDN access disabled.
  Each client should still receive distinct credentials permitting
  `GetObject` only for `current.json` and its own `nodes/{hostname}/`
  prefix. Under v2 this is defense in depth rather than the isolation
  mechanism itself. It denies other nodes even the ciphertext and
  shrinks what a leaked node credential exposes. The server has
  read/write/list/delete permissions for the reserved namespace.
- R2 supports scoped temporary credentials with exact-object and prefix
  restrictions. The out-of-band credential issuer grants `current.json`
  plus the node prefix, supplies the access key, secret key, and session
  token, and renews them before expiry. Keep issuer/parent credentials off
  VPN clients. The stable node prefix continues to work across generations
  ([R2 temporary credentials](https://developers.cloudflare.com/r2/api/s3/temporary-credentials/)).
  For GetObject-only R2 credentials, the issuer can locally sign a scoped
  token with `actions: ["GetObject"]`, `paths.objectPaths: ["current.json"]`,
  and `paths.prefixPaths: ["nodes/<hostname>/"]`. Broader object-read-only
  presets can also grant listing, which this client does not require.
  A provider/deployment unable to enforce per-node reads can still run
  schema v2 safely. Per-node encryption is the isolation mechanism; only
  the defense-in-depth layer is lost.
- The shared discovery object exposes fleet hostnames, content digests,
  and recipient fingerprints, but no private keys or PSKs. Access to it is part of the client trust
  model; deployments requiring private fleet membership need a different
  discovery mechanism.
- Both binaries validate `r2_config.endpoint` in their own configuration
  validation (`wg_common::topology::validate_r2_endpoint`): reject a
  non-`http`/`https` scheme, URL userinfo, query strings, and fragments,
  and reject empty credential fields
  (`wg_common::topology::validate_r2_credentials`). Rejecting plain
  `http://` specifically is deliberately not yet enforced —
  `docker-tests/` talks to local MinIO over plain HTTP by design (see
  CLAUDE.md "Known simplifications"), and closing this gap needs a
  TLS-enabled test harness first. Until then, the SDK's own permissiveness
  for HTTP endpoints must not be relied on as a substitute
  ([SDK endpoint documentation](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/endpoints.html)).
- Both retained generations contain private keys, encrypted. Deletion or
  removal from the current topology does not revoke previously downloaded
  keys, a node's own age identity (which can decrypt every file ever
  addressed to it, including retained ones), or storage credentials.
  Digests are covered by the pointer signature, so together they
  authenticate content, not just detect corruption.
- Because reuse is read back from the bucket itself, `wg-server` reuses a
  `PrivateKey` only from a file it could decrypt with its own identity,
  listed in a pointer whose signature verified against its own key. A
  bucket writer without `publication_keys` therefore can't plant a
  `.conf` that the server would reuse. Under schema v1 they could, and
  the read-write credentials were the real trust boundary. Under v2,
  `publication_keys` is.

To remove a node, revoke its storage credentials and remove it from the
topology/master list, then publish. Confirm remaining affected peers have
applied the removal; publication alone does not revoke an offline peer's
cached configuration. For urgent revocation, block the node at the host or
network firewall while scheduled updates propagate. If bucket-wide client
credentials were ever exposed, revoke them. Under v2 that alone isn't a
key compromise: only ciphertext was readable. If the server age identity
or both signing keys were exposed, treat the fleet's private keys and PSKs as
compromised, which requires a controlled fleet rotation. The
previous generation's retained keys are secret recovery material,
not authorization to reconnect a removed node.

## 13. Logging & observability

- Structured logs (`tracing`), one line per hostname processed
  (`reused` / `rotated` / `uploaded` / `skipped-unchanged`), plus a final
  summary naming current/previous generation IDs, publication outcome,
  nodes/edges freshly keyed, and cleanup counts/failures.
- `--dry-run` prints the planned publication and cleanup without writes,
  deletes, configuration bodies, or secret key material. A later real
  invocation re-reads state and computes its own plan.
- Redact credential fields, private keys, PSKs, age identities, the
  signing keys, plaintext and ciphertext config bodies, SDK request
  signing headers, and parser error values. Log field names, hostnames,
  generation IDs, digests, and sanitized error categories instead. Disable
  secret-bearing debug dumps and core dumps in the job environment.

## 14. Dependencies (planned crates)

| Crate | Purpose |
|---|---|
| `clap` | CLI parsing |
| `serde`, `serde_json` | Config (de)serialization |
| `tokio` | Async runtime (required by the S3 SDK) |
| `aws-sdk-s3`, `aws-config` | S3-compatible storage client, with custom endpoint + `auto`/path-style support for R2 |
| `x25519-dalek`, `rand` | WireGuard-compatible key generation, PSKs, and 32-byte random generation IDs |
| `base64` | WireGuard configuration key encoding |
| `sha2` | SHA-256 digest computation over uploaded ciphertext bytes and recipient fingerprints |
| `age` | age v1 file format (encrypt to two recipients, decrypt with server identities); the `mlkem768x25519` recipient/identity type itself lives in `wg-common` until upstream ships it (§10) |
| `libcrux-kem` (via `wg-common`) | X-Wing (`XWingKemDraft06`), the KEM inside the `mlkem768x25519` age type (§10) |
| `hpke` (via `wg-common`) | HPKE base mode around X-Wing, plugged in the same way the `age` crate's `tagpq` module does |
| `ed25519-dalek` | Ed25519 half of the hybrid pointer signature (§10) |
| `libcrux-ml-dsa` | ML-DSA-87 half of the hybrid pointer signature (§10); formally verified arithmetic (hax) |
| `wg-common` (shared crate, §3) | Strict config parser/renderer and key validation shared with `wg-client`; wraps `ipnet`/`url` for typed route/endpoint validation and implements the canonical lowercase Crockford Base32 encoding for IDs/digests (§10) — both binaries call this one implementation rather than each reimplementing the encoding |
| `thiserror` / `anyhow` | Error types |
| `tracing`, `tracing-subscriber` | Logging |

## 15. Future enhancements

- Scheduled key rotation with durable operation IDs in bucket metadata
  for idempotent retries, plus a documented maintenance/rollout policy.
- Per-node scoped storage tokens generated and rotated by `wg-server`
  itself, if the provider exposes a token-management API.
- Switch `wg-common`'s `mlkem768x25519` implementation to the upstream
  `age` crate's once released (§10).
- IPv6 tunnel addresses / dual-stack `AllowedIPs`.
- Webhook/notification on publish (e.g. notify a chat channel of topology
  changes).
- Terraform/Ansible provider wrapping `wg-server apply` for GitOps-style
  fleet management.
