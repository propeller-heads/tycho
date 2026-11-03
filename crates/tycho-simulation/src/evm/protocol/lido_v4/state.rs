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

// Gas each venue call costs, measured on a mainnet fork at block 25990000 from an account
// trading for the first time, so every balance slot the call touches is cold. The 21,000
// intrinsic and the calldata are left out: the router pays those once for the whole
// transaction, and `estimate_gas_usage` adds the transfers around the leg separately.
//
// `wstETH.receive()` submits ETH and mints wstETH in one call. `wrap` additionally reads and
// updates the stETH allowance during `transferFrom`.
const SUBMIT_GAS: u64 = 83_000;
const SUBMIT_AND_WRAP_GAS: u64 = 97_000;
const WRAP_GAS: u64 = 103_000;
const UNWRAP_GAS: u64 = 80_000;

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
/// minimal-length values, so a wider one is malformed, and the `Err` names it for the caller to
/// report.
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

    /// `stETH.totalSupply()`: `Lido._getTotalPooledEther()`, the internal ether plus the ether
    /// backing shares minted outside the protocol, valued at the internal share rate.
    fn total_pooled_ether(&self) -> Result<U256, SimulationError> {
        let external_ether = self.pooled_eth_by_shares(self.external_shares)?;
        safe_add_u256(self.internal_ether(), external_ether)
    }

    /// Caps deposits by staking capacity and the remaining uint128 storage capacity.
    fn deposit_limit(&self) -> Result<U256, SimulationError> {
        let cap = U256::from(u128::MAX);
        let max_input = cap - U256::ONE;
        let shares = self.internal_shares()?;
        if shares.is_zero() || self.internal_ether().is_zero() {
            return Err(SimulationError::FatalError("invalid Lido share rate state".to_string()));
        }
        let share_headroom = safe_sub_u256(cap, self.total_shares)?;
        // The product can exceed 256 bits. Floor division gives a conservative deposit
        // whose minted shares fit the field; cap before converting back to U256.
        let share_capacity = u256_to_biguint(share_headroom) *
            u256_to_biguint(self.internal_ether()) /
            u256_to_biguint(shares);
        let share_capacity = biguint_to_u256(&share_capacity.min(u256_to_biguint(max_input)));
        Ok(self
            .staking_state
            .current_limit(self.execution_block_number)
            .min(max_input)
            .min(safe_sub_u256(cap, self.buffered_ether)?)
            .min(share_capacity))
    }

    fn check_deposit_limit(&self, amount: U256) -> Result<(), SimulationError> {
        if amount > self.deposit_limit()? {
            return Err(SimulationError::RecoverableError("DEPOSIT_LIMIT".to_string()));
        }
        Ok(())
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
        self.check_deposit_limit(amount_in)?;
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
        if amount_in > self.total_pooled_ether()? {
            return Err(SimulationError::RecoverableError("STETH_SUPPLY_EXCEEDED".to_string()));
        }
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
    /// shares `submit` returned, so the output is that share count.
    fn amount_out_eth_to_wsteth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let shares_amount = self.shares_for_pooled_eth(amount_in)?;
        let mut new_state = self.clone();
        new_state
            .staking_state
            .decrease(amount_in, new_state.execution_block_number)?;
        self.check_deposit_limit(amount_in)?;
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

    fn unwrap_limit(&self) -> Result<U256, SimulationError> {
        let max_input = U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE;
        // stETH.transfer converts its nominal amount back to shares and bounds that amount.
        Ok(self
            .wsteth_shares
            .min(max_input)
            .min(self.shares_for_pooled_eth(max_input)?))
    }

    fn amount_out_wsteth_to_steth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // The wrapper must hold enough shares, and the nominal stETH transfer must fit
        // the contract's conversion bound. `get_limits` applies the same cap.
        if amount_in > self.unwrap_limit()? {
            return Err(SimulationError::RecoverableError("UNWRAP_LIMIT".to_string()));
        }
        let amount_out = self.pooled_eth_by_shares(amount_in)?;
        // `unwrap` burns the caller's wstETH and pays out `amount_out` stETH, and `transfer`
        // re-derives the shares that amount is worth. Both conversions round down, so what
        // leaves the wrapper is what the round trip resolves to.
        let mut new_state = self.clone();
        let shares_paid_out = self.shares_for_pooled_eth(amount_out)?;
        new_state.wsteth_shares = safe_sub_u256(new_state.wsteth_shares, shares_paid_out)?;
        // A receiver's balance increase is at least the value of the transferred shares.
        let amount_out = self.pooled_eth_by_shares(shares_paid_out)?;
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
            // Submitting mints shares, and the depositor holds the stETH balance those shares
            // are worth. Taken from the share rate, which holds while staking is paused or its
            // limit is exhausted: those bound capacity, not price.
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
        if amount_in.bits() > 128 {
            return Err(SimulationError::InvalidInput(
                "amount exceeds uint128 bound".to_string(),
                None,
            ));
        }
        let amount_in = biguint_to_u256(&amount_in);
        validate_u128_bound("amount", amount_in)?;
        // Every direction reverts on a zero amount: `submit` with ZERO_DEPOSIT, and the wrapper
        // with its own zero-amount guards.
        if amount_in.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }

        let result = match (token_in.address.as_ref(), token_out.address.as_ref()) {
            (ETH, STETH) => self.amount_out_eth_to_steth(amount_in),
            (STETH, WSTETH) => self.amount_out_steth_to_wsteth(amount_in),
            (WSTETH, STETH) => self.amount_out_wsteth_to_steth(amount_in),
            (ETH, WSTETH) => self.amount_out_eth_to_wsteth(amount_in),
            _ => Err(SimulationError::FatalError("unsupported swap".to_string())),
        }?;
        if result.amount == BigUint::ZERO {
            return Err(SimulationError::RecoverableError("ZERO_OUTPUT".to_string()));
        }
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let max_input = U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE;
        let max_sell = match (sell_token.as_ref(), buy_token.as_ref()) {
            (ETH, STETH | WSTETH) => self.deposit_limit()?,
            (STETH, WSTETH) => self
                .total_pooled_ether()?
                .min(max_input),
            (WSTETH, STETH) => self.unwrap_limit()?,
            // Unstaking requires the asynchronous withdrawal queue.
            (STETH, ETH) | (WSTETH, ETH) => U256::ZERO,
            _ => return Err(SimulationError::FatalError("unsupported swap".to_string())),
        };
        if max_sell.is_zero() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let max_buy = match (sell_token.as_ref(), buy_token.as_ref()) {
            (ETH, STETH) => {
                self.amount_out_eth_to_steth(max_sell)?
                    .amount
            }
            (ETH, WSTETH) => {
                self.amount_out_eth_to_wsteth(max_sell)?
                    .amount
            }
            (STETH, WSTETH) => {
                self.amount_out_steth_to_wsteth(max_sell)?
                    .amount
            }
            (WSTETH, STETH) => {
                self.amount_out_wsteth_to_steth(max_sell)?
                    .amount
            }
            _ => return Err(SimulationError::FatalError("unsupported swap".to_string())),
        };
        if max_buy == BigUint::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        Ok((u256_to_biguint(max_sell), max_buy))
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

        // Decode all fields before mutation so invalid deltas leave a consistent quote state.
        let total_shares = read(TOTAL_SHARES_ATTR)?;
        let external_shares = read(EXTERNAL_SHARES_ATTR)?;
        let buffered_ether = read(BUFFERED_ETHER_ATTR)?;
        let deposited_post_report = read(DEPOSITED_POST_REPORT_ATTR)?;
        let cl_validators_balance = read(CL_VALIDATORS_BALANCE_ATTR)?;
        let cl_pending_balance = read(CL_PENDING_BALANCE_ATTR)?;
        let wsteth_shares = read(WSTETH_SHARES_ATTR)?;
        let prev_stake_block_number = read_u32(PREV_STAKE_BLOCK_NUMBER_ATTR)?;
        let prev_stake_limit = read(PREV_STAKE_LIMIT_ATTR)?;
        let max_stake_limit_growth_blocks = read_u32(MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR)?;
        let max_stake_limit = read(MAX_STAKE_LIMIT_ATTR)?;

        if let Some(value) = total_shares {
            self.total_shares = value;
        }
        if let Some(value) = external_shares {
            self.external_shares = value;
        }
        if let Some(value) = buffered_ether {
            self.buffered_ether = value;
        }
        if let Some(value) = deposited_post_report {
            self.deposited_post_report = value;
        }
        if let Some(value) = cl_validators_balance {
            self.cl_validators_balance = value;
        }
        if let Some(value) = cl_pending_balance {
            self.cl_pending_balance = value;
        }
        if let Some(value) = wsteth_shares {
            self.wsteth_shares = value;
        }
        if let Some(value) = prev_stake_block_number {
            self.staking_state
                .prev_stake_block_number = value;
        }
        if let Some(value) = prev_stake_limit {
            self.staking_state.prev_stake_limit = value;
        }
        if let Some(value) = max_stake_limit_growth_blocks {
            self.staking_state
                .max_stake_limit_growth_blocks = value;
        }
        if let Some(value) = max_stake_limit {
            self.staking_state.max_stake_limit = value;
        }
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
    /// The attributes the component carries, in the order the decoder reads them.
    pub(super) const COMPONENT_ATTRS: [&str; 11] = [
        TOTAL_SHARES_ATTR,
        EXTERNAL_SHARES_ATTR,
        BUFFERED_ETHER_ATTR,
        DEPOSITED_POST_REPORT_ATTR,
        CL_VALIDATORS_BALANCE_ATTR,
        CL_PENDING_BALANCE_ATTR,
        PREV_STAKE_BLOCK_NUMBER_ATTR,
        PREV_STAKE_LIMIT_ATTR,
        MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR,
        MAX_STAKE_LIMIT_ATTR,
        WSTETH_SHARES_ATTR,
    ];
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

    /// The width of every attribute is the width of the stETH field it is unpacked from, so each
    /// one has to be pinned on its own: a width that is too generous accepts a value the field
    /// cannot hold, and one that is too tight rejects a value the package legitimately emits.
    #[test]
    fn every_attribute_is_read_at_the_width_of_its_field() {
        let widths: HashMap<&str, usize> = HashMap::from([
            (TOTAL_SHARES_ATTR, 16),
            (EXTERNAL_SHARES_ATTR, 16),
            (BUFFERED_ETHER_ATTR, 16),
            (DEPOSITED_POST_REPORT_ATTR, 16),
            (CL_VALIDATORS_BALANCE_ATTR, 16),
            (CL_PENDING_BALANCE_ATTR, 16),
            (PREV_STAKE_BLOCK_NUMBER_ATTR, 4),
            (MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR, 4),
            (PREV_STAKE_LIMIT_ATTR, 12),
            (MAX_STAKE_LIMIT_ATTR, 12),
            (WSTETH_SHARES_ATTR, 32),
        ]);
        assert_eq!(widths.len(), COMPONENT_ATTRS.len());

        for name in COMPONENT_ATTRS {
            let width = widths[name];
            decode_attribute(name, &vec![0xffu8; width])
                .unwrap_or_else(|e| panic!("{name} rejects a full {width}-byte field: {e}"));
            let err = decode_attribute(name, &vec![0xffu8; width + 1])
                .expect_err("a value wider than the field is malformed");
            assert!(err.contains(name), "{err}");
        }
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
        // `unwrap` pays the stETH out and `transfer` re-derives the shares it is worth. Both
        // conversions round down, so the wrapper keeps one wei-share of what was burnt.
        assert_eq!(
            unwrapped.wsteth_shares,
            state.wsteth_shares - biguint_to_u256(&amount_in) + U256::ONE
        );
    }

    /// Draining the wrapper moves the bound down to what is left, which is the dust the two
    /// roundings strand and stETH cannot pay out.
    #[test]
    fn unwrapping_the_whole_wrapper_strands_the_rounding_dust() {
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

        assert_eq!(drained.wsteth_shares, U256::ONE);
        let (drained_max_in, _) = drained
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .expect("limits");
        assert_eq!(drained_max_in, BigUint::ZERO);
        // And a second unwrap of the original size is refused.
        assert!(drained
            .get_amount_out(max_in, &wsteth_token(), &steth_token())
            .is_err());
    }

    #[test]
    fn unwrap_quotes_transferred_shares_and_rejects_zero_receipts() {
        let mut state = sample_state();
        state.total_shares = U256::from(10);
        state.external_shares = U256::ZERO;
        state.buffered_ether = U256::from(15);
        state.deposited_post_report = U256::ZERO;
        state.cl_validators_balance = U256::ZERO;
        state.cl_pending_balance = U256::ZERO;
        state.wsteth_shares = U256::from(3);
        let quote = state
            .get_amount_out(BigUint::from(3u8), &wsteth_token(), &steth_token())
            .unwrap();
        assert_eq!(quote.amount, BigUint::from(3u8));
        assert_eq!(
            state
                .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
                .unwrap(),
            (BigUint::from(3u8), BigUint::from(3u8))
        );
        assert!(
            matches!(state.get_amount_out(BigUint::from(1u8), &wsteth_token(), &steth_token()),
            Err(SimulationError::RecoverableError(message)) if message == "ZERO_OUTPUT")
        );
        state.wsteth_shares = U256::ONE;
        assert_eq!(
            state
                .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    #[test]
    fn unwrap_limit_respects_the_steth_transfer_amount_bound() {
        let mut state = sample_state();
        let cap = U256::from(u128::MAX);
        state.total_shares = cap / U256::from(2);
        state.external_shares = U256::ZERO;
        state.buffered_ether = cap;
        state.deposited_post_report = U256::ZERO;
        state.cl_validators_balance = U256::ZERO;
        state.cl_pending_balance = U256::ZERO;
        state.wsteth_shares = state.total_shares;
        let (max_sell, max_buy) = state
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .unwrap();
        assert_eq!(max_sell, u256_to_biguint(state.total_shares - U256::ONE));
        assert_eq!(max_buy, u256_to_biguint(cap - U256::from(5)));
        let quote = state
            .get_amount_out(max_sell.clone(), &wsteth_token(), &steth_token())
            .unwrap();
        assert_eq!(quote.amount, max_buy);
        assert!(state
            .get_amount_out(max_sell + BigUint::from(1u8), &wsteth_token(), &steth_token())
            .is_err());
    }

    #[test]
    fn oversized_input_returns_an_error() {
        assert!(matches!(
            sample_state().get_amount_out(
                BigUint::from(1u8) << 256usize,
                &eth_token(),
                &steth_token()
            ),
            Err(SimulationError::InvalidInput(_, _))
        ));
    }

    #[test]
    fn wrap_refuses_more_than_the_reported_supply() {
        let state = sample_state();
        let (limit, _) = state
            .get_limits(Bytes::from(STETH_ADDRESS), Bytes::from(WSTETH_ADDRESS))
            .unwrap();
        assert!(state
            .get_amount_out(limit + BigUint::from(1u8), &steth_token(), &wsteth_token())
            .is_err());
    }

    #[test]
    fn mint_limits_respect_storage_headroom() {
        let cap = U256::from(u128::MAX);
        for (shares, buffer, validators, expected_limit) in [
            (cap - U256::from(10), U256::ONE, cap - U256::from(11), 10u64),
            (cap - U256::from(100), cap - U256::from(10), U256::ZERO, 10),
            (cap - U256::from(10), U256::from(100), U256::ZERO, 0),
            (U256::from(100), cap - U256::from(10), U256::ZERO, 0),
        ] {
            let mut state = sample_state();
            state.total_shares = shares;
            state.external_shares = U256::ZERO;
            state.buffered_ether = buffer;
            state.deposited_post_report = U256::ZERO;
            state.cl_validators_balance = validators;
            state.cl_pending_balance = U256::ZERO;
            state.staking_state.max_stake_limit = U256::ZERO;
            for output in [steth_token(), wsteth_token()] {
                let (limit, _) = state
                    .get_limits(Bytes::from(ETH_ADDRESS), output.address.clone())
                    .unwrap();
                assert_eq!(limit, BigUint::from(expected_limit));
                if limit != BigUint::ZERO {
                    let quote = state
                        .get_amount_out(limit.clone(), &eth_token(), &output)
                        .unwrap();
                    let next = quote
                        .new_state
                        .as_any()
                        .downcast_ref::<LidoV4State>()
                        .unwrap();
                    assert!(next.total_shares <= cap);
                    assert!(next.buffered_ether <= cap);
                }
                assert!(state
                    .get_amount_out(limit + BigUint::from(1u8), &eth_token(), &output)
                    .is_err());
            }
        }
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

        // Unwrapping pays out of the wrapper's stETH, so the limit is its share balance.
        assert_eq!(max_in, u256_to_biguint(sample_wsteth_shares()));
        assert_eq!(
            max_out,
            u256_to_biguint(
                state
                    .pooled_eth_by_shares(
                        state
                            .shares_for_pooled_eth(
                                state
                                    .pooled_eth_by_shares(sample_wsteth_shares())
                                    .unwrap()
                            )
                            .unwrap()
                    )
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

        // No more stETH can be wrapped than exists, and what exists is `totalSupply()`: the
        // internal ether plus the ether backing the externally minted shares.
        let supply = U256::from_str_radix("21803278404946205780741210", 10).expect("supply");
        assert_eq!(max_in, u256_to_biguint(supply));
        assert!(max_in > u256_to_biguint(state.internal_ether()), "external ether is missing");
        // Wrapping the whole supply mints every share but the one the round trip rounds away.
        assert_eq!(max_out, u256_to_biguint(state.total_shares - U256::ONE));
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
        // ... and one wei past it, it refuses. `get_limits` and `get_amount_out` have to agree
        // on the same bound.
        let err = state
            .get_amount_out(max_in + BigUint::from(1u64), &wsteth_token(), &steth_token())
            .unwrap_err();
        assert!(matches!(err, SimulationError::RecoverableError(ref m) if m == "UNWRAP_LIMIT"));
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

    fn every_token_pair() -> Vec<(Bytes, Bytes)> {
        let tokens = [ETH_ADDRESS, STETH_ADDRESS, WSTETH_ADDRESS];
        let mut pairs = Vec::new();
        for sell in tokens {
            for buy in tokens {
                if sell != buy {
                    pairs.push((Bytes::from(sell), Bytes::from(buy)));
                }
            }
        }
        pairs
    }

    /// A reported limit has to be a trade the venue performs: quoting at it must succeed and
    /// return exactly the reported output.
    #[test]
    fn every_reported_limit_quotes_at_its_own_size() {
        let state = sample_state();
        for (sell, buy) in every_token_pair() {
            let (max_in, max_out) = state
                .get_limits(sell.clone(), buy.clone())
                .expect("a pair the component holds");
            if max_in == BigUint::ZERO {
                assert_eq!(max_out, BigUint::ZERO, "{sell:x} -> {buy:x} pays out of a zero limit");
                continue;
            }
            let token_in = Token::new(&sell, "in", 18, 0, &[], Chain::Ethereum, 100);
            let token_out = Token::new(&buy, "out", 18, 0, &[], Chain::Ethereum, 100);
            let quoted = state
                .get_amount_out(max_in.clone(), &token_in, &token_out)
                .unwrap_or_else(|e| panic!("{sell:x} -> {buy:x} limit does not quote: {e:?}"));
            assert_eq!(quoted.amount, max_out, "{sell:x} -> {buy:x} limit disagrees with quote");
        }
    }

    /// The share rate is applied in one direction or the other, and the two are inverse up to
    /// their rounding, so neither can be applied to a figure already in the other unit.
    #[test]
    fn shares_and_pooled_ether_round_trip() {
        let state = sample_state();
        // Each division truncates by under one unit, and the first loss is then scaled
        // by the share rate, so a round trip can lose the rate plus one.
        let tolerance = state
            .pooled_eth_by_shares(U256::ONE)
            .expect("rate") +
            U256::from(2u8);
        for exponent in [15u32, 18, 21, 24] {
            let amount = U256::from(10u64).pow(U256::from(exponent));
            let back = state
                .pooled_eth_by_shares(
                    state
                        .shares_for_pooled_eth(amount)
                        .expect("shares"),
                )
                .expect("amount");
            assert!(back <= amount && amount - back <= tolerance, "amount drifted at 1e{exponent}");

            let shares = U256::from(10u64).pow(U256::from(exponent));
            let back = state
                .shares_for_pooled_eth(
                    state
                        .pooled_eth_by_shares(shares)
                        .expect("amount"),
                )
                .expect("shares");
            assert!(back <= shares && shares - back <= tolerance, "shares drifted at 1e{exponent}");
        }
    }

    /// The decoder requires every name the component carries, so a value the package emits
    /// cannot be left unread.
    #[tokio::test]
    async fn decoder_requires_every_attribute_the_component_carries() {
        assert_eq!(snapshot().state.attributes.len(), COMPONENT_ATTRS.len());
        for name in COMPONENT_ATTRS {
            let mut snapshot = snapshot();
            snapshot.state.attributes.remove(name);
            let err = try_decode_snapshot_with_defaults::<LidoV4State>(snapshot)
                .await
                .unwrap_err();
            let InvalidSnapshotError::MissingAttribute(missing) = err else {
                panic!("{name} removed but the decoder did not report it: {err:?}");
            };
            assert_eq!(missing, name);
        }
    }

    /// A delta and a fresh snapshot of the same attributes produce identical state.
    #[tokio::test]
    async fn delta_transition_applies_every_attribute_the_component_carries() {
        let base = try_decode_snapshot_with_defaults::<LidoV4State>(snapshot())
            .await
            .unwrap();
        for name in COMPONENT_ATTRS {
            let mut state = base.clone();
            state
                .delta_transition(
                    ProtocolStateDelta {
                        component_id: STETH_COMPONENT_ID.to_string(),
                        updated_attributes: HashMap::from([(
                            name.to_string(),
                            attribute(U256::from(7u64)),
                        )]),
                        deleted_attributes: Default::default(),
                    },
                    &HashMap::new(),
                    &Balances::default(),
                )
                .unwrap_or_else(|e| panic!("{name} was rejected: {e:?}"));
            let mut updated = snapshot();
            updated
                .state
                .attributes
                .insert(name.to_string(), attribute(U256::from(7u64)));
            let expected = try_decode_snapshot_with_defaults::<LidoV4State>(updated)
                .await
                .unwrap();
            assert_eq!(state, expected, "{name} updated the wrong state");
        }
    }

    #[test]
    fn delta_transition_ignores_unknown_attributes() {
        let mut state = sample_state();
        let expected = state.clone();
        state
            .delta_transition(
                ProtocolStateDelta {
                    component_id: STETH_COMPONENT_ID.to_string(),
                    updated_attributes: HashMap::from([(
                        "future_parameter".to_string(),
                        Bytes::from(vec![0xff; 64]),
                    )]),
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert_eq!(state, expected);
    }

    /// The stream decoder puts the chain head in every delta. Those names are not Lido
    /// attributes and leave the state alone.
    #[test]
    fn delta_transition_accepts_the_injected_block_attributes() {
        let mut state = sample_state();
        state
            .delta_transition(
                ProtocolStateDelta {
                    component_id: STETH_COMPONENT_ID.to_string(),
                    updated_attributes: HashMap::from([
                        (
                            "block_number".to_string(),
                            Bytes::from(24_083_114u64.to_be_bytes().to_vec()),
                        ),
                        (
                            "block_timestamp".to_string(),
                            Bytes::from(1_700_000_000u64.to_be_bytes().to_vec()),
                        ),
                    ]),
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("the injected names are tolerated");
        assert_eq!(state, sample_state());
    }
}
