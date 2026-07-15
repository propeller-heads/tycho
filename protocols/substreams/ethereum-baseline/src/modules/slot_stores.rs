use super::slot_layout::{
    block_pricing_base_slot, maker_base_slot, pool_base_slot, slot_with_offset, ADDRESS_LEN,
    SLOT_LEN,
};
use crate::pool_factories::{factory_b_token, RELAY_ADDRESS};
use std::collections::HashMap;
use substreams::store::{
    StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetIfNotExists, StoreSetIfNotExistsString,
    StoreSetString,
};
use substreams_ethereum::pb::eth;

const POOL_SLOT_COUNT: u8 = 8;
const MAKER_SLOT_COUNT: u8 = 4;
const BLOCK_PRICING_SLOT_COUNT: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateArea {
    Pool,
    Maker,
    BlockPricing,
}

impl StateArea {
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
pub(crate) struct IndexedSlot {
    pub component_id: String,
    pub area: StateArea,
    pub offset: u8,
    pub slot: [u8; SLOT_LEN],
}

impl IndexedSlot {
    fn encode_location(&self) -> String {
        format!("{}|{}|{}", self.component_id, self.area.as_str(), self.offset)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SlotLocation {
    pub component_id: String,
    pub area: StateArea,
    pub offset: u8,
}

impl SlotLocation {
    fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split('|');
        let component_id = parts.next()?.to_string();
        let area = StateArea::from_str(parts.next()?)?;
        let offset = parts.next()?.parse().ok()?;
        parts
            .next()
            .is_none()
            .then_some(Self { component_id, area, offset })
    }
}

/// Maps relay storage slots to typed per-component locations.
///
/// Slots are keccak-derived per bToken (`keccak(bToken . namespace_slot)`), so they cannot be
/// bounded by a numeric range. Instead, the 16 slots we care about are indexed the moment a
/// bToken address becomes known: on `BTokenCreated` (which fires in the same transaction as the
/// pre-pool `pool.totalSupply` write) or on `PoolCreated`. Everything else written to the relay
/// is ignored.
#[substreams::handlers::store]
fn store_slot_index(block: eth::v2::Block, store: StoreSetIfNotExistsString) {
    block
        .logs()
        .filter_map(|log| factory_b_token(log.log))
        .flat_map(|b_token| indexed_slots_for_component(&component_id_from_address(&b_token)))
        .for_each(|indexed_slot| {
            store.set_if_not_exists(
                0,
                slot_index_key(&indexed_slot.slot),
                &indexed_slot.encode_location(),
            )
        });
}

/// Tracks the current value of every indexed relay slot, keyed by component and typed location.
#[substreams::handlers::store]
fn store_state_slots(block: eth::v2::Block, slot_index: StoreGetString, store: StoreSetString) {
    // Writes in the same block as the indexing event (e.g. pool.totalSupply written by
    // createBToken right before BTokenCreated) must be captured too, so the index is
    // complemented with locations derived from this block's own factory logs.
    let same_block_index = same_block_slot_index(&block);

    block.transactions().for_each(|tx| {
        tx.calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.address.as_slice() == RELAY_ADDRESS)
            .filter_map(|change| {
                let key = slot_index_key(&change.key);
                let location = same_block_index
                    .get(&key)
                    .cloned()
                    .or_else(|| slot_index.get_last(key))?;
                let location = SlotLocation::decode(&location)?;
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

fn same_block_slot_index(block: &eth::v2::Block) -> HashMap<String, String> {
    block
        .logs()
        .filter_map(|log| factory_b_token(log.log))
        .flat_map(|b_token| indexed_slots_for_component(&component_id_from_address(&b_token)))
        .map(|indexed_slot| (slot_index_key(&indexed_slot.slot), indexed_slot.encode_location()))
        .collect()
}

pub(crate) fn component_id_from_address(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

pub(crate) fn indexed_slots_for_component(component_id: &str) -> Vec<IndexedSlot> {
    let Some(b_token) = component_id_to_address(component_id) else {
        return Vec::new();
    };

    let mut slots = Vec::with_capacity(
        (POOL_SLOT_COUNT + MAKER_SLOT_COUNT + BLOCK_PRICING_SLOT_COUNT) as usize,
    );
    slots.extend(indexed_slots(
        component_id,
        StateArea::Pool,
        &pool_base_slot(&b_token),
        POOL_SLOT_COUNT,
    ));
    slots.extend(indexed_slots(
        component_id,
        StateArea::Maker,
        &maker_base_slot(&b_token),
        MAKER_SLOT_COUNT,
    ));
    slots.extend(indexed_slots(
        component_id,
        StateArea::BlockPricing,
        &block_pricing_base_slot(&b_token),
        BLOCK_PRICING_SLOT_COUNT,
    ));
    slots
}

fn indexed_slots(
    component_id: &str,
    area: StateArea,
    base_slot: &[u8; SLOT_LEN],
    count: u8,
) -> Vec<IndexedSlot> {
    (0..count)
        .map(|offset| IndexedSlot {
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

pub(crate) fn state_slot_key(component_id: &str, area: StateArea, offset: u8) -> String {
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

    const MAINNET_BTOKEN: &str = "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63";

    #[test]
    fn indexes_all_state_slots_for_component() {
        let slots = indexed_slots_for_component(MAINNET_BTOKEN);

        assert_eq!(slots.len(), 16);
        assert_eq!(slots[0].area, StateArea::Pool);
        assert_eq!(slots[0].offset, 0);
        assert_eq!(
            slot_index_key(&slots[0].slot),
            "slot:f1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad849"
        );
        assert_eq!(
            slot_index_key(&slots[7].slot),
            "slot:f1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad850"
        );
        assert_eq!(slots[8].area, StateArea::Maker);
        assert_eq!(slots[12].area, StateArea::BlockPricing);
    }

    #[test]
    fn round_trips_slot_location_encoding() {
        let slot = indexed_slots_for_component(MAINNET_BTOKEN)
            .into_iter()
            .find(|slot| slot.area == StateArea::BlockPricing && slot.offset == 3)
            .unwrap();

        let decoded = SlotLocation::decode(&slot.encode_location()).unwrap();

        assert_eq!(decoded.component_id, MAINNET_BTOKEN);
        assert_eq!(decoded.area, StateArea::BlockPricing);
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
    fn builds_same_block_slot_index_from_factory_logs() {
        let b_token = hex::decode(MAINNET_BTOKEN.trim_start_matches("0x")).unwrap();
        let block = block_with_log(b_token_created_log(&b_token));

        let index = same_block_slot_index(&block);

        assert_eq!(index.len(), 16);
        assert_eq!(
            index
                .get("slot:f1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad849")
                .unwrap(),
            &format!("{}|pool|0", component_id_from_address(&b_token))
        );
    }

    fn block_with_log(log: eth::v2::Log) -> eth::v2::Block {
        eth::v2::Block {
            transaction_traces: vec![eth::v2::TransactionTrace {
                status: 1,
                receipt: Some(eth::v2::TransactionReceipt {
                    logs: vec![log],
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn b_token_created_log(b_token: &[u8]) -> eth::v2::Log {
        use ethabi::{Address, Token, Uint};
        use tiny_keccak::{Hasher, Keccak};

        let mut topic = [0u8; 32];
        let mut hasher = Keccak::v256();
        hasher.update(b"BTokenCreated(address,string,string,uint8,uint256,address)");
        hasher.finalize(&mut topic);

        eth::v2::Log {
            address: RELAY_ADDRESS.to_vec(),
            topics: vec![topic.to_vec()],
            data: ethabi::encode(&[
                Token::Address(Address::from_slice(b_token)),
                Token::String("Token".to_string()),
                Token::String("TKN".to_string()),
                Token::Uint(Uint::from(18)),
                Token::Uint(Uint::from(1_000_000u64)),
                Token::Address(Address::from_slice(&[0u8; 20])),
            ]),
            ..Default::default()
        }
    }
}
