use crate::{common::*, config::DeploymentConfig};
use alloy_primitives::B256;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use substreams_ethereum::pb::eth;
use tycho_substreams::{attributes::json_serialize_address_list, prelude::*};

#[substreams::handlers::map]
pub fn map_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config = DeploymentConfig::parse(&params)?;
    let tx_components = block
        .transactions()
        .filter_map(|tx| {
            let components = discover(tx, &config);
            (!components.is_empty())
                .then(|| TransactionProtocolComponents { tx: Some(tx.into()), components })
        })
        .collect();
    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// The highest-ordinal write to each storage key: `(ordinal, new value)`.
type LatestWrites<'a> = HashMap<&'a [u8], (u64, &'a [u8])>;

fn keep_latest<'a>(writes: &mut LatestWrites<'a>, w: &'a eth::v2::StorageChange) {
    let latest = writes
        .entry(&w.key)
        .or_insert((w.ordinal, &w.new_value));
    if w.ordinal >= latest.0 {
        *latest = (w.ordinal, &w.new_value);
    }
}

/// Pairs initialized and registered with this Engine in the same committed transaction.
///
/// A Pair initialized in one transaction and registered in a later one is never discovered. That
/// is a deployment assumption, not a property of the code: every Pair on Base so far was created
/// in one transaction that sets the implementation and both token slots and writes the registry
/// entry. Recheck it if Tessera deploys Pairs differently.
fn discover(tx: &eth::v2::TransactionTrace, config: &DeploymentConfig) -> Vec<ProtocolComponent> {
    let init_slots = [IMPLEMENTATION_SLOT, PAIR_BASE_TOKEN_SLOT, PAIR_QUOTE_TOKEN_SLOT];
    // Nested call order is not storage-write order, so the highest ordinal wins, including an
    // Engine registry clear.
    let mut registry = LatestWrites::new();
    let mut candidates: BTreeMap<&[u8], LatestWrites> = BTreeMap::new();
    for w in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| &call.storage_changes)
    {
        if *w.address == config.engine {
            keep_latest(&mut registry, w);
        } else if init_slots
            .iter()
            .any(|key| *key == *w.key) &&
            zero(&w.old_value) &&
            !zero(&w.new_value)
        {
            keep_latest(
                candidates
                    .entry(&w.address)
                    .or_default(),
                w,
            );
        }
    }
    if registry.is_empty() {
        return vec![];
    }
    candidates
        .into_iter()
        .filter_map(|(pair, slots)| {
            if !slots.contains_key(IMPLEMENTATION_SLOT.as_slice()) {
                return None;
            }
            let token = |key: B256| Some(address(slots.get(key.as_slice())?.1));
            let base = token(PAIR_BASE_TOKEN_SLOT)?;
            let quote = token(PAIR_QUOTE_TOKEN_SLOT)?;
            if base.is_zero() || quote.is_zero() || base == quote {
                return None;
            }
            let key = registry_slot(base, quote);
            if address(registry.get(key.as_slice())?.1) != *pair {
                return None;
            }
            let mut tokens = vec![base.to_vec(), quote.to_vec()];
            tokens.sort();
            let contained = json_serialize_address_list(&tokens);
            Some(
                ProtocolComponent::new(&id(pair))
                    .with_tokens(&tokens)
                    .with_contracts(&[
                        config.tesseraswap.to_vec(),
                        config.engine.to_vec(),
                        pair.to_vec(),
                    ])
                    .with_attributes(&[
                        ("base_token", base.as_slice()),
                        ("quote_token", quote.as_slice()),
                        ("self_contained_tokens", &contained),
                    ])
                    .as_swap_type("tessera_pair", ImplementationType::Vm),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{common::testing::slot, config::TEST_CONFIG};
    use alloy_primitives::{address, b256, Address};

    // Two Pairs on Base and the words their creation wrote, read with `cast storage`. Registry
    // slots are `cast index bytes32 $(cast keccak $(cast abi-encode "f(address,address)" lo hi))
    // 8`.
    const WETH: Address = address!("4200000000000000000000000000000000000006");
    const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const CBBTC: Address = address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf");
    const WETH_USDC: Address = address!("f524c1bc1c64a2c99bc7eccf19ede9a1d89d5a7c");
    const CBBTC_USDC: Address = address!("ed57bacdc2a990b631f8817853935791c122c356");
    const IMPLEMENTATION: B256 =
        b256!("0000000000000000000000006d9dd143e42b6338f4f6a7c0c26d124658f641cb");
    /// Pair slot 49 packs the quote token's decimals (6) and the base token's (18 for WETH, 8 for
    /// cbBTC) above the quote token.
    const WETH_USDC_QUOTE_WORD: B256 =
        b256!("000000000000000000000612833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const CBBTC_USDC_QUOTE_WORD: B256 =
        b256!("000000000000000000000608833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const WETH_USDC_REGISTRY_SLOT: B256 =
        b256!("7de34ae1e8ba739875fb9abb94e6baebe5cefd45e23d860202def0971a284fe7");
    const CBBTC_USDC_REGISTRY_SLOT: B256 =
        b256!("43df1786f293c82c2359b918eb71cd73bb3085abf3d30229e5165e38e89325b8");

    fn write(address: Address, key: B256, new_value: B256) -> eth::v2::StorageChange {
        eth::v2::StorageChange {
            address: address.to_vec(),
            key: key.to_vec(),
            old_value: vec![0; 32],
            new_value: new_value.to_vec(),
            ..Default::default()
        }
    }

    /// The writes that create a Pair: implementation, base token, quote word, registry entry.
    fn creation(
        pair: Address,
        base: Address,
        quote_word: B256,
        registry_slot: B256,
    ) -> Vec<eth::v2::StorageChange> {
        vec![
            write(pair, IMPLEMENTATION_SLOT, IMPLEMENTATION),
            write(pair, slot(48), base.into_word()),
            write(pair, slot(49), quote_word),
            write(TEST_CONFIG.engine, registry_slot, pair.into_word()),
        ]
    }

    fn weth_usdc() -> Vec<eth::v2::StorageChange> {
        creation(WETH_USDC, WETH, WETH_USDC_QUOTE_WORD, WETH_USDC_REGISTRY_SLOT)
    }

    fn cbbtc_usdc() -> Vec<eth::v2::StorageChange> {
        creation(CBBTC_USDC, CBBTC, CBBTC_USDC_QUOTE_WORD, CBBTC_USDC_REGISTRY_SLOT)
    }

    fn tx(writes: Vec<eth::v2::StorageChange>) -> eth::v2::TransactionTrace {
        eth::v2::TransactionTrace {
            calls: vec![eth::v2::Call { storage_changes: writes, ..Default::default() }],
            ..Default::default()
        }
    }

    fn attribute(component: &ProtocolComponent, name: &str) -> Option<Vec<u8>> {
        component
            .static_att
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.value.clone())
    }

    #[test]
    fn discovers_registered_pair() {
        let result = discover(&tx(weth_usdc()), &TEST_CONFIG);
        assert_eq!(result.len(), 1);
        let pair = &result[0];
        assert_eq!(pair.id, "0xf524c1bc1c64a2c99bc7eccf19ede9a1d89d5a7c");
        assert_eq!(pair.tokens, vec![WETH.to_vec(), USDC.to_vec()]);
        assert_eq!(
            pair.contracts,
            vec![TEST_CONFIG.tesseraswap.to_vec(), TEST_CONFIG.engine.to_vec(), WETH_USDC.to_vec()]
        );
        assert_eq!(attribute(pair, "base_token"), Some(WETH.to_vec()));
        assert_eq!(attribute(pair, "quote_token"), Some(USDC.to_vec()));
    }

    #[test]
    fn discovers_pair_whose_base_token_sorts_above_its_quote_token() {
        let result = discover(&tx(cbbtc_usdc()), &TEST_CONFIG);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tokens, vec![USDC.to_vec(), CBBTC.to_vec()]);
        assert_eq!(attribute(&result[0], "base_token"), Some(CBBTC.to_vec()));
    }

    #[test]
    fn discovers_only_the_registered_pair_of_two_in_one_transaction() {
        let mut writes = weth_usdc();
        let mut unregistered = cbbtc_usdc();
        unregistered.pop();
        writes.extend(unregistered);
        let result = discover(&tx(writes), &TEST_CONFIG);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, id(WETH_USDC));
    }

    #[test]
    fn pair_updates_on_any_change_to_its_contracts_or_balances() {
        // With `manual_updates`, consumers drop a balance-only change unless it is marked, and
        // a swap on one Pair moves the treasury balance of every Pair sharing a token.
        let result = discover(&tx(weth_usdc()), &TEST_CONFIG);
        assert_eq!(attribute(&result[0], "manual_updates"), None);
    }

    #[test]
    fn rejects_pair_missing_an_initialization_write() {
        for (i, missing) in ["implementation", "base token", "quote token"]
            .iter()
            .enumerate()
        {
            let mut writes = weth_usdc();
            writes.remove(i);
            assert!(discover(&tx(writes), &TEST_CONFIG).is_empty(), "without the {missing}");
        }
    }

    #[test]
    fn rejects_invalid_tokens_or_registration() {
        type Edit = fn(&mut Vec<eth::v2::StorageChange>);
        let cases: [(&str, Edit); 6] = [
            ("an upgraded proxy", |w| w[0].old_value = IMPLEMENTATION.to_vec()),
            ("an unregistered pair", |w| {
                w.pop();
            }),
            ("a registry entry in another mapping", |w| w[3].key = slot(8).to_vec()),
            ("a registry entry naming another pair", |w| {
                w[3].new_value = CBBTC_USDC.into_word().to_vec()
            }),
            ("a base token equal to the quote token", |w| {
                w[2].new_value = WETH.into_word().to_vec();
                w[3].key =
                    b256!("753981f9c0afce31d11cbd6fec29b5399db556ed00771d8362ff48fd79e26dc3")
                        .to_vec();
            }),
            ("a quote word with decimals but no address", |w| {
                w[2].new_value =
                    b256!("0000000000000000000006120000000000000000000000000000000000000000")
                        .to_vec();
                w[3].key =
                    b256!("37698330db4f5f1c94ae6999f5c9d2c06edc7cbf7e37e59010a764b9bc2dfbe8")
                        .to_vec();
            }),
        ];
        for (case, edit) in cases {
            let mut writes = weth_usdc();
            edit(&mut writes);
            assert!(discover(&tx(writes), &TEST_CONFIG).is_empty(), "{case}");
        }
    }

    #[test]
    fn ignores_writes_from_reverted_calls() {
        let mut tx = tx(weth_usdc());
        tx.calls[0].state_reverted = true;
        assert!(discover(&tx, &TEST_CONFIG).is_empty());
    }

    #[test]
    fn nested_registry_writes_use_last_ordinal_including_clear() {
        let mut tx = tx(weth_usdc());
        tx.calls[0].storage_changes[3].ordinal = 40;
        let mut earlier = tx.calls[0].storage_changes[3].clone();
        earlier.ordinal = 30;
        earlier.new_value = CBBTC_USDC.into_word().to_vec();
        tx.calls
            .push(eth::v2::Call { storage_changes: vec![earlier], ..Default::default() });
        assert_eq!(discover(&tx, &TEST_CONFIG).len(), 1);
        tx.calls[0].storage_changes[3].old_value = WETH_USDC.into_word().to_vec();
        tx.calls[0].storage_changes[3].new_value = vec![0; 32];
        assert!(discover(&tx, &TEST_CONFIG).is_empty());
    }
}
