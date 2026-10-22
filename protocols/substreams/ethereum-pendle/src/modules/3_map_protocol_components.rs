use anyhow::Result;
use std::collections::HashSet;
use substreams::pb::substreams::StoreDeltas;
use tycho_substreams::prelude::*;

use crate::consts::PENDLE_SY;

/// Drops the repeated SY components, leaving one creation per SY.
///
/// A store delta exists only where `store_sy_seen` actually wrote, so the delta keys for this
/// block name the SYs seen for the first time — that rules out an SY re-emitted in a later
/// block. Within this block the delta cannot distinguish two markets sharing an SY, since both
/// carry the same first-seen key, so emitted ids are also tracked as the block is walked.
#[substreams::handlers::map]
pub fn map_protocol_components(
    market_components: BlockTransactionProtocolComponents,
    sy_seen_deltas: StoreDeltas,
) -> Result<BlockTransactionProtocolComponents> {
    Ok(first_creations(market_components, &sy_seen_deltas))
}

/// The components of `market_components` with every SY that is not first seen in this block, or
/// that an earlier component of this block already created, left out. Markets pass unchanged, and
/// a transaction left with no components is dropped.
fn first_creations(
    market_components: BlockTransactionProtocolComponents,
    sy_seen_deltas: &StoreDeltas,
) -> BlockTransactionProtocolComponents {
    let first_seen: HashSet<&str> = sy_seen_deltas
        .deltas
        .iter()
        .map(|delta| delta.key.as_str())
        .collect();

    let mut emitted_sy: HashSet<String> = HashSet::new();
    let mut tx_components = Vec::new();
    for tx in market_components.tx_components {
        let mut components = Vec::new();
        for component in tx.components {
            let is_sy = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_SY);
            if is_sy &&
                (!first_seen.contains(component.id.as_str()) ||
                    !emitted_sy.insert(component.id.clone()))
            {
                continue;
            }
            components.push(component);
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: tx.tx, components });
        }
    }
    BlockTransactionProtocolComponents { tx_components }
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::{store_delta::Operation, StoreDelta};

    use super::*;
    use crate::consts::PENDLE_MARKET;

    /// The wstETH market and its SY, and a second market on the same SY.
    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const OTHER_MARKET: &str = "0x00000000000000000000000000000000000000cc";
    /// An SY first seen in an earlier block.
    const OLD_SY: &str = "0x00000000000000000000000000000000000000b1";

    fn component(id: &str) -> ProtocolComponent {
        let kind = if id == SY || id == OLD_SY { PENDLE_SY } else { PENDLE_MARKET };
        ProtocolComponent::new(id).as_swap_type(kind, ImplementationType::Custom)
    }

    fn block(batches: &[(u64, &[&str])]) -> BlockTransactionProtocolComponents {
        let mut tx_components = Vec::new();
        for (index, ids) in batches {
            tx_components.push(TransactionProtocolComponents {
                tx: Some(Transaction { index: *index, ..Default::default() }),
                components: ids
                    .iter()
                    .map(|id| component(id))
                    .collect(),
            });
        }
        BlockTransactionProtocolComponents { tx_components }
    }

    /// The deltas `store_sy_seen` writes for SYs it sees for the first time.
    fn first_seen(ids: &[&str]) -> StoreDeltas {
        StoreDeltas {
            deltas: ids
                .iter()
                .map(|id| StoreDelta {
                    operation: Operation::Create.into(),
                    ordinal: 0,
                    key: id.to_string(),
                    old_value: vec![],
                    new_value: id.as_bytes().to_vec(),
                })
                .collect(),
        }
    }

    /// Each transaction's index and the ids it creates.
    fn ids(components: &BlockTransactionProtocolComponents) -> Vec<(u64, Vec<String>)> {
        let mut ids = Vec::new();
        for tx in &components.tx_components {
            let created = tx
                .components
                .iter()
                .map(|component| component.id.clone())
                .collect();
            ids.push((tx.tx.as_ref().unwrap().index, created));
        }
        ids
    }

    /// An SY that an earlier block already created is dropped, while the market that re-emitted
    /// it is kept. A transaction left with nothing to create is dropped with it.
    #[test]
    fn an_sy_seen_in_an_earlier_block_is_not_created_again() {
        let block = block(&[(1, &[MARKET, OLD_SY]), (2, &[OLD_SY])]);
        assert_eq!(
            ids(&first_creations(block, &first_seen(&[]))),
            vec![(1, vec![MARKET.to_string()])]
        );
    }

    /// Two markets sharing an SY that is new this block both carry it, and the store delta cannot
    /// tell them apart: the SY is created once, by the first, in a later transaction or the same.
    #[test]
    fn two_markets_sharing_a_new_sy_create_it_once() {
        let across = block(&[(1, &[MARKET, SY]), (2, &[OTHER_MARKET, SY])]);
        assert_eq!(
            ids(&first_creations(across, &first_seen(&[SY]))),
            vec![
                (1, vec![MARKET.to_string(), SY.to_string()]),
                (2, vec![OTHER_MARKET.to_string()]),
            ]
        );

        let within = block(&[(1, &[MARKET, SY, OTHER_MARKET, SY])]);
        assert_eq!(
            ids(&first_creations(within, &first_seen(&[SY]))),
            vec![(1, vec![MARKET.to_string(), SY.to_string(), OTHER_MARKET.to_string()])]
        );
    }

    /// Markets never pass through `store_sy_seen`, so they are kept without a delta.
    #[test]
    fn a_market_is_kept_without_a_first_seen_delta() {
        let block = block(&[(1, &[MARKET]), (2, &[OTHER_MARKET, SY])]);
        assert_eq!(
            ids(&first_creations(block, &first_seen(&[SY]))),
            vec![
                (1, vec![MARKET.to_string()]),
                (2, vec![OTHER_MARKET.to_string(), SY.to_string()]),
            ]
        );
    }
}
