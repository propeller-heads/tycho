use substreams::store::{StoreAddBigInt, StoreGet, StoreGetProto, StoreNew};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::{
    balances::extract_balance_deltas_from_tx,
    models::{BalanceDelta, BlockBalanceDeltas},
};

use crate::pb::nami::v4::Pool;

/// Pool balances from the `Transfer` logs that move a pool's own tokens in or out of it.
///
/// Pool events alone miss the community fee: the pool pays it to the community vault with a
/// plain transfer and no event of its own, so a balance summed from Mint, Swap, Collect, Flash
/// and Skim drifts above `balanceOf`. Transfers of any other token to a pool do not count.
#[substreams::handlers::map]
pub fn map_balance_changes(
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> BlockBalanceDeltas {
    let balance_deltas =
        block_balance_deltas(&block, |address| pools_store.get_last(hex::encode(address)));

    BlockBalanceDeltas { balance_deltas }
}

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}

/// Balance deltas of the known pools in the block. `pool_by_address` returns an address's pool
/// when it is one.
fn block_balance_deltas(
    block: &eth::Block,
    pool_by_address: impl Fn(&[u8]) -> Option<Pool>,
) -> Vec<BalanceDelta> {
    block
        .transactions()
        .flat_map(|tx| {
            extract_balance_deltas_from_tx(tx, |token, transactor| {
                pool_by_address(transactor).is_some_and(|pool| {
                    token == pool.token0.as_slice() || token == pool.token1.as_slice()
                })
            })
        })
        // The helper writes the pool's hex address without the `0x` the component ids carry.
        .map(|delta| BalanceDelta {
            component_id: [b"0x", delta.component_id.as_slice()].concat(),
            ..delta
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use substreams::scalar::BigInt;
    use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionTrace};

    use super::*;

    const POOL: [u8; 20] = [0x11; 20];
    const TOKEN0: [u8; 20] = [0x22; 20];
    const TOKEN1: [u8; 20] = [0x33; 20];
    const VAULT: [u8; 20] = [0x44; 20];
    const USER: [u8; 20] = [0x55; 20];
    const OTHER_TOKEN: [u8; 20] = [0x66; 20];

    /// An ERC20 `Transfer(from, to, value)` log emitted by `token`.
    fn transfer(ordinal: u64, token: [u8; 20], from: [u8; 20], to: [u8; 20], value: u64) -> Log {
        let topic = |address: [u8; 20]| [[0u8; 12].as_slice(), &address].concat();
        let mut data = vec![0u8; 24];
        data.extend_from_slice(&value.to_be_bytes());
        Log {
            address: token.to_vec(),
            topics: vec![
                hex::decode("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef")
                    .unwrap(),
                topic(from),
                topic(to),
            ],
            data,
            ordinal,
            ..Default::default()
        }
    }

    /// A block with one successful transaction holding `logs`.
    fn block(logs: Vec<Log>) -> eth::Block {
        eth::Block {
            transaction_traces: vec![TransactionTrace {
                status: 1,
                calls: vec![Call { logs, ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn pool_by_address(address: &[u8]) -> Option<Pool> {
        (address == POOL).then(|| Pool { token0: TOKEN0.to_vec(), token1: TOKEN1.to_vec() })
    }

    fn summary(deltas: &[BalanceDelta]) -> Vec<(u64, Vec<u8>, String)> {
        deltas
            .iter()
            .map(|delta| {
                (
                    delta.ord,
                    delta.token.clone(),
                    BigInt::from_signed_bytes_be(&delta.delta).to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn test_counts_transfers_of_the_pools_tokens_in_both_directions() {
        let block = block(vec![
            transfer(1, TOKEN0, USER, POOL, 1_000),
            transfer(2, TOKEN1, POOL, USER, 900),
            // The community fee leaves the pool without a pool event.
            transfer(3, TOKEN0, POOL, VAULT, 10),
        ]);

        let deltas = block_balance_deltas(&block, pool_by_address);

        assert_eq!(
            summary(&deltas),
            vec![
                (1, TOKEN0.to_vec(), "1000".to_string()),
                (2, TOKEN1.to_vec(), "-900".to_string()),
                (3, TOKEN0.to_vec(), "-10".to_string()),
            ]
        );
        let pool_id = format!("0x{}", hex::encode(POOL)).into_bytes();
        assert!(deltas
            .iter()
            .all(|delta| delta.component_id == pool_id));
    }

    #[test]
    fn test_ignores_other_tokens_and_other_addresses() {
        let block = block(vec![
            transfer(1, OTHER_TOKEN, USER, POOL, 5),
            transfer(2, TOKEN0, USER, VAULT, 5),
        ]);

        assert!(block_balance_deltas(&block, pool_by_address).is_empty());
    }
}
