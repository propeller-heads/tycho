use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures::StreamExt;
use miette::miette;
use rand::prelude::IteratorRandom;
use tokio::{sync::mpsc::Sender, task::JoinHandle};
use tracing::{info, warn};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_simulation::{
    price_level_stream::stream::PriceLevelStreamBuilder,
    protocol::models::{ProtocolComponent, Update},
};

use crate::{
    metrics,
    stream_processor::{StreamUpdate, UpdateType},
};

/// Streams Titan pAMM price level updates and forwards throttled, sampled [`StreamUpdate`]s.
pub struct PriceLevelStreamProcessor {
    chain: Chain,
    sample_size: usize,
    /// One sampled update is emitted per this many blocks. Titan pushes several snapshots per
    /// second, far more than is useful for validation.
    block_interval: u64,
    /// Mark the served venues stale when no Titan message arrives within this window (zero
    /// disables the watchdog). Lives here rather than on the emitted updates because throttling
    /// hides most messages from the consumer.
    stale_threshold: Duration,
}

impl PriceLevelStreamProcessor {
    /// Creates the processor, or `None` on chains the Titan price level stream does not serve
    /// (it only serves Ethereum).
    pub fn new(
        chain: Chain,
        sample_size: usize,
        block_interval: u64,
        stale_threshold: Duration,
    ) -> Option<Self> {
        if chain != Chain::Ethereum {
            return None;
        }
        Some(Self { chain, sample_size, block_interval, stale_threshold })
    }

    pub async fn run_stream(
        &self,
        all_tokens: &HashMap<Bytes, Token>,
        stream_tx: Sender<miette::Result<StreamUpdate>>,
    ) -> miette::Result<JoinHandle<()>> {
        info!("Starting price level stream processor for chain {:?}", self.chain);
        // The default venues are served under their names, auto-detection additionally serves
        // any newly streamed pAMM under its address.
        let stream = PriceLevelStreamBuilder::new()
            .with_known_pamms()
            .auto_detect(true)
            .with_tokens(all_tokens.clone())
            .build();

        let mut emitter = SampledEmitter::new(self.sample_size, self.block_interval);
        let stale_threshold = self.stale_threshold;
        let handle = tokio::spawn(async move {
            info!("Price level stream processor started");
            tokio::pin!(stream);
            loop {
                let next = if stale_threshold.is_zero() {
                    stream.next().await
                } else {
                    match tokio::time::timeout(stale_threshold, stream.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            // Titan pushes several snapshots per second, so a silent window this
                            // long means the connection is down. A dead connection emits nothing,
                            // so the watchdog must live here, where every message is seen before
                            // throttling — not with the consumer of the emitted updates.
                            emitter.mark_stale();
                            continue;
                        }
                    }
                };
                let Some(update) = next else { break };
                if let Some(result) = emitter.handle(update) {
                    if stream_tx.send(result).await.is_err() {
                        warn!("Receiver dropped, stopping stream processor");
                        break;
                    }
                }
            }
        });
        Ok(handle)
    }
}

/// Folds raw stream messages into throttled, sampled [`StreamUpdate`]s, separated from the IO
/// loop in [`PriceLevelStreamProcessor::run_stream`] so the hold-back, sampling, and cache logic
/// is unit-testable.
struct SampledEmitter {
    sample_size: usize,
    /// One sampled update is emitted per this many blocks.
    block_interval: u64,
    /// Components seen so far. Updates announce a pair under `new_pairs` only once, so sampled
    /// states of later updates must be re-joined with their components from here.
    components: HashMap<String, ProtocolComponent>,
    /// Pairs removed since the last emission and not re-added since. Removals announced by
    /// skipped messages must survive until the next emitted update.
    removed: HashMap<String, ProtocolComponent>,
    /// Venues currently exported to the pair-count gauge, so a venue whose last pair disappears
    /// drops to zero instead of freezing at its final count.
    gauged_protocols: HashSet<String>,
    is_first_update: bool,
    /// The snapshot chosen for emission, held back until the stream moves past its block: Titan
    /// streams many snapshots per built block, and the block's last one has the least drift to
    /// what the builder finalizes, so the downstream wait for the target block to land on-chain
    /// stays minimal.
    chosen: Option<Update>,
    next_emission_block: u64,
}

impl SampledEmitter {
    fn new(sample_size: usize, block_interval: u64) -> Self {
        Self {
            sample_size,
            block_interval,
            components: HashMap::new(),
            removed: HashMap::new(),
            gauged_protocols: HashSet::new(),
            is_first_update: true,
            chosen: None,
            next_emission_block: 0,
        }
    }

    /// Marks all served venues stale in metrics; called by the watchdog on Titan silence.
    fn mark_stale(&self) {
        for protocol in &self.gauged_protocols {
            metrics::mark_protocol_stale(protocol);
        }
    }

    /// Folds one stream message into the caches and returns the update to emit, if the message
    /// moved the stream past the chosen block.
    fn handle(&mut self, mut update: Update) -> Option<miette::Result<StreamUpdate>> {
        // Receipt is the liveness signal: flip the served venues (back) to Ready.
        for protocol in &self.gauged_protocols {
            metrics::mark_protocol_ready(protocol);
        }

        // Emit before folding the current message into the caches, so the emitted update is a
        // consistent view as of the chosen block's last snapshot.
        let block = update.block_number_or_timestamp;
        let emitted = self
            .chosen
            .take_if(|snapshot| block > snapshot.block_number_or_timestamp)
            .map(|snapshot| self.emit(snapshot));

        // Keep the caches in sync with every message, including skipped ones, so later samples
        // always resolve. A re-added pair is no removal.
        let pairs_changed = !update.new_pairs.is_empty() || !update.removed_pairs.is_empty();
        for id in update.new_pairs.keys() {
            self.removed.remove(id);
        }
        self.components
            .extend(std::mem::take(&mut update.new_pairs));
        for (id, component) in std::mem::take(&mut update.removed_pairs) {
            self.components.remove(&id);
            self.removed.insert(id, component);
        }
        if pairs_changed {
            self.update_pair_count_gauges();
        }

        if let Some(snapshot) = &mut self.chosen {
            // A fresher snapshot of the chosen block supersedes the held one.
            if snapshot.block_number_or_timestamp == block {
                *snapshot = update;
            }
        } else if block >= self.next_emission_block {
            self.next_emission_block = block + self.block_interval;
            self.chosen = Some(update);
        }

        emitted
    }

    /// Turns the chosen snapshot into the emitted update: samples random pair states, attaches
    /// their components under `new_pairs` (where the update processor looks up components of
    /// off-chain streams), and flushes the removals accumulated since the last emission.
    fn emit(&mut self, mut emitted: Update) -> miette::Result<StreamUpdate> {
        emitted.states = emitted
            .states
            .into_iter()
            .choose_multiple(&mut rand::rng(), self.sample_size)
            .into_iter()
            .collect();
        emitted.new_pairs = emitted
            .states
            .keys()
            .filter_map(|id| {
                self.components
                    .get(id)
                    .map(|component| (id.clone(), component.clone()))
            })
            .collect();
        emitted.removed_pairs = std::mem::take(&mut self.removed);

        let received_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| miette!(e).wrap_err("Error getting current timestamp"))?;
        let stream_update = StreamUpdate {
            update_type: UpdateType::PriceLevelStream,
            update: emitted,
            is_first_update: self.is_first_update,
            received_at,
        };
        self.is_first_update = false;
        Ok(stream_update)
    }

    /// Re-exports the per-venue pair-count gauge, dropping venues whose last pair disappeared
    /// to zero instead of freezing them at their final count.
    fn update_pair_count_gauges(&mut self) {
        let mut counts: HashMap<&String, usize> = HashMap::new();
        for component in self.components.values() {
            *counts
                .entry(&component.protocol_system)
                .or_default() += 1;
        }
        for protocol in &self.gauged_protocols {
            if !counts.contains_key(protocol) {
                metrics::record_protocol_pool_count(protocol, 0);
            }
        }
        for (protocol, count) in &counts {
            metrics::record_protocol_pool_count(protocol, *count);
        }
        self.gauged_protocols = counts.into_keys().cloned().collect();
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_common::simulation::protocol_sim::ProtocolSim;
    use tycho_simulation::price_level_stream::state::PriceLevelStreamState;

    use super::*;

    fn component(id: &str) -> ProtocolComponent {
        ProtocolComponent::new(
            Bytes::from(id.as_bytes().to_vec()),
            "pricelevelstream:fermiswap".to_string(),
            "pricelevelstream:fermiswap".to_string(),
            Chain::Ethereum,
            vec![],
            vec![],
            HashMap::new(),
            Bytes::default(),
            Default::default(),
        )
    }

    /// A pair state whose `gas_cost` doubles as a marker to tell snapshots apart.
    fn state(marker: u64) -> Box<dyn ProtocolSim> {
        Box::new(PriceLevelStreamState::new(
            Bytes::zero(20),
            Bytes::zero(20),
            vec![],
            vec![],
            BigUint::from(marker),
        ))
    }

    fn marker(emitted: &StreamUpdate, id: &str) -> u64 {
        let state = emitted.update.states[id]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        u64::try_from(&state.gas_cost).expect("marker fits u64")
    }

    /// A stream message: `states` are `(pair id, marker)`, `new`/`removed` announce pairs.
    fn update(block: u64, states: &[(&str, u64)], new: &[&str], removed: &[&str]) -> Update {
        let states = states
            .iter()
            .map(|(id, marker)| (id.to_string(), state(*marker)))
            .collect();
        let new_pairs = new
            .iter()
            .map(|id| (id.to_string(), component(id)))
            .collect();
        let removed_pairs = removed
            .iter()
            .map(|id| (id.to_string(), component(id)))
            .collect();
        Update::new(block, states, new_pairs)
            .set_is_partial(true)
            .set_removed_pairs(removed_pairs)
    }

    #[test]
    fn emits_the_chosen_blocks_last_snapshot_once_the_stream_moves_past_it() {
        let mut emitter = SampledEmitter::new(10, 1);

        // Two snapshots of the chosen block: held back, the fresher one supersedes.
        assert!(emitter
            .handle(update(100, &[("a", 1)], &["a"], &[]))
            .is_none());
        assert!(emitter
            .handle(update(100, &[("a", 2)], &[], &[]))
            .is_none());

        // The first message of block 101 releases block 100's last snapshot.
        let emitted = emitter
            .handle(update(101, &[("a", 3)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert!(emitted.is_first_update);
        assert_eq!(emitted.update.block_number_or_timestamp, 100);
        assert_eq!(marker(&emitted, "a"), 2);
        // The component was announced two messages ago; the emitted update re-joins it.
        assert!(emitted
            .update
            .new_pairs
            .contains_key("a"));

        let emitted = emitter
            .handle(update(102, &[("a", 4)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert!(!emitted.is_first_update);
        assert_eq!(emitted.update.block_number_or_timestamp, 101);
        assert_eq!(marker(&emitted, "a"), 3);
    }

    #[test]
    fn out_of_order_messages_are_folded_but_never_emitted() {
        let mut emitter = SampledEmitter::new(10, 1);
        emitter.handle(update(100, &[("a", 1)], &["a"], &[]));

        // A message for an OLDER block than the chosen snapshot's: it must not trigger an
        // emission (that requires moving past the chosen block) and must not overwrite the
        // held snapshot (that only happens for the same block) — the emission below still
        // carries block 100's own states. Its component announcement is folded into the cache
        // regardless, since component metadata is block-independent.
        assert!(emitter
            .handle(update(99, &[("a", 9), ("b", 9)], &["b"], &[]))
            .is_none());

        let emitted = emitter
            .handle(update(101, &[("a", 2), ("b", 2)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert_eq!(emitted.update.block_number_or_timestamp, 100);
        assert_eq!(marker(&emitted, "a"), 1);

        // The stale message's announcement of pair b resolves this emission's component lookup.
        let emitted = emitter
            .handle(update(102, &[("a", 3)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert!(emitted
            .update
            .new_pairs
            .contains_key("b"));
    }

    #[test]
    fn block_interval_throttles_emissions() {
        let mut emitter = SampledEmitter::new(10, 2);

        emitter.handle(update(100, &[("a", 1)], &["a"], &[]));
        let emitted = emitter
            .handle(update(101, &[("a", 2)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert_eq!(emitted.update.block_number_or_timestamp, 100);

        // Block 101 fell into the throttle window, so its snapshot is never chosen: block 102's
        // arrival emits nothing, but is chosen itself.
        assert!(emitter
            .handle(update(102, &[("a", 3)], &[], &[]))
            .is_none());
        let emitted = emitter
            .handle(update(103, &[("a", 4)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert_eq!(emitted.update.block_number_or_timestamp, 102);
        assert_eq!(marker(&emitted, "a"), 3);
    }

    #[test]
    fn removals_survive_skipped_messages() {
        let mut emitter = SampledEmitter::new(10, 1);
        emitter.handle(update(100, &[("a", 1), ("b", 1)], &["a", "b"], &[]));
        // A skipped same-block message drops pair b; the removal must reach the next emission.
        emitter.handle(update(100, &[("a", 2)], &[], &["b"]));

        let emitted = emitter
            .handle(update(101, &[("a", 3)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert_eq!(emitted.update.block_number_or_timestamp, 100);
        assert!(emitted
            .update
            .removed_pairs
            .contains_key("b"));
        assert!(!emitted.update.states.contains_key("b"));

        // Flushed removals do not linger into later emissions.
        let emitted = emitter
            .handle(update(102, &[("a", 4)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert!(emitted.update.removed_pairs.is_empty());
    }

    #[test]
    fn a_readded_pair_is_no_removal() {
        let mut emitter = SampledEmitter::new(10, 1);
        emitter.handle(update(100, &[("a", 1), ("b", 1)], &["a", "b"], &[]));
        emitter.handle(update(100, &[("a", 2)], &[], &["b"]));
        emitter.handle(update(100, &[("a", 3), ("b", 3)], &["b"], &[]));

        let emitted = emitter
            .handle(update(101, &[("a", 4), ("b", 4)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert!(emitted.update.removed_pairs.is_empty());
        assert!(emitted.update.states.contains_key("b"));
        assert!(emitted
            .update
            .new_pairs
            .contains_key("b"));
    }

    #[test]
    fn sampling_caps_the_emitted_states() {
        let mut emitter = SampledEmitter::new(2, 1);
        emitter.handle(update(100, &[("a", 1), ("b", 1), ("c", 1)], &["a", "b", "c"], &[]));

        let emitted = emitter
            .handle(update(101, &[("a", 2)], &[], &[]))
            .expect("emission expected")
            .unwrap();
        assert_eq!(emitted.update.states.len(), 2);
        // Exactly the sampled pairs carry their component.
        assert_eq!(
            emitted
                .update
                .new_pairs
                .keys()
                .collect::<HashSet<_>>(),
            emitted
                .update
                .states
                .keys()
                .collect::<HashSet<_>>()
        );
    }
}
