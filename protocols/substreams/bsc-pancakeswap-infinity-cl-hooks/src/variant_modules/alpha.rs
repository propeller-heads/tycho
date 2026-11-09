//! What every module of this package shares about `CLAlphaHook`.
use base_pancakeswap_infinity_cl::{
    modules::map_pool_created::decode_address,
    parameters::{
        hooks_registration_bitmap, HOOKS_AFTER_SWAP_OFFSET, HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET,
        HOOKS_BEFORE_SWAP_OFFSET, HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET,
    },
};
use substreams_ethereum::pb::eth::v2::Log;
use substreams_helper::hex::Hexable;

/// State attribute holding `poolStartedTimestamp[id]`, in seconds. Zero disables the pool.
pub const START_TIMESTAMP_ATTRIBUTE: &str = "alpha_start_timestamp";

/// `keccak256("PoolStartedAtUpdated(bytes32,uint256,address)")`, the same in every
/// `CLAlphaHook` version: `event PoolStartedAtUpdated(PoolId indexed poolId, uint256
/// startedTimestamp, address operator)`.
pub const POOL_STARTED_AT_UPDATED: [u8; 32] = [
    0xca, 0xcc, 0xc4, 0xbc, 0x88, 0x6d, 0x75, 0xb1, 0x3d, 0xe8, 0x06, 0xbf, 0x42, 0x92, 0xe4, 0xcc,
    0x78, 0xa0, 0x42, 0xea, 0xe4, 0x08, 0x49, 0xe6, 0xa9, 0x62, 0x42, 0xf7, 0xd0, 0x3c, 0xb5, 0xfb,
];

/// Parses the comma-separated hook addresses (hex, no `0x`) the manifest lists as `CLAlphaHook`.
pub fn parse_hooks(list: &str) -> Result<Vec<Vec<u8>>, substreams::errors::Error> {
    list.split(',')
        .map(|hook| decode_address("hooks", hook.trim()))
        .collect()
}

/// Store key of `store_alpha_start_times`, keyed like `store_pools`: `start:0x<pool id>`.
pub fn start_key(pool_id: &str) -> String {
    format!("start:{pool_id}")
}

/// Whether `parameters` registers exactly the swap callback `CLAlphaHook` does: `beforeSwap`, and
/// neither `afterSwap` nor a returns-delta flag. A pool claiming more is not one this package can
/// price, whatever its hook address.
pub fn has_alpha_swap_permissions(parameters: &[u8; 32]) -> bool {
    let swap_bits = (1u16 << HOOKS_BEFORE_SWAP_OFFSET) |
        (1u16 << HOOKS_AFTER_SWAP_OFFSET) |
        (1u16 << HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET) |
        (1u16 << HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET);
    hooks_registration_bitmap(parameters) & swap_bits == 1u16 << HOOKS_BEFORE_SWAP_OFFSET
}

/// `(pool id as 0x-hex, startedTimestamp as a 32-byte big-endian word)` of a
/// `PoolStartedAtUpdated` log emitted by one of `hooks`, `None` for any other log.
pub fn decode_start_update(log: &Log, hooks: &[Vec<u8>]) -> Option<(String, Vec<u8>)> {
    if !hooks.contains(&log.address) ||
        log.topics.len() != 2 ||
        log.topics[0] != POOL_STARTED_AT_UPDATED ||
        log.data.len() != 64
    {
        return None;
    }
    Some((log.topics[1].to_hex(), log.data[..32].to_vec()))
}

#[cfg(test)]
mod tests {
    use base_pancakeswap_infinity_cl::parameters::HOOKS_BEFORE_SWAP_OFFSET;
    use rstest::rstest;

    use super::*;

    pub(crate) const HOOK: [u8; 20] = [0xb0; 20];

    fn parameters(bitmap: u16) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[30..].copy_from_slice(&bitmap.to_be_bytes());
        out
    }

    #[test]
    fn topic_is_the_event_signature_hash() {
        use tiny_keccak::{Hasher, Keccak};
        let mut hasher = Keccak::v256();
        hasher.update(b"PoolStartedAtUpdated(bytes32,uint256,address)");
        let mut out = [0u8; 32];
        hasher.finalize(&mut out);
        assert_eq!(out, POOL_STARTED_AT_UPDATED);
    }

    /// `0x45` and `0x55` are the bitmaps the deployed versions register.
    #[rstest]
    #[case::v2_v3(0x45, true)]
    #[case::v1(0x55, true)]
    #[case::no_swap_hook(0x05, false)]
    #[case::after_swap_too(0x45 | 1 << 7, false)]
    #[case::before_swap_returns_delta(0x45 | 1 << 10, false)]
    #[case::after_swap_returns_delta(0x45 | 1 << 11, false)]
    fn only_a_bare_before_swap_qualifies(#[case] bitmap: u16, #[case] expected: bool) {
        assert_eq!(has_alpha_swap_permissions(&parameters(bitmap)), expected);
        assert_eq!(HOOKS_BEFORE_SWAP_OFFSET, 6);
    }

    #[test]
    fn parses_the_hook_list() {
        let hooks = parse_hooks(
            "9a9b5331ce8d74b2b721291d57de696e878353fd, b0bb171d333569cfd28a37f5c5dddaaa90ad46af",
        )
        .unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[1][0], 0xb0);
        assert!(parse_hooks("0x9a9b").is_err());
    }

    fn log(address: [u8; 20], topic0: [u8; 32], data_len: usize) -> Log {
        Log {
            address: address.to_vec(),
            topics: vec![topic0.to_vec(), vec![0xab; 32]],
            data: vec![0x01; data_len],
            ..Default::default()
        }
    }

    #[test]
    fn decodes_only_the_listed_hooks_event() {
        let hooks = vec![HOOK.to_vec()];

        let (pool, start) = decode_start_update(&log(HOOK, POOL_STARTED_AT_UPDATED, 64), &hooks)
            .expect("the hook's own event decodes");
        assert_eq!(pool, format!("0x{}", "ab".repeat(32)));
        assert_eq!(start, vec![0x01; 32]);

        assert!(
            decode_start_update(&log([0x77; 20], POOL_STARTED_AT_UPDATED, 64), &hooks).is_none()
        );
        assert!(decode_start_update(&log(HOOK, [0x11; 32], 64), &hooks).is_none());
        assert!(decode_start_update(&log(HOOK, POOL_STARTED_AT_UPDATED, 32), &hooks).is_none());
    }
}
