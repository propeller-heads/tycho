//! EtherFi indexing: the LiquidityPool venue and the weETH wrapper as two components.
//!
//! Neither contract has a creation event to discover, so the manifest carries a storage snapshot
//! in `params`, along with the transaction in `start_block` to anchor the components to. Every
//! later block is driven by raw storage writes on the four contracts the venue spans.
//!
//! Handlers below are in manifest order.

use anyhow::{anyhow, Result};
use itertools::Itertools;
use std::{cell::LazyCell, collections::HashMap};
use substreams::{pb::substreams::StoreDeltas, prelude::*, scalar::BigInt};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    models::{
        BlockChanges, ChangeType, EntityChanges, ImplementationType, ProtocolComponent,
        TransactionChangesBuilder,
    },
    prelude::{BalanceChange, BlockTransactionProtocolComponents, TransactionProtocolComponents},
};

use crate::{
    constants::{
        Component, TrackedSlot, EETH_ADDRESS, ETH_ADDRESS, LIQUIDITY_POOL_VALUE_KEY,
        TOTAL_SHARES_KEY, TRACKED_SLOTS, WEETH_ADDRESS, WEETH_SHARES_KEY,
    },
    state::{unpack_fields, BalanceState, InitialState},
    utils::bytes_from_hex,
};

/// Creates both components on `start_block`, and nothing on any other block.
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let initial_state = InitialState::parse(&params)?;

    if block.number != initial_state.start_block {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] });
    }

    let creation_tx = bytes_from_hex(&initial_state.creation_tx)?;
    let tx = block
        .transactions()
        .find(|tx| tx.hash == creation_tx)
        .ok_or_else(|| {
            anyhow!(
                "Activation transaction {} not found in block {}",
                initial_state.creation_tx,
                block.number
            )
        })?;

    Ok(BlockTransactionProtocolComponents {
        tx_components: vec![TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: vec![pool_component(), wrapper_component()],
        }],
    })
}

/// ETH -> eETH through `LiquidityPool.deposit`, eETH -> ETH through
/// `EtherFiRedemptionManager.redeemEEth`.
fn pool_component() -> ProtocolComponent {
    ProtocolComponent::new(Component::Pool.id())
        .with_tokens(&[EETH_ADDRESS, ETH_ADDRESS])
        .as_swap_type("ethereum_etherfi_pool", ImplementationType::Custom)
}

/// eETH <-> weETH through `wrap` / `unwrap`.
fn wrapper_component() -> ProtocolComponent {
    ProtocolComponent::new(Component::Wrapper.id())
        .with_tokens(&[WEETH_ADDRESS, EETH_ADDRESS])
        .as_swap_type("ethereum_etherfi_pool", ImplementationType::Custom)
}

/// Carries the latest raw value of every slot a component balance is derived from, so a block
/// that touches only one of them can still report both balances. Seeded from the manifest
/// snapshot on `start_block`.
#[substreams::handlers::store]
pub fn store_balance_slots(params: String, block: eth::v2::Block, store: StoreSetBigInt) {
    let initial_state = InitialState::parse(&params).expect("Failed to parse EtherFi params");

    if block.number == initial_state.start_block {
        let seed = initial_state
            .balance_state()
            .expect("Failed to decode the EtherFi initial state");
        store.set(0, LIQUIDITY_POOL_VALUE_KEY, &seed.liquidity_pool_value);
        store.set(0, TOTAL_SHARES_KEY, &seed.total_shares);
        store.set(0, WEETH_SHARES_KEY, &seed.weeth_shares);
        return;
    }

    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
        {
            for storage_change in &call.storage_changes {
                let Some(key) = tracked_slot(&storage_change.address, &storage_change.key)
                    .and_then(|slot| slot.balance_key)
                else {
                    continue;
                };
                store.set(
                    storage_change.ordinal,
                    key,
                    &BigInt::from_unsigned_bytes_be(&storage_change.new_value),
                );
            }
        }
    }
}

/// The tracked slot at `position` on `contract`, or `None` for a write this package ignores.
///
/// Both halves of the key matter: the LiquidityPool and eETH are separate contracts whose
/// low-numbered slots overlap, and the LiquidityPool's slot 202 holds an address.
fn tracked_slot(contract: &[u8], position: &[u8]) -> Option<&'static TrackedSlot> {
    TRACKED_SLOTS
        .iter()
        .find(|slot| contract == slot.contract && position == slot.position)
}

/// Emits the component creations on `start_block`, and attribute plus balance updates on every
/// later block. The two paths are mutually exclusive.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    protocol_components: BlockTransactionProtocolComponents,
    balance_deltas: StoreDeltas,
    balance_store: StoreGetBigInt,
) -> Result<BlockChanges> {
    let initial_state = InitialState::parse(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    if block.number == initial_state.start_block {
        initialize_protocol_components(
            &initial_state,
            protocol_components,
            &mut transaction_changes,
        )?;
    } else {
        handle_state_updates(&block, &balance_deltas, &balance_store, &mut transaction_changes);
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: vec![],
    })
}

/// Registers both components on the activation transaction and seeds them from the snapshot.
fn initialize_protocol_components(
    initial_state: &InitialState,
    protocol_components: BlockTransactionProtocolComponents,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    let tx_component = protocol_components
        .tx_components
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Missing activation transaction component"))?;
    let tx = tx_component
        .tx
        .as_ref()
        .ok_or_else(|| anyhow!("Activation transaction missing"))?;

    let builder = transaction_changes
        .entry(tx.index)
        .or_insert_with(|| TransactionChangesBuilder::new(tx));

    for component in tx_component.components {
        builder.add_protocol_component(&component);
    }

    for component in [Component::Pool, Component::Wrapper] {
        builder.add_entity_change(&EntityChanges {
            component_id: component.id().to_string(),
            attributes: initial_state.creation_attributes(component)?,
        });
    }

    let balances = initial_state.balance_state()?;
    add_pool_balance(builder, &balances);
    add_wrapper_balance(builder, &balances);

    Ok(())
}

/// Turns storage writes on the tracked contracts into per-transaction attribute and balance
/// changes.
fn handle_state_updates(
    block: &eth::v2::Block,
    balance_deltas: &StoreDeltas,
    balance_store: &StoreGetBigInt,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    // Deferred: most blocks touch none of the three balance slots and never read this.
    let mut balances = LazyCell::new(|| block_start_balance_state(balance_deltas, balance_store));

    for tx in block.transactions() {
        let mut pool_balance_touched = false;
        let mut wrapper_balance_touched = false;

        for call in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
        {
            for storage_change in &call.storage_changes {
                let Some(tracked) = tracked_slot(&storage_change.address, &storage_change.key)
                else {
                    continue;
                };

                let builder = transaction_changes
                    .entry(tx.index as u64)
                    .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

                for component in tracked.components {
                    builder.add_entity_change(&EntityChanges {
                        component_id: component.id().to_string(),
                        attributes: unpack_fields(
                            tracked,
                            &storage_change.new_value,
                            ChangeType::Update,
                        ),
                    });
                }

                if let Some(key) = tracked.balance_key {
                    balances.apply(key, BigInt::from_unsigned_bytes_be(&storage_change.new_value));
                    // The pool balance is the value word alone; the wrapper's needs all three.
                    pool_balance_touched |= key == LIQUIDITY_POOL_VALUE_KEY;
                    wrapper_balance_touched = true;
                }
            }
        }

        // Balances are absolute, so one report per transaction that moved any of the inputs is
        // enough - intermediate values within the transaction are never observable.
        if pool_balance_touched || wrapper_balance_touched {
            let builder = transaction_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));
            if pool_balance_touched {
                add_pool_balance(builder, &balances);
            }
            if wrapper_balance_touched {
                add_wrapper_balance(builder, &balances);
            }
        }
    }
}

/// The pool's balance is `totalValueInLp`, the ETH redemptions are paid from.
fn add_pool_balance(builder: &mut TransactionChangesBuilder, balances: &BalanceState) {
    builder.add_balance_change(&BalanceChange {
        token: ETH_ADDRESS.to_vec(),
        balance: balances
            .pool_eth_balance()
            .to_signed_bytes_be(),
        component_id: Component::Pool.id().as_bytes().to_vec(),
    });
}

/// The wrapper's balance is `eETH.balanceOf(weETH)`: what unwrapping everything would pay out.
fn add_wrapper_balance(builder: &mut TransactionChangesBuilder, balances: &BalanceState) {
    builder.add_balance_change(&BalanceChange {
        token: EETH_ADDRESS.to_vec(),
        balance: balances
            .wrapper_eeth_balance()
            .to_signed_bytes_be(),
        component_id: Component::Wrapper
            .id()
            .as_bytes()
            .to_vec(),
    });
}

/// Rebuilds the balance inputs as of the start of the block.
///
/// The store module runs before this one, so `get_last` already reflects this block's writes.
/// Where a key changed in this block, the first delta's `old_value` is the value it held on
/// entry; otherwise the store still holds it.
fn block_start_balance_state(
    balance_deltas: &StoreDeltas,
    balance_store: &StoreGetBigInt,
) -> BalanceState {
    let value_for = |key: &str| -> BigInt {
        match balance_deltas
            .deltas
            .iter()
            .filter(|delta| delta.key == key)
            .min_by_key(|delta| delta.ordinal)
        {
            Some(first_delta) => decode_store_value(key, &first_delta.old_value),
            None => balance_store
                .get_last(key)
                .unwrap_or_else(|| {
                    panic!("EtherFi store key {key} was never seeded; the store module runs first")
                }),
        }
    };

    BalanceState {
        liquidity_pool_value: value_for(LIQUIDITY_POOL_VALUE_KEY),
        total_shares: value_for(TOTAL_SHARES_KEY),
        weeth_shares: value_for(WEETH_SHARES_KEY),
    }
}

/// `StoreSetBigInt` serialises values as decimal strings.
///
/// A value that does not decode means the store module or the runtime is broken, so this panics
/// and names the key. The figure it returns becomes a component balance, where any stand-in for
/// it reads as genuine liquidity.
///
/// An empty value is the seed on `start_block`: `StoreDeltas` carries an empty `old_value` for a
/// key's first write.
fn decode_store_value(key: &str, bytes: &[u8]) -> BigInt {
    if bytes.is_empty() {
        return BigInt::zero();
    }
    let text = std::str::from_utf8(bytes)
        .unwrap_or_else(|_| panic!("EtherFi store key {key} holds non-UTF-8 bytes: {bytes:02x?}"));
    text.parse::<BigInt>()
        .unwrap_or_else(|_| panic!("EtherFi store key {key} holds an unparsable value: {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        EETH_TOTAL_SHARES_POSITION, LIQUIDITY_POOL_ADDRESS, LIQUIDITY_POOL_VALUE_POSITION,
        REDEMPTION_MANAGER_ADDRESS,
    };

    /// Each row is reachable by its own contract and position, so no two rows share a key and
    /// no attribute is paired with the wrong slot.
    #[test]
    fn every_tracked_slot_resolves_to_its_own_row() {
        for slot in TRACKED_SLOTS.iter() {
            let found =
                tracked_slot(&slot.contract, &slot.position).expect("declared slot resolves");
            let found_names: Vec<_> = found
                .fields
                .iter()
                .map(|f| f.attribute)
                .collect();
            let want_names: Vec<_> = slot
                .fields
                .iter()
                .map(|f| f.attribute)
                .collect();
            assert_eq!(found_names, want_names);
            assert_eq!(found.balance_key, slot.balance_key);
        }
    }

    /// The LiquidityPool's slot 202 holds an address, and eETH's slot 202 holds `totalShares`.
    /// A write to the former must not be read as the latter.
    #[test]
    fn the_same_position_on_another_contract_is_not_tracked() {
        assert!(tracked_slot(&LIQUIDITY_POOL_ADDRESS, &EETH_TOTAL_SHARES_POSITION).is_none());
        assert!(tracked_slot(&EETH_ADDRESS, &LIQUIDITY_POOL_VALUE_POSITION).is_none());
        assert!(tracked_slot(&REDEMPTION_MANAGER_ADDRESS, &LIQUIDITY_POOL_VALUE_POSITION).is_none());
    }

    #[test]
    fn untracked_positions_resolve_to_none() {
        assert!(tracked_slot(&LIQUIDITY_POOL_ADDRESS, &[0u8; 32]).is_none());
        assert!(tracked_slot(&[0u8; 20], &LIQUIDITY_POOL_VALUE_POSITION).is_none());
        assert!(tracked_slot(&[], &[]).is_none());
    }

    /// Every attribute name is unique across the whole table, so no two slots can report the
    /// same name and silently overwrite one another.
    #[test]
    fn attribute_names_are_unique() {
        let mut names: Vec<&str> = TRACKED_SLOTS
            .iter()
            .flat_map(|slot| slot.fields.iter().map(|f| f.attribute))
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate attribute name in TRACKED_SLOTS");
        assert_eq!(total, 19);
    }

    /// Each slot's fields tile its word from the bottom without overlapping, so no bit is
    /// reported twice or dropped between two attributes.
    #[test]
    fn packed_fields_tile_their_word() {
        for slot in TRACKED_SLOTS.iter() {
            let mut next = 0u32;
            for field in slot.fields {
                assert_eq!(field.offset, next, "gap or overlap before {}", field.attribute);
                next = field.offset + field.width;
            }
            assert!(next <= 256, "slot overflows a word at {}", slot.fields[0].attribute);
        }
    }

    /// Every slot reaches at least one component, and the balance inputs reach the component
    /// whose balance they feed.
    #[test]
    fn every_slot_reaches_a_component() {
        for slot in TRACKED_SLOTS.iter() {
            assert!(
                !slot.components.is_empty(),
                "{} reaches no component",
                slot.fields[0].attribute
            );
        }
        let wrapper_inputs: Vec<_> = TRACKED_SLOTS
            .iter()
            .filter(|slot| slot.balance_key.is_some())
            .map(|slot| {
                slot.components
                    .contains(&Component::Wrapper)
            })
            .collect();
        assert_eq!(wrapper_inputs, [true, true, true]);
    }

    /// The three inputs `BalanceState` derives both balances from, and only those.
    #[test]
    fn only_the_balance_inputs_carry_a_store_key() {
        let keys: Vec<_> = TRACKED_SLOTS
            .iter()
            .filter_map(|slot| slot.balance_key)
            .collect();

        assert_eq!(keys, [LIQUIDITY_POOL_VALUE_KEY, TOTAL_SHARES_KEY, WEETH_SHARES_KEY]);
    }

    /// A key's first delta carries an empty `old_value`, which means "nothing yet", not a
    /// malformed store.
    #[test]
    fn an_empty_store_value_decodes_to_zero() {
        assert_eq!(decode_store_value(TOTAL_SHARES_KEY, &[]), BigInt::zero());
    }

    #[test]
    fn a_decimal_store_value_round_trips() {
        assert_eq!(
            decode_store_value(TOTAL_SHARES_KEY, b"2001243491556134113932753"),
            "2001243491556134113932753"
                .parse::<BigInt>()
                .unwrap()
        );
    }

    /// Zero is a plausible balance, so a value that cannot be decoded must not become one.
    #[test]
    #[should_panic(expected = "unparsable value")]
    fn an_unparsable_store_value_panics_rather_than_reading_as_zero() {
        decode_store_value(TOTAL_SHARES_KEY, b"not a number");
    }

    #[test]
    #[should_panic(expected = "non-UTF-8")]
    fn a_non_utf8_store_value_panics() {
        decode_store_value(TOTAL_SHARES_KEY, &[0xff, 0xfe]);
    }
}
