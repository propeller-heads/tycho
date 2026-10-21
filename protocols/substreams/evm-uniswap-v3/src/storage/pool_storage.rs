use crate::storage::utils;
use tycho_substreams::prelude::{Attribute, ChangeType};

use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::StorageChange;

use super::{constants::TICKS_MAP_SLOT, utils::read_bytes};

/// `StorageLocation` is a struct that represents a specific location within a contract's storage
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

pub struct UniswapPoolStorage<'a> {
    pub storage_changes: &'a Vec<StorageChange>,
}

impl<'a> UniswapPoolStorage<'a> {
    pub fn new(storage_changes: &'a Vec<StorageChange>) -> UniswapPoolStorage<'a> {
        Self { storage_changes }
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
    pub fn get_changed_attributes(&self, locations: Vec<&StorageLocation>) -> Vec<Attribute> {
        let mut attributes = Vec::new();

        // For each storage change, check if it changes a tracked slot.
        // If it does, add the attribute to the list of attributes
        for change in self.storage_changes {
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

    /// Iterates over a list of tick indexes and checks for modifications in the list of
    /// storage changes. If a relevent change is detected, it's added to the returned `Attribute`
    /// list.
    ///
    /// Arguments:
    ///     ticks_idx: `Vec<&BigInt>` - A vector of references to tick indexes as BigInt objects.
    ///
    /// Returns:
    ///     `Vec<Attribute>`: A vector containing Attributes for each change detected. Returns an
    /// empty vector if no changes are detected.
    ///
    /// Note: Currently, we only track the net-liquidity attribute for each tick.
    pub fn get_ticks_changes(&self, ticks_idx: Vec<&BigInt>) -> Vec<Attribute> {
        let mut storage_locs = Vec::new();
        let mut tick_names = Vec::new();

        // First, create all the names and push them into tick_names.
        // We need this to keep the references to the names alive until we call
        // `get_changed_attributes()`
        for tick_idx in ticks_idx.iter() {
            tick_names.push(format!("ticks/{tick_idx}/net-liquidity"));
        }

        // Then, iterate over ticks_idx and tick_names simultaneously
        for (tick_idx, tick_name) in ticks_idx.iter().zip(tick_names.iter()) {
            let tick_slot =
                utils::calc_map_slot(&utils::left_pad_from_bigint(tick_idx), &TICKS_MAP_SLOT);

            storage_locs.push(StorageLocation {
                name: tick_name,
                slot: tick_slot,
                offset: 16,
                number_of_bytes: 16,
                signed: true,
            });
        }

        self.get_changed_attributes(storage_locs.iter().collect())
    }
}
