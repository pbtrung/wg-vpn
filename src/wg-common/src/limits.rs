//! Shared limits (wg-server.md §6, wg-client.md §6). Each is an inclusive
//! maximum: exactly the stated value is valid, one past it is rejected.

pub const MAX_NODES: usize = 4096;
pub const MAX_DISCOVERY_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RENDERED_CONFIG_BYTES: usize = 1024 * 1024;

pub const DEFAULT_LISTEN_PORT: u16 = 51820;
pub const DEFAULT_PERSISTENT_KEEPALIVE_WHEN_NO_ENDPOINT: u16 = 25;

pub const MIN_MTU: u16 = 576;
