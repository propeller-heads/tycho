//! Server-side parsing of the generic `X-Tycho-Client-Metadata` header.
//!
//! Clients send an opaque `key=value; key=value` map. The server parses it generically, applies
//! defensive size caps (never trusting the client to have validated), and projects a small
//! allowlist of keys into Prometheus labels. Values are emitted verbatim (after the caps): the
//! server never interprets them, so it stays free of any client-specific vocabulary. The allowlist
//! bounds label *names*, which is what protects the in-process metrics registry from being flooded
//! with arbitrary series; bounding label *value* cardinality, if ever needed, is left to the
//! Prometheus scrape layer.
//!
//! This mirrors the client serializer in `tycho-client` but deliberately does not depend on that
//! crate, so the two can ship independently.

use std::collections::HashMap;

use metrics::Label;

/// Header carrying generic client metadata.
pub(in crate::services) const CLIENT_METADATA_HEADER: &str = "x-tycho-client-metadata";

/// Metadata keys projected onto Prometheus labels. This is a projection policy, not an
/// interpretation of the values: the server decides which keys become labels (bounding label
/// names) but never inspects what they mean. Adding a key multiplies series count.
pub(in crate::services) const METADATA_METRIC_KEYS: &[&str] = &["fynd_version", "fynd_preset"];

// Defensive caps mirroring the client serializer (by value, not by crate dependency).
const MAX_ENTRIES: usize = 16;
const MAX_KEY_BYTES: usize = 64;
const MAX_VALUE_BYTES: usize = 128;
const MAX_HEADER_BYTES: usize = 1024;

/// Parses the raw header into a metadata map, applying defensive caps. Oversized input is dropped
/// rather than trusted: a header over the total-length cap yields an empty map, and individual
/// pairs over the key/value caps or beyond the entry cap are skipped.
pub(in crate::services) fn parse_client_metadata(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if raw.len() > MAX_HEADER_BYTES {
        return out;
    }
    for pair in raw.split(';') {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            continue;
        }
        if key.len() > MAX_KEY_BYTES || value.len() > MAX_VALUE_BYTES {
            continue;
        }
        out.insert(key.to_string(), value.to_string());
    }
    out
}

/// Builds the Prometheus labels for every allowlisted metadata key, in allowlist order. Missing
/// keys yield `"none"`; present values are emitted verbatim (already size-capped by the parser).
/// Non-allowlisted keys are never emitted.
pub(in crate::services) fn metric_labels(metadata: &HashMap<String, String>) -> Vec<Label> {
    METADATA_METRIC_KEYS
        .iter()
        .map(|&key| {
            let value = metadata
                .get(key)
                .map(String::as_str)
                .unwrap_or("none")
                .to_owned();
            Label::new(key, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_map(metadata: &HashMap<String, String>) -> HashMap<String, String> {
        metric_labels(metadata)
            .into_iter()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect()
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_and_trims_pairs() {
        let parsed = parse_client_metadata("fynd_version=1.2.3; fynd_preset=fast");
        assert_eq!(parsed, map(&[("fynd_version", "1.2.3"), ("fynd_preset", "fast")]));
    }

    #[test]
    fn skips_empty_and_malformed_pairs() {
        let parsed = parse_client_metadata("a=1;; no_equals ;b=;=novalue;c=2");
        assert_eq!(parsed, map(&[("a", "1"), ("c", "2")]));
    }

    #[test]
    fn drops_header_over_total_length_cap() {
        let raw = format!("k={}", "v".repeat(MAX_HEADER_BYTES));
        assert!(parse_client_metadata(&raw).is_empty());
    }

    #[test]
    fn skips_oversized_key_or_value() {
        let long_key = "a".repeat(MAX_KEY_BYTES + 1);
        let long_value = "b".repeat(MAX_VALUE_BYTES + 1);
        let raw = format!("{long_key}=ok; k={long_value}; good=1");
        assert_eq!(parse_client_metadata(&raw), map(&[("good", "1")]));
    }

    #[test]
    fn caps_entry_count() {
        let raw = (0..MAX_ENTRIES + 5)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(";");
        assert_eq!(parse_client_metadata(&raw).len(), MAX_ENTRIES);
    }

    #[test]
    fn labels_default_to_none_when_absent() {
        let labels = labels_map(&HashMap::new());
        assert_eq!(labels, map(&[("fynd_version", "none"), ("fynd_preset", "none")]));
    }

    #[test]
    fn labels_pass_through_values_verbatim() {
        // The server never interprets values, so any string on an allowlisted key is emitted as-is
        // (after the parser's size caps). Value semantics are the client's concern.
        let labels =
            labels_map(&map(&[("fynd_version", "not-a-version"), ("fynd_preset", "turbo")]));
        assert_eq!(labels, map(&[("fynd_version", "not-a-version"), ("fynd_preset", "turbo")]));
    }

    #[test]
    fn labels_omit_non_allowlisted_keys() {
        let labels = labels_map(&map(&[("secret", "abc"), ("fynd_preset", "fast")]));
        assert_eq!(labels, map(&[("fynd_version", "none"), ("fynd_preset", "fast")]));
        assert!(!labels.contains_key("secret"));
    }
}
