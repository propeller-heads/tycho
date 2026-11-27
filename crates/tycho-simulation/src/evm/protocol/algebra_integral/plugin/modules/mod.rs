//! Plugin modules the simulation can reproduce.
//!
//! One directory per vendor ([`algebra`] holds Algebra's own modules; a DEX with modules of its
//! own adds a directory named after it), one file per module. A module is built from the
//! `<module>/<field>` attributes the Substreams package emits for it, whose values are the
//! field's bytes: big-endian and exactly as wide as the field.

use std::collections::HashMap;

use tycho_common::Bytes;

use crate::protocol::errors::InvalidSnapshotError;

pub mod algebra;
pub mod nami;

/// The `N` bytes of attribute `name`.
///
/// Fails when the attribute is missing or is not exactly `N` bytes wide.
pub(crate) fn field_bytes<const N: usize>(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<[u8; N], InvalidSnapshotError> {
    let value = attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))?;
    let Ok(bytes) = <[u8; N]>::try_from(value.as_ref()) else {
        return Err(InvalidSnapshotError::ValueError(format!(
            "attribute {name} is {} bytes wide, expected {N}",
            value.len()
        )));
    };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn attributes(value: &[u8]) -> HashMap<String, Bytes> {
        HashMap::from([("module/field".to_string(), Bytes::from(value.to_vec()))])
    }

    #[test]
    fn reads_a_field_of_the_expected_width() {
        let bytes = field_bytes::<2>(&attributes(&[0x01, 0xf4]), "module/field")
            .expect("a two-byte attribute should read");

        assert_eq!(bytes, [0x01, 0xf4]);
    }

    #[test]
    fn missing_attribute_is_named() {
        let error = field_bytes::<2>(&HashMap::new(), "module/field")
            .expect_err("an absent attribute should not read");

        let InvalidSnapshotError::MissingAttribute(name) = error else {
            panic!("expected a missing attribute, got {error:?}");
        };
        assert_eq!(name, "module/field");
    }

    #[rstest]
    #[case::too_narrow(&[0xf4])]
    #[case::too_wide(&[0x00, 0x01, 0xf4])]
    #[case::empty(&[])]
    fn wrong_width_is_a_value_error(#[case] value: &[u8]) {
        let error = field_bytes::<2>(&attributes(value), "module/field")
            .expect_err("only a two-byte attribute should read");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("module/field"), "{message}");
        assert!(message.contains("expected 2"), "{message}");
    }
}
