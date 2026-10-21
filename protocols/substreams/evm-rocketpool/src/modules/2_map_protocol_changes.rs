use crate::{
    abi::{
        rocket_dao_protocol_proposal, rocket_deposit_pool, rocket_network_balances,
        rocket_token_reth,
    },
    constants::{
        ALL_STORAGE_SLOTS, DEPOSITS_ENABLED_SLOT, DEPOSIT_ASSIGN_ENABLED_SLOT,
        DEPOSIT_ASSIGN_MAXIMUM_SLOT, DEPOSIT_ASSIGN_SOCIALISED_MAXIMUM_SLOT, DEPOSIT_FEE_SLOT,
        ETH_ADDRESS, MAX_DEPOSIT_POOL_SIZE_SLOT, MEGAPOOL_QUEUE_REQUESTED_TOTAL_SLOT,
        MIN_DEPOSIT_AMOUNT_SLOT, RETH_ADDRESS, ROCKET_DAO_PROTOCOL_PROPOSAL_ADDRESS,
        ROCKET_DEPOSIT_POOL_ADDRESS, ROCKET_DEPOSIT_POOL_ETH_BALANCE_SLOT,
        ROCKET_NETWORK_BALANCES_ADDRESS, ROCKET_POOL_COMPONENT_ID, ROCKET_STORAGE_ADDRESS,
        ROCKET_VAULT_ADDRESS, TARGET_RETH_COLLATERAL_RATE_SLOT,
    },
    utils::{get_changed_attributes, hex_to_bytes},
};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams_ethereum::{
    pb::eth::{self, v2::TransactionTrace},
    Event,
};
use tycho_substreams::{
    models::{
        Attribute, BlockChanges, BlockTransactionProtocolComponents, ChangeType, EntityChanges,
        TransactionChangesBuilder,
    },
    prelude::BalanceChange,
};

/// Aggregates protocol component, balance and attribute changes by transaction.
///
/// Indexes the RocketPool deposit pool from the v1.4 activation block onwards.
#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    protocol_components: BlockTransactionProtocolComponents,
) -> Result<BlockChanges> {
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // On the starting block, initialize the component with provided initial state values
    // that represent the state at the END of the starting block. We skip update handlers on
    // this block because the initial params already capture the final state.
    let component_created = !protocol_components
        .tx_components
        .is_empty();

    if component_created {
        initialize_protocol_component(&params, protocol_components, &mut transaction_changes)?;
    } else {
        update_deposit_liquidity(&block, &mut transaction_changes);
        update_reth_liquidity(&block, &mut transaction_changes);
        update_network_balance(&block, &mut transaction_changes);
        update_protocol_settings(&block, &mut transaction_changes);
        update_megapool_queue_state(&block, &mut transaction_changes);
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

/// Initializes the protocol component with initial state values.
///
/// Called only once on the starting block. Adds the protocol component, initial attributes
/// (with ChangeType::Creation), and initial ETH balance.
fn initialize_protocol_component(
    params: &str,
    protocol_components: BlockTransactionProtocolComponents,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    let initial_state: HashMap<String, String> = serde_json::from_str(params)
        .map_err(|e| anyhow::anyhow!("Failed to parse initial state: {}", e))?;

    let tx_component = protocol_components
        .tx_components
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No transaction component found"))?;

    let tx = tx_component
        .tx
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Transaction missing in protocol components"))?;

    let component = tx_component
        .components
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No component found in transaction"))?;

    let builder = transaction_changes
        .entry(tx.index)
        .or_insert_with(|| TransactionChangesBuilder::new(tx));

    builder.add_protocol_component(&component);

    builder.add_entity_change(&EntityChanges {
        component_id: ROCKET_POOL_COMPONENT_ID.to_owned(),
        attributes: build_initial_attributes(&initial_state)?,
    });

    builder.add_balance_change(&BalanceChange {
        token: ETH_ADDRESS.to_vec(),
        balance: get_initial_eth_balance(&initial_state)?,
        component_id: ROCKET_POOL_COMPONENT_ID
            .as_bytes()
            .to_vec(),
    });

    Ok(())
}

/// Updates deposit contract liquidity based on deposit pool events.
///
/// Listens for deposit-related events from the RocketDepositPool and fetches the updated
/// ETH balance from RocketVault's etherBalances storage slot. Also listens for FundsAssigned
/// events which change the vault balance when megapool queue entries are processed.
fn update_deposit_liquidity(
    block: &eth::v2::Block,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for log in block.logs() {
        if log.log.address != ROCKET_DEPOSIT_POOL_ADDRESS {
            continue;
        }

        let is_deposit_event = rocket_deposit_pool::events::DepositReceived::match_log(log.log) ||
            rocket_deposit_pool::events::DepositAssigned::match_log(log.log) ||
            rocket_deposit_pool::events::DepositRecycled::match_log(log.log) ||
            rocket_deposit_pool::events::ExcessWithdrawn::match_log(log.log) ||
            rocket_deposit_pool::events::FundsAssigned::match_log(log.log);

        if !is_deposit_event {
            continue;
        }

        let tx = log.receipt.transaction;

        // Extract the updated liquidity from RocketVault's etherBalances storage
        let attributes = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .filter(|call| call.address == ROCKET_VAULT_ADDRESS)
            .flat_map(|call| {
                get_changed_attributes(
                    &call.storage_changes,
                    &[ROCKET_DEPOSIT_POOL_ETH_BALANCE_SLOT],
                )
            })
            .collect::<Vec<_>>();

        add_entity_change_if_needed(attributes, tx, transaction_changes);
    }
}

/// Updates rETH contract liquidity based on rETH events.
///
/// Listens for EtherDeposited and TokensBurned events from the RocketTokenRETH contract and
/// fetches the updated native ETH balance from the transaction's balance changes.
///
/// The reason we do not use the event parameters directly is that they only contain the delta
/// change, and would force us to start indexing from the token creation.
fn update_reth_liquidity(
    block: &eth::v2::Block,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for log in block.logs() {
        if log.log.address != RETH_ADDRESS {
            continue;
        }

        let is_eth_event = rocket_token_reth::events::EtherDeposited::match_log(log.log) ||
            rocket_token_reth::events::TokensBurned::match_log(log.log);

        if !is_eth_event {
            continue;
        }

        let tx = log.receipt.transaction;

        // Extract the updated ETH balance from the transaction's balance changes
        let reth_balance = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.balance_changes.iter())
            .filter(|bc| bc.address == RETH_ADDRESS)
            .filter_map(|bc| bc.new_value.as_ref())
            .next_back();

        if let Some(reth_balance) = reth_balance {
            let attributes = vec![Attribute {
                name: "reth_contract_liquidity".to_string(),
                value: reth_balance.bytes.to_vec(),
                change: ChangeType::Update.into(),
            }];

            add_entity_change_if_needed(attributes, tx, transaction_changes);
        }
    }
}

/// Extracts Rocket Pool network balance updates from the block logs.
///
/// Uses the RocketNetworkBalances contract. The BalancesUpdated event provides total_eth
/// and reth_supply which are used for the rETH exchange rate calculation.
fn update_network_balance(
    block: &eth::v2::Block,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for log in block.logs() {
        if log.log.address != ROCKET_NETWORK_BALANCES_ADDRESS {
            continue;
        }

        let balance_update =
            rocket_network_balances::events::BalancesUpdated::match_and_decode(log)
                .map(|event| (event.total_eth, event.reth_supply));

        let (total_eth, reth_supply) = match balance_update {
            Some(values) => values,
            None => continue,
        };

        let tx = log.receipt.transaction;

        let builder = transaction_changes
            .entry(tx.index as u64)
            .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

        let eth_bc = BalanceChange {
            token: ETH_ADDRESS.to_vec(),
            balance: total_eth.to_signed_bytes_be(),
            component_id: ROCKET_POOL_COMPONENT_ID
                .as_bytes()
                .to_vec(),
        };

        let attributes = vec![
            Attribute {
                name: "reth_supply".to_string(),
                value: reth_supply.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            },
            Attribute {
                name: "total_eth".to_string(),
                value: total_eth.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            },
        ];

        builder.add_balance_change(&eth_bc);
        builder.add_entity_change(&EntityChanges {
            component_id: ROCKET_POOL_COMPONENT_ID.to_owned(),
            attributes,
        });
    }
}

/// Extracts protocol settings updates from the block logs.
///
/// Note: that the protocol settings updates can only be triggered by executing DAO proposals, hence
/// it is sufficient to first check for the `ProposalExecuted` event and only then check the
/// associated storage changes.
fn update_protocol_settings(
    block: &eth::v2::Block,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for log in block.logs() {
        if !(log.log.address == ROCKET_DAO_PROTOCOL_PROPOSAL_ADDRESS &&
            rocket_dao_protocol_proposal::events::ProposalExecuted::match_log(log.log))
        {
            continue;
        }

        let tx = log.receipt.transaction;

        let attributes = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .filter(|call| call.address == ROCKET_STORAGE_ADDRESS)
            .flat_map(|call| {
                get_changed_attributes(
                    &call.storage_changes,
                    &[
                        DEPOSITS_ENABLED_SLOT,
                        DEPOSIT_ASSIGN_ENABLED_SLOT,
                        DEPOSIT_ASSIGN_MAXIMUM_SLOT,
                        DEPOSIT_ASSIGN_SOCIALISED_MAXIMUM_SLOT,
                        MIN_DEPOSIT_AMOUNT_SLOT,
                        MAX_DEPOSIT_POOL_SIZE_SLOT,
                        DEPOSIT_FEE_SLOT,
                        TARGET_RETH_COLLATERAL_RATE_SLOT,
                    ],
                )
            })
            .collect::<Vec<_>>();

        add_entity_change_if_needed(attributes, tx, transaction_changes);
    }
}

/// Updates megapool queue state based on queue events.
///
/// Listens for FundsRequested, FundsAssigned, and QueueExited events from the
/// RocketDepositPool, then extracts the updated megapool_queue_requested_total
/// from RocketStorage storage changes.
fn update_megapool_queue_state(
    block: &eth::v2::Block,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for log in block.logs() {
        if log.log.address != ROCKET_DEPOSIT_POOL_ADDRESS {
            continue;
        }

        let is_queue_event = rocket_deposit_pool::events::FundsRequested::match_log(log.log) ||
            rocket_deposit_pool::events::FundsAssigned::match_log(log.log) ||
            rocket_deposit_pool::events::QueueExited::match_log(log.log);

        if !is_queue_event {
            continue;
        }

        let tx = log.receipt.transaction;

        let attributes = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .filter(|call| call.address == ROCKET_STORAGE_ADDRESS)
            .flat_map(|call| {
                get_changed_attributes(
                    &call.storage_changes,
                    &[MEGAPOOL_QUEUE_REQUESTED_TOTAL_SLOT],
                )
            })
            .collect::<Vec<_>>();

        add_entity_change_if_needed(attributes, tx, transaction_changes);
    }
}

/// Helper to add entity changes for a transaction if attributes are non-empty.
fn add_entity_change_if_needed(
    attributes: Vec<Attribute>,
    tx: &TransactionTrace,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    if !attributes.is_empty() {
        let builder = transaction_changes
            .entry(tx.index as u64)
            .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

        builder.add_entity_change(&EntityChanges {
            component_id: ROCKET_POOL_COMPONENT_ID.to_owned(),
            attributes,
        });
    }
}

/// Build initial attributes from parsed state, validating all required attributes exist.
fn build_initial_attributes(state: &HashMap<String, String>) -> Result<Vec<Attribute>> {
    ALL_STORAGE_SLOTS
        .iter()
        .map(|loc| loc.name)
        .chain(["total_eth", "reth_supply", "reth_contract_liquidity"])
        .map(|name| {
            let value = state
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Missing initial attribute: {}", name))?;
            Ok(Attribute {
                name: name.to_string(),
                value: hex_to_bytes(value)?,
                change: ChangeType::Creation.into(),
            })
        })
        .collect()
}

/// Get the initial ETH balance from parsed state.
fn get_initial_eth_balance(state: &HashMap<String, String>) -> Result<Vec<u8>> {
    hex_to_bytes(
        state
            .get("total_eth")
            .ok_or_else(|| anyhow::anyhow!("Missing initial attribute: total_eth"))?,
    )
}
