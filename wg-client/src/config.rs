//! Client configuration schema (wg-client.md §4).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use wg_common::strict_json;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct R2ReadConfig {
    pub endpoint: String,
    pub read_only_access_key_id: String,
    pub read_only_secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    pub region: String,
    pub bucket: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub r2_config: R2ReadConfig,
    pub conf_path: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("duplicate JSON key: {0}")]
    DuplicateJsonKey(String),
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("invalid conf_path {0:?}: {1}")]
    InvalidConfPath(String, &'static str),
}

pub fn parse(text: &str) -> Result<ClientConfig, ConfigError> {
    strict_json::check_no_duplicate_keys(text)
        .map_err(|e| ConfigError::DuplicateJsonKey(e.to_string()))?;
    serde_json::from_str(text).map_err(|e| ConfigError::InvalidJson(e.to_string()))
}

/// The WireGuard interface name is the filename stem of `conf_path`
/// (wg-client.md §4): `/etc/wireguard/wg0.conf` -> `wg0`. Must be
/// 1-15 characters from `[a-zA-Z0-9_=+.-]`, excluding `.` and `..`, and
/// `conf_path` must be absolute.
pub fn interface_name(conf_path: &str) -> Result<String, ConfigError> {
    let path = Path::new(conf_path);
    if !path.is_absolute() {
        return Err(ConfigError::InvalidConfPath(
            conf_path.to_string(),
            "must be an absolute path",
        ));
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or(ConfigError::InvalidConfPath(
            conf_path.to_string(),
            "no filename",
        ))?;
    let ext = path.extension().and_then(|s| s.to_str());
    if ext != Some("conf") {
        return Err(ConfigError::InvalidConfPath(
            conf_path.to_string(),
            "filename must end in .conf",
        ));
    }
    if stem == "." || stem == ".." {
        return Err(ConfigError::InvalidConfPath(
            conf_path.to_string(),
            "interface name must not be . or ..",
        ));
    }
    let valid_len = !stem.is_empty() && stem.len() <= 15;
    let valid_chars = stem
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'=' | b'+' | b'.' | b'-'));
    if !valid_len || !valid_chars {
        return Err(ConfigError::InvalidConfPath(
            conf_path.to_string(),
            "interface name must be 1-15 characters from [a-zA-Z0-9_=+.-]",
        ));
    }
    Ok(stem.to_string())
}

pub fn conf_path_buf(conf_path: &str) -> PathBuf {
    PathBuf::from(conf_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_interface_name() {
        assert_eq!(interface_name("/etc/wireguard/wg0.conf").unwrap(), "wg0");
    }

    #[test]
    fn rejects_relative_path() {
        assert!(interface_name("wg0.conf").is_err());
    }

    #[test]
    fn rejects_non_conf_extension() {
        assert!(interface_name("/etc/wireguard/wg0.txt").is_err());
    }

    #[test]
    fn rejects_dot_dot_stem() {
        assert!(interface_name("/etc/wireguard/...conf").is_err());
    }

    #[test]
    fn accepts_exactly_15_char_interface_name() {
        assert!(interface_name("/etc/wireguard/wg0123456789012.conf").is_ok()); // 15 chars
    }

    #[test]
    fn rejects_16_char_interface_name() {
        assert!(interface_name("/etc/wireguard/wg01234567890123.conf").is_err()); // 16 chars
    }

    #[test]
    fn rejects_invalid_characters() {
        assert!(interface_name("/etc/wireguard/wg 0.conf").is_err());
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let text = r#"{"r2_config":{"endpoint":"https://x","read_only_access_key_id":"a","read_only_secret_access_key":"b","region":"auto","bucket":"c"},"conf_path":"/etc/wireguard/wg0.conf","extra":1}"#;
        assert!(parse(text).is_err());
    }

    #[test]
    fn parse_accepts_valid_config() {
        let text = r#"{"r2_config":{"endpoint":"https://x","read_only_access_key_id":"a","read_only_secret_access_key":"b","region":"auto","bucket":"c"},"conf_path":"/etc/wireguard/wg0.conf"}"#;
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.conf_path, "/etc/wireguard/wg0.conf");
    }
}
