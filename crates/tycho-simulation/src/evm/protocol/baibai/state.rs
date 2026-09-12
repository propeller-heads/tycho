use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
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

use super::math::{add, big, invalid, uint, Side};

/// Indexed words: ttl; pair slots 0..4; twelve ask and twelve bid slots;
/// totalClaimable for base and quote. Retaining the words permits atomic partial updates.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BaibaiState {
    pub(super) id: String,
    pub(super) tokens: [Bytes; 2],
    pub(super) words: [U256; 32],
    pub(super) balances: [U256; 2],
    pub(super) c_unit: U256,
    pub(super) timestamp: u64,
}

impl BaibaiState {
    pub(super) fn fresh(&self) -> bool {
        let last: u64 = self.words[1].as_limbs()[2];
        let valid: u64 = self.words[1].as_limbs()[3];
        let ttl: u64 = self.words[0].as_limbs()[0];
        self.words[3] != U256::ZERO &&
            self.timestamp <= valid &&
            (ttl == 0 || u128::from(self.timestamp) <= u128::from(last) + u128::from(ttl))
    }

    fn direction(&self, input: &Bytes, output: &Bytes) -> Result<usize, SimulationError> {
        if input == &self.tokens[0] && output == &self.tokens[1] {
            return Ok(0);
        }
        if input == &self.tokens[1] && output == &self.tokens[0] {
            return Ok(1);
        }
        Err(invalid("invalid token pair"))
    }

    fn available(&self, token: usize) -> U256 {
        self.balances[token].saturating_sub(self.words[30 + token])
    }

    pub(super) fn side(&self, asks: bool) -> Result<Side, SimulationError> {
        let packed = self.words[2];
        let field = |shift: usize, bits: usize| {
            (packed >> shift) & ((U256::from(1) << bits) - U256::from(1))
        };
        let mid = field(0, 80);
        let spread = field(if asks { 80 } else { 96 }, 16).to::<u16>() as i16;
        let depth = field(if asks { 112 } else { 128 }, 16);
        let count = field(if asks { 144 } else { 152 }, 8).to::<usize>();
        if count > 36 || spread <= -10_000 || depth > U256::from(10_000) {
            return Err(invalid("invalid curve parameters"));
        }
        let mut knots = Vec::with_capacity(count);
        let mask: U256 = (U256::from(1) << 42) - U256::from(1);
        for i in 0..count {
            let word: U256 = self.words[if asks { 6 } else { 18 } + i / 3] >> (172 - (i % 3) * 84);
            let q = ((word >> 42usize) & mask)
                .checked_mul(self.words[3])
                .ok_or_else(|| invalid("knot quantity overflow"))?;
            let c = (word & mask)
                .checked_mul(self.c_unit)
                .ok_or_else(|| invalid("knot cost overflow"))?;
            let prev = knots
                .last()
                .copied()
                .unwrap_or((U256::ZERO, U256::ZERO));
            if q <= prev.0 || c < prev.1 {
                return Err(invalid("nonmonotonic knots"));
            }
            knots.push((q, c));
        }
        let max = knots
            .last()
            .map_or(U256::ZERO, |k| k.0)
            .checked_mul(depth)
            .ok_or_else(|| invalid("depth overflow"))? /
            U256::from(10_000);
        Ok(Side {
            price: mid * U256::from((10_000i32 + i32::from(spread)) as u32) / U256::from(10_000),
            filled: self.words[if asks { 4 } else { 5 }],
            max,
            knots,
            asks,
        })
    }
}

#[typetag::serde]
impl ProtocolSim for BaibaiState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = self.direction(&base.address, &quote.address)?;
        if !self.fresh() || self.available(1 - direction) == U256::ZERO {
            return Err(invalid("unavailable liquidity"));
        }
        Ok(self
            .side(direction == 1)?
            .marginal_price()? *
            10f64.powi(base.decimals as i32 - quote.decimals as i32))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let direction = self.direction(&token_in.address, &token_out.address)?;
        if !self.fresh() {
            return Err(invalid("curve expired or uninitialized"));
        }
        let input = uint(&amount_in)?;
        let (output, cursor) = self
            .side(direction == 1)?
            .quote(input)?;
        if output == U256::ZERO && input != U256::ZERO {
            return Err(invalid("input below atomic precision"));
        }
        if output > self.available(1 - direction) {
            return Err(invalid("insufficient custody liquidity"));
        }
        let mut next = self.clone();
        next.words[if direction == 1 { 4 } else { 5 }] = cursor;
        next.balances[direction] = add(next.balances[direction], input)?;
        next.balances[1 - direction] -= output;
        // Includes headroom over the fork-tested 36-knot router swap for cold storage reads.
        Ok(GetAmountOutResult::new(big(output), BigUint::from(400_000u64), Box::new(next)))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction = self.direction(&sell_token, &buy_token)?;
        if !self.fresh() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let side = self.side(direction == 1)?;
        let mut high = side.capacity()?;
        let mut low = U256::ZERO;
        let available = self.available(1 - direction);
        if available == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        high = high.min(U256::MAX - self.balances[direction]);
        let full_output = side.quote(high)?.0;
        if full_output == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        if side.output_bound(high)? <= available {
            return Ok((big(high), big(full_output)));
        }
        // Capacity is bounded by curve depth and the custodian's unreserved output balance.
        while low < high {
            let mid = low + (high - low) / U256::from(2) + (high - low) % U256::from(2);
            if side.output_bound(mid)? <= available {
                low = mid;
            } else {
                high = mid - U256::from(1);
            }
        }
        let output = side.quote(low)?.0;
        if output == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        Ok((big(low), big(output)))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        let mut next = self.clone();
        if !delta.deleted_attributes.is_empty() {
            return Err(TransitionError::DecodeError("BaiBai state words cannot be deleted".into()));
        }
        super::decoder::apply_words(&mut next.words, &delta.updated_attributes, false)
            .map_err(|e| TransitionError::DecodeError(e.to_string()))?;
        if let Some(updated) = balances
            .component_balances
            .get(&self.id)
        {
            for (i, token) in self.tokens.iter().enumerate() {
                if let Some(value) = updated.get(token) {
                    next.balances[i] = super::decoder::word(value)
                        .map_err(|e| TransitionError::DecodeError(e.to_string()))?;
                }
            }
        }
        next.side(true)
            .and_then(|_| next.side(false))
            .map_err(|e| TransitionError::DecodeError(e.to_string()))?;
        *self = next;
        Ok(())
    }

    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let before = self.fresh();
        self.timestamp = block.timestamp();
        before != self.fresh()
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
