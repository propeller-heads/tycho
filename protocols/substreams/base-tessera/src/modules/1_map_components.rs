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
    use crate::common::testing::slot;
    use alloy_primitives::Address;

    fn config() -> DeploymentConfig {
        DeploymentConfig::parse("tesseraswap=1111111111111111111111111111111111111111&engine=2222222222222222222222222222222222222222&treasury=3333333333333333333333333333333333333333").unwrap()
    }
    fn transaction() -> eth::v2::TransactionTrace {
        let c = config();
        let pair = Address::repeat_byte(4);
        let base = Address::repeat_byte(5);
        let quote = Address::repeat_byte(6);
        let write = |address: Address, key: B256, new_value: B256| eth::v2::StorageChange {
            address: address.to_vec(),
            key: key.to_vec(),
            old_value: vec![0; 32],
            new_value: new_value.to_vec(),
            ..Default::default()
        };
        eth::v2::TransactionTrace {
            calls: vec![eth::v2::Call {
                storage_changes: vec![
                    write(pair, IMPLEMENTATION_SLOT, B256::repeat_byte(7)),
                    write(pair, slot(48), base.into_word()),
                    write(pair, slot(49), quote.into_word()),
                    write(c.engine, registry_slot(base, quote), pair.into_word()),
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn discovers_registered_pair() {
        let result = discover(&transaction(), &config());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, id([4; 20]));
        assert_eq!(result[0].tokens, vec![vec![5; 20], vec![6; 20]]);
    }
    #[test]
    fn pair_updates_on_any_change_to_its_contracts_or_balances() {
        // With `manual_updates`, consumers drop a balance-only change unless it is marked, and
        // a swap on one Pair moves the treasury balance of every Pair sharing a token.
        let result = discover(&transaction(), &config());
        assert!(!result[0]
            .static_att
            .iter()
            .any(|attribute| attribute.name == "manual_updates"));
    }
    #[test]
    fn rejects_unregistered_proxy_and_reverted_calls() {
        let mut tx = transaction();
        tx.calls[0].storage_changes.pop();
        assert!(discover(&tx, &config()).is_empty());
        let mut tx = transaction();
        tx.calls[0].state_reverted = true;
        assert!(discover(&tx, &config()).is_empty());
    }
    #[test]
    fn upgrade_does_not_create_component() {
        let mut tx = transaction();
        tx.calls[0].storage_changes[0].old_value = vec![8; 32];
        assert!(discover(&tx, &config()).is_empty());
    }
    #[test]
    fn discovers_pair_whose_base_token_sorts_above_its_quote_token() {
        // The Engine keys its registry by the sorted token pair: keccak256(abi.encode(0x0505…,
        // 0x0606…)) at mapping slot 8, computed with `cast index`.
        let key =
            substreams::hex!("1015ebf4b5bba5d4765ea3c43a1df3370fc92a53197740322f1bf44963ef2c9f");
        let mut tx = transaction();
        let writes = &mut tx.calls[0].storage_changes;
        writes[1].new_value = Address::repeat_byte(6)
            .into_word()
            .to_vec();
        writes[2].new_value = Address::repeat_byte(5)
            .into_word()
            .to_vec();
        writes[3].key = key.to_vec();
        let result = discover(&tx, &config());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tokens, vec![vec![5; 20], vec![6; 20]]);
    }
    #[test]
    fn wrong_registry_mapping_is_rejected() {
        let mut tx = transaction();
        tx.calls[0].storage_changes[3].key = slot(8).to_vec();
        assert!(discover(&tx, &config()).is_empty());
    }

    #[test]
    fn nested_registry_writes_use_last_ordinal_including_clear() {
        let mut tx = transaction();
        tx.calls[0].storage_changes[3].ordinal = 40;
        let mut earlier = tx.calls[0].storage_changes[3].clone();
        earlier.ordinal = 30;
        earlier.new_value = vec![9; 32];
        tx.calls
            .push(eth::v2::Call { storage_changes: vec![earlier], ..Default::default() });
        assert_eq!(discover(&tx, &config()).len(), 1);
        tx.calls[0].storage_changes[3].old_value = Address::repeat_byte(4)
            .into_word()
            .to_vec();
        tx.calls[0].storage_changes[3].new_value = vec![0; 32];
        assert!(discover(&tx, &config()).is_empty());
    }
}
