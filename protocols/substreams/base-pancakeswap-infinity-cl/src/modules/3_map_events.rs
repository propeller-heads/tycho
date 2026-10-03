use crate::{
    abi::cl_pool_manager::events::{Initialize, ModifyLiquidity, ProtocolFeeUpdated, Swap},
    pb::pancakeswap::infinity::cl::{
        events::{
            pool_event::{self, Type},
            PoolEvent,
        },
        Events, Pool,
    },
};
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;

/// Decodes CLPoolManager logs of known pools into `Events`, sorted by log ordinal.
///
/// Ported from `ethereum-uniswap-v4/shared/src/modules/3_map_events.rs`. `Swap.fee` is the
/// combined swap fee, protocol share included, so it is kept for `map_balance_changes` to net out
/// and `Swap.protocolFee` is dropped:
/// [SwapState](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-cl/libraries/CLPool.sol#L162-L163),
/// [swap](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-cl/libraries/CLPool.sol#L236).
/// `DynamicLPFeeUpdated` is ignored because dynamic-fee pools are filtered at creation, and
/// `Donate` because it moves fee growth, not price or liquidity.
#[substreams::handlers::map]
pub fn map_events(
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<Events, anyhow::Error> {
    let mut pool_manager_events = block
        .transaction_traces
        .into_iter()
        .filter(|tx| tx.status == 1)
        .flat_map(|tx| {
            let receipt = tx
                .receipt
                .as_ref()
                .expect("all transaction traces have a receipt");

            receipt
                .logs
                .iter()
                .filter_map(|log| log_to_event(log, &tx, &pools_store))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    pool_manager_events.sort_unstable_by_key(|e| e.log_ordinal);

    Ok(Events { pool_events: pool_manager_events })
}

/// Resolves the pool the log belongs to and wraps the decoded payload.
///
/// The store lookup drops unknown pools, which is also what keeps other managers' events out: a
/// pool id hashes its `poolManager`, so a Bin pool id is never in this store.
fn log_to_event(
    log: &Log,
    tx: &TransactionTrace,
    pools_store: &StoreGetProto<Pool>,
) -> Option<PoolEvent> {
    let (pool_id, event) = decode_log(log)?;
    let pool = pools_store.get_last(format!("pool:{pool_id}"))?;

    Some(PoolEvent {
        log_ordinal: log.ordinal,
        pool_id,
        currency0: pool.currency0.to_hex(),
        currency1: pool.currency1.to_hex(),
        transaction: Some(tx.into()),
        r#type: Some(event),
    })
}

/// Pool id and payload of a CLPoolManager log, `None` for any other log.
///
/// `Initialize` is decoded here as well as in `1_map_pool_created` because the current tick and
/// sqrt price stores are seeded from it.
fn decode_log(log: &Log) -> Option<(String, Type)> {
    if let Some(init) = Initialize::match_and_decode(log) {
        Some((
            init.id.to_vec().to_hex(),
            Type::Initialize(pool_event::Initialize {
                sqrt_price_x96: init.sqrt_price_x96.to_string(),
                tick: init.tick.into(),
                fee: init.fee.into(),
                tick_spacing: crate::parameters::tick_spacing(&init.parameters),
                hooks: init.hooks.to_vec().to_hex(),
                parameters: init.parameters.to_vec(),
            }),
        ))
    } else if let Some(swap) = Swap::match_and_decode(log) {
        Some((
            swap.id.to_vec().to_hex(),
            Type::Swap(pool_event::Swap {
                sender: swap.sender.to_hex(),
                amount0: swap.amount0.to_string(),
                amount1: swap.amount1.to_string(),
                sqrt_price_x96: swap.sqrt_price_x96.to_string(),
                liquidity: swap.liquidity.to_string(),
                tick: swap.tick.into(),
                fee: swap.fee.into(),
            }),
        ))
    } else if let Some(modify_liquidity) = ModifyLiquidity::match_and_decode(log) {
        Some((
            modify_liquidity.id.to_vec().to_hex(),
            Type::ModifyLiquidity(pool_event::ModifyLiquidity {
                sender: modify_liquidity.sender.to_hex(),
                tick_lower: modify_liquidity.tick_lower.into(),
                tick_upper: modify_liquidity.tick_upper.into(),
                liquidity_delta: modify_liquidity
                    .liquidity_delta
                    .to_string(),
                salt: modify_liquidity.salt.to_vec().to_hex(),
            }),
        ))
    } else if let Some(fee_updated) = ProtocolFeeUpdated::match_and_decode(log) {
        let pool_id = fee_updated.id.to_vec().to_hex();
        Some((
            pool_id.clone(),
            Type::ProtocolFeeUpdated(pool_event::ProtocolFeeUpdated {
                pool_id,
                protocol_fee: fee_updated.protocol_fee.into(),
            }),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tiny_keccak::{Hasher, Keccak};

    const POOL_ID: [u8; 32] = [0xab; 32];
    const SENDER: [u8; 20] = [0x11; 20];

    fn topic(signature: &str) -> Vec<u8> {
        let mut hasher = Keccak::v256();
        hasher.update(signature.as_bytes());
        let mut out = [0u8; 32];
        hasher.finalize(&mut out);
        out.to_vec()
    }

    fn word(value: u128) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[16..].copy_from_slice(&value.to_be_bytes());
        out
    }

    /// Two's complement of `-magnitude` in a full word, as the ABI sign-extends signed ints.
    fn negative_word(magnitude: u8) -> Vec<u8> {
        let mut out = vec![0xffu8; 32];
        out[31] = magnitude.wrapping_neg();
        out
    }

    fn padded(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 32 - bytes.len()];
        out.extend_from_slice(bytes);
        out
    }

    fn log(signature: &str, indexed: &[Vec<u8>], data: Vec<Vec<u8>>) -> Log {
        let mut topics = vec![topic(signature)];
        topics.extend(indexed.iter().cloned());

        Log {
            address: Vec::new(),
            topics,
            data: data.concat(),
            index: 0,
            block_index: 0,
            ordinal: 7,
        }
    }

    /// `parameters` carries the tick spacing in bits 16..40, and `tick` is a signed int24 in a
    /// full word.
    #[test]
    fn initialize_carries_tick_spacing_and_signed_tick() {
        let log = log(
            "Initialize(bytes32,address,address,address,uint24,bytes32,uint160,int24)",
            &[POOL_ID.to_vec(), padded(&SENDER), padded(&[0x22; 20])],
            vec![word(0), word(500), word(60 << 16), word(1 << 96), negative_word(5)],
        );

        let (pool_id, event) = decode_log(&log).expect("Initialize decodes");

        assert_eq!(pool_id, POOL_ID.to_vec().to_hex());
        let Type::Initialize(init) = event else { panic!("wrong event type") };
        assert_eq!((init.fee, init.tick_spacing, init.tick), (500, 60, -5));
        assert_eq!(init.sqrt_price_x96, "79228162514264337593543950336");
    }

    /// `amount0` and `amount1` are int128, so a sold side must survive as a negative string.
    #[test]
    fn swap_keeps_signed_amounts() {
        let log = log(
            "Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24,uint16)",
            &[POOL_ID.to_vec(), padded(&SENDER)],
            vec![
                word(1_000),
                negative_word(2),
                word(1 << 96),
                word(9_000),
                negative_word(1),
                word(500),
                word(0),
            ],
        );

        let (_, event) = decode_log(&log).expect("Swap decodes");

        let Type::Swap(swap) = event else { panic!("wrong event type") };
        assert_eq!((swap.amount0.as_str(), swap.amount1.as_str()), ("1000", "-2"));
        assert_eq!((swap.liquidity.as_str(), swap.tick, swap.fee), ("9000", -1, 500));
    }

    /// A burn is a negative `liquidityDelta`, and ticks below zero are common.
    #[test]
    fn modify_liquidity_keeps_signed_ticks_and_delta() {
        let log = log(
            "ModifyLiquidity(bytes32,address,int24,int24,int256,bytes32)",
            &[POOL_ID.to_vec(), padded(&SENDER)],
            vec![negative_word(120), word(60), negative_word(7), word(0)],
        );

        let (_, event) = decode_log(&log).expect("ModifyLiquidity decodes");

        let Type::ModifyLiquidity(modify) = event else { panic!("wrong event type") };
        assert_eq!((modify.tick_lower, modify.tick_upper), (-120, 60));
        assert_eq!(modify.liquidity_delta, "-7");
    }

    #[test]
    fn protocol_fee_updated_carries_the_packed_fee() {
        let log =
            log("ProtocolFeeUpdated(bytes32,uint24)", &[POOL_ID.to_vec()], vec![word(0x2002)]);

        let (pool_id, event) = decode_log(&log).expect("ProtocolFeeUpdated decodes");

        assert_eq!(pool_id, POOL_ID.to_vec().to_hex());
        let Type::ProtocolFeeUpdated(updated) = event else { panic!("wrong event type") };
        assert_eq!((updated.pool_id, updated.protocol_fee), (pool_id, 0x2002));
    }

    /// Only the four CLPoolManager events decode; everything else in a block is skipped.
    #[rstest]
    #[case::wrong_signature("Transfer(address,address,uint256)")]
    #[case::bin_pool_swap("Swap(bytes32,address,int128,int128,uint24,uint24,uint16)")]
    #[case::donate("Donate(bytes32,address,uint256,uint256,int24)")]
    fn unrelated_logs_are_skipped(#[case] signature: &str) {
        let log = log(signature, &[POOL_ID.to_vec(), padded(&SENDER)], vec![word(0)]);

        assert!(decode_log(&log).is_none(), "{signature} must not decode as a CL event");
    }
}
