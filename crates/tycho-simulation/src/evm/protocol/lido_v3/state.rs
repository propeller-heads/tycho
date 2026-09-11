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
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64};

pub const STETH_COMPONENT_ID: &str = "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84";
pub const WSTETH_COMPONENT_ID: &str = "0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0";

pub const STETH_ADDRESS: [u8; 20] = hex!("ae7ab96520de3a18e5e111b5eaab095312d7fe84");
pub const WSTETH_ADDRESS: [u8; 20] = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0");
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");

pub const TOTAL_AND_EXTERNAL_SHARES_ATTR: &str = "total_and_external_shares";
pub const BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR: &str =
    "buffered_ether_and_deposited_validators";
pub const CL_BALANCE_AND_CL_VALIDATORS_ATTR: &str = "cl_balance_and_cl_validators";
pub const STAKING_STATE_ATTR: &str = "staking_state";
pub const WSTETH_SHARES_ATTR: &str = "wsteth_shares";

const DEPOSIT_SIZE: u128 = 32_000_000_000_000_000_000;
const UINT128_MAX_EXCLUSIVE: u128 = u128::MAX;

const SUBMIT_GAS: u64 = 160_000;
const WRAP_GAS: u64 = 81_000;
const UNWRAP_GAS: u64 = 66_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LidoV3PoolKind {
    StEth,
    WstEth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LidoV3State {
    kind: LidoV3PoolKind,
    block_number: u64,
    block_timestamp: u64,
    total_shares: U256,
    external_shares: U256,
    buffered_ether: U256,
    deposited_validators: U256,
    cl_balance: U256,
    cl_validators: U256,
    staking_state: Option<StakingState>,
    /// `sharesOf(wstETH)`, i.e. the shares the wrapper holds. Only set for the wstETH component,
    /// where it bounds how much can be unwrapped.
    wsteth_shares: Option<U256>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StakingState {
    prev_stake_block_number: u32,
    prev_stake_limit: U256,
    max_stake_limit_growth_blocks: u32,
    max_stake_limit: U256,
}

impl LidoV3State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: LidoV3PoolKind,
        block_number: u64,
        block_timestamp: u64,
        total_shares: U256,
        external_shares: U256,
        buffered_ether: U256,
        deposited_validators: U256,
        cl_balance: U256,
        cl_validators: U256,
        staking_state: Option<StakingState>,
        wsteth_shares: Option<U256>,
    ) -> Self {
        Self {
            kind,
            block_number,
            block_timestamp,
            total_shares,
            external_shares,
            buffered_ether,
            deposited_validators,
            cl_balance,
            cl_validators,
            staking_state,
            wsteth_shares,
        }
    }

    pub(crate) fn split_low_high_u128(value: U256) -> (U256, U256) {
        let mask = U256::from(u128::MAX);
        (value & mask, (value >> 128u32) & mask)
    }

    fn validate_u128_bound(name: &str, value: U256) -> Result<(), SimulationError> {
        if value >= U256::from(UINT128_MAX_EXCLUSIVE) {
            return Err(SimulationError::InvalidInput(
                format!("{name} exceeds uint128 bound"),
                None,
            ));
        }
        Ok(())
    }

    fn internal_shares(&self) -> Result<U256, SimulationError> {
        if self.external_shares > self.total_shares {
            return Err(SimulationError::FatalError(
                "external shares exceed total shares".to_string(),
            ));
        }
        Ok(self.total_shares - self.external_shares)
    }

    fn transient_ether(&self) -> Result<U256, SimulationError> {
        if self.cl_validators > self.deposited_validators {
            return Err(SimulationError::FatalError(
                "cl validators exceed deposited validators".to_string(),
            ));
        }
        Ok((self.deposited_validators - self.cl_validators) * U256::from(DEPOSIT_SIZE))
    }

    fn internal_ether(&self) -> Result<U256, SimulationError> {
        Ok(self.buffered_ether + self.cl_balance + self.transient_ether()?)
    }

    fn shares_for_pooled_eth(&self, eth_amount: U256) -> Result<U256, SimulationError> {
        Self::validate_u128_bound("eth amount", eth_amount)?;
        let denominator = self.internal_shares()?;
        let numerator = self.internal_ether()?;
        if denominator.is_zero() || numerator.is_zero() {
            return Err(SimulationError::FatalError("invalid Lido share rate state".to_string()));
        }
        Ok((eth_amount * denominator) / numerator)
    }

    fn pooled_eth_by_shares(&self, shares_amount: U256) -> Result<U256, SimulationError> {
        Self::validate_u128_bound("shares amount", shares_amount)?;
        let numerator = self.internal_ether()?;
        let denominator = self.internal_shares()?;
        if denominator.is_zero() || numerator.is_zero() {
            return Err(SimulationError::FatalError("invalid Lido share rate state".to_string()));
        }
        Ok((shares_amount * numerator) / denominator)
    }

    fn wsteth_shares(&self) -> Result<U256, SimulationError> {
        self.wsteth_shares.ok_or_else(|| {
            SimulationError::FatalError("missing wstETH shares for wstETH component".to_string())
        })
    }

    fn staking_state(&self) -> Result<StakingState, SimulationError> {
        self.staking_state.ok_or_else(|| {
            SimulationError::FatalError("missing staking state for stETH component".to_string())
        })
    }

    fn decrease_staking_limit(&mut self, amount: U256) -> Result<(), SimulationError> {
        let mut staking_state = self.staking_state()?;
        staking_state.decrease(amount, self.block_number)?;
        self.staking_state = Some(staking_state);
        Ok(())
    }

    fn steth_spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // The pair has a rate in both directions even though only ETH -> stETH is tradable:
        // unstaking goes through the withdrawal queue, which `get_limits` reports as a zero
        // limit. Quoting only one ordering leaves callers that key their data by swap direction
        // without a price for the direction that does trade.
        let (eth, steth, invert) = if base.address.as_ref() == ETH_ADDRESS &&
            quote.address.as_ref() == STETH_ADDRESS
        {
            (base, quote, false)
        } else if base.address.as_ref() == STETH_ADDRESS && quote.address.as_ref() == ETH_ADDRESS {
            (quote, base, true)
        } else {
            return Err(SimulationError::FatalError("unsupported spot price".to_string()));
        };

        // Submitting ETH mints shares, and the depositor holds the stETH balance those shares
        // are worth. Derived from the share rate rather than from a `get_amount_out` probe, so
        // the rate stays available while staking is paused or its limit is exhausted - those
        // bound capacity, not price.
        let eth_unit = U256::from(10).pow(U256::from(eth.decimals));
        let steth_out = self.pooled_eth_by_shares(self.shares_for_pooled_eth(eth_unit)?)?;
        let steth_unit_f64 = u256_to_f64(U256::from(10).pow(U256::from(steth.decimals)))?;
        let steth_per_eth = u256_to_f64(steth_out)? / steth_unit_f64;

        if !invert {
            return Ok(steth_per_eth);
        }
        if steth_per_eth == 0.0 {
            return Err(SimulationError::FatalError("invalid Lido share rate state".to_string()));
        }
        Ok(1.0 / steth_per_eth)
    }

    fn wsteth_spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let quote_unit_f64 = u256_to_f64(U256::from(10).pow(U256::from(quote.decimals)))?;
        let to_price = |amount_out: U256| -> Result<f64, SimulationError> {
            Ok(u256_to_f64(amount_out)? / quote_unit_f64)
        };

        if base.address.as_ref() == WSTETH_ADDRESS && quote.address.as_ref() == STETH_ADDRESS {
            to_price(self.pooled_eth_by_shares(U256::from(10).pow(U256::from(base.decimals)))?)
        } else if base.address.as_ref() == STETH_ADDRESS && quote.address.as_ref() == WSTETH_ADDRESS
        {
            to_price(self.shares_for_pooled_eth(U256::from(10).pow(U256::from(base.decimals)))?)
        } else {
            Err(SimulationError::FatalError("unsupported spot price".to_string()))
        }
    }

    fn amount_out_eth_to_steth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let shares_amount = self.shares_for_pooled_eth(amount_in)?;
        let mut new_state = self.clone();
        new_state.decrease_staking_limit(amount_in)?;
        new_state.total_shares += shares_amount;
        new_state.buffered_ether += amount_in;
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
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(WRAP_GAS),
            self.clone_box(),
        ))
    }

    fn amount_out_wsteth_to_steth(
        &self,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_out = self.pooled_eth_by_shares(amount_in)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(UNWRAP_GAS),
            self.clone_box(),
        ))
    }
}

impl StakingState {
    pub(crate) fn from_u256(value: U256) -> Self {
        let mask_32 = U256::from(u32::MAX);
        let mask_96 = (U256::from(1u8) << 96u32) - U256::ONE;

        Self {
            prev_stake_block_number: (value & mask_32).to::<u32>(),
            prev_stake_limit: (value >> 32u32) & mask_96,
            max_stake_limit_growth_blocks: ((value >> 128u32) & mask_32).to::<u32>(),
            max_stake_limit: (value >> 160u32) & mask_96,
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
impl ProtocolSim for LidoV3State {
    fn fee(&self) -> f64 {
        0f64
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        match self.kind {
            LidoV3PoolKind::StEth => self.steth_spot_price(base, quote),
            LidoV3PoolKind::WstEth => self.wsteth_spot_price(base, quote),
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);

        match self.kind {
            LidoV3PoolKind::StEth
                if token_in.address.as_ref() == ETH_ADDRESS &&
                    token_out.address.as_ref() == STETH_ADDRESS =>
            {
                self.amount_out_eth_to_steth(amount_in)
            }
            LidoV3PoolKind::WstEth
                if token_in.address.as_ref() == STETH_ADDRESS &&
                    token_out.address.as_ref() == WSTETH_ADDRESS =>
            {
                self.amount_out_steth_to_wsteth(amount_in)
            }
            LidoV3PoolKind::WstEth
                if token_in.address.as_ref() == WSTETH_ADDRESS &&
                    token_out.address.as_ref() == STETH_ADDRESS =>
            {
                self.amount_out_wsteth_to_steth(amount_in)
            }
            _ => Err(SimulationError::FatalError("unsupported swap".to_string())),
        }
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let max_input = U256::from(UINT128_MAX_EXCLUSIVE) - U256::ONE;

        match self.kind {
            LidoV3PoolKind::StEth
                if sell_token.as_ref() == ETH_ADDRESS && buy_token.as_ref() == STETH_ADDRESS =>
            {
                let max_sell = self
                    .staking_state()?
                    .current_limit(self.block_number)
                    .min(max_input);
                if max_sell.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                let max_buy = self
                    .amount_out_eth_to_steth(max_sell)?
                    .amount;
                Ok((u256_to_biguint(max_sell), max_buy))
            }
            LidoV3PoolKind::WstEth
                if sell_token.as_ref() == STETH_ADDRESS && buy_token.as_ref() == WSTETH_ADDRESS =>
            {
                // Wrapping mints against the caller's own stETH, so the protocol only bounds it
                // by how much stETH exists.
                let max_sell = self.internal_ether()?.min(max_input);
                Ok((
                    u256_to_biguint(max_sell),
                    u256_to_biguint(self.shares_for_pooled_eth(max_sell)?),
                ))
            }
            LidoV3PoolKind::WstEth
                if sell_token.as_ref() == WSTETH_ADDRESS && buy_token.as_ref() == STETH_ADDRESS =>
            {
                // Unwrapping pays out of the stETH the wrapper holds, so it is bounded by the
                // wrapper's shares. Quoting an unbounded limit here makes callers size trades the
                // wrapper cannot settle, and `wstETH.unwrap` reverts with a SafeMath underflow.
                let max_sell = self.wsteth_shares()?.min(max_input);
                if max_sell.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                Ok((
                    u256_to_biguint(max_sell),
                    u256_to_biguint(self.pooled_eth_by_shares(max_sell)?),
                ))
            }
            // Staking is one-directional: unstaking goes through the asynchronous withdrawal
            // queue, so stETH -> ETH has no quote. Report a zero limit rather than an error so
            // callers skip the direction instead of treating the component as broken.
            _ => Ok((BigUint::ZERO, BigUint::ZERO)),
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(block_number) = delta
            .updated_attributes
            .get("block_number")
        {
            self.block_number = U256::from_be_slice(block_number).to::<u64>();
        }
        if let Some(block_timestamp) = delta
            .updated_attributes
            .get("block_timestamp")
        {
            self.block_timestamp = U256::from_be_slice(block_timestamp).to::<u64>();
        }
        if let Some(total_and_external_shares) = delta
            .updated_attributes
            .get(TOTAL_AND_EXTERNAL_SHARES_ATTR)
        {
            let (total_shares, external_shares) =
                Self::split_low_high_u128(U256::from_be_slice(total_and_external_shares));
            self.total_shares = total_shares;
            self.external_shares = external_shares;
        }
        if let Some(buffered_and_deposited) = delta
            .updated_attributes
            .get(BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR)
        {
            let (buffered_ether, deposited_validators) =
                Self::split_low_high_u128(U256::from_be_slice(buffered_and_deposited));
            self.buffered_ether = buffered_ether;
            self.deposited_validators = deposited_validators;
        }
        if let Some(wsteth_shares) = delta
            .updated_attributes
            .get(WSTETH_SHARES_ATTR)
        {
            self.wsteth_shares = Some(U256::from_be_slice(wsteth_shares));
        }
        if let Some(cl_balance_and_validators) = delta
            .updated_attributes
            .get(CL_BALANCE_AND_CL_VALIDATORS_ATTR)
        {
            let (cl_balance, cl_validators) =
                Self::split_low_high_u128(U256::from_be_slice(cl_balance_and_validators));
            self.cl_balance = cl_balance;
            self.cl_validators = cl_validators;
        }
        if let Some(staking_state) = delta
            .updated_attributes
            .get(STAKING_STATE_ATTR)
        {
            self.staking_state = Some(StakingState::from_u256(U256::from_be_slice(staking_state)));
        }
        Ok(())
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
        simulation::errors::SimulationError,
        Bytes,
    };

    use super::*;
    use crate::{
        evm::protocol::test_utils::try_decode_snapshot_with_defaults,
        protocol::models::TryFromWithBlock,
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

    fn sample_steth_state() -> LidoV3State {
        LidoV3State::new(
            LidoV3PoolKind::StEth,
            24_083_113,
            1_744_791_234,
            U256::from_str_radix("6696604823358181328750512", 10).unwrap(),
            U256::from_str_radix("80758346894447149184", 10).unwrap(),
            U256::from_str_radix("658338852056838456032283", 10).unwrap(),
            U256::from(413_700u64),
            U256::from_str_radix("21114116614166341429013364", 10).unwrap(),
            U256::from(412_745u64),
            Some(sample_staking_state()),
            None,
        )
    }

    /// The shares the wstETH wrapper holds - a little under half the pool.
    fn sample_wsteth_shares() -> U256 {
        U256::from_str_radix("2960000000000000000000000", 10).unwrap()
    }

    fn sample_wsteth_state() -> LidoV3State {
        let mut state = sample_steth_state();
        state.kind = LidoV3PoolKind::WstEth;
        state.staking_state = None;
        state.wsteth_shares = Some(sample_wsteth_shares());
        state
    }

    fn staking_state_raw(state: StakingState) -> U256 {
        U256::from(state.prev_stake_block_number) |
            (state.prev_stake_limit << 32u32) |
            (U256::from(state.max_stake_limit_growth_blocks) << 128u32) |
            (state.max_stake_limit << 160u32)
    }

    fn snapshot_for(kind: LidoV3PoolKind) -> tycho_client::feed::synchronizer::ComponentWithState {
        let state = sample_steth_state();
        let mut attributes = HashMap::from([
            (
                TOTAL_AND_EXTERNAL_SHARES_ATTR.to_string(),
                Bytes::from(
                    (state.total_shares | (state.external_shares << 128u32)).to_be_bytes_vec(),
                ),
            ),
            (
                BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR.to_string(),
                Bytes::from(
                    (state.buffered_ether | (state.deposited_validators << 128u32))
                        .to_be_bytes_vec(),
                ),
            ),
            (
                CL_BALANCE_AND_CL_VALIDATORS_ATTR.to_string(),
                Bytes::from((state.cl_balance | (state.cl_validators << 128u32)).to_be_bytes_vec()),
            ),
        ]);
        if kind == LidoV3PoolKind::WstEth {
            attributes.insert(
                WSTETH_SHARES_ATTR.to_string(),
                Bytes::from(sample_wsteth_shares().to_be_bytes_vec()),
            );
        }
        let component_id = match kind {
            LidoV3PoolKind::StEth => {
                attributes.insert(
                    STAKING_STATE_ATTR.to_string(),
                    Bytes::from(staking_state_raw(sample_staking_state()).to_be_bytes_vec()),
                );
                STETH_COMPONENT_ID.to_string()
            }
            LidoV3PoolKind::WstEth => WSTETH_COMPONENT_ID.to_string(),
        };

        tycho_client::feed::synchronizer::ComponentWithState {
            state: ProtocolComponentState {
                component_id: component_id.clone(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: component_id,
                protocol_system: "lido_v3".to_string(),
                protocol_type_name: "lido_v3_pool".to_string(),
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

    #[test]
    fn split_low_high_u128_decodes_packed_slots() {
        let low = U256::from(123u64);
        let high = U256::from(456u64);
        let packed = low | (high << 128u32);
        let decoded = LidoV3State::split_low_high_u128(packed);
        assert_eq!(decoded, (low, high));
    }

    #[test]
    fn staking_state_from_u256_decodes_fields() {
        let raw = staking_state_raw(sample_staking_state());
        let decoded = StakingState::from_u256(raw);
        assert_eq!(decoded, sample_staking_state());
    }

    #[tokio::test]
    async fn decoder_reads_steth_snapshot() {
        let state =
            try_decode_snapshot_with_defaults::<LidoV3State>(snapshot_for(LidoV3PoolKind::StEth))
                .await
                .unwrap();

        assert_eq!(state.kind, LidoV3PoolKind::StEth);
        assert!(state.staking_state.is_some());
        assert_eq!(state.total_shares, sample_steth_state().total_shares);
    }

    #[tokio::test]
    async fn decoder_reads_wsteth_snapshot() {
        let state =
            try_decode_snapshot_with_defaults::<LidoV3State>(snapshot_for(LidoV3PoolKind::WstEth))
                .await
                .unwrap();

        assert_eq!(state.kind, LidoV3PoolKind::WstEth);
        assert!(state.staking_state.is_none());
        assert_eq!(state.buffered_ether, sample_steth_state().buffered_ether);
    }

    #[test]
    fn eth_to_steth_updates_state_and_consumes_stake_limit() {
        let state = sample_steth_state();
        let amount_in = BigUint::from(10u64).pow(18);
        let result = state
            .get_amount_out(amount_in.clone(), &eth_token(), &steth_token())
            .unwrap();

        assert!(result.amount > BigUint::ZERO);
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<LidoV3State>()
            .unwrap();
        assert_eq!(
            new_state.buffered_ether,
            state.buffered_ether + U256::from(10).pow(U256::from(18))
        );
        assert!(new_state.total_shares > state.total_shares);
        let old_limit = state
            .staking_state
            .unwrap()
            .current_limit(state.block_number);
        let new_limit = new_state
            .staking_state
            .unwrap()
            .current_limit(new_state.block_number);
        assert!(new_limit < old_limit);
    }

    #[test]
    fn steth_to_wsteth_and_back_keeps_state_constant() {
        let state = sample_wsteth_state();
        let amount_in = BigUint::from(10u64).pow(18);

        let wrap = state
            .get_amount_out(amount_in.clone(), &steth_token(), &wsteth_token())
            .unwrap();
        let wrapped_state = wrap
            .new_state
            .as_any()
            .downcast_ref::<LidoV3State>()
            .unwrap();
        assert_eq!(wrapped_state, &state);

        let unwrap = state
            .get_amount_out(amount_in, &wsteth_token(), &steth_token())
            .unwrap();
        let unwrapped_state = unwrap
            .new_state
            .as_any()
            .downcast_ref::<LidoV3State>()
            .unwrap();
        assert_eq!(unwrapped_state, &state);
        assert!(unwrap.amount > BigUint::ZERO);
    }

    #[test]
    fn steth_spot_price_quotes_both_orderings() {
        let state = sample_steth_state();

        let steth_per_eth = state
            .spot_price(&eth_token(), &steth_token())
            .expect("ETH -> stETH price");
        let eth_per_steth = state
            .spot_price(&steth_token(), &eth_token())
            .expect("stETH -> ETH price");

        // Staking is near parity, and the two orderings are reciprocals. The tradable direction
        // is ETH -> stETH; a caller keying prices by swap direction needs that one to exist.
        assert!((steth_per_eth - 1.0).abs() < 1e-6, "ETH -> stETH off parity: {steth_per_eth}");
        assert!((eth_per_steth - 1.0).abs() < 1e-6, "stETH -> ETH off parity: {eth_per_steth}");
        assert!((steth_per_eth * eth_per_steth - 1.0).abs() < 1e-9);
    }

    #[test]
    fn steth_spot_price_survives_exhausted_staking_capacity() {
        let mut state = sample_steth_state();
        let mut staking_state = state.staking_state.unwrap();
        // Capacity gone, but the pair still has a rate: the limit bounds size, not price.
        staking_state.prev_stake_limit = U256::ZERO;
        staking_state.prev_stake_block_number = state.block_number as u32;
        staking_state.max_stake_limit_growth_blocks = 0;
        state.staking_state = Some(staking_state);

        // A quote is unavailable ...
        assert!(state
            .get_amount_out(BigUint::from(10u64).pow(18), &eth_token(), &steth_token())
            .is_err());
        // ... but both prices still resolve.
        assert!(state
            .spot_price(&eth_token(), &steth_token())
            .is_ok());
        assert!(state
            .spot_price(&steth_token(), &eth_token())
            .is_ok());
    }

    #[test]
    fn steth_spot_price_rejects_unrelated_pair() {
        let state = sample_steth_state();

        assert!(state
            .spot_price(&steth_token(), &wsteth_token())
            .is_err());
    }

    #[test]
    fn get_limits_unwrap_is_bounded_by_wrapper_shares() {
        let state = sample_wsteth_state();

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
        let state = sample_wsteth_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(STETH_ADDRESS), Bytes::from(WSTETH_ADDRESS))
            .unwrap();

        // No more stETH can be wrapped than exists.
        let supply = state.internal_ether().unwrap();
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
        let mut state = sample_wsteth_state();
        state.wsteth_shares = Some(U256::ZERO);

        let (max_in, max_out) = state
            .get_limits(Bytes::from(WSTETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .unwrap();

        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn decoder_reads_wsteth_shares() {
        let state = sample_wsteth_state();
        assert_eq!(state.wsteth_shares, Some(sample_wsteth_shares()));
    }

    #[test]
    fn get_limits_unsupported_direction_returns_zero() {
        let state = sample_steth_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(STETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn get_limits_respects_current_stake_limit() {
        let state = sample_steth_state();
        let (max_in, max_out) = state
            .get_limits(Bytes::from(ETH_ADDRESS), Bytes::from(STETH_ADDRESS))
            .unwrap();

        assert_eq!(
            max_in,
            u256_to_biguint(
                state
                    .staking_state
                    .unwrap()
                    .current_limit(state.block_number)
            )
        );
        assert!(max_out > BigUint::ZERO);
    }

    #[test]
    fn paused_staking_blocks_eth_to_steth() {
        let mut state = sample_steth_state();
        let mut staking_state = state.staking_state.unwrap();
        staking_state.prev_stake_block_number = 0;
        state.staking_state = Some(staking_state);

        let err = state
            .get_amount_out(BigUint::from(10u64).pow(18), &eth_token(), &steth_token())
            .unwrap_err();

        assert!(
            matches!(err, SimulationError::RecoverableError(ref msg) if msg == "STAKING_PAUSED")
        );
    }

    #[test]
    fn delta_transition_updates_state() {
        let mut state = sample_steth_state();
        let new_total = U256::from(999u64);
        let new_external = U256::from(111u64);
        let new_buffered = U256::from(222u64);
        let new_deposited = U256::from(333u64);
        let new_cl_balance = U256::from(444u64);
        let new_cl_validators = U256::from(555u64);
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
                        ("block_number".to_string(), Bytes::from(88u64.to_be_bytes().to_vec())),
                        ("block_timestamp".to_string(), Bytes::from(99u64.to_be_bytes().to_vec())),
                        (
                            TOTAL_AND_EXTERNAL_SHARES_ATTR.to_string(),
                            Bytes::from((new_total | (new_external << 128u32)).to_be_bytes_vec()),
                        ),
                        (
                            BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR.to_string(),
                            Bytes::from(
                                (new_buffered | (new_deposited << 128u32)).to_be_bytes_vec(),
                            ),
                        ),
                        (
                            CL_BALANCE_AND_CL_VALIDATORS_ATTR.to_string(),
                            Bytes::from(
                                (new_cl_balance | (new_cl_validators << 128u32)).to_be_bytes_vec(),
                            ),
                        ),
                        (
                            STAKING_STATE_ATTR.to_string(),
                            Bytes::from(staking_state_raw(new_staking_state).to_be_bytes_vec()),
                        ),
                    ]),
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        assert_eq!(state.block_number, 88);
        assert_eq!(state.block_timestamp, 99);
        assert_eq!(state.total_shares, new_total);
        assert_eq!(state.external_shares, new_external);
        assert_eq!(state.buffered_ether, new_buffered);
        assert_eq!(state.deposited_validators, new_deposited);
        assert_eq!(state.cl_balance, new_cl_balance);
        assert_eq!(state.cl_validators, new_cl_validators);
        assert_eq!(state.staking_state, Some(new_staking_state));
    }

    #[test]
    fn unsupported_direction_errors() {
        let err = sample_steth_state()
            .get_amount_out(BigUint::from(10u64).pow(18), &steth_token(), &eth_token())
            .unwrap_err();
        assert!(matches!(err, SimulationError::FatalError(_)));
    }

    #[tokio::test]
    async fn decoder_uses_header_block_info() {
        let snapshot = snapshot_for(LidoV3PoolKind::StEth);
        let state = LidoV3State::try_from_with_header(
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

        assert_eq!(state.block_number, 123);
        assert_eq!(state.block_timestamp, 456);
    }
}
