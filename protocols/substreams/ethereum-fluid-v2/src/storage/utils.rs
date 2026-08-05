use num_bigint::Sign;
use substreams::scalar::BigInt;
use tiny_keccak::{Hasher, Keccak};

pub fn keccak256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    let mut out = [0u8; 32];
    hasher.update(input);
    hasher.finalize(&mut out);
    out
}

pub fn u256_be32_from_u64(value: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&value.to_be_bytes());
    out
}

pub fn double_mapping_slot(slot: u64, key1: &[u8; 32], key2: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key1);
    buf[32..].copy_from_slice(&u256_be32_from_u64(slot));
    let intermediate = keccak256(&buf);

    let mut buf2 = [0u8; 64];
    buf2[..32].copy_from_slice(key2);
    buf2[32..].copy_from_slice(&intermediate);
    keccak256(&buf2)
}

pub fn triple_mapping_slot(
    slot: u64,
    key1: &[u8; 32],
    key2: &[u8; 32],
    key3: &[u8; 32],
) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key1);
    buf[32..].copy_from_slice(&u256_be32_from_u64(slot));
    let intermediate1 = keccak256(&buf);

    let mut buf2 = [0u8; 64];
    buf2[..32].copy_from_slice(key2);
    buf2[32..].copy_from_slice(&intermediate1);
    let intermediate2 = keccak256(&buf2);

    let mut buf3 = [0u8; 64];
    buf3[..32].copy_from_slice(key3);
    buf3[32..].copy_from_slice(&intermediate2);
    keccak256(&buf3)
}

pub fn int256_be32_from_bigint(value: &BigInt) -> [u8; 32] {
    let (sign, _) = value.to_bytes_be();
    let bytes = value.to_signed_bytes_be();
    if bytes.len() > 32 {
        panic!("cannot convert bigint to int256");
    }
    let mut out = if sign == Sign::Minus { [0xFFu8; 32] } else { [0u8; 32] };
    let start = 32usize.saturating_sub(bytes.len());
    out[start..].copy_from_slice(&bytes);
    out
}

pub fn read_bytes(buf: &[u8], offset: usize, number_of_bytes: usize) -> &[u8] {
    let buf_length = buf.len();
    if buf_length < number_of_bytes {
        panic!(
            "attempting to read {number_of_bytes} bytes in buffer size {buf_size}",
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
            "number of bytes {number_of_bytes} with offset {offset} exceeds buffer size {buf_size}",
            number_of_bytes = number_of_bytes,
            offset = offset,
            buf_size = buf.len()
        )
    }
    let start = start_opt.unwrap();

    &buf[start..=end]
}
