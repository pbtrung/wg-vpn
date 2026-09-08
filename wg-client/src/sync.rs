//! The sync algorithm (wg-client.md §6 "Sync algorithm" / "Local
//! application"): discover the current generation, download and
//! digest-verify this node's file, and apply it through the local
//! transaction/recovery state machine.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;
use wg_common::base32;

use crate::storage::ReadStorage;
use crate::system::SystemOps;

const MAX_DISCOVERY_ROUNDS: u32 = 3;

#[derive(Debug, Serialize, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct LocalState {
    pub hostname: String,
    pub applied_generation_id: Option<String>,
    pub applied_digest: Option<String>,
    /// Set before teardown/install, cleared only once the new config is
    /// confirmed up (or a rollback to the backup succeeds). A restart
    /// while this is `true` must recover from the backup before
    /// considering any content-hash no-op (wg-client.md §6).
    pub pending: bool,
    pub had_backup_before_pending: bool,
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn state_path_for(conf_path: &Path) -> PathBuf {
    append_suffix(conf_path, ".state.json")
}

pub fn backup_path_for(conf_path: &Path) -> PathBuf {
    append_suffix(conf_path, ".bak")
}

fn load_state<O: SystemOps>(ops: &O, path: &Path) -> LocalState {
    ops.read_file(path)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state<O: SystemOps>(ops: &O, path: &Path, state: &LocalState) {
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = ops.write_file_atomic(path, &bytes);
    }
}

fn digest_of(bytes: &[u8]) -> String {
    let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
    base32::encode(&hash)
}

#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub no_op: bool,
    pub applied: bool,
    pub recovered_pending: bool,
    pub rollback_attempted: bool,
    pub rollback_succeeded: bool,
    pub error: Option<String>,
}

async fn discover_and_download<S: ReadStorage>(
    storage: &S,
    hostname: &str,
) -> Result<(String, String, Vec<u8>), String> {
    for round in 0..MAX_DISCOVERY_ROUNDS {
        tracing::debug!(round, "fetching current.json");
        let current_bytes = storage
            .get_object("current.json")
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no publication available: current.json is missing".to_string())?;
        let doc = wg_common::discovery::parse(&String::from_utf8_lossy(&current_bytes))
            .map_err(|e| e.to_string())?;
        wg_common::discovery::validate(&doc).map_err(|e| e.to_string())?;
        let current = doc
            .current
            .ok_or_else(|| "no generation has been published yet".to_string())?;
        let expected_digest = current
            .nodes
            .get(hostname)
            .ok_or_else(|| {
                format!("hostname {hostname:?} is not registered in the current generation")
            })?
            .clone();
        let key = format!("nodes/{hostname}/{}.conf", current.id);
        tracing::debug!(generation = %current.id, %key, "downloading generation file");
        match storage.get_object(&key).await.map_err(|e| e.to_string())? {
            Some(bytes) => {
                let digest = digest_of(&bytes);
                if digest != expected_digest {
                    return Err(format!("digest mismatch for {hostname:?}"));
                }
                tracing::debug!(generation = %current.id, %digest, "digest verified");
                return Ok((current.id, digest, bytes));
            }
            None => {
                tracing::info!(generation = %current.id, "generation file missing, rediscovering");
                continue; // retired generation: rediscover current and retry
            }
        }
    }
    Err(format!(
        "exhausted {MAX_DISCOVERY_ROUNDS} discovery rounds without finding a stable generation"
    ))
}

async fn attempt_rollback<O: SystemOps>(
    ops: &O,
    state_path: &Path,
    state: &mut LocalState,
    conf_path: &Path,
    backup_path: &Path,
    had_backup: bool,
    outcome: &mut SyncOutcome,
) {
    outcome.rollback_attempted = true;
    if !had_backup {
        tracing::info!("no backup to roll back to (first install failure)");
        save_state(ops, state_path, state); // nothing to roll back to; pending stays set
        return;
    }
    tracing::info!("restoring backup config");
    if ops.copy_file(backup_path, conf_path).is_err() {
        tracing::warn!("failed to restore backup file during rollback");
        save_state(ops, state_path, state);
        return;
    }
    let ok = ops.wg_quick_up(conf_path).await.is_ok();
    tracing::info!(rollback_succeeded = ok, "rollback wg-quick up attempted");
    outcome.rollback_succeeded = ok;
    if ok {
        state.pending = false;
    }
    save_state(ops, state_path, state);
}

/// Run one sync pass. `iface` is the interface name derived from
/// `conf_path` (config::interface_name).
pub async fn run_once<S: ReadStorage, O: SystemOps>(
    storage: &S,
    ops: &O,
    hostname: &str,
    iface: &str,
    conf_path: &Path,
    force: bool,
) -> SyncOutcome {
    tracing::info!(hostname, iface, force, "starting sync pass");
    let state_path = state_path_for(conf_path);
    let backup_path = backup_path_for(conf_path);
    let mut outcome = SyncOutcome::default();
    let mut state = load_state(ops, &state_path);

    if state.pending {
        tracing::info!(
            had_backup = state.had_backup_before_pending,
            "recovering pending transaction from a previous run"
        );
        outcome.recovered_pending = true;
        if state.had_backup_before_pending && ops.file_exists(&backup_path) {
            let _ = ops.copy_file(&backup_path, conf_path);
            if !ops.interface_exists(iface) {
                let _ = ops.wg_quick_up(conf_path).await;
            }
        }
        state.pending = false;
        save_state(ops, &state_path, &state);
    }

    let (generation_id, digest, bytes) = match discover_and_download(storage, hostname).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "discovery/download failed");
            outcome.error = Some(e);
            return outcome;
        }
    };

    let text = String::from_utf8_lossy(&bytes).to_string();
    if let Err(e) = wg_common::render::parse_wg_quick(&text) {
        outcome.error = Some(format!("downloaded config failed validation: {e}"));
        return outcome;
    }

    let unchanged = !force
        && state.applied_digest.as_deref() == Some(digest.as_str())
        && ops.interface_exists(iface);
    if unchanged {
        tracing::debug!(generation = %generation_id, "content unchanged and interface up, no-op");
        outcome.no_op = true;
        return outcome;
    }

    tracing::info!(generation = %generation_id, "applying new configuration");
    let had_backup = ops.file_exists(conf_path);
    if had_backup && let Err(e) = ops.copy_file(conf_path, &backup_path) {
        outcome.error = Some(format!("failed to back up current config: {e}"));
        return outcome;
    }
    state.pending = true;
    state.had_backup_before_pending = had_backup;
    save_state(ops, &state_path, &state);

    if ops.interface_exists(iface) {
        tracing::debug!(iface, "tearing down interface before install");
        if let Err(e) = ops.wg_quick_down(conf_path).await {
            outcome.error = Some(format!("wg-quick down failed: {e}"));
            return outcome; // pending stays set; next run recovers from the backup
        }
    }

    if let Err(e) = ops.write_file_atomic(conf_path, &bytes) {
        outcome.error = Some(format!("failed to install candidate config: {e}"));
        attempt_rollback(
            ops,
            &state_path,
            &mut state,
            conf_path,
            &backup_path,
            had_backup,
            &mut outcome,
        )
        .await;
        return outcome;
    }

    if let Err(e) = ops.wg_quick_up(conf_path).await {
        outcome.error = Some(format!("wg-quick up failed: {e}"));
        attempt_rollback(
            ops,
            &state_path,
            &mut state,
            conf_path,
            &backup_path,
            had_backup,
            &mut outcome,
        )
        .await;
        return outcome;
    }

    state.pending = false;
    state.applied_generation_id = Some(generation_id);
    state.applied_digest = Some(digest);
    state.hostname = hostname.to_string();
    save_state(ops, &state_path, &state);
    outcome.applied = true;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockReadStorage;
    use crate::system::mock::MockSystemOps;
    use std::path::PathBuf;
    use wg_common::keys;

    const HOSTNAME: &str = "workstation-01";
    const IFACE: &str = "wg0";

    fn conf_path() -> PathBuf {
        PathBuf::from("/etc/wireguard/wg0.conf")
    }

    /// A minimal valid wg-quick config, distinguishable via `mtu` so
    /// tests can produce two different-but-valid generations.
    fn valid_conf(mtu: u16) -> Vec<u8> {
        let kp = keys::Keypair::generate();
        format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.2/32\nListenPort = 51820\nMTU = {mtu}\n",
            kp.private_key_base64()
        )
        .into_bytes()
    }

    fn digest(bytes: &[u8]) -> String {
        digest_of(bytes)
    }

    fn publish(storage: &MockReadStorage, generation_id: &str, bytes: &[u8]) {
        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(HOSTNAME.to_string(), digest(bytes));
        let doc = wg_common::discovery::CurrentJson {
            schema_version: wg_common::discovery::SCHEMA_VERSION,
            revision: base32::random_id(),
            current: Some(wg_common::discovery::GenerationEntry {
                id: generation_id.to_string(),
                nodes,
            }),
            previous: None,
        };
        storage.set("current.json", serde_json::to_vec(&doc).unwrap());
        storage.set(
            &format!("nodes/{HOSTNAME}/{generation_id}.conf"),
            bytes.to_vec(),
        );
    }

    #[tokio::test]
    async fn first_sync_downloads_and_applies() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        let bytes = valid_conf(1400);
        publish(&storage, &base32::random_id(), &bytes);

        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(outcome.applied);
        assert_eq!(ops.get_file(&conf_path()).unwrap(), bytes);
        assert!(
            ops.calls()
                .contains(&format!("up:{}", conf_path().display()))
        );
    }

    #[tokio::test]
    async fn unchanged_content_is_a_no_op() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        let bytes = valid_conf(1400);
        publish(&storage, &base32::random_id(), &bytes);

        run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        let calls_before = ops.calls().len();

        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        assert!(outcome.no_op);
        assert!(!outcome.applied);
        assert_eq!(ops.calls().len(), calls_before); // no additional up/down calls
    }

    #[tokio::test]
    async fn force_reapplies_even_when_unchanged() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        let bytes = valid_conf(1400);
        publish(&storage, &base32::random_id(), &bytes);

        run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), true).await;
        assert!(outcome.applied);
    }

    #[tokio::test]
    async fn changed_content_tears_down_and_reapplies() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        publish(&storage, &base32::random_id(), &valid_conf(1400));
        run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;

        let new_bytes = valid_conf(1500);
        publish(&storage, &base32::random_id(), &new_bytes);
        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;

        assert!(outcome.applied);
        assert_eq!(ops.get_file(&conf_path()).unwrap(), new_bytes);
        assert!(
            ops.calls()
                .contains(&format!("down:{}", conf_path().display()))
        );
    }

    #[tokio::test]
    async fn wg_quick_up_failure_rolls_back_to_backup() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        let old_bytes = valid_conf(1400);
        publish(&storage, &base32::random_id(), &old_bytes);
        run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;

        publish(&storage, &base32::random_id(), &valid_conf(1500));
        *ops.fail_next_wg_quick_up.lock().unwrap() = 1;
        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;

        assert!(outcome.error.is_some());
        assert!(outcome.rollback_attempted);
        assert!(outcome.rollback_succeeded);
        assert_eq!(ops.get_file(&conf_path()).unwrap(), old_bytes);
    }

    #[tokio::test]
    async fn first_install_failure_has_nothing_to_roll_back_to() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        publish(&storage, &base32::random_id(), &valid_conf(1400));
        *ops.fail_next_wg_quick_up.lock().unwrap() = 1;

        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        assert!(outcome.error.is_some());
        assert!(outcome.rollback_attempted);
        assert!(!outcome.rollback_succeeded);
    }

    #[tokio::test]
    async fn pending_transaction_is_recovered_before_anything_else() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        *ops.iface_name.lock().unwrap() = IFACE.to_string();
        let backup_bytes = valid_conf(1400);
        ops.set_file(&backup_path_for(&conf_path()), backup_bytes.clone());
        ops.set_file(&conf_path(), valid_conf(9999)); // a half-installed candidate
        let state = LocalState {
            hostname: HOSTNAME.to_string(),
            applied_generation_id: Some("old".to_string()),
            applied_digest: Some(digest(&backup_bytes)),
            pending: true,
            had_backup_before_pending: true,
        };
        ops.set_file(
            &state_path_for(&conf_path()),
            serde_json::to_vec(&state).unwrap(),
        );

        publish(&storage, &base32::random_id(), &backup_bytes);
        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;

        assert!(outcome.recovered_pending);
        // Recovery restores the backup first; since the freshly-published
        // generation is byte-identical to that backup, the rest of the
        // pass is then correctly a no-op.
        assert_eq!(ops.get_file(&conf_path()).unwrap(), backup_bytes);
        assert!(outcome.no_op);
    }

    #[tokio::test]
    async fn digest_mismatch_is_rejected() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        let generation_id = base32::random_id();
        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(HOSTNAME.to_string(), "0".repeat(52)); // wrong digest
        let doc = wg_common::discovery::CurrentJson {
            schema_version: wg_common::discovery::SCHEMA_VERSION,
            revision: base32::random_id(),
            current: Some(wg_common::discovery::GenerationEntry {
                id: generation_id.clone(),
                nodes,
            }),
            previous: None,
        };
        storage.set("current.json", serde_json::to_vec(&doc).unwrap());
        storage.set(
            &format!("nodes/{HOSTNAME}/{generation_id}.conf"),
            valid_conf(1400),
        );

        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        assert!(outcome.error.unwrap().contains("digest mismatch"));
    }

    #[tokio::test]
    async fn hostname_not_registered_is_rejected() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        publish(&storage, &base32::random_id(), &valid_conf(1400));

        let outcome = run_once(&storage, &ops, "someone-else", IFACE, &conf_path(), false).await;
        assert!(outcome.error.unwrap().contains("not registered"));
    }

    #[tokio::test]
    async fn missing_generation_file_exhausts_rediscovery_rounds() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        let generation_id = base32::random_id();
        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(HOSTNAME.to_string(), digest(&valid_conf(1400)));
        let doc = wg_common::discovery::CurrentJson {
            schema_version: wg_common::discovery::SCHEMA_VERSION,
            revision: base32::random_id(),
            current: Some(wg_common::discovery::GenerationEntry {
                id: generation_id,
                nodes,
            }),
            previous: None,
        };
        storage.set("current.json", serde_json::to_vec(&doc).unwrap());
        // Deliberately never publish the generation file itself.

        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        assert!(outcome.error.unwrap().contains("exhausted"));
    }

    #[tokio::test]
    async fn invalid_downloaded_config_is_rejected_before_touching_the_tunnel() {
        let storage = MockReadStorage::new();
        let ops = MockSystemOps::new();
        let bad = b"not a valid wg-quick config".to_vec();
        publish(&storage, &base32::random_id(), &bad);

        let outcome = run_once(&storage, &ops, HOSTNAME, IFACE, &conf_path(), false).await;
        assert!(outcome.error.unwrap().contains("failed validation"));
        assert!(ops.get_file(&conf_path()).is_none());
    }
}
