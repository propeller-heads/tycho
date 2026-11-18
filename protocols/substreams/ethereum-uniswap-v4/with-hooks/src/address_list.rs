//! Addresses read from a module's query-string parameters.
//!
//! An address is 20 hexadecimal bytes, with the `0x` prefix optional and the digits read
//! case-insensitively. An address list is a non-empty, comma-separated list of such addresses
//! without duplicates.

use anyhow::{anyhow, Result};

/// A 20-byte EVM address.
pub(crate) type Address = [u8; 20];

/// Parses `value`, the value of parameter `key`, as one address. Fails, naming `key` and `value`,
/// when `value` is not hex or not 20 bytes.
pub(crate) fn parse_address(key: &str, value: &str) -> Result<Address> {
    let digits = value
        .strip_prefix("0x")
        .unwrap_or(value);
    let bytes =
        hex::decode(digits).map_err(|e| anyhow!("`{key}` value `{value}` is not hex: {e}"))?;
    Address::try_from(bytes.as_slice()).map_err(|_: std::array::TryFromSliceError| {
        // The error carries no detail beyond the length mismatch the message names.
        anyhow!("`{key}` value `{value}` is {} bytes, not 20", bytes.len())
    })
}

/// Parses `value`, the value of parameter `key`, as an address list, in the order it lists them.
/// Fails, naming `key`, when `value` is empty, when an entry is empty, when an entry fails
/// [`parse_address`], and when an address appears twice in any case.
pub(crate) fn parse_address_list(key: &str, value: &str) -> Result<Vec<Address>> {
    if value.is_empty() {
        return Err(anyhow!("`{key}` lists no address"));
    }
    let mut addresses = Vec::new();
    for (position, entry) in value.split(',').enumerate() {
        if entry.is_empty() {
            return Err(anyhow!("`{key}` entry {} is empty", position + 1));
        }
        let address = parse_address(key, entry)?;
        if addresses.contains(&address) {
            return Err(anyhow!("`{key}` lists `{entry}` twice"));
        }
        addresses.push(address);
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;

    #[test]
    fn test_address_list_keeps_the_listed_order_in_any_case_and_prefix() {
        assert_eq!(
            parse_address_list(
                "hooks",
                "0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC,0000000000000000000000000000000000000002"
            )
            .unwrap(),
            vec![
                hex!("6b1216a40b65c83e9c9e18a562c446697b8cf8ec"),
                hex!("0000000000000000000000000000000000000002"),
            ]
        );
    }

    #[rstest]
    #[case::empty_list("", "`hooks` lists no address")]
    #[case::trailing_comma("6b1216a40b65c83e9c9e18a562c446697b8cf8ec,", "`hooks` entry 2 is empty")]
    #[case::leading_comma(",6b1216a40b65c83e9c9e18a562c446697b8cf8ec", "`hooks` entry 1 is empty")]
    #[case::not_hex("0xzz", "`hooks` value `0xzz` is not hex")]
    #[case::odd_digits(
        "6b1216a40b65c83e9c9e18a562c446697b8cf8e",
        "`hooks` value `6b1216a40b65c83e9c9e18a562c446697b8cf8e` is not hex"
    )]
    #[case::too_short(
        "6b1216a40b65c83e9c9e18a562c446697b8cf8",
        "`hooks` value `6b1216a40b65c83e9c9e18a562c446697b8cf8` is 19 bytes, not 20"
    )]
    #[case::too_long(
        "6b1216a40b65c83e9c9e18a562c446697b8cf8ecaa",
        "`hooks` value `6b1216a40b65c83e9c9e18a562c446697b8cf8ecaa` is 21 bytes, not 20"
    )]
    #[case::duplicate(
        "6b1216a40b65c83e9c9e18a562c446697b8cf8ec,0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC",
        "`hooks` lists `0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC` twice"
    )]
    fn test_address_list_rejects_a_malformed_list(#[case] value: &str, #[case] expected: &str) {
        let error = parse_address_list("hooks", value)
            .unwrap_err()
            .to_string();

        assert!(error.starts_with(expected), "{error}");
    }
}
