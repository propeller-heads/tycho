//! Pauses the component when a tracked proxy changes implementation.
//!
//! The slots this package reads were verified against the implementations the manifest records.
//! An upgrade may move or repurpose them while the old positions keep decoding to plausible
//! numbers, so the block that installs a different implementation pauses the component. It
//! stays paused until someone re-verifies the slots, records the new implementation and
//! re-releases.

use anyhow::{anyhow, Result};
use substreams_ethereum::pb::eth::v2::{Block, TransactionTrace};

use crate::{
    constants::{
        TrackedProxy, ARAGON_APP_BASES_NAMESPACE, ARAGON_SET_APP_TOPIC, LIDO_KERNEL_ADDRESS,
        TRACKED_PROXIES,
    },
    state::InitialState,
};

/// The transactions in `block` that put a tracked proxy behind an implementation other than the
/// recorded one, each listed once.
///
/// stETH is an Aragon `AppProxyUpgradeable`: the Kernel maps `(APP_BASES_NAMESPACE, appId)` to
/// the implementation and emits `SetApp` when the mapping changes. Only a final implementation
/// different from the recorded one triggers a pause.
pub fn detect_upgrades<'a>(
    block: &'a Block,
    initial_state: &InitialState,
) -> Result<Vec<&'a TransactionTrace>> {
    let mut upgrading = Vec::new();
    for tx in block.transactions() {
        if upgrades_a_tracked_proxy(tx, initial_state)? {
            upgrading.push(tx);
        }
    }
    Ok(upgrading)
}

/// Whether `tx` leaves a tracked proxy behind an implementation other than the recorded one.
///
/// Uses the final `SetApp` for each tracked app within the transaction.
fn upgrades_a_tracked_proxy(tx: &TransactionTrace, initial_state: &InitialState) -> Result<bool> {
    let Some(receipt) = tx.receipt.as_ref() else {
        return Ok(false);
    };
    let mut installed: Vec<(&TrackedProxy, [u8; 20])> = Vec::new();
    for log in &receipt.logs {
        if log.address != LIDO_KERNEL_ADDRESS {
            continue;
        }
        let [topic, namespace, app_id] = log.topics.as_slice() else {
            continue;
        };
        if topic.as_slice() != ARAGON_SET_APP_TOPIC ||
            namespace.as_slice() != ARAGON_APP_BASES_NAMESPACE
        {
            continue;
        }
        for proxy in TRACKED_PROXIES.iter() {
            if app_id.as_slice() != proxy.app_id {
                continue;
            }
            let address = address_in_word(&log.data)?;
            match installed
                .iter_mut()
                .find(|(tracked, _)| tracked.label == proxy.label)
            {
                Some((_, last)) => *last = address,
                None => installed.push((proxy, address)),
            }
        }
    }
    for (proxy, address) in installed {
        if address != initial_state.implementation_of(proxy)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The address in an ABI-encoded 32-byte word.
fn address_in_word(word: &[u8]) -> Result<[u8; 20]> {
    let (zeroes, address) = split_word(word)?;
    if zeroes.iter().any(|byte| *byte != 0) {
        return Err(anyhow!("SetApp data is not an address: {word:02x?}"));
    }
    Ok(address)
}

fn split_word(word: &[u8]) -> Result<([u8; 12], [u8; 20])> {
    let word: [u8; 32] = word
        .try_into()
        .map_err(|_| anyhow!("SetApp data is {} bytes, not a word", word.len()))?;
    let mut prefix = [0u8; 12];
    let mut address = [0u8; 20];
    prefix.copy_from_slice(&word[..12]);
    address.copy_from_slice(&word[12..]);
    Ok((prefix, address))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::collections::HashMap;

    use substreams::hex;
    use substreams_ethereum::pb::eth::v2::{Log, TransactionReceipt, TransactionTraceStatus};

    use super::*;

    /// The implementation the v4 migration installed at block 25603297.
    pub(crate) const V4: [u8; 20] = hex!("028271e30a695c0527a0c50ca30603fed004cdb0");
    pub(crate) const OTHER: [u8; 20] = hex!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    pub(crate) fn initial_state() -> InitialState {
        InitialState {
            start_block: 25_603_297,
            total_and_external_shares: "0x00".to_string(),
            buffered_ether_and_deposited_post_report: "0x00".to_string(),
            cl_validators_balance_and_cl_pending_balance: "0x00".to_string(),
            staking_state: "0x00".to_string(),
            wsteth_shares: "0x00".to_string(),
            creation_tx: "0x00".to_string(),
            implementations: HashMap::from([(
                "steth".to_string(),
                format!("0x{}", hex::encode(V4)),
            )]),
        }
    }

    fn word(address: [u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(&address);
        word
    }

    pub(crate) fn set_app(namespace: [u8; 32], app_id: [u8; 32], app: [u8; 20]) -> Log {
        Log {
            address: LIDO_KERNEL_ADDRESS.to_vec(),
            topics: vec![ARAGON_SET_APP_TOPIC.to_vec(), namespace.to_vec(), app_id.to_vec()],
            data: word(app),
            ..Default::default()
        }
    }

    pub(crate) fn block_with(logs: Vec<Log>, status: TransactionTraceStatus) -> Block {
        Block {
            number: 25_603_297,
            transaction_traces: vec![TransactionTrace {
                index: 7,
                status: status as i32,
                receipt: Some(TransactionReceipt { logs, ..Default::default() }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::TransactionTraceStatus;

    use super::{fixtures::*, *};
    use crate::constants::STETH_APP_ID;

    /// Block 25603297 installs the implementation the snapshot was taken against, so the
    /// migration that starts the package does not pause it.
    #[test]
    fn installing_the_recorded_implementation_is_not_an_upgrade() {
        let block = block_with(
            vec![set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, V4)],
            TransactionTraceStatus::Succeeded,
        );
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    #[test]
    fn installing_another_implementation_is_an_upgrade() {
        let block = block_with(
            vec![set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER)],
            TransactionTraceStatus::Succeeded,
        );

        let upgrades = detect_upgrades(&block, &initial_state()).expect("detect");

        let [tx] = upgrades.as_slice() else {
            panic!("expected one upgrading transaction, got {}", upgrades.len());
        };
        assert_eq!(tx.index, 7);
    }

    /// The Kernel emits `SetApp` for every namespace and every app; only stETH's base
    /// implementation is this package's concern.
    #[test]
    fn other_namespaces_and_apps_are_ignored() {
        let other_namespace = [0x11u8; 32];
        let other_app = [0x22u8; 32];
        let block = block_with(
            vec![
                set_app(other_namespace, STETH_APP_ID, OTHER),
                set_app(ARAGON_APP_BASES_NAMESPACE, other_app, OTHER),
            ],
            TransactionTraceStatus::Succeeded,
        );
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    /// A `SetApp` from a contract that is not the Kernel is not a Lido upgrade.
    #[test]
    fn a_set_app_from_another_contract_is_ignored() {
        let mut log = set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER);
        log.address = OTHER.to_vec();
        let block = block_with(vec![log], TransactionTraceStatus::Succeeded);
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    /// A transaction that installs another implementation and then puts the recorded one back
    /// ends with the layout the slots were verified against, so it is not an upgrade.
    #[test]
    fn a_transaction_that_restores_the_recorded_implementation_is_not_an_upgrade() {
        let block = block_with(
            vec![
                set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER),
                set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, V4),
            ],
            TransactionTraceStatus::Succeeded,
        );
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    /// The other way round is an upgrade: the transaction ends on the other implementation.
    #[test]
    fn a_transaction_that_ends_on_another_implementation_is_an_upgrade() {
        let block = block_with(
            vec![
                set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, V4),
                set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER),
            ],
            TransactionTraceStatus::Succeeded,
        );
        assert_eq!(
            detect_upgrades(&block, &initial_state())
                .expect("detect")
                .len(),
            1
        );
    }

    #[test]
    fn a_reverted_transaction_installs_nothing() {
        let block = block_with(
            vec![set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER)],
            TransactionTraceStatus::Reverted,
        );
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    #[test]
    fn set_app_data_that_is_not_an_address_is_an_error() {
        let mut log = set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER);
        log.data = vec![1u8; 31];
        let block = block_with(vec![log], TransactionTraceStatus::Succeeded);
        assert!(detect_upgrades(&block, &initial_state()).is_err());
    }

    /// A word of the right length whose top twelve bytes are not zero does not hold an address.
    /// Taking its last twenty bytes anyway would read an arbitrary implementation out of it.
    #[test]
    fn set_app_data_wider_than_an_address_is_an_error() {
        let mut log = set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER);
        log.data = vec![0xffu8; 32];
        let block = block_with(vec![log], TransactionTraceStatus::Succeeded);
        assert!(detect_upgrades(&block, &initial_state()).is_err());
    }
}
