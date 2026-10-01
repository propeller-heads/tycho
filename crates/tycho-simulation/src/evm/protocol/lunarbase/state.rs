use std::{any::Any, collections::HashMap};

use lunarbase_pmm_math::{
    sqrt_price_x96_to_price, try_simulate_successful_swap, Direction, MathError, PoolParams,
    RollbackReason, SimulationStatus, MAX_U112, MAX_U24, U256,
};
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
        },
    },
    Bytes,
};

use super::decoder::apply_delta;

pub type Address = [u8; 20];
const DEFAULT_GAS: u64 = 180_000;

/// Native simulation of the linear anchor and immediate punishment mechanism.
///
/// Quotes use the fee policy of the extractor's configured `quote_caller`.
/// Active-reserve transitions assume standard token transfers and fully credited
/// fees with sufficient fee-bucket capacity; caller-specific fee accounting and
/// unsynced donations are not indexed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseState {
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub anchor_price_x96: U256,
    pub fee_ask_x24: u32,
    pub fee_bid_x24: u32,
    pub latest_update_block: u64,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub max_punishment_x24: u32,
    pub blacklist_fee_multiplier: U256,
    pub quote_caller_whitelisted: bool,
    pub block_delay: u64,
    pub paused: bool,
    pub head_block: u64,
}

impl LunarBaseState {
    pub fn pool_params(&self) -> PoolParams {
        PoolParams {
            sqrt_price_x96: self.anchor_price_x96,
            fee_ask_x24: self.fee_ask_x24,
            fee_bid_x24: self.fee_bid_x24,
            reserve_x: self.reserve_x,
            reserve_y: self.reserve_y,
            max_punishment_x24: self.max_punishment_x24,
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.head_block <
            self.latest_update_block
                .saturating_add(self.block_delay)
    }

    pub fn fee_multiplier(&self) -> U256 {
        if self.quote_caller_whitelisted || self.blacklist_fee_multiplier.is_zero() {
            U256::from(1u64)
        } else {
            self.blacklist_fee_multiplier
        }
    }

    fn quote_exact_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<(U256, Self), QuoteError> {
        if self.paused {
            return Err(QuoteError::Paused);
        }

        if !self.is_fresh() {
            return Err(QuoteError::Stale {
                block_number: self.head_block,
                latest_update_block: self.latest_update_block,
                block_delay: self.block_delay,
            });
        }

        let direction = if token_in == self.token_x && token_out == self.token_y {
            Direction::XToY
        } else if token_in == self.token_y && token_out == self.token_x {
            Direction::YToX
        } else {
            return Err(QuoteError::InvalidTokenPair);
        };
        let simulation = try_simulate_successful_swap(
            &self.pool_params(),
            amount_in,
            direction,
            self.fee_multiplier(),
        )
        .map_err(QuoteError::Math)?;
        match simulation.status {
            SimulationStatus::Applied => {}
            SimulationStatus::RolledBack(RollbackReason::SwapImpossible) => {
                return Err(QuoteError::Rejected);
            }
            SimulationStatus::RolledBack(RollbackReason::ReserveTransitionOverflow) => {
                return Err(QuoteError::ReserveOverflow);
            }
            SimulationStatus::RolledBack(RollbackReason::LaterRevert) => {
                return Err(QuoteError::Rejected);
            }
        }

        let mut next = self.clone();
        next.reserve_x = simulation.post_swap.reserve_x;
        next.reserve_y = simulation.post_swap.reserve_y;
        next.fee_ask_x24 = simulation.post_swap.fee_ask_x24;
        next.fee_bid_x24 = simulation.post_swap.fee_bid_x24;
        Ok((simulation.quote.amount_out, next))
    }
}

#[typetag::serde]
impl ProtocolSim for LunarBaseState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.pool_params()
            .validate()
            .map_err(|err| map_quote_error(QuoteError::Math(err)))?;
        let token_in = address_from_bytes(base.address.as_ref())?;
        let token_out = address_from_bytes(quote.address.as_ref())?;
        if token_in == self.token_x && token_out == self.token_y {
            return Ok(apply_fee_discount(
                anchor_price(self.anchor_price_x96, base, quote),
                self.fee_bid_x24,
                self.fee_multiplier(),
            ));
        }
        if token_in == self.token_y && token_out == self.token_x {
            if self.anchor_price_x96.is_zero() {
                return Ok(0.0);
            }
            return Ok(apply_fee_discount(
                1.0 / anchor_price(self.anchor_price_x96, quote, base),
                self.fee_ask_x24,
                self.fee_multiplier(),
            ));
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::ZERO {
            return Ok(GetAmountOutResult::new(
                BigUint::ZERO,
                BigUint::from(DEFAULT_GAS),
                Box::new(self.clone()),
            ));
        }

        let (amount_out, next_state) = self
            .quote_exact_in(
                address_from_bytes(token_in.address.as_ref())?,
                address_from_bytes(token_out.address.as_ref())?,
                biguint_to_u256(&amount_in)?,
            )
            .map_err(map_quote_error)?;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(DEFAULT_GAS),
            Box::new(next_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell = address_from_bytes(sell_token.as_ref())?;
        let buy = address_from_bytes(buy_token.as_ref())?;
        if sell == self.token_x && buy == self.token_y {
            return quote_limit(self, sell, buy, initial_quote_limit(self, Direction::XToY));
        }
        if sell == self.token_y && buy == self.token_x {
            return quote_limit(self, sell, buy, initial_quote_limit(self, Direction::YToX));
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(name) = delta.deleted_attributes.iter().next() {
            return Err(TransitionError::DecodeError(format!(
                "LunarBase does not support deleted attributes: {name}"
            )));
        }

        apply_delta(self, delta.updated_attributes)
            .map_err(|err| TransitionError::DecodeError(format!("{err:?}")))?;
        Ok(())
    }

    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let was_fresh = self.is_fresh();
        self.head_block = block.number();
        was_fresh != self.is_fresh()
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QuoteError {
    Paused,
    Stale { block_number: u64, latest_update_block: u64, block_delay: u64 },
    InvalidTokenPair,
    Rejected,
    ReserveOverflow,
    Math(MathError),
}

fn anchor_price(anchor_price_x96: U256, token_in: &Token, token_out: &Token) -> f64 {
    let decimals_adjustment = 10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
    sqrt_price_x96_to_price(anchor_price_x96) * decimals_adjustment
}

fn apply_fee_discount(price: f64, fee_x24: u32, multiplier: U256) -> f64 {
    if fee_x24 == MAX_U24 {
        return 0.0;
    }
    let Some(scaled_fee) = U256::from(fee_x24).checked_mul(multiplier) else {
        return 0.0;
    };
    if scaled_fee >= U256::from(1u32 << 24) {
        return 0.0;
    }
    price * (1.0 - scaled_fee.to::<u32>() as f64 / (1u64 << 24) as f64)
}

// This soft bound mirrors Tycho's CPMM `get_limits` convention:
// https://github.com/propeller-heads/tycho/blob/main/crates/tycho-simulation/src/evm/protocol/cpmm/protocol.rs/#L113
//
// CPMM uses `(sqrt(10) - 1) * reserve_in ~= 2.162 * reserve_in` as the
// amount-in that would produce roughly 90% price impact in a fee-less
// constant-product pool. LunarBase does not treat this as a protocol limit;
// it is only the initial probe for `quote_limit`, which halves the amount
// until the LunarBase quote math accepts it.
fn soft_limit(reserve_in: u128) -> BigUint {
    BigUint::from(reserve_in) * 2162u32 / 1000u32
}

fn initial_quote_limit(state: &LunarBaseState, direction: Direction) -> BigUint {
    let (reserve_in, reserve_out) = match direction {
        Direction::XToY => (state.reserve_x, state.reserve_y),
        Direction::YToX => (state.reserve_y, state.reserve_x),
    };
    if reserve_in != 0 {
        return soft_limit(reserve_in);
    }
    if reserve_out == 0 || state.anchor_price_x96.is_zero() {
        return BigUint::ZERO;
    }

    // A linear pool can accept input into an empty reserve. Value its output
    // inventory in input units, then let quote_limit check rounding and fees.
    let anchor = u256_to_biguint(state.anchor_price_x96);
    let q96 = BigUint::from(1u64) << 96;
    let (numerator, denominator) = match direction {
        Direction::XToY => (q96, anchor),
        Direction::YToX => (anchor, q96),
    };
    let probe = (BigUint::from(reserve_out) * &numerator / &denominator) * numerator / denominator;
    probe
        .max(BigUint::from(1u64))
        .min(BigUint::from(MAX_U112))
}

fn quote_limit(
    state: &LunarBaseState,
    token_in: Address,
    token_out: Address,
    mut amount_in: BigUint,
) -> Result<(BigUint, BigUint), SimulationError> {
    if amount_in == BigUint::ZERO {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }

    loop {
        match state.quote_exact_in(token_in, token_out, biguint_to_u256(&amount_in)?) {
            Ok((amount_out, _)) => return Ok((amount_in, u256_to_biguint(amount_out))),
            Err(QuoteError::Rejected | QuoteError::ReserveOverflow) => {
                amount_in >>= 1;
                if amount_in == BigUint::ZERO {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
            }
            Err(err) => return Err(map_quote_error(err)),
        }
    }
}

fn address_from_bytes(value: &[u8]) -> Result<Address, SimulationError> {
    value.try_into().map_err(|_| {
        SimulationError::InvalidInput(
            format!("expected 20-byte address, got {}", value.len()),
            None,
        )
    })
}

fn biguint_to_u256(value: &BigUint) -> Result<U256, SimulationError> {
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        return Err(SimulationError::InvalidInput("amount_in exceeds uint256".to_owned(), None));
    }
    Ok(U256::from_be_slice(&bytes))
}

fn u256_to_biguint(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

fn map_quote_error(err: QuoteError) -> SimulationError {
    SimulationError::InvalidInput(format!("LunarBase quote rejected: {err:?}"), None)
}

#[cfg(test)]
mod tests {
    use tycho_common::models::Chain;

    use super::*;

    fn addr(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    fn token(address: Address, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from(address.to_vec()),
            symbol,
            decimals,
            100,
            &[Some(100_000)],
            Chain::Base,
            100,
        )
    }

    fn state() -> LunarBaseState {
        LunarBaseState {
            pool: addr(9),
            token_x: addr(1),
            token_y: addr(2),
            anchor_price_x96: U256::from(1u128 << 96),
            fee_ask_x24: 0,
            fee_bid_x24: 0,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 1_000_000,
            max_punishment_x24: 0,
            blacklist_fee_multiplier: U256::from(1u64),
            quote_caller_whitelisted: false,
            block_delay: 2,
            paused: false,
            head_block: 100,
        }
    }

    #[test]
    fn quotes_x_to_y_and_transitions_reserves() {
        let state = state();
        let (amount_out, next_state) = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1_000u64))
            .unwrap();

        assert_eq!(amount_out, U256::from(1_000u64));
        assert_eq!(next_state.reserve_x, 1_001_000);
        assert_eq!(next_state.reserve_y, 999_000);
        assert_eq!(next_state.anchor_price_x96, state.anchor_price_x96);
        assert_eq!(next_state.head_block, state.head_block);
    }

    #[test]
    fn zero_amount_out_returns_zero_without_transition() {
        let state = state();
        let token_x = token(state.token_x, "ETH", 18);
        let token_y = token(state.token_y, "USDC", 6);

        let quote = state
            .get_amount_out(BigUint::ZERO, &token_x, &token_y)
            .unwrap();
        let next_state = quote
            .new_state
            .as_any()
            .downcast_ref::<LunarBaseState>()
            .unwrap();

        assert_eq!(quote.amount, BigUint::ZERO);
        assert_eq!(next_state, &state);
    }

    #[test]
    fn spot_price_uses_anchor_price() {
        let mut state = state();
        state.anchor_price_x96 = lunarbase_pmm_math::price_to_sqrt_price_x96(2_000.0 / 1e12);
        state.reserve_x = 1_000_000_000_000_000_000;
        state.reserve_y = 1_000_000;

        let token_x = token(state.token_x, "ETH", 18);
        let token_y = token(state.token_y, "USDC", 6);

        let price = state
            .spot_price(&token_x, &token_y)
            .unwrap();
        let inverse = state
            .spot_price(&token_y, &token_x)
            .unwrap();

        assert!((price - 2_000.0).abs() < 1e-6);
        assert!((inverse - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn spot_price_applies_directional_base_fee() {
        let mut state = state();
        state.anchor_price_x96 = lunarbase_pmm_math::price_to_sqrt_price_x96(2_000.0 / 1e12);
        state.fee_ask_x24 = (1u32 << 24) / 100;
        state.fee_bid_x24 = (2 * (1u32 << 24)) / 100;

        let token_x = token(state.token_x, "ETH", 18);
        let token_y = token(state.token_y, "USDC", 6);

        let price = state
            .spot_price(&token_x, &token_y)
            .unwrap();
        let inverse = state
            .spot_price(&token_y, &token_x)
            .unwrap();

        assert!((price - (2_000.0 * 0.98)).abs() < 1e-3);
        assert!((inverse - (0.0005 * 0.99)).abs() < 1e-9);
    }

    #[test]
    fn rejects_stale_state() {
        let mut state = state();
        state.head_block = 102;

        let err = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1_000u64))
            .unwrap_err();

        assert_eq!(
            err,
            QuoteError::Stale { block_number: 102, latest_update_block: 100, block_delay: 2 }
        );
    }

    #[test]
    fn charges_immediate_punishment_and_accumulates_only_the_traded_direction() {
        let mut state = state();
        state.max_punishment_x24 = 16_778;
        let amount = U256::from(100_000u64);

        let (first_out, first) = state
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(first_out, U256::from(99_995u64));
        assert_eq!((first.reserve_x, first.reserve_y), (1_100_000, 900_000));
        assert_eq!((first.fee_ask_x24, first.fee_bid_x24), (0, 839));
        assert_eq!(first.anchor_price_x96, state.anchor_price_x96);
        assert_eq!(first.latest_update_block, state.latest_update_block);

        let (second_out, second) = first
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(second_out, U256::from(99_990u64));
        assert_eq!((second.fee_ask_x24, second.fee_bid_x24), (0, 1_678));

        let (reverse_out, reverse) = second
            .quote_exact_in(state.token_y, state.token_x, amount)
            .unwrap();
        assert_eq!(reverse_out, U256::from(99_995u64));
        assert_eq!((reverse.fee_ask_x24, reverse.fee_bid_x24), (839, 1_678));
        assert_eq!((state.fee_ask_x24, state.fee_bid_x24), (0, 0));
    }

    #[test]
    fn operator_update_replaces_accumulated_fees_and_restores_freshness() {
        let mut initial = state();
        initial.max_punishment_x24 = 16_778;
        let (_, mut state) = initial
            .quote_exact_in(initial.token_x, initial.token_y, U256::from(100_000u64))
            .unwrap();
        state.apply_block(&BlockContext::new(102, 0));
        let reserves = (state.reserve_x, state.reserve_y);
        state
            .delta_transition(
                ProtocolStateDelta {
                    updated_attributes: HashMap::from([
                        (
                            "anchor_price_x96".to_owned(),
                            Bytes::from(
                                (U256::from(2u64) << 96usize).to_be_bytes::<32>()[12..].to_vec(),
                            ),
                        ),
                        ("fee_ask_x24".to_owned(), Bytes::from(11u32)),
                        ("fee_bid_x24".to_owned(), Bytes::from(22u32)),
                        ("latest_update_block".to_owned(), Bytes::from(102u64)),
                        ("block_number".to_owned(), Bytes::from(102u64)),
                    ]),
                    ..Default::default()
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        assert!(state.is_fresh());
        assert_eq!((state.fee_ask_x24, state.fee_bid_x24), (11, 22));
        assert_eq!(state.max_punishment_x24, 16_778);
        assert_eq!((state.reserve_x, state.reserve_y), reserves);
    }

    #[test]
    fn full_fee_and_punishment_saturation_reject_without_changing_state() {
        let mut state = state();
        state.fee_bid_x24 = MAX_U24;
        let before = state.clone();
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
        assert_eq!(state, before);
        assert_eq!(
            state
                .spot_price(&token(state.token_x, "X", 18), &token(state.token_y, "Y", 18))
                .unwrap(),
            0.0
        );

        state.fee_bid_x24 = MAX_U24 - 1;
        state.max_punishment_x24 = 1;
        let before = state.clone();
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
        assert_eq!(state, before);
    }

    #[test]
    fn max_punishment_sentinel_uses_conceptual_one_hundred_percent() {
        let mut state = state();
        state.reserve_x = 0;
        state.reserve_y = 1_000_000;
        state.max_punishment_x24 = MAX_U24;
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1_000_000u64)),
            Err(QuoteError::Rejected)
        );
    }

    #[test]
    fn rejects_uint112_reserve_overflow_and_invalid_math_state() {
        let mut state = state();
        state.reserve_x = lunarbase_pmm_math::MAX_U112;
        let before = state.clone();
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::ReserveOverflow)
        );
        assert_eq!(state, before);

        state.max_punishment_x24 = 1 << 24;
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Math(MathError::MaxPunishmentExceedsUint24))
        );
    }

    #[test]
    fn quotes_anchor_above_uint128_and_checks_gross_liquidity() {
        let mut state = state();
        state.anchor_price_x96 = U256::from(1u64) << 129usize;
        state.reserve_y = 1u128 << 80;
        let expected_gross = U256::from(1u64) << 66usize;
        let (out, next) = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1u64))
            .unwrap();
        assert_eq!(out, expected_gross);
        assert_eq!(next.reserve_y, state.reserve_y - (1u128 << 66));
        state.reserve_y = (1u128 << 66) - 1;
        state.fee_bid_x24 = (1 << 24) / 2;
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
    }

    #[test]
    fn execution_block_hook_expires_idle_pools_and_handles_reorgs() {
        let mut state = state();
        assert!(!state.apply_block(&BlockContext::new(101, 0)));
        assert_eq!(state.head_block, 101);
        assert!(state.is_fresh());

        assert!(state.apply_block(&BlockContext::new(102, 0)));
        assert!(!state.is_fresh());
        assert!(!state.apply_block(&BlockContext::new(102, 0)));
        assert!(!state.apply_block(&BlockContext::new(103, 0)));
        assert!(matches!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Stale { block_number: 103, .. })
        ));

        assert!(state.apply_block(&BlockContext::new(101, 0)));
        assert!(state.is_fresh());
    }

    #[test]
    fn zero_anchor_never_returns_infinite_spot_price() {
        let mut state = state();
        state.anchor_price_x96 = U256::ZERO;
        let x = token(state.token_x, "X", 18);
        let y = token(state.token_y, "Y", 18);
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.0);
        assert_eq!(state.spot_price(&y, &x).unwrap(), 0.0);
        assert_eq!(
            state.quote_exact_in(state.token_y, state.token_x, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
    }

    #[test]
    fn limits_allow_both_directions_with_an_empty_input_reserve() {
        let mut state = state();
        state.anchor_price_x96 = U256::from(2u64) << 96usize;
        state.max_punishment_x24 = 16_778;

        state.reserve_x = 0;
        state.reserve_y = 4_000_000;
        let (input, output) = state
            .get_limits(Bytes::from(state.token_x), Bytes::from(state.token_y))
            .unwrap();
        assert_eq!(input, BigUint::from(1_000_000u64));
        assert_eq!(output, BigUint::from(3_996_000u64));

        state.reserve_x = 1_000_000;
        state.reserve_y = 0;
        let (input, output) = state
            .get_limits(Bytes::from(state.token_y), Bytes::from(state.token_x))
            .unwrap();
        assert_eq!(input, BigUint::from(4_000_000u64));
        assert_eq!(output, BigUint::from(999_000u64));
    }

    #[test]
    fn empty_input_reserve_limit_respects_uint112_capacity() {
        let mut state = state();
        state.anchor_price_x96 = U256::from(2u64) << 96usize;
        state.reserve_x = MAX_U112;
        state.reserve_y = 0;
        let (input, output) = state
            .get_limits(Bytes::from(state.token_y), Bytes::from(state.token_x))
            .unwrap();
        assert_eq!(input, BigUint::from(MAX_U112));
        assert_eq!(output, BigUint::from(MAX_U112 / 4));
    }

    #[test]
    fn caller_fee_policy_changes_quotes_without_multiplying_stored_punishment() {
        let mut state = state();
        state.max_punishment_x24 = 16_778;
        state.blacklist_fee_multiplier = U256::from(100u64);
        let amount = U256::from(100_000u64);
        let (out, next) = state
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(out, U256::from(99_500u64));
        assert_eq!(next.fee_bid_x24, 839);
        assert_eq!(next.reserve_y, 900_000);

        apply_delta(
            &mut state,
            HashMap::from([("quote_caller_whitelisted".to_owned(), Bytes::from([1u8]))]),
        )
        .unwrap();
        let (out, _) = state
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(out, U256::from(99_995u64));

        apply_delta(
            &mut state,
            HashMap::from([
                ("quote_caller_whitelisted".to_owned(), Bytes::from([0u8])),
                (
                    "blacklist_fee_multiplier".to_owned(),
                    Bytes::from(U256::from(2u64).to_be_bytes::<32>()),
                ),
            ]),
        )
        .unwrap();
        let (out, _) = state
            .quote_exact_in(state.token_y, state.token_x, amount)
            .unwrap();
        assert_eq!(out, U256::from(99_990u64));
        assert_eq!(state.latest_update_block, 100);

        state.blacklist_fee_multiplier = U256::ZERO;
        assert_eq!(state.fee_multiplier(), U256::from(1u64));
    }

    #[test]
    fn spot_price_applies_caller_multiplier_and_clamps_full_consumption() {
        let mut state = state();
        state.fee_bid_x24 = (1 << 24) / 4;
        state.fee_ask_x24 = (1 << 24) / 8;
        state.blacklist_fee_multiplier = U256::from(2u64);
        let x = token(state.token_x, "X", 18);
        let y = token(state.token_y, "Y", 18);
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.5);
        assert_eq!(state.spot_price(&y, &x).unwrap(), 0.75);

        state.quote_caller_whitelisted = true;
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.75);
        state.quote_caller_whitelisted = false;
        state.blacklist_fee_multiplier = U256::MAX;
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.0);
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(100u64)),
            Err(QuoteError::Rejected)
        );
        state.fee_bid_x24 = 0;
        assert_eq!(state.spot_price(&x, &y).unwrap(), 1.0);
    }

    #[test]
    fn matches_base_router_quotes_observed_at_block_51297469() {
        // Recorded eth_call results at Base block 51,297,469, pool
        // 0x0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99, with `from` set to
        // TychoRouter 0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67.
        // The router was unwhitelisted and paid multiplier 100 at this block.
        let mut state = state();
        state.pool =
            address_from_bytes(&hex::decode("0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99").unwrap())
                .unwrap();
        state.token_x = [0; 20];
        state.token_y =
            address_from_bytes(&hex::decode("833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap())
                .unwrap();
        state.anchor_price_x96 = U256::from(3_971_029_714_873_270_134_636_544u128);
        state.fee_ask_x24 = 1_835;
        state.fee_bid_x24 = 8_475;
        state.latest_update_block = 51_297_469;
        state.head_block = 51_297_469;
        state.max_punishment_x24 = 160_000;
        state.block_delay = 3;
        state.reserve_x = 9_683_337_552_179_635_497;
        state.reserve_y = 23_231_346_797;
        state.blacklist_fee_multiplier = U256::from(100u64);

        // Direction, input, observed net output, observed output-token fee.
        let quotes = [
            (true, 1_000_000_000_000_000u128, 2_385_158u128, 127_000u128),
            (true, 100_000_000_000_000_000, 237_258_990, 13_956_900),
            (true, 1_000_000_000_000_000_000, 2_258_700_200, 253_458_700),
            (false, 1_000_000, 393_700_695_234_017, 4_363_296_513_600),
            (false, 100_000_000, 39_291_060_402_299_481, 515_338_772_582_000),
            (false, 1_000_000_000, 385_726_231_079_417_225, 12_337_760_669_557_200),
        ];
        for (x_to_y, input, expected_out, expected_fee) in quotes {
            let (token_in, token_out) = if x_to_y {
                (state.token_x, state.token_y)
            } else {
                (state.token_y, state.token_x)
            };
            let (out, next) = state
                .quote_exact_in(token_in, token_out, U256::from(input))
                .unwrap();
            assert_eq!(out, U256::from(expected_out), "direction={x_to_y}, input={input}");
            let gross = if x_to_y {
                state.reserve_y - next.reserve_y
            } else {
                state.reserve_x - next.reserve_x
            };
            assert_eq!(gross - expected_out, expected_fee);
            assert_eq!(next.anchor_price_x96, state.anchor_price_x96);
        }
    }

    #[test]
    fn matches_bsc_router_quotes_observed_at_block_121835898() {
        // Recorded eth_call results at BSC block 121,835,898, pool
        // 0x00007904d186680c709519e71f4dc3e2df8f1b99, with `from` set to
        // 0x7f3d12bbafb8955e51b3ab9588b34c8ad95bda4e. Both tokens use 18
        // decimals; the caller was unwhitelisted and the multiplier was 1.
        let mut state = state();
        state.pool =
            address_from_bytes(&hex::decode("00007904d186680c709519e71f4dc3e2df8f1b99").unwrap())
                .unwrap();
        state.token_x = [0; 20];
        state.token_y =
            address_from_bytes(&hex::decode("55d398326f99059ff775485246999027b3197955").unwrap())
                .unwrap();
        state.anchor_price_x96 = U256::from(2_129_209_909_442_387_799_622_216_056_832u128);
        state.fee_ask_x24 = 3_092;
        state.fee_bid_x24 = 552;
        state.latest_update_block = 121_835_890;
        state.head_block = 121_835_898;
        state.max_punishment_x24 = 125_000;
        state.block_delay = 25;
        state.reserve_x = 40_982_882_360_761_506_608;
        state.reserve_y = 37_489_098_599_819_531_485_143;

        // Direction, input, observed net output, observed output-token fee.
        let quotes: [(bool, u128, u128, u128); 6] = [
            (true, 1_000_000_000_000_000, 722_209_919_138_098_675, 23_848_861_901_283),
            (true, 100_000_000_000_000_000, 72_220_419_368_930_290_024, 2_957_431_069_707_870),
            (true, 1_000_000_000_000_000_000, 722_152_061_971_320_131_445, 81_706_028_679_847_714),
            (false, 1_000_000_000_000_000_000, 1_384_337_907_652_450, 255_342_216_199),
            (false, 100_000_000_000_000_000_000, 138_432_263_993_751_885, 27_060_993_113_036),
            (false, 1_000_000_000_000_000_000_000, 1_384_184_240_164_873_034, 409_009_703_776_182),
        ];
        for (x_to_y, input, expected_out, expected_fee) in quotes {
            let (token_in, token_out) = if x_to_y {
                (state.token_x, state.token_y)
            } else {
                (state.token_y, state.token_x)
            };
            let (out, next) = state
                .quote_exact_in(token_in, token_out, U256::from(input))
                .unwrap();
            assert_eq!(out, U256::from(expected_out), "direction={x_to_y}, input={input}");
            let gross = if x_to_y {
                state.reserve_y - next.reserve_y
            } else {
                state.reserve_x - next.reserve_x
            };
            assert_eq!(gross - expected_out, expected_fee);
            assert_eq!(next.anchor_price_x96, state.anchor_price_x96);
        }
    }
}
