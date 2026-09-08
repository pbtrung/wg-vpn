//! `current.json` discovery-pointer schema and validation
//! (wg-server.md §10 "Storage layout and generation discovery").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{base32, hostname, limits, strict_json};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationEntry {
    pub id: String,
    /// hostname -> SHA-256 digest of that node's rendered `.conf`, both in
    /// the shared 52-character Crockford Base32 encoding.
    pub nodes: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CurrentJson {
    pub schema_version: u32,
    pub revision: String,
    pub current: Option<GenerationEntry>,
    pub previous: Option<GenerationEntry>,
}

impl CurrentJson {
    /// The empty, just-initialized state (wg-server.md §10
    /// "Initialization"): no generation published yet.
    pub fn empty(revision: String) -> Self {
        CurrentJson {
            schema_version: SCHEMA_VERSION,
            revision,
            current: None,
            previous: None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DiscoveryError {
    #[error("duplicate JSON key: {0}")]
    DuplicateJsonKey(String),
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported schema_version {0} (expected {SCHEMA_VERSION})")]
    UnsupportedSchemaVersion(u32),
    #[error("revision is not a canonical 52-character Crockford Base32 value")]
    InvalidRevision,
    #[error("generation id is not a canonical 52-character Crockford Base32 value: {0:?}")]
    InvalidGenerationId(String),
    #[error("digest for hostname {0:?} is not a canonical 52-character Crockford Base32 value")]
    InvalidDigest(String),
    #[error("generation's node map must be non-empty")]
    EmptyNodeMap,
    #[error("generation's node map has {0} entries, exceeding the limit of {max}", max = limits::MAX_NODES)]
    TooManyNodes(usize),
    #[error("invalid hostname {hostname:?} in generation node map: {source}")]
    InvalidHostname {
        hostname: String,
        source: hostname::HostnameError,
    },
    #[error("previous generation set without a current generation")]
    PreviousWithoutCurrent,
    #[error("current and previous generations must not share the same id")]
    EqualCurrentPreviousIds,
}

pub fn parse(text: &str) -> Result<CurrentJson, DiscoveryError> {
    strict_json::check_no_duplicate_keys(text)
        .map_err(|e| DiscoveryError::DuplicateJsonKey(e.to_string()))?;
    serde_json::from_str(text).map_err(|e| DiscoveryError::InvalidJson(e.to_string()))
}

fn validate_generation_entry(g: &GenerationEntry) -> Result<(), DiscoveryError> {
    if !base32::is_canonical(&g.id) {
        return Err(DiscoveryError::InvalidGenerationId(g.id.clone()));
    }
    if g.nodes.is_empty() {
        return Err(DiscoveryError::EmptyNodeMap);
    }
    if g.nodes.len() > limits::MAX_NODES {
        return Err(DiscoveryError::TooManyNodes(g.nodes.len()));
    }
    for (h, digest) in &g.nodes {
        hostname::validate(h).map_err(|source| DiscoveryError::InvalidHostname {
            hostname: h.clone(),
            source,
        })?;
        if !base32::is_canonical(digest) {
            return Err(DiscoveryError::InvalidDigest(h.clone()));
        }
    }
    Ok(())
}

pub fn validate(doc: &CurrentJson) -> Result<(), DiscoveryError> {
    if doc.schema_version != SCHEMA_VERSION {
        return Err(DiscoveryError::UnsupportedSchemaVersion(doc.schema_version));
    }
    if !base32::is_canonical(&doc.revision) {
        return Err(DiscoveryError::InvalidRevision);
    }
    if doc.current.is_none() && doc.previous.is_some() {
        return Err(DiscoveryError::PreviousWithoutCurrent);
    }
    if let (Some(c), Some(p)) = (&doc.current, &doc.previous)
        && c.id == p.id
    {
        return Err(DiscoveryError::EqualCurrentPreviousIds);
    }
    for g in [&doc.current, &doc.previous].into_iter().flatten() {
        validate_generation_entry(g)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> String {
        base32::encode(&[n; 32])
    }

    #[test]
    fn empty_pointer_is_valid() {
        let doc = CurrentJson::empty(id(1));
        assert!(validate(&doc).is_ok());
    }

    #[test]
    fn rejects_previous_without_current() {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".to_string(), id(9));
        let doc = CurrentJson {
            schema_version: SCHEMA_VERSION,
            revision: id(1),
            current: None,
            previous: Some(GenerationEntry { id: id(2), nodes }),
        };
        assert_eq!(validate(&doc), Err(DiscoveryError::PreviousWithoutCurrent));
    }

    #[test]
    fn rejects_equal_current_previous_ids() {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".to_string(), id(9));
        let entry = GenerationEntry {
            id: id(2),
            nodes: nodes.clone(),
        };
        let doc = CurrentJson {
            schema_version: SCHEMA_VERSION,
            revision: id(1),
            current: Some(entry.clone()),
            previous: Some(entry),
        };
        assert_eq!(validate(&doc), Err(DiscoveryError::EqualCurrentPreviousIds));
    }

    #[test]
    fn rejects_empty_node_map() {
        let entry = GenerationEntry {
            id: id(2),
            nodes: BTreeMap::new(),
        };
        let doc = CurrentJson {
            schema_version: SCHEMA_VERSION,
            revision: id(1),
            current: Some(entry),
            previous: None,
        };
        assert_eq!(validate(&doc), Err(DiscoveryError::EmptyNodeMap));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let doc = CurrentJson {
            schema_version: 2,
            revision: id(1),
            current: None,
            previous: None,
        };
        assert_eq!(
            validate(&doc),
            Err(DiscoveryError::UnsupportedSchemaVersion(2))
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = format!(
            r#"{{"schema_version":1,"revision":"{}","current":null,"previous":null,"extra":1}}"#,
            id(1)
        );
        assert!(parse(&text).is_err());
    }

    #[test]
    fn rejects_duplicate_keys() {
        let text = format!(
            r#"{{"schema_version":1,"schema_version":1,"revision":"{}","current":null,"previous":null}}"#,
            id(1)
        );
        assert!(matches!(
            parse(&text),
            Err(DiscoveryError::DuplicateJsonKey(_))
        ));
    }
}
