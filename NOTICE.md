# Third-party license notice

This project's own code is MIT-licensed (see [`LICENSE`](LICENSE)). The
compiled `wg-client` binary additionally links
[`wireguard-control`](https://crates.io/crates/wireguard-control) (from
[innernet](https://github.com/tonarino/innernet)), which is licensed
**LGPL-2.1-or-later**, to configure the kernel WireGuard interface
directly over netlink/UAPI instead of shelling out to `wg`/`wg-quick`
(see `docs/wg-client.md` §1, §8).

Everything else `wg-client` links for interface/address/route management
(`netlink-request`, `netlink-packet-core`, `netlink-packet-route`, also
from the innernet project) is MIT-licensed. `wg-server` does not depend
on any of the above — it never touches a live interface (see
`docs/wg-server.md` §2).

Practical implications of the LGPL-2.1-or-later component:

- Source availability: `wireguard-control`'s own source is public at the
  URL above; this project does not modify it.
- Relinking: LGPL-2.1 §6 gives a user of the combined `wg-client` binary
  the right to relink it against a modified version of
  `wireguard-control`. Because Rust binaries are statically linked, the
  practical way to exercise that right is to rebuild `wg-client` from
  source (this repo's `Cargo.lock`/`Cargo.toml` pin the exact
  `wireguard-control` version in use) against a substituted
  `wireguard-control` source tree, rather than swapping a shared object
  at runtime.
- This notice, plus the pinned dependency versions in `wg-client/Cargo.toml`
  and `Cargo.lock`, is this project's compliance mechanism; it is not a
  substitute for reviewing LGPL-2.1-or-later's actual terms before
  redistributing built binaries.
