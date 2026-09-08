//! Crockford Base32 encoding of a 32-byte value, treated as one big-endian
//! 256-bit integer (per <https://www.crockford.com/base32.html>), not as
//! RFC 4648 byte-grouped Base32. See wg-server.md §10 for why this
//! distinction matters and must not be reimplemented via a stock RFC 4648
//! Base32 crate.

use thiserror::Error;

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
pub const ENCODED_LEN: usize = 52;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Base32Error {
    #[error("expected exactly {ENCODED_LEN} characters, got {0}")]
    WrongLength(usize),
    #[error("invalid character {0:?} at position {1}")]
    InvalidChar(char, usize),
    #[error("value overflows 256 bits")]
    Overflow,
}

/// Encode 32 bytes (big-endian) as a 52-character lowercase Crockford
/// Base32 string.
pub fn encode(bytes: &[u8; 32]) -> String {
    let mut num = *bytes;
    let mut digits = [0u8; ENCODED_LEN];
    for slot in digits.iter_mut().rev() {
        let mut rem: u16 = 0;
        for byte in num.iter_mut() {
            let cur = (rem << 8) | (*byte as u16);
            *byte = (cur / 32) as u8;
            rem = cur % 32;
        }
        *slot = ALPHABET[rem as usize];
    }
    // SAFETY: every byte written above is one of ALPHABET's ASCII bytes.
    String::from_utf8(digits.to_vec()).expect("alphabet is ASCII")
}

/// Decode a 52-character lowercase Crockford Base32 string back into 32
/// bytes (big-endian). Rejects wrong length, any character outside the
/// canonical lowercase alphabet (including uppercase and the human-input
/// aliases i/l/o, which are simply absent from `ALPHABET`), and a value
/// that would overflow 256 bits.
pub fn decode(s: &str) -> Result<[u8; 32], Base32Error> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() != ENCODED_LEN {
        return Err(Base32Error::WrongLength(chars.len()));
    }
    let mut acc = [0u8; 32];
    for (i, ch) in chars.iter().enumerate() {
        let digit = ALPHABET
            .iter()
            .position(|&a| a == *ch as u8)
            .ok_or(Base32Error::InvalidChar(*ch, i))? as u16;
        let mut carry = digit;
        for byte in acc.iter_mut().rev() {
            let cur = (*byte as u16) * 32 + carry;
            *byte = cur as u8;
            carry = cur >> 8;
        }
        if carry != 0 {
            return Err(Base32Error::Overflow);
        }
    }
    Ok(acc)
}

/// `true` iff `s` is exactly 52 characters from the canonical lowercase
/// alphabet — the validation rule from wg-server.md §10
/// (`^[01][0-9abcdefghjkmnpqrstvwxyz]{51}$`), checked structurally here
/// rather than via a second regex.
pub fn is_canonical(s: &str) -> bool {
    decode(s).is_ok() && matches!(s.as_bytes().first(), Some(b'0') | Some(b'1'))
}

/// A fresh random 52-character value from 32 CSPRNG bytes — used for
/// generation IDs and pointer revisions alike (wg-server.md §10), which
/// share this exact encoding and randomness source.
pub fn random_id() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS CSPRNG must be available");
    encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_bytes_encode_to_all_zero_digits() {
        let encoded = encode(&[0u8; 32]);
        assert_eq!(encoded, "0".repeat(52));
        assert_eq!(encoded.len(), ENCODED_LEN);
    }

    #[test]
    fn max_bytes_encode_to_leading_one_then_all_z() {
        let encoded = encode(&[0xffu8; 32]);
        assert_eq!(encoded, format!("1{}", "z".repeat(51)));
    }

    #[test]
    fn single_low_bit_encodes_to_trailing_one() {
        let mut bytes = [0u8; 32];
        bytes[31] = 1;
        let encoded = encode(&bytes);
        assert_eq!(encoded, format!("{}1", "0".repeat(51)));
    }

    #[test]
    fn round_trips_arbitrary_bytes() {
        let bytes: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
        let encoded = encode(&bytes);
        assert_eq!(encoded.len(), ENCODED_LEN);
        assert_eq!(decode(&encoded).unwrap(), bytes);
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(decode("0"), Err(Base32Error::WrongLength(1)));
        assert_eq!(decode(&"0".repeat(53)), Err(Base32Error::WrongLength(53)));
    }

    #[test]
    fn rejects_uppercase_and_aliases() {
        // Uppercase is rejected because ALPHABET only contains lowercase.
        let upper = "0".repeat(51) + "A";
        assert!(matches!(
            decode(&upper),
            Err(Base32Error::InvalidChar('A', 51))
        ));
        // i, l, o are human-input aliases deliberately excluded from the alphabet.
        for alias in ['i', 'l', 'o'] {
            let s = "0".repeat(51) + &alias.to_string();
            assert!(matches!(decode(&s), Err(Base32Error::InvalidChar(c, 51)) if c == alias));
        }
    }

    #[test]
    fn is_canonical_rejects_nonzero_unused_high_bits() {
        // A 52-char string starting with '2' would require >256 bits and
        // must be rejected by is_canonical (and caught as overflow by decode
        // once the accumulation actually exceeds 256 bits).
        let s = format!("2{}", "0".repeat(51));
        assert!(!is_canonical(&s));
    }

    #[test]
    fn is_canonical_accepts_boundary_values() {
        assert!(is_canonical(&"0".repeat(52)));
        assert!(is_canonical(&encode(&[0xff; 32])));
    }
}
