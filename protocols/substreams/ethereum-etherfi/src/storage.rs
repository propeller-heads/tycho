use substreams::{hex, scalar::BigInt};
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::{Attribute, ChangeType};

// LiquidityPool Stoage Layout: https://repo.sourcify.dev/1/0xA5C1ddD9185901E3c05E0660126627E039D0a626
const TOTAL_VALUE_OUT_OF_LP_SLOT: StorageLocation = StorageLocation {
    name: "totalValueOutOfLp",
    slot: hex!("00000000000000000000000000000000000000000000000000000000000000cf"),
    offset: 0,
    number_of_bytes: 16,
    signed: false,
};

const TOTAL_VALUE_IN_LP_SLOT: StorageLocation = StorageLocation {
    name: "totalValueInLp",
    slot: hex!("00000000000000000000000000000000000000000000000000000000000000cf"),
    offset: 16,
    number_of_bytes: 16,
    signed: false,
};

const ETH_AMOUNT_LOCKED_FOR_WITHDRAWL_SLOT: StorageLocation = StorageLocation {
    name: "ethAmountLockedForWithdrawl",
    slot: hex!("00000000000000000000000000000000000000000000000000000000000000dc"),
    offset: 1,
    number_of_bytes: 16,
    signed: false,
};

// eETH Storage Layout: https://repo.sourcify.dev/1/0xcb3d917a965a70214f430a135154cd5adda2ad84
const E_ETH_TOTAL_SUPPLY_SLOT: StorageLocation = StorageLocation {
    name: "totalShares",
    slot: hex!("00000000000000000000000000000000000000000000000000000000000000ca"),
    offset: 0,
    number_of_bytes: 32,
    signed: false,
};

// RedeemptionManager Storage Layout: https://repo.sourcify.dev/1/0xe3f384dc7002547dd240ac1ad69a430cce1e292d
const ETH_BUCKET_LIMITER_SLOT: StorageLocation = StorageLocation {
    name: "ethBucketLimiter",
    slot: hex!("de214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e4089"),
    offset: 0,
    number_of_bytes: 32,
    signed: false,
};

const ETH_REDEMPTION_INFO_MAP_SLOT: StorageLocation = StorageLocation {
    name: "ethRedemptionInfo",
    slot: hex!("de214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e408a"),
    offset: 0,
    number_of_bytes: 32,
    signed: false,
};

pub(crate) const WEETH_POOL_TRACKED_SLOTS: [StorageLocation; 3] =
    [TOTAL_VALUE_OUT_OF_LP_SLOT, TOTAL_VALUE_IN_LP_SLOT, E_ETH_TOTAL_SUPPLY_SLOT];

pub(crate) const EETH_POOL_TRACKED_SLOTS: [StorageLocation; 6] = [
    TOTAL_VALUE_OUT_OF_LP_SLOT,
    TOTAL_VALUE_IN_LP_SLOT,
    ETH_AMOUNT_LOCKED_FOR_WITHDRAWL_SLOT,
    E_ETH_TOTAL_SUPPLY_SLOT,
    ETH_BUCKET_LIMITER_SLOT,
    ETH_REDEMPTION_INFO_MAP_SLOT,
];

#[derive(Clone)]
pub struct StorageLocation<'a> {
    pub name: &'a str,
    pub slot: [u8; 32],
    pub offset: usize,
    pub number_of_bytes: usize,
    pub signed: bool,
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

pub fn get_changed_attributes(
    storage_changes: &Vec<StorageChange>,
    locations: Vec<&StorageLocation>,
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
