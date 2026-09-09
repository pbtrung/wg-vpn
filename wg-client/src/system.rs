//! Host operations `sync`'s algorithm needs (files, locking-adjacent
//! checks, and WireGuard interface configuration), behind a trait so the
//! state machine can be tested with an in-memory fake instead of
//! touching a real filesystem/interface (wg-client.md §6, tested this
//! way per docs/milestones.md §3 "an explicit host-operation test
//! adapter").
//!
//! `RealSystemOps` configures the kernel interface directly over
//! netlink/UAPI via the `wireguard-control` crate (keys/peers) plus this
//! crate's own `netlink` module (address/link/route) -- the way
//! [innernet](https://github.com/tonarino/innernet) does it -- rather
//! than shelling out to `wg`/`wg-quick`. See NOTICE.md for the licensing
//! consequence of depending on `wireguard-control`.

use std::path::{Path, PathBuf};

pub trait SystemOps {
    fn read_file(&self, path: &Path) -> Option<Vec<u8>>;

    /// Write `contents` to `path` atomically: stage in the same
    /// directory, fsync, rename over the target, fsync the parent
    /// directory. Mode `0600`.
    fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), String>;

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), String>;
    fn file_exists(&self, path: &Path) -> bool;

    /// Whether a WireGuard interface named `iface` currently exists.
    fn interface_exists(&self, iface: &str) -> bool;

    /// Configure the interface (creating it if absent) exactly per the
    /// parsed contents of `conf_path`: private key, listen port, the
    /// full peer set (replacing whatever peers were there before),
    /// tunnel address, MTU, and routes for every peer's `AllowedIPs`.
    async fn apply_interface(&self, iface: &str, conf_path: &Path) -> Result<(), String>;

    /// Remove the interface entirely (kernel cleans up its addresses
    /// and routes as part of deleting the link).
    async fn teardown_interface(&self, iface: &str) -> Result<(), String>;

    /// Route-conflict preflight (wg-client.md §6): before tearing down
    /// the working interface for `cfg`, reject it if any candidate peer
    /// route would overlap another interface's existing route or capture
    /// the storage endpoint, this tunnel's configured DNS, or a peer's
    /// transport endpoint.
    async fn preflight(
        &self,
        cfg: &wg_common::render::ParsedConfig,
        storage_endpoint_host: &str,
        managed_iface: &str,
    ) -> Result<(), String>;
}

pub struct RealSystemOps;

impl SystemOps for RealSystemOps {
    fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let tmp = tmp_path_for(path);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("open {tmp:?}: {e}"))?;
            f.write_all(contents)
                .map_err(|e| format!("write {tmp:?}: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync {tmp:?}: {e}"))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {tmp:?} -> {path:?}: {e}"))?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), String> {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| format!("copy {from:?} -> {to:?}: {e}"))
    }

    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn interface_exists(&self, iface: &str) -> bool {
        // Query the kernel directly, the same way innernet's
        // interface_is_up() re-checks Device::list() on every call
        // rather than trusting a cached/filesystem-based answer.
        match iface.parse::<wireguard_control::InterfaceName>() {
            Ok(name) => wireguard_control::Device::list(wireguard_control::Backend::Kernel)
                .map(|ifaces| ifaces.contains(&name))
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    async fn apply_interface(&self, iface: &str, conf_path: &Path) -> Result<(), String> {
        let bytes = std::fs::read(conf_path).map_err(|e| format!("read {conf_path:?}: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        let cfg = wg_common::render::parse_wg_quick(&text)
            .map_err(|e| format!("parse {conf_path:?}: {e}"))?;

        // Resolve endpoint names before handing off to the blocking
        // netlink/UAPI work below (wg-client.md §6: "resolve endpoint
        // names with a finite deadline" before touching the interface).
        let mut endpoints = Vec::with_capacity(cfg.peers.len());
        for peer in &cfg.peers {
            endpoints.push(match &peer.endpoint {
                Some(endpoint) => Some(resolve_endpoint(endpoint).await?),
                None => None,
            });
        }

        let iface = iface.to_string();
        tokio::task::spawn_blocking(move || apply_interface_blocking(&iface, &cfg, &endpoints))
            .await
            .map_err(|e| format!("interface apply task panicked: {e}"))?
    }

    async fn teardown_interface(&self, iface: &str) -> Result<(), String> {
        let iface = iface.to_string();
        tokio::task::spawn_blocking(move || teardown_interface_blocking(&iface))
            .await
            .map_err(|e| format!("interface teardown task panicked: {e}"))?
    }

    async fn preflight(
        &self,
        cfg: &wg_common::render::ParsedConfig,
        storage_endpoint_host: &str,
        managed_iface: &str,
    ) -> Result<(), String> {
        crate::preflight::check(cfg, storage_endpoint_host, managed_iface).await
    }
}

/// Resolve a `host:port`/`ip:port` endpoint string with a finite
/// deadline (wg-client.md §6). Picks the first resolved address.
async fn resolve_endpoint(endpoint: &str) -> Result<std::net::SocketAddr, String> {
    let lookup = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::lookup_host(endpoint),
    )
    .await
    .map_err(|_| format!("resolving endpoint {endpoint:?} timed out"))?
    .map_err(|e| format!("resolving endpoint {endpoint:?}: {e}"))?;
    lookup
        .into_iter()
        .next()
        .ok_or_else(|| format!("endpoint {endpoint:?} resolved to no addresses"))
}

/// The synchronous netlink/UAPI work behind [`RealSystemOps::apply_interface`],
/// run on a blocking thread since these are ordinary blocking syscalls,
/// not async I/O.
fn apply_interface_blocking(
    iface: &str,
    cfg: &wg_common::render::ParsedConfig,
    endpoints: &[Option<std::net::SocketAddr>],
) -> Result<(), String> {
    let name: wireguard_control::InterfaceName = iface
        .parse()
        .map_err(|e| format!("invalid interface name {iface:?}: {e}"))?;

    let private_key = wireguard_control::Key::from_base64(&cfg.interface.private_key)
        .map_err(|_| "invalid private key".to_string())?;

    let mut update = wireguard_control::DeviceUpdate::new()
        .set_private_key(private_key)
        .set_listen_port(cfg.interface.listen_port)
        .replace_peers();

    for (peer, endpoint) in cfg.peers.iter().zip(endpoints) {
        let public_key = wireguard_control::Key::from_base64(&peer.public_key)
            .map_err(|_| "invalid peer public key".to_string())?;
        let preshared_key = wireguard_control::Key::from_base64(&peer.preshared_key)
            .map_err(|_| "invalid peer preshared key".to_string())?;

        let mut builder = wireguard_control::PeerConfigBuilder::new(&public_key)
            .set_preshared_key(preshared_key)
            .replace_allowed_ips();
        for net in &peer.allowed_ips {
            builder = builder.add_allowed_ip(std::net::IpAddr::V4(net.network()), net.prefix_len());
        }
        if let Some(keepalive) = peer.persistent_keepalive {
            builder = builder.set_persistent_keepalive_interval(keepalive);
        }
        if let Some(endpoint) = endpoint {
            builder = builder.set_endpoint(*endpoint);
        }
        update = update.add_peer(builder);
    }

    update
        .apply(&name, wireguard_control::Backend::Kernel)
        .map_err(|e| format!("applying WireGuard device config: {e}"))?;

    crate::netlink::set_addr(&name, cfg.interface.address, 32)
        .map_err(|e| format!("setting address: {e}"))?;
    crate::netlink::set_up(&name, cfg.interface.mtu.unwrap_or(1420) as u32)
        .map_err(|e| format!("bringing link up: {e}"))?;

    for peer in &cfg.peers {
        for net in &peer.allowed_ips {
            crate::netlink::add_route(&name, *net)
                .map_err(|e| format!("adding route {net}: {e}"))?;
        }
    }

    Ok(())
}

/// The synchronous netlink/UAPI work behind [`RealSystemOps::teardown_interface`].
/// Deletes the whole device; the kernel removes its addresses and routes
/// as part of that, so there is nothing else to unwind separately.
fn teardown_interface_blocking(iface: &str) -> Result<(), String> {
    let name: wireguard_control::InterfaceName = iface
        .parse()
        .map_err(|e| format!("invalid interface name {iface:?}: {e}"))?;
    match wireguard_control::Device::get(&name, wireguard_control::Backend::Kernel) {
        Ok(device) => device
            .delete()
            .map_err(|e| format!("deleting interface: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("looking up interface before delete: {e}")),
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".wg-client-tmp");
    PathBuf::from(s)
}

#[cfg(test)]
pub mod mock {
    use super::SystemOps;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockSystemOps {
        files: Mutex<HashMap<PathBuf, Vec<u8>>>,
        up_interfaces: Mutex<HashSet<String>>,
        /// Fail the next N `apply_interface`/`teardown_interface` calls,
        /// then succeed — so a test can fail exactly the candidate's
        /// apply attempt and still let a subsequent rollback succeed.
        pub fail_next_apply: Mutex<u32>,
        pub fail_next_teardown: Mutex<u32>,
        /// Fail the next N `write_file_atomic` calls, then succeed.
        pub fail_next_write: Mutex<u32>,
        /// Fail the next N `preflight` calls, then succeed.
        pub fail_next_preflight: Mutex<u32>,
        pub calls: Mutex<Vec<String>>,
    }

    impl MockSystemOps {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn set_file(&self, path: &Path, contents: Vec<u8>) {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), contents);
        }

        pub fn get_file(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(path).cloned()
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SystemOps for MockSystemOps {
        fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(path).cloned()
        }

        fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
            {
                let mut n = self.fail_next_write.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err("injected write failure".to_string());
                }
            }
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), contents.to_vec());
            Ok(())
        }

        fn copy_file(&self, from: &Path, to: &Path) -> Result<(), String> {
            let contents = self
                .files
                .lock()
                .unwrap()
                .get(from)
                .cloned()
                .ok_or_else(|| format!("{from:?} does not exist"))?;
            self.files
                .lock()
                .unwrap()
                .insert(to.to_path_buf(), contents);
            Ok(())
        }

        fn file_exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }

        fn interface_exists(&self, iface: &str) -> bool {
            self.up_interfaces.lock().unwrap().contains(iface)
        }

        async fn apply_interface(&self, iface: &str, conf_path: &Path) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("up:{iface}"));
            {
                let mut n = self.fail_next_apply.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err("injected interface apply failure".to_string());
                }
            }
            // A real apply would fail if conf_path doesn't parse; keep
            // the mock honest about that precondition too.
            if let Some(bytes) = self.files.lock().unwrap().get(conf_path) {
                let text = String::from_utf8_lossy(bytes).to_string();
                wg_common::render::parse_wg_quick(&text)
                    .map_err(|e| format!("parse {conf_path:?}: {e}"))?;
            } else {
                return Err(format!("{conf_path:?} does not exist"));
            }
            self.up_interfaces.lock().unwrap().insert(iface.to_string());
            Ok(())
        }

        async fn teardown_interface(&self, iface: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("down:{iface}"));
            {
                let mut n = self.fail_next_teardown.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err("injected interface teardown failure".to_string());
                }
            }
            self.up_interfaces.lock().unwrap().remove(iface);
            Ok(())
        }

        async fn preflight(
            &self,
            _cfg: &wg_common::render::ParsedConfig,
            _storage_endpoint_host: &str,
            _managed_iface: &str,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push("preflight".to_string());
            let mut n = self.fail_next_preflight.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err("injected preflight failure".to_string());
            }
            Ok(())
        }
    }
}
