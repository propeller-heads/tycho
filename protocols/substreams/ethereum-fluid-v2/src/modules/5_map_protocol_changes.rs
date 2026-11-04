use crate::{
    events,
    modules::{
        utils::{self, Params},
        POOL_IDS_KEY,
    },
    pb::tycho::evm::fluid_v2::Pool,
};
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::{
    balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

/// `getTokenBaseState(address)` on the DEX v2 resolver.
const GET_TOKEN_BASE_STATE_SIGNATURE: &str = "getTokenBaseState(address)";
const GET_TOKEN_BASE_STATE_SELECTOR: [u8; 4] = [0x70, 0x56, 0xa7, 0x94];

/// `readFromStorage(bytes32)` on Fluid Liquidity, read for slot 1: `_status`, which
/// `changeStatus(2)` sets to pause every Liquidity operation.
const READ_FROM_STORAGE_SIGNATURE: &str = "readFromStorage(bytes32)";
const READ_FROM_STORAGE_SELECTOR: [u8; 4] = [0xb5, 0xc7, 0x36, 0xe4];
const LIQUIDITY_STATUS_SLOT: [u8; 32] = {
    let mut slot = [0u8; 32];
    slot[31] = 1;
    slot
};

/// EIP-1967 implementation slot of the DEX v2 proxy.
const IMPLEMENTATION_SLOT: [u8; 32] = [
    0x36, 0x08, 0x94, 0xa1, 0x3b, 0xa1, 0xa3, 0x21, 0x06, 0x67, 0xc8, 0x28, 0x49, 0x2d, 0xb9, 0x8d,
    0xca, 0x3e, 0x20, 0x76, 0xcc, 0x37, 0x35, 0xa9, 0x20, 0xa3, 0xca, 0x50, 0x5d, 0x38, 0x2b, 0xbc,
];

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    protocol_components: BlockChanges,
    pools_store: StoreGetProto<Pool>,
    balance_store: StoreDeltas,
    balance_deltas: BlockBalanceDeltas,
    pool_ids_store: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let dex_v2_address = params.dex_v2_address()?;
    let dex_v2_implementation = params.dex_v2_implementation()?;
    let resolver_address = params.resolver_address()?;
    let liquidity_address = params.liquidity_address()?;

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
            add_liquidity_status_entrypoint(builder, component, &liquidity_address);
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

    for tx in upgrading_transactions(&block, &dex_v2_address, &dex_v2_implementation) {
        let builder = transaction_changes
            .entry(tx.index.into())
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));
        for component_id in pool_ids(&pool_ids_store) {
            builder.change_component_pause_state(&component_id, true);
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        // The indexer keeps the contracts traced from the resolver entrypoints up to date from
        // these, and only for contracts it tracks.
        storage_changes: get_block_storage_changes(&block),
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

/// Registers Fluid Liquidity's `_status` with the Dynamic Contract Indexer, so the simulation can
/// tell when Liquidity is paused as a whole, which no resolver call reports.
fn add_liquidity_status_entrypoint(
    builder: &mut TransactionChangesBuilder,
    component: &ProtocolComponent,
    liquidity_address: &[u8],
) {
    let mut calldata = READ_FROM_STORAGE_SELECTOR.to_vec();
    calldata.extend_from_slice(&LIQUIDITY_STATUS_SLOT);
    let (entrypoint, entrypoint_params) = create_entrypoint(
        liquidity_address.to_vec(),
        READ_FROM_STORAGE_SIGNATURE.to_string(),
        component.id.clone(),
        TraceData::Rpc(RpcTraceData { caller: None, calldata }),
    );
    builder.add_entrypoint(&entrypoint);
    builder.add_entrypoint_params(&entrypoint_params);
}

/// The transactions that leave the DEX v2 proxy behind an implementation other than the one this
/// package and the simulation mirror.
///
/// An upgrade can replace the swap and position modules, and the slot and event layout the
/// package decodes, so every pool is paused from that transaction on, until the new implementation
/// is verified and recorded in the params. The live implementation emits no `Upgraded` event, so
/// the write to the EIP-1967 slot is what reveals an upgrade.
fn upgrading_transactions<'a>(
    block: &'a eth::Block,
    dex_v2_address: &[u8],
    dex_v2_implementation: &[u8],
) -> Vec<&'a eth::TransactionTrace> {
    let mut upgrading = Vec::new();
    for tx in block.transactions() {
        let last_write = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.address == dex_v2_address && change.key == IMPLEMENTATION_SLOT)
            .max_by_key(|change| change.ordinal);
        if let Some(change) = last_write {
            if change.new_value[12..] != *dex_v2_implementation {
                upgrading.push(tx);
            }
        }
    }
    upgrading
}

/// Every indexed pool's component id.
fn pool_ids(pool_ids_store: &StoreGetString) -> Vec<String> {
    pool_ids_store
        .get_last(POOL_IDS_KEY)
        .map(|joined| {
            joined
                .split(';')
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
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

    #[test]
    fn read_from_storage_selector_matches_signature() {
        let mut hasher = Keccak::v256();
        let mut hash = [0u8; 32];
        hasher.update(READ_FROM_STORAGE_SIGNATURE.as_bytes());
        hasher.finalize(&mut hash);

        assert_eq!(hash[..4], READ_FROM_STORAGE_SELECTOR);
    }

    #[test]
    fn implementation_slot_is_eip1967() {
        let mut hasher = Keccak::v256();
        let mut hash = [0u8; 32];
        hasher.update(b"eip1967.proxy.implementation");
        hasher.finalize(&mut hash);
        // The slot is the hash minus one, big-endian.
        for byte in hash.iter_mut().rev() {
            let borrowed = *byte == 0;
            *byte = byte.wrapping_sub(1);
            if !borrowed {
                break;
            }
        }

        assert_eq!(hash, IMPLEMENTATION_SLOT);
    }

    const DEX: [u8; 20] = [0xab; 20];
    const RECORDED: [u8; 20] = [0x81; 20];
    const OTHER: [u8; 20] = [0x99; 20];

    fn implementation_write(
        address: [u8; 20],
        implementation: [u8; 20],
        ordinal: u64,
    ) -> eth::StorageChange {
        let mut new_value = vec![0u8; 12];
        new_value.extend_from_slice(&implementation);
        eth::StorageChange {
            address: address.to_vec(),
            key: IMPLEMENTATION_SLOT.to_vec(),
            new_value,
            ordinal,
            ..Default::default()
        }
    }

    fn block_with_call(
        storage_changes: Vec<eth::StorageChange>,
        state_reverted: bool,
    ) -> eth::Block {
        eth::Block {
            transaction_traces: vec![eth::TransactionTrace {
                index: 7,
                status: eth::TransactionTraceStatus::Succeeded.into(),
                calls: vec![eth::Call { storage_changes, state_reverted, ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn write_of_another_implementation_is_an_upgrade() {
        let block = block_with_call(vec![implementation_write(DEX, OTHER, 1)], false);

        let upgrading = upgrading_transactions(&block, &DEX, &RECORDED);

        assert_eq!(upgrading.len(), 1);
        assert_eq!(upgrading[0].index, 7);
    }

    #[test]
    fn last_write_decides_the_implementation() {
        let block = block_with_call(
            vec![implementation_write(DEX, RECORDED, 2), implementation_write(DEX, OTHER, 1)],
            false,
        );

        assert!(upgrading_transactions(&block, &DEX, &RECORDED).is_empty());
    }

    #[test]
    fn reverted_or_foreign_writes_are_no_upgrade() {
        let reverted = block_with_call(vec![implementation_write(DEX, OTHER, 1)], true);
        let foreign = block_with_call(vec![implementation_write([0x01; 20], OTHER, 1)], false);

        assert!(upgrading_transactions(&reverted, &DEX, &RECORDED).is_empty());
        assert!(upgrading_transactions(&foreign, &DEX, &RECORDED).is_empty());
    }
}
