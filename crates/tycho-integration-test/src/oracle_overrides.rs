//! Finds the registry slots to override for a pAMM quote, per block.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, RwLock},
};

use alloy::primitives::{
    map::{AddressHashMap, B256HashMap},
    Address, B256, U256,
};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use tycho_simulation::evm::override_stream::{
    titan::default_providers, OverrideSnapshot, StateOverrideProvider,
};

/// The pAMM protocol systems collected from the stream — every one Titan's state override stream
/// serves. A price level stream venue outside this set (Metric, TaurusFi, auto-detected ones)
/// simulates on the indexed state; see [`override_protocol`].
const OVERRIDE_STREAM_PROTOCOLS: [&str; 3] = ["vm:fermiswap", "vm:kipseli", "vm:bopamm"];

/// The protocol system whose overrides a price level stream venue needs, or `None` when Titan's
/// state override stream carries no channel for it.
///
/// `venue` is the bare name a pAMM component is served under (`fermiswap`, or a `0x…` address for
/// an auto-detected venue). Auto-detected venues are never covered: the price level stream keys
/// them by their router address, which is not the key the override stream publishes under.
pub fn override_protocol(venue: &str) -> Option<&'static str> {
    match venue {
        "fermiswap" => Some("vm:fermiswap"),
        "kipseli" => Some("vm:kipseli"),
        "bebop" => Some("vm:bopamm"),
        _ => None,
    }
}

/// How many quoted blocks are kept.
const RETAINED_BLOCKS: usize = 8;

/// Storage overrides keyed by contract address, then by storage slot.
type Storage = HashMap<Address, HashMap<U256, U256>>;

/// The latest snapshot of every protocol that published for one block.
///
/// A Titan frame carries a venue's whole `stateOverride` for the block it targets, so a protocol's
/// newest snapshot replaces its previous one rather than accumulating with it. Protocols are kept
/// apart because they publish independently, and are merged only when the block is read.
type BlockEntry = HashMap<&'static str, Arc<Storage>>;

/// The overrides of the most recent quoted blocks, newest at the back.
type Blocks = Arc<RwLock<VecDeque<(u64, BlockEntry)>>>;

/// pAMM oracle overrides per quoted block.
pub struct OracleOverrides {
    blocks: Blocks,
}

/// What was collected for one quoted block.
///
/// Carries the protocols that published alongside their merged slots, because a swap is only
/// priced by its own venue when that venue's protocol is among them.
pub struct BlockOverrides {
    protocols: HashSet<&'static str>,
    storage: AddressHashMap<B256HashMap<B256>>,
}

impl BlockOverrides {
    /// Whether `protocol` published overrides for this block.
    pub fn covers(&self, protocol: &str) -> bool {
        self.protocols.contains(protocol)
    }

    /// The slots to apply, merged across every protocol that published.
    pub fn into_storage(self) -> AddressHashMap<B256HashMap<B256>> {
        self.storage
    }
}

/// Opens the one Titan state override connection this process uses, mapped under every protocol
/// system it serves.
///
/// The same providers back the indexed pAMM pools (through
/// `ProtocolStreamBuilder::with_override_provider`) and [`OracleOverrides`], so both read one
/// socket and one parse loop instead of opening one each and disagreeing about the current frame.
pub fn titan_providers() -> HashMap<String, Arc<dyn StateOverrideProvider>> {
    default_providers(
        OVERRIDE_STREAM_PROTOCOLS
            .iter()
            .map(|protocol| protocol.to_string()),
    )
}

impl OracleOverrides {
    /// Collects Titan's state overrides in the background. `None` when `providers` serves no pAMM
    /// channel.
    pub fn spawn(providers: &HashMap<String, Arc<dyn StateOverrideProvider>>) -> Option<Self> {
        let mut receivers = Vec::new();
        for protocol in OVERRIDE_STREAM_PROTOCOLS {
            match providers
                .get(protocol)
                .and_then(|provider| provider.subscribe(protocol))
            {
                Some(receiver) => receivers.push((protocol, receiver)),
                None => warn!(protocol, "Titan's state override stream serves no channel"),
            }
        }
        if receivers.is_empty() {
            return None;
        }

        let blocks: Blocks = Arc::new(RwLock::new(VecDeque::new()));
        for (protocol, receiver) in receivers {
            tokio::spawn(collect(protocol, receiver, blocks.clone()));
        }
        info!("Collecting pAMM oracle overrides from Titan's state override stream");
        Some(Self { blocks })
    }

    /// The overrides Titan published for `block`, or `None` when none were collected for it.
    ///
    /// Every protocol that published for the block contributes its latest snapshot; a slot written
    /// by two protocols takes an arbitrary one of the two values.
    pub fn for_block(&self, block: u64) -> Option<BlockOverrides> {
        let blocks = match self.blocks.read() {
            Ok(blocks) => blocks,
            Err(e) => {
                warn!("Failed to acquire read lock on pAMM oracle overrides: {e}");
                return None;
            }
        };
        blocks
            .iter()
            .find(|(number, _)| *number == block)
            .map(|(_, entry)| BlockOverrides {
                protocols: entry.keys().copied().collect(),
                storage: slot_overrides(entry),
            })
    }
}

/// Records every snapshot of one protocol's channel.
async fn collect(
    protocol: &'static str,
    mut receiver: watch::Receiver<OverrideSnapshot>,
    blocks: Blocks,
) {
    loop {
        if receiver.changed().await.is_err() {
            warn!(protocol, "Titan override channel closed; stopping oracle override collection");
            return;
        }
        let (block_number, storage) = {
            let snapshot = receiver.borrow_and_update();
            (snapshot.block_number, snapshot.storage.clone())
        };
        let Some(block_number) = block_number else {
            debug!(protocol, "Titan override snapshot carries no block number; skipping");
            continue;
        };
        if storage.is_empty() {
            continue;
        }
        record(&blocks, protocol, block_number, storage);
    }
}

/// Stores `storage` as `protocol`'s snapshot of `block`, replacing the one it published before and
/// evicting the oldest block past [`RETAINED_BLOCKS`].
fn record(blocks: &Blocks, protocol: &'static str, block: u64, storage: Arc<Storage>) {
    let mut blocks = match blocks.write() {
        Ok(blocks) => blocks,
        Err(e) => {
            warn!("Failed to acquire write lock on pAMM oracle overrides: {e}");
            return;
        }
    };
    if let Some((_, entry)) = blocks
        .iter_mut()
        .find(|(number, _)| *number == block)
    {
        entry.insert(protocol, storage);
        return;
    }
    blocks.push_back((block, BlockEntry::from([(protocol, storage)])));
    while blocks.len() > RETAINED_BLOCKS {
        blocks.pop_front();
    }
}

/// Converts one block's collected storage into the `B256` slots the execution simulation takes,
/// merged across the protocols that published for it.
fn slot_overrides(entry: &BlockEntry) -> AddressHashMap<B256HashMap<B256>> {
    let mut overrides: AddressHashMap<B256HashMap<B256>> = AddressHashMap::default();
    for storage in entry.values() {
        for (account, slots) in storage.iter() {
            let account_slots = overrides.entry(*account).or_default();
            for (slot, value) in slots {
                account_slots.insert(
                    B256::from(slot.to_be_bytes::<32>()),
                    B256::from(value.to_be_bytes::<32>()),
                );
            }
        }
    }
    overrides
}

#[cfg(test)]
mod tests {
    use super::*;

    const FERMISWAP: &str = "vm:fermiswap";
    const BOPAMM: &str = "vm:bopamm";

    const REGISTRY: Address =
        alloy::primitives::address!("da7afeed01fe625cf15d187a19f94b45f00b8c5f");
    const VENUE: Address = alloy::primitives::address!("160141a205f5ddcf096ba3f48b7ed21eb52c62ea");

    fn storage(account: Address, slots: &[(u64, u64)]) -> Arc<Storage> {
        let slots = slots
            .iter()
            .map(|(slot, value)| (U256::from(*slot), U256::from(*value)))
            .collect();
        Arc::new(HashMap::from([(account, slots)]))
    }

    fn overrides() -> OracleOverrides {
        OracleOverrides { blocks: Arc::new(RwLock::new(VecDeque::new())) }
    }

    fn slot_value(overrides: &BlockOverrides, account: Address, slot: u64) -> Option<B256> {
        overrides
            .storage
            .get(&account)?
            .get(&B256::from(U256::from(slot).to_be_bytes::<32>()))
            .copied()
    }

    /// Every protocol in [`OVERRIDE_STREAM_PROTOCOLS`] must be reachable from a venue name, or its
    /// subscription collects overrides no swap ever claims.
    #[test]
    fn every_subscribed_protocol_is_reachable_from_a_venue() {
        let reachable: HashSet<&str> = ["fermiswap", "kipseli", "bebop"]
            .iter()
            .filter_map(|venue| override_protocol(venue))
            .collect();
        assert_eq!(reachable, HashSet::from_iter(OVERRIDE_STREAM_PROTOCOLS));
    }

    #[test]
    fn a_venue_titan_does_not_serve_has_no_protocol() {
        assert_eq!(override_protocol("metric"), None);
        assert_eq!(override_protocol("0x5979458912f80b96d30d4220af8e2e4925a33320"), None);
    }

    #[test]
    fn a_block_covers_only_the_protocols_that_published() {
        let overrides = overrides();
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11)]));

        let block = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert!(block.covers(FERMISWAP));
        assert!(!block.covers(BOPAMM));
    }

    #[test]
    fn several_protocols_write_one_block() {
        let overrides = overrides();
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11), (2, 22)]));
        record(&overrides.blocks, BOPAMM, 100, storage(VENUE, &[(1, 33)]));

        let block = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert_eq!(slot_value(&block, REGISTRY, 1), Some(B256::from(U256::from(11))));
        assert_eq!(slot_value(&block, REGISTRY, 2), Some(B256::from(U256::from(22))));
        assert_eq!(slot_value(&block, VENUE, 1), Some(B256::from(U256::from(33))));
    }

    /// A frame carries the venue's whole override set, so a slot the newest frame no longer
    /// publishes must not survive from an earlier one.
    #[test]
    fn a_later_snapshot_replaces_the_protocols_previous_one() {
        let overrides = overrides();
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11), (2, 22)]));
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 99)]));

        let block = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert_eq!(slot_value(&block, REGISTRY, 1), Some(B256::from(U256::from(99))));
        assert_eq!(slot_value(&block, REGISTRY, 2), None);
    }

    /// One protocol replacing its snapshot must leave another protocol's slots for the same block
    /// intact.
    #[test]
    fn a_replaced_snapshot_keeps_another_protocols_slots() {
        let overrides = overrides();
        record(&overrides.blocks, BOPAMM, 100, storage(VENUE, &[(1, 33)]));
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11)]));
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 99)]));

        let block = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert_eq!(slot_value(&block, REGISTRY, 1), Some(B256::from(U256::from(99))));
        assert_eq!(slot_value(&block, VENUE, 1), Some(B256::from(U256::from(33))));
    }

    #[test]
    fn two_blocks_write_the_same_slot() {
        let overrides = overrides();
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11)]));
        record(&overrides.blocks, FERMISWAP, 101, storage(REGISTRY, &[(1, 22)]));

        assert_eq!(
            slot_value(
                &overrides
                    .for_block(100)
                    .expect("recorded"),
                REGISTRY,
                1
            ),
            Some(B256::from(U256::from(11)))
        );
        assert_eq!(
            slot_value(
                &overrides
                    .for_block(101)
                    .expect("recorded"),
                REGISTRY,
                1
            ),
            Some(B256::from(U256::from(22)))
        );
    }

    #[test]
    fn a_block_that_was_never_recorded() {
        let overrides = overrides();
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11)]));

        assert!(overrides.for_block(102).is_none());
    }

    #[test]
    fn more_blocks_than_the_cache_holds() {
        let overrides = overrides();
        for block in 0..=RETAINED_BLOCKS as u64 {
            record(&overrides.blocks, FERMISWAP, block, storage(REGISTRY, &[(1, block)]));
        }

        assert!(overrides.for_block(0).is_none());
        assert!(overrides
            .for_block(RETAINED_BLOCKS as u64)
            .is_some());
        assert_eq!(
            overrides
                .blocks
                .read()
                .expect("lock")
                .len(),
            RETAINED_BLOCKS
        );
    }
}
