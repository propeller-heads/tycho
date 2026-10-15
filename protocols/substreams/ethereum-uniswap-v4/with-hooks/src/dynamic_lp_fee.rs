//! The LP fee a dynamic-fee pool stores in `slot0`. `updateDynamicLPFee` writes it without
//! emitting an event, so it is read off the PoolManager's storage writes instead.

use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::prelude::*;

use crate::storage::Word;

/// Adds an `lp_fee` attribute update for every tracked pool whose `slot0.lpFee` a transaction of
/// `block` changed. `pool_for_slot` maps a hex-encoded `slot0` storage slot to the id of the
/// dynamic-fee pool that owns it.
pub fn add_lp_fee_changes(
    _pool_manager: &[u8],
    _block: &eth::Block,
    _pool_for_slot: impl Fn(&str) -> Option<String>,
    _changes: &mut Vec<TransactionChanges>,
) {
}

/// The key under which the pool with `pool_id` is tracked.
pub fn lp_fee_slot_key(_pool_id: &Word) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use substreams::scalar::BigInt;
    use substreams_ethereum::pb::eth::v2::{
        Block, Call, StorageChange, TransactionTrace, TransactionTraceStatus,
    };

    use super::*;
    use crate::storage::{mapping_slot, pad32};

    const POOL_MANAGER: [u8; 20] = hex_literal::hex!("000000000004444c5dc75cb358380d2e3de08a90");
    const OTHER_CONTRACT: [u8; 20] = hex_literal::hex!("a12760be00000000000000000000000000000001");
    const DYNAMIC_POOL: &str = "96dcaf4c3546003fa209f267765e2edb9e8c6ecd031803b972df4d2d21d57871";
    const STATIC_POOL: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const SQRT_PRICE_A: u8 = 0xaa;
    const SQRT_PRICE_B: u8 = 0xbb;

    fn pool_word(pool_id: &str) -> Word {
        pad32(&hex::decode(pool_id).unwrap()).unwrap()
    }

    fn slot0_key(pool_id: &str) -> Word {
        mapping_slot(&pool_word(pool_id), 6)
    }

    /// A `slot0` word: sqrtPriceX96 (low bits), tick, protocolFee, then lpFee at bits 208..232.
    fn slot0(sqrt_price_marker: u8, lp_fee: u32) -> Vec<u8> {
        let mut word = vec![0u8; 32];
        word[31] = sqrt_price_marker;
        word[3..6].copy_from_slice(&lp_fee.to_be_bytes()[1..]);
        word
    }

    fn write(
        address: &[u8],
        pool_id: &str,
        old: Vec<u8>,
        new: Vec<u8>,
        ordinal: u64,
    ) -> StorageChange {
        StorageChange {
            address: address.to_vec(),
            key: slot0_key(pool_id).to_vec(),
            old_value: old,
            new_value: new,
            ordinal,
        }
    }

    fn call(storage_changes: Vec<StorageChange>) -> Call {
        Call { storage_changes, ..Default::default() }
    }

    fn reverted_call(storage_changes: Vec<StorageChange>) -> Call {
        Call { storage_changes, state_reverted: true, ..Default::default() }
    }

    fn block(calls: Vec<Call>) -> Block {
        let trace = TransactionTrace {
            hash: vec![7u8; 32],
            index: 3,
            status: i32::from(TransactionTraceStatus::Succeeded),
            calls,
            ..Default::default()
        };
        Block { number: 24_000_000, transaction_traces: vec![trace], ..Default::default() }
    }

    fn tracked() -> impl Fn(&str) -> Option<String> {
        let slots: HashMap<String, String> = HashMap::from([(
            lp_fee_slot_key(&slot0_key(DYNAMIC_POOL)),
            format!("0x{DYNAMIC_POOL}"),
        )]);
        move |key| slots.get(key).cloned()
    }

    fn run(block: &Block, changes: Vec<TransactionChanges>) -> Vec<TransactionChanges> {
        let mut changes = changes;
        add_lp_fee_changes(&POOL_MANAGER, block, tracked(), &mut changes);
        changes
    }

    fn lp_fee_updates(changes: &[TransactionChanges]) -> Vec<(String, Vec<u8>, i32)> {
        changes
            .iter()
            .flat_map(|tx| &tx.entity_changes)
            .flat_map(|entity| {
                entity
                    .attributes
                    .iter()
                    .filter(|attribute| attribute.name == "lp_fee")
                    .map(|attribute| {
                        (entity.component_id.clone(), attribute.value.clone(), attribute.change)
                    })
            })
            .collect()
    }

    fn fee_bytes(fee: u32) -> Vec<u8> {
        BigInt::from(fee).to_signed_bytes_be()
    }

    #[test]
    fn a_fee_write_emits_the_new_lp_fee() {
        let block = block(vec![call(vec![write(
            &POOL_MANAGER,
            DYNAMIC_POOL,
            slot0(SQRT_PRICE_A, 3_000),
            slot0(SQRT_PRICE_A, 20_000),
            1,
        )])]);

        let changes = run(&block, vec![]);

        assert_eq!(
            lp_fee_updates(&changes),
            vec![(format!("0x{DYNAMIC_POOL}"), fee_bytes(20_000), i32::from(ChangeType::Update))]
        );
        assert_eq!(changes[0].tx.as_ref().unwrap().index, 3);
    }

    #[test]
    fn a_swap_that_leaves_the_fee_unchanged_emits_nothing() {
        let block = block(vec![call(vec![write(
            &POOL_MANAGER,
            DYNAMIC_POOL,
            slot0(SQRT_PRICE_A, 20_000),
            slot0(SQRT_PRICE_B, 20_000),
            1,
        )])]);

        assert!(lp_fee_updates(&run(&block, vec![])).is_empty());
    }

    #[test]
    fn a_reverted_call_emits_nothing() {
        let block = block(vec![reverted_call(vec![write(
            &POOL_MANAGER,
            DYNAMIC_POOL,
            slot0(SQRT_PRICE_A, 0),
            slot0(SQRT_PRICE_A, 20_000),
            1,
        )])]);

        assert!(lp_fee_updates(&run(&block, vec![])).is_empty());
    }

    #[test]
    fn a_static_fee_pool_emits_nothing() {
        let block = block(vec![call(vec![write(
            &POOL_MANAGER,
            STATIC_POOL,
            slot0(SQRT_PRICE_A, 3_000),
            slot0(SQRT_PRICE_A, 20_000),
            1,
        )])]);

        assert!(lp_fee_updates(&run(&block, vec![])).is_empty());
    }

    #[test]
    fn a_write_by_another_contract_emits_nothing() {
        let block = block(vec![call(vec![write(
            &OTHER_CONTRACT,
            DYNAMIC_POOL,
            slot0(SQRT_PRICE_A, 0),
            slot0(SQRT_PRICE_A, 20_000),
            1,
        )])]);

        assert!(lp_fee_updates(&run(&block, vec![])).is_empty());
    }

    #[test]
    fn initialize_and_after_initialize_in_one_transaction_emit_the_final_fee() {
        let block = block(vec![
            call(vec![write(&POOL_MANAGER, DYNAMIC_POOL, vec![0; 32], slot0(SQRT_PRICE_A, 0), 1)]),
            call(vec![write(
                &POOL_MANAGER,
                DYNAMIC_POOL,
                slot0(SQRT_PRICE_A, 0),
                slot0(SQRT_PRICE_A, 20_000),
                2,
            )]),
        ]);
        let creation = TransactionChanges {
            tx: Some(Transaction { hash: vec![7u8; 32], index: 3, ..Default::default() }),
            entity_changes: vec![EntityChanges {
                component_id: format!("0x{DYNAMIC_POOL}"),
                attributes: vec![Attribute {
                    name: "liquidity".to_string(),
                    value: vec![0],
                    change: ChangeType::Creation.into(),
                }],
            }],
            ..Default::default()
        };

        let changes = run(&block, vec![creation]);

        assert_eq!(changes.len(), 1, "the fee joins the creation transaction's changes");
        assert_eq!(
            lp_fee_updates(&changes),
            vec![(format!("0x{DYNAMIC_POOL}"), fee_bytes(20_000), i32::from(ChangeType::Creation))]
        );
    }
}
