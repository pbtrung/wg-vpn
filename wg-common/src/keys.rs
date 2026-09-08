//! WireGuard key material: X25519 keypairs and preshared keys, in the
//! Base64 encoding WireGuard configs use on the wire.

use base64::{Engine as _, prelude::BASE64_STANDARD};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};

pub const KEY_LEN: usize = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("key is not valid base64: {0}")]
    InvalidBase64(String),
    #[error("key must decode to exactly {KEY_LEN} bytes, got {0}")]
    WrongLength(usize),
    #[error("all-zero keys are rejected (they disable the key/PSK setting)")]
    ZeroKey,
}

fn decode_key_bytes(b64: &str) -> Result<[u8; KEY_LEN], KeyError> {
    let bytes = BASE64_STANDARD
        .decode(b64.trim_end())
        .map_err(|e| KeyError::InvalidBase64(e.to_string()))?;
    let len = bytes.len();
    let arr: [u8; KEY_LEN] = bytes.try_into().map_err(|_| KeyError::WrongLength(len))?;
    if arr == [0u8; KEY_LEN] {
        return Err(KeyError::ZeroKey);
    }
    Ok(arr)
}

pub fn encode_key(bytes: &[u8; KEY_LEN]) -> String {
    BASE64_STANDARD.encode(bytes)
}

/// A node's X25519 keypair, decoded/validated from a Base64 private key
/// (reuse path) or freshly generated (new node / `--rotate` path).
#[derive(Clone)]
pub struct Keypair {
    secret: StaticSecret,
}

impl Keypair {
    /// Generate a fresh, randomly generated keypair via the OS CSPRNG.
    pub fn generate() -> Self {
        Keypair {
            secret: StaticSecret::random(),
        }
    }

    /// Reuse an existing private key, e.g. parsed from a currently
    /// published `.conf`'s `PrivateKey =` line.
    pub fn from_private_key_base64(b64: &str) -> Result<Self, KeyError> {
        let bytes = decode_key_bytes(b64)?;
        Ok(Keypair {
            secret: StaticSecret::from(bytes),
        })
    }

    pub fn private_key_base64(&self) -> String {
        encode_key(&self.secret.to_bytes())
    }

    pub fn public_key_base64(&self) -> String {
        encode_key(PublicKey::from(&self.secret).as_bytes())
    }

    /// A distinct-keypair equality check used by fleet-wide duplicate
    /// detection (wg-server.md §8): compare by public key, never by
    /// private key or hostname.
    pub fn public_key_bytes(&self) -> [u8; KEY_LEN] {
        *PublicKey::from(&self.secret).as_bytes()
    }
}

/// Generic WireGuard key format validation: valid Base64, exactly 32
/// bytes, not all-zero. The same structural rule applies to private,
/// public, and preshared keys alike — only how the bytes are later used
/// differs.
pub fn validate_key_bytes(b64: &str) -> Result<[u8; KEY_LEN], KeyError> {
    decode_key_bytes(b64)
}

/// Validate a standalone public key string (e.g. a peer's `PublicKey =`
/// line when parsing a downloaded configuration).
pub fn validate_public_key(b64: &str) -> Result<[u8; KEY_LEN], KeyError> {
    decode_key_bytes(b64)
}

/// A preshared key: 32 raw random bytes, no clamping (it's a symmetric
/// value, not an X25519 scalar).
pub fn generate_preshared_key() -> String {
    let mut bytes = [0u8; KEY_LEN];
    getrandom::fill(&mut bytes).expect("OS CSPRNG must be available");
    encode_key(&bytes)
}

/// Validate a standalone preshared key string.
pub fn validate_preshared_key(b64: &str) -> Result<[u8; KEY_LEN], KeyError> {
    decode_key_bytes(b64)
}

/// Derive the Base64 public key corresponding to a Base64 private key,
/// e.g. to check whether a downloaded config's peer matches its own
/// local interface key.
pub fn public_key_from_private_base64(b64: &str) -> Result<String, KeyError> {
    Ok(Keypair::from_private_key_base64(b64)?.public_key_base64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keypair_round_trips_through_base64() {
        let kp = Keypair::generate();
        let priv_b64 = kp.private_key_base64();
        let reused = Keypair::from_private_key_base64(&priv_b64).unwrap();
        assert_eq!(reused.public_key_base64(), kp.public_key_base64());
    }

    #[test]
    fn rejects_zero_private_key() {
        let zero = encode_key(&[0u8; KEY_LEN]);
        assert_eq!(
            Keypair::from_private_key_base64(&zero).err(),
            Some(KeyError::ZeroKey)
        );
    }

    #[test]
    fn rejects_zero_public_key() {
        let zero = encode_key(&[0u8; KEY_LEN]);
        assert_eq!(validate_public_key(&zero), Err(KeyError::ZeroKey));
    }

    #[test]
    fn rejects_zero_preshared_key() {
        let zero = encode_key(&[0u8; KEY_LEN]);
        assert_eq!(validate_preshared_key(&zero), Err(KeyError::ZeroKey));
    }

    #[test]
    fn rejects_non_canonical_base64() {
        assert!(matches!(
            validate_public_key("not valid base64!!"),
            Err(KeyError::InvalidBase64(_))
        ));
    }

    #[test]
    fn rejects_wrong_decoded_length() {
        let short = BASE64_STANDARD.encode([1u8; 16]);
        assert_eq!(validate_public_key(&short), Err(KeyError::WrongLength(16)));
    }

    #[test]
    fn generated_preshared_keys_are_nonzero_and_distinct() {
        let a = generate_preshared_key();
        let b = generate_preshared_key();
        assert_ne!(a, b);
        assert!(validate_preshared_key(&a).is_ok());
    }
}
