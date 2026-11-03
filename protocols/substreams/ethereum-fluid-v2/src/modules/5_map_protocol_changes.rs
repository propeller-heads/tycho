use crate::{
    events,
    modules::utils::{self, Params},
    pb::tycho::evm::fluid_v2::Pool,
};
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::{
    balances::aggregate_balances_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

/// `getTokenBaseState(address)` on the DEX v2 resolver.
const GET_TOKEN_BASE_STATE_SIGNATURE: &str = "getTokenBaseState(address)";
const GET_TOKEN_BASE_STATE_SELECTOR: [u8; 4] = [0x70, 0x56, 0xa7, 0x94];

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
    let dex_v2_address = params.dex_v2_address()?;
    let resolver_address = params.resolver_address()?;

    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    for change in protocol_components.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &change.component_changes {
            let is_creation_tx = pools_store
                .get_last(utils::pool_store_key_for_component(&component.id))
                .is_some_and(|pool| pool.created_tx_hash == tx.hash);
            if !is_creation_tx {
                continue;
            }
            builder.add_protocol_component(component);
            add_token_base_state_entrypoints(builder, component, &resolver_address);
        }
        for entity_change in &change.entity_changes {
            builder.add_entity_change(entity_change);
        }
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
        let tx_index: u64 = trx.index.into();
        for (log, call_view) in trx.logs_with_calls() {
            if log.address != dex_v2_address {
                continue;
            }
            let Some((component_id, attributes)) = events::get_log_changed_attributes(
                log,
                &call_view.call.storage_changes,
                &dex_v2_address,
            ) else {
                continue;
            };
            // Events of pools that are not indexed (D1/D2) carry no state for Tycho.
            if pools_store
                .get_last(utils::pool_store_key_for_component(&component_id))
                .is_none()
            {
                continue;
            }
            let builder = transaction_changes
                .entry(tx_index)
                .or_insert_with(|| {
                    TransactionChangesBuilder::new(&Transaction {
                        to: trx.to.clone(),
                        from: trx.from.clone(),
                        hash: trx.hash.clone(),
                        index: tx_index,
                    })
                });
            builder.add_entity_change(&EntityChanges { component_id, attributes });
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

/// Registers `getTokenBaseState` for both pool tokens with the Dynamic Contract Indexer.
///
/// What a swap can pay out and the Liquidity exchange prices the pool math runs on live in the dex
/// base state and in Fluid Liquidity, not in the pool. Tracing the resolver call makes Tycho index
/// every contract and slot it reads, so the simulation can run the same call.
fn add_token_base_state_entrypoints(
    builder: &mut TransactionChangesBuilder,
    component: &ProtocolComponent,
    resolver_address: &[u8],
) {
    for token in &component.tokens {
        let mut calldata = GET_TOKEN_BASE_STATE_SELECTOR.to_vec();
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(&utils::to_fluid_token(token));
        let (entrypoint, entrypoint_params) = create_entrypoint(
            resolver_address.to_vec(),
            GET_TOKEN_BASE_STATE_SIGNATURE.to_string(),
            component.id.clone(),
            TraceData::Rpc(RpcTraceData { caller: None, calldata }),
        );
        builder.add_entrypoint(&entrypoint);
        builder.add_entrypoint_params(&entrypoint_params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_keccak::{Hasher, Keccak};

    #[test]
    fn selector_matches_signature() {
        let mut hasher = Keccak::v256();
        let mut hash = [0u8; 32];
        hasher.update(GET_TOKEN_BASE_STATE_SIGNATURE.as_bytes());
        hasher.finalize(&mut hash);

        assert_eq!(hash[..4], GET_TOKEN_BASE_STATE_SELECTOR);
    }
}
