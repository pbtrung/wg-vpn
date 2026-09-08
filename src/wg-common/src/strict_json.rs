//! `serde_json` silently keeps the *last* occurrence of a duplicate object
//! key rather than erroring, so "reject duplicate JSON keys" (required
//! throughout wg-server.md §6 and wg-client.md §4) needs a small
//! independent pre-pass over the raw text before handing it to `serde_json`.
//!
//! This is a minimal structural scanner, not a full JSON parser: it only
//! tracks object/array nesting and string boundaries well enough to know
//! whether a given string literal is in "key position" within an object.
//! Escape sequences are consumed but not fully decoded (`\uXXXX` becomes
//! four literal hex-digit characters rather than the real code point) —
//! that only matters for the pathological case of two differently-escaped
//! spellings of the same key, which is out of scope for this check.

use std::collections::HashSet;
use std::iter::Peekable;
use std::str::Chars;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StrictJsonError {
    #[error("duplicate key {0:?} in JSON object")]
    DuplicateKey(String),
    #[error("malformed JSON: {0}")]
    Malformed(&'static str),
}

enum Ctx {
    Object {
        seen: HashSet<String>,
        expect_key: bool,
    },
    Array,
}

pub fn check_no_duplicate_keys(text: &str) -> Result<(), StrictJsonError> {
    let mut stack: Vec<Ctx> = Vec::new();
    let mut chars = text.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '{' => {
                chars.next();
                stack.push(Ctx::Object {
                    seen: HashSet::new(),
                    expect_key: true,
                });
            }
            '}' => {
                chars.next();
                match stack.pop() {
                    Some(Ctx::Object { .. }) => {}
                    _ => return Err(StrictJsonError::Malformed("unexpected }")),
                }
            }
            '[' => {
                chars.next();
                stack.push(Ctx::Array);
            }
            ']' => {
                chars.next();
                match stack.pop() {
                    Some(Ctx::Array) => {}
                    _ => return Err(StrictJsonError::Malformed("unexpected ]")),
                }
            }
            ',' => {
                chars.next();
                if let Some(Ctx::Object { expect_key, .. }) = stack.last_mut() {
                    *expect_key = true;
                }
            }
            ':' => {
                chars.next();
            }
            '"' => {
                let s = parse_string(&mut chars)?;
                if let Some(Ctx::Object { seen, expect_key }) = stack.last_mut()
                    && *expect_key
                {
                    if !seen.insert(s.clone()) {
                        return Err(StrictJsonError::DuplicateKey(s));
                    }
                    *expect_key = false;
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    if !stack.is_empty() {
        return Err(StrictJsonError::Malformed("unterminated object/array"));
    }
    Ok(())
}

fn parse_string(chars: &mut Peekable<Chars>) -> Result<String, StrictJsonError> {
    debug_assert_eq!(chars.peek(), Some(&'"'));
    chars.next();
    let mut s = String::new();
    loop {
        match chars.next() {
            None => return Err(StrictJsonError::Malformed("unterminated string")),
            Some('"') => return Ok(s),
            Some('\\') => match chars.next() {
                Some(esc) => s.push(esc),
                None => return Err(StrictJsonError::Malformed("unterminated escape")),
            },
            Some(c) => s.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_json_without_duplicates() {
        assert!(check_no_duplicate_keys(r#"{"a":1,"b":{"c":2},"d":[1,2,3]}"#).is_ok());
    }

    #[test]
    fn rejects_top_level_duplicate_key() {
        assert_eq!(
            check_no_duplicate_keys(r#"{"hostname":"a","hostname":"b"}"#),
            Err(StrictJsonError::DuplicateKey("hostname".into()))
        );
    }

    #[test]
    fn rejects_nested_duplicate_key() {
        assert_eq!(
            check_no_duplicate_keys(r#"{"wg_config":{"mtu":1,"mtu":2}}"#),
            Err(StrictJsonError::DuplicateKey("mtu".into()))
        );
    }

    #[test]
    fn does_not_confuse_string_values_for_keys() {
        // The literal text "hostname" appears twice here, but only once
        // in key position — must not be a false positive.
        assert!(check_no_duplicate_keys(r#"{"hostname":"hostname"}"#).is_ok());
    }

    #[test]
    fn allows_same_key_name_in_sibling_objects() {
        assert!(
            check_no_duplicate_keys(r#"{"nodes":[{"hostname":"a"},{"hostname":"b"}]}"#).is_ok()
        );
    }

    #[test]
    fn rejects_duplicate_key_inside_array_element_object() {
        assert_eq!(
            check_no_duplicate_keys(r#"{"nodes":[{"hostname":"a","hostname":"b"}]}"#),
            Err(StrictJsonError::DuplicateKey("hostname".into()))
        );
    }

    #[test]
    fn rejects_unbalanced_braces() {
        assert!(check_no_duplicate_keys(r#"{"a":1"#).is_err());
        assert!(check_no_duplicate_keys(r#"{"a":1}}"#).is_err());
    }
}
