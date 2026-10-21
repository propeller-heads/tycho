use hex_literal::hex;

use super::pool_storage::StorageLocation;

const SLOT0: [u8; 32] = hex!("0000000000000000000000000000000000000000000000000000000000000005");

const LIQUIDITY_SLOT: StorageLocation = StorageLocation {
    name: "liquidity",
    slot: hex!("000000000000000000000000000000000000000000000000000000000000000f"),
    offset: 0,
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

pub(crate) const TICKS_MAP_SLOT: [u8; 32] =
    hex!("0000000000000000000000000000000000000000000000000000000000000010");

pub(crate) const TRACKED_SLOTS: [StorageLocation; 3] =
    [LIQUIDITY_SLOT, SQRT_PRICE_X96_SLOT, CURRENT_TICK_SLOT];
