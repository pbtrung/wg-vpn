//! Shared parser/renderer, key handling, topology algorithm, Base32
//! encoding, and discovery-pointer schema used by both `wg-server` and
//! `wg-client`. See `docs/wg-server.md` and `docs/wg-client.md`.

pub mod base32;
pub mod discovery;
pub mod hostname;
pub mod keys;
pub mod limits;
pub mod render;
pub mod strict_json;
pub mod topology;
