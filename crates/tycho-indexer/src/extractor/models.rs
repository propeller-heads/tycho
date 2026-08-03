use std::collections::{hash_map::Entry, HashMap, HashSet};

pub use tycho_common::models::blockchain::{BlockChanges, TxWithContractChanges};
use tycho_common::{
    models::{
        blockchain::{Transaction, TxWithChanges},
        contract::AccountBalance,
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Address, AttrStoreKey, ComponentId, MergeError,
    },
    Bytes,
};

use crate::extractor::{
    reorg_buffer::ProtocolStateIdType, AccountStateIdType, AccountStateKeyType,
    AccountStateValueType, ProtocolStateKeyType, ProtocolStateValueType, StateUpdateBufferEntry,
};

/// Inserts or updates a state attribute for a protocol component within a specific transaction.
///
/// If a transaction with the same hash already exists, the state delta is merged with any
/// existing deltas for the same component. Otherwise, a new transaction entry is created.
///
/// # Arguments
///
/// * `txs_with_update` - Mutable reference to the vector of transaction changes
/// * `component_id` - The unique identifier of the protocol component
/// * `tx` - The transaction in which the state change occurred
/// * `attr` - The attribute key being updated
/// * `val` - The new value for the attribute
///
/// # Returns
///
/// * `Ok(())` if the operation succeeds
/// * `Err(MergeError)` if merging with existing deltas fails
pub(crate) fn insert_state_attribute_update(
    txs_with_update: &mut Vec<TxWithChanges>,
    component_id: &ComponentId,
    tx: &Transaction,
    attr: &AttrStoreKey,
    val: &ProtocolStateValueType,
) -> Result<(), MergeError> {
    let delta = ProtocolComponentStateDelta::new(
        component_id,
        HashMap::from([(attr.clone(), val.clone())]),
        HashSet::new(),
    );
    match txs_with_update
        .iter_mut()
        .find(|tx_with_changes| tx_with_changes.tx.hash == tx.hash)
    {
        Some(tx_with_changes) => {
            match tx_with_changes
                .state_updates
                .entry(component_id.clone())
            {
                Entry::Vacant(entry) => {
                    entry.insert(delta);
                }
                Entry::Occupied(mut entry) => {
                    let existing_delta = entry.get_mut();
                    existing_delta.merge(delta)?;
                }
            }
        }
        None => {
            let tx_with_changes = TxWithChanges {
                tx: tx.clone(),
                state_updates: HashMap::from([(component_id.clone(), delta)]),
                ..Default::default()
            };
            txs_with_update.push(tx_with_changes);
        }
    }
    Ok(())
}

/// Inserts a state attribute deletion for a protocol component within a specific transaction.
///
/// If a transaction with the same hash already exists, the state delta is merged with any
/// existing deltas for the same component. Otherwise, a new transaction entry is created.
///
/// # Arguments
///
/// * `txs_with_update` - Mutable reference to the vector of transaction changes
/// * `component_id` - The unique identifier of the protocol component
/// * `tx` - The transaction in which the attribute deletion occurred
/// * `attr` - The attribute key being deleted
///
/// # Returns
///
/// * `Ok(())` if the operation succeeds
/// * `Err(MergeError)` if merging with existing deltas fails
#[allow(dead_code)]
pub(crate) fn insert_state_attribute_deletion(
    txs_with_update: &mut Vec<TxWithChanges>,
    component_id: &ComponentId,
    tx: &Transaction,
    attr: &AttrStoreKey,
) -> Result<(), MergeError> {
    let delta = ProtocolComponentStateDelta::new(
        component_id,
        HashMap::new(),
        HashSet::from([attr.clone()]),
    );
    match txs_with_update
        .iter_mut()
        .find(|tx_with_changes| tx_with_changes.tx.hash == tx.hash)
    {
        Some(tx_with_changes) => {
            match tx_with_changes
                .state_updates
                .entry(component_id.clone())
            {
                Entry::Vacant(entry) => {
                    entry.insert(delta);
                }
                Entry::Occupied(mut entry) => {
                    let existing_delta = entry.get_mut();
                    existing_delta.merge(delta)?;
                }
            }
        }
        None => {
            let tx_with_changes = TxWithChanges {
                tx: tx.clone(),
                state_updates: HashMap::from([(component_id.clone(), delta)]),
                ..Default::default()
            };
            txs_with_update.push(tx_with_changes);
        }
    }
    Ok(())
}

impl StateUpdateBufferEntry for BlockChanges {
    fn get_filtered_protocol_state_update(
        &self,
        keys: Vec<(&ProtocolStateIdType, &ProtocolStateKeyType)>,
    ) -> HashMap<(ProtocolStateIdType, ProtocolStateKeyType), ProtocolStateValueType> {
        // Convert keys to a HashSet for faster lookups
        let keys_set: HashSet<(&ComponentId, &AttrStoreKey)> = keys.into_iter().collect();
        let mut res = HashMap::new();

        for update in self.txs_with_update.iter().rev() {
            for (component_id, protocol_update) in update.state_updates.iter() {
                for (attr, val) in protocol_update
                    .updated_attributes
                    .iter()
                    .filter(|(attr, _)| keys_set.contains(&(component_id, attr)))
                {
                    res.entry((component_id.clone(), attr.clone()))
                        .or_insert(val.clone());
                }
            }
        }

        res
    }

    #[allow(clippy::mutable_key_type)]
    fn get_filtered_account_state_update(
        &self,
        keys: Vec<(&AccountStateIdType, &AccountStateKeyType)>,
    ) -> HashMap<(AccountStateIdType, AccountStateKeyType), AccountStateValueType> {
        let keys_set: HashSet<_> = keys.into_iter().collect();
        let mut res = HashMap::new();

        for update in self.txs_with_update.iter().rev() {
            for (address, account_update) in update.account_deltas.iter() {
                for (slot, val) in account_update
                    .slots
                    .iter()
                    .filter(|(slot, _)| keys_set.contains(&(address, *slot)))
                {
                    res.entry((address.clone(), slot.clone()))
                        .or_insert(val.clone().unwrap_or_default());
                }
            }
        }

        res
    }

    #[allow(clippy::mutable_key_type)] // Clippy thinks that tuple with Bytes are a mutable type.
    fn get_filtered_component_balance_update(
        &self,
        keys: Vec<(&ComponentId, &Address)>,
    ) -> HashMap<(String, Bytes), ComponentBalance> {
        // Convert keys to a HashSet for faster lookups
        let keys_set: HashSet<(&String, &Bytes)> = keys.into_iter().collect();

        let mut res = HashMap::new();

        for update in self.txs_with_update.iter().rev() {
            for (component_id, balance_update) in update.balance_changes.iter() {
                for (token, value) in balance_update
                    .iter()
                    .filter(|(token, _)| keys_set.contains(&(component_id, token)))
                {
                    res.entry((component_id.clone(), token.clone()))
                        .or_insert(value.clone());
                }
            }
        }

        res
    }

    #[allow(clippy::mutable_key_type)] // Clippy thinks that tuple with Bytes are a mutable type.
    fn get_filtered_account_balance_update(
        &self,
        keys: Vec<(&Address, &Address)>,
    ) -> HashMap<(Address, Address), AccountBalance> {
        // Convert keys to a HashSet for faster lookups
        let keys_set: HashSet<(&Bytes, &Bytes)> = keys.into_iter().collect();

        let mut res = HashMap::new();

        for update in self.txs_with_update.iter().rev() {
            for (account, balance_update) in update.account_balance_changes.iter() {
                for (token, value) in balance_update
                    .iter()
                    .filter(|(token, _)| keys_set.contains(&(account, token)))
                {
                    res.entry((account.clone(), token.clone()))
                        .or_insert(value.clone());
                }
            }
        }

        res
    }
}

#[cfg(test)]
pub mod fixtures {
    use std::collections::HashMap;

    use tycho_common::{
        models::{
            blockchain::{EntryPoint, RPCTracerParams, TracingParams, Transaction},
            contract::{AccountBalance, AccountDelta},
            protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
            Chain, ChangeType,
        },
        Bytes,
    };

    use super::*;

    pub const HASH_256_0: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000000";
    pub const HASH_256_1: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000001";

    pub fn slots(data: impl IntoIterator<Item = (u64, u64)>) -> HashMap<Bytes, Bytes> {
        data.into_iter()
            .map(|(s, v)| (Bytes::from(s).lpad(32, 0), Bytes::from(v).lpad(32, 0)))
            .collect()
    }

    // Utils function that return slots that match `AccountDelta` slots.
    // TODO: this is temporary, we shoud make AccountDelta.slots use Bytes instead of Option<Bytes>
    pub fn optional_slots(
        data: impl IntoIterator<Item = (u64, u64)>,
    ) -> HashMap<Bytes, Option<Bytes>> {
        data.into_iter()
            .map(|(s, v)| (Bytes::from(s).lpad(32, 0), Some(Bytes::from(v).lpad(32, 0))))
            .collect()
    }

    // PERF: duplicated in tycho_common::models::blockchain::fixtures — consider a `test-utils`
    // feature flag to share test fixtures cross-crate.
    pub fn create_transaction(hash: &str, block: &str, index: u64) -> Transaction {
        Transaction::new(
            hash.parse().unwrap(),
            block.parse().unwrap(),
            Bytes::zero(20),
            Some(Bytes::zero(20)),
            index,
        )
    }

    /// Returns a pre-built `TxWithChanges` for testing.
    ///
    /// Both indices share the same keys (component `"pool_0"`, token `0xaa..`, contract `0xbb..`)
    /// but with different values, so "later wins" precedence can be verified across all fields:
    ///
    /// - Index 0: tx_index=1, component_balance {token=800, token2=300}, account_balance
    ///   {token=500, token2=150}, slots {1=>100, 2=>200}, state {"reserve"=>1000, "fee"=>50}, 1
    ///   entrypoint, ChangeType::Creation
    /// - Index 1: tx_index=2, component_balance {token=1000}, account_balance {token=700}, slots
    ///   {1=>300} (overlaps slot 1), state {"reserve"=>2000} (overlaps), 2 entrypoints (superset),
    ///   ChangeType::Update
    // PERF: duplicated from tycho_common::models::blockchain::fixtures::tx_with_changes.
    // Consider adding a `test-utils` feature flag to share test fixtures cross-crate.
    pub fn tx_with_changes(index: u8) -> TxWithChanges {
        let token = Bytes::from(vec![0xaa; 20]);
        let token2 = Bytes::from(vec![0xcc; 20]);
        let contract = Bytes::from(vec![0xbb; 20]);
        let c_id = "pool_0".to_string();

        match index {
            0 => {
                let tx = create_transaction("0x01", "0x00", 1);
                TxWithChanges {
                    tx: tx.clone(),
                    protocol_components: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponent { id: c_id.clone(), ..Default::default() },
                    )]),
                    account_deltas: HashMap::from([(
                        contract.clone(),
                        AccountDelta::new(
                            Chain::Ethereum,
                            contract.clone(),
                            HashMap::from([
                                (
                                    Bytes::from(1u64).lpad(32, 0),
                                    Some(Bytes::from(100u64).lpad(32, 0)),
                                ),
                                (
                                    Bytes::from(2u64).lpad(32, 0),
                                    Some(Bytes::from(200u64).lpad(32, 0)),
                                ),
                            ]),
                            None,
                            Some(Bytes::from(vec![0; 4])),
                            ChangeType::Creation,
                        ),
                    )]),
                    state_updates: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponentStateDelta::new(
                            &c_id,
                            HashMap::from([
                                ("reserve".into(), Bytes::from(1000u64).lpad(32, 0)),
                                ("fee".into(), Bytes::from(50u64).lpad(32, 0)),
                            ]),
                            HashSet::new(),
                        ),
                    )]),
                    balance_changes: HashMap::from([(
                        c_id.clone(),
                        HashMap::from([
                            (
                                token.clone(),
                                ComponentBalance {
                                    token: token.clone(),
                                    balance: Bytes::from(800_u64).lpad(32, 0),
                                    balance_float: 800.0,
                                    component_id: c_id.clone(),
                                    modify_tx: tx.hash.clone(),
                                },
                            ),
                            (
                                token2.clone(),
                                ComponentBalance {
                                    token: token2.clone(),
                                    balance: Bytes::from(300_u64).lpad(32, 0),
                                    balance_float: 300.0,
                                    component_id: c_id.clone(),
                                    modify_tx: tx.hash.clone(),
                                },
                            ),
                        ]),
                    )]),
                    account_balance_changes: HashMap::from([(
                        contract.clone(),
                        HashMap::from([
                            (
                                token.clone(),
                                AccountBalance {
                                    token: token.clone(),
                                    balance: Bytes::from(500_u64).lpad(32, 0),
                                    modify_tx: tx.hash.clone(),
                                    account: contract.clone(),
                                },
                            ),
                            (
                                token2,
                                AccountBalance {
                                    token: Bytes::from(vec![0xcc; 20]),
                                    balance: Bytes::from(150_u64).lpad(32, 0),
                                    modify_tx: tx.hash,
                                    account: contract,
                                },
                            ),
                        ]),
                    )]),
                    entrypoints: HashMap::from([(
                        c_id.clone(),
                        HashSet::from([EntryPoint::new(
                            "ep_0".into(),
                            Bytes::zero(20),
                            "fn_a()".into(),
                        )]),
                    )]),
                    entrypoint_params: HashMap::from([(
                        "ep_0".into(),
                        HashSet::from([(
                            TracingParams::RPCTracer(RPCTracerParams::new(
                                None,
                                Bytes::from(vec![1]),
                            )),
                            c_id,
                        )]),
                    )]),
                }
            }
            1 => {
                let tx = create_transaction("0x02", "0x00", 2);
                TxWithChanges {
                    tx: tx.clone(),
                    protocol_components: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponent { id: c_id.clone(), ..Default::default() },
                    )]),
                    account_deltas: HashMap::from([(
                        contract.clone(),
                        AccountDelta::new(
                            Chain::Ethereum,
                            contract.clone(),
                            HashMap::from([(
                                Bytes::from(1u64).lpad(32, 0),
                                Some(Bytes::from(300u64).lpad(32, 0)),
                            )]),
                            None,
                            None,
                            ChangeType::Update,
                        ),
                    )]),
                    state_updates: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponentStateDelta::new(
                            &c_id,
                            HashMap::from([("reserve".into(), Bytes::from(2000u64).lpad(32, 0))]),
                            HashSet::new(),
                        ),
                    )]),
                    balance_changes: HashMap::from([(
                        c_id.clone(),
                        HashMap::from([(
                            token.clone(),
                            ComponentBalance {
                                token: token.clone(),
                                balance: Bytes::from(1000_u64).lpad(32, 0),
                                balance_float: 1000.0,
                                component_id: c_id.clone(),
                                modify_tx: tx.hash.clone(),
                            },
                        )]),
                    )]),
                    account_balance_changes: HashMap::from([(
                        contract.clone(),
                        HashMap::from([(
                            token.clone(),
                            AccountBalance {
                                token: token.clone(),
                                balance: Bytes::from(700_u64).lpad(32, 0),
                                modify_tx: tx.hash,
                                account: contract,
                            },
                        )]),
                    )]),
                    entrypoints: HashMap::from([(
                        c_id.clone(),
                        HashSet::from([
                            EntryPoint::new("ep_0".into(), Bytes::zero(20), "fn_a()".into()),
                            EntryPoint::new("ep_1".into(), Bytes::zero(20), "fn_b()".into()),
                        ]),
                    )]),
                    entrypoint_params: HashMap::from([(
                        "ep_1".into(),
                        HashSet::from([(
                            TracingParams::RPCTracer(RPCTracerParams::new(
                                None,
                                Bytes::from(vec![2]),
                            )),
                            c_id,
                        )]),
                    )]),
                }
            }
            _ => panic!("tx_with_changes: index must be 0 or 1, got {index}"),
        }
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, str::FromStr};

    use fixtures::{create_transaction, HASH_256_0};
    use rstest::rstest;
    use tycho_common::{
        models::{
            blockchain::{Block, Transaction, TxWithChanges},
            contract::AccountBalance,
            protocol::ComponentBalance,
            token::Token,
            Address, Chain, MergeError,
        },
        Bytes,
    };

    use super::*;

    fn default_tx() -> Transaction {
        create_transaction(HASH_256_0, HASH_256_0, 0)
    }

    fn block_changes_with(txs: Vec<TxWithChanges>) -> BlockChanges {
        BlockChanges::new("test".into(), Chain::Ethereum, Block::default(), 0, false, txs, vec![])
    }

    fn block_changes_from_fixtures() -> BlockChanges {
        block_changes_with(vec![fixtures::tx_with_changes(0), fixtures::tx_with_changes(1)])
    }

    /// Creates a partial `BlockChanges` from `tx_with_changes(index)`.
    ///
    /// Reuses the shared fixture for tx-level data (balances, deltas, state, entrypoints)
    /// and adds block-level objects: `partial_block_index`, `new_tokens`,
    /// `block_contract_changes`. Overrides tx index to 5/2 for sort-order testing.
    fn partial_block_changes(index: u8) -> BlockChanges {
        let mut tx = fixtures::tx_with_changes(index);
        let (tx_index, token_addr, symbol, decimals): (u64, _, _, u32) = match index {
            0 => (5, Bytes::from(vec![0x33; 20]), "TOKEN1", 18),
            1 => (2, Bytes::from(vec![0x44; 20]), "TOKEN2", 6),
            _ => panic!("partial_block_changes: index must be 0 or 1, got {index}"),
        };
        tx.tx.index = tx_index;

        BlockChanges {
            extractor: "test".to_string(),
            chain: Chain::Ethereum,
            block: Block { number: 100, ..Block::default() },
            partial_block_index: Some(index as u32),
            txs_with_update: vec![tx],
            new_tokens: HashMap::from([(
                token_addr.clone(),
                Token::new(&token_addr, symbol, decimals, 0, &[], Chain::Ethereum, 0),
            )]),
            block_contract_changes: vec![TxWithContractChanges::default()],
            ..Default::default()
        }
    }

    #[test]
    fn test_block_changes_account_state_filter() {
        let block_changes = block_changes_from_fixtures();

        let contract = Bytes::from(vec![0xbb; 20]);
        let slot1 = Bytes::from(1u64).lpad(32, 0);
        let slot2 = Bytes::from(2u64).lpad(32, 0);
        let missing_account = Bytes::from(vec![0xff; 20]);
        let missing_slot = Bytes::from(99u64).lpad(32, 0);

        let keys = vec![
            (&contract, &slot1),
            (&contract, &slot2),
            (&missing_account, &slot1),
            (&contract, &missing_slot),
        ];

        #[allow(clippy::mutable_key_type)]
        let filtered = block_changes.get_filtered_account_state_update(keys);

        assert_eq!(
            filtered,
            HashMap::from([
                // slot1 in both txs: tx1 value (300) wins over tx0 value (100)
                ((contract.clone(), slot1), Bytes::from(300u64).lpad(32, 0)),
                // slot2 only in tx0: value (200) returned
                ((contract, slot2), Bytes::from(200u64).lpad(32, 0)),
            ])
        );
    }

    #[rstest]
    #[case::commit_before_finalized(None, Ok(None))]
    #[case::commit_before_finalized(Some(4), Ok(Some(4)))]
    #[case::commit_equals_finalized(Some(5), Ok(Some(5)))]
    #[case::commit_exceeds_finalized(Some(6), Err(MergeError::InvalidState(String::new())))]
    fn into_aggregated_respects_commit_invariant(
        #[case] committed_height: Option<u64>,
        #[case] expected: Result<Option<u64>, MergeError>,
    ) {
        let changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            Block::default(),
            5,
            false,
            Vec::new(),
            Vec::new(),
        );

        let result = changes.into_aggregated(committed_height);

        match expected {
            Ok(expected_height) => {
                let aggregated = result.expect("expected success");
                assert_eq!(aggregated.db_committed_block_height, expected_height);
            }
            Err(MergeError::InvalidState(_)) => {
                assert!(matches!(result, Err(MergeError::InvalidState(_))));
            }
            _ => panic!("unexpected error type"),
        }
    }

    #[test]
    fn test_block_changes_component_balance_filter() {
        let block_changes = block_changes_from_fixtures();

        let c_id = "pool_0".to_string();
        let token = Bytes::from(vec![0xaa; 20]);
        let token2 = Bytes::from(vec![0xcc; 20]);
        let missing_token = Bytes::zero(20);
        let missing_component = "missing".to_string();

        let keys = vec![
            (&c_id, &token),
            (&c_id, &token2),
            (&c_id, &missing_token),
            (&missing_component, &token),
        ];

        #[allow(clippy::mutable_key_type)]
        let filtered = block_changes.get_filtered_component_balance_update(keys);

        assert_eq!(
            filtered,
            HashMap::from([
                (
                    (c_id.clone(), token.clone()),
                    // token in both txs: tx1 balance (1000) wins over tx0 balance (800)
                    ComponentBalance {
                        token: token.clone(),
                        balance: Bytes::from(1000_u64).lpad(32, 0),
                        balance_float: 1000.0,
                        modify_tx: "0x02".parse().unwrap(),
                        component_id: c_id.clone(),
                    }
                ),
                (
                    (c_id.clone(), token2.clone()),
                    // token2 only in tx0: value (300) returned
                    ComponentBalance {
                        token: token2,
                        balance: Bytes::from(300_u64).lpad(32, 0),
                        balance_float: 300.0,
                        modify_tx: "0x01".parse().unwrap(),
                        component_id: c_id.clone(),
                    }
                ),
            ])
        );
    }

    #[test]
    fn test_block_changes_account_balance_filter() {
        let block_changes = block_changes_from_fixtures();

        let contract = Bytes::from(vec![0xbb; 20]);
        let token = Bytes::from(vec![0xaa; 20]);
        let token2 = Bytes::from(vec![0xcc; 20]);
        let missing_token = Bytes::zero(20);
        let missing_account = Bytes::from(vec![0xff; 20]);

        let keys = vec![
            (&contract, &token),
            (&contract, &token2),
            (&contract, &missing_token),
            (&missing_account, &token),
        ];

        #[allow(clippy::mutable_key_type)]
        let filtered = block_changes.get_filtered_account_balance_update(keys);

        assert_eq!(
            filtered,
            HashMap::from([
                (
                    (contract.clone(), token.clone()),
                    // token in both txs: tx1 balance (700) wins over tx0 balance (500)
                    AccountBalance {
                        token: token.clone(),
                        balance: Bytes::from(700_u64).lpad(32, 0),
                        modify_tx: "0x02".parse().unwrap(),
                        account: contract.clone(),
                    }
                ),
                (
                    (contract.clone(), token2.clone()),
                    // token2 only in tx0: value (150) returned
                    AccountBalance {
                        token: token2,
                        balance: Bytes::from(150_u64).lpad(32, 0),
                        modify_tx: "0x01".parse().unwrap(),
                        account: contract.clone(),
                    }
                ),
            ])
        );
    }

    #[test]
    fn test_block_changes_protocol_state_filter() {
        let block_changes = block_changes_from_fixtures();

        let c_id = "pool_0".to_string();
        let attr_reserve = "reserve".to_string();
        let attr_fee = "fee".to_string();
        let missing = "missing".to_string();

        let keys = vec![
            (&c_id, &attr_reserve),
            (&c_id, &attr_fee),
            (&missing, &attr_reserve),
            (&c_id, &missing),
        ];

        let filtered = block_changes.get_filtered_protocol_state_update(keys);
        assert_eq!(
            filtered,
            HashMap::from([
                // "reserve" in both txs: tx1 value (2000) wins over tx0 value (1000)
                ((c_id.clone(), attr_reserve), Bytes::from(2000u64).lpad(32, 0)),
                // "fee" only in tx0: value (50) returned
                ((c_id, attr_fee), Bytes::from(50u64).lpad(32, 0)),
            ])
        );
    }

    #[test]
    fn test_insert_state_attribute_update_new_transaction() {
        let mut block_changes = block_changes_with(vec![]);

        let component_id = "test_component".to_string();
        let tx = default_tx();
        let attr = "reserve0".to_string();
        let val = Bytes::from(1000u64).lpad(32, 0);

        insert_state_attribute_update(
            &mut block_changes.txs_with_update,
            &component_id,
            &tx,
            &attr,
            &val,
        )
        .unwrap();

        assert_eq!(block_changes.txs_with_update.len(), 1);
        let state_delta = &block_changes.txs_with_update[0].state_updates[&component_id];
        assert_eq!(state_delta.updated_attributes[&attr], val);
    }

    #[test]
    fn test_insert_state_attribute_update_existing_transaction_no_component() {
        let tx = default_tx();
        let existing_tx_with_changes =
            TxWithChanges { tx: tx.clone(), state_updates: HashMap::new(), ..Default::default() };

        let mut block_changes = block_changes_with(vec![existing_tx_with_changes]);

        let component_id = "test_component".to_string();
        let attr = "reserve0".to_string();
        let val = Bytes::from(1000u64).lpad(32, 0);

        insert_state_attribute_update(
            &mut block_changes.txs_with_update,
            &component_id,
            &tx,
            &attr,
            &val,
        )
        .unwrap();

        assert_eq!(block_changes.txs_with_update.len(), 1);
        let state_delta = &block_changes.txs_with_update[0].state_updates[&component_id];
        assert_eq!(state_delta.updated_attributes[&attr], val);
    }

    #[test]
    fn test_insert_state_attribute_update_existing_transaction_with_component() {
        let component_id = "test_component".to_string();
        let tx = default_tx();

        // Create existing delta for the component
        let existing_delta = ProtocolComponentStateDelta::new(
            &component_id,
            HashMap::from([("existing_attr".to_string(), Bytes::from(500u64).lpad(32, 0))]),
            HashSet::new(),
        );

        let existing_tx_with_changes = TxWithChanges {
            tx: tx.clone(),
            state_updates: HashMap::from([(component_id.clone(), existing_delta)]),
            ..Default::default()
        };

        let mut block_changes = block_changes_with(vec![existing_tx_with_changes]);

        let attr = "new_attr".to_string();
        let val = Bytes::from(1000u64).lpad(32, 0);

        insert_state_attribute_update(
            &mut block_changes.txs_with_update,
            &component_id,
            &tx,
            &attr,
            &val,
        )
        .unwrap();

        assert_eq!(block_changes.txs_with_update.len(), 1);
        let state_delta = &block_changes.txs_with_update[0].state_updates[&component_id];
        assert_eq!(state_delta.updated_attributes.len(), 2);
        assert_eq!(state_delta.updated_attributes[&attr], val);
        assert_eq!(
            state_delta.updated_attributes["existing_attr"],
            Bytes::from(500u64).lpad(32, 0)
        );
    }

    #[test]
    fn test_insert_state_attribute_deletion_new_transaction() {
        let mut block_changes = block_changes_with(vec![]);

        let component_id = "test_component".to_string();
        let tx = default_tx();
        let attr = "deprecated_attr".to_string();

        insert_state_attribute_deletion(
            &mut block_changes.txs_with_update,
            &component_id,
            &tx,
            &attr,
        )
        .unwrap();

        assert_eq!(block_changes.txs_with_update.len(), 1);
        let state_delta = &block_changes.txs_with_update[0].state_updates[&component_id];
        assert!(state_delta
            .deleted_attributes
            .contains(&attr));
    }

    #[test]
    fn test_insert_state_attribute_deletion_existing_transaction_no_component() {
        let tx = default_tx();
        let existing_tx_with_changes =
            TxWithChanges { tx: tx.clone(), state_updates: HashMap::new(), ..Default::default() };

        let mut block_changes = block_changes_with(vec![existing_tx_with_changes]);

        let component_id = "test_component".to_string();
        let attr = "deprecated_attr".to_string();

        insert_state_attribute_deletion(
            &mut block_changes.txs_with_update,
            &component_id,
            &tx,
            &attr,
        )
        .unwrap();

        assert_eq!(block_changes.txs_with_update.len(), 1);
        let state_delta = &block_changes.txs_with_update[0].state_updates[&component_id];
        assert!(state_delta
            .deleted_attributes
            .contains(&attr));
    }

    #[test]
    fn test_insert_state_attribute_deletion_existing_transaction_with_component() {
        let component_id = "test_component".to_string();
        let tx = default_tx();

        // Create existing delta for the component
        let existing_delta = ProtocolComponentStateDelta::new(
            &component_id,
            HashMap::new(),
            HashSet::from(["existing_deleted_attr".to_string()]),
        );

        let existing_tx_with_changes = TxWithChanges {
            tx: tx.clone(),
            state_updates: HashMap::from([(component_id.clone(), existing_delta)]),
            ..Default::default()
        };

        let mut block_changes = block_changes_with(vec![existing_tx_with_changes]);

        let attr = "new_deleted_attr".to_string();

        insert_state_attribute_deletion(
            &mut block_changes.txs_with_update,
            &component_id,
            &tx,
            &attr,
        )
        .unwrap();

        assert_eq!(block_changes.txs_with_update.len(), 1);
        let state_delta = &block_changes.txs_with_update[0].state_updates[&component_id];
        assert_eq!(state_delta.deleted_attributes.len(), 2);
        assert!(state_delta
            .deleted_attributes
            .contains(&attr));
        assert!(state_delta
            .deleted_attributes
            .contains("existing_deleted_attr"));
    }

    #[rstest]
    #[case::merge_newer(0, 1)] // block0.merge(block1)
    #[case::merge_older(1, 0)] // block1.merge(block0) - should produce identical result
    fn test_merge_partial_combines_all_fields(#[case] first_idx: u8, #[case] second_idx: u8) {
        let first = partial_block_changes(first_idx);
        let second = partial_block_changes(second_idx);

        let result = first.merge_partial(second).unwrap();

        // txs from both partials are combined and sorted by index
        assert_eq!(result.txs_with_update.len(), 2);
        let indices: Vec<u64> = result
            .txs_with_update
            .iter()
            .map(|tx| tx.tx.index)
            .collect();
        assert_eq!(indices, vec![2, 5]);

        // tokens from both partials are merged
        assert_eq!(result.new_tokens.len(), 2);
        let token_addr0 = Bytes::from(vec![0x33; 20]);
        let token_addr1 = Bytes::from(vec![0x44; 20]);
        assert!(result
            .new_tokens
            .contains_key(&token_addr0));
        assert!(result
            .new_tokens
            .contains_key(&token_addr1));

        // Each partial's per-tx data survives the merge intact.
        // After sorting: [0] = second partial (tx_index=2), [1] = first partial (tx_index=5).
        let token = Bytes::from(vec![0xaa; 20]);
        let token2 = Bytes::from(vec![0xcc; 20]);
        let contract = Bytes::from(vec![0xbb; 20]);
        let c_id = "pool_0".to_string();

        // Second partial (tx_index=2): token balance=1000, account_balance=700 (no token2)
        assert_eq!(
            result.txs_with_update[0].balance_changes[&c_id][&token].balance,
            Bytes::from(1000_u64).lpad(32, 0)
        );
        assert_eq!(
            result.txs_with_update[0].account_balance_changes[&contract][&token].balance,
            Bytes::from(700_u64).lpad(32, 0)
        );

        // First partial (tx_index=5): token balance=800, token2 balance=300
        assert_eq!(
            result.txs_with_update[1].balance_changes[&c_id][&token].balance,
            Bytes::from(800_u64).lpad(32, 0)
        );
        assert_eq!(
            result.txs_with_update[1].balance_changes[&c_id][&token2].balance,
            Bytes::from(300_u64).lpad(32, 0)
        );

        // First partial (tx_index=5): account_balance token=500, token2=150
        assert_eq!(
            result.txs_with_update[1].account_balance_changes[&contract][&token].balance,
            Bytes::from(500_u64).lpad(32, 0)
        );
        assert_eq!(
            result.txs_with_update[1].account_balance_changes[&contract][&token2].balance,
            Bytes::from(150_u64).lpad(32, 0)
        );

        assert_eq!(result.block_contract_changes.len(), 2);
        assert_eq!(result.partial_block_index, Some(1));
    }

    #[rstest]
    #[case(Some(String::from("different")), None, None, None, "extractor")]
    #[case(None, Some(Chain::Arbitrum), None, None, "chain")]
    #[case(None, None, Some(101), None, "different blocks")]
    #[case(None, None, None, Some(true), "different revert")]
    fn test_merge_partial_validation_fails(
        #[case] extractor_override: Option<String>,
        #[case] chain_override: Option<Chain>,
        #[case] block_num_override: Option<u64>,
        #[case] revert_override: Option<bool>,
        #[case] expected_error_msg: &str,
    ) {
        let original_partial_block = partial_block_changes(0);

        let mut other_block = original_partial_block.clone();
        other_block.partial_block_index = Some(1);

        other_block.extractor =
            extractor_override.unwrap_or(original_partial_block.extractor.clone());
        other_block.chain = chain_override.unwrap_or(original_partial_block.chain);
        other_block.block.number =
            block_num_override.unwrap_or(original_partial_block.block.number);
        other_block.revert = revert_override.unwrap_or(original_partial_block.revert);

        let result1 = original_partial_block
            .clone()
            .merge_partial(other_block.clone());
        let result2 = other_block.merge_partial(original_partial_block);

        assert!(result1.is_err(), "Expected MergeError for original.merge(other)");
        assert!(result2.is_err(), "Expected MergeError for other.merge(original)");

        let error_msg1 = result1.unwrap_err().to_string();
        assert!(
            error_msg1.contains(expected_error_msg),
            "Expected error to contain '{}', got: {}",
            expected_error_msg,
            error_msg1
        );

        let error_msg2 = result2.unwrap_err().to_string();
        assert!(
            error_msg2.contains(expected_error_msg),
            "Expected symmetric error to contain '{}', got: {}",
            expected_error_msg,
            error_msg2
        );
    }

    #[test]
    fn test_merge_partial_same_index_fails() {
        let partial = partial_block_changes(0);
        let duplicate = partial_block_changes(0);

        let result = partial.clone().merge_partial(duplicate);
        assert!(matches!(result, Err(MergeError::InvalidState(_))));
    }

    #[test]
    fn test_merge_partial_transaction_sorting() {
        let mut partial1 = partial_block_changes(0);
        let mut partial2 = partial_block_changes(1);

        let mut tx1 = TxWithChanges::default();
        tx1.tx.index = 7;
        partial1.txs_with_update.push(tx1);

        let mut tx2 = TxWithChanges::default();
        tx2.tx.index = 1;
        partial2.txs_with_update.push(tx2);

        let result = partial1
            .merge_partial(partial2)
            .unwrap();

        let indices: Vec<u64> = result
            .txs_with_update
            .iter()
            .map(|tx| tx.tx.index)
            .collect();
        assert_eq!(indices, vec![1, 2, 5, 7]);
    }

    #[test]
    fn test_normalize_block_hash_empty() {
        let mut block_changes = partial_block_changes(0);
        block_changes.txs_with_update.clear();
        block_changes
            .block_contract_changes
            .clear();
        let expected_hash = block_changes.block.hash.clone();
        block_changes.normalize_block_hash();
        assert_eq!(block_changes.block.hash, expected_hash);
    }

    #[test]
    fn test_normalize_block_hash() {
        let block_hash =
            Bytes::from_str("0xabababababababababababababababababababababababababababababababab")
                .unwrap();
        let wrong_hash = Bytes::zero(32);

        let mut block_changes = BlockChanges {
            block: Block { hash: block_hash.clone(), ..Block::default() },
            ..Default::default()
        };
        for i in 0..3 {
            let mut tx = TxWithChanges::default();
            tx.tx.index = i;
            tx.tx.block_hash = wrong_hash.clone();
            block_changes.txs_with_update.push(tx);
        }
        for _ in 0..2 {
            let mut cc = TxWithContractChanges::default();
            cc.tx.block_hash = wrong_hash.clone();
            block_changes
                .block_contract_changes
                .push(cc);
        }

        block_changes.normalize_block_hash();

        for tx_with_changes in &block_changes.txs_with_update {
            assert_eq!(tx_with_changes.tx.block_hash, block_hash);
        }
        for tx_with_contract in &block_changes.block_contract_changes {
            assert_eq!(tx_with_contract.tx.block_hash, block_hash);
        }
    }

    #[rstest]
    #[case::merge_newer(0, 1)] // block0.merge(block1)
    #[case::merge_older(1, 0)] // block1.merge(block0)
    fn test_merge_partial_token_precedence(#[case] first_idx: u8, #[case] second_idx: u8) {
        let mut first = partial_block_changes(first_idx);
        let mut second = partial_block_changes(second_idx);

        let shared_token_addr =
            Address::from_str("0x5555555555555555555555555555555555555555").unwrap();

        first.new_tokens.clear();
        second.new_tokens.clear();

        let early_token = Token::new(&shared_token_addr, "EARLY", 6, 0, &[], Chain::Ethereum, 0);
        let late_token = Token::new(&shared_token_addr, "LATE", 18, 0, &[], Chain::Ethereum, 0);

        if first_idx == 0 {
            first
                .new_tokens
                .insert(shared_token_addr.clone(), early_token.clone());
            second
                .new_tokens
                .insert(shared_token_addr.clone(), late_token.clone());
        } else {
            first
                .new_tokens
                .insert(shared_token_addr.clone(), late_token.clone());
            second
                .new_tokens
                .insert(shared_token_addr.clone(), early_token.clone());
        }

        let result = first.merge_partial(second).unwrap();

        assert_eq!(result.new_tokens[&shared_token_addr].symbol, "LATE");
        assert_eq!(result.new_tokens[&shared_token_addr].decimals, 18);
    }
}
