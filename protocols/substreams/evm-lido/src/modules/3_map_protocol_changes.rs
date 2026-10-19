use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{hex, pb::substreams::StoreDeltas, prelude::*};
use substreams_ethereum::pb::eth::{
    self,
    v2::{Call, CallType, StorageChange},
};
use tycho_substreams::prelude::*;

use crate::{
    ETH_ADDRESS, ST_ETH_ADDRESS_IMPL, ST_ETH_ADDRESS_PROXY, ST_ETH_ADDRESS_PROXY_COMPONENT_ID,
    WST_ETH_ADDRESS, WST_ETH_ADDRESS_COMPONENT_ID,
};

const STORAGE_SLOT_TOTAL_SHARES: [u8; 32] =
    hex!("e3b4b636e601189b5f4c6742edf2538ac12bb61ed03e6da26949d69838fa447e");
const STORAGE_SLOT_WRAPPED_ETH: [u8; 32] =
    hex!("0000000000000000000000000000000000000000000000000000000000000002");
const STORAGE_SLOT_STAKE_LIMIT: [u8; 32] =
    hex!("a3678de4a579be090bed1177e0a24f77cc29d181ac22fd7688aca344d8938015");

const ZERO_STAKING_LIMIT: &str = "000000000000000000000000";
const DEPOSIT_SIZE_STR: &str = "32000000000000000000"; // 32 ETH

/// Extracts balances per component
///
/// This function uses ERC20 transfer events to extract balances. It
/// assumes that each component is deployed at a dedicated contract address. If a
/// transaction involving the component is detected, its balance is updated accordingly.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    protocol_components: BlockTransactionProtocolComponents,
    storage_deltas: StoreDeltas,
    storage_store: StoreGetBigInt,
) -> Result<BlockChanges, substreams::errors::Error> {
    let start_block: u64 = params
        .parse()
        .expect("Failed to parse start block parameter");
    let mut transaction_changes: HashMap<u32, TransactionChangesBuilder> = HashMap::new();

    for tx_component in protocol_components.tx_components.iter() {
        let tx = tx_component
            .tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot get staking limit from the storage slot"))?;

        let builder = transaction_changes
            .entry(tx.index as u32)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));

        tx_component
            .components
            .iter()
            .for_each(|c| {
                builder.add_protocol_component(c);
            });
    }

    handle_sync(&block, &mut transaction_changes, storage_deltas, storage_store, start_block)?;

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: vec![],
    })
}

fn handle_sync(
    block: &eth::v2::Block,
    transaction_changes: &mut HashMap<u32, TransactionChangesBuilder>,
    storage_deltas: StoreDeltas,
    storage_store: StoreGetBigInt,
    start_block: u64,
) -> Result<()> {
    for tx in block.transactions() {
        for call in tx.calls.iter() {
            if call.state_reverted {
                continue;
            }
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

            // Only these cases need handling: transactions to stETH for deposit/withdraw and oracle
            // update, to wstETH for wrap/unwrap, and delegate actions for the
            // governance DAO that modify the staking limit.
            if call.address == ST_ETH_ADDRESS_IMPL {
                st_eth_entity_changes(call, builder)?
            } else if call.address == WST_ETH_ADDRESS {
                wst_eth_entity_changes(call, builder)?
            } else if call.call_type == CallType::Delegate as i32 {
                staking_entity_changes(call, builder)?;
            } else {
                continue;
            };
        }
    }

    // Handle storage deltas - process in chronological order to maintain correct state
    let mut sorted_deltas = storage_deltas
        .deltas
        .iter()
        .filter(|delta| {
            delta
                .key
                .contains(&format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:"))
        })
        .collect::<Vec<_>>();
    sorted_deltas.sort_by_key(|delta| delta.ordinal);

    let mut current_buffered_eth =
        get_initial_value_for_block(&sorted_deltas, &storage_store, "buffered_eth");
    let mut current_cl_balance_position =
        get_initial_value_for_block(&sorted_deltas, &storage_store, "cl_balance_position");
    let mut current_cl_validators_position =
        get_initial_value_for_block(&sorted_deltas, &storage_store, "cl_validators_position");
    let mut current_deposited_validators_position = get_initial_value_for_block(
        &sorted_deltas,
        &storage_store,
        "deposited_validators_position",
    );

    for delta in sorted_deltas.iter() {
        let tx_ordinal = delta.ordinal;

        // Update the specific variable that changed
        if delta.key.ends_with("buffered_eth") {
            let value_str = String::from_utf8(delta.new_value.clone())
                .expect("Failed to decode buffered_eth delta value as UTF-8");
            current_buffered_eth = BigInt::from(
                num_bigint::BigInt::parse_bytes(value_str.as_bytes(), 10)
                    .expect("Failed to parse buffered_eth as decimal BigInt"),
            );
        } else if delta
            .key
            .ends_with("cl_balance_position")
        {
            let value_str = String::from_utf8(delta.new_value.clone())
                .expect("Failed to decode cl_balance_position delta value as UTF-8");
            current_cl_balance_position = BigInt::from(
                num_bigint::BigInt::parse_bytes(value_str.as_bytes(), 10)
                    .expect("Failed to parse cl_balance_position as decimal BigInt"),
            );
        } else if delta
            .key
            .ends_with("cl_validators_position")
        {
            let value_str = String::from_utf8(delta.new_value.clone())
                .expect("Failed to decode cl_validators_position delta value as UTF-8");
            current_cl_validators_position = BigInt::from(
                num_bigint::BigInt::parse_bytes(value_str.as_bytes(), 10)
                    .expect("Failed to parse cl_validators_position as decimal BigInt"),
            );
        } else if delta
            .key
            .ends_with("deposited_validators_position")
        {
            let value_str = String::from_utf8(delta.new_value.clone())
                .expect("Failed to decode deposited_validators_position delta value as UTF-8");
            current_deposited_validators_position = BigInt::from(
                num_bigint::BigInt::parse_bytes(value_str.as_bytes(), 10)
                    .expect("Failed to parse deposited_validators_position as decimal BigInt"),
            );
        }

        // Check if this is the last delta for this transaction ordinal
        let is_last_delta_for_tx = sorted_deltas
            .iter()
            .rfind(|d| d.ordinal == tx_ordinal)
            .map(|last_delta| std::ptr::eq(delta, last_delta))
            .unwrap_or(false);

        // Only emit balance change on the last delta for each transaction
        if is_last_delta_for_tx {
            // Skip balance change emission for the start block (initialization only)
            if block.number == start_block {
                continue;
            }

            let tx = block
                .transactions()
                .find(|tx| {
                    tx.calls.iter().any(|call| {
                        call.begin_ordinal <= tx_ordinal && call.end_ordinal >= tx_ordinal
                    })
                })
                .expect("Transaction should exist for ordinal");

            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            // Calculate total pooled ETH with current state
            let deposit_size = BigInt::from(
                num_bigint::BigInt::parse_bytes(DEPOSIT_SIZE_STR.as_bytes(), 10)
                    .expect("Failed to parse DEPOSIT_SIZE_STR as decimal BigInt"),
            );
            let total_pooled_eth = &current_buffered_eth +
                &current_cl_balance_position +
                &(&current_deposited_validators_position - &current_cl_validators_position) *
                    &deposit_size;

            builder.add_balance_change(&BalanceChange {
                token: ETH_ADDRESS.to_vec(),
                balance: total_pooled_eth.to_signed_bytes_be(),
                component_id: ST_ETH_ADDRESS_PROXY_COMPONENT_ID
                    .as_bytes()
                    .to_vec(),
            });
            // The balance of the wstETH component is the same as the stETH one.
            builder.add_balance_change(&BalanceChange {
                token: ST_ETH_ADDRESS_PROXY.to_vec(),
                balance: total_pooled_eth.to_signed_bytes_be(),
                component_id: WST_ETH_ADDRESS_COMPONENT_ID
                    .as_bytes()
                    .to_vec(),
            });
        }
    }
    Ok(())
}

fn get_initial_value_for_block(
    deltas: &[&substreams::pb::substreams::StoreDelta],
    storage_store: &StoreGetBigInt,
    suffix: &str,
) -> BigInt {
    // Find first delta for this key in this block
    if let Some(first_delta) = deltas
        .iter()
        .filter(|d| d.key.ends_with(suffix))
        .min_by_key(|d| d.ordinal)
    {
        // Use old_value from first change (value at start of block)
        if first_delta.old_value.is_empty() {
            BigInt::zero()
        } else {
            // Parse as UTF-8 string, then as BigInt
            let value_str = String::from_utf8(first_delta.old_value.clone())
                .expect("Failed to decode old_value as UTF-8");
            BigInt::from(
                num_bigint::BigInt::parse_bytes(value_str.as_bytes(), 10)
                    .expect("Failed to parse old_value as decimal BigInt"),
            )
        }
    } else {
        // No changes in this block for this variable, use current value
        storage_store
            .get_last(format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:{}", suffix))
            .unwrap_or_default()
    }
}

fn staking_status_and_limit(storage_change: &StorageChange) -> Result<(StakingStatus, BigInt)> {
    let stake_limit_new_hex = hex::encode(storage_change.new_value.clone());
    if stake_limit_new_hex.get(0..24) == Some(ZERO_STAKING_LIMIT) &&
        stake_limit_new_hex.get(32..56) != Some(ZERO_STAKING_LIMIT)
    {
        Ok((StakingStatus::Unlimited, BigInt::zero()))
    } else if stake_limit_new_hex.get(32..56) == Some(ZERO_STAKING_LIMIT) {
        Ok((StakingStatus::Paused, BigInt::zero()))
    } else {
        Ok((
            StakingStatus::Limited,
            BigInt::from(
                num_bigint::BigInt::parse_bytes(
                    stake_limit_new_hex
                        .get(0..24)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Cannot get staking limit from the storage slot")
                        })?
                        .as_bytes(),
                    16,
                )
                .ok_or_else(|| anyhow::anyhow!("Cannot parse BigInt from staking limit"))?,
            ),
        ))
    }
}

// This function assumes that all storage slot values are updated
fn st_eth_entity_changes(call: &Call, builder: &mut TransactionChangesBuilder) -> Result<()> {
    for storage_change in call.storage_changes.iter() {
        if storage_change.key == STORAGE_SLOT_TOTAL_SHARES {
            builder.add_entity_change(&EntityChanges {
                component_id: ST_ETH_ADDRESS_PROXY_COMPONENT_ID.to_owned(),
                attributes: vec![create_entity_change(
                    "total_shares",
                    storage_change.new_value.clone(),
                )],
            });
            // wstETH component needs to track total shares too for simulation
            builder.add_entity_change(&EntityChanges {
                component_id: WST_ETH_ADDRESS_COMPONENT_ID.to_owned(),
                attributes: vec![create_entity_change(
                    "total_shares",
                    storage_change.new_value.clone(),
                )],
            });
        } else if storage_change.key == STORAGE_SLOT_STAKE_LIMIT {
            let (staking_status, staking_limit) = staking_status_and_limit(storage_change)?;

            builder.add_entity_change(&EntityChanges {
                component_id: ST_ETH_ADDRESS_PROXY_COMPONENT_ID.to_owned(),
                attributes: vec![
                    create_entity_change("staking_status", staking_status.as_str_name().into()),
                    create_entity_change("staking_limit", staking_limit.to_signed_bytes_be()),
                ],
            });
        };
    }
    Ok(())
}

fn staking_entity_changes(call: &Call, builder: &mut TransactionChangesBuilder) -> Result<()> {
    for storage_change in call.storage_changes.iter() {
        if storage_change.address == ST_ETH_ADDRESS_PROXY &&
            storage_change.key == STORAGE_SLOT_STAKE_LIMIT
        {
            let (staking_status, staking_limit) = staking_status_and_limit(storage_change)?;
            builder.add_entity_change(&EntityChanges {
                component_id: ST_ETH_ADDRESS_PROXY_COMPONENT_ID.to_owned(),
                attributes: vec![
                    create_entity_change("staking_status", staking_status.as_str_name().into()),
                    create_entity_change("staking_limit", staking_limit.to_signed_bytes_be()),
                ],
            });
        };
    }
    Ok(())
}

fn wst_eth_entity_changes(call: &Call, builder: &mut TransactionChangesBuilder) -> Result<()> {
    for storage_change in call.storage_changes.iter() {
        if storage_change.key == STORAGE_SLOT_WRAPPED_ETH {
            builder.add_entity_change(&EntityChanges {
                component_id: WST_ETH_ADDRESS_COMPONENT_ID.to_owned(),
                attributes: vec![create_entity_change(
                    "total_wstETH",
                    storage_change.new_value.clone(),
                )],
            });
        }
    }
    Ok(())
}

fn create_entity_change(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_owned(), value, change: ChangeType::Update.into() }
}

#[derive(Clone, Copy, Debug)]
pub enum StakingStatus {
    Limited = 0,
    Paused = 1,
    Unlimited = 2,
}

impl StakingStatus {
    pub fn as_str_name(&self) -> &'static str {
        match self {
            StakingStatus::Limited => "Limited",
            StakingStatus::Paused => "Paused",
            StakingStatus::Unlimited => "Unlimited",
        }
    }
}
