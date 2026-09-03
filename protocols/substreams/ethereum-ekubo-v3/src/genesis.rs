//! Seeded start of the package.
//!
//! Every build embeds `seed.bin`, a snapshot of every pool at one block written by the package's
//! seed writer (`../seed`). At that block `map_events` replaces the real events with one synthetic
//! transaction carrying a `PoolSnapshot` per pool, so every store module rebuilds its state from
//! the snapshot. The seed describes the state *after* the block, which is why the real events of
//! the seed block are dropped rather than applied on top.
//!
//! The committed `seed.bin` is an empty seed pinned to the block before the protocol's first pool,
//! a block the stock manifest never streams, so the package behaves as if it had no seed. A seeded
//! deployment replaces the file and moves the manifest's `initialBlock` to the seed block.

use alloy_primitives::Uint;
use ekubo_sdk::chain::evm::float_sqrt_ratio_to_fixed;
use prost::Message as _;
use substreams_ethereum::pb::eth;
use tycho_substreams::seed::Seed as SeedFile;

use crate::{
    addresses::CORE_ADDRESS,
    pb::ekubo::{
        block_transaction_events::{
            transaction_events::{pool_log::Event, PoolLog},
            TransactionEvents,
        },
        BlockTransactionEvents, Seed, Transaction,
    },
};

/// A `tycho-seed-format` file whose body is an `ekubo.Seed` protobuf.
static SEED_BYTES: &[u8] = include_bytes!("../seed.bin");

/// The block whose events the embedded seed replaces.
///
/// Panics when `seed.bin` is empty, truncated or written for another package; a package without a
/// valid seed is a broken build, not an unseeded one.
pub fn seed_block_number() -> u64 {
    seed_file().header.block_number
}

/// The synthetic events for the seed block, built from the embedded seed.
///
/// Panics when the streamed block is not the block the seed was taken from.
pub fn genesis_events(block: &eth::v2::Block) -> BlockTransactionEvents {
    let file = seed_file();
    file.assert_block(block);
    let seed = Seed::decode(file.body).expect("seed body to be a valid `ekubo.Seed` protobuf");

    genesis_events_from_seed(
        seed,
        file.transaction(block, CORE_ADDRESS.as_slice())
            .into(),
        block,
    )
}

fn seed_file() -> SeedFile<'static> {
    SeedFile::parse(SEED_BYTES, env!("CARGO_PKG_NAME"))
}

impl From<tycho_substreams::models::Transaction> for Transaction {
    fn from(tx: tycho_substreams::models::Transaction) -> Self {
        Self { hash: tx.hash, from: tx.from, to: tx.to, index: tx.index }
    }
}

/// One transaction of `PoolSnapshot` events, one per seeded pool, in seed order. Ordinals are
/// strictly increasing because the balance store rejects repeated ordinals per component and
/// token.
fn genesis_events_from_seed(
    seed: Seed,
    transaction: Transaction,
    block: &eth::v2::Block,
) -> BlockTransactionEvents {
    let pool_logs = seed
        .pools
        .into_iter()
        .enumerate()
        .map(|(i, pool)| {
            let mut snapshot = pool
                .snapshot
                .expect("pool seed to carry a snapshot");
            snapshot.sqrt_ratio =
                float_sqrt_ratio_to_fixed(Uint::from_be_slice(&pool.sqrt_ratio_float))
                    .to_be_bytes_trimmed_vec();

            PoolLog {
                ordinal: i as u64 + 1,
                pool_id: pool.pool_id,
                event: Some(Event::PoolSnapshot(snapshot)),
            }
        })
        .collect();

    BlockTransactionEvents {
        block_transaction_events: vec![TransactionEvents {
            transaction: Some(transaction),
            pool_logs,
        }],
        timestamp: block
            .header
            .as_ref()
            .unwrap()
            .timestamp
            .as_ref()
            .unwrap()
            .seconds
            .try_into()
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::uint;
    use substreams_ethereum::pb::eth::v2::{Block, BlockHeader, TransactionTrace};

    use super::*;
    use crate::pb::ekubo::{
        block_transaction_events::transaction_events::pool_log::{PoolSnapshot, TimedSnapshot},
        PoolSeed,
    };

    const BLOCK_NUMBER: u64 = 25_000_000;

    fn block(tx_count: usize) -> Block {
        Block {
            number: BLOCK_NUMBER,
            hash: vec![9; 32],
            header: Some(BlockHeader {
                timestamp: Some(prost_types::Timestamp { seconds: 1_700_000_000, nanos: 0 }),
                ..Default::default()
            }),
            transaction_traces: vec![TransactionTrace::default(); tx_count],
            ..Default::default()
        }
    }

    fn seed() -> Seed {
        Seed {
            pools: vec![
                PoolSeed {
                    pool_id: vec![1; 32],
                    sqrt_ratio_float: uint!(0x3ffffffffffffffff9ba1f6d_U96)
                        .to_be_bytes::<12>()
                        .to_vec(),
                    snapshot: Some(PoolSnapshot { tick: 7, ..Default::default() }),
                },
                PoolSeed {
                    pool_id: vec![2; 32],
                    sqrt_ratio_float: vec![0; 12],
                    snapshot: Some(PoolSnapshot {
                        timed: Some(TimedSnapshot { last_time: 42, ..Default::default() }),
                        ..Default::default()
                    }),
                },
            ],
        }
    }

    fn transaction() -> Transaction {
        Transaction {
            hash: vec![7; 32],
            from: CORE_ADDRESS.to_vec(),
            to: CORE_ADDRESS.to_vec(),
            index: 3,
        }
    }

    #[test]
    fn genesis_events_carry_the_transaction_and_one_log_per_pool() {
        let events = genesis_events_from_seed(seed(), transaction(), &block(3));

        assert_eq!(events.timestamp, 1_700_000_000);
        assert_eq!(events.block_transaction_events.len(), 1);

        let tx_events = &events.block_transaction_events[0];
        assert_eq!(tx_events.transaction, Some(transaction()));

        let ordinals: Vec<_> = tx_events
            .pool_logs
            .iter()
            .map(|log| log.ordinal)
            .collect();
        assert_eq!(ordinals, vec![1, 2]);
        assert_eq!(tx_events.pool_logs[0].pool_id, vec![1; 32]);
        assert_eq!(tx_events.pool_logs[1].pool_id, vec![2; 32]);
    }

    #[test]
    fn genesis_events_convert_sqrt_ratio_and_keep_timed_state() {
        let events = genesis_events_from_seed(seed(), transaction(), &block(0));
        let logs = &events.block_transaction_events[0].pool_logs;

        let Some(Event::PoolSnapshot(first)) = &logs[0].event else {
            panic!("expected a pool snapshot");
        };
        assert_eq!(first.tick, 7);
        assert_eq!(
            first.sqrt_ratio,
            float_sqrt_ratio_to_fixed(uint!(0x3ffffffffffffffff9ba1f6d_U96))
                .to_be_bytes_trimmed_vec()
        );
        assert!(first.timed.is_none());

        let Some(Event::PoolSnapshot(second)) = &logs[1].event else {
            panic!("expected a pool snapshot");
        };
        assert_eq!(second.timed.as_ref().unwrap().last_time, 42);
    }
}
