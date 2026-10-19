use num_bigint::BigInt;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    str::FromStr,
};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::pb::eth::{self, v2::CallType};

use tycho_substreams::prelude::*;

use crate::{contracts::main::AMBIENT_CONTRACT, pb::tycho::ambient::v1::BlockPoolChanges};

struct SlotValue {
    new_value: Vec<u8>,
    start_value: Vec<u8>,
}

impl SlotValue {
    fn has_changed(&self) -> bool {
        self.start_value != self.new_value
    }
}

// uses a map for slots, protobuf does not
// allow bytes in hashmap keys
struct InterimContractChange {
    address: Vec<u8>,
    balance: Vec<u8>,
    code: Vec<u8>,
    slots: HashMap<Vec<u8>, SlotValue>,
    change: ChangeType,
}

impl From<InterimContractChange> for ContractChange {
    fn from(value: InterimContractChange) -> Self {
        ContractChange {
            address: value.address,
            balance: value.balance,
            code: value.code,
            slots: value
                .slots
                .into_iter()
                .filter(|(_, value)| value.has_changed())
                .map(|(slot, value)| ContractSlot { slot, value: value.new_value })
                .collect(),
            change: value.change.into(),
            token_balances: Vec::new(),
        }
    }
}

/// Extracts all contract changes relevant to vm simulations
///
/// This implementation has currently two major limitations:
/// 1. It is hardwired to only care about changes to the ambient main contract, this is ok for this
///    particular use case but for a more general purpose implementation this is not ideal
/// 2. Changes are processed separately, this means that if there are any side effects between each
///    other (e.g. if account is deleted and then created again in ethereum all the storage is set
///    to 0. So there is a side effect between account creation and contract storage.) these might
///    not be properly accounted for. Most of the time this should not be a major issue but may lead
///    to wrong results so consume this implementation with care. See example below for a concrete
///    case where this is problematic.
///
/// ## A very contrived example:
/// 1. Some existing contract receives a transaction that changes it state, the state is updated
/// 2. Next, this contract has self destruct called on itself
/// 3. The contract is created again using CREATE2 at the same address
/// 4. The contract receives a transaction that changes it state
/// 5. We would emit this as as contract creation with slots set from 1 and from 4, although we
///    should only emit the slots changed from 4.
#[substreams::handlers::map]
fn map_changes(
    block: eth::v2::Block,
    block_pool_changes: BlockPoolChanges,
    balance_store: StoreDeltas,
    pool_store: StoreGetProto<ProtocolComponent>,
) -> Result<BlockChanges, substreams::errors::Error> {
    // TODO: this needs to be refactored to use the tycho-substreams helper functions.
    // The current implementation probably suffers from the bug fixed in https://github.com/propeller-heads/tycho-protocol-sdk/pull/117
    let mut block_changes = BlockChanges::default();

    let mut transaction_changes = TransactionChanges::default();

    let mut changed_contracts: HashMap<Vec<u8>, InterimContractChange> = HashMap::new();

    // collect all account creations in the block
    let created_accounts: HashSet<_> = block
        .transactions()
        .flat_map(|tx| {
            tx.calls
                .iter()
                .filter(|call| call.call_type() == CallType::Create)
                .map(|call| call.address.clone())
        })
        .collect();

    for block_tx in block.transactions() {
        let tx = Transaction {
            hash: block_tx.hash.clone(),
            from: block_tx.from.clone(),
            to: block_tx.to.clone(),
            index: block_tx.index as u64,
        };

        // extract storage changes
        let mut storage_changes = block_tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| {
                call.storage_changes
                    .iter()
                    .filter(|c| c.address == AMBIENT_CONTRACT)
            })
            .collect::<Vec<_>>();
        storage_changes.sort_unstable_by_key(|change| change.ordinal);

        // Note: some contracts change slot values and change them back to their
        //  original value before the transactions ends we remember the initial
        //  value before the first change and in the end filter found deltas
        //  that ended up not actually changing anything.
        for storage_change in storage_changes.iter() {
            match changed_contracts.entry(storage_change.address.clone()) {
                // We have already an entry recording a change about this contract
                //  only append the change about this storage slot
                Entry::Occupied(mut e) => {
                    let contract_change = e.get_mut();
                    match contract_change
                        .slots
                        .entry(storage_change.key.clone())
                    {
                        // The storage slot was already changed before, simply
                        //  update new_value
                        Entry::Occupied(mut v) => {
                            let slot_value = v.get_mut();
                            slot_value
                                .new_value
                                .copy_from_slice(&storage_change.new_value);
                        }
                        // The storage slots is being initialised for the first time
                        Entry::Vacant(v) => {
                            v.insert(SlotValue {
                                new_value: storage_change.new_value.clone(),
                                start_value: storage_change.old_value.clone(),
                            });
                        }
                    }
                }
                // Intialise a new contract change after observing a storage change
                Entry::Vacant(e) => {
                    let mut slots = HashMap::new();
                    slots.insert(
                        storage_change.key.clone(),
                        SlotValue {
                            new_value: storage_change.new_value.clone(),
                            start_value: storage_change.old_value.clone(),
                        },
                    );
                    e.insert(InterimContractChange {
                        address: storage_change.address.clone(),
                        balance: Vec::new(),
                        code: Vec::new(),
                        slots,
                        change: if created_accounts.contains(&storage_change.address) {
                            ChangeType::Creation
                        } else {
                            ChangeType::Update
                        },
                    });
                }
            }
        }

        // extract balance changes
        let mut balance_changes = block_tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| {
                call.balance_changes
                    .iter()
                    .filter(|c| c.address == AMBIENT_CONTRACT)
            })
            .collect::<Vec<_>>();
        balance_changes.sort_unstable_by_key(|change| change.ordinal);

        for balance_change in balance_changes.iter() {
            match changed_contracts.entry(balance_change.address.clone()) {
                Entry::Occupied(mut e) => {
                    let contract_change = e.get_mut();
                    if let Some(new_balance) = &balance_change.new_value {
                        contract_change.balance.clear();
                        contract_change
                            .balance
                            .extend_from_slice(&new_balance.bytes);
                    }
                }
                Entry::Vacant(e) => {
                    if let Some(new_balance) = &balance_change.new_value {
                        e.insert(InterimContractChange {
                            address: balance_change.address.clone(),
                            balance: new_balance.bytes.clone(),
                            code: Vec::new(),
                            slots: HashMap::new(),
                            change: if created_accounts.contains(&balance_change.address) {
                                ChangeType::Creation
                            } else {
                                ChangeType::Update
                            },
                        });
                    }
                }
            }
        }

        // extract code changes
        let mut code_changes = block_tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| {
                call.code_changes
                    .iter()
                    .filter(|c| c.address == AMBIENT_CONTRACT)
            })
            .collect::<Vec<_>>();
        code_changes.sort_unstable_by_key(|change| change.ordinal);

        for code_change in code_changes.iter() {
            match changed_contracts.entry(code_change.address.clone()) {
                Entry::Occupied(mut e) => {
                    let contract_change = e.get_mut();
                    contract_change.code.clear();
                    contract_change
                        .code
                        .extend_from_slice(&code_change.new_code);
                }
                Entry::Vacant(e) => {
                    e.insert(InterimContractChange {
                        address: code_change.address.clone(),
                        balance: Vec::new(),
                        code: code_change.new_code.clone(),
                        slots: HashMap::new(),
                        change: if created_accounts.contains(&code_change.address) {
                            ChangeType::Creation
                        } else {
                            ChangeType::Update
                        },
                    });
                }
            }
        }

        // if there were any changes, add transaction and push the changes
        if !storage_changes.is_empty() || !balance_changes.is_empty() || !code_changes.is_empty() {
            transaction_changes.tx = Some(tx.clone());

            // reuse changed_contracts hash map by draining it, next iteration
            // will start empty. This avoids a costly reallocation
            for (_, change) in changed_contracts.drain() {
                transaction_changes
                    .contract_changes
                    .push(change.into())
            }

            block_changes
                .changes
                .push(transaction_changes.clone());

            // clear out the interim contract changes after we pushed those.
            transaction_changes.tx = None;
            transaction_changes
                .contract_changes
                .clear();
        }
    }

    // extract new protocol components
    let mut grouped_components = HashMap::new();
    for component in &block_pool_changes.new_components {
        grouped_components
            .entry(component.tx_index)
            .or_insert_with(Vec::new)
            .push(component.clone());
    }
    for (tx_index, components) in grouped_components {
        if let Some(tx_change) = block_changes
            .changes
            .iter_mut()
            // TODO: be better than this (quadratic complexity)
            .find(|tx_change| {
                tx_change
                    .tx
                    .as_ref()
                    .is_some_and(|tx| tx.index == tx_index)
            })
        {
            let new_components = components
                .into_iter()
                .map(Into::into)
                .collect::<Vec<ProtocolComponent>>();
            tx_change
                .component_changes
                .extend(new_components);
        }
    }

    // extract component balance changes
    let mut balance_changes = HashMap::new();
    balance_store
        .deltas
        .into_iter()
        .zip(block_pool_changes.balance_deltas)
        .for_each(|(store_delta, balance_delta)| {
            let pool_hash_hex = hex::encode(balance_delta.pool_hash);
            let pool = match pool_store.get_last(pool_hash_hex.clone()) {
                Some(pool) => pool,
                None => panic!("Pool not found in store for given hash: {pool_hash_hex}"),
            };
            let token_type = substreams::key::segment_at(&store_delta.key, 1);
            let token_index = if token_type == "quote" { 1 } else { 0 };

            // store_delta.new_value is an ASCII string representing an integer
            let ascii_string =
                String::from_utf8(store_delta.new_value.clone()).expect("Invalid UTF-8 sequence");
            let balance = BigInt::from_str(&ascii_string).expect("Failed to parse integer");
            let big_endian_bytes_balance = balance.to_bytes_be().1;

            let balance_change = BalanceChange {
                component_id: pool_hash_hex.as_bytes().to_vec(),
                token: pool.tokens[token_index].clone(),
                balance: big_endian_bytes_balance.to_vec(),
            };
            balance_changes
                .entry(balance_delta.tx_index)
                .or_insert_with(Vec::new)
                .push(balance_change);
        });
    for (tx_index, grouped_balance_changes) in balance_changes {
        if let Some(tx_change) = block_changes
            .changes
            .iter_mut()
            // TODO: be better than this (quadratic complexity)
            .find(|tx_change| {
                tx_change
                    .tx
                    .as_ref()
                    .is_some_and(|tx| tx.index == tx_index)
            })
        {
            tx_change
                .balance_changes
                .extend(grouped_balance_changes);
        }
    }

    block_changes.block = Some(Block {
        number: block.number,
        hash: block.hash.clone(),
        parent_hash: block
            .header
            .as_ref()
            .expect("Block header not present")
            .parent_hash
            .clone(),
        ts: block.timestamp_seconds(),
    });

    Ok(block_changes)
}
