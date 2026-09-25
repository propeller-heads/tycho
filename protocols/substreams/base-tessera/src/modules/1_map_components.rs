use crate::{common::*, config::DeploymentConfig};
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

/// Skip unrelated transactions before allocating or sorting their storage writes.
fn discovery_writes<'a>(
    tx: &'a eth::v2::TransactionTrace,
    config: &DeploymentConfig,
) -> Vec<&'a eth::v2::StorageChange> {
    let writes = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| &call.storage_changes);
    if !writes
        .clone()
        .any(|write| write.address == config.engine)
    {
        return vec![];
    }
    let base_slot = slot(config.pair_base_token_slot);
    let quote_slot = slot(config.pair_quote_token_slot);
    let mut relevant: Vec<_> = writes
        .filter(|write| {
            // Keep all Engine writes so the last registry value wins, including clears.
            write.address == config.engine ||
                ((write.key == IMPLEMENTATION_SLOT ||
                    write.key == base_slot ||
                    write.key == quote_slot) &&
                    zero(&write.old_value) &&
                    !zero(&write.new_value))
        })
        .collect();
    // Nested call order is not storage-write order.
    relevant.sort_by_key(|write| write.ordinal);
    relevant
}

/// Only an initialized proxy registered by this Engine in the same committed tx is a Pair.
fn discover(tx: &eth::v2::TransactionTrace, config: &DeploymentConfig) -> Vec<ProtocolComponent> {
    let writes = discovery_writes(tx, config);
    let registry: HashMap<_, _> = writes
        .iter()
        .filter(|w| w.address == config.engine)
        .map(|w| (&w.key, &w.new_value))
        .collect();
    if registry.is_empty() {
        return vec![];
    }
    let base_slot = slot(config.pair_base_token_slot);
    let quote_slot = slot(config.pair_quote_token_slot);
    let mut candidates: BTreeMap<Vec<u8>, HashMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
    for w in writes {
        if (w.key == IMPLEMENTATION_SLOT || w.key == base_slot || w.key == quote_slot) &&
            zero(&w.old_value) &&
            !zero(&w.new_value)
        {
            candidates
                .entry(w.address.clone())
                .or_default()
                .insert(w.key.clone(), w.new_value.clone());
        }
    }
    candidates
        .into_iter()
        .filter_map(|(pair, slots)| {
            slots.get(IMPLEMENTATION_SLOT.as_slice())?;
            let base = address(slots.get(&base_slot)?);
            let quote = address(slots.get(&quote_slot)?);
            if zero(&base) || zero(&quote) || base == quote {
                return None;
            }
            let key = registry_slot(&base, &quote, config.pair_map_slot);
            if address(registry.get(&key)?) != pair {
                return None;
            }
            let mut tokens = vec![base.clone(), quote.clone()];
            tokens.sort();
            let contained = json_serialize_address_list(&tokens);
            Some(
                ProtocolComponent::new(&id(&pair))
                    .with_tokens(&tokens)
                    .with_contracts(&[config.tesseraswap.clone(), config.engine.clone(), pair])
                    .with_attributes(&[
                        ("base_token", base.as_slice()),
                        ("quote_token", quote.as_slice()),
                        ("manual_updates", &[1]),
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
    fn config() -> DeploymentConfig {
        DeploymentConfig::parse("tesseraswap=1111111111111111111111111111111111111111&engine=2222222222222222222222222222222222222222&treasury=3333333333333333333333333333333333333333&treasury_slot=1&pair_map_slot=8&pair_base_token_slot=48&pair_quote_token_slot=49&pair_lib_slot=51&pair_write_helper_slot=52").unwrap()
    }
    fn transaction() -> eth::v2::TransactionTrace {
        let c = config();
        let pair = vec![4; 20];
        let base = vec![5; 20];
        let quote = vec![6; 20];
        let write = |address, key, new_value| eth::v2::StorageChange {
            address,
            key,
            old_value: vec![0; 32],
            new_value,
            ..Default::default()
        };
        eth::v2::TransactionTrace {
            calls: vec![eth::v2::Call {
                storage_changes: vec![
                    write(pair.clone(), IMPLEMENTATION_SLOT.to_vec(), vec![7; 32]),
                    write(pair.clone(), slot(48), base.clone()),
                    write(pair.clone(), slot(49), quote.clone()),
                    write(c.engine, registry_slot(&base, &quote, 8), pair),
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn discovers_non_usdc_pair_and_uses_pair_id() {
        let result = discover(&transaction(), &config());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, id(&[4; 20]));
        assert_eq!(result[0].tokens, vec![vec![5; 20], vec![6; 20]]);
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
    fn registry_key_is_sorted_and_wrong_mapping_is_rejected() {
        assert_eq!(registry_slot(&[5; 20], &[6; 20], 8), registry_slot(&[6; 20], &[5; 20], 8));
        let mut tx = transaction();
        tx.calls[0].storage_changes[3].key = slot(8);
        assert!(discover(&tx, &config()).is_empty());
    }
    #[test]
    fn invalid_address_config_is_rejected() {
        assert!(DeploymentConfig::parse("tesseraswap=11").is_err());
    }

    #[test]
    fn filters_unrelated_writes_and_transactions() {
        let mut tx = transaction();
        tx.calls[0]
            .storage_changes
            .extend((100..200).map(|key| eth::v2::StorageChange {
                address: vec![9; 20],
                key: slot(key),
                old_value: vec![0; 32],
                new_value: vec![1; 32],
                ..Default::default()
            }));
        assert_eq!(discovery_writes(&tx, &config()).len(), 4);
        assert_eq!(discover(&tx, &config()).len(), 1);
        tx.calls[0].storage_changes.remove(3);
        assert!(discovery_writes(&tx, &config()).is_empty());
    }

    #[test]
    fn nested_registry_writes_use_last_ordinal_including_clear() {
        let mut tx = transaction();
        tx.calls[0].storage_changes[3].ordinal = 40;
        let mut earlier = tx.calls[0].storage_changes[3].clone();
        earlier.ordinal = 30;
        earlier.new_value = vec![9; 20];
        tx.calls
            .push(eth::v2::Call { storage_changes: vec![earlier], ..Default::default() });
        assert_eq!(discover(&tx, &config()).len(), 1);
        tx.calls[0].storage_changes[3].old_value = vec![4; 20];
        tx.calls[0].storage_changes[3].new_value = vec![0; 32];
        assert!(discover(&tx, &config()).is_empty());
    }
}
