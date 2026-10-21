use hex_literal::hex;

use super::pool_storage::StorageLocation;

const SLOT0: [u8; 32] = hex!("0000000000000000000000000000000000000000000000000000000000000006");

const LIQUIDITY_SLOT: StorageLocation = StorageLocation {
    name: "liquidity",
    slot: hex!("0000000000000000000000000000000000000000000000000000000000000010"),
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

const OBSERVATION_INDEX_SLOT: StorageLocation = StorageLocation {
    name: "observationIndex",
    slot: SLOT0,
    offset: 23,
    number_of_bytes: 2,
    signed: false,
};

const OBSERVATION_CARDINALITY_SLOT: StorageLocation = StorageLocation {
    name: "observationCardinality",
    slot: SLOT0,
    offset: 25,
    number_of_bytes: 2,
    signed: false,
};

pub(crate) const TICKS_MAP_SLOT: [u8; 32] =
    hex!("0000000000000000000000000000000000000000000000000000000000000011");

pub(crate) const OBSERVATIONS: [u8; 32] =
    hex!("0000000000000000000000000000000000000000000000000000000000000014");

pub(crate) const OBSERVATIONS_BASE_U64: u64 = 20;

pub(crate) const TRACKED_SLOTS: [StorageLocation; 5] = [
    LIQUIDITY_SLOT,
    SQRT_PRICE_X96_SLOT,
    CURRENT_TICK_SLOT,
    OBSERVATION_INDEX_SLOT,
    OBSERVATION_CARDINALITY_SLOT,
];
