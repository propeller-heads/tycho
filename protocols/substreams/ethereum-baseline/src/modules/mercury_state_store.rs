use super::mercury_storage::{
    block_pricing_base_slot, maker_base_slot, pool_base_slot, slot_with_offset, ADDRESS_LEN,
    SLOT_LEN,
};
use crate::pool_factories::{maybe_create_component, DeploymentConfig};
use std::collections::HashMap;
use substreams::store::{
    StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetIfNotExists, StoreSetIfNotExistsString,
    StoreSetString,
};
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude::BlockTransactionProtocolComponents;

const POOL_SLOT_COUNT: u8 = 8;
const MAKER_SLOT_COUNT: u8 = 4;
const BLOCK_PRICING_SLOT_COUNT: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MercuryStateArea {
    Pool,
    Maker,
    BlockPricing,
}

impl MercuryStateArea {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pool => "pool",
            Self::Maker => "maker",
            Self::BlockPricing => "block_pricing",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "pool" => Some(Self::Pool),
            "maker" => Some(Self::Maker),
            "block_pricing" => Some(Self::BlockPricing),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MercuryIndexedSlot {
    pub component_id: String,
    pub area: MercuryStateArea,
    pub offset: u8,
    pub slot: [u8; SLOT_LEN],
}

impl MercuryIndexedSlot {
    fn encode_location(&self) -> String {
        format!("{}|{}|{}", self.component_id, self.area.as_str(), self.offset)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MercurySlotLocation {
    pub component_id: String,
    pub area: MercuryStateArea,
    pub offset: u8,
}

impl MercurySlotLocation {
    fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split('|');
        let component_id = parts.next()?.to_string();
        let area = MercuryStateArea::from_str(parts.next()?)?;
        let offset = parts.next()?.parse().ok()?;
        parts
            .next()
            .is_none()
            .then_some(Self { component_id, area, offset })
    }
}

#[substreams::handlers::store]
fn store_mercury_slot_index(
    components: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsString,
) {
    components
        .tx_components
        .iter()
        .flat_map(|tx_components| tx_components.components.iter())
        .flat_map(|component| indexed_slots_for_component(&component.id))
        .for_each(|indexed_slot| {
            store.set_if_not_exists(
                0,
                slot_index_key(&indexed_slot.slot),
                &indexed_slot.encode_location(),
            )
        });
}

#[substreams::handlers::store]
fn store_mercury_raw_slots(
    params: String,
    block: eth::v2::Block,
    slot_index: StoreGetString,
    store: StoreSetString,
) {
    let config: DeploymentConfig =
        serde_qs::from_str(params.as_str()).expect("invalid Baseline deployment config params");

    // Some Mercury slots, notably pool.totalSupply, can be written before PoolCreated makes the
    // component address indexable. Keep pre-index relay writes so creation can backfill typed state
    // from storage without relying on event payloads.
    block.transactions().for_each(|tx| {
        tx.calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.address == config.relay_address)
            .filter(|change| {
                slot_index
                    .get_last(slot_index_key(&change.key))
                    .is_none()
            })
            .for_each(|change| {
                store.set(
                    change.ordinal,
                    slot_index_key(&change.key),
                    &slot_value_hex(&change.new_value),
                )
            });
    });
}

#[substreams::handlers::store]
fn store_mercury_state_slots(
    params: String,
    block: eth::v2::Block,
    components: BlockTransactionProtocolComponents,
    raw_slots: StoreGetString,
    slot_index: StoreGetString,
    store: StoreSetString,
) {
    let config: DeploymentConfig =
        serde_qs::from_str(params.as_str()).expect("invalid Baseline deployment config params");
    let same_block_index = same_block_slot_index(&components);
    let creation_ordinals: HashMap<_, _> = block
        .logs()
        .filter_map(|log| {
            maybe_create_component(log.log, &config).map(|component| (component.id, log.ordinal()))
        })
        .collect();

    // On PoolCreated, typed slots become computable. Seed them from raw pre-index storage writes,
    // then let same-block and future storage diffs update typed state normally.
    components
        .tx_components
        .iter()
        .flat_map(|tx_components| tx_components.components.iter())
        .flat_map(|component| {
            let ordinal = creation_ordinals
                .get(&component.id)
                .copied();
            indexed_slots_for_component(&component.id)
                .into_iter()
                .filter_map(move |indexed_slot| ordinal.map(|ordinal| (indexed_slot, ordinal)))
        })
        .filter_map(|(indexed_slot, ordinal)| {
            raw_slots
                .get_last(slot_index_key(&indexed_slot.slot))
                .map(|value| (indexed_slot, ordinal, value))
        })
        .for_each(|(indexed_slot, ordinal, value)| {
            store.set(
                ordinal,
                state_slot_key(&indexed_slot.component_id, indexed_slot.area, indexed_slot.offset),
                &value,
            )
        });

    block.transactions().for_each(|tx| {
        tx.calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.address == config.relay_address)
            .filter_map(|change| {
                let key = slot_index_key(&change.key);
                let location = same_block_index
                    .get(&key)
                    .cloned()
                    .or_else(|| slot_index.get_last(key))?;
                let location = MercurySlotLocation::decode(&location)?;
                Some((change.ordinal, location, slot_value_hex(&change.new_value)))
            })
            .for_each(|(ordinal, location, value)| {
                store.set(
                    ordinal,
                    state_slot_key(&location.component_id, location.area, location.offset),
                    &value,
                )
            });
    });
}

fn same_block_slot_index(
    components: &BlockTransactionProtocolComponents,
) -> HashMap<String, String> {
    components
        .tx_components
        .iter()
        .flat_map(|tx_components| tx_components.components.iter())
        .flat_map(|component| indexed_slots_for_component(&component.id))
        .map(|indexed_slot| (slot_index_key(&indexed_slot.slot), indexed_slot.encode_location()))
        .collect()
}

pub(crate) fn indexed_slots_for_component(component_id: &str) -> Vec<MercuryIndexedSlot> {
    let Some(b_token) = component_id_to_address(component_id) else {
        return Vec::new();
    };

    let mut slots = Vec::with_capacity(
        (POOL_SLOT_COUNT + MAKER_SLOT_COUNT + BLOCK_PRICING_SLOT_COUNT) as usize,
    );
    slots.extend(indexed_slots(
        component_id,
        MercuryStateArea::Pool,
        &pool_base_slot(&b_token),
        POOL_SLOT_COUNT,
    ));
    slots.extend(indexed_slots(
        component_id,
        MercuryStateArea::Maker,
        &maker_base_slot(&b_token),
        MAKER_SLOT_COUNT,
    ));
    slots.extend(indexed_slots(
        component_id,
        MercuryStateArea::BlockPricing,
        &block_pricing_base_slot(&b_token),
        BLOCK_PRICING_SLOT_COUNT,
    ));
    slots
}

fn indexed_slots(
    component_id: &str,
    area: MercuryStateArea,
    base_slot: &[u8; SLOT_LEN],
    count: u8,
) -> Vec<MercuryIndexedSlot> {
    (0..count)
        .map(|offset| MercuryIndexedSlot {
            component_id: component_id.to_string(),
            area,
            offset,
            slot: slot_with_offset(base_slot, offset),
        })
        .collect()
}

pub(crate) fn slot_index_key(slot: &[u8]) -> String {
    format!("slot:{}", fixed_hex(slot, SLOT_LEN))
}

pub(crate) fn state_slot_key(component_id: &str, area: MercuryStateArea, offset: u8) -> String {
    format!("state:{component_id}:{}:{offset}", area.as_str())
}

pub(crate) fn slot_value_hex(value: &[u8]) -> String {
    fixed_hex(value, SLOT_LEN)
}

fn component_id_to_address(component_id: &str) -> Option<[u8; ADDRESS_LEN]> {
    let hex = component_id
        .strip_prefix("0x")
        .unwrap_or(component_id);
    let bytes = hex::decode(hex).ok()?;
    (bytes.len() == ADDRESS_LEN).then(|| {
        let mut address = [0; ADDRESS_LEN];
        address.copy_from_slice(&bytes);
        address
    })
}

fn fixed_hex(value: &[u8], len: usize) -> String {
    let mut bytes = vec![0; len.saturating_sub(value.len())];
    if value.len() > len {
        bytes.extend_from_slice(&value[value.len() - len..]);
    } else {
        bytes.extend_from_slice(value);
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tycho_substreams::prelude::{
        ProtocolComponent, Transaction, TransactionProtocolComponents,
    };

    const MAINNET_BTOKEN: &str = "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63";

    #[test]
    fn indexes_all_mercury_state_slots_for_component() {
        let slots = indexed_slots_for_component(MAINNET_BTOKEN);

        assert_eq!(slots.len(), 16);
        assert_eq!(slots[0].area, MercuryStateArea::Pool);
        assert_eq!(slots[0].offset, 0);
        assert_eq!(
            slot_index_key(&slots[0].slot),
            "slot:f1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad849"
        );
        assert_eq!(
            slot_index_key(&slots[7].slot),
            "slot:f1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad850"
        );
        assert_eq!(slots[8].area, MercuryStateArea::Maker);
        assert_eq!(slots[12].area, MercuryStateArea::BlockPricing);
    }

    #[test]
    fn round_trips_slot_location_encoding() {
        let slot = indexed_slots_for_component(MAINNET_BTOKEN)
            .into_iter()
            .find(|slot| slot.area == MercuryStateArea::BlockPricing && slot.offset == 3)
            .unwrap();

        let decoded = MercurySlotLocation::decode(&slot.encode_location()).unwrap();

        assert_eq!(decoded.component_id, MAINNET_BTOKEN);
        assert_eq!(decoded.area, MercuryStateArea::BlockPricing);
        assert_eq!(decoded.offset, 3);
    }

    #[test]
    fn formats_slot_values_as_fixed_width_hex() {
        assert_eq!(
            slot_value_hex(&[0x12, 0x34]),
            "0000000000000000000000000000000000000000000000000000000000001234"
        );
        assert_eq!(
            slot_value_hex(&[0xff; 33]),
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
    }

    #[test]
    fn builds_same_block_slot_index_for_created_components() {
        let components = BlockTransactionProtocolComponents {
            tx_components: vec![TransactionProtocolComponents {
                tx: Some(Transaction::default()),
                components: vec![ProtocolComponent {
                    id: MAINNET_BTOKEN.to_string(),
                    ..Default::default()
                }],
            }],
        };

        let index = same_block_slot_index(&components);

        assert_eq!(index.len(), 16);
        assert_eq!(
            index
                .get("slot:f1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad849")
                .unwrap(),
            "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63|pool|0"
        );
    }
}
