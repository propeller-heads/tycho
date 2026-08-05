use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::{Attribute, ChangeType};

use crate::storage::utils::read_bytes;

#[derive(Clone)]
pub struct StorageLocation {
    pub name: String,
    pub slot: [u8; 32],
    pub offset: usize,
    pub number_of_bytes: usize,
    pub signed: bool,
}

pub struct StorageChangesView {
    storage_changes: Vec<StorageChange>,
}

impl StorageChangesView {
    pub fn new_filtered(address: &[u8], storage_changes: &[StorageChange]) -> Self {
        let filtered_storage_changes = storage_changes
            .iter()
            .filter(|change| change.address == address)
            .cloned()
            .collect();
        Self { storage_changes: filtered_storage_changes }
    }

    pub fn get_changed_attributes(&self, locations: &[StorageLocation]) -> Vec<Attribute> {
        let mut attributes = Vec::new();

        for change in self.storage_changes.iter() {
            for storage_location in locations.iter() {
                if change.key.as_slice() == storage_location.slot {
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

                    if old_data != new_data {
                        let value_bytes = if storage_location.signed {
                            BigInt::from_signed_bytes_be(new_data).to_signed_bytes_be()
                        } else {
                            new_data.to_vec()
                        };
                        attributes.push(Attribute {
                            name: storage_location.name.clone(),
                            value: value_bytes,
                            change: ChangeType::Update.into(),
                        });
                    }
                }
            }
        }

        attributes
    }
}
