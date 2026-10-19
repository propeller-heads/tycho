use anyhow::Result;
use substreams::prelude::BigInt;
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::models::{Attribute, ChangeType};

/// associated with a name.
///
/// # Fields
///
/// * `name` - A string slice (`&str`) reference representing the unique name associated with this
///   storage location.
/// * `slot` - A fixed-size byte array `[u8; 32]` representing the slot in the contract storage
///   where this data is stored. This acts as a primary identifier for the location of the data.
/// * `offset` - A usize value indicating the offset in bytes from the start of the slot. This
///   allows for fine-grained control and access within a single slot.
/// * `number_of_bytes` - A usize value indicating the size of the data in bytes.
/// ```
#[derive(Clone)]
pub struct StorageLocation<'a> {
    pub name: &'a str,
    pub slot: [u8; 32],
    pub offset: usize,
    pub number_of_bytes: usize,
    pub signed: bool,
}

/// Iterates through storage changes and checks for modifications in the provided list of
/// storage locations. For each change, it compares the old and new values at the specified
/// offset and length for that location. If a change is detected, it's added to the returned
/// `Attribute` list.
///
/// Arguments:
///     locations: Vec<&StorageLocation> - A vector of references to StorageLocation objects
/// that define the slots, offsets, and lengths to be checked for changes.
///
/// Returns:
///     `Vec<Attribute>`: A vector containing Attributes for each change detected in the tracked
/// slots. Returns an empty vector if no changes are detected.
pub fn get_changed_attributes(
    storage_changes: &[StorageChange],
    locations: &[StorageLocation],
) -> Vec<Attribute> {
    let mut attributes = Vec::new();

    // For each storage change, check if it changes a tracked slot.
    // If it does, add the attribute to the list of attributes
    for change in storage_changes {
        for storage_location in locations.iter() {
            // Check if the change slot matches the tracked slot
            if change.key == storage_location.slot {
                let old_data = read_bytes(
                    &change.old_value,
                    storage_location.offset,
                    storage_location.number_of_bytes,
                );
                let new_data = read_bytes(
                    &change.new_value,
                    storage_location.offset,
                    storage_location.number_of_bytes,
                );

                // Check if there is a change in the data
                if old_data != new_data {
                    let value = match storage_location.signed {
                        true => BigInt::from_signed_bytes_be(new_data),
                        false => BigInt::from_unsigned_bytes_be(new_data),
                    };
                    attributes.push(Attribute {
                        name: storage_location.name.to_string(),
                        value: value.to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    });
                }
            }
        }
    }

    attributes
}

/// Convert a hex string (with or without 0x prefix) to bytes.
pub(crate) fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    hex::decode(hex).map_err(|e| anyhow::anyhow!("Failed to decode hex string: {}", e))
}

pub fn read_bytes(buf: &[u8], offset: usize, number_of_bytes: usize) -> &[u8] {
    let buf_length = buf.len();
    if buf_length < number_of_bytes {
        panic!(
            "attempting to read {number_of_bytes} bytes in buffer  size {buf_size}",
            number_of_bytes = number_of_bytes,
            buf_size = buf.len()
        )
    }

    if offset > (buf_length - 1) {
        panic!(
            "offset {offset} exceeds buffer size {buf_size}",
            offset = offset,
            buf_size = buf.len()
        )
    }

    let end = buf_length - 1 - offset;
    let start_opt = (end + 1).checked_sub(number_of_bytes);
    if start_opt.is_none() {
        panic!(
            "number of bytes {number_of_bytes} with offset {offset} exceeds buffer size
{buf_size}",
            number_of_bytes = number_of_bytes,
            offset = offset,
            buf_size = buf.len()
        )
    }
    let start = start_opt.unwrap();

    &buf[start..=end]
}

#[cfg(test)]
mod tests {
    use crate::utils::read_bytes;
    use std::{fmt::Write, num::ParseIntError};
    use substreams::hex;

    #[test]
    #[should_panic]
    fn read_bytes_buf_too_small() {
        let buf = decode_hex("ff").unwrap();
        let offset = 0;
        let number_of_bytes = 3;
        let _ = read_bytes(&buf, offset, number_of_bytes);
    }

    #[test]
    fn read_one_byte_with_no_offset() {
        let buf = decode_hex("aabb").unwrap();
        let offset = 0;
        let number_of_bytes = 1;
        assert_eq!(read_bytes(&buf, offset, number_of_bytes), hex!("bb"));
    }

    #[test]
    fn read_one_byte_with_offset() {
        let buf = decode_hex("aabb").unwrap();
        let offset = 1;
        let number_of_bytes = 1;
        assert_eq!(read_bytes(&buf, offset, number_of_bytes), hex!("aa"));
    }

    #[test]
    #[should_panic]
    fn read_bytes_overflow() {
        let buf = decode_hex("aabb").unwrap();
        let offset = 1;
        let number_of_bytes = 2;
        let _ = read_bytes(&buf, offset, number_of_bytes);
    }

    #[test]
    fn read_bytes_with_no_offset() {
        let buf =
            decode_hex("ffffffffffffffffffffecb6826b89a60000000000000000000013497d94765a").unwrap();
        let offset = 0;
        let number_of_bytes = 16;
        let out = read_bytes(&buf, offset, number_of_bytes);
        assert_eq!(encode_hex(out), "0000000000000000000013497d94765a".to_string());
    }

    #[test]
    fn read_byte_with_big_offset() {
        let buf =
            decode_hex("0100000000000000000000000000000000000000000000000000000000000000").unwrap();
        let offset = 31;
        let number_of_bytes = 1;
        let out = read_bytes(&buf, offset, number_of_bytes);
        assert_eq!(encode_hex(out), "01".to_string());
    }

    #[test]
    fn read_byte_with_no_offset() {
        let buf =
            decode_hex("0000000000000000000000000000000000000000000000000000000000000001").unwrap();
        let offset = 0;
        let number_of_bytes = 1;
        let out = read_bytes(&buf, offset, number_of_bytes);
        assert_eq!(encode_hex(out), "01".to_string());
    }

    fn decode_hex(s: &str) -> Result<Vec<u8>, ParseIntError> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect()
    }

    fn encode_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            write!(&mut s, "{:02x}", b).unwrap();
        }
        s
    }
}
