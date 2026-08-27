//! Turns a pending block's post-execution accounts into the override set the pool getters run
//! against.

use std::collections::HashMap;

use alloy::primitives::{Address as AlloyAddress, U256};
use tycho_common::{
    models::{blockchain::PendingBlock, contract::AccountDelta},
    Bytes,
};
use tycho_simulation::evm::simulation::{BlockEnvOverrides, PendingOverrides};

/// The state `pending` would leave behind, as overrides on top of the engine's confirmed state.
///
/// Only the accounts the block touched appear, so every slot it did not write keeps its confirmed
/// value. The block environment always comes from the pending block itself: a ramping `A()` and
/// the rate providers behind `stored_rates()` interpolate against the block's own timestamp.
pub fn pending_overrides(pending: &PendingBlock) -> PendingOverrides {
    let mut storage: HashMap<AlloyAddress, HashMap<U256, U256>> = HashMap::new();
    let mut native_balances: HashMap<AlloyAddress, U256> = HashMap::new();

    for (address, delta) in pending.accounts() {
        let Some(address) = as_address(address) else { continue };
        let slots = account_slots(delta);
        if !slots.is_empty() {
            storage.insert(address, slots);
        }
        if let Some(balance) = &delta.balance {
            native_balances.insert(address, word(balance));
        }
    }

    let block = pending.block();
    PendingOverrides {
        storage: (!storage.is_empty()).then_some(storage),
        native_balances: (!native_balances.is_empty()).then_some(native_balances),
        block: Some(BlockEnvOverrides {
            number: Some(block.number),
            timestamp: Some(block.ts.and_utc().timestamp() as u64),
        }),
    }
}

/// A cleared slot is stored as `None` and reads back as zero, which is what the EVM would see.
fn account_slots(delta: &AccountDelta) -> HashMap<U256, U256> {
    delta
        .slots
        .iter()
        .map(|(slot, value)| {
            (
                word(slot),
                value
                    .as_ref()
                    .map(word)
                    .unwrap_or(U256::ZERO),
            )
        })
        .collect()
}

/// Interprets `bytes` as a big-endian EVM word. Values shorter than 32 bytes are left-padded,
/// which is how the indexer stores slots and balances with leading zeros trimmed.
fn word(bytes: &Bytes) -> U256 {
    U256::from_be_slice(bytes.as_ref())
}

fn as_address(bytes: &Bytes) -> Option<AlloyAddress> {
    (bytes.len() == 20).then(|| AlloyAddress::from_slice(bytes.as_ref()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_common::models::{blockchain::Block, contract::AccountDelta, Chain, ChangeType};

    use super::*;

    fn pending(accounts: HashMap<Bytes, AccountDelta>) -> PendingBlock {
        let block = Block {
            number: 23_526_115,
            chain: Chain::Ethereum,
            hash: Bytes::from(vec![1u8; 32]),
            parent_hash: Bytes::from(vec![2u8; 32]),
            ts: chrono::DateTime::from_timestamp(1_759_842_947, 0)
                .unwrap()
                .naive_utc(),
        };
        PendingBlock::new(block, vec![], accounts)
    }

    fn delta(
        address: [u8; 20],
        slots: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        balance: Option<Vec<u8>>,
    ) -> (Bytes, AccountDelta) {
        let address = Bytes::from(address.to_vec());
        (
            address.clone(),
            AccountDelta::new(
                Chain::Ethereum,
                address,
                slots
                    .into_iter()
                    .map(|(slot, value)| (Bytes::from(slot), value.map(Bytes::from)))
                    .collect(),
                balance.map(Bytes::from),
                None,
                ChangeType::Update,
            ),
        )
    }

    #[test]
    fn test_slots_and_balances_reach_the_overrides() {
        let pool = [0xaau8; 20];
        let overrides = pending_overrides(&pending(HashMap::from([delta(
            pool,
            vec![(vec![3], Some(vec![7]))],
            Some(vec![0x0d, 0xe0]),
        )])));

        let address = AlloyAddress::from_slice(&pool);
        assert_eq!(
            overrides
                .storage
                .as_ref()
                .and_then(|s| s.get(&address))
                .and_then(|slots| slots.get(&U256::from(3))),
            Some(&U256::from(7)),
            "a written slot must be overridden"
        );
        assert_eq!(
            overrides
                .native_balances
                .as_ref()
                .and_then(|b| b.get(&address)),
            Some(&U256::from(0x0de0)),
            "a native balance must be overridden"
        );
    }

    #[test]
    fn test_cleared_slot_reads_as_zero() {
        let pool = [0xbbu8; 20];
        let overrides =
            pending_overrides(&pending(HashMap::from([delta(pool, vec![(vec![1], None)], None)])));

        assert_eq!(
            overrides
                .storage
                .unwrap()
                .get(&AlloyAddress::from_slice(&pool))
                .and_then(|slots| slots.get(&U256::from(1))),
            Some(&U256::ZERO),
            "a cleared slot must override to zero, not fall through to confirmed state"
        );
    }

    #[test]
    fn test_block_environment_comes_from_the_pending_block() {
        let overrides = pending_overrides(&pending(HashMap::new()));

        assert_eq!(
            overrides.block,
            Some(BlockEnvOverrides { number: Some(23_526_115), timestamp: Some(1_759_842_947) }),
            "reading under the parent block's clock misprices a ramping pool"
        );
    }

    #[test]
    fn test_empty_accounts_override_no_state() {
        let overrides = pending_overrides(&pending(HashMap::new()));

        assert!(overrides.storage.is_none());
        assert!(overrides.native_balances.is_none());
    }

    #[test]
    fn test_short_slot_values_are_left_padded() {
        // The indexer trims leading zeros, so a one-byte value is the word 0x…01, not 0x01…00.
        assert_eq!(word(&Bytes::from(vec![1u8])), U256::from(1));
    }
}
