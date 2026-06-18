//! Storage helpers for Baseline's namespaced relay state.
//!
//! Baseline components execute through the relay, but core quote state is stored
//! in namespaced mappings on the relay itself. These helpers are intentionally
//! pure so slot math and packed decoding can be tested before substream stores
//! are wired into `map_protocol_changes`.

use num_bigint::{BigInt, Sign};
use tiny_keccak::{Hasher, Keccak};

pub(crate) const ADDRESS_LEN: usize = 20;
pub(crate) const SLOT_LEN: usize = 32;

pub(crate) const POOL_SLOT_NAMESPACE: &str = "Baseline.State.Pool";
pub(crate) const MAKER_SLOT_NAMESPACE: &str = "Baseline.State.Maker";
pub(crate) const BLOCK_PRICING_SLOT_NAMESPACE: &str = "Baseline.State.BlockPricing";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PoolState {
    pub reserve: [u8; ADDRESS_LEN],
    pub paused: bool,
    pub total_supply: BigInt,
    pub total_reserves: BigInt,
    pub total_b_tokens: BigInt,
    pub pending_surplus: BigInt,
    pub settled_reserves: BigInt,
    pub fee_recipient: [u8; ADDRESS_LEN],
    pub reserve_decimals: u8,
    pub b_token_decimals: u8,
    pub creator: [u8; ADDRESS_LEN],
    pub creator_fee_pct: BigInt,
    pub protocol_fee_pct: BigInt,
    pub liquidity_fee_pct: BigInt,
    pub creator_claimable: BigInt,
    pub protocol_claimable: BigInt,
    pub pending_yield: BigInt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MakerState {
    pub initialized: bool,
    pub blv_price: BigInt,
    pub swap_fee: BigInt,
    pub max_circ: BigInt,
    pub max_reserves: BigInt,
    pub convexity_exp: BigInt,
    pub last_invariant: BigInt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockPricingState {
    pub start_reserves: BigInt,
    pub start_supply: BigInt,
    pub block_buy_delta_circ: BigInt,
    pub block_sell_delta_circ: BigInt,
    pub start_last_invariant: BigInt,
    pub block_number: u64,
}

pub(crate) fn pool_namespace_slot() -> [u8; SLOT_LEN] {
    namespace_slot(POOL_SLOT_NAMESPACE)
}

pub(crate) fn maker_namespace_slot() -> [u8; SLOT_LEN] {
    namespace_slot(MAKER_SLOT_NAMESPACE)
}

pub(crate) fn block_pricing_namespace_slot() -> [u8; SLOT_LEN] {
    namespace_slot(BLOCK_PRICING_SLOT_NAMESPACE)
}

pub(crate) fn pool_base_slot(b_token: &[u8; ADDRESS_LEN]) -> [u8; SLOT_LEN] {
    mapping_base_slot(b_token, &pool_namespace_slot())
}

pub(crate) fn maker_base_slot(b_token: &[u8; ADDRESS_LEN]) -> [u8; SLOT_LEN] {
    mapping_base_slot(b_token, &maker_namespace_slot())
}

pub(crate) fn block_pricing_base_slot(b_token: &[u8; ADDRESS_LEN]) -> [u8; SLOT_LEN] {
    mapping_base_slot(b_token, &block_pricing_namespace_slot())
}

pub(crate) fn slot_with_offset(base: &[u8; SLOT_LEN], offset: u8) -> [u8; SLOT_LEN] {
    let mut slot = *base;
    add_small(&mut slot, offset);
    slot
}

pub(crate) fn decode_pool(slots: &[[u8; SLOT_LEN]; 8]) -> PoolState {
    PoolState {
        reserve: low_address(&slots[0]),
        paused: byte_from_right(&slots[0], 20) != 0,
        total_supply: uint256(&slots[1]),
        total_reserves: low_uint128(&slots[2]),
        total_b_tokens: high_uint128(&slots[2]),
        pending_surplus: low_uint128(&slots[3]),
        settled_reserves: high_uint128(&slots[3]),
        fee_recipient: low_address(&slots[4]),
        reserve_decimals: byte_from_right(&slots[4], 20),
        b_token_decimals: byte_from_right(&slots[4], 21),
        creator: low_address(&slots[5]),
        creator_fee_pct: mid_uint64(&slots[5], 20),
        protocol_fee_pct: low_uint64(&slots[6]),
        liquidity_fee_pct: mid_uint64(&slots[6], 8),
        creator_claimable: high_uint128(&slots[6]),
        protocol_claimable: low_uint128(&slots[7]),
        pending_yield: high_uint128(&slots[7]),
    }
}

pub(crate) fn decode_maker(slots: &[[u8; SLOT_LEN]; 4]) -> MakerState {
    MakerState {
        initialized: byte_from_right(&slots[0], 0) != 0,
        blv_price: mid_uint128(&slots[0], 1),
        swap_fee: low_uint128(&slots[1]),
        max_circ: high_uint128(&slots[1]),
        max_reserves: low_uint128(&slots[2]),
        convexity_exp: high_uint128(&slots[2]),
        last_invariant: uint256(&slots[3]),
    }
}

pub(crate) fn decode_block_pricing(slots: &[[u8; SLOT_LEN]; 4]) -> BlockPricingState {
    BlockPricingState {
        start_reserves: low_uint128(&slots[0]),
        start_supply: high_uint128(&slots[0]),
        block_buy_delta_circ: low_uint128(&slots[1]),
        block_sell_delta_circ: high_uint128(&slots[1]),
        start_last_invariant: uint256(&slots[2]),
        block_number: low_uint64_raw(&slots[3]),
    }
}

fn namespace_slot(name: &str) -> [u8; SLOT_LEN] {
    let mut slot = keccak256(name.as_bytes());
    sub_one(&mut slot);
    slot
}

fn mapping_base_slot(
    key_address: &[u8; ADDRESS_LEN],
    namespace_slot: &[u8; SLOT_LEN],
) -> [u8; SLOT_LEN] {
    let mut encoded = [0u8; SLOT_LEN * 2];
    encoded[12..32].copy_from_slice(key_address);
    encoded[32..64].copy_from_slice(namespace_slot);
    keccak256(&encoded)
}

fn keccak256(input: &[u8]) -> [u8; SLOT_LEN] {
    let mut output = [0u8; SLOT_LEN];
    let mut hasher = Keccak::v256();
    hasher.update(input);
    hasher.finalize(&mut output);
    output
}

fn sub_one(value: &mut [u8; SLOT_LEN]) {
    for byte in value.iter_mut().rev() {
        if *byte > 0 {
            *byte -= 1;
            return;
        }
        *byte = u8::MAX;
    }
}

fn add_small(value: &mut [u8; SLOT_LEN], addend: u8) {
    let mut carry = addend as u16;
    for byte in value.iter_mut().rev() {
        let sum = *byte as u16 + carry;
        *byte = sum as u8;
        carry = sum >> 8;
        if carry == 0 {
            return;
        }
    }
}

fn low_address(slot: &[u8; SLOT_LEN]) -> [u8; ADDRESS_LEN] {
    let mut address = [0u8; ADDRESS_LEN];
    address.copy_from_slice(&slot[12..32]);
    address
}

fn byte_from_right(slot: &[u8; SLOT_LEN], offset: usize) -> u8 {
    slot[SLOT_LEN - 1 - offset]
}

fn uint256(slot: &[u8; SLOT_LEN]) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, slot)
}

fn high_uint128(slot: &[u8; SLOT_LEN]) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &slot[0..16])
}

fn low_uint128(slot: &[u8; SLOT_LEN]) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &slot[16..32])
}

fn mid_uint128(slot: &[u8; SLOT_LEN], right_offset: usize) -> BigInt {
    let end = SLOT_LEN - right_offset;
    BigInt::from_bytes_be(Sign::Plus, &slot[end - 16..end])
}

fn low_uint64(slot: &[u8; SLOT_LEN]) -> BigInt {
    mid_uint64(slot, 0)
}

fn mid_uint64(slot: &[u8; SLOT_LEN], right_offset: usize) -> BigInt {
    let end = SLOT_LEN - right_offset;
    BigInt::from_bytes_be(Sign::Plus, &slot[end - 8..end])
}

fn low_uint64_raw(slot: &[u8; SLOT_LEN]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&slot[24..32]);
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTOKEN: &str = "2A6b1BF66542CB1463541d211747B28C6bb39e83";

    fn address(hex_address: &str) -> [u8; ADDRESS_LEN] {
        let mut address = [0u8; ADDRESS_LEN];
        address.copy_from_slice(&hex::decode(hex_address.trim_start_matches("0x")).unwrap());
        address
    }

    fn slot(hex_slot: &str) -> [u8; SLOT_LEN] {
        let mut slot = [0u8; SLOT_LEN];
        slot.copy_from_slice(&hex::decode(hex_slot.trim_start_matches("0x")).unwrap());
        slot
    }

    fn dec(value: &str) -> BigInt {
        value.parse().unwrap()
    }

    #[test]
    fn computes_namespace_slots() {
        assert_eq!(
            hex::encode(pool_namespace_slot()),
            "5e0c90d658953447a9a6183fafee6ce210d189b3996a6a327daf1c089411a92d"
        );
        assert_eq!(
            hex::encode(maker_namespace_slot()),
            "e1f3e5d5ea876721e96ead31698de4d68b16ce3ec39e86844058729c52a8c4a0"
        );
        assert_eq!(
            hex::encode(block_pricing_namespace_slot()),
            "d5b8512e303453ba959118773f2c71ab3dc2c27edcc544a7dd32dc7c10b70d26"
        );
    }

    #[test]
    fn computes_mapping_base_slots() {
        let b_token = address(BTOKEN);

        assert_eq!(
            hex::encode(pool_base_slot(&b_token)),
            "b6d363ca52ba8e017072ab98ce56c65d6c52c86b8c3f92847e9bd2f4fe987294"
        );
        assert_eq!(
            hex::encode(maker_base_slot(&b_token)),
            "9f5ea7400b37ac5d7ea0f8d604d37cbddf85dd47d76ae09799de27276dd1103a"
        );
        assert_eq!(
            hex::encode(block_pricing_base_slot(&b_token)),
            "0f1d449ee7e8567b7934443c9940e15ef332ab725ad6490db6d1028122c2b0e4"
        );
        assert_eq!(
            hex::encode(slot_with_offset(&pool_base_slot(&b_token), 8)),
            "b6d363ca52ba8e017072ab98ce56c65d6c52c86b8c3f92847e9bd2f4fe98729c"
        );
    }

    #[test]
    fn decodes_pool_state_from_live_storage_slots() {
        let slots = [
            slot("0000000000000000000000004200000000000000000000000000000000000006"),
            slot("0000000000000000000000000000000000000001431e0fae6d7217caa0000000"),
            slot("00000001131d880178b3eb1356bf70cf00000000000000000212a34d158575ed"),
            slot("00000000000000000214e82e57415af4000000000000000000012270a0ddf283"),
            slot("00000000000000000000121201422cf811f6f97186db733d9f390ab01f27ceac"),
            slot("00000000000000000000000001422cf811f6f97186db733d9f390ab01f27ceac"),
            slot("0000000000000000000000000000000006f05b59d3b2000003782dace9d90000"),
            slot("00000000000000000000d9d478a675e300000000000000000000489c28377ca1"),
        ];

        let pool = decode_pool(&slots);

        assert_eq!(hex::encode(pool.reserve), "4200000000000000000000000000000000000006");
        assert!(!pool.paused);
        assert_eq!(pool.total_supply, dec("100000000000000000000000000000"));
        assert_eq!(pool.total_reserves, dec("149361289125524973"));
        assert_eq!(pool.total_b_tokens, dec("85144078818624686264514736335"));
        assert_eq!(pool.pending_surplus, dec("319342107292291"));
        assert_eq!(pool.settled_reserves, dec("149999973340109556"));
        assert_eq!(pool.reserve_decimals, 18);
        assert_eq!(pool.b_token_decimals, 18);
        assert_eq!(hex::encode(pool.fee_recipient), "01422cf811f6f97186db733d9f390ab01f27ceac");
        assert_eq!(hex::encode(pool.creator), "01422cf811f6f97186db733d9f390ab01f27ceac");
        assert_eq!(pool.creator_fee_pct, dec("0"));
        assert_eq!(pool.protocol_fee_pct, dec("250000000000000000"));
        assert_eq!(pool.liquidity_fee_pct, dec("500000000000000000"));
        assert_eq!(pool.creator_claimable, dec("0"));
        assert_eq!(pool.protocol_claimable, dec("79835526823073"));
        assert_eq!(pool.pending_yield, dec("239506580469219"));
    }

    #[test]
    fn decodes_maker_state_from_live_storage_slots() {
        let slots = [
            slot("000000000000000000000000000000000000000000000000000000009756e501"),
            slot("00000000204fce5e3e250261100000000000000000000000000aa87bee538000"),
            slot("00000000000000001bc16d674ec800000000000000000000016345785d8a0000"),
            slot("00000000000000000000000000000000000000000000000000eb734c0d02cc00"),
        ];

        let maker = decode_maker(&slots);

        assert!(maker.initialized);
        assert_eq!(maker.blv_price, dec("9918181"));
        assert_eq!(maker.swap_fee, dec("3000000000000000"));
        assert_eq!(maker.max_circ, dec("10000000000000000000000000000"));
        assert_eq!(maker.max_reserves, dec("100000000000000000"));
        assert_eq!(maker.convexity_exp, dec("2000000000000000000"));
        assert_eq!(maker.last_invariant, dec("66273390000000000"));
    }

    #[test]
    fn decodes_block_pricing_state_from_live_storage_slots() {
        let slots = [
            slot("0000000122ce41502f4d1569900000000000000000000000016345785d8a0000"),
            slot("00000000000000000000000000000000000000000fb0b94eb6992a5639408f31"),
            slot("00000000000000000000000000000000000000000000000000eb734c0d02cc00"),
            slot("0000000000000000000000000000000000000000000000000000000002ca42b2"),
        ];

        let pricing = decode_block_pricing(&slots);

        assert_eq!(pricing.start_reserves, dec("100000000000000000"));
        assert_eq!(pricing.start_supply, dec("90000000000000000000000000000"));
        assert_eq!(pricing.block_buy_delta_circ, dec("4855921181375313735485263665"));
        assert_eq!(pricing.block_sell_delta_circ, dec("0"));
        assert_eq!(pricing.start_last_invariant, dec("66273390000000000"));
        assert_eq!(pricing.block_number, 46_809_778);
    }
}
