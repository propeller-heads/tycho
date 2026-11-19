//! Modules of Nami's plugins.

use std::collections::HashMap;

use tycho_common::Bytes;

use crate::protocol::errors::InvalidSnapshotError;

pub mod anti_sniper;
pub mod farming;
pub mod fee_override;
pub mod fee_source;
mod gold_fee_source;
pub mod mevx;
mod nyse_fee_source;
mod us_market_time;

/// The `N` bytes of attribute `name`, or zeros when it is absent.
///
/// No attribute is emitted for storage a plugin never wrote, and that storage reads as zero on
/// chain. Fails when the attribute is present but not exactly `N` bytes wide.
fn field_or_zero<const N: usize>(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<[u8; N], InvalidSnapshotError> {
    let Some(value) = attributes.get(name) else {
        return Ok([0; N]);
    };
    <[u8; N]>::try_from(value.as_ref()).map_err(|error| {
        InvalidSnapshotError::ValueError(format!(
            "attribute {name} is {} bytes wide, expected {N}: {error}",
            value.len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn attributes(value: &[u8]) -> HashMap<String, Bytes> {
        HashMap::from([("module/field".to_string(), Bytes::from(value.to_vec()))])
    }

    #[test]
    fn present_field_is_read() {
        let bytes = field_or_zero::<2>(&attributes(&[0x01, 0xf4]), "module/field")
            .expect("a two-byte attribute should read");

        assert_eq!(bytes, [0x01, 0xf4]);
    }

    #[test]
    fn absent_field_reads_as_zero() {
        let bytes = field_or_zero::<2>(&HashMap::new(), "module/field")
            .expect("an absent attribute should read");

        assert_eq!(bytes, [0, 0]);
    }

    #[rstest]
    #[case::too_narrow(&[0xf4])]
    #[case::too_wide(&[0x00, 0x01, 0xf4])]
    #[case::empty(&[])]
    fn wrong_width_is_a_value_error(#[case] value: &[u8]) {
        let error = field_or_zero::<2>(&attributes(value), "module/field")
            .expect_err("only a two-byte attribute should read");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("module/field"), "{message}");
        assert!(message.contains("expected 2"), "{message}");
    }
}
