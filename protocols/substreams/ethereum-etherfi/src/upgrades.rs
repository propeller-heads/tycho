//! Pauses both components when a tracked proxy changes implementation.
//!
//! The slots this package reads were verified against the implementations the manifest records.
//! An upgrade may change swap behavior or repurpose slots that still decode to plausible numbers,
//! so the block that installs a different implementation pauses the components. They
//! stay paused until someone re-verifies the slots and behavior, records the new implementations
//! and re-releases.

use anyhow::{anyhow, Result};
use substreams_ethereum::pb::eth::v2::{Block, StorageChange, TransactionTrace};

use crate::{
    constants::{TrackedProxy, EIP1967_IMPLEMENTATION_POSITION, TRACKED_PROXIES},
    state::InitialState,
    utils::ordered_storage_changes,
};

/// The transactions in `block` that put a tracked proxy behind an implementation other than the
/// recorded one, each listed once.
///
/// All five proxies are EIP-1967: an upgrade is a write to the implementation slot on the proxy
/// itself. A write that lands on the recorded implementation is not an upgrade.
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
/// Uses the final implementation-slot write in execution order for each tracked proxy.
fn upgrades_a_tracked_proxy(tx: &TransactionTrace, initial_state: &InitialState) -> Result<bool> {
    let mut installed: Vec<(&TrackedProxy, [u8; 20])> = Vec::new();
    let is_a_tracked_implementation_slot = |change: &StorageChange| {
        change.key == EIP1967_IMPLEMENTATION_POSITION &&
            TRACKED_PROXIES
                .iter()
                .any(|proxy| change.address == proxy.proxy)
    };
    for change in ordered_storage_changes(tx, is_a_tracked_implementation_slot) {
        let Some(proxy) = TRACKED_PROXIES
            .iter()
            .find(|proxy| change.address == proxy.proxy)
        else {
            continue;
        };
        let address = address_in_word(&change.new_value)?;
        match installed
            .iter_mut()
            .find(|(tracked, _)| tracked.label == proxy.label)
        {
            Some((_, last)) => *last = address,
            None => installed.push((proxy, address)),
        }
    }

    for (proxy, address) in installed {
        if address != initial_state.implementation_of(proxy)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The address in a 32-byte storage word.
fn address_in_word(word: &[u8]) -> Result<[u8; 20]> {
    let (zeroes, address) = split_word(word)?;
    if zeroes.iter().any(|byte| *byte != 0) {
        return Err(anyhow!("implementation slot does not hold an address: {word:02x?}"));
    }
    Ok(address)
}

fn split_word(word: &[u8]) -> Result<([u8; 12], [u8; 20])> {
    let word: [u8; 32] = word
        .try_into()
        .map_err(|_| anyhow!("implementation slot value is {} bytes, not a word", word.len()))?;
    let mut prefix = [0u8; 12];
    let mut address = [0u8; 20];
    prefix.copy_from_slice(&word[..12]);
    address.copy_from_slice(&word[12..]);
    Ok((prefix, address))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use substreams::hex;
    use substreams_ethereum::pb::eth::v2::{Call, StorageChange, TransactionTraceStatus};

    use super::*;
    use crate::{constants::RATE_LIMITER_ADDRESS, state::tests::snapshot};

    /// The rate limiter's implementation at block 25940000.
    pub(crate) const RATE_LIMITER_V1: [u8; 20] = hex!("9ea4d0fd09b628e23b1998f2153e27e5261b1b67");
    pub(crate) const OTHER: [u8; 20] = hex!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    pub(crate) fn initial_state() -> InitialState {
        snapshot()
    }

    fn word(address: [u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(&address);
        word
    }

    /// A write of `implementation` to `proxy`'s EIP-1967 implementation slot.
    pub(crate) fn upgrade_write(proxy: [u8; 20], implementation: [u8; 20]) -> StorageChange {
        StorageChange {
            address: proxy.to_vec(),
            key: EIP1967_IMPLEMENTATION_POSITION.to_vec(),
            new_value: word(implementation),
            ..Default::default()
        }
    }

    pub(crate) fn rate_limiter_upgrade_to(implementation: [u8; 20]) -> StorageChange {
        upgrade_write(RATE_LIMITER_ADDRESS, implementation)
    }

    /// A block whose only successful transaction (index 7) made `storage_changes` in one call.
    pub(crate) fn block_with(storage_changes: Vec<StorageChange>, state_reverted: bool) -> Block {
        Block {
            number: 25_940_000,
            transaction_traces: vec![TransactionTrace {
                index: 7,
                status: TransactionTraceStatus::Succeeded as i32,
                calls: vec![Call { storage_changes, state_reverted, ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::Call;

    use super::{fixtures::*, *};

    #[test]
    fn writing_the_recorded_implementation_is_not_an_upgrade() {
        let block = block_with(vec![rate_limiter_upgrade_to(RATE_LIMITER_V1)], false);
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    #[test]
    fn writing_another_implementation_is_an_upgrade() {
        let block = block_with(vec![rate_limiter_upgrade_to(OTHER)], false);

        let upgrades = detect_upgrades(&block, &initial_state()).expect("detect");

        let [tx] = upgrades.as_slice() else {
            panic!("expected one upgrading transaction, got {}", upgrades.len());
        };
        assert_eq!(tx.index, 7);
    }

    /// Any one of the five proxies moving is enough.
    #[test]
    fn every_tracked_proxy_is_watched() {
        for proxy in TRACKED_PROXIES.iter() {
            let block = block_with(vec![upgrade_write(proxy.proxy, OTHER)], false);
            let upgrades = detect_upgrades(&block, &initial_state()).expect("detect");
            assert_eq!(upgrades.len(), 1, "{} was not watched", proxy.label);
        }
    }

    /// A transaction upgrading several tracked proxies is listed once.
    #[test]
    fn a_transaction_upgrading_several_proxies_is_listed_once() {
        let block = block_with(
            TRACKED_PROXIES
                .iter()
                .map(|proxy| upgrade_write(proxy.proxy, OTHER))
                .collect(),
            false,
        );
        assert_eq!(
            detect_upgrades(&block, &initial_state())
                .expect("detect")
                .len(),
            1
        );
    }

    /// An unrelated address does not affect either component.
    #[test]
    fn an_untracked_proxy_is_ignored() {
        let block = block_with(vec![upgrade_write([0x42; 20], OTHER)], false);
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    /// The same word written to any other slot on a tracked proxy is ordinary state.
    #[test]
    fn another_slot_on_a_tracked_proxy_is_ignored() {
        let mut change = rate_limiter_upgrade_to(OTHER);
        change.key = vec![0u8; 32];
        let block = block_with(vec![change], false);
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    /// A transaction that writes another implementation and then puts the recorded one back
    /// ends with the layout the slots were verified against, so it is not an upgrade.
    #[test]
    fn a_transaction_that_restores_the_recorded_implementation_is_not_an_upgrade() {
        let block = block_with(
            vec![rate_limiter_upgrade_to(OTHER), rate_limiter_upgrade_to(RATE_LIMITER_V1)],
            false,
        );
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    /// The other way round is an upgrade: the transaction ends on the other implementation.
    #[test]
    fn a_transaction_that_ends_on_another_implementation_is_an_upgrade() {
        let block = block_with(
            vec![rate_limiter_upgrade_to(RATE_LIMITER_V1), rate_limiter_upgrade_to(OTHER)],
            false,
        );
        assert_eq!(
            detect_upgrades(&block, &initial_state())
                .expect("detect")
                .len(),
            1
        );
    }

    #[test]
    fn a_reverted_call_installs_nothing() {
        let block = block_with(vec![rate_limiter_upgrade_to(OTHER)], true);
        assert!(detect_upgrades(&block, &initial_state())
            .expect("detect")
            .is_empty());
    }

    #[test]
    fn a_slot_value_that_is_not_an_address_is_an_error() {
        let mut change = rate_limiter_upgrade_to(OTHER);
        change.new_value = vec![1u8; 32];
        let block = block_with(vec![change], false);
        assert!(detect_upgrades(&block, &initial_state()).is_err());
    }

    /// A parent call that writes the slot, calls into a child that writes it again, and then
    /// writes it once more resumes with the final value. Whichever implementation that last
    /// write installs is the one the transaction ends on.
    #[test]
    fn nested_calls_use_the_last_executed_write() {
        for (parent_implementation, child_implementation, expected_upgrades) in
            [(OTHER, RATE_LIMITER_V1, 1), (RATE_LIMITER_V1, OTHER, 0)]
        {
            let mut parent_write = rate_limiter_upgrade_to(parent_implementation);
            parent_write.ordinal = 30;
            let mut child_write = rate_limiter_upgrade_to(child_implementation);
            child_write.ordinal = 20;
            let mut block = block_with(vec![parent_write], false);
            block.transaction_traces[0]
                .calls
                .push(Call {
                    index: 1,
                    parent_index: 0,
                    storage_changes: vec![child_write],
                    ..Default::default()
                });

            let upgrades = detect_upgrades(&block, &initial_state()).expect("detect");

            assert_eq!(
                upgrades.len(),
                expected_upgrades,
                "parent ends on {parent_implementation:02x?}"
            );
        }
    }
}
