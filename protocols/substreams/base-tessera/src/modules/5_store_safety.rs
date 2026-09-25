use crate::{common::*, config::DeploymentConfig};
use std::collections::HashSet;
use substreams::store::{StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth;

/// Persist fee-tag-0 and epoch signals even when the affected pair has no price update.
#[substreams::handlers::store]
pub fn store_safety(
    params: String,
    block: eth::v2::Block,
    pairs: StoreGetString,
    store: StoreSetString,
) {
    let config = DeploymentConfig::parse(&params).expect("invalid deployment config");
    let known: HashSet<_> = crate::common::pairs(&pairs, "pairs")
        .into_iter()
        .collect();
    for (ordinal, key, value) in safety_writes(&config, &block, &known) {
        store.set(ordinal, key, &value);
    }
}

/// Safety signals to persist from one block, as `(ordinal, key, value)`.
///
/// Produces each known pair's write-helper address under `helper:0x<pair>`, the tag-0 fee word
/// of every address writing that mapping slot under `fee:0x<address>`, and the Engine held in
/// TesseraSwap slot 0 under `engine`. Values are hex without `0x`. Reverted writes are skipped.
fn safety_writes(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    known: &HashSet<String>,
) -> Vec<(u64, String, String)> {
    let fee_key = fee_tag_zero_slot();
    let mut writes = vec![];
    for tx in block.transactions() {
        for w in committed_writes(tx) {
            if known.contains(&id(&w.address)) && w.key == slot(config.pair_write_helper_slot) {
                writes.push((
                    w.ordinal,
                    format!("helper:{}", id(&w.address)),
                    hex::encode(address(&w.new_value)),
                ));
            }
            // Remember this single candidate slot even before helper assignment.
            // Otherwise an already-configured helper could be adopted with a nonzero
            // tag-0 fee and look safe. No other candidate storage/code is indexed.
            if w.key == fee_key.as_slice() {
                writes.push((
                    w.ordinal,
                    format!("fee:{}", id(&w.address)),
                    hex::encode(&w.new_value),
                ));
            }
            if w.address == config.tesseraswap && w.key == slot(0) {
                writes.push((w.ordinal, "engine".to_string(), hex::encode(address(&w.new_value))));
            }
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAIR: [u8; 20] = [0x0a; 20];
    const STRANGER: [u8; 20] = [0x0f; 20];
    const HELPER: [u8; 20] = [0x0c; 20];

    fn config() -> DeploymentConfig {
        DeploymentConfig::parse(&format!(
            "tesseraswap={}&engine={}&treasury={}&treasury_slot=1&pair_map_slot=8\
             &pair_base_token_slot=48&pair_quote_token_slot=49&pair_lib_slot=51\
             &pair_write_helper_slot=52",
            hex::encode([0x11; 20]),
            hex::encode([0x22; 20]),
            hex::encode([0x33; 20]),
        ))
        .unwrap()
    }

    fn write(
        address: &[u8],
        key: Vec<u8>,
        new_value: Vec<u8>,
        ordinal: u64,
    ) -> eth::v2::StorageChange {
        eth::v2::StorageChange {
            address: address.to_vec(),
            key,
            new_value,
            ordinal,
            ..Default::default()
        }
    }

    fn word(addr: &[u8]) -> Vec<u8> {
        let mut w = vec![0; 32];
        w[12..].copy_from_slice(addr);
        w
    }

    fn block(writes: Vec<eth::v2::StorageChange>, reverted: bool) -> eth::v2::Block {
        eth::v2::Block {
            transaction_traces: vec![eth::v2::TransactionTrace {
                status: 1,
                calls: vec![eth::v2::Call {
                    storage_changes: writes,
                    state_reverted: reverted,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn known() -> HashSet<String> {
        HashSet::from([id(&PAIR)])
    }

    #[test]
    fn records_helper_only_for_known_pairs() {
        let writes = safety_writes(
            &config(),
            &block(
                vec![
                    write(&PAIR, slot(52), word(&HELPER), 1),
                    write(&STRANGER, slot(52), word(&HELPER), 2),
                ],
                false,
            ),
            &known(),
        );
        assert_eq!(writes, vec![(1, format!("helper:{}", id(&PAIR)), hex::encode(HELPER))]);
    }

    #[test]
    fn records_tag_zero_fee_for_any_writer() {
        let fee = slot(10_000);
        let writes = safety_writes(
            &config(),
            &block(vec![write(&STRANGER, fee_tag_zero_slot(), fee.clone(), 3)], false),
            &known(),
        );
        assert_eq!(writes, vec![(3, format!("fee:{}", id(&STRANGER)), hex::encode(fee))]);
    }

    #[test]
    fn records_engine_only_from_tesseraswap_slot_zero() {
        let engine = [0x23; 20];
        let writes = safety_writes(
            &config(),
            &block(
                vec![
                    write(&config().tesseraswap, slot(0), word(&engine), 4),
                    write(&STRANGER, slot(0), word(&engine), 5),
                ],
                false,
            ),
            &known(),
        );
        assert_eq!(writes, vec![(4, "engine".to_string(), hex::encode(engine))]);
    }

    #[test]
    fn ignores_reverted_writes() {
        let writes = safety_writes(
            &config(),
            &block(
                vec![
                    write(&PAIR, slot(52), word(&HELPER), 1),
                    write(&config().tesseraswap, slot(0), word(&[0x23; 20]), 2),
                ],
                true,
            ),
            &known(),
        );
        assert!(writes.is_empty());
    }
}
