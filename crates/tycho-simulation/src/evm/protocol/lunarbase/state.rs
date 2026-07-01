use std::{any::Any, collections::HashMap, sync::Arc};

use alloy::primitives::U256 as RuintU256;
use lunarbase_pmm_math::{
    curve_pmm::{quote_x_to_y_with_multiplier, quote_y_to_x_with_multiplier},
    PoolParams, U256,
};
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
        swap::{
            LimitsParams, MarginalPrice, MarginalPriceParams, QuerySwapParams, Quote, QuoteAmount,
            QuoteParams, Range, SimulationResult, Swap, SwapFee, SwapLimits, SwapQuoter,
            Transition, TransitionParams,
        },
    },
    Bytes,
};

use super::decoder::apply_delta;
use crate::evm::protocol::u256_num::biguint_to_u256 as biguint_to_ruint;

pub type Address = [u8; 20];
const DEFAULT_GAS: u64 = 180_000;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseTychoState {
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub anchor_price_x96: u128,
    pub fee_ask_x24: u32,
    pub fee_bid_x24: u32,
    pub latest_update_block: u64,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub concentration_k: u32,
    pub block_delay: u64,
    pub paused: bool,
    pub head_block: u64,
    #[serde(skip)]
    pub(super) component: Option<Arc<ProtocolComponent<Arc<Token>>>>,
}

impl PartialEq for LunarBaseTychoState {
    fn eq(&self, other: &Self) -> bool {
        self.pool == other.pool &&
            self.token_x == other.token_x &&
            self.token_y == other.token_y &&
            self.anchor_price_x96 == other.anchor_price_x96 &&
            self.fee_ask_x24 == other.fee_ask_x24 &&
            self.fee_bid_x24 == other.fee_bid_x24 &&
            self.latest_update_block == other.latest_update_block &&
            self.reserve_x == other.reserve_x &&
            self.reserve_y == other.reserve_y &&
            self.concentration_k == other.concentration_k &&
            self.block_delay == other.block_delay &&
            self.paused == other.paused &&
            self.head_block == other.head_block
    }
}

impl Eq for LunarBaseTychoState {}

impl LunarBaseTychoState {
    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        self.component = Some(component);
        self
    }

    pub fn pool_params(&self) -> PoolParams {
        PoolParams {
            sqrt_price_x96: self.anchor_price_x96,
            fee_ask_x24: self.fee_ask_x24,
            fee_bid_x24: self.fee_bid_x24,
            reserve_x: self.reserve_x,
            reserve_y: self.reserve_y,
            concentration_k: self.concentration_k,
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.head_block <
            self.latest_update_block
                .saturating_add(self.block_delay)
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

        let params = self.pool_params();
        if token_in == self.token_x && token_out == self.token_y {
            let math_result = quote_x_to_y_with_multiplier(&params, amount_in, U256::from(1u64));
            if math_result.amount_out.is_zero() {
                return Err(QuoteError::Rejected);
            }

            let input = u256_to_u128(amount_in)?;
            let gross_output = u256_to_u128(
                math_result
                    .amount_out
                    .checked_add(math_result.fee)
                    .ok_or(QuoteError::ReserveOverflow)?,
            )?;
            let mut next = self.clone();
            next.reserve_x = next
                .reserve_x
                .checked_add(input)
                .ok_or(QuoteError::ReserveOverflow)?;
            next.reserve_y = next
                .reserve_y
                .checked_sub(gross_output)
                .ok_or(QuoteError::ReserveUnderflow)?;
            return Ok((math_result.amount_out, next));
        }

        if token_in == self.token_y && token_out == self.token_x {
            let math_result = quote_y_to_x_with_multiplier(&params, amount_in, U256::from(1u64));
            if math_result.amount_out.is_zero() {
                return Err(QuoteError::Rejected);
            }

            let input = u256_to_u128(amount_in)?;
            let gross_output = u256_to_u128(
                math_result
                    .amount_out
                    .checked_add(math_result.fee)
                    .ok_or(QuoteError::ReserveOverflow)?,
            )?;
            let mut next = self.clone();
            next.reserve_y = next
                .reserve_y
                .checked_add(input)
                .ok_or(QuoteError::ReserveOverflow)?;
            next.reserve_x = next
                .reserve_x
                .checked_sub(gross_output)
                .ok_or(QuoteError::ReserveUnderflow)?;
            return Ok((math_result.amount_out, next));
        }

        Err(QuoteError::InvalidTokenPair)
    }
}

#[typetag::serde]
impl ProtocolSim for LunarBaseTychoState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let token_in = address_from_bytes(base.address.as_ref())?;
        let token_out = address_from_bytes(quote.address.as_ref())?;
        if token_in == self.token_x && token_out == self.token_y {
            return spot_from_reserves(self.reserve_x, self.reserve_y, base, quote);
        }
        if token_in == self.token_y && token_out == self.token_x {
            return spot_from_reserves(self.reserve_y, self.reserve_x, base, quote);
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
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
            return quote_limit(self, sell, buy, soft_limit(self.reserve_x));
        }
        if sell == self.token_y && buy == self.token_x {
            return quote_limit(self, sell, buy, soft_limit(self.reserve_y));
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

        let head_block = delta
            .updated_attributes
            .get("block_number")
            .map(|value| u64::from(value.clone()));

        let updated_attributes = delta
            .updated_attributes
            .into_iter()
            .filter(|(key, _)| key != "block_number" && key != "block_timestamp")
            .collect();
        apply_delta(self, updated_attributes)
            .map_err(|err| TransitionError::DecodeError(format!("{err:?}")))?;
        if let Some(head_block) = head_block {
            self.head_block = head_block;
        }
        Ok(())
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

#[typetag::serde]
impl SwapQuoter for LunarBaseTychoState {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component.clone().ok_or_else(|| {
            SimulationError::FatalError(
                "LunarBaseTychoState: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Ok(SwapFee::new(0.0))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component.as_ref().ok_or_else(|| {
            SimulationError::FatalError("LunarBaseTychoState: component not set".to_string())
        })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;
        let price = self.spot_price(base.as_ref(), quote.as_ref())?;
        Ok(MarginalPrice::new(price))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "LunarBaseTychoState does not yet support exact-out (FixedOut) quoting"
                        .to_string(),
                ))
            }
        };

        let token_in = address_from_bytes(params.token_in().as_ref())?;
        let token_out = address_from_bytes(params.token_out().as_ref())?;
        // Bridge ruint U256 <-> lunarbase U256 via big-endian bytes (no heap allocation).
        let amount_in_lb = U256::from_be_slice(&amount_in.to_be_bytes::<32>());
        let (amount_out_lb, next_state) = self
            .quote_exact_in(token_in, token_out, amount_in_lb)
            .map_err(map_quote_error)?;
        let amount_out = RuintU256::from_be_slice(&amount_out_lb.to_be_bytes::<32>());

        let new_state = if params.should_return_new_state() {
            Some(Arc::new(next_state) as Arc<dyn SwapQuoter>)
        } else {
            None
        };
        Ok(Quote::new(amount_out, DEFAULT_GAS, new_state))
    }

    fn swap_limits(&self, params: LimitsParams) -> SimulationResult<SwapLimits> {
        let (max_in, max_out) =
            ProtocolSim::get_limits(self, params.token_in().clone(), params.token_out().clone())?;
        Ok(SwapLimits::new(
            Range::new(RuintU256::ZERO, biguint_to_ruint(&max_in))?,
            Range::new(RuintU256::ZERO, biguint_to_ruint(&max_out))?,
        ))
    }

    fn query_swap(&self, _params: QuerySwapParams) -> SimulationResult<Swap> {
        Err(SimulationError::FatalError(
            "LunarBaseTychoState::query_swap is not yet wired (pending token plumbing)".to_string(),
        ))
    }

    fn delta_transition(
        &mut self,
        params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        ProtocolSim::delta_transition(
            self,
            params.delta().clone(),
            params.tokens(),
            params.balances(),
        )?;
        Ok(Transition::default())
    }

    fn clone_box(&self) -> Box<dyn SwapQuoter> {
        Box::new(self.clone())
    }

    #[allow(deprecated)]
    fn to_protocol_sim(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QuoteError {
    Paused,
    Stale { block_number: u64, latest_update_block: u64, block_delay: u64 },
    InvalidTokenPair,
    Rejected,
    ReserveOverflow,
    ReserveUnderflow,
}

fn u256_to_u128(value: U256) -> Result<u128, QuoteError> {
    if value.bit_len() > 128 {
        return Err(QuoteError::ReserveOverflow);
    }
    let limbs = value.as_limbs();
    Ok(((limbs[1] as u128) << 64) | limbs[0] as u128)
}

fn spot_from_reserves(
    reserve_in: u128,
    reserve_out: u128,
    token_in: &Token,
    token_out: &Token,
) -> Result<f64, SimulationError> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err(SimulationError::RecoverableError("zero LunarBase reserve".to_owned()));
    }
    let decimals_adjustment = 10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
    Ok((reserve_out as f64 / reserve_in as f64) * decimals_adjustment)
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

fn quote_limit(
    state: &LunarBaseTychoState,
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
            Err(
                QuoteError::Rejected | QuoteError::ReserveOverflow | QuoteError::ReserveUnderflow,
            ) => {
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
    use super::*;

    fn addr(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    fn state() -> LunarBaseTychoState {
        LunarBaseTychoState {
            pool: addr(9),
            token_x: addr(1),
            token_y: addr(2),
            anchor_price_x96: 1u128 << 96,
            fee_ask_x24: 0,
            fee_bid_x24: 0,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 1_000_000,
            concentration_k: 0,
            block_delay: 2,
            paused: false,
            head_block: 100,
            component: None,
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
    fn test_swap_quoter_matches_protocol_sim() {
        use tycho_common::models::Chain;

        let state = state();
        let mk = |a: [u8; 20]| {
            Token::new(&Bytes::from(a), "T", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
        };

        for (token_in, token_out) in [(mk(addr(1)), mk(addr(2))), (mk(addr(2)), mk(addr(1)))] {
            for amount in [1_000u64, 250_000u64] {
                let amount = BigUint::from(amount);

                let legacy = state
                    .get_amount_out(amount.clone(), &token_in, &token_out)
                    .unwrap();
                let quote = state
                    .quote(
                        QuoteParams::fixed_in(
                            &token_in.address,
                            &token_out.address,
                            biguint_to_ruint(&amount),
                        )
                        .unwrap()
                        .with_new_state(),
                    )
                    .unwrap();

                assert_eq!(ruint_to_biguint_local(quote.amount_out()), legacy.amount);
                assert_eq!(BigUint::from(quote.gas()), legacy.gas);
            }

            let (legacy_in, legacy_out) = state
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .unwrap();
            let limits = state
                .swap_limits(LimitsParams::new(&token_in.address, &token_out.address))
                .unwrap();
            assert_eq!(ruint_to_biguint_local(limits.range_in().upper()), legacy_in);
            assert_eq!(ruint_to_biguint_local(limits.range_out().upper()), legacy_out);
        }
    }

    fn ruint_to_biguint_local(value: RuintU256) -> BigUint {
        BigUint::from_bytes_be(&value.to_be_bytes::<32>())
    }

    #[test]
    fn test_marginal_price_matches_spot_price() {
        use tycho_common::models::Chain;

        let state = state();
        let tok_x = Token::new(
            &Bytes::from(state.token_x.to_vec()),
            "TX",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let tok_y = Token::new(
            &Bytes::from(state.token_y.to_vec()),
            "TY",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![tok_x.address.clone(), tok_y.address.clone()];
        let all_tokens = std::collections::HashMap::from([
            (tok_x.address.clone(), tok_x.clone()),
            (tok_y.address.clone(), tok_y.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = state.with_component(component);

        for (base, quote) in [(&tok_x, &tok_y), (&tok_y, &tok_x)] {
            let spot = state.spot_price(base, quote).unwrap();
            let marginal = state
                .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
                .unwrap();
            approx::assert_ulps_eq!(marginal.price(), spot);
        }
    }
}
