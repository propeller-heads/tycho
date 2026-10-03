use crate::{
    pb::pancakeswap::infinity::cl::{
        events::{pool_event, PoolEvent},
        Events,
    },
    uni_math::calculate_token_amounts,
};
use anyhow::Ok;
use std::str::FromStr;
use substreams::{
    prelude::StoreGet,
    scalar::BigInt,
    store::{StoreAddBigInt, StoreGetBigInt, StoreNew},
};
use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas, Transaction};

/// Per-pool balance deltas from events. The Vault holds every pool's funds, so a pool's balance
/// is only knowable from its events, as in v4: `ModifyLiquidity` amounts are recomputed with
/// `uni_math::calculate_token_amounts` at the current sqrt price, `Swap` deltas are net of the
/// swap fee on the input side, and `Donate` is excluded.
///
/// Ported unchanged from `ethereum-uniswap-v4/shared/src/modules/5_map_store_balance_changes.rs`.
#[substreams::handlers::map]
pub fn map_balance_changes(
    events: Events,
    pools_current_sqrt_price_store: StoreGetBigInt,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let balance_deltas = events
        .pool_events
        .into_iter()
        .filter(PoolEvent::can_introduce_balance_changes)
        .map(|e| {
            (
                pools_current_sqrt_price_store
                    .get_at(e.log_ordinal, format!("pool:{0}", e.pool_id))
                    .unwrap_or(BigInt::zero()),
                e,
            )
        })
        .filter_map(|(current_sqrt_price, event)| {
            event_to_balance_deltas(current_sqrt_price, event)
        })
        .flatten()
        .collect();

    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}

fn event_to_balance_deltas(
    current_sqrt_price: BigInt,
    event: PoolEvent,
) -> Option<Vec<BalanceDelta>> {
    let (delta0, delta1) = match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(e) => {
            get_amount_delta(current_sqrt_price, e.tick_lower, e.tick_upper, &e.liquidity_delta)
        }
        // The event reports amounts from the swapper's side, so the pool's delta is the negation.
        pool_event::Type::Swap(e) => (
            net_of_swap_fee(
                BigInt::from_str(&e.amount0)
                    .unwrap()
                    .neg(),
                e.fee,
            ),
            net_of_swap_fee(
                BigInt::from_str(&e.amount1)
                    .unwrap()
                    .neg(),
                e.fee,
            ),
        ),
        pool_event::Type::Initialize(_) |
        pool_event::Type::Donate(_) |
        pool_event::Type::ProtocolFeeUpdated(_) => return None,
    };

    let component_id = event.pool_id.as_bytes().to_vec();
    let tx: Option<Transaction> = event
        .transaction
        .as_ref()
        .map(Into::into);
    Some(
        [(&event.currency0, delta0), (&event.currency1, delta1)]
            .into_iter()
            .map(|(token, delta)| BalanceDelta {
                token: hex::decode(token.trim_start_matches("0x")).unwrap(),
                delta: delta.to_signed_bytes_be(),
                component_id: component_id.clone(),
                ord: event.log_ordinal,
                tx: tx.clone(),
            })
            .collect(),
    )
}

/// Takes the swap fee off the token the pool received. Collected fees are not liquidity a swap
/// can use, so they stay out of the component balance and of TVL. Rounds to the nearest unit.
fn net_of_swap_fee(delta: BigInt, fee_pips: u32) -> BigInt {
    if delta <= BigInt::zero() {
        return delta;
    }
    let (quotient, remainder) = (delta.clone() * fee_pips).div_rem(&BigInt::from(1_000_000));
    delta - if remainder >= BigInt::from(500_000) { quotient + 1u32 } else { quotient }
}

impl PoolEvent {
    fn can_introduce_balance_changes(&self) -> bool {
        matches!(
            self.r#type.as_ref().unwrap(),
            pool_event::Type::ModifyLiquidity(_) | pool_event::Type::Swap(_)
        )
    }
}

fn get_amount_delta(
    current_sqrt_price: BigInt,
    tick_lower: i32,
    tick_upper: i32,
    liquidity_delta: &str,
) -> (BigInt, BigInt) {
    // The contract emits an int256 that fits int128, so the parse cannot fail.
    let liquidity_delta: i128 = liquidity_delta
        .parse()
        .expect("Failed to parse liquidity delta");

    let (amount0, amount1) =
        calculate_token_amounts(current_sqrt_price.into(), tick_lower, tick_upper, liquidity_delta)
            .expect("Failed to calculate token amounts from liquidity delta");
    (BigInt::from(amount0), BigInt::from(amount1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const POOL_ID: &str = "0xabababababababababababababababababababababababababababababababab";
    const TOKEN0: [u8; 20] = [0x11; 20];
    const TOKEN1: [u8; 20] = [0x22; 20];

    fn swap_event(amount0: &str, amount1: &str, fee: u32) -> PoolEvent {
        PoolEvent {
            log_ordinal: 7,
            pool_id: POOL_ID.to_string(),
            currency0: format!("0x{}", hex::encode(TOKEN0)),
            currency1: format!("0x{}", hex::encode(TOKEN1)),
            transaction: None,
            r#type: Some(pool_event::Type::Swap(pool_event::Swap {
                amount0: amount0.to_string(),
                amount1: amount1.to_string(),
                fee,
                ..Default::default()
            })),
        }
    }

    /// Exact-in 2_000_000 at a 500 pip swap fee. `computeSwapStep` swaps
    /// `2_000_000 * (1e6 - 500) / 1e6 = 1_999_000` and keeps the remaining 1_000 as the fee, and
    /// the output side leaves the pool in full:
    /// [SwapMath](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-cl/libraries/SwapMath.sol#L63-L83).
    #[rstest]
    #[case::zero_for_one("-2000000", "1998000", 1_999_000, -1_998_000)]
    #[case::one_for_zero("1998000", "-2000000", -1_998_000, 1_999_000)]
    fn swap_deltas_are_net_of_the_fee_on_the_input_side(
        #[case] amount0: &str,
        #[case] amount1: &str,
        #[case] expected0: i64,
        #[case] expected1: i64,
    ) {
        let deltas = event_to_balance_deltas(BigInt::zero(), swap_event(amount0, amount1, 500))
            .expect("a swap moves balances");

        assert_eq!(deltas.len(), 2, "one balance delta per token");
        assert_eq!(deltas[0].token, TOKEN0.to_vec());
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[0].delta), BigInt::from(expected0));
        assert_eq!(deltas[1].token, TOKEN1.to_vec());
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[1].delta), BigInt::from(expected1));
        assert!(
            deltas
                .iter()
                .all(|delta| delta.ord == 7 && delta.component_id == POOL_ID.as_bytes()),
            "deltas carry the log's ordinal and the 0x-hex pool id as component id"
        );
    }

    #[test]
    fn events_without_token_movement_yield_no_deltas() {
        let mut event = swap_event("0", "0", 0);
        event.r#type = Some(pool_event::Type::ProtocolFeeUpdated(Default::default()));

        assert!(
            event_to_balance_deltas(BigInt::zero(), event).is_none(),
            "ProtocolFeeUpdated moves no tokens"
        );
    }
}
