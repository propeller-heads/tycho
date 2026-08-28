use std::collections::{HashMap, HashSet};

/// Per-address init-write evidence collected within one transaction: EIP-1967 impl slot
/// initialized, the base token written to the token slot, and whether the quote slot carries
/// the configured hub token.
#[derive(Default)]
struct InitWrites {
    has_impl: bool,
    base_token: Option<Vec<u8>>,
    has_quote: bool,
}

use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::{attributes::json_serialize_address_list, prelude::*};

use crate::{
    common::{address_from_word, component_id, is_zero, slot_key, EIP1967_IMPL_SLOT},
    config::DeploymentConfig,
};

/// Discovers books from the creation of their per-book price store.
///
/// Tessera emits no creation event. A book is created by an owner-Safe → engine admin call
/// that internally CREATEs the store proxy; in that same transaction the store's init writes
/// its EIP-1967 implementation slot and its identity slots (base token, quote token), and the
/// engine writes its token→store mapping entry (value = the store address). A store address is
/// recognized as a new book when, within one non-reverted transaction:
///
/// * its EIP-1967 implementation slot goes zero → non-zero,
/// * its base-token slot goes zero → non-zero,
/// * its quote-token slot is written with the configured hub token (USDC) in the low bytes,
/// * and the engine writes some slot whose value is that store address (the mapping entry) —
///   this last check ties the generic proxy-init shape to this specific venue.
#[substreams::handlers::map]
pub fn map_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    let token_slot = slot_key(config.book_token_slot);
    let quote_slot = slot_key(config.book_quote_slot);

    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        // First pass over the tx: engine mapping values and per-address init writes.
        let mut engine_values: HashSet<Vec<u8>> = HashSet::new();
        let mut candidates: HashMap<Vec<u8>, InitWrites> = HashMap::new();
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address == config.engine {
                    engine_values.insert(address_from_word(&change.new_value));
                    continue;
                }
                if change.address == config.tesseraswap {
                    continue;
                }
                if change.key == EIP1967_IMPL_SLOT &&
                    is_zero(&change.old_value) &&
                    !is_zero(&change.new_value)
                {
                    candidates
                        .entry(change.address.clone())
                        .or_default()
                        .has_impl = true;
                } else if change.key == token_slot &&
                    is_zero(&change.old_value) &&
                    !is_zero(&change.new_value)
                {
                    candidates
                        .entry(change.address.clone())
                        .or_default()
                        .base_token = Some(address_from_word(&change.new_value));
                } else if change.key == quote_slot &&
                    address_from_word(&change.new_value) == config.usdc
                {
                    candidates
                        .entry(change.address.clone())
                        .or_default()
                        .has_quote = true;
                }
            }
        }

        let mut components = Vec::new();
        for (store_addr, writes) in candidates {
            let Some(base_token) = writes.base_token else { continue };
            if !writes.has_impl || !writes.has_quote || !engine_values.contains(&store_addr) {
                continue;
            }
            let mut tokens = vec![base_token.clone(), config.usdc.clone()];
            tokens.sort_unstable();
            // Tessera emits no token-contract storage, so in the shared simulation DB its
            // tokens are self-contained mock proxies (tycho-simulation PR #1118): the
            // treasury→recipient output `transferFrom` is resolved via local proxy
            // bookkeeping instead of delegating to an implementation another VM protocol
            // indexed for the same token.
            let self_contained = json_serialize_address_list(&tokens);
            let mut contracts =
                vec![config.tesseraswap.clone(), config.engine.clone(), store_addr.clone()];
            contracts.extend(config.tracked_addresses());
            components.push(
                ProtocolComponent::new(&component_id(&config.tesseraswap, &base_token))
                    .with_tokens(&tokens)
                    .with_contracts(&contracts)
                    .with_attributes(&[
                        ("base_token", base_token.as_slice()),
                        ("price_store", store_addr.as_slice()),
                        ("manual_updates", &[1u8][..]),
                        ("self_contained_tokens", self_contained.as_slice()),
                    ])
                    .as_swap_type("tessera_book", ImplementationType::Vm),
            );
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}
