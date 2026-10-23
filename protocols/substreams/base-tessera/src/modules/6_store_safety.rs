use crate::{common::*, config::DeploymentConfig, pb::tessera::v1::BlockStorageChanges};
use std::collections::HashSet;
use substreams::store::{StoreGet, StoreGetArray, StoreNew, StoreSet, StoreSetRaw};

/// Persist TesseraSwap's Engine and treasury, each Pair's write helper and the tag-0 fee words,
/// even when the affected pair has no price update.
#[substreams::handlers::store]
pub fn store_safety(
    params: String,
    changes: BlockStorageChanges,
    pair_store: StoreGetArray<String>,
    store: StoreSetRaw,
) {
    let config = DeploymentConfig::parse(&params).expect("invalid deployment config");
    let known: HashSet<_> = pair_store
        .get_last("pairs")
        .unwrap_or_default()
        .into_iter()
        .collect();
    for (ordinal, key, value) in safety_writes(&config, &changes, &known) {
        store.set(ordinal, key, &value);
    }
}

/// Values to persist from one block, as `(ordinal, key, value)`.
///
/// Produces each known pair's write-helper address under `helper:0x<pair>`, the tag-0 fee word
/// of every address writing that mapping slot under `fee:0x<address>`, and the Engine and
/// treasury held in TesseraSwap slots 0 and 1 under `engine` and `treasury`. Values are raw
/// bytes: 20-byte addresses and 32-byte fee words.
fn safety_writes(
    config: &DeploymentConfig,
    changes: &BlockStorageChanges,
    known: &HashSet<String>,
) -> Vec<(u64, String, Vec<u8>)> {
    let mut writes = vec![];
    for tx in &changes.transactions {
        for w in &tx.writes {
            if *w.key == PAIR_WRITE_HELPER_SLOT && known.contains(&id(&w.address)) {
                writes.push((
                    w.ordinal,
                    format!("helper:{}", id(&w.address)),
                    address(&w.new_value).to_vec(),
                ));
            } else if *w.key == FEE_TAG_ZERO_SLOT {
                // Remember this single candidate slot even before helper assignment.
                // Otherwise an already-configured helper could be adopted with a nonzero
                // tag-0 fee and look safe. No other candidate storage/code is indexed.
                writes.push((w.ordinal, format!("fee:{}", id(&w.address)), w.new_value.clone()));
            } else if *w.address == config.tesseraswap && *w.key == TESSERASWAP_ENGINE_SLOT {
                // An Engine replacement crosses the configured discovery and state-indexing
                // boundary; enforce_pauses evaluates it.
                writes.push((w.ordinal, "engine".to_string(), address(&w.new_value).to_vec()));
            } else if *w.address == config.tesseraswap && *w.key == TESSERASWAP_TREASURY_SLOT {
                writes.push((w.ordinal, "treasury".to_string(), address(&w.new_value).to_vec()));
            }
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::testing::slot,
        config::TEST_CONFIG,
        pb::tessera::v1::{StorageChange, TransactionStorageChanges},
    };

    const PAIR: [u8; 20] = [0x0a; 20];
    const STRANGER: [u8; 20] = [0x0f; 20];
    const HELPER: [u8; 20] = [0x0c; 20];

    fn write(
        address: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
        new_value: impl AsRef<[u8]>,
        ordinal: u64,
    ) -> StorageChange {
        StorageChange {
            address: address.as_ref().to_vec(),
            key: key.as_ref().to_vec(),
            new_value: new_value.as_ref().to_vec(),
            ordinal,
            ..Default::default()
        }
    }

    fn word(addr: &[u8]) -> Vec<u8> {
        let mut w = vec![0; 32];
        w[12..].copy_from_slice(addr);
        w
    }

    fn block(writes: Vec<StorageChange>) -> BlockStorageChanges {
        BlockStorageChanges {
            transactions: vec![TransactionStorageChanges { tx: Some(Default::default()), writes }],
        }
    }

    fn known() -> HashSet<String> {
        HashSet::from([id(PAIR)])
    }

    #[test]
    fn records_helper_only_for_known_pairs() {
        let writes = safety_writes(
            &TEST_CONFIG,
            &block(vec![
                write(PAIR, slot(52), word(&HELPER), 1),
                // Retained upstream as an Engine write, but the Engine is not a pair.
                write(TEST_CONFIG.engine, slot(52), word(&HELPER), 2),
            ]),
            &known(),
        );
        assert_eq!(writes, vec![(1, format!("helper:{}", id(PAIR)), HELPER.to_vec())]);
    }

    #[test]
    fn records_tag_zero_fee_for_any_writer() {
        let fee = slot(10_000);
        let writes = safety_writes(
            &TEST_CONFIG,
            &block(vec![write(STRANGER, FEE_TAG_ZERO_SLOT, fee, 3)]),
            &known(),
        );
        assert_eq!(writes, vec![(3, format!("fee:{}", id(STRANGER)), fee.to_vec())]);
    }

    #[test]
    fn records_engine_only_from_tesseraswap_slot_zero() {
        let engine = [0x23; 20];
        let writes = safety_writes(
            &TEST_CONFIG,
            &block(vec![
                write(TEST_CONFIG.tesseraswap, slot(0), word(&engine), 4),
                write(TEST_CONFIG.engine, slot(0), word(&engine), 5),
                write(PAIR, slot(0), word(&engine), 6),
            ]),
            &known(),
        );
        assert_eq!(writes, vec![(4, "engine".to_string(), engine.to_vec())]);
    }

    #[test]
    fn records_treasury_only_from_tesseraswap_treasury_slot() {
        let treasury = [0x34; 20];
        let writes = safety_writes(
            &TEST_CONFIG,
            &block(vec![
                write(TEST_CONFIG.tesseraswap, slot(1), word(&treasury), 7),
                write(TEST_CONFIG.engine, slot(1), word(&treasury), 8),
            ]),
            &known(),
        );
        assert_eq!(writes, vec![(7, "treasury".to_string(), treasury.to_vec())]);
    }
}
