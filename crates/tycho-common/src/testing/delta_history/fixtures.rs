//! Hand-built blocks, messages and histories for the delta history tests.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, DCIUpdate, EntryPoint, TracingResult},
        contract::AccountBalance,
        protocol::{ComponentBalance, ProtocolComponent},
        Address, BlockHash, ChangeType,
    },
    testing::delta_history::{
        DeltaStateModel, History, HistoryConfig, HistoryEvent, RestartKind, CHAIN, EXTRACTOR,
    },
    Bytes,
};

/// An address of 20 copies of `n`.
pub(super) fn byte_address(n: u8) -> Address {
    Bytes::from(vec![n; 20])
}

pub(super) fn hash(number: u64, branch: u8) -> BlockHash {
    let mut bytes = vec![branch];
    bytes.extend_from_slice(&number.to_be_bytes());
    Bytes::from(bytes)
}

pub(super) fn block(number: u64, branch: u8, parent_branch: u8) -> Block {
    Block::new(
        number,
        CHAIN,
        hash(number, branch),
        hash(number.saturating_sub(1), parent_branch),
        Default::default(),
    )
}

pub(super) fn full_block(
    number: u64,
    branch: u8,
    parent_branch: u8,
) -> Arc<BlockAggregatedChanges> {
    Arc::new(BlockAggregatedChanges {
        extractor: EXTRACTOR.to_string(),
        chain: CHAIN,
        block: block(number, branch, parent_branch),
        ..Default::default()
    })
}

/// A full block on branch 0 that carries the given watermarks.
pub(super) fn block_with_watermarks(
    number: u64,
    finalized: u64,
    committed: Option<u64>,
) -> Arc<BlockAggregatedChanges> {
    Arc::new(BlockAggregatedChanges {
        finalized_block_height: finalized,
        db_committed_block_height: committed,
        ..(*full_block(number, 0, 0)).clone()
    })
}

/// A full block on branch 0 whose watermarks make every block at or below `number`
/// finalized and committed.
pub(super) fn committed_block(number: u64) -> Arc<BlockAggregatedChanges> {
    block_with_watermarks(number, number, Some(number))
}

pub(super) fn revert_to(number: u64, branch: u8, parent_branch: u8) -> Arc<BlockAggregatedChanges> {
    Arc::new(BlockAggregatedChanges {
        revert: true,
        ..(*full_block(number, branch, parent_branch)).clone()
    })
}

/// A full block that links `entrypoint` to `component` and traces it to `contract`.
pub(super) fn block_with_dci(
    number: u64,
    branch: u8,
    parent_branch: u8,
    component: &str,
    entrypoint: &str,
    contract: Address,
) -> Arc<BlockAggregatedChanges> {
    let linked = EntryPoint::new(entrypoint.to_string(), contract.clone(), "f()".to_string());
    let traced = TracingResult::new(HashSet::new(), HashMap::from([(contract, HashSet::new())]));
    Arc::new(BlockAggregatedChanges {
        dci_update: DCIUpdate {
            new_entrypoints: HashMap::from([(component.to_string(), HashSet::from([linked]))]),
            new_entrypoint_params: HashMap::new(),
            trace_results: HashMap::from([(entrypoint.to_string(), traced)]),
        },
        ..(*full_block(number, branch, parent_branch)).clone()
    })
}

pub(super) fn messages(blocks: &[Arc<BlockAggregatedChanges>]) -> Vec<HistoryEvent> {
    blocks
        .iter()
        .cloned()
        .map(HistoryEvent::Message)
        .collect()
}

pub(super) fn restarted(
    last_committed: &Arc<BlockAggregatedChanges>,
    kind: RestartKind,
) -> HistoryEvent {
    HistoryEvent::ExtractorRestarted { last_committed: last_committed.block.clone(), kind }
}

/// A hand-built history for [`catch_up_spec`], which reads only the events and the config.
pub(super) fn history(
    events: Vec<HistoryEvent>,
    canonical: Vec<Arc<BlockAggregatedChanges>>,
    window_depth: u64,
) -> History {
    History {
        canonical_states: vec![DeltaStateModel::default(); events.len()],
        connected_states: vec![DeltaStateModel::default(); events.len()],
        events,
        canonical,
        config: HistoryConfig { window_depth, ..Default::default() },
    }
}

pub(super) fn component(id: &str, change: ChangeType) -> ProtocolComponent {
    ProtocolComponent { id: id.to_string(), change, ..Default::default() }
}

pub(super) fn component_balance(id: &str, token: &Address, balance: u8) -> ComponentBalance {
    ComponentBalance::new(token.clone(), Bytes::from(vec![balance]), 0.0, Bytes::new(), id)
}

pub(super) fn account_balance(account: &Address, token: &Address, balance: u8) -> AccountBalance {
    AccountBalance::new(account.clone(), token.clone(), Bytes::from(vec![balance]), Bytes::new())
}

pub(super) fn one(byte: u8) -> Bytes {
    Bytes::from(vec![byte])
}
