use crate::{
    abi::dynamic_swap_fee_module::events::{
        CustomFeeSet, DynamicFeeReset, FeeCapSet, ScalingFactorSet,
    },
    events::get_log_changed_attributes,
    modules::utils::Params,
    pb::tycho::evm::aerodrome::Pool,
};

use itertools::Itertools;
use num_bigint::BigInt;
use std::{collections::HashMap, vec};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::{
    pb::eth::v2::{self as eth},
    Event,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    protocol_components: BlockChanges,
    pools_store: StoreGetProto<Pool>,
    balance_store: StoreDeltas,
    balance_deltas: BlockBalanceDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let dynamic_fee_modules = params
        .dynamic_fee_modules
        .iter()
        .map(|f| hex::decode(f).expect("Invalid dynamic_fee_module hex"))
        .collect::<Vec<Vec<u8>>>();
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    for change in protocol_components.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        change
            .component_changes
            .iter()
            .for_each(|c| {
                builder.add_protocol_component(c);
            });
        change
            .entity_changes
            .iter()
            .for_each(|c| {
                builder.add_entity_change(c);
            });
    }

    aggregate_balances_changes(balance_store, balance_deltas)
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

    for trx in block.transactions() {
        let tx = Transaction {
            to: trx.to.clone(),
            from: trx.from.clone(),
            hash: trx.hash.clone(),
            index: trx.index.into(),
        };
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));

        for (log, call_view) in trx.logs_with_calls() {
            if let Some(pool) = pools_store.get_last(format!("{}:{}", "Pool", log.address.to_hex()))
            {
                let changed_attributes = get_log_changed_attributes(
                    log,
                    &call_view.call.storage_changes,
                    pool.address
                        .clone()
                        .as_slice()
                        .try_into()
                        .expect("Pool address is not 20 bytes long"),
                );
                if !changed_attributes.is_empty() {
                    builder.add_entity_change(&EntityChanges {
                        component_id: pool.address.clone().to_hex(),
                        attributes: changed_attributes,
                    });
                }
            }
            if dynamic_fee_modules.contains(&log.address) {
                let mut handle_event = |pool: &Vec<u8>, attrs: Vec<Attribute>| {
                    let pool_key = format!("Pool:{}", pool.to_hex());
                    if pools_store
                        .get_last(&pool_key)
                        .is_some()
                    {
                        builder.add_entity_change(&EntityChanges {
                            component_id: pool.to_hex(),
                            attributes: attrs,
                        });
                    }
                };
                if let Some(e) = CustomFeeSet::match_and_decode(log) {
                    handle_event(
                        &e.pool.clone(),
                        vec![Attribute {
                            name: "dfc_baseFee".into(),
                            value: e.fee.to_signed_bytes_be(),
                            change: ChangeType::Update.into(),
                        }],
                    );
                } else if let Some(e) = ScalingFactorSet::match_and_decode(log) {
                    handle_event(
                        &e.pool.clone(),
                        vec![Attribute {
                            name: "dfc_scalingFactor".into(),
                            value: e.scaling_factor.to_signed_bytes_be(),
                            change: ChangeType::Update.into(),
                        }],
                    );
                } else if let Some(e) = FeeCapSet::match_and_decode(log) {
                    handle_event(
                        &e.pool.clone(),
                        vec![Attribute {
                            name: "dfc_feeCap".into(),
                            value: e.fee_cap.to_signed_bytes_be(),
                            change: ChangeType::Update.into(),
                        }],
                    );
                } else if let Some(e) = DynamicFeeReset::match_and_decode(log) {
                    handle_event(
                        &e.pool.clone(),
                        vec![
                            Attribute {
                                name: "dfc_scalingFactor".into(),
                                value: BigInt::from(0).to_signed_bytes_be(),
                                change: ChangeType::Update.into(),
                            },
                            Attribute {
                                name: "dfc_feeCap".into(),
                                value: BigInt::from(0).to_signed_bytes_be(),
                                change: ChangeType::Update.into(),
                            },
                        ],
                    );
                }
            }
        }
    }

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
