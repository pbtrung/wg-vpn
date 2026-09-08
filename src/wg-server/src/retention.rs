//! Retention cleanup (wg-server.md §10 "Retention: current plus one
//! previous generation"): delete generation objects that are neither
//! `current` nor `previous`, with a minimum-age grace period as a
//! defense-in-depth layer under the scheduler's serialization guarantee.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use wg_common::{base32, discovery::CurrentJson, hostname};

use crate::apply::{ApplyError, NODES_PREFIX};
use crate::storage::Storage;

/// wg-server.md §10: "e.g. 15 minutes".
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default)]
pub struct RetentionReport {
    pub deleted: Vec<String>,
    pub skipped_within_grace: Vec<String>,
    pub skipped_unrecognized: Vec<String>,
}

fn parse_node_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix(NODES_PREFIX)?;
    let (host, filename) = rest.split_once('/')?;
    let id = filename.strip_suffix(".conf")?;
    if hostname::validate(host).is_err() {
        return None;
    }
    if !base32::is_canonical(id) {
        return None;
    }
    Some((host.to_string(), id.to_string()))
}

/// Delete every `nodes/` object whose generation ID is neither
/// `current.id` nor `previous.id`, except those younger than
/// `grace_period`. `--dry-run` computes and returns the same report
/// without deleting anything.
pub async fn cleanup<S: Storage>(
    storage: &S,
    pointer: &CurrentJson,
    grace_period: Duration,
    dry_run: bool,
) -> Result<RetentionReport, ApplyError> {
    let retained: BTreeSet<&str> = [
        pointer.current.as_ref().map(|e| e.id.as_str()),
        pointer.previous.as_ref().map(|e| e.id.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect();

    let entries = storage.list_all_with_last_modified(NODES_PREFIX).await?;
    let now = SystemTime::now();
    let mut report = RetentionReport::default();

    for (key, last_modified) in entries {
        let Some((_, id)) = parse_node_key(&key) else {
            report.skipped_unrecognized.push(key);
            continue;
        };
        if retained.contains(id.as_str()) {
            continue;
        }
        let age = now.duration_since(last_modified).unwrap_or(Duration::ZERO);
        if age < grace_period {
            report.skipped_within_grace.push(key);
            continue;
        }
        if !dry_run {
            storage.delete_object(&key).await?;
        }
        report.deleted.push(key);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use std::collections::BTreeMap;
    use wg_common::discovery::GenerationEntry;

    fn pointer_with(current_id: &str, previous_id: Option<&str>) -> CurrentJson {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".to_string(), base32::encode(&[1; 32]));
        CurrentJson {
            schema_version: wg_common::discovery::SCHEMA_VERSION,
            revision: base32::random_id(),
            current: Some(GenerationEntry {
                id: current_id.to_string(),
                nodes: nodes.clone(),
            }),
            previous: previous_id.map(|id| GenerationEntry {
                id: id.to_string(),
                nodes,
            }),
        }
    }

    #[tokio::test]
    async fn deletes_orphaned_generation_past_the_grace_period() {
        let storage = MockStorage::new();
        let old_id = base32::encode(&[9; 32]);
        let key = format!("nodes/a/{old_id}.conf");
        storage
            .put_if_absent(&key, b"x".to_vec(), "text/plain")
            .await
            .unwrap();
        storage.backdate(&key, Duration::from_secs(3600));

        let current_id = base32::encode(&[1; 32]);
        let pointer = pointer_with(&current_id, None);
        let report = cleanup(&storage, &pointer, DEFAULT_GRACE_PERIOD, false)
            .await
            .unwrap();

        assert_eq!(report.deleted, vec![key]);
        assert_eq!(storage.object_count(), 0);
    }

    #[tokio::test]
    async fn keeps_orphan_still_within_grace_period() {
        let storage = MockStorage::new();
        let old_id = base32::encode(&[9; 32]);
        let key = format!("nodes/a/{old_id}.conf");
        storage
            .put_if_absent(&key, b"x".to_vec(), "text/plain")
            .await
            .unwrap();
        // No backdate: freshly created, age ~0.

        let current_id = base32::encode(&[1; 32]);
        let pointer = pointer_with(&current_id, None);
        let report = cleanup(&storage, &pointer, DEFAULT_GRACE_PERIOD, false)
            .await
            .unwrap();

        assert!(report.deleted.is_empty());
        assert_eq!(report.skipped_within_grace, vec![key]);
        assert_eq!(storage.object_count(), 1);
    }

    #[tokio::test]
    async fn never_deletes_current_or_previous_regardless_of_age() {
        let storage = MockStorage::new();
        let current_id = base32::encode(&[1; 32]);
        let previous_id = base32::encode(&[2; 32]);
        let current_key = format!("nodes/a/{current_id}.conf");
        let previous_key = format!("nodes/a/{previous_id}.conf");
        storage
            .put_if_absent(&current_key, b"x".to_vec(), "text/plain")
            .await
            .unwrap();
        storage
            .put_if_absent(&previous_key, b"x".to_vec(), "text/plain")
            .await
            .unwrap();
        storage.backdate(&current_key, Duration::from_secs(999_999));
        storage.backdate(&previous_key, Duration::from_secs(999_999));

        let pointer = pointer_with(&current_id, Some(&previous_id));
        let report = cleanup(&storage, &pointer, DEFAULT_GRACE_PERIOD, false)
            .await
            .unwrap();

        assert!(report.deleted.is_empty());
        assert_eq!(storage.object_count(), 2);
    }

    #[tokio::test]
    async fn dry_run_reports_without_deleting() {
        let storage = MockStorage::new();
        let old_id = base32::encode(&[9; 32]);
        let key = format!("nodes/a/{old_id}.conf");
        storage
            .put_if_absent(&key, b"x".to_vec(), "text/plain")
            .await
            .unwrap();
        storage.backdate(&key, Duration::from_secs(3600));

        let current_id = base32::encode(&[1; 32]);
        let pointer = pointer_with(&current_id, None);
        let report = cleanup(&storage, &pointer, DEFAULT_GRACE_PERIOD, true)
            .await
            .unwrap();

        assert_eq!(report.deleted, vec![key]);
        assert_eq!(storage.object_count(), 1); // still there: dry run
    }
}
