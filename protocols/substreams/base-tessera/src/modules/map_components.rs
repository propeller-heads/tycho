use std::collections::HashMap;

use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::{attributes::json_serialize_address_list, prelude::*};

use crate::{
    common::{
        address_from_word, component_id, engine_pair_slot, is_zero, slot_key, EIP1967_IMPL_SLOT,
    },
    config::DeploymentConfig,
};

/// Per-address init-write evidence collected within one transaction: EIP-1967 impl slot
/// initialized, and the base/quote tokens written to the pair's identity slots.
#[derive(Default)]
struct InitWrites {
    has_impl: bool,
    base_token: Option<Vec<u8>>,
    quote_token: Option<Vec<u8>>,
}

/// Discovers pairs from the creation of their contract.
///
/// Tessera emits no creation event. A pair is created by an owner-Safe → engine admin call
/// that internally CREATEs the pair contract (an EIP-1967 proxy); in that same transaction the
/// pair's init writes its implementation slot and its identity slots (base and quote token),
/// and the engine registers it in its `pairKey => pair` mapping. An address is recognized as a
/// new pair when, within one non-reverted transaction:
///
/// * its EIP-1967 implementation slot goes zero → non-zero,
/// * its base-token and quote-token slots go zero → non-zero,
/// * and the engine writes the pair's address into **exactly** the mapping slot derived from those
///   two tokens (`keccak(abi.encode(keccak(abi.encode(lo, hi)), pair_map_slot))`) — the formula is
///   verified against all 15 registered pairs on Base, and the exact-slot match ties the generic
///   proxy-init shape to this specific venue.
///
/// The quote token is **not** constrained: most pairs quote in USDC, but WETH/USDbC and
/// ZORA/USDT exist in the registry, and the engine routes through its bridge token only when a
/// direct pair is missing.
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
    let base_token_slot = slot_key(config.pair_base_token_slot);
    let quote_token_slot = slot_key(config.pair_quote_token_slot);

    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        // First pass over the tx: engine mapping writes and per-address init writes.
        // engine slot key -> value's low 20 bytes.
        let mut engine_writes: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut candidates: HashMap<Vec<u8>, InitWrites> = HashMap::new();
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address == config.engine {
                    engine_writes.insert(change.key.clone(), address_from_word(&change.new_value));
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
                } else if change.key == base_token_slot &&
                    is_zero(&change.old_value) &&
                    !is_zero(&change.new_value)
                {
                    candidates
                        .entry(change.address.clone())
                        .or_default()
                        .base_token = Some(address_from_word(&change.new_value));
                } else if change.key == quote_token_slot &&
                    is_zero(&change.old_value) &&
                    !is_zero(&change.new_value)
                {
                    candidates
                        .entry(change.address.clone())
                        .or_default()
                        .quote_token = Some(address_from_word(&change.new_value));
                }
            }
        }

        let mut components = Vec::new();
        for (pair, writes) in candidates {
            let (Some(base_token), Some(quote_token)) = (writes.base_token, writes.quote_token)
            else {
                continue;
            };
            if !writes.has_impl {
                continue;
            }
            let registry_slot = engine_pair_slot(&base_token, &quote_token, config.pair_map_slot);
            if engine_writes.get(&registry_slot) != Some(&pair) {
                continue;
            }
            let mut tokens = vec![base_token.clone(), quote_token.clone()];
            tokens.sort_unstable();
            // Tessera emits no token-contract storage, so in the shared simulation DB its
            // tokens are self-contained mock proxies (tycho-simulation PR #1118): the
            // treasury→recipient output `transferFrom` is resolved via local proxy
            // bookkeeping instead of delegating to an implementation another VM protocol
            // indexed for the same token.
            let self_contained = json_serialize_address_list(&tokens);
            // Only contracts that exist on-chain at creation time may be referenced here: the
            // storage layer resolves every entry against known accounts and fails the flush on
            // a miss. The code-only satellites (pair implementations, pricing libs, the
            // write-path contract) are deployed at unrelated blocks, so they are delivered as
            // plain account changes through the tracked-contract predicate instead (and via
            // `initialized_accounts` bootstrap for ranges that start after their deployment).
            let contracts = vec![config.tesseraswap.clone(), config.engine.clone(), pair.clone()];
            components.push(
                ProtocolComponent::new(&component_id(&pair))
                    .with_tokens(&tokens)
                    .with_contracts(&contracts)
                    .with_attributes(&[
                        ("base_token", base_token.as_slice()),
                        ("quote_token", quote_token.as_slice()),
                        ("manual_updates", &[1u8][..]),
                        ("self_contained_tokens", self_contained.as_slice()),
                    ])
                    .as_swap_type("tessera_pair", ImplementationType::Vm),
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
                          &tracked=\
                          &treasury_slot=1&treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae\
                          &pair_map_slot=8&pair_base_token_slot=48&pair_quote_token_slot=49\
                          &pair_lib_slot=51";
    const ENGINE: [u8; 20] = hex!("31e99e05fee3dce580af777c3fd63ee1b3b40c17");
    // The NVDAc pair creation (Base block 50,526,653) as ground truth.
    const PAIR: [u8; 20] = hex!("ede940cdf2a9c5620cbf97e45947594723e29c14");
    const NVDAC: [u8; 20] = hex!("b20000000000000000000078ee7ce2fe4908108c");
    const USDC: [u8; 20] = hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const IMPL: [u8; 20] = hex!("6d9dd143e42b6338f4f6a7c0c26d124658f641cb");

    fn config() -> DeploymentConfig {
        serde_qs::from_str(PARAMS).unwrap()
    }

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

    fn creation_changes() -> Vec<StorageChange> {
        vec![
            change(&PAIR, EIP1967_IMPL_SLOT.to_vec(), vec![0u8; 32], word(&IMPL)),
            change(&PAIR, slot_key(48).to_vec(), vec![0u8; 32], word(&NVDAC)),
            change(&PAIR, slot_key(49).to_vec(), vec![0u8; 32], word(&USDC)),
            change(&ENGINE, engine_pair_slot(&NVDAC, &USDC, 8), vec![0u8; 32], word(&PAIR)),
        ]
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
    fn discovers_pair_from_creation_writes() {
        let out = discover(&config(), &block_with(creation_changes(), false));
        assert_eq!(out.len(), 1);
        let components = &out[0].components;
        assert_eq!(components.len(), 1);
        let c = &components[0];
        assert_eq!(c.id, "0xede940cdf2a9c5620cbf97e45947594723e29c14");
        // Tokens sorted ascending: USDC (0x83…) before NVDAc (0xb2…).
        assert_eq!(c.tokens, vec![USDC.to_vec(), NVDAC.to_vec()]);
        assert_eq!(c.get_attribute_value("base_token"), Some(NVDAC.to_vec()));
        assert_eq!(c.get_attribute_value("quote_token"), Some(USDC.to_vec()));
        assert_eq!(c.get_attribute_value("manual_updates"), Some(vec![1u8]));
        assert!(c.contracts.contains(&PAIR.to_vec()));
        assert!(c.contracts.contains(&ENGINE.to_vec()));
    }

    #[test]
    fn discovers_non_usdc_quoted_pairs() {
        // ZORA/USDT is a registered pair; the quote token is not constrained.
        let zora = hex!("1111111111166b7fe7bd91427724b487980afc69");
        let usdt = hex!("fde4c96c8593536e31f229ea8f37b2ada2699bb2");
        let pair = hex!("e77ed4807953f27f4777a6e03023b550beb4f831");
        let changes = vec![
            change(&pair, EIP1967_IMPL_SLOT.to_vec(), vec![0u8; 32], word(&IMPL)),
            change(&pair, slot_key(48).to_vec(), vec![0u8; 32], word(&zora)),
            change(&pair, slot_key(49).to_vec(), vec![0u8; 32], word(&usdt)),
            change(&ENGINE, engine_pair_slot(&zora, &usdt, 8), vec![0u8; 32], word(&pair)),
        ];
        let out = discover(&config(), &block_with(changes, false));
        assert_eq!(out.len(), 1);
        let c = &out[0].components[0];
        assert_eq!(c.id, "0xe77ed4807953f27f4777a6e03023b550beb4f831");
        assert_eq!(c.get_attribute_value("quote_token"), Some(usdt.to_vec()));
    }

    #[test]
    fn requires_the_exact_engine_registry_write() {
        // An engine write of the right value at the WRONG slot must not count.
        let mut changes = creation_changes();
        changes.retain(|c| c.address != ENGINE);
        changes.push(change(&ENGINE, [0xaa; 32].to_vec(), vec![0u8; 32], word(&PAIR)));
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
    fn requires_both_token_slots() {
        let mut changes = creation_changes();
        changes.retain(|c| c.key != slot_key(49).to_vec());
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
            change(&PAIR, EIP1967_IMPL_SLOT.to_vec(), word(&IMPL), word(&ENGINE)),
            change(&ENGINE, engine_pair_slot(&NVDAC, &USDC, 8), word(&PAIR), word(&PAIR)),
        ];
        let out = discover(&config(), &block_with(changes, false));
        assert!(out.is_empty());
    }

    #[test]
    fn discovers_multiple_pairs_in_one_transaction() {
        // The NVDAc creation tx actually created four pairs at once.
        let aaplc = hex!("b200000000000000000000c2e324d24d7eecd1fb");
        let pair2 = hex!("4dc9db885cff6bdee2c26d1d23b9be6c4c2ade94");
        let mut changes = creation_changes();
        changes.extend([
            change(&pair2, EIP1967_IMPL_SLOT.to_vec(), vec![0u8; 32], word(&IMPL)),
            change(&pair2, slot_key(48).to_vec(), vec![0u8; 32], word(&aaplc)),
            change(&pair2, slot_key(49).to_vec(), vec![0u8; 32], word(&USDC)),
            change(&ENGINE, engine_pair_slot(&aaplc, &USDC, 8), vec![0u8; 32], word(&pair2)),
        ]);
        let out = discover(&config(), &block_with(changes, false));
        assert_eq!(out[0].components.len(), 2);
    }
}
