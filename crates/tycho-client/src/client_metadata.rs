//! Generic client-metadata carried to the Tycho server in a dedicated header.
//!
//! This is the single source of truth for the header name and serialization so the RPC and
//! WebSocket paths can never drift. The map is deliberately untyped: `tycho-client` never learns
//! what the keys mean — consumers supply their own vocabulary.

use std::collections::HashMap;

use thiserror::Error;

/// Header name carrying serialized client metadata. Lowercase so it can be used with
/// `HeaderName::from_static`.
pub const CLIENT_METADATA_HEADER: &str = "x-tycho-client-metadata";

/// Maximum number of entries a client may send.
pub const MAX_ENTRIES: usize = 16;
/// Maximum key length in bytes.
pub const MAX_KEY_BYTES: usize = 64;
/// Maximum value length in bytes.
pub const MAX_VALUE_BYTES: usize = 128;
/// Maximum serialized header length in bytes.
pub const MAX_HEADER_BYTES: usize = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ClientMetadataError {
    #[error("invalid client metadata key: {0:?}")]
    InvalidKey(String),
    #[error("invalid client metadata value: {0:?}")]
    InvalidValue(String),
    #[error("too many client metadata entries: {0} (max {MAX_ENTRIES})")]
    TooManyEntries(usize),
    #[error("client metadata key too long: {0:?} (max {MAX_KEY_BYTES} bytes)")]
    KeyTooLong(String),
    #[error("client metadata value too long: {0:?} (max {MAX_VALUE_BYTES} bytes)")]
    ValueTooLong(String),
    #[error("serialized client metadata too long: {0} bytes (max {MAX_HEADER_BYTES})")]
    HeaderTooLong(usize),
}

/// Serializes client metadata into the `X-Tycho-Client-Metadata` header value.
///
/// Entries are emitted in key order as `key=value;key=value`. Returns `Ok(None)` for an empty
/// map, meaning no header should be sent (back-compatible default). Keys must be non-empty and
/// match `[A-Za-z0-9_.-]`; values must be non-empty visible ASCII excluding `;` and `=`. These
/// rules are stricter than `HeaderValue::from_str`, so any accepted output is always a valid
/// header value and the RPC path can never fail on serialized input.
pub(crate) fn serialize_client_metadata(
    meta: &HashMap<String, String>,
) -> Result<Option<String>, ClientMetadataError> {
    if meta.is_empty() {
        return Ok(None);
    }
    if meta.len() > MAX_ENTRIES {
        return Err(ClientMetadataError::TooManyEntries(meta.len()));
    }
    // Sort by key so the serialized header is deterministic regardless of map iteration order.
    let mut entries: Vec<_> = meta.iter().collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut parts = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        if !is_valid_key(key) {
            return Err(ClientMetadataError::InvalidKey(key.clone()));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(ClientMetadataError::KeyTooLong(key.clone()));
        }
        if !is_valid_value(value) {
            return Err(ClientMetadataError::InvalidValue(value.clone()));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(ClientMetadataError::ValueTooLong(value.clone()));
        }
        parts.push(format!("{key}={value}"));
    }
    let serialized = parts.join(";");
    if serialized.len() > MAX_HEADER_BYTES {
        return Err(ClientMetadataError::HeaderTooLong(serialized.len()));
    }
    Ok(Some(serialized))
}

fn is_valid_key(key: &str) -> bool {
    !key.is_empty() &&
        key.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn is_valid_value(value: &str) -> bool {
    !value.is_empty() &&
        value
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b';' && b != b'=')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn empty_map_yields_no_header() {
        assert_eq!(serialize_client_metadata(&HashMap::new()), Ok(None));
    }

    #[test]
    fn serializes_in_deterministic_key_order() {
        let meta = map(&[("preset", "best"), ("fynd_version", "0.57.0")]);
        assert_eq!(
            serialize_client_metadata(&meta),
            Ok(Some("fynd_version=0.57.0;preset=best".to_string()))
        );
    }

    #[test]
    fn rejects_invalid_keys() {
        for bad in ["", "has space", "semi;colon", "eq=uals", "unicod\u{00e9}"] {
            let meta = map(&[(bad, "v")]);
            assert!(
                matches!(serialize_client_metadata(&meta), Err(ClientMetadataError::InvalidKey(_))),
                "expected InvalidKey for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_values() {
        for bad in ["", "has space", "semi;colon", "eq=uals", "ctrl\u{0007}", "unicod\u{00e9}"] {
            let meta = map(&[("k", bad)]);
            assert!(
                matches!(
                    serialize_client_metadata(&meta),
                    Err(ClientMetadataError::InvalidValue(_))
                ),
                "expected InvalidValue for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_too_many_entries() {
        let meta: HashMap<String, String> = (0..MAX_ENTRIES + 1)
            .map(|i| (format!("k{i}"), "v".to_string()))
            .collect();
        assert_eq!(
            serialize_client_metadata(&meta),
            Err(ClientMetadataError::TooManyEntries(MAX_ENTRIES + 1))
        );
    }

    #[test]
    fn rejects_overlong_key() {
        let meta = map(&[("v", "ok")]);
        let long_key = "a".repeat(MAX_KEY_BYTES + 1);
        let mut meta2 = meta;
        meta2.insert(long_key.clone(), "v".to_string());
        assert_eq!(
            serialize_client_metadata(&meta2),
            Err(ClientMetadataError::KeyTooLong(long_key))
        );
    }

    #[test]
    fn rejects_overlong_value() {
        let long_value = "a".repeat(MAX_VALUE_BYTES + 1);
        let meta = map(&[("k", long_value.as_str())]);
        assert_eq!(
            serialize_client_metadata(&meta),
            Err(ClientMetadataError::ValueTooLong(long_value))
        );
    }

    #[test]
    fn rejects_overlong_header() {
        // Nine entries, each value at the per-value cap, exceed the 1 KiB header cap.
        let value = "a".repeat(MAX_VALUE_BYTES);
        let meta: HashMap<String, String> = (0..9)
            .map(|i| (format!("key{i}"), value.clone()))
            .collect();
        assert!(matches!(
            serialize_client_metadata(&meta),
            Err(ClientMetadataError::HeaderTooLong(_))
        ));
    }

    #[test]
    fn accepts_entries_at_the_caps() {
        let meta = map(&[
            ("k", "a".repeat(MAX_VALUE_BYTES).as_str()),
            ("a".repeat(MAX_KEY_BYTES).as_str(), "v"),
        ]);
        assert!(serialize_client_metadata(&meta).is_ok());
    }

    #[test]
    fn accepted_output_is_a_valid_header_value() {
        let meta = map(&[("fynd_version", "0.57.0"), ("preset", "best")]);
        let serialized = serialize_client_metadata(&meta)
            .unwrap()
            .unwrap();
        assert!(reqwest::header::HeaderValue::from_str(&serialized).is_ok());
    }
}
