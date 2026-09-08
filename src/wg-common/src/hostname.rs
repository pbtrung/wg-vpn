//! Hostname validation shared by `wg-server` and `wg-client`
//! (wg-server.md §6 / wg-client.md §5): DNS-label-safe, safe as an S3
//! object-key path segment and as a filename.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HostnameError {
    #[error("hostname must not be empty")]
    Empty,
    #[error("hostname must be at most 63 characters, got {0}")]
    TooLong(usize),
    #[error(
        "hostname {0:?} must match ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ (lowercase, no leading/trailing hyphen, no underscores or dots)"
    )]
    InvalidFormat(String),
}

/// Validates against `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`.
pub fn validate(hostname: &str) -> Result<(), HostnameError> {
    if hostname.is_empty() {
        return Err(HostnameError::Empty);
    }
    if hostname.len() > 63 {
        return Err(HostnameError::TooLong(hostname.len()));
    }
    let bytes = hostname.as_bytes();
    let is_alnum_lower = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let first_ok = is_alnum_lower(bytes[0]);
    let last_ok = is_alnum_lower(bytes[bytes.len() - 1]);
    let middle_ok = if bytes.len() <= 2 {
        true
    } else {
        bytes[1..bytes.len() - 1]
            .iter()
            .all(|&b| is_alnum_lower(b) || b == b'-')
    };
    if first_ok && last_ok && middle_ok {
        Ok(())
    } else {
        Err(HostnameError::InvalidFormat(hostname.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_boundary_cases() {
        assert!(validate("a").is_ok());
        assert!(validate("9").is_ok());
        assert!(validate(&"a".repeat(63)).is_ok());
        assert!(validate("master-us").is_ok());
        assert!(validate("workstation-01").is_ok());
    }

    #[test]
    fn rejects_too_long() {
        assert_eq!(validate(&"a".repeat(64)), Err(HostnameError::TooLong(64)));
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate(""), Err(HostnameError::Empty));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(validate("Master-US").is_err());
    }

    #[test]
    fn rejects_underscore() {
        assert!(validate("master_us").is_err());
    }

    #[test]
    fn rejects_leading_and_trailing_hyphen() {
        assert!(validate("-master").is_err());
        assert!(validate("master-").is_err());
    }

    #[test]
    fn rejects_fqdn_suffix() {
        assert!(validate("workstation-01.corp.example.com").is_err());
    }
}
