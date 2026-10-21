use substreams::scalar::BigInt;
use tiny_keccak::{Hasher, Keccak};

pub fn calc_map_slot(map_index: &[u8; 32], base_slot: &[u8; 32]) -> [u8; 32] {
    let mut output = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(map_index);
    hasher.update(base_slot);
    hasher.finalize(&mut output);
    output
}

pub fn left_pad_from_bigint(input: &BigInt) -> [u8; 32] {
    if input.lt(&BigInt::zero()) {
        return left_pad(&input.to_signed_bytes_be(), 255);
    }

    left_pad(&input.to_signed_bytes_be(), 0)
}

pub fn left_pad(input: &[u8], padding_value: u8) -> [u8; 32] {
    if input.len() > 32 {
        panic!("cannot convert vec<u8> to H256");
    }
    let mut data = [padding_value; 32];
    let offset = 32 - input.len();
    data[offset..(input.len() + offset)].copy_from_slice(input);

    data
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
    use crate::storage::utils::{left_pad, read_bytes};
    use hex_literal::hex;
    use std::{fmt::Write, num::ParseIntError};

    #[test]
    fn left_pad_lt_32_bytes() {
        let input = hex!("dd62ed3e");
        assert_eq!(
            hex!("00000000000000000000000000000000000000000000000000000000dd62ed3e"),
            left_pad(&input, 0)
        )
    }

    #[test]
    fn left_pad_eq_32_bytes() {
        let input = hex!("00000a0000000000005d000000000000000000000000000000000000dd62ed3e");
        assert_eq!(
            hex!("00000a0000000000005d000000000000000000000000000000000000dd62ed3e"),
            left_pad(&input, 0)
        )
    }

    #[test]
    #[should_panic]
    fn left_pad_gt_32_bytes() {
        let input = hex!("070000000a0000000000005d000000000000000000000000000000000000dd62ed3e");
        let _ = left_pad(&input, 0);
    }

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
