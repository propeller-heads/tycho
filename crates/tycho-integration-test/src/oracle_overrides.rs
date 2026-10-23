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
    titan::{default_providers, TitanChannel},
    OverrideSnapshot, StateOverrideProvider,
};

/// The channels of Metric's two Ethereum oracles. A Metric pool on Ethereum only fills after
/// Metric's oracle update for its block, so a Metric swap needs both channels for that block,
/// whether it reaches the pool through Titan's price level stream or through Metric's API.
pub const METRIC_ORACLE_CHANNELS: &[TitanChannel] =
    &[TitanChannel::MetricPythOracle, TitanChannel::MetricCompressedOracle];

/// The Titan state override channels each price level stream venue needs; every channel listed
/// here is collected. A venue is covered at a block only when all of its channels published for
/// it. A venue missing from this table (auto-detected ones, and the named venues in the tests'
/// `VENUES_WITHOUT_CHANNELS`) simulates on the indexed state.
const VENUE_CHANNELS: [(&str, &[TitanChannel]); 4] = [
    ("fermiswap", &[TitanChannel::FermiSwap]),
    ("kipseli", &[TitanChannel::Kipseli]),
    ("bebop", &[TitanChannel::BopAmm]),
    ("metric", METRIC_ORACLE_CHANNELS),
];

/// The override channels a price level stream venue needs, or `None` when Titan's state override
/// stream carries none for it.
///
/// `venue` is the bare name a pAMM component is served under (`fermiswap`, or a `0x…` address for
/// an auto-detected venue). Auto-detected venues are never covered: the price level stream keys
/// them by their router address, which is not the key the override stream publishes under.
pub fn override_channels(venue: &str) -> Option<&'static [TitanChannel]> {
    VENUE_CHANNELS
        .iter()
        .find(|(name, _)| *name == venue)
        .map(|(_, channels)| *channels)
}

/// How many quoted blocks are kept.
const RETAINED_BLOCKS: usize = 8;

/// Storage overrides keyed by contract address, then by storage slot.
type Storage = HashMap<Address, HashMap<U256, U256>>;

/// The latest snapshot of every channel that published for one block.
///
/// A Titan frame carries a venue's whole `stateOverride` for the block it targets, so a channel's
/// newest snapshot replaces its previous one rather than accumulating with it. Channels are kept
/// apart because they publish independently, and are merged only when the block is read.
type BlockEntry = HashMap<TitanChannel, Arc<Storage>>;

/// The overrides of the most recent quoted blocks, newest at the back.
type Blocks = Arc<RwLock<VecDeque<(u64, BlockEntry)>>>;

/// pAMM oracle overrides per quoted block.
pub struct OracleOverrides {
    blocks: Blocks,
}

/// What was collected for one quoted block.
///
/// Carries the channels that published alongside their merged slots, because a swap is only
/// priced by its own venue when that venue's channels are among them.
pub struct BlockOverrides {
    channels: HashSet<TitanChannel>,
    storage: AddressHashMap<B256HashMap<B256>>,
}

impl BlockOverrides {
    /// Whether every one of `channels` published overrides for this block.
    pub fn covers_all(&self, channels: &[TitanChannel]) -> bool {
        channels
            .iter()
            .all(|channel| self.channels.contains(channel))
    }

    /// The slots to apply, merged across every channel that published.
    pub fn into_storage(self) -> AddressHashMap<B256HashMap<B256>> {
        self.storage
    }
}

/// Opens the one Titan state override connection this process uses, mapped under the key of every
/// channel it serves.
///
/// The same providers back the indexed pAMM pools (through
/// `ProtocolStreamBuilder::with_override_provider`) and [`OracleOverrides`], so both read one
/// socket and one parse loop instead of opening one each and disagreeing about the current frame.
pub fn titan_providers() -> HashMap<String, Arc<dyn StateOverrideProvider>> {
    default_providers(subscribed_channels().map(|channel| channel.key().to_string()))
}

/// Every channel collected from Titan's state override stream.
fn subscribed_channels() -> impl Iterator<Item = TitanChannel> {
    VENUE_CHANNELS
        .iter()
        .flat_map(|(_, channels)| channels.iter().copied())
}

impl OracleOverrides {
    /// Collects Titan's state overrides in the background. `None` when `providers` serves no pAMM
    /// channel.
    pub fn spawn(providers: &HashMap<String, Arc<dyn StateOverrideProvider>>) -> Option<Self> {
        let mut receivers = Vec::new();
        for channel in subscribed_channels() {
            let key = channel.key();
            match providers
                .get(key)
                .and_then(|provider| provider.subscribe(key))
            {
                Some(receiver) => receivers.push((channel, receiver)),
                None => warn!(channel = key, "Titan's state override stream serves no channel"),
            }
        }
        if receivers.is_empty() {
            return None;
        }

        let blocks: Blocks = Arc::new(RwLock::new(VecDeque::new()));
        for (channel, receiver) in receivers {
            tokio::spawn(collect(channel, receiver, blocks.clone()));
        }
        info!("Collecting pAMM oracle overrides from Titan's state override stream");
        Some(Self { blocks })
    }

    /// The overrides Titan published for `block`, or `None` when none were collected for it.
    ///
    /// Every channel that published for the block contributes its latest snapshot; a slot written
    /// by two channels takes an arbitrary one of the two values.
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
                channels: entry.keys().copied().collect(),
                storage: slot_overrides(entry),
            })
    }
}

/// Records every snapshot of one channel.
async fn collect(
    channel: TitanChannel,
    mut receiver: watch::Receiver<OverrideSnapshot>,
    blocks: Blocks,
) {
    loop {
        if receiver.changed().await.is_err() {
            warn!(
                channel = channel.key(),
                "Titan override channel closed; stopping oracle override collection"
            );
            return;
        }
        let (block_number, storage) = {
            let snapshot = receiver.borrow_and_update();
            (snapshot.block_number, snapshot.storage.clone())
        };
        let Some(block_number) = block_number else {
            debug!(
                channel = channel.key(),
                "Titan override snapshot carries no block number; skipping"
            );
            continue;
        };
        if storage.is_empty() {
            continue;
        }
        record(&blocks, channel, block_number, storage);
    }
}

/// Stores `storage` as `channel`'s snapshot of `block`, replacing the one it published before and
/// evicting the oldest block past [`RETAINED_BLOCKS`].
fn record(blocks: &Blocks, channel: TitanChannel, block: u64, storage: Arc<Storage>) {
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
        entry.insert(channel, storage);
        return;
    }
    blocks.push_back((block, BlockEntry::from([(channel, storage)])));
    while blocks.len() > RETAINED_BLOCKS {
        blocks.pop_front();
    }
}

/// Converts one block's collected storage into the `B256` slots the execution simulation takes,
/// merged across the channels that published for it.
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
    use tycho_simulation::price_level_stream::config::default_served_pamms;

    use super::*;

    const FERMISWAP: TitanChannel = TitanChannel::FermiSwap;
    const BOPAMM: TitanChannel = TitanChannel::BopAmm;

    /// The named price level stream venues Titan's state override stream carries no channel for.
    const VENUES_WITHOUT_CHANNELS: [&str; 5] =
        ["taurusfi", "tempest", "goliath", "stelaxis", "elzorro"];

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

    #[test]
    fn the_metric_venue_needs_both_metric_oracles() {
        assert_eq!(override_channels("metric"), Some(METRIC_ORACLE_CHANNELS));
        assert_eq!(override_channels("fermiswap"), Some(&[TitanChannel::FermiSwap][..]));
    }

    /// A venue the price level stream starts serving has to be placed in [`VENUE_CHANNELS`] or in
    /// [`VENUES_WITHOUT_CHANNELS`], so it cannot fall back to the indexed state unnoticed; and
    /// neither list may name a venue the stream does not serve.
    #[test]
    fn every_served_venue_is_classified_once() {
        let served: HashSet<String> = default_served_pamms()
            .into_iter()
            .map(|pamm| pamm.protocol)
            .collect();
        for venue in &served {
            let with_channels = override_channels(venue).is_some();
            let without_channels = VENUES_WITHOUT_CHANNELS.contains(&venue.as_str());
            assert!(
                with_channels != without_channels,
                "venue {venue} must be in exactly one of VENUE_CHANNELS and VENUES_WITHOUT_CHANNELS"
            );
        }
        let classified = VENUE_CHANNELS
            .iter()
            .map(|(venue, _)| *venue)
            .chain(VENUES_WITHOUT_CHANNELS);
        for venue in classified {
            assert!(
                served.contains(venue),
                "venue {venue} is not served by the price level stream"
            );
        }
    }

    #[test]
    fn a_venue_titan_does_not_serve_has_no_channels() {
        assert_eq!(override_channels("tempest"), None);
        assert_eq!(override_channels("0x5979458912f80b96d30d4220af8e2e4925a33320"), None);
    }

    #[test]
    fn a_block_covers_only_the_protocols_that_published() {
        let overrides = overrides();
        record(&overrides.blocks, FERMISWAP, 100, storage(REGISTRY, &[(1, 11)]));

        let block = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert!(block.covers_all(&[FERMISWAP]));
        assert!(!block.covers_all(&[BOPAMM]));
    }

    #[test]
    fn metric_needs_both_oracles_for_a_block() {
        let overrides = overrides();
        record(
            &overrides.blocks,
            TitanChannel::MetricPythOracle,
            100,
            storage(REGISTRY, &[(1, 11)]),
        );
        let one_oracle = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert!(!one_oracle.covers_all(METRIC_ORACLE_CHANNELS));

        record(
            &overrides.blocks,
            TitanChannel::MetricCompressedOracle,
            100,
            storage(VENUE, &[(1, 22)]),
        );
        let both_oracles = overrides
            .for_block(100)
            .expect("block 100 was recorded");
        assert!(both_oracles.covers_all(METRIC_ORACLE_CHANNELS));
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
