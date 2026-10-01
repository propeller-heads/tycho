use tycho_substreams::prelude as tycho;

use crate::lunarbase::{
    events::{event_attributes, LunarBaseEvent},
    state::{attrs, creation_attribute},
};

pub fn initial_entity_change(component_id: &str) -> tycho::EntityChanges {
    // The getter maps an untouched zero storage slot to effective multiplier 1.
    let mut blacklist_fee_multiplier = vec![0; 32];
    blacklist_fee_multiplier[31] = 1;
    tycho::EntityChanges {
        component_id: component_id.to_owned(),
        attributes: vec![
            creation_attribute(attrs::ANCHOR_PRICE_X96, vec![0u8; 20]),
            creation_attribute(attrs::FEE_ASK_X24, 0u32.to_be_bytes().to_vec()),
            creation_attribute(attrs::FEE_BID_X24, 0u32.to_be_bytes().to_vec()),
            creation_attribute(attrs::LATEST_UPDATE_BLOCK, 0u64.to_be_bytes().to_vec()),
            creation_attribute(attrs::RESERVE_X, 0u128.to_be_bytes().to_vec()),
            creation_attribute(attrs::RESERVE_Y, 0u128.to_be_bytes().to_vec()),
            creation_attribute(attrs::MAX_PUNISHMENT_X24, 0u32.to_be_bytes().to_vec()),
            creation_attribute(attrs::BLACKLIST_FEE_MULTIPLIER, blacklist_fee_multiplier),
            creation_attribute(attrs::QUOTE_CALLER_WHITELISTED, vec![0u8]),
            creation_attribute(attrs::BLOCK_DELAY, 2u64.to_be_bytes().to_vec()),
            creation_attribute(attrs::PAUSED, vec![1u8]),
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

/// Builds absolute-value balance changes for a `Sync` event.
///
/// LunarBase does not emit balance deltas, so the pool reserves carried by the `Sync` event are
/// used directly as the component's token balances: `reserve_x` for `token_x`, `reserve_y` for
/// `token_y`. Returns an empty vector for events that do not change reserves.
pub fn balance_changes_for_event(
    component_id: &str,
    token_x: &[u8],
    token_y: &[u8],
    event: &LunarBaseEvent,
) -> Vec<tycho::BalanceChange> {
    let LunarBaseEvent::Sync { reserve_x, reserve_y } = event else {
        return vec![];
    };
    vec![
        balance_change(component_id, token_x, *reserve_x),
        balance_change(component_id, token_y, *reserve_y),
    ]
}

fn balance_change(component_id: &str, token: &[u8], reserve: u128) -> tycho::BalanceChange {
    tycho::BalanceChange {
        token: token.to_vec(),
        balance: reserve.to_be_bytes().to_vec(),
        component_id: component_id.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_X: [u8; 20] = [0x11; 20];
    const TOKEN_Y: [u8; 20] = [0x22; 20];

    #[test]
    fn fresh_pool_starts_paused_with_punishment_disabled() {
        let change = initial_entity_change("0xpool");
        let value = |name: &str| {
            &change
                .attributes
                .iter()
                .find(|attr| attr.name == name)
                .unwrap()
                .value
        };
        assert_eq!(*value(attrs::PAUSED), vec![1]);
        assert_eq!(*value(attrs::MAX_PUNISHMENT_X24), 0u32.to_be_bytes());
        assert_eq!(*value(attrs::ANCHOR_PRICE_X96), vec![0; 20]);
        let mut effective_multiplier = vec![0; 32];
        effective_multiplier[31] = 1;
        assert_eq!(*value(attrs::BLACKLIST_FEE_MULTIPLIER), effective_multiplier);
        assert_eq!(*value(attrs::QUOTE_CALLER_WHITELISTED), vec![0]);
    }

    #[test]
    fn sync_emits_absolute_reserve_balances() {
        let event = LunarBaseEvent::Sync { reserve_x: 1_000u128, reserve_y: 2_500u128 };

        let changes = balance_changes_for_event("0xpool", &TOKEN_X, &TOKEN_Y, &event);

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].token, TOKEN_X.to_vec());
        assert_eq!(changes[0].balance, 1_000u128.to_be_bytes().to_vec());
        assert_eq!(changes[0].component_id, b"0xpool".to_vec());
        assert_eq!(changes[1].token, TOKEN_Y.to_vec());
        assert_eq!(changes[1].balance, 2_500u128.to_be_bytes().to_vec());
    }

    #[test]
    fn non_sync_event_emits_no_balance_changes() {
        let event = LunarBaseEvent::Paused;

        let changes = balance_changes_for_event("0xpool", &TOKEN_X, &TOKEN_Y, &event);

        assert!(changes.is_empty());
    }
}
