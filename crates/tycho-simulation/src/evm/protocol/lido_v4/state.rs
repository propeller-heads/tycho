use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
use hex_literal::hex;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_mul_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
};

/// One component covers the whole venue: stETH mints, and wstETH wraps, unwraps and mints
/// through `receive()`. Keyed by stETH, the contract that holds the pool.
pub const STETH_COMPONENT_ID: &str = "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84";

pub const STETH_ADDRESS: [u8; 20] = hex!("ae7ab96520de3a18e5e111b5eaab095312d7fe84");
pub const WSTETH_ADDRESS: [u8; 20] = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0");
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");

/// Slice views of the addresses above, so a swap direction can be matched as a tuple.
const ETH: &[u8] = &ETH_ADDRESS;
const STETH: &[u8] = &STETH_ADDRESS;
const WSTETH: &[u8] = &WSTETH_ADDRESS;

// One attribute per value Lido names. The substreams package unpacks the storage words, so
// each of these carries a single scalar.
pub const TOTAL_SHARES_ATTR: &str = "total_shares";
pub const EXTERNAL_SHARES_ATTR: &str = "external_shares";
pub const BUFFERED_ETHER_ATTR: &str = "buffered_ether";
pub const DEPOSITED_POST_REPORT_ATTR: &str = "deposited_post_report";
pub const CL_VALIDATORS_BALANCE_ATTR: &str = "cl_validators_balance";
pub const CL_PENDING_BALANCE_ATTR: &str = "cl_pending_balance";
pub const PREV_STAKE_BLOCK_NUMBER_ATTR: &str = "prev_stake_block_number";
pub const PREV_STAKE_LIMIT_ATTR: &str = "prev_stake_limit";
pub const MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR: &str = "max_stake_limit_growth_blocks";
pub const MAX_STAKE_LIMIT_ATTR: &str = "max_stake_limit";
pub const WSTETH_SHARES_ATTR: &str = "wsteth_shares";

const UINT128_MAX_EXCLUSIVE: u128 = u128::MAX;

const SUBMIT_GAS: u64 = 160_000;
/// wstETH's `receive()` runs `stETH.submit` and mints the wrapper's shares in one call. Measured
/// on mainnet at 101,826 against 86,779 for a bare `submit`, so the wrap adds ~15,000 on top of
/// whatever the submit path costs.
const SUBMIT_AND_WRAP_GAS: u64 = SUBMIT_GAS + 15_000;
const WRAP_GAS: u64 = 81_000;
const UNWRAP_GAS: u64 = 66_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LidoV4State {
    /// Height of the block a quote is expected to execute in, maintained by `apply_block`.
    ///
    /// The stake limit accrues per block, so this tracks the chain head: `apply_block` advances
    /// it on every message, including the blocks where stETH storage does not move, which are
    /// most of them.
    execution_block_number: u64,
    total_shares: U256,
    external_shares: U256,
    buffered_ether: U256,
    /// ETH sent to the deposit contract since the last oracle report; the next report moves it
    /// into the consensus-layer balances below.
    deposited_post_report: U256,
    cl_validators_balance: U256,
    cl_pending_balance: U256,
    staking_state: StakingState,
    /// `sharesOf(wstETH)`, i.e. the shares the wrapper holds. Bounds how much can be unwrapped.
    wsteth_shares: U256,
}

/// Lido stores amounts as uint128 or narrower, so anything wider is a malformed input rather
/// than a trade the venue could serve.
fn validate_u128_bound(name: &str, value: U256) -> Result<(), SimulationError> {
    if value >= U256::from(UINT128_MAX_EXCLUSIVE) {
        return Err(SimulationError::InvalidInput(format!("{name} exceeds uint128 bound"), None));
    }
    Ok(())
}

/// Bytes an attribute occupies on the wire: the width of the stETH storage field the substreams
/// package unpacks it from. The balances and share counts are `getLowAndHighUint128` halves, the
/// stake limit fields are laid out by `StakeLimitUtils`, and `sharesOf(wstETH)` is a whole word.
fn attribute_width(name: &str) -> Option<usize> {
    match name {
        TOTAL_SHARES_ATTR |
        EXTERNAL_SHARES_ATTR |
        BUFFERED_ETHER_ATTR |
        DEPOSITED_POST_REPORT_ATTR |
        CL_VALIDATORS_BALANCE_ATTR |
        CL_PENDING_BALANCE_ATTR => Some(16),
        PREV_STAKE_LIMIT_ATTR | MAX_STAKE_LIMIT_ATTR => Some(12),
        PREV_STAKE_BLOCK_NUMBER_ATTR | MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR => Some(4),
        WSTETH_SHARES_ATTR => Some(32),
        _ => None,
    }
}

/// Reads a big-endian attribute, refusing one wider than its storage field. The package emits
/// minimal-length values, so a wider one is malformed; the `Err` names the attribute so the
/// caller can report it instead of truncating it into a plausible number.
pub(super) fn decode_attribute(name: &str, value: &[u8]) -> Result<U256, String> {
    let Some(width) = attribute_width(name) else {
        return Err(format!("{name} is not a Lido V4 attribute"));
    };
    if value.len() > width {
        return Err(format!("{name} is {} bytes, wider than its {width}-byte field", value.len()));
    }
    Ok(U256::from_be_slice(value))
}

/// `decode_attribute` for the two 32-bit block counters in `StakeLimitUtils`.
pub(super) fn decode_u32_attribute(name: &str, value: &[u8]) -> Result<u32, String> {
    let value = decode_attribute(name, value)?;
    u32::try_from(value).map_err(|_| format!("{name} does not fit in 32 bits"))
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StakingState {
    prev_stake_block_number: u32,
    prev_stake_limit: U256,
    max_stake_limit_growth_blocks: u32,
    max_stake_limit: U256,
}

impl LidoV4State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_block_number: u64,
        total_shares: U256,
        external_shares: U256,
        buffered_ether: U256,
        deposited_post_report: U256,
        cl_validators_balance: U256,
        cl_pending_balance: U256,
        staking_state: StakingState,
        wsteth_shares: U256,
    ) -> Self {
        Self {
            execution_block_number,
            total_shares,
            external_shares,
            buffered_ether,
            deposited_post_report,
            cl_validators_balance,
            cl_pending_balance,
            staking_state,
            wsteth_shares,
        }
    }

    fn internal_shares(&self) -> Result<U256, SimulationError> {
        if self.external_shares > self.total_shares {
            return Err(SimulationError::FatalError(
                "external shares exceed total shares".to_string(),
            ));
        }
        Ok(self.total_shares - self.external_shares)
    }

    /// `Lido._getInternalEther()`: buffered ether plus every balance counted on the consensus
    /// layer - active validators, deposits pending activation, and deposits made since the last
    /// oracle report. Each term is one half of a word read through `getLowAndHighUint128`, which
    /// masks it to 128 bits, and `decode_attribute` holds it to that width, so four of them sum
    /// below 2^130.
    fn internal_ether(&self) -> U256 {
        self.buffered_ether +
            self.cl_validators_balance +
            self.cl_pending_balance +
            self.deposited_post_report
    }

    fn shares_for_pooled_eth(&self, eth_amount: U256) -> Result<U256, SimulationError> {
        validate_u128_bound("eth amount", eth_amount)?;
        let denominator = self.internal_shares()?;
        let numerator = self.internal_ether();
        if denominator.is_zero() || numerator.is_zero() {
            return Err(SimulationError::FatalError("invalid Lido share rate state".to_string()));
        }
        Ok(safe_mul_u256(eth_amount, denominator)? / numerator)
    }

    fn pooled_eth_by_shares(&self, shares_amount: U256) -> Result<U256, SimulationError> {
        validate_u128_bound("shares amount", shares_amount)?;
        let numerator = self.internal_ether();
        let denominator = self.internal_shares()?;
        if denominator.is_zero() || numerator.is_zero() {
            return Err(SimulationError::FatalError("invalid Lido share rate state".to_string()));
        }
        Ok(safe_mul_u256(shares_amount, numerator)? / denominator)
    }

    fn amount_out_eth_to_steth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let shares_amount = self.shares_for_pooled_eth(amount_in)?;
        let mut new_state = self.clone();
        new_state
            .staking_state
            .decrease(amount_in, new_state.execution_block_number)?;
        new_state.total_shares = safe_add_u256(new_state.total_shares, shares_amount)?;
        new_state.buffered_ether = safe_add_u256(new_state.buffered_ether, amount_in)?;
        let amount_out = new_state.pooled_eth_by_shares(shares_amount)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(SUBMIT_GAS),
            Box::new(new_state),
        ))
    }

    fn amount_out_steth_to_wsteth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_out = self.shares_for_pooled_eth(amount_in)?;
        // `wrap` pulls the stETH into the wrapper, so the shares it holds grow by what it minted.
        let mut new_state = self.clone();
        new_state.wsteth_shares = safe_add_u256(new_state.wsteth_shares, amount_out)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(WRAP_GAS),
            Box::new(new_state),
        ))
    }

    /// ETH -> wstETH through the wrapper's `receive()`: it submits the ETH and mints exactly the
    /// shares `submit` returned, so the output is the share count itself, not a stETH balance.
    fn amount_out_eth_to_wsteth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let shares_amount = self.shares_for_pooled_eth(amount_in)?;
        let mut new_state = self.clone();
        new_state
            .staking_state
            .decrease(amount_in, new_state.execution_block_number)?;
        new_state.total_shares = safe_add_u256(new_state.total_shares, shares_amount)?;
        new_state.buffered_ether = safe_add_u256(new_state.buffered_ether, amount_in)?;
        // The submitted stETH lands on the wrapper, so its share balance grows with the mint.
        new_state.wsteth_shares = safe_add_u256(new_state.wsteth_shares, shares_amount)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(shares_amount),
            BigUint::from(SUBMIT_AND_WRAP_GAS),
            Box::new(new_state),
        ))
    }

    fn amount_out_wsteth_to_steth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // Unwrapping pays out of the stETH the wrapper holds. Beyond that `wstETH.unwrap` reverts
        // with a SafeMath underflow, and since the rate is linear nothing about a larger quote
        // looks wrong - so reject it here rather than hand back a number that cannot settle.
        // `get_limits` caps this direction at the same value.
        if amount_in > self.wsteth_shares {
            return Err(SimulationError::RecoverableError("WRAPPER_BALANCE_EXCEEDED".to_string()));
        }
        let amount_out = self.pooled_eth_by_shares(amount_in)?;
        // `unwrap` burns the caller's wstETH and sends the stETH out, so the wrapper holds that
        // many fewer shares - which is exactly the bound above, so it has to move with it.
        let mut new_state = self.clone();
        new_state.wsteth_shares = safe_sub_u256(new_state.wsteth_shares, amount_in)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(UNWRAP_GAS),
            Box::new(new_state),
        ))
    }
}

impl StakingState {
    pub(crate) fn new(
        prev_stake_block_number: u32,
        prev_stake_limit: U256,
        max_stake_limit_growth_blocks: u32,
        max_stake_limit: U256,
    ) -> Self {
        Self {
            prev_stake_block_number,
            prev_stake_limit,
            max_stake_limit_growth_blocks,
            max_stake_limit,
        }
    }

    fn is_staking_paused(&self) -> bool {
        self.prev_stake_block_number == 0
    }

    fn is_staking_limit_set(&self) -> bool {
        !self.max_stake_limit.is_zero()
    }

    fn calculate_current_stake_limit(&self, block_number: u64) -> U256 {
        let stake_limit_inc_per_block = if self.max_stake_limit_growth_blocks != 0 {
            self.max_stake_limit / U256::from(self.max_stake_limit_growth_blocks)
        } else {
            U256::ZERO
        };

        let blocks_passed = block_number.saturating_sub(self.prev_stake_block_number as u64);
        let change = U256::from(blocks_passed) * stake_limit_inc_per_block;

        if self.prev_stake_limit < self.max_stake_limit {
            (self.prev_stake_limit + change).min(self.max_stake_limit)
        } else {
            self.prev_stake_limit
                .saturating_sub(change)
                .max(self.max_stake_limit)
        }
    }

    fn current_limit(&self, block_number: u64) -> U256 {
        if self.is_staking_paused() {
            U256::ZERO
        } else if !self.is_staking_limit_set() {
            U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE
        } else {
            self.calculate_current_stake_limit(block_number)
        }
    }

    fn decrease(&mut self, amount: U256, block_number: u64) -> Result<(), SimulationError> {
        if self.is_staking_paused() {
            return Err(SimulationError::RecoverableError("STAKING_PAUSED".to_string()));
        }

        if self.is_staking_limit_set() {
            let current_stake_limit = self.calculate_current_stake_limit(block_number);
            if amount > current_stake_limit {
                return Err(SimulationError::RecoverableError("STAKE_LIMIT".to_string()));
            }
            self.prev_stake_limit = current_stake_limit - amount;
            self.prev_stake_block_number = block_number as u32;
        }

        Ok(())
    }
}

#[typetag::serde]
impl ProtocolSim for LidoV4State {
    fn fee(&self) -> f64 {
        0f64
    }

    /// Prices exactly the four directions the venue performs. stETH -> ETH and wstETH -> ETH run
    /// through the asynchronous withdrawal queue, so they have no rate here, matching the zero
    /// limit `get_limits` reports for them.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let quote_unit_f64 = u256_to_f64(U256::from(10).pow(U256::from(quote.decimals)))?;
        let base_unit = U256::from(10).pow(U256::from(base.decimals));
        let to_price = |amount_out: U256| -> Result<f64, SimulationError> {
            Ok(u256_to_f64(amount_out)? / quote_unit_f64)
        };

        match (base.address.as_ref(), quote.address.as_ref()) {
            // Submitting mints shares, and the depositor holds the stETH balance those shares are
            // worth. Derived from the share rate rather than from a `get_amount_out` probe, so the
            // rate stays available while staking is paused or its limit is exhausted - those bound
            // capacity, not price.
            (ETH, STETH) => {
                to_price(self.pooled_eth_by_shares(self.shares_for_pooled_eth(base_unit)?)?)
            }
            // Submitting ETH mints shares worth the ETH, and wrapping stETH mints shares worth the
            // stETH - the same conversion either way, since submit is at parity.
            (ETH | STETH, WSTETH) => to_price(self.shares_for_pooled_eth(base_unit)?),
            (WSTETH, STETH) => to_price(self.pooled_eth_by_shares(base_unit)?),
            _ => Err(SimulationError::FatalError("unsupported spot price".to_string())),
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        // Every direction reverts on a zero amount: `submit` with ZERO_DEPOSIT, and the wrapper
        // with its own zero-amount guards. Quoting a zero output would report the trade as
        // settling for nothing rather than as not settling.
        if amount_in.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }

        match (token_in.address.as_ref(), token_out.address.as_ref()) {
            (ETH, STETH) => self.amount_out_eth_to_steth(amount_in),
            (STETH, WSTETH) => self.amount_out_steth_to_wsteth(amount_in),
            (WSTETH, STETH) => self.amount_out_wsteth_to_steth(amount_in),
            (ETH, WSTETH) => self.amount_out_eth_to_wsteth(amount_in),
            _ => Err(SimulationError::FatalError("unsupported swap".to_string())),
        }
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let max_input = U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE;

        match (sell_token.as_ref(), buy_token.as_ref()) {
            (ETH, STETH) => {
                let max_sell = self
                    .staking_state
                    .current_limit(self.execution_block_number)
                    .min(max_input);
                if max_sell.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                let max_buy = self
                    .amount_out_eth_to_steth(max_sell)?
                    .amount;
                Ok((u256_to_biguint(max_sell), max_buy))
            }
            (STETH, WSTETH) => {
                // Wrapping mints against the caller's own stETH, so the protocol only bounds it
                // by how much stETH exists.
                let max_sell = self.internal_ether().min(max_input);
                Ok((
                    u256_to_biguint(max_sell),
                    u256_to_biguint(self.shares_for_pooled_eth(max_sell)?),
                ))
            }
            (WSTETH, STETH) => {
                // Unwrapping pays out of the stETH the wrapper holds, so it is bounded by the
                // wrapper's shares. Quoting an unbounded limit here makes callers size trades the
                // wrapper cannot settle, and `wstETH.unwrap` reverts with a SafeMath underflow.
                let max_sell = self.wsteth_shares.min(max_input);
                if max_sell.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                Ok((
                    u256_to_biguint(max_sell),
                    u256_to_biguint(self.pooled_eth_by_shares(max_sell)?),
                ))
            }
            (ETH, WSTETH) => {
                // `receive()` stakes through `stETH.submit`, so the stake limit bounds it exactly
                // as it bounds ETH -> stETH.
                let max_sell = self
                    .staking_state
                    .current_limit(self.execution_block_number)
                    .min(max_input);
                if max_sell.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                let max_buy = self
                    .amount_out_eth_to_wsteth(max_sell)?
                    .amount;
                Ok((u256_to_biguint(max_sell), max_buy))
            }
            // Staking is one-directional: unstaking goes through the asynchronous withdrawal
            // queue. A zero limit, not an error - the cluster test counts every `get_limits`
            // error against the protocol, so erroring here would accrue failures forever for two
            // directions the venue structurally cannot serve.
            (STETH, ETH) | (WSTETH, ETH) => Ok((BigUint::ZERO, BigUint::ZERO)),
            // Anything else is a token this component does not hold. Zero would claim the venue
            // knows the pair and has no capacity; `spot_price` and `get_amount_out` already draw
            // this line.
            _ => Err(SimulationError::FatalError("unsupported swap".to_string())),
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let read = |name: &str| -> Result<Option<U256>, TransitionError> {
            delta
                .updated_attributes
                .get(name)
                .map(|value| decode_attribute(name, value))
                .transpose()
                .map_err(TransitionError::DecodeError)
        };
        let read_u32 = |name: &str| -> Result<Option<u32>, TransitionError> {
            delta
                .updated_attributes
                .get(name)
                .map(|value| decode_u32_attribute(name, value))
                .transpose()
                .map_err(TransitionError::DecodeError)
        };

        // Applied to a copy so a malformed attribute leaves `self` as it was.
        let mut next = self.clone();
        if let Some(value) = read(TOTAL_SHARES_ATTR)? {
            next.total_shares = value;
        }
        if let Some(value) = read(EXTERNAL_SHARES_ATTR)? {
            next.external_shares = value;
        }
        if let Some(value) = read(BUFFERED_ETHER_ATTR)? {
            next.buffered_ether = value;
        }
        if let Some(value) = read(DEPOSITED_POST_REPORT_ATTR)? {
            next.deposited_post_report = value;
        }
        if let Some(value) = read(CL_VALIDATORS_BALANCE_ATTR)? {
            next.cl_validators_balance = value;
        }
        if let Some(value) = read(CL_PENDING_BALANCE_ATTR)? {
            next.cl_pending_balance = value;
        }
        if let Some(value) = read(WSTETH_SHARES_ATTR)? {
            next.wsteth_shares = value;
        }
        if let Some(value) = read_u32(PREV_STAKE_BLOCK_NUMBER_ATTR)? {
            next.staking_state
                .prev_stake_block_number = value;
        }
        if let Some(value) = read(PREV_STAKE_LIMIT_ATTR)? {
            next.staking_state.prev_stake_limit = value;
        }
        if let Some(value) = read_u32(MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR)? {
            next.staking_state
                .max_stake_limit_growth_blocks = value;
        }
        if let Some(value) = read(MAX_STAKE_LIMIT_ATTR)? {
            next.staking_state.max_stake_limit = value;
        }
        *self = next;
        Ok(())
    }

    /// Advances to the block a quote would execute in, so the stake limit keeps accruing on the
    /// blocks where Lido's own storage did not move.
    ///
    /// Re-emits only when the resolved limit actually changed: a repeated block short-circuits,
    /// and once the limit has settled at `max_stake_limit` further blocks cost one virtual call
    /// and no clone.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let number = block.number();
        if number == self.execution_block_number {
            return false;
        }
        let limit_before = self
            .staking_state
            .current_limit(self.execution_block_number);
        self.execution_block_number = number;
        limit_before != self.staking_state.current_limit(number)
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_client::feed::BlockHeader;
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain,
        },
        simulation::errors::{SimulationError, TransitionError},
        Bytes,
    };

    use super::*;
    use crate::{
        evm::protocol::test_utils::try_decode_snapshot_with_defaults,
        protocol::{errors::InvalidSnapshotError, models::TryFromWithBlock},
    };

    fn eth_token() -> Token {
        Token::new(&Bytes::from(ETH_ADDRESS), "ETH", 18, 0, &[], Chain::Ethereum, 100)
    }

    fn steth_token() -> Token {
        Token::new(&Bytes::from(STETH_ADDRESS), "stETH", 18, 0, &[], Chain::Ethereum, 75)
    }

    fn wsteth_token() -> Token {
        Token::new(&Bytes::from(WSTETH_ADDRESS), "wstETH", 18, 0, &[], Chain::Ethereum, 100)
    }

    fn sample_staking_state() -> StakingState {
        StakingState {
            prev_stake_block_number: 24_083_113,
            prev_stake_limit: U256::from(1_000u64) * U256::from(10).pow(U256::from(18)),
            max_stake_limit_growth_blocks: 10,
            max_stake_limit: U256::from(1_000u64) * U256::from(10).pow(U256::from(18)),
        }
    }

    fn sample_state() -> LidoV4State {
        LidoV4State::new(
            24_083_113,
            U256::from_str_radix("6696604823358181328750512", 10).unwrap(),
            U256::from_str_radix("80758346894447149184", 10).unwrap(),
            U256::from_str_radix("658338852056838456032283", 10).unwrap(),
            U256::from(30_560u64) * U256::from(10).pow(U256::from(18)),
            U256::from_str_radix("21114116614166341429013364", 10).unwrap(),
            U256::ZERO,
            sample_staking_state(),
            sample_wsteth_shares(),
        )
    }

    /// The shares the wstETH wrapper holds - a little under half the pool.
    fn sample_wsteth_shares() -> U256 {
        U256::from_str_radix("2960000000000000000000000", 10).unwrap()
    }

    /// Encodes an attribute the way the substreams package does: big-endian with no leading zero
    /// bytes, and a single zero byte for zero.
    fn attribute(value: U256) -> Bytes {
        let bytes = value.to_be_bytes_vec();
        let start = bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(bytes.len() - 1);
        Bytes::from(bytes[start..].to_vec())
    }

    fn snapshot() -> tycho_client::feed::synchronizer::ComponentWithState {
        let state = sample_state();
        let staking = sample_staking_state();
        let attributes: HashMap<String, Bytes> = [
            (TOTAL_SHARES_ATTR, state.total_shares),
            (EXTERNAL_SHARES_ATTR, state.external_shares),
            (BUFFERED_ETHER_ATTR, state.buffered_ether),
            (DEPOSITED_POST_REPORT_ATTR, state.deposited_post_report),
            (CL_VALIDATORS_BALANCE_ATTR, state.cl_validators_balance),
            (CL_PENDING_BALANCE_ATTR, state.cl_pending_balance),
            (WSTETH_SHARES_ATTR, sample_wsteth_shares()),
            (PREV_STAKE_BLOCK_NUMBER_ATTR, U256::from(staking.prev_stake_block_number)),
            (PREV_STAKE_LIMIT_ATTR, staking.prev_stake_limit),
            (MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR, U256::from(staking.max_stake_limit_growth_blocks)),
            (MAX_STAKE_LIMIT_ATTR, staking.max_stake_limit),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), attribute(value)))
        .collect();
        let component_id = STETH_COMPONENT_ID.to_string();

        tycho_client::feed::synchronizer::ComponentWithState {
            state: ProtocolComponentState {
                component_id: component_id.clone(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: component_id,
                protocol_system: "lido_v4".to_string(),
                protocol_type_name: "lido_v4_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: Vec::new(),
                contract_addresses: Vec::new(),
                static_attributes: HashMap::new(),
                change: Default::default(),
                creation_tx: Bytes::new(),
                created_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    /// One component carries every attribute, so the decoded state is complete for all four
    /// directions - the stake limit that bounds the two submit paths and the wrapper's shares
    /// that bound unwrapping.
    #[tokio::test]
    async fn decoder_reads_the_snapshot() {
        let state = try_decode_snapshot_with_defaults::<LidoV4State>(snapshot())
            .await
            .unwrap();
        let expected = sample_state();

        assert_eq!(state.total_shares, expected.total_shares);
        assert_eq!(state.external_shares, expected.external_shares);
        assert_eq!(state.buffered_ether, expected.buffered_ether);
        assert_eq!(state.deposited_post_report, expected.deposited_post_report);
        assert_eq!(state.cl_validators_balance, expected.cl_validators_balance);
        assert_eq!(state.cl_pending_balance, expected.cl_pending_balance);
        assert_eq!(state.staking_state, expected.staking_state);
        assert_eq!(state.wsteth_shares, expected.wsteth_shares);
    }

    /// A whole word where the package emits a `getLowAndHighUint128` half cannot be a value the
    /// package produced, so the decoder reports it by name.
    #[tokio::test]
    async fn decoder_rejects_an_attribute_wider_than_its_field() {
        let mut snapshot = snapshot();
        snapshot.state.attributes.insert(
            TOTAL_SHARES_ATTR.to_string(),
            Bytes::from(
                sample_state()
                    .total_shares
                    .to_be_bytes_vec(),
            ),
        );

        let err = try_decode_snapshot_with_defaults::<LidoV4State>(snapshot)
            .await
            .unwrap_err();

        let InvalidSnapshotError::ValueError(message) = err else {
            panic!("expected a value error, got {err:?}");
        };
        assert!(message.contains(TOTAL_SHARES_ATTR), "{message}");
    }

    #[tokio::test]
    async fn decoder_rejects_an_unknown_component_id() {
        let mut snapshot = snapshot();
        snapshot.component.id = "0xdeadbeef".to_string();

        assert!(try_decode_snapshot_with_defaults::<LidoV4State>(snapshot)
            .await
            .is_err());
    }

    #[test]
    fn eth_to_steth_updates_state_and_consumes_stake_limit() {
        let state = sample_state();
        let amount_in = BigUint::from(10u64).pow(18);
        let result = state
            .get_amount_out(amount_in.clone(), &eth_token(), &steth_token())
            .unwrap();

        assert!(result.amount > BigUint::ZERO);
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<LidoV4State>()
            .unwrap();
        assert_eq!(
            new_state.buffered_ether,
            state.buffered_ether + U256::from(10).pow(U256::from(18))
        );
        assert!(new_state.total_shares > state.total_shares);
        let old_limit = state
            .staking_state
            .current_limit(state.execution_block_number);
        let new_limit = new_state
            .staking_state
            .current_limit(new_state.execution_block_number);
        assert!(new_limit < old_limit);
    }

    /// Both legs move the stETH the wrapper holds, and that balance is what bounds unwrapping,
    /// so a consumer walking the returned state has to see it change.
    #[test]
    fn wrapping_and_unwrapping_move_the_wrapper_shares() {
        let state = sample_state();
        let amount_in = BigUint::from(10u64).pow(18);

        let wrap = state
            .get_amount_out(amount_in.clone(), &steth_token(), &wsteth_token())
            .expect("wrap");
        let wrapped = wrap
            .new_state
            .as_any()
            .downcast_ref::<LidoV4State>()
            .unwrap();
        // `wrap` pulls the stETH in, so the wrapper holds the shares it just minted on top.
        assert_eq!(wrapped.wsteth_shares, state.wsteth_shares + biguint_to_u256(&wrap.amount));

        let unwrap = state
            .get_amount_out(amount_in.clone(), &wsteth_token(), &steth_token())
            .expect("unwrap");
        let unwrapped = unwrap
            .new_state
            .as_any()
            .downcast_ref::<LidoV4State>()
            .unwrap();
        // `unwrap` burns the caller's wstETH and sends the stETH back out.
        assert_eq!(unwrapped.wsteth_shares, state.wsteth_shares - biguint_to_u256(&amount_in));
    }

    /// Draining the wrapper has to close the direction, not leave the bound where it started.
    #[test]
    fn unwrapping_the_whole_wrapper_closes_the_direction() {
        let state = sample_state();
        let (max_in, _) = state
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .expect("limits");

        let drained = state
            .get_amount_out(max_in.clone(), &wsteth_token(), &steth_token())
            .expect("drain");
        let drained = drained
            .new_state
            .as_any()
            .downcast_ref::<LidoV4State>()
            .unwrap();

        assert_eq!(drained.wsteth_shares, U256::ZERO);
        assert_eq!(
            drained
                .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
                .expect("limits"),
            (BigUint::ZERO, BigUint::ZERO)
        );
        // And a second full unwrap is refused rather than quoted again.
        assert!(drained
            .get_amount_out(max_in, &wsteth_token(), &steth_token())
            .is_err());
    }

    /// Every direction reverts on chain at a zero amount.
    #[test]
    fn zero_amount_is_refused_in_every_direction() {
        let state = sample_state();
        for (token_in, token_out) in [
            (eth_token(), steth_token()),
            (eth_token(), wsteth_token()),
            (steth_token(), wsteth_token()),
            (wsteth_token(), steth_token()),
        ] {
            let err = state
                .get_amount_out(BigUint::ZERO, &token_in, &token_out)
                .unwrap_err();
            assert!(
                matches!(err, SimulationError::RecoverableError(ref m) if m == "ZERO_AMOUNT"),
                "{} -> {} quoted a zero amount",
                token_in.symbol,
                token_out.symbol
            );
        }
    }

    #[test]
    fn spot_price_covers_every_tradable_direction() {
        let state = sample_state();

        // Submitting is at parity, and wrapping is the share rate, so the two wstETH legs agree.
        let steth_per_eth = state
            .spot_price(&eth_token(), &steth_token())
            .expect("ETH -> stETH price");
        let wsteth_per_eth = state
            .spot_price(&eth_token(), &wsteth_token())
            .expect("ETH -> wstETH price");
        let wsteth_per_steth = state
            .spot_price(&steth_token(), &wsteth_token())
            .expect("stETH -> wstETH price");
        let steth_per_wsteth = state
            .spot_price(&wsteth_token(), &steth_token())
            .expect("wstETH -> stETH price");

        assert!((steth_per_eth - 1.0).abs() < 1e-6, "submit off parity: {steth_per_eth}");
        assert_eq!(wsteth_per_eth, wsteth_per_steth);
        assert!((wsteth_per_steth * steth_per_wsteth - 1.0).abs() < 1e-9);
    }

    #[test]
    fn spot_price_survives_exhausted_staking_capacity() {
        let mut state = sample_state();
        let mut staking_state = state.staking_state;
        // Capacity gone, but the pair still has a rate: the limit bounds size, not price.
        staking_state.prev_stake_limit = U256::ZERO;
        staking_state.prev_stake_block_number = state.execution_block_number as u32;
        staking_state.max_stake_limit_growth_blocks = 0;
        state.staking_state = staking_state;

        // A quote is unavailable ...
        assert!(state
            .get_amount_out(BigUint::from(10u64).pow(18), &eth_token(), &steth_token())
            .is_err());
        // ... but the price still resolves, for both submit paths.
        assert!(state
            .spot_price(&eth_token(), &steth_token())
            .is_ok());
        assert!(state
            .spot_price(&eth_token(), &wsteth_token())
            .is_ok());
    }

    #[test]
    fn spot_price_rejects_a_token_the_venue_does_not_hold() {
        let weth = Token::new(&Bytes::from([0xc0u8; 20]), "WETH", 18, 0, &[], Chain::Ethereum, 100);

        assert!(sample_state()
            .spot_price(&weth, &steth_token())
            .is_err());
    }

    #[test]
    fn get_limits_unwrap_is_bounded_by_wrapper_shares() {
        let state = sample_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .unwrap();

        // Unwrapping pays out of the wrapper's stETH, so the limit is its share balance - not an
        // unbounded sentinel, which would make callers size trades that revert on chain.
        assert_eq!(max_in, u256_to_biguint(sample_wsteth_shares()));
        assert_eq!(
            max_out,
            u256_to_biguint(
                state
                    .pooled_eth_by_shares(sample_wsteth_shares())
                    .unwrap()
            )
        );
        assert!(max_in < u256_to_biguint(U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE));
    }

    #[test]
    fn get_limits_wrap_is_bounded_by_steth_supply() {
        let state = sample_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(STETH_ADDRESS), Bytes::from(WSTETH_ADDRESS))
            .unwrap();

        // No more stETH can be wrapped than exists.
        let supply = state.internal_ether();
        assert_eq!(max_in, u256_to_biguint(supply));
        assert_eq!(
            max_out,
            u256_to_biguint(
                state
                    .shares_for_pooled_eth(supply)
                    .unwrap()
            )
        );
        assert!(max_in < u256_to_biguint(U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE));
    }

    #[test]
    fn get_limits_unwrap_with_empty_wrapper_returns_zero() {
        let mut state = sample_state();
        state.wsteth_shares = U256::ZERO;

        let (max_in, max_out) = state
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .unwrap();

        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn decoder_reads_wsteth_shares() {
        let state = sample_state();
        assert_eq!(state.wsteth_shares, sample_wsteth_shares());
    }

    #[test]
    fn get_limits_unsupported_direction_returns_zero() {
        let state = sample_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(STETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn get_limits_respects_current_stake_limit() {
        let state = sample_state();
        let (max_in, max_out) = state
            .get_limits(Bytes::from(ETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .unwrap();

        assert_eq!(
            max_in,
            u256_to_biguint(
                state
                    .staking_state
                    .current_limit(state.execution_block_number)
            )
        );
        assert!(max_out > BigUint::ZERO);
    }

    #[test]
    fn paused_staking_blocks_eth_to_steth() {
        let mut state = sample_state();
        let mut staking_state = state.staking_state;
        staking_state.prev_stake_block_number = 0;
        state.staking_state = staking_state;

        let err = state
            .get_amount_out(BigUint::from(10u64).pow(18), &eth_token(), &steth_token())
            .unwrap_err();

        assert!(
            matches!(err, SimulationError::RecoverableError(ref msg) if msg == "STAKING_PAUSED")
        );
    }

    #[test]
    fn delta_transition_updates_state() {
        let mut state = sample_state();
        let new_total = U256::from(999u64);
        let new_external = U256::from(111u64);
        let new_buffered = U256::from(222u64);
        let new_deposited_post_report = U256::from(333u64);
        let new_cl_validators_balance = U256::from(444u64);
        let new_cl_pending_balance = U256::from(555u64);
        let new_staking_state = StakingState {
            prev_stake_block_number: 77,
            prev_stake_limit: U256::from(888u64),
            max_stake_limit_growth_blocks: 9,
            max_stake_limit: U256::from(999u64),
        };

        state
            .delta_transition(
                ProtocolStateDelta {
                    component_id: STETH_COMPONENT_ID.to_string(),
                    updated_attributes: HashMap::from([
                        (TOTAL_SHARES_ATTR.to_string(), attribute(new_total)),
                        (EXTERNAL_SHARES_ATTR.to_string(), attribute(new_external)),
                        (BUFFERED_ETHER_ATTR.to_string(), attribute(new_buffered)),
                        (
                            DEPOSITED_POST_REPORT_ATTR.to_string(),
                            attribute(new_deposited_post_report),
                        ),
                        (
                            CL_VALIDATORS_BALANCE_ATTR.to_string(),
                            attribute(new_cl_validators_balance),
                        ),
                        (CL_PENDING_BALANCE_ATTR.to_string(), attribute(new_cl_pending_balance)),
                        (
                            PREV_STAKE_BLOCK_NUMBER_ATTR.to_string(),
                            attribute(U256::from(new_staking_state.prev_stake_block_number)),
                        ),
                        (
                            PREV_STAKE_LIMIT_ATTR.to_string(),
                            attribute(new_staking_state.prev_stake_limit),
                        ),
                        (
                            MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR.to_string(),
                            attribute(U256::from(new_staking_state.max_stake_limit_growth_blocks)),
                        ),
                        (
                            MAX_STAKE_LIMIT_ATTR.to_string(),
                            attribute(new_staking_state.max_stake_limit),
                        ),
                    ]),
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        assert_eq!(state.total_shares, new_total);
        assert_eq!(state.external_shares, new_external);
        assert_eq!(state.buffered_ether, new_buffered);
        assert_eq!(state.deposited_post_report, new_deposited_post_report);
        assert_eq!(state.cl_validators_balance, new_cl_validators_balance);
        assert_eq!(state.cl_pending_balance, new_cl_pending_balance);
        assert_eq!(state.staking_state, new_staking_state);
    }

    /// `prev_stake_block_number` is a 32-bit field. A five-byte value cannot come from the
    /// package, and narrowing it would wrap the block the stake limit accrues from.
    #[test]
    fn delta_transition_rejects_a_block_number_wider_than_its_field() {
        let mut state = sample_state();
        let before = state.clone();

        let err = state
            .delta_transition(
                ProtocolStateDelta {
                    component_id: STETH_COMPONENT_ID.to_string(),
                    updated_attributes: HashMap::from([
                        (TOTAL_SHARES_ATTR.to_string(), attribute(U256::from(1u64))),
                        (
                            PREV_STAKE_BLOCK_NUMBER_ATTR.to_string(),
                            attribute(U256::from(1u64) << 32),
                        ),
                    ]),
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap_err();

        let TransitionError::DecodeError(message) = err else {
            panic!("expected a decode error, got {err:?}");
        };
        assert!(message.contains(PREV_STAKE_BLOCK_NUMBER_ATTR), "{message}");
        assert_eq!(state, before, "a rejected delta must leave the state untouched");
    }

    #[test]
    fn unwrap_quote_is_bounded_by_wrapper_shares() {
        let state = sample_state();
        let (max_in, _) = state
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .expect("limits");

        // At the limit it quotes ...
        assert!(state
            .get_amount_out(max_in.clone(), &wsteth_token(), &steth_token())
            .is_ok());
        // ... and one wei past it, it refuses rather than quoting a trade the wrapper cannot
        // settle. `get_limits` and `get_amount_out` have to agree on the same bound.
        let err = state
            .get_amount_out(max_in + BigUint::from(1u64), &wsteth_token(), &steth_token())
            .unwrap_err();
        assert!(
            matches!(err, SimulationError::RecoverableError(ref m) if m == "WRAPPER_BALANCE_EXCEEDED")
        );
    }

    #[test]
    fn eth_to_wsteth_mints_the_submitted_shares() {
        let state = sample_state();
        let amount_in = BigUint::from(10u64).pow(18);

        let result = state
            .get_amount_out(amount_in.clone(), &eth_token(), &wsteth_token())
            .expect("quote");

        // `receive()` mints exactly the shares `submit` returned.
        let expected = state
            .shares_for_pooled_eth(biguint_to_u256(&amount_in))
            .unwrap();
        assert_eq!(result.amount, u256_to_biguint(expected));

        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<LidoV4State>()
            .unwrap();
        // The submitted ETH is buffered, the shares are minted, and they land on the wrapper.
        assert_eq!(new_state.buffered_ether, state.buffered_ether + biguint_to_u256(&amount_in));
        assert_eq!(new_state.total_shares, state.total_shares + expected);
        assert_eq!(new_state.wsteth_shares, state.wsteth_shares + expected);
    }

    #[test]
    fn eth_to_wsteth_is_bounded_by_the_stake_limit() {
        let state = sample_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(ETH_ADDRESS), Bytes::from(WSTETH_ADDRESS))
            .expect("limits");

        assert_eq!(
            max_in,
            u256_to_biguint(
                state
                    .staking_state
                    .current_limit(state.execution_block_number)
            )
        );
        assert!(max_out > BigUint::ZERO);
    }

    #[test]
    fn eth_to_wsteth_beats_routing_through_steth() {
        let state = sample_state();
        let amount_in = BigUint::from(10u64).pow(18);

        let direct = state
            .get_amount_out(amount_in.clone(), &eth_token(), &wsteth_token())
            .expect("direct");
        // The two-hop route: submit on the stETH component, then wrap on this one.
        let submitted = sample_state()
            .get_amount_out(amount_in, &eth_token(), &steth_token())
            .expect("submit");
        let wrapped = state
            .get_amount_out(submitted.amount, &steth_token(), &wsteth_token())
            .expect("wrap");

        assert!(direct.amount >= wrapped.amount, "shortcut must not quote worse");
        assert!(direct.gas < submitted.gas + wrapped.gas, "shortcut must be cheaper");
    }

    /// One component carries three tokens, so a caller can ask for any of six orderings. Only
    /// the four the venue performs may quote; the two that would unstake have to report a zero
    /// limit and refuse to price or swap, in every method, or a router builds a leg that cannot
    /// settle.
    #[test]
    fn component_serves_only_the_four_directions_the_venue_performs() {
        let state = sample_state();
        let amount = BigUint::from(10u64).pow(18);

        let tradable = [
            (eth_token(), steth_token()),
            (steth_token(), wsteth_token()),
            (wsteth_token(), steth_token()),
            (eth_token(), wsteth_token()),
        ];
        // Unstaking runs through the asynchronous withdrawal queue.
        let untradable = [(steth_token(), eth_token()), (wsteth_token(), eth_token())];

        for (token_in, token_out) in &tradable {
            let pair = format!("{} -> {}", token_in.symbol, token_out.symbol);
            let (max_in, max_out) = state
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .unwrap_or_else(|e| panic!("{pair} limits: {e:?}"));
            assert!(max_in > BigUint::ZERO, "{pair} has no input capacity");
            assert!(max_out > BigUint::ZERO, "{pair} has no output capacity");
            assert!(
                state
                    .spot_price(token_in, token_out)
                    .is_ok(),
                "{pair} has no price"
            );
            assert!(
                state
                    .get_amount_out(amount.clone(), token_in, token_out)
                    .is_ok(),
                "{pair} does not quote"
            );
        }

        // A token the component does not hold is a different answer from "no capacity".
        let weth = Token::new(&Bytes::from([0xc0u8; 20]), "WETH", 18, 0, &[], Chain::Ethereum, 100);
        assert!(
            state
                .get_limits(weth.address.clone(), steth_token().address.clone())
                .is_err(),
            "an unknown token reported a limit instead of an error"
        );
        assert!(state
            .spot_price(&weth, &steth_token())
            .is_err());
        assert!(state
            .get_amount_out(amount.clone(), &weth, &steth_token())
            .is_err());

        for (token_in, token_out) in &untradable {
            let pair = format!("{} -> {}", token_in.symbol, token_out.symbol);
            assert_eq!(
                state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .unwrap_or_else(|e| panic!("{pair} limits: {e:?}")),
                (BigUint::ZERO, BigUint::ZERO),
                "{pair} reports capacity it cannot settle"
            );
            assert!(
                state
                    .spot_price(token_in, token_out)
                    .is_err(),
                "{pair} has a price"
            );
            assert!(
                state
                    .get_amount_out(amount.clone(), token_in, token_out)
                    .is_err(),
                "{pair} quotes a swap the venue cannot perform"
            );
        }
    }

    /// State read from stETH storage at the Lido v4 migration block 25603297. Pricing
    /// `sharesOf(wstETH)` at the share rate has to land on `stETH.balanceOf(wstETH)` from the
    /// same block, which pins the v4 pooled-ether formula to the chain.
    #[test]
    fn share_rate_matches_chain_on_the_v4_storage_layout() {
        let state = LidoV4State::new(
            25_603_297,
            U256::from_str_radix("7526667021904051320418763", 10).unwrap(),
            U256::from_str_radix("3721126242498807385407", 10).unwrap(),
            U256::from_str_radix("539569870340371095571", 10).unwrap(),
            U256::from_str_radix("761440000000000000000000", 10).unwrap(),
            U256::from_str_radix("8567227049119653000000000", 10).unwrap(),
            U256::ZERO,
            sample_staking_state(),
            U256::from_str_radix("3628434125893615122886002", 10).unwrap(),
        );

        let wsteth_backing = state
            .pooled_eth_by_shares(state.wsteth_shares)
            .unwrap();

        assert_eq!(wsteth_backing, U256::from_str_radix("4499621841408863271318368", 10).unwrap());
    }

    #[test]
    fn unsupported_direction_errors() {
        let err = sample_state()
            .get_amount_out(BigUint::from(10u64).pow(18), &steth_token(), &eth_token())
            .unwrap_err();
        assert!(matches!(err, SimulationError::FatalError(_)));
    }

    #[tokio::test]
    async fn decoder_seeds_the_execution_block_from_the_header() {
        let snapshot = snapshot();
        let state = LidoV4State::try_from_with_header(
            snapshot,
            BlockHeader {
                number: 123,
                timestamp: 456,
                hash: Bytes::new(),
                parent_hash: Bytes::new(),
                revert: false,
                partial_block_index: None,
            },
            &HashMap::new(),
            &HashMap::new(),
            &Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(state.execution_block_number, 123);
    }

    /// The stake limit accrues per block, so an idle block still has to move it - that is the
    /// whole reason the block cannot come in on a delta.
    #[test]
    fn apply_block_accrues_the_stake_limit_without_a_delta() {
        let mut state = sample_state();
        // The fixture starts at `max_stake_limit`, where nothing can accrue. A partly consumed
        // limit is the case this guards.
        state.staking_state.prev_stake_limit = state.staking_state.max_stake_limit / U256::from(2);
        let limit_before = state
            .staking_state
            .current_limit(state.execution_block_number);

        let changed = state.apply_block(&BlockContext::new(state.execution_block_number + 1, 0));

        assert!(changed, "an accruing limit must re-emit");
        assert!(
            state
                .staking_state
                .current_limit(state.execution_block_number) >
                limit_before
        );
    }

    #[test]
    fn apply_block_is_idempotent_for_a_repeated_block() {
        let mut state = sample_state();
        let block = BlockContext::new(state.execution_block_number, 0);

        assert!(!state.apply_block(&block));
        assert!(!state.apply_block(&block));
    }

    /// Once the limit sits at `max_stake_limit` it cannot grow further, so later blocks must not
    /// keep re-emitting the state to consumers.
    #[test]
    fn apply_block_does_not_re_emit_once_the_limit_is_saturated() {
        let mut state = sample_state();
        state.staking_state.prev_stake_limit = state.staking_state.max_stake_limit / U256::from(2);
        state.apply_block(&BlockContext::new(state.execution_block_number + 10_000_000, 0));
        let saturated = state
            .staking_state
            .current_limit(state.execution_block_number);
        assert_eq!(saturated, state.staking_state.max_stake_limit);

        let changed = state.apply_block(&BlockContext::new(state.execution_block_number + 1, 0));

        assert!(!changed);
    }
}
