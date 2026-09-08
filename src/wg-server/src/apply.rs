//! The `apply` algorithm (wg-server.md §8 "apply algorithm", §10
//! "Storage layout and generation discovery" / "Publication commit"),
//! including M2's rotation (`--rotate`) and retention cleanup.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use thiserror::Error;
use wg_common::discovery::{CurrentJson, GenerationEntry};
use wg_common::render::{ParsedConfig, ResolvedKeys, render_node_config};
use wg_common::topology::{TopologyConfig, ValidatedTopology, ValidationError};
use wg_common::{base32, discovery, keys, render};

use crate::retention;
use crate::storage::{PutOutcome, Storage, StorageError};

pub(crate) const CURRENT_JSON_KEY: &str = "current.json";
pub(crate) const NODES_PREFIX: &str = "nodes/";
const MAX_POINTER_RETRIES: u32 = 5;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Discovery(#[from] discovery::DiscoveryError),
    #[error(
        "existing generation objects under nodes/ were found with no current.json pointer; this is lost/inconsistent state requiring explicit recovery"
    )]
    OrphanedStateNoPointer,
    #[error("current.json references {hostname}'s generation file, but it is missing from storage")]
    MissingReferencedFile { hostname: String },
    #[error("current.json's digest for {hostname} does not match the fetched file's actual digest")]
    DigestMismatch { hostname: String },
    #[error("failed to parse currently-published config for {hostname}: {source}")]
    UnparsableCurrentConfig {
        hostname: String,
        source: render::ParseError,
    },
    #[error("failed to reuse private key for {hostname}: {source}")]
    BadStoredKey {
        hostname: String,
        source: keys::KeyError,
    },
    #[error(
        "edge ({a}, {b}) is one-sided: only {only} records it in its currently-published config"
    )]
    OneSidedEdge { a: String, b: String, only: String },
    #[error("edge ({a}, {b}) has conflicting preshared keys recorded by each endpoint")]
    ConflictingPsk { a: String, b: String },
    #[error(transparent)]
    Render(#[from] render::RenderError),
    #[error(
        "upload of a freshly created generation object for {hostname} unexpectedly collided with different existing content"
    )]
    GenerationIdCollision { hostname: String },
    #[error("pointer commit reconciliation found a conflicting concurrent publication")]
    PointerConflict,
    #[error(
        "pointer commit outcome could not be determined (unreadable current.json after a write)"
    )]
    PointerCommitUnknown,
    #[error("exceeded {MAX_POINTER_RETRIES} attempts reconciling the pointer commit")]
    PointerRetriesExhausted,
    #[error("--rotate names {0:?}, which is not in the current topology config")]
    UnknownRotateHostname(String),
}

pub struct ApplyOptions {
    pub dry_run: bool,
    pub rotate: RotateSelection,
    pub grace_period: std::time::Duration,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        ApplyOptions {
            dry_run: false,
            rotate: RotateSelection::None,
            grace_period: crate::retention::DEFAULT_GRACE_PERIOD,
        }
    }
}

#[derive(Debug, Default)]
pub struct ApplyReport {
    pub warnings: Vec<String>,
    pub reused_hostnames: Vec<String>,
    pub freshly_keyed_hostnames: Vec<String>,
    pub published: bool,
    pub new_generation_id: Option<String>,
    pub uploaded_hostnames: Vec<String>,
    pub cleanup_deleted: Vec<String>,
    pub cleanup_skipped_within_grace: Vec<String>,
    /// Set when cleanup itself failed. Per wg-server.md §11 this does not
    /// invalidate the publication (`published`/`new_generation_id` above
    /// remain valid), but the caller should still exit non-zero.
    pub cleanup_error: Option<String>,
}

fn digest_of(bytes: &[u8]) -> String {
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    base32::encode(&hash)
}

fn node_object_key(hostname: &str, generation_id: &str) -> String {
    format!("{NODES_PREFIX}{hostname}/{generation_id}.conf")
}

async fn fetch_current_pointer<S: Storage>(
    storage: &S,
    dry_run: bool,
) -> Result<(CurrentJson, String), ApplyError> {
    match storage.get_object(CURRENT_JSON_KEY).await? {
        Some(obj) => {
            let doc = discovery::parse(std::str::from_utf8(&obj.bytes).unwrap_or_default())?;
            discovery::validate(&doc)?;
            Ok((doc, obj.etag))
        }
        None => {
            let existing = storage.list_all(NODES_PREFIX).await?;
            if !existing.is_empty() {
                return Err(ApplyError::OrphanedStateNoPointer);
            }
            let empty = CurrentJson::empty(base32::random_id());
            if dry_run {
                // Never write in a dry run; the plan just proceeds as if
                // an empty pointer were already there. This placeholder
                // ETag is never used for a real write in dry-run mode.
                return Ok((empty, String::new()));
            }
            let body = serde_json::to_vec(&empty).expect("CurrentJson always serializes");
            match storage
                .put_if_absent(CURRENT_JSON_KEY, body, "application/json")
                .await?
            {
                PutOutcome::Written(etag) => Ok((empty, etag)),
                PutOutcome::PreconditionFailed => {
                    // Someone else initialized concurrently; just re-read it.
                    let obj = storage
                        .get_object(CURRENT_JSON_KEY)
                        .await?
                        .expect("just observed a precondition failure, so the key now exists");
                    let doc =
                        discovery::parse(std::str::from_utf8(&obj.bytes).unwrap_or_default())?;
                    discovery::validate(&doc)?;
                    Ok((doc, obj.etag))
                }
            }
        }
    }
}

/// Fetch and parse every desired hostname's currently-published config,
/// for hostnames listed in `current`'s generation. Verifies each file's
/// digest against the pointer before trusting its content.
async fn fetch_reusable_configs<S: Storage>(
    storage: &S,
    current: &Option<GenerationEntry>,
    desired_hostnames: &BTreeSet<String>,
) -> Result<BTreeMap<String, ParsedConfig>, ApplyError> {
    let mut out = BTreeMap::new();
    let Some(entry) = current else {
        return Ok(out);
    };
    for hostname in desired_hostnames {
        let Some(expected_digest) = entry.nodes.get(hostname) else {
            continue; // not in the current generation: a new node.
        };
        let key = node_object_key(hostname, &entry.id);
        let obj =
            storage
                .get_object(&key)
                .await?
                .ok_or_else(|| ApplyError::MissingReferencedFile {
                    hostname: hostname.clone(),
                })?;
        if &digest_of(&obj.bytes) != expected_digest {
            return Err(ApplyError::DigestMismatch {
                hostname: hostname.clone(),
            });
        }
        let text = String::from_utf8_lossy(&obj.bytes);
        let parsed = render::parse_wg_quick(&text).map_err(|source| {
            ApplyError::UnparsableCurrentConfig {
                hostname: hostname.clone(),
                source,
            }
        })?;
        out.insert(hostname.clone(), parsed);
    }
    Ok(out)
}

#[derive(Debug)]
struct KeyResolution {
    resolved: ResolvedKeys,
    reused: Vec<String>,
    freshly_keyed: Vec<String>,
}

/// Which nodes `apply --rotate` forces a fresh keypair for, ignoring any
/// currently-published key (wg-server.md §4/§8).
#[derive(Debug, Clone, Default)]
pub enum RotateSelection {
    #[default]
    None,
    All,
    Named(BTreeSet<String>),
}

impl RotateSelection {
    fn is_rotated(&self, hostname: &str) -> bool {
        match self {
            RotateSelection::None => false,
            RotateSelection::All => true,
            RotateSelection::Named(set) => set.contains(hostname),
        }
    }
}

fn resolve_keys(
    topo: &ValidatedTopology,
    old_configs: &BTreeMap<String, ParsedConfig>,
    rotate: &RotateSelection,
) -> Result<KeyResolution, ApplyError> {
    let mut resolved = ResolvedKeys::default();
    let mut reused = Vec::new();
    let mut freshly_keyed = Vec::new();
    let mut is_fresh: BTreeMap<String, bool> = BTreeMap::new();

    for node in &topo.nodes {
        let existing = if rotate.is_rotated(&node.hostname) {
            None
        } else {
            old_configs.get(&node.hostname)
        };
        if let Some(old) = existing {
            let kp = keys::Keypair::from_private_key_base64(&old.interface.private_key).map_err(
                |source| ApplyError::BadStoredKey {
                    hostname: node.hostname.clone(),
                    source,
                },
            )?;
            resolved.node_keys.insert(
                node.hostname.clone(),
                (kp.private_key_base64(), kp.public_key_base64()),
            );
            reused.push(node.hostname.clone());
            is_fresh.insert(node.hostname.clone(), false);
        } else {
            let kp = keys::Keypair::generate();
            resolved.node_keys.insert(
                node.hostname.clone(),
                (kp.private_key_base64(), kp.public_key_base64()),
            );
            freshly_keyed.push(node.hostname.clone());
            is_fresh.insert(node.hostname.clone(), true);
        }
    }

    for (a, b) in topo.edges() {
        let a_fresh = is_fresh[&a];
        let b_fresh = is_fresh[&b];
        let psk = if a_fresh || b_fresh {
            keys::generate_preshared_key()
        } else {
            let a_pub = &resolved.node_keys[&a].1;
            let b_pub = &resolved.node_keys[&b].1;
            let a_record = old_configs
                .get(&a)
                .and_then(|c| c.peers.iter().find(|p| &p.public_key == b_pub))
                .map(|p| p.preshared_key.clone());
            let b_record = old_configs
                .get(&b)
                .and_then(|c| c.peers.iter().find(|p| &p.public_key == a_pub))
                .map(|p| p.preshared_key.clone());
            match (a_record, b_record) {
                (Some(pa), Some(pb)) if pa == pb => pa,
                (Some(_), Some(_)) => {
                    return Err(ApplyError::ConflictingPsk {
                        a: a.clone(),
                        b: b.clone(),
                    });
                }
                (Some(_), None) => {
                    return Err(ApplyError::OneSidedEdge {
                        a: a.clone(),
                        b: b.clone(),
                        only: a.clone(),
                    });
                }
                (None, Some(_)) => {
                    return Err(ApplyError::OneSidedEdge {
                        a: a.clone(),
                        b: b.clone(),
                        only: b.clone(),
                    });
                }
                (None, None) => keys::generate_preshared_key(),
            }
        };
        resolved.edge_psks.insert((a, b), psk);
    }

    Ok(KeyResolution {
        resolved,
        reused,
        freshly_keyed,
    })
}

/// Conditionally replace `current.json` with `intended`, reconciling an
/// ambiguous outcome per wg-server.md §10 "Publication commit": if a
/// `PreconditionFailed` turns out to already hold our intended bytes (a
/// lost response from our own write), treat it as success; if the
/// observed pointer still matches `baseline`'s revision, retry against
/// its fresh ETag; otherwise, another writer won — report a conflict.
async fn commit_pointer<S: Storage>(
    storage: &S,
    baseline: &CurrentJson,
    baseline_etag: &str,
    intended: &CurrentJson,
) -> Result<(), ApplyError> {
    let intended_bytes = serde_json::to_vec(intended).expect("CurrentJson always serializes");
    let mut etag = baseline_etag.to_string();
    for _ in 0..MAX_POINTER_RETRIES {
        match storage
            .put_if_match(
                CURRENT_JSON_KEY,
                intended_bytes.clone(),
                "application/json",
                &etag,
            )
            .await?
        {
            PutOutcome::Written(_) => return Ok(()),
            PutOutcome::PreconditionFailed => {
                let obj = storage
                    .get_object(CURRENT_JSON_KEY)
                    .await?
                    .ok_or(ApplyError::PointerCommitUnknown)?;
                if obj.bytes == intended_bytes {
                    return Ok(()); // our own write landed; a prior response was lost
                }
                let observed =
                    discovery::parse(std::str::from_utf8(&obj.bytes).unwrap_or_default())?;
                if observed.revision == baseline.revision {
                    etag = obj.etag;
                    continue;
                }
                return Err(ApplyError::PointerConflict);
            }
        }
    }
    Err(ApplyError::PointerRetriesExhausted)
}

pub async fn apply<S: Storage>(
    storage: &S,
    config: &TopologyConfig,
    opts: &ApplyOptions,
) -> Result<ApplyReport, ApplyError> {
    let (topo, warnings) = wg_common::topology::validate(config)?;
    let desired_hostnames: BTreeSet<String> =
        topo.nodes.iter().map(|n| n.hostname.clone()).collect();

    if let RotateSelection::Named(names) = &opts.rotate {
        for name in names {
            if !desired_hostnames.contains(name) {
                return Err(ApplyError::UnknownRotateHostname(name.clone()));
            }
        }
    }

    let (pointer, pointer_etag) = fetch_current_pointer(storage, opts.dry_run).await?;
    let old_configs = fetch_reusable_configs(storage, &pointer.current, &desired_hostnames).await?;

    let KeyResolution {
        resolved,
        reused,
        freshly_keyed,
    } = resolve_keys(&topo, &old_configs, &opts.rotate)?;

    let mut rendered: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut digests: BTreeMap<String, String> = BTreeMap::new();
    for hostname in &desired_hostnames {
        let text = render_node_config(&topo, &resolved, hostname)?;
        let bytes = text.into_bytes();
        digests.insert(hostname.clone(), digest_of(&bytes));
        rendered.insert(hostname.clone(), bytes);
    }

    let current_hostnames: BTreeSet<String> = pointer
        .current
        .as_ref()
        .map(|e| e.nodes.keys().cloned().collect())
        .unwrap_or_default();
    let unchanged = current_hostnames == desired_hostnames
        && pointer.current.as_ref().is_some_and(|e| e.nodes == digests);

    let mut report = ApplyReport {
        warnings,
        reused_hostnames: reused,
        freshly_keyed_hostnames: freshly_keyed,
        published: false,
        new_generation_id: None,
        uploaded_hostnames: Vec::new(),
        cleanup_deleted: Vec::new(),
        cleanup_skipped_within_grace: Vec::new(),
        cleanup_error: None,
    };

    let final_pointer = if unchanged {
        let refreshed = CurrentJson {
            schema_version: discovery::SCHEMA_VERSION,
            revision: base32::random_id(),
            current: pointer.current.clone(),
            previous: pointer.previous.clone(),
        };
        if !opts.dry_run {
            commit_pointer(storage, &pointer, &pointer_etag, &refreshed).await?;
        }
        refreshed
    } else {
        let new_id = base32::random_id();
        report.new_generation_id = Some(new_id.clone());
        let new_pointer = CurrentJson {
            schema_version: discovery::SCHEMA_VERSION,
            revision: base32::random_id(),
            current: Some(GenerationEntry {
                id: new_id.clone(),
                nodes: digests,
            }),
            previous: pointer.current.clone(),
        };

        if opts.dry_run {
            report.uploaded_hostnames = desired_hostnames.iter().cloned().collect();
        } else {
            for hostname in &desired_hostnames {
                let key = node_object_key(hostname, &new_id);
                let body = rendered[hostname].clone();
                match storage
                    .put_if_absent(&key, body.clone(), "text/plain; charset=utf-8")
                    .await?
                {
                    PutOutcome::Written(_) => {
                        report.uploaded_hostnames.push(hostname.clone());
                    }
                    PutOutcome::PreconditionFailed => {
                        let existing = storage.get_object(&key).await?.ok_or_else(|| {
                            ApplyError::GenerationIdCollision {
                                hostname: hostname.clone(),
                            }
                        })?;
                        if existing.bytes != body {
                            return Err(ApplyError::GenerationIdCollision {
                                hostname: hostname.clone(),
                            });
                        }
                        report.uploaded_hostnames.push(hostname.clone());
                    }
                }
            }
            commit_pointer(storage, &pointer, &pointer_etag, &new_pointer).await?;
            report.published = true;
        }
        new_pointer
    };

    // Retention cleanup runs on every pass, changed or not (wg-server.md
    // §10 "Retention"). A cleanup failure doesn't invalidate a successful
    // publication, but the caller (main.rs) still exits non-zero for it.
    match retention::cleanup(storage, &final_pointer, opts.grace_period, opts.dry_run).await {
        Ok(r) => {
            report.cleanup_deleted = r.deleted;
            report.cleanup_skipped_within_grace = r.skipped_within_grace;
        }
        Err(e) => report.cleanup_error = Some(e.to_string()),
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use wg_common::topology::{NodeConfig, R2Config, WgConfig};

    fn r2() -> R2Config {
        R2Config {
            endpoint: "https://example.r2.cloudflarestorage.com".into(),
            read_write_access_key_id: "id".into(),
            read_write_secret_access_key: "secret".into(),
            session_token: None,
            region: "auto".into(),
            bucket: "wg-confs".into(),
        }
    }

    fn node(hostname: &str, addr: &str, endpoint: Option<&str>) -> NodeConfig {
        NodeConfig {
            hostname: hostname.into(),
            wg_config: WgConfig {
                tunnel_address: format!("{addr}/32"),
                listen_port: 51820,
                endpoint: endpoint.map(String::from),
                dns: None,
                mtu: None,
                extra_allowed_ips: Vec::new(),
                persistent_keepalive: None,
            },
        }
    }

    fn two_master_two_spoke() -> TopologyConfig {
        TopologyConfig {
            r2_config: r2(),
            nodes: vec![
                node("master-us", "10.10.0.1", Some("203.0.113.10:51820")),
                node("master-eu", "10.10.0.2", Some("198.51.100.20:51820")),
                node("workstation-01", "10.10.0.100", None),
                node("mobile-01", "10.10.0.101", None),
            ],
            master: vec!["master-us".into(), "master-eu".into()],
        }
    }

    #[tokio::test]
    async fn first_apply_on_empty_bucket_publishes_a_generation() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        let report = apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(report.published);
        assert_eq!(report.freshly_keyed_hostnames.len(), 4);
        assert!(report.reused_hostnames.is_empty());
        assert_eq!(report.uploaded_hostnames.len(), 4);
        // current.json + 4 node files
        assert_eq!(storage.object_count(), 5);
    }

    #[tokio::test]
    async fn second_apply_with_unchanged_config_is_a_no_op() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let before = storage.object_count();

        let report = apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!report.published);
        assert_eq!(report.reused_hostnames.len(), 4);
        assert!(report.freshly_keyed_hostnames.is_empty());
        assert_eq!(storage.object_count(), before);
    }

    #[tokio::test]
    async fn unchanged_pass_still_refreshes_the_pointer_revision() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = storage.get_object(CURRENT_JSON_KEY).await.unwrap().unwrap();
        let first_doc: CurrentJson = serde_json::from_slice(&first.bytes).unwrap();

        apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let second = storage.get_object(CURRENT_JSON_KEY).await.unwrap().unwrap();
        let second_doc: CurrentJson = serde_json::from_slice(&second.bytes).unwrap();

        assert_eq!(first_doc.current, second_doc.current);
        assert_ne!(first_doc.revision, second_doc.revision);
        assert_ne!(first.etag, second.etag);
    }

    #[tokio::test]
    async fn adding_a_node_reuses_existing_keys_and_edges() {
        let storage = MockStorage::new();
        let mut cfg = two_master_two_spoke();
        apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        cfg.nodes.push(node("mobile-02", "10.10.0.102", None));
        let report = apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(report.published);
        assert_eq!(
            report.freshly_keyed_hostnames,
            vec!["mobile-02".to_string()]
        );
        assert_eq!(report.reused_hostnames.len(), 4);
        // All 5 nodes' files get uploaded under the new generation, even
        // the 4 whose content is unaffected (wg-server.md §8 step 6).
        assert_eq!(report.uploaded_hostnames.len(), 5);
    }

    #[tokio::test]
    async fn rotate_named_only_affects_that_node_and_its_edges() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let opts = ApplyOptions {
            dry_run: false,
            rotate: RotateSelection::Named(["master-us".to_string()].into_iter().collect()),
            ..Default::default()
        };
        let report = apply(&storage, &cfg, &opts).await.unwrap();
        assert!(report.published);
        assert_eq!(
            report.freshly_keyed_hostnames,
            vec!["master-us".to_string()]
        );
        // master-eu, workstation-01, mobile-01 all keep their own identity
        // keys, but every one of them has an edge to master-us and so
        // gets a re-uploaded file (new PSK/peer public key), even though
        // none of them is itself "freshly keyed".
        assert_eq!(report.reused_hostnames.len(), 3);
        assert_eq!(report.uploaded_hostnames.len(), 4);
    }

    #[tokio::test]
    async fn bare_rotate_regenerates_every_key() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let opts = ApplyOptions {
            dry_run: false,
            rotate: RotateSelection::All,
            ..Default::default()
        };
        let report = apply(&storage, &cfg, &opts).await.unwrap();
        assert!(report.published);
        assert_eq!(report.freshly_keyed_hostnames.len(), 4);
        assert!(report.reused_hostnames.is_empty());
    }

    #[tokio::test]
    async fn rotate_rejects_unknown_hostname() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        let opts = ApplyOptions {
            dry_run: false,
            rotate: RotateSelection::Named(["nonexistent".to_string()].into_iter().collect()),
            ..Default::default()
        };
        let err = apply(&storage, &cfg, &opts).await.unwrap_err();
        assert!(matches!(err, ApplyError::UnknownRotateHostname(_)));
    }

    #[tokio::test]
    async fn cleanup_removes_orphaned_generations_past_grace_period() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        let opts = ApplyOptions {
            grace_period: std::time::Duration::ZERO,
            ..Default::default()
        };

        apply(&storage, &cfg, &opts).await.unwrap(); // generation 1
        let mut cfg2 = cfg.clone();
        cfg2.nodes[0].wg_config.mtu = Some(1400); // force a change -> generation 2
        apply(&storage, &cfg2, &opts).await.unwrap();
        let mut cfg3 = cfg2.clone();
        cfg3.nodes[0].wg_config.mtu = Some(1401); // generation 3
        let report = apply(&storage, &cfg3, &opts).await.unwrap();

        // Only current + previous generations' objects remain, plus
        // current.json: (4 nodes * 2 generations) + 1.
        assert_eq!(storage.object_count(), 4 * 2 + 1);
        assert!(!report.cleanup_deleted.is_empty());
        assert!(report.cleanup_error.is_none());
    }

    #[tokio::test]
    async fn dry_run_makes_no_storage_writes() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        let report = apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!report.published);
        assert_eq!(report.uploaded_hostnames.len(), 4); // planned, not actually uploaded
        assert_eq!(storage.object_count(), 0);
    }

    #[tokio::test]
    async fn upload_failure_leaves_current_json_untouched() {
        let storage = MockStorage::new();
        let cfg = two_master_two_spoke();
        *storage.fail_next_puts.lock().unwrap() = 1;
        let result = apply(
            &storage,
            &cfg,
            &ApplyOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err());
        // Nothing should have been published: current.json was never
        // touched because the upload batch (which precedes any pointer
        // write) failed first.
        assert_eq!(storage.object_count(), 0);
    }

    #[test]
    fn resolve_keys_detects_one_sided_edge() {
        let cfg = two_master_two_spoke();
        let (topo, _) = wg_common::topology::validate(&cfg).unwrap();

        // Build a self-consistent "current generation" for master-us and
        // master-eu, then corrupt it: master-us's file will record a peer
        // stanza for master-eu, but master-eu's file will not reciprocate.
        let us_kp = keys::Keypair::generate();
        let eu_kp = keys::Keypair::generate();
        let psk = keys::generate_preshared_key();

        let us_conf = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.10.0.1/32\nListenPort = 51820\n\n[Peer]\nPublicKey = {}\nPresharedKey = {psk}\nAllowedIPs = 10.10.0.2/32\n",
            us_kp.private_key_base64(),
            eu_kp.public_key_base64(),
        );
        // master-eu's own file has NO peer entry for master-us at all.
        let eu_conf = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.10.0.2/32\nListenPort = 51820\n",
            eu_kp.private_key_base64(),
        );

        let mut old_configs = BTreeMap::new();
        old_configs.insert(
            "master-us".to_string(),
            render::parse_wg_quick(&us_conf).unwrap(),
        );
        old_configs.insert(
            "master-eu".to_string(),
            render::parse_wg_quick(&eu_conf).unwrap(),
        );

        let err = resolve_keys(&topo, &old_configs, &RotateSelection::None).unwrap_err();
        assert!(matches!(err, ApplyError::OneSidedEdge { .. }));
    }

    #[test]
    fn resolve_keys_detects_conflicting_psk() {
        let cfg = two_master_two_spoke();
        let (topo, _) = wg_common::topology::validate(&cfg).unwrap();

        let us_kp = keys::Keypair::generate();
        let eu_kp = keys::Keypair::generate();
        let psk_a = keys::generate_preshared_key();
        let psk_b = keys::generate_preshared_key();

        let us_conf = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.10.0.1/32\nListenPort = 51820\n\n[Peer]\nPublicKey = {}\nPresharedKey = {psk_a}\nAllowedIPs = 10.10.0.2/32\n",
            us_kp.private_key_base64(),
            eu_kp.public_key_base64(),
        );
        let eu_conf = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.10.0.2/32\nListenPort = 51820\n\n[Peer]\nPublicKey = {}\nPresharedKey = {psk_b}\nAllowedIPs = 10.10.0.1/32\n",
            eu_kp.private_key_base64(),
            us_kp.public_key_base64(),
        );

        let mut old_configs = BTreeMap::new();
        old_configs.insert(
            "master-us".to_string(),
            render::parse_wg_quick(&us_conf).unwrap(),
        );
        old_configs.insert(
            "master-eu".to_string(),
            render::parse_wg_quick(&eu_conf).unwrap(),
        );

        let err = resolve_keys(&topo, &old_configs, &RotateSelection::None).unwrap_err();
        assert!(matches!(err, ApplyError::ConflictingPsk { .. }));
    }
}
