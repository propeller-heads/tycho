use hex_literal::hex;

use super::pool_storage::StorageLocation;

const SLOT0: [u8; 32] = hex!("0000000000000000000000000000000000000000000000000000000000000000");

const LIQUIDITY_SLOT: StorageLocation = StorageLocation {
    name: "liquidity",
    slot: hex!("0000000000000000000000000000000000000000000000000000000000000004"),
    offset: 0,
    number_of_bytes: 16,
    signed: false,
};

const PROTOCOL_FEES_TOKEN_0_SLOT: StorageLocation = StorageLocation {
    name: "protocol_fees/token0",
    slot: hex!("0000000000000000000000000000000000000000000000000000000000000003"),
    offset: 0,
    number_of_bytes: 16,
    signed: false,
};

const PROTOCOL_FEES_TOKEN_1_SLOT: StorageLocation = StorageLocation {
    name: "protocol_fees/token1",
    slot: hex!("0000000000000000000000000000000000000000000000000000000000000003"),
    offset: 16,
    number_of_bytes: 16,
    signed: false,
};

const SQRT_PRICE_X96_SLOT: StorageLocation = StorageLocation {
    name: "sqrt_price_x96",
    slot: SLOT0,
    offset: 0,
    number_of_bytes: 20,
    signed: false,
};

const CURRENT_TICK_SLOT: StorageLocation =
    StorageLocation { name: "tick", slot: SLOT0, offset: 20, number_of_bytes: 3, signed: true };

const FEE_PROTOCOL_SLOT: StorageLocation = StorageLocation {
    name: "fee_protocol",
    slot: SLOT0,
    offset: 29,
    number_of_bytes: 1,
    signed: false,
};

pub(crate) const TICKS_MAP_SLOT: [u8; 32] =
    hex!("0000000000000000000000000000000000000000000000000000000000000005");

pub(crate) const TRACKED_SLOTS: [StorageLocation; 6] = [
    LIQUIDITY_SLOT,
    PROTOCOL_FEES_TOKEN_0_SLOT,
    PROTOCOL_FEES_TOKEN_1_SLOT,
    SQRT_PRICE_X96_SLOT,
    CURRENT_TICK_SLOT,
    FEE_PROTOCOL_SLOT,
];
