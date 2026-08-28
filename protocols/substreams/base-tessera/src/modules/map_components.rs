use std::collections::{HashMap, HashSet};

use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::{attributes::json_serialize_address_list, prelude::*};

use crate::{
    common::{address_from_word, component_id, is_zero, slot_key, EIP1967_IMPL_SLOT},
    config::DeploymentConfig,
};

/// Per-address init-write evidence collected within one transaction: EIP-1967 impl slot
/// initialized, the base token written to the token slot, and whether the quote slot carries
/// the configured hub token.
#[derive(Default)]
struct InitWrites {
    has_impl: bool,
    base_token: Option<Vec<u8>>,
    has_quote: bool,
}

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
/// * and the engine writes some slot whose value is that store address (the mapping entry) — this
///   last check ties the generic proxy-init shape to this specific venue.
#[substreams::handlers::map]
pub fn map_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    Ok(BlockTransactionProtocolComponents { tx_components: discover(&config, &block) })
}

/// The discovery scan, separated from the handler so it can be unit-tested (the substreams
/// handler macro rewrites the outer function's signature).
fn discover(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
) -> Vec<TransactionProtocolComponents> {
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
    tx_components
}

#[cfg(test)]
mod tests {
    use substreams::hex;
    use substreams_ethereum::pb::eth::v2::{Call, StorageChange, TransactionTrace};

    use super::*;

    const PARAMS: &str = "tesseraswap=55555522005bcae1c2424d474bfd5ed477749e3e\
                          &engine=31e99e05fee3dce580af777c3fd63ee1b3b40c17\
                          &usdc=833589fcd6edb6e08f4c7c32d4f71b54bda02913\
                          &tracked=\
                          &treasury_slot=1&book_token_slot=48&book_quote_slot=49";
    const ENGINE: [u8; 20] = hex!("31e99e05fee3dce580af777c3fd63ee1b3b40c17");
    // The NVDAc book creation (Base block 50,526,653) as ground truth.
    const STORE: [u8; 20] = hex!("ede940cdf2a9c5620cbf97e45947594723e29c14");
    const NVDAC: [u8; 20] = hex!("b20000000000000000000078ee7ce2fe4908108c");
    const USDC: [u8; 20] = hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const IMPL: [u8; 20] = hex!("6d9dd143e42b6338f4f6a7c0c26d124658f641cb");

    fn word(address: &[u8]) -> Vec<u8> {
        let mut w = vec![0u8; 32];
        w[12..].copy_from_slice(address);
        w
    }

    fn change(address: &[u8], key: Vec<u8>, old: Vec<u8>, new: Vec<u8>) -> StorageChange {
        StorageChange {
            address: address.to_vec(),
            key,
            old_value: old,
            new_value: new,
            ..Default::default()
        }
    }

    fn quote_word() -> Vec<u8> {
        // slot 49: 0x00 06 08 ‖ USDC (quote decimals, base decimals, quote token).
        let mut w = word(&USDC);
        w[10] = 0x06;
        w[11] = 0x08;
        w
    }

    fn creation_changes() -> Vec<StorageChange> {
        vec![
            change(&STORE, EIP1967_IMPL_SLOT.to_vec(), vec![0u8; 32], word(&IMPL)),
            change(&STORE, slot_key(48).to_vec(), vec![0u8; 32], word(&NVDAC)),
            change(&STORE, slot_key(49).to_vec(), vec![0u8; 32], quote_word()),
            change(&ENGINE, [0xaa; 32].to_vec(), vec![0u8; 32], word(&STORE)),
        ]
    }

    fn config() -> DeploymentConfig {
        serde_qs::from_str(PARAMS).unwrap()
    }

    fn block_with(changes: Vec<StorageChange>, reverted: bool) -> eth::v2::Block {
        eth::v2::Block {
            transaction_traces: vec![TransactionTrace {
                status: 1, // succeeded
                calls: vec![Call {
                    storage_changes: changes,
                    state_reverted: reverted,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn discovers_book_from_creation_writes() {
        let out = discover(&config(), &block_with(creation_changes(), false));
        assert_eq!(out.len(), 1);
        let components = &out[0].components;
        assert_eq!(components.len(), 1);
        let c = &components[0];
        assert_eq!(c.id, "0x55555522005bcae1c2424d474bfd5ed477749e3e00000078ee7ce2fe4908108c");
        // Tokens sorted ascending: USDC (0x83…) before NVDAc (0xb2…).
        assert_eq!(c.tokens, vec![USDC.to_vec(), NVDAC.to_vec()]);
        assert_eq!(c.get_attribute_value("base_token"), Some(NVDAC.to_vec()));
        assert_eq!(c.get_attribute_value("price_store"), Some(STORE.to_vec()));
        assert_eq!(c.get_attribute_value("manual_updates"), Some(vec![1u8]));
        // Store and stable addresses are tracked contracts.
        assert!(c.contracts.contains(&STORE.to_vec()));
        assert!(c.contracts.contains(&ENGINE.to_vec()));
    }

    #[test]
    fn requires_the_engine_mapping_write() {
        let mut changes = creation_changes();
        changes.retain(|c| c.address != ENGINE);
        let out = discover(&config(), &block_with(changes, false));
        assert!(out.is_empty());
    }

    #[test]
    fn requires_the_impl_slot_init() {
        let mut changes = creation_changes();
        changes.retain(|c| c.key != EIP1967_IMPL_SLOT.to_vec());
        let out = discover(&config(), &block_with(changes, false));
        assert!(out.is_empty());
    }

    #[test]
    fn requires_the_quote_token_to_match() {
        let mut changes = creation_changes();
        for c in &mut changes {
            if c.key == slot_key(49).to_vec() {
                c.new_value = word(&NVDAC); // wrong quote token
            }
        }
        let out = discover(&config(), &block_with(changes, false));
        assert!(out.is_empty());
    }

    #[test]
    fn ignores_reverted_calls() {
        let out = discover(&config(), &block_with(creation_changes(), true));
        assert!(out.is_empty());
    }

    #[test]
    fn ignores_non_creation_impl_upgrades() {
        // An impl upgrade writes the EIP-1967 slot non-zero -> non-zero and has no
        // token-slot init; it must not re-create the component.
        let changes = vec![
            change(&STORE, EIP1967_IMPL_SLOT.to_vec(), word(&IMPL), word(&ENGINE)),
            change(&ENGINE, [0xaa; 32].to_vec(), word(&STORE), word(&STORE)),
        ];
        let out = discover(&config(), &block_with(changes, false));
        assert!(out.is_empty());
    }
}
