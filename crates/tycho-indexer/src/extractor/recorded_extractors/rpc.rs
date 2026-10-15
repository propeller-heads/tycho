//! The RPC services the recorded extractors call. Each answers from its request alone, so a
//! request made again after a restart gets the same answer, as it would from an unchanged chain.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use tycho_common::{
    keccak256,
    models::{
        blockchain::{
            AddressStorageLocation, Block, BlockTag, EntryPointWithTracingParams, TracedEntryPoint,
            TracingResult,
        },
        contract::AccountDelta,
        token::Token,
        Address, BlockHash, Chain, ChangeType, StoreKey,
    },
    traits::{
        MockAccountExtractor, MockEntryPointTracer, StorageSnapshotRequest, TokenOwnerFinding,
        TokenPreProcessor,
    },
    Bytes,
};

use super::streams::{self, FixtureToken};

/// Byte offset of the address in the implementation slot of an entry point target.
const ADDRESS_OFFSET: u8 = 12;

/// Metadata of the tokens the database does not know yet, as the token pre-processor reads it
/// over RPC. Tax and gas are not the database defaults, so a lossy round trip through the
/// database shows up in the token details.
pub(super) struct TokenMetadata;

#[async_trait]
impl TokenPreProcessor for TokenMetadata {
    /// # Panics
    /// If an address is not one of the stream fixture tokens.
    async fn get_tokens(
        &self,
        addresses: Vec<Bytes>,
        _token_finder: Arc<dyn TokenOwnerFinding>,
        _block: BlockTag,
    ) -> Vec<Token> {
        let mut tokens = Vec::with_capacity(addresses.len());
        for address in addresses {
            let fixture: &FixtureToken = streams::TOKENS
                .into_iter()
                .find(|token| token.address() == address)
                .unwrap_or_else(|| panic!("{address} is not a fixture token"));
            tokens.push(Token::new(
                &address,
                fixture.symbol,
                fixture.decimals,
                25,
                &[Some(40_000), None],
                Chain::Ethereum,
                100,
            ));
        }
        tokens
    }
}

/// A tracer whose entry points read two slots of their target, one slot of the contract the
/// target delegates to at the traced block, and the target's WETH balance slot. A new address
/// in the target's implementation slot retriggers the trace. Tracing
/// [`streams::UNTRACEABLE`] always fails.
pub(super) fn tracer() -> MockEntryPointTracer {
    let mut tracer = MockEntryPointTracer::new();
    tracer
        .expect_trace()
        .returning(|block_hash, entry_points| {
            entry_points
                .into_iter()
                .map(|entry_point| {
                    if entry_point.entry_point.target == streams::UNTRACEABLE.target() {
                        return Err(format!("{} reverts", entry_point.entry_point.external_id));
                    }
                    Ok(trace(&block_hash, entry_point))
                })
                .collect()
        });
    tracer
}

fn trace(block_hash: &BlockHash, entry_point: EntryPointWithTracingParams) -> TracedEntryPoint {
    let target = entry_point.entry_point.target.clone();
    let accessed_slots = HashMap::from([
        (target.clone(), HashSet::from([slot(0), implementation_slot()])),
        (delegate(&target, block_hash), HashSet::from([slot(0)])),
        (streams::WETH.address(), HashSet::from([balance_slot(&target)])),
    ]);
    let retriggers = HashSet::from([(
        target,
        AddressStorageLocation::new(implementation_slot(), ADDRESS_OFFSET),
    )]);
    TracedEntryPoint::new(
        entry_point,
        block_hash.clone(),
        TracingResult::new(retriggers, accessed_slots),
    )
}

/// An account extractor that returns every requested account at the block: the requested
/// slots, or two slots for a whole account, with a balance, code and slot values that depend
/// on the account and the block.
pub(super) fn account_extractor() -> MockAccountExtractor {
    let mut extractor = MockAccountExtractor::new();
    extractor
        .expect_get_accounts_at_block()
        .returning(|block, requests| {
            Ok(requests
                .iter()
                .map(|request| (request.address.clone(), account_at(block, request)))
                .collect())
        });
    extractor
}

fn account_at(block: &Block, request: &StorageSnapshotRequest) -> AccountDelta {
    let keys = request
        .slots
        .clone()
        .unwrap_or_else(|| vec![slot(0), slot(1)]);
    let slots = keys
        .into_iter()
        .map(|key| {
            let value =
                keccak256([request.address.as_ref(), &key, &block.number.to_be_bytes()].concat());
            (key, Some(Bytes::from(value)))
        })
        .collect();
    AccountDelta::new(
        Chain::Ethereum,
        request.address.clone(),
        slots,
        Some(Bytes::from(block.number).lpad(32, 0)),
        Some(Bytes::from(keccak256(&request.address))),
        ChangeType::Creation,
    )
}

/// The contract `target` delegates to at the block with `block_hash`.
pub(super) fn delegate(target: &Address, block_hash: &BlockHash) -> Address {
    Address::from(&keccak256([target.as_ref(), block_hash.as_ref()].concat())[12..])
}

/// The slot of the WETH contract that holds `holder`'s balance.
pub(super) fn balance_slot(holder: &Address) -> StoreKey {
    Bytes::from(keccak256(holder))
}

/// The slot an entry point target keeps the address it delegates to in.
pub(super) fn implementation_slot() -> StoreKey {
    slot(0x1d)
}

/// The storage slot at `index`.
pub(super) fn slot(index: u64) -> StoreKey {
    Bytes::from(index).lpad(32, 0)
}

/// `address` as the word of an implementation slot holds it.
pub(super) fn address_word(address: &Address) -> Bytes {
    address.lpad(32, 0)
}
