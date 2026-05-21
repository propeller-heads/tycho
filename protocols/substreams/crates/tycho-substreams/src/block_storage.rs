use std::collections::{HashMap, HashSet};

use substreams_ethereum::pb::eth::{
    self,
    v2::{block::DetailLevel, BalanceChange, StorageChange},
};

use crate::{
    models::{ContractSlot, StorageChanges},
    pb::tycho::evm::v1::TransactionStorageChanges,
};

#[allow(dead_code)]
/// Helper function to extract all storage changes on a block.
/// The raw block information collected is intended to be used by the DCI (Dynamic Contract Indexer)
/// to extract and index relevant changes. This is specifically for dynamically identified contracts
/// that the DCI has chosen to index. Note that core protocol data should still be properly
/// integrated and indexed by the substreams package as per usual.
///
/// ## Panics
/// Panics if the provided block is not an extended block model, as indicated by its detail level.
///
/// ## Warning
/// ⚠️ This function *only* works if the **extended block model** is available,
/// more [here](https://streamingfastio.medium.com/new-block-model-to-accelerate-chain-integration-9f65126e5425)
pub fn get_block_storage_changes(block: &eth::v2::Block) -> Vec<TransactionStorageChanges> {
    if block.detail_level != Into::<i32>::into(DetailLevel::DetaillevelExtended) {
        panic!("Only extended blocks are supported");
    }
    let mut block_storage_changes = Vec::with_capacity(block.transaction_traces.len());

    for block_tx in block.transactions() {
        let mut changes_by_address: HashMap<Vec<u8>, Vec<StorageChange>> = HashMap::new();
        for storage_change in block_tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
        {
            changes_by_address
                .entry(storage_change.address.clone())
                .or_default()
                .push(storage_change.clone());
        }

        let mut native_balance_changes_by_address: HashMap<Vec<u8>, Vec<BalanceChange>> =
            HashMap::new();
        for balance_change in block_tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.balance_changes.iter())
        {
            native_balance_changes_by_address
                .entry(balance_change.address.clone())
                .or_default()
                .push(balance_change.clone());
        }

        // Collect all unique addresses from both storage changes and balance changes
        let mut all_addresses = HashSet::new();
        all_addresses.extend(changes_by_address.keys().cloned());
        all_addresses.extend(
            native_balance_changes_by_address
                .keys()
                .cloned(),
        );

        // For each address, collect both storage changes and balance changes
        let tx_storage_changes: Vec<StorageChanges> = all_addresses
            .into_iter()
            .map(|address| {
                // Process storage changes for this address
                let slots = if let Some(changes) = changes_by_address.get(&address) {
                    let mut changes = changes.clone();
                    changes.sort_unstable_by_key(|change| change.ordinal);

                    // Collect latest change per slot
                    let mut latest_changes: HashMap<Vec<u8>, ContractSlot> = HashMap::new();
                    for change in changes {
                        latest_changes
                            .entry(change.key.clone())
                            .and_modify(|slot| {
                                // Only update the latest value, previous value stays the first seen
                                // one.
                                slot.value = change.new_value.clone();
                            })
                            .or_insert(ContractSlot {
                                slot: change.key,
                                value: change.new_value,
                                previous_value: change.old_value,
                            });
                    }
                    latest_changes.into_values().collect()
                } else {
                    vec![]
                };

                // Filter out slots that have the same value before and after the transaction
                let slots = slots
                    .into_iter()
                    .filter(|slot| slot.previous_value != slot.value)
                    .collect();

                // Process native balance changes for this address
                let native_balance = native_balance_changes_by_address
                    .get(&address)
                    .and_then(|balance_changes| {
                        let (first, last) = balance_changes.iter().fold(
                            (None, None),
                            |(min, max): (Option<&BalanceChange>, Option<&BalanceChange>),
                             change| {
                                let new_min = match min {
                                    None => Some(change),
                                    Some(m) if change.ordinal < m.ordinal => Some(change),
                                    _ => min,
                                };
                                let new_max = match max {
                                    None => Some(change),
                                    Some(m) if change.ordinal > m.ordinal => Some(change),
                                    _ => max,
                                };
                                (new_min, new_max)
                            },
                        );

                        let balance_before_tx = first.map(|f| {
                            f.old_value
                                .as_ref()
                                .map(|b| b.bytes.clone())
                                .unwrap_or_default()
                        });
                        let balance_after_tx = last.map(|l| {
                            l.new_value
                                .as_ref()
                                .map(|b| b.bytes.clone())
                                .unwrap_or_default()
                        });

                        (balance_before_tx != balance_after_tx).then_some(balance_after_tx.clone())
                    })
                    .flatten();

                StorageChanges { address, slots, native_balance }
            })
            .collect();

        block_storage_changes.push(TransactionStorageChanges {
            tx: Some(block_tx.into()),
            storage_changes: tx_storage_changes,
        });
    }

    block_storage_changes
}

#[cfg(test)]
mod test {

    use super::*;
    use crate::testing::assets::read_block;

    #[test]
    fn test_get_block_storage_changes_ethereum_block_23490768() {
        let block = read_block("./assets/ethereum-block-23490768.binpb.base64");
        let changes = get_block_storage_changes(&block);

        let mut balance_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        #[allow(clippy::type_complexity)]
        let mut storage_map: HashMap<
            String,
            HashMap<String, HashMap<String, (String, String)>>,
        > = HashMap::new();
        for change in changes {
            let tx_hash = change.tx.unwrap().hash.clone();
            let balance_tx_entry = balance_map
                .entry(hex::encode(tx_hash.clone()))
                .or_default();
            let storage_tx_entry = storage_map
                .entry(hex::encode(tx_hash.clone()))
                .or_default();
            for storage_change in change.storage_changes {
                if let Some(native_balance) = storage_change.native_balance {
                    balance_tx_entry.insert(
                        hex::encode(storage_change.address.clone()),
                        hex::encode(native_balance.clone()),
                    );
                }
                for slot in storage_change.slots {
                    let contract_tx_entry = storage_tx_entry
                        .entry(hex::encode(storage_change.address.clone()))
                        .or_default();
                    contract_tx_entry.insert(
                        hex::encode(slot.slot.clone()),
                        (hex::encode(slot.previous_value.clone()), hex::encode(slot.value.clone())),
                    );
                }
            }
        }

        // Assertions for https://etherscan.io/tx/0x44a34ba7400fa7004ec5037aeb1103a7c0cd8a83a95c4cd5cf9561c3c38db326#statechange
        // Check balance changes
        let balance_tx_entry = balance_map
            .get("44a34ba7400fa7004ec5037aeb1103a7c0cd8a83a95c4cd5cf9561c3c38db326")
            .unwrap();

        assert_eq!(balance_tx_entry.len(), 4);
        assert_eq!(
            balance_tx_entry
                .get("dadb0d80178819f2319190d340ce9a924f783711")
                .unwrap(),
            "052196f442fadb8314"
        );

        assert_eq!(
            balance_tx_entry
                .get("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
                .unwrap(),
            "0207150b274902c5e7871c"
        );
        assert_eq!(
            balance_tx_entry
                .get("ad01c20d5886137e056775af56915de824c8fce5")
                .unwrap(),
            "c83a1d6287cb5e"
        );
        assert_eq!(
            balance_tx_entry
                .get("638f1db9881a84af9835c6625d17b0af034234ad")
                .unwrap(),
            "0f69303da21468"
        );

        // Check storage changes
        let storage_tx_entry = storage_map
            .get("44a34ba7400fa7004ec5037aeb1103a7c0cd8a83a95c4cd5cf9561c3c38db326")
            .unwrap();

        assert_eq!(storage_tx_entry.len(), 3);

        let storage_tx_entry_0f9e3401a5155a02c86353c3d9b24214876779dd = HashMap::from([
            (
                "0000000000000000000000000000000000000000000000000000000000000009".to_string(),
                (
                    "00000000000000000000000000000000009faeae5180599c05015fcfa242d3b0".to_string(),
                    "00000000000000000000000000000000009faebb96f403f1913f425b3ea446e0".to_string(),
                ),
            ),
            (
                "000000000000000000000000000000000000000000000000000000000000000a".to_string(),
                (
                    "00000000000000000000000000f94f053f65617829584571d9de584cd219fb88".to_string(),
                    "00000000000000000000000000f94f66e6e9d8f6688d6ca53ff9baae52e11cd8".to_string(),
                ),
            ),
            (
                "0000000000000000000000000000000000000000000000000000000000000008".to_string(),
                (
                    "68de8f37000000000001fb7a6a5bb2b548080000000560989aab8af59d9be89b".to_string(),
                    "68de8f5b000000000001fb8b2909997ca55100000005606b52e81f19442026af".to_string(),
                ),
            ),
        ]);
        assert_eq!(
            storage_tx_entry
                .get("0f9e3401a5155a02c86353c3d9b24214876779dd")
                .unwrap(),
            &storage_tx_entry_0f9e3401a5155a02c86353c3d9b24214876779dd
        );

        let storage_tx_entry_11dfc652eb62c723ad8c2ae731fcede58ab07564 = HashMap::from([
            (
                "654f44e59f538551b5124259a61eaadb863c6c10cc9d43aa550237a76a7de0b0".to_string(),
                (
                    "000000000000000000000000000000000000000000000077c1c5e25db942af6a".to_string(),
                    "0000000000000000000000000000000000000000000000a2c5f2bc08a7dea7a4".to_string(),
                ),
            ),
            (
                "6b12653da4ae5b17258ea9b02a62123c9305455af47b7dceea1b7137f7c69671".to_string(),
                (
                    "0000000000000000000000000000000000000000000001454f7d5d0ce8d4a21e".to_string(),
                    "0000000000000000000000000000000000000000000001479313ef3e53b46bd0".to_string(),
                ),
            ),
            (
                "8f60e36f69a92730149f231ad2475b4aa8a8e50f4072f62a1f099ffc11d0f647".to_string(),
                (
                    "0000000000000000000000000000000000000000000560989aab8af59d9be89b".to_string(),
                    "00000000000000000000000000000000000000000005606b52e81f19442026af".to_string(),
                ),
            ),
        ]);
        assert_eq!(
            storage_tx_entry
                .get("11dfc652eb62c723ad8c2ae731fcede58ab07564")
                .unwrap(),
            &storage_tx_entry_11dfc652eb62c723ad8c2ae731fcede58ab07564
        );

        let storage_tx_entry_c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2 = HashMap::from([(
            "77f05379c72cc19907ba9648dcd0bda409fabc68ca111b532de62ffdb67e868f".to_string(),
            (
                "000000000000000000000000000000000000000000000001fb7a6a5bb2b54808".to_string(),
                "000000000000000000000000000000000000000000000001fb8b2909997ca551".to_string(),
            ),
        )]);
        assert_eq!(
            storage_tx_entry
                .get("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
                .unwrap(),
            &storage_tx_entry_c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2
        );
    }
}
