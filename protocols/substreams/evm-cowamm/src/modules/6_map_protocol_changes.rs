use crate::pb::cowamm::{BlockPoolChanges, CowPool};
use anyhow::Result;
use itertools::Itertools;
use std::{collections::HashMap, str::FromStr};
use substreams::{
    key,
    pb::substreams::{StoreDelta, StoreDeltas},
    prelude::{BigInt, StoreGetProto},
    store::StoreGet,
};
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

#[substreams::handlers::map]
fn map_protocol_changes(
    block: Block,
    block_pool_changes: BlockPoolChanges,
    pool_store: StoreGetProto<CowPool>,
    balance_store: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let protocol_components = block_pool_changes
        .tx_protocol_components
        .expect("no tx components");
    let balance_deltas = block_pool_changes
        .block_balance_deltas
        .expect("no block balance deltas")
        .balance_deltas
        .into_iter()
        .map(Into::into)
        .collect::<Vec<BalanceDelta>>();

    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    // Aggregate newly created components per tx
    protocol_components
        .tx_components
        .into_iter()
        .for_each(|tx_component| {
            let tx = tx_component.tx.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&(&tx).into()));

            // iterate over individual components created within this tx
            tx_component
                .components
                .iter()
                .for_each(|component| {
                    builder.add_protocol_component(&component.into());
                });
        });
    // Register the Pool liquidities for token_a , token_b and lp_token_supply as Entity Changes
    balance_store
        .clone()
        .deltas
        .into_iter()
        .zip(balance_deltas.clone())
        .for_each(|(store_delta, balance_delta)| {
            let tx = balance_delta.tx.clone().unwrap();
            let new_value_bigint =
                BigInt::from_str(&String::from_utf8(store_delta.new_value).unwrap()).unwrap();

            let address = String::from_utf8(balance_delta.component_id).unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            let pool = pool_store.must_get_last(format!("Pool:{}", address));

            let mut attr_vec: Vec<Attribute> = Vec::new();
            match balance_delta.token {
                t if t == pool.token_a => {
                    attr_vec.push(Attribute {
                        name: "liquidity_a".to_string(),
                        value: new_value_bigint.to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    });
                }
                t if t == pool.token_b => {
                    attr_vec.push(Attribute {
                        name: "liquidity_b".to_string(),
                        value: new_value_bigint.to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    });
                }
                t if t == pool.lp_token => {
                    attr_vec.push(Attribute {
                        name: "lp_token_supply".to_string(),
                        value: new_value_bigint.to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    });
                }
                _ => panic!("unknown token balance delta"), // not even possible
            }
            builder.add_entity_change(&EntityChanges {
                component_id: address.clone(),
                attributes: attr_vec,
            });
        });

    // Aggregate absolute balances per transaction.

    // We do not want to create balance changes (that is BalanceChange objects) for the changes in
    // the lp_token_supply, because its change does not reflect a change to the TVL of the
    // component, the purpose of creating balance_deltas was for the lp_token_supply entity
    // changes.

    // So we create new vecs with the balance_deltas and balance_store_deltas
    // of the lp_token_supply filtered out

    //Remember that the lp_token_address is the same as the pool address (which is the
    // component_id)
    //we add the bind balance entity changes in the same txn as the lp token supply changes
    let store_delta_vec = balance_store
        .deltas
        .into_iter()
        .filter(|store_delta| {
            let component_id = key::segment_at(&store_delta.key, 0).trim_start_matches("0x"); //key is component_id/pool_address + token
            let token_address = key::segment_at(&store_delta.key, 1);
            let formatted_token_address = token_address.to_string();

            component_id != formatted_token_address
        })
        .collect::<Vec<StoreDelta>>();

    let new_balance_store = StoreDeltas { deltas: store_delta_vec };

    let balance_deltas_vec = balance_deltas
        .into_iter()
        .filter(|balance_delta| {
            let delta = String::from_utf8(balance_delta.component_id.clone()).unwrap();
            let address = delta.to_string();
            balance_delta.token.to_hex() != address
        })
        .collect::<Vec<BalanceDelta>>();

    let new_balance_deltas = BlockBalanceDeltas { balance_deltas: balance_deltas_vec };

    aggregate_balances_changes(new_balance_store, new_balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _): &(u64, TransactionChangesBuilder)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
    })
}
