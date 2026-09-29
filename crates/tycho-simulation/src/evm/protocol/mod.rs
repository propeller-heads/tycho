pub mod aerodrome_slipstreams;
pub mod aerodrome_v1;
pub mod balancer_v3;
mod clmm;
pub mod cowamm;
mod cpmm;
pub mod curve;
pub mod ekubo;
pub mod ekubo_v3;
pub mod erc4626;
pub mod etherfi;
pub mod filters;
pub mod fluid;
pub mod lido_v4;
pub mod lunarbase;
pub mod native_wrapper;
pub mod pancakeswap_v2;
pub mod ramses_v3;
pub mod ring_swap_v2;
pub mod rocketpool;
pub mod safe_math;
pub mod sky;
pub mod u256_num;
pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod uniswap_v4;
pub mod utils;
pub mod velodrome_slipstreams;
pub mod vm;
#[cfg(test)]
mod test_utils {
    use std::collections::HashMap;

    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};

    use crate::protocol::models::TryFromWithBlock;

    pub(super) async fn try_decode_snapshot_with_defaults<
        T: TryFromWithBlock<ComponentWithState, BlockHeader>,
    >(
        snapshot: ComponentWithState,
    ) -> Result<T, T::Error> {
        T::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &HashMap::default(),
            &Default::default(),
        )
        .await
    }

    /// Live chain access for parity tests that compare a hybrid state, read through the VM from
    /// an RPC-backed `SimulationDB` (the storage the indexer would hold), with the protocol's own
    /// on-chain quote at the same block. Reads `RPC_URL`.
    pub(crate) mod parity {
        use std::{
            sync::Arc,
            time::{SystemTime, UNIX_EPOCH},
        };

        use alloy::{
            eips::BlockNumberOrTag,
            primitives::{Address, Bytes as AlloyBytes, U256},
            providers::Provider,
            rpc::types::TransactionRequest,
        };
        use tokio::runtime::Runtime;
        use tycho_client::feed::BlockHeader;

        use crate::evm::{
            engine_db::{
                simulation_db::{EVMProvider, SimulationDB},
                utils::{get_client, get_runtime},
            },
            simulation::SimulationEngine,
        };

        pub(crate) struct LiveChain {
            client: Arc<EVMProvider>,
            runtime: Arc<Runtime>,
            seed: u64,
        }

        impl LiveChain {
            pub(crate) fn connect() -> Self {
                let seed = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos() as u64 |
                    1;
                Self {
                    client: get_client(None).expect("RPC_URL"),
                    runtime: get_runtime()
                        .expect("runtime")
                        .expect("runtime"),
                    seed,
                }
            }

            /// Pseudo-random value from a per-run xorshift seed; printed failures carry the block
            /// and amount, which is what a rerun needs.
            pub(crate) fn next_random(&mut self) -> u64 {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 7;
                self.seed ^= self.seed << 17;
                self.seed
            }

            /// `count` distinct random block headers among the latest `lookback` blocks.
            pub(crate) fn random_recent_blocks(
                &mut self,
                count: usize,
                lookback: u64,
            ) -> Vec<BlockHeader> {
                let latest = self
                    .runtime
                    .block_on(self.client.get_block_number())
                    .expect("block number");
                assert!(count as u64 <= lookback, "cannot draw {count} blocks from {lookback}");
                let mut numbers = Vec::new();
                while numbers.len() < count {
                    let number = latest.saturating_sub(1 + self.next_random() % lookback);
                    if !numbers.contains(&number) {
                        numbers.push(number);
                    }
                }
                numbers
                    .into_iter()
                    .map(|number| {
                        let block = self
                            .runtime
                            .block_on(async {
                                self.client
                                    .get_block_by_number(BlockNumberOrTag::Number(number))
                                    .await
                            })
                            .expect("block")
                            .expect("block exists");
                        BlockHeader {
                            number,
                            hash: block.header.hash.0.into(),
                            timestamp: block.header.timestamp,
                            ..Default::default()
                        }
                    })
                    .collect()
            }

            /// A random exact-in swap `(i, j, amount_in)` between two distinct tokens, sized at
            /// 0.01% to 5% of the input reserve; `None` when that rounds to zero.
            pub(crate) fn random_swap(
                &mut self,
                reserves: &[U256],
            ) -> Option<(usize, usize, U256)> {
                let n = reserves.len() as u64;
                let i = (self.next_random() % n) as usize;
                let j = ((i as u64 + 1 + self.next_random() % (n - 1)) % n) as usize;
                let bps = 1 + self.next_random() % 500;
                let amount_in = reserves[i] * U256::from(bps) / U256::from(10_000);
                (!amount_in.is_zero()).then_some((i, j, amount_in))
            }

            /// A VM engine whose storage is fetched from the chain as of `block`.
            pub(crate) fn engine_at(
                &self,
                block: &BlockHeader,
            ) -> SimulationEngine<SimulationDB<EVMProvider>> {
                let mut db =
                    SimulationDB::new(self.client.clone(), Some(self.runtime.clone()), None);
                db.set_block(Some(block.clone()));
                SimulationEngine::new(db, false)
            }

            /// `eth_call` from the zero address at `block`, or `None` if it reverts.
            pub(crate) fn eth_call(
                &self,
                to: Address,
                data: Vec<u8>,
                block: u64,
            ) -> Option<Vec<u8>> {
                let tx = TransactionRequest::default()
                    .from(Address::ZERO)
                    .to(to)
                    .input(AlloyBytes::from(data).into());
                self.runtime
                    .block_on(async {
                        self.client
                            .call(tx)
                            .block(block.into())
                            .await
                    })
                    .ok()
                    .map(|out| out.to_vec())
            }
        }
    }
}
