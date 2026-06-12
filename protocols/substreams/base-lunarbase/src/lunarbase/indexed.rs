use tycho_substreams::prelude as tycho;

use crate::lunarbase::{
    events::{event_attributes, LunarBaseEvent},
    state::{attrs, creation_attribute},
};

pub fn initial_entity_change(component_id: &str) -> tycho::EntityChanges {
    tycho::EntityChanges {
        component_id: component_id.to_owned(),
        attributes: vec![
            creation_attribute(attrs::ANCHOR_PRICE_X96, 0u128.to_be_bytes().to_vec()),
            creation_attribute(attrs::FEE_ASK_X24, 0u32.to_be_bytes().to_vec()),
            creation_attribute(attrs::FEE_BID_X24, 0u32.to_be_bytes().to_vec()),
            creation_attribute(attrs::LATEST_UPDATE_BLOCK, 0u64.to_be_bytes().to_vec()),
            creation_attribute(attrs::RESERVE_X, 0u128.to_be_bytes().to_vec()),
            creation_attribute(attrs::RESERVE_Y, 0u128.to_be_bytes().to_vec()),
            creation_attribute(attrs::CONCENTRATION_K, 0u32.to_be_bytes().to_vec()),
            creation_attribute(attrs::BLOCK_DELAY, 2u64.to_be_bytes().to_vec()),
            creation_attribute(attrs::PAUSED, vec![0u8]),
        ],
    }
}

pub fn entity_change_for_event(
    component_id: &str,
    event: &LunarBaseEvent,
    block_number: u64,
) -> tycho::EntityChanges {
    tycho::EntityChanges {
        component_id: component_id.to_owned(),
        attributes: event_attributes(event, block_number),
    }
}
