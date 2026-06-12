use keccak_hash::keccak;

pub(crate) fn address_id(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

pub(crate) fn pool_store_key(address: &[u8]) -> String {
    format!("pool:{}", address_id(address))
}

pub(crate) fn buffer_mapping_key(wrapped_token: &[u8]) -> String {
    format!("buffer_mapping_{}", hex::encode(wrapped_token))
}

pub(crate) fn mapping_storage_key_for_address(address: &[u8], slot: u8) -> Vec<u8> {
    let mut input = [0u8; 64];
    input[12..32].copy_from_slice(address);
    input[63] = slot;

    keccak(input.as_slice())
        .as_bytes()
        .to_vec()
}

pub(crate) fn decode_address_from_storage_word(value: &[u8]) -> Option<Vec<u8>> {
    if value.len() < 20 {
        return None;
    }

    Some(value[value.len() - 20..].to_vec())
}
