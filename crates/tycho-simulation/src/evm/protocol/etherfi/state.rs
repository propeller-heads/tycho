use std::collections::HashMap;

use alloy::primitives::U256;
use hex_literal::hex;
use num_bigint::{BigInt, BigUint};
use num_traits::ToPrimitive;
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

use crate::evm::protocol::{
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    utils::solidity_math::mul_div,
};

pub const EETH_ADDRESS: [u8; 20] = hex!("35fA164735182de50811E8e2E824cFb9B6118ac2");
pub const WEETH_ADDRESS: [u8; 20] = hex!("Cd5fE23C85820F7B72D0926FC9b05b43E359b7ee");
pub const ETH_ADDRESS: [u8; 20] = hex!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
pub const BASIS_POINT_SCALE: u64 = 10000;
pub const BUCKET_UNIT_SCALE: u64 = 1_000_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtherfiState {
    block_timestamp: u64,
    total_value_out_of_lp: U256,
    total_value_in_lp: U256,
    total_shares: U256,
    eth_amount_locked_for_withdrawl: Option<U256>,
    liquidity_pool_native_balance: Option<U256>,
    eth_redemption_info: Option<RedemptionInfo>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RedemptionInfo {
    limit: BucketLimit,
    exit_fee_split_to_treasury_in_bps: u16,
    exit_fee_in_bps: u16,
    low_watermark_in_bps_of_tvl: u16,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BucketLimit {
    // The maximum capacity of the bucket, in consumable units (eg. tokens)
    capacity: u64,
    // The remaining capacity in the bucket, that can be consumed
    remaining: u64,
    // The timestamp of the last time the bucket was refilled
    last_refill: u64,
    // The rate at which the bucket refills, in units per second
    refill_rate: u64,
}

impl EtherfiState {
    pub fn new(
        block_timestamp: u64,
        total_value_out_of_lp: U256,
        total_value_in_lp: U256,
        total_shares: U256,
        eth_amount_locked_for_withdrawl: Option<U256>,
        eth_redemption_info: Option<RedemptionInfo>,
        liquidity_pool_native_balance: Option<U256>,
    ) -> Self {
        EtherfiState {
            block_timestamp,
            total_value_out_of_lp,
            total_value_in_lp,
            total_shares,
            eth_amount_locked_for_withdrawl,
            liquidity_pool_native_balance,
            eth_redemption_info,
        }
    }

    fn require_redemption_info(&self) -> Result<RedemptionInfo, SimulationError> {
        self.eth_redemption_info
            .ok_or_else(|| SimulationError::FatalError("missing eth redemption info".to_string()))
    }

    fn require_liquidity_balance(&self) -> Result<U256, SimulationError> {
        self.liquidity_pool_native_balance
            .ok_or_else(|| {
                SimulationError::FatalError("missing liquidity pool native balance".to_string())
            })
    }

    fn require_eth_amount_locked_for_withdrawl(&self) -> Result<U256, SimulationError> {
        self.eth_amount_locked_for_withdrawl
            .ok_or_else(|| {
                SimulationError::FatalError("missing eth amount locked for withdrawal".to_string())
            })
    }

    fn shares_for_amount(&self, amount: U256) -> Result<U256, SimulationError> {
        let total_pooled_ether = self.total_value_in_lp + self.total_value_out_of_lp;
        if total_pooled_ether == U256::ZERO {
            return Ok(U256::ZERO);
        }
        // Pro-rata shares for a given pooled ETH amount.
        Ok(amount * self.total_shares / total_pooled_ether)
    }

    fn amount_for_share(&self, share: U256) -> Result<U256, SimulationError> {
        if self.total_shares == U256::ZERO {
            return Ok(U256::ZERO);
        }
        // Pro-rata ETH amount for a given share count.
        let total_pooled_ether = self.total_value_in_lp + self.total_value_out_of_lp;
        Ok(share * total_pooled_ether / self.total_shares)
    }

    fn shares_for_withdrawal_amount(&self, amount: U256) -> Result<U256, SimulationError> {
        let total_pooled_ether = self.total_value_in_lp + self.total_value_out_of_lp;
        if total_pooled_ether == U256::ZERO {
            return Ok(U256::ZERO);
        }
        let numerator = amount * self.total_shares;
        Ok(numerator + total_pooled_ether - U256::ONE / total_pooled_ether)
    }
}

impl BucketLimit {
    pub(crate) fn from_u256(value: U256) -> Self {
        let mask = U256::from(u64::MAX);
        Self {
            capacity: (value & mask).to::<u64>(),
            remaining: ((value >> 64u32) & mask).to::<u64>(),
            last_refill: ((value >> 128u32) & mask).to::<u64>(),
            refill_rate: ((value >> 192u32) & mask).to::<u64>(),
        }
    }

    // Apply time-based refill to remaining capacity.
    fn refill(mut self, now: u64) -> Self {
        if now <= self.last_refill {
            return self;
        }
        let delta = now - self.last_refill;
        let tokens = (delta as u128) * (self.refill_rate as u128);
        let new_remaining = (self.remaining as u128) + tokens;
        if new_remaining > self.capacity as u128 {
            self.remaining = self.capacity;
        } else {
            self.remaining = new_remaining as u64;
        }
        self.last_refill = now;
        self
    }
}

// Convert wei amount to bucket units with optional rounding up.
fn convert_to_bucket_unit(amount: U256, rounding_up: bool) -> Result<u64, SimulationError> {
    let scale = U256::from(BUCKET_UNIT_SCALE);
    let max_amount = U256::from(u64::MAX) * scale;
    if amount >= max_amount {
        return Err(SimulationError::FatalError(
            "EtherFiRedemptionManager: Amount too large".to_string(),
        ));
    }
    // Convert wei to rate-limit bucket units with optional ceil/floor.
    let bucket = if rounding_up { (amount + scale - U256::ONE) / scale } else { amount / scale };
    if bucket > U256::from(u64::MAX) {
        return Err(SimulationError::FatalError(
            "EtherFiRedemptionManager: Amount too large".to_string(),
        ));
    }
    Ok(bucket.to::<u64>())
}

impl RedemptionInfo {
    pub(crate) fn from_u256(limit: BucketLimit, value: U256) -> Self {
        let mask = U256::from(u64::from(u16::MAX));
        let exit_fee_split_to_treasury_in_bps = (value & mask).to::<u16>();
        let exit_fee_in_bps = ((value >> 16u32) & mask).to::<u16>();
        let low_watermark_in_bps_of_tvl = ((value >> 32u32) & mask).to::<u16>();
        Self {
            limit,
            exit_fee_split_to_treasury_in_bps,
            exit_fee_in_bps,
            low_watermark_in_bps_of_tvl,
        }
    }
}

#[typetag::serde]
impl ProtocolSim for EtherfiState {
    fn fee(&self) -> f64 {
        0 as f64
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let base_unit = U256::from(10).pow(U256::from(base.decimals));
        let quote_unit = U256::from(10).pow(U256::from(quote.decimals));
        let quote_unit_f64 = u256_to_f64(quote_unit)?;
        let to_price = |amount_out: U256| -> Result<f64, SimulationError> {
            Ok(u256_to_f64(amount_out)? / quote_unit_f64)
        };

        if base.address.as_ref() == EETH_ADDRESS && quote.address.as_ref() == WEETH_ADDRESS {
            to_price(self.shares_for_amount(base_unit)?)
        } else if base.address.as_ref() == WEETH_ADDRESS && quote.address.as_ref() == EETH_ADDRESS {
            to_price(self.amount_for_share(base_unit)?)
        } else if base.address.as_ref() == ETH_ADDRESS && quote.address.as_ref() == EETH_ADDRESS {
            to_price(self.shares_for_amount(base_unit)?)
        } else if base.address.as_ref() == EETH_ADDRESS && quote.address.as_ref() == ETH_ADDRESS {
            to_price(self.amount_for_share(base_unit)?)
        } else {
            Err(SimulationError::FatalError("unsupported spot price".to_string()))
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let mut new_state = self.clone();
        let amount_in = biguint_to_u256(&amount_in);
        if token_in.address.as_ref() == ETH_ADDRESS && token_out.address.as_ref() == EETH_ADDRESS {
            // eth --> eeth
            let amount_out = self.shares_for_amount(amount_in)?;
            new_state.total_shares += amount_out;
            new_state.total_value_in_lp += amount_in;
            return Ok(GetAmountOutResult::new(
                u256_to_biguint(amount_out),
                BigUint::from(46_886u32), // LiquidityPool.deposit function gas used
                Box::new(new_state),
            ));
        }

        if token_in.address.as_ref() == EETH_ADDRESS && token_out.address.as_ref() == ETH_ADDRESS {
            // eeth --> eth
            let liquidity_pool_native_balance = self.require_liquidity_balance()?;
            let eth_amount_locked_for_withdrawl = self.require_eth_amount_locked_for_withdrawl()?;
            let eth_redemption_info = self.require_redemption_info()?;
            let liquid_eth_amount = liquidity_pool_native_balance - eth_amount_locked_for_withdrawl;
            let low_watermark = mul_div(
                self.total_value_in_lp + self.total_value_out_of_lp,
                U256::from(eth_redemption_info.low_watermark_in_bps_of_tvl),
                U256::from(BASIS_POINT_SCALE),
            )?;
            if liquid_eth_amount < low_watermark || liquid_eth_amount - low_watermark < amount_in {
                return Err(SimulationError::FatalError("Exceeded total redeemable amount".into()));
            } else {
                // Enforce the rate-limit bucket before applying exit fees and balances.
                let bucket_unit = convert_to_bucket_unit(amount_in, true)?;
                let mut limit = eth_redemption_info
                    .limit
                    .refill(self.block_timestamp);
                if limit.remaining < bucket_unit {
                    return Err(SimulationError::FatalError("Exceeded rate limit".into()));
                }
                limit.remaining -= bucket_unit;
                limit.last_refill = self.block_timestamp;
                let mut updated_info = eth_redemption_info;
                updated_info.limit = limit;
                new_state.eth_redemption_info = Some(updated_info);
            }
            let eeth_shares = self.shares_for_amount(amount_in)?;
            let eth_amount_out = self.amount_for_share(mul_div(
                eeth_shares,
                U256::from(BASIS_POINT_SCALE) - U256::from(eth_redemption_info.exit_fee_in_bps),
                U256::from(BASIS_POINT_SCALE),
            )?)?;
            new_state.total_value_in_lp -= eth_amount_out;
            new_state.total_shares -= self.shares_for_withdrawal_amount(eth_amount_out)?;
            new_state.liquidity_pool_native_balance =
                Some(liquidity_pool_native_balance - eth_amount_out);
            let amount_out = u256_to_biguint(eth_amount_out);
            return Ok(GetAmountOutResult::new(
                amount_out,
                BigUint::from(151_676u32), /* EtherFiRedemptionManager._redeemEEth function gas
                                            * used */
                Box::new(new_state),
            ));
        }

        if token_in.address.as_ref() == EETH_ADDRESS && token_out.address.as_ref() == WEETH_ADDRESS
        {
            // eeth --> weeth
            let amount_out = u256_to_biguint(self.shares_for_amount(amount_in)?);
            return Ok(GetAmountOutResult::new(
                amount_out,
                BigUint::from(70_489u32), // weeth.wrap function gas used
                Box::new(new_state),
            ));
        }

        if token_in.address.as_ref() == WEETH_ADDRESS && token_out.address.as_ref() == EETH_ADDRESS
        {
            // weeth --> eeth
            let amount_out = u256_to_biguint(self.amount_for_share(amount_in)?);
            return Ok(GetAmountOutResult::new(
                amount_out,
                BigUint::from(60_182u32), // weeth.unwrap function gas used
                Box::new(new_state),
            ));
        }

        Err(SimulationError::FatalError("unsupported swap".to_string()))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if sell_token.as_ref() == WEETH_ADDRESS && buy_token.as_ref() == EETH_ADDRESS {
            let max_weeth_amount = self.shares_for_amount(self.total_shares)?;
            let max_eeth_amount = self.total_shares;
            return Ok((u256_to_biguint(max_weeth_amount), u256_to_biguint(max_eeth_amount)));
        }

        if sell_token.as_ref() == EETH_ADDRESS && buy_token.as_ref() == ETH_ADDRESS {
            let liquidity_pool_native_balance = self.require_liquidity_balance()?;
            let eth_amount_locked_for_withdrawl = self.require_eth_amount_locked_for_withdrawl()?;
            let eth_redemption_info = self.require_redemption_info()?;
            let liquid_eth_amount = liquidity_pool_native_balance - eth_amount_locked_for_withdrawl;
            let low_watermark = mul_div(
                self.total_value_in_lp + self.total_value_out_of_lp,
                U256::from(eth_redemption_info.low_watermark_in_bps_of_tvl),
                U256::from(BASIS_POINT_SCALE),
            )?;
            if liquid_eth_amount < low_watermark {
                return Ok((u256_to_biguint(liquid_eth_amount), BigUint::ZERO));
            }
            let mut max_eeth_amount = self.total_value_in_lp + self.total_value_out_of_lp;
            let limit = eth_redemption_info
                .limit
                .refill(self.block_timestamp);
            // Cap max sell amount by the rate-limit bucket, in wei.
            let bucket_unit = convert_to_bucket_unit(max_eeth_amount, true)?;
            if limit.remaining < bucket_unit {
                max_eeth_amount = U256::from(limit.remaining) * U256::from(BUCKET_UNIT_SCALE);
            }
            let eeth_shares = self.shares_for_amount(max_eeth_amount)?;
            let eth_amount_out = self.amount_for_share(mul_div(
                eeth_shares,
                U256::from(BASIS_POINT_SCALE) - U256::from(eth_redemption_info.exit_fee_in_bps),
                U256::from(BASIS_POINT_SCALE),
            )?)?;
            return Ok((u256_to_biguint(max_eeth_amount), u256_to_biguint(eth_amount_out)));
        }

        if sell_token.as_ref() == EETH_ADDRESS && buy_token.as_ref() == WEETH_ADDRESS {
            return Ok((u256_to_biguint(U256::MAX), u256_to_biguint(U256::MAX)));
        }

        if sell_token.as_ref() == ETH_ADDRESS && buy_token.as_ref() == EETH_ADDRESS {
            return Ok((u256_to_biguint(U256::MAX), u256_to_biguint(U256::MAX)));
        }

        Err(SimulationError::FatalError("unsupported swap".to_string()))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(block_timestamp) = delta
            .updated_attributes
            .get("block_timestamp")
        {
            self.block_timestamp = BigInt::from_signed_bytes_be(block_timestamp)
                .to_u64()
                .unwrap();
        }
        if let Some(total_value_out_of_lp) = delta
            .updated_attributes
            .get("totalValueOutOfLp")
        {
            self.total_value_out_of_lp = U256::from_be_slice(total_value_out_of_lp);
        }
        if let Some(total_value_in_lp) = delta
            .updated_attributes
            .get("totalValueInLp")
        {
            self.total_value_in_lp = U256::from_be_slice(total_value_in_lp);
        }
        if let Some(total_shares) = delta
            .updated_attributes
            .get("totalShares")
        {
            self.total_shares = U256::from_be_slice(total_shares);
        }
        if let Some(eth_amount_locked_for_withdrawl) = delta
            .updated_attributes
            .get("ethAmountLockedForWithdrawl")
        {
            self.eth_amount_locked_for_withdrawl =
                Some(U256::from_be_slice(eth_amount_locked_for_withdrawl));
        }
        if let Some(liquidity_pool_native_balance) = delta
            .updated_attributes
            .get("liquidity_pool_native_balance")
        {
            self.liquidity_pool_native_balance =
                Some(U256::from_be_slice(liquidity_pool_native_balance));
        }
        let eth_bucket_limiter = delta
            .updated_attributes
            .get("ethBucketLimiter")
            .map(|value| U256::from_be_slice(value));
        let eth_redemption_info = delta
            .updated_attributes
            .get("ethRedemptionInfo")
            .map(|value| U256::from_be_slice(value));
        if eth_bucket_limiter.is_some() || eth_redemption_info.is_some() {
            let existing = self
                .eth_redemption_info
                .unwrap_or_default();
            let mut limit = existing.limit;
            let mut exit_fee_split = existing.exit_fee_split_to_treasury_in_bps;
            let mut exit_fee = existing.exit_fee_in_bps;
            let mut low_watermark = existing.low_watermark_in_bps_of_tvl;

            if let Some(eth_bucket_limiter) = eth_bucket_limiter {
                limit = BucketLimit::from_u256(eth_bucket_limiter);
            }
            if let Some(eth_redemption_info) = eth_redemption_info {
                let parsed = RedemptionInfo::from_u256(limit, eth_redemption_info);
                limit = parsed.limit;
                exit_fee_split = parsed.exit_fee_split_to_treasury_in_bps;
                exit_fee = parsed.exit_fee_in_bps;
                low_watermark = parsed.low_watermark_in_bps_of_tvl;
            }

            self.eth_redemption_info = Some(RedemptionInfo {
                limit,
                exit_fee_split_to_treasury_in_bps: exit_fee_split,
                exit_fee_in_bps: exit_fee,
                low_watermark_in_bps_of_tvl: low_watermark,
            });
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        if let Some(other_state) = other.as_any().downcast_ref::<Self>() {
            self == other_state
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u256_dec(value: &str) -> U256 {
        U256::from_str_radix(value, 10).expect("valid base-10 U256")
    }

    fn sample_state() -> EtherfiState {
        EtherfiState {
            block_timestamp: 1_764_901_727,
            total_value_out_of_lp: u256_dec("2649956291248983147816190"),
            total_value_in_lp: u256_dec("35878437939234433682319"),
            total_shares: u256_dec("2479957712837255941780080"),
            eth_amount_locked_for_withdrawl: Some(u256_dec("5572247384784800483589")),
            liquidity_pool_native_balance: Some(u256_dec("35878437939234433682319")),
            eth_redemption_info: Some(RedemptionInfo {
                limit: BucketLimit {
                    capacity: 2_000_000_000,
                    remaining: 1_998_993_391,
                    last_refill: 1_764_901_727,
                    refill_rate: 23_148,
                },
                exit_fee_split_to_treasury_in_bps: 1000,
                exit_fee_in_bps: 30,
                low_watermark_in_bps_of_tvl: 100,
            }),
        }
    }

    #[test]
    fn bucket_limit_from_u256_parses_fields() {
        let capacity = 2_000_000_000u64;
        let remaining = 1_999_995_000u64;
        let last_refill = 1_767_694_523u64;
        let refill_rate = 23_148u64;

        let value = U256::from(capacity) |
            (U256::from(remaining) << 64u32) |
            (U256::from(last_refill) << 128u32) |
            (U256::from(refill_rate) << 192u32);

        let limit = BucketLimit::from_u256(value);
        assert_eq!(limit.capacity, capacity);
        assert_eq!(limit.remaining, remaining);
        assert_eq!(limit.last_refill, last_refill);
        assert_eq!(limit.refill_rate, refill_rate);
    }

    #[test]
    fn redemption_info_from_u256_parses_fields() {
        let limit = BucketLimit { capacity: 1, remaining: 2, last_refill: 3, refill_rate: 4 };
        let exit_fee_split = 1000u16;
        let exit_fee = 30u16;
        let low_watermark = 100u16;

        let value = U256::from(u64::from(exit_fee_split)) |
            (U256::from(u64::from(exit_fee)) << 16u32) |
            (U256::from(u64::from(low_watermark)) << 32u32);

        let info = RedemptionInfo::from_u256(limit, value);
        assert_eq!(info.limit, limit);
        assert_eq!(info.exit_fee_split_to_treasury_in_bps, exit_fee_split);
        assert_eq!(info.exit_fee_in_bps, exit_fee);
        assert_eq!(info.low_watermark_in_bps_of_tvl, low_watermark);
    }

    #[test]
    fn convert_to_bucket_unit_rounds_up() {
        let amount = U256::from(BUCKET_UNIT_SCALE - 1);
        let bucket = convert_to_bucket_unit(amount, true).expect("bucket");
        assert_eq!(bucket, 1);

        let exact = U256::from(BUCKET_UNIT_SCALE * 2);
        let bucket_exact = convert_to_bucket_unit(exact, true).expect("bucket");
        assert_eq!(bucket_exact, 2);
    }

    #[test]
    fn convert_to_bucket_unit_rounds_down() {
        let amount = U256::from(BUCKET_UNIT_SCALE - 1);
        let bucket = convert_to_bucket_unit(amount, false).expect("bucket");
        assert_eq!(bucket, 0);

        let exact = U256::from(BUCKET_UNIT_SCALE * 3);
        let bucket_exact = convert_to_bucket_unit(exact, false).expect("bucket");
        assert_eq!(bucket_exact, 3);
    }

    #[test]
    fn convert_to_bucket_unit_rejects_large_amounts() {
        let scale = U256::from(BUCKET_UNIT_SCALE);
        let max_amount = U256::from(u64::MAX) * scale;
        let err = convert_to_bucket_unit(max_amount, true).unwrap_err();
        match err {
            SimulationError::FatalError(msg) => {
                assert!(msg.contains("Amount too large"));
            }
            _ => panic!("unexpected error type"),
        }
    }

    #[test]
    fn bucket_limit_refill_caps_at_capacity() {
        let limit = BucketLimit { capacity: 10, remaining: 1, last_refill: 100, refill_rate: 5 };
        let refilled = limit.refill(103);
        assert_eq!(refilled.remaining, 10);
        assert_eq!(refilled.last_refill, 103);
    }

    #[test]
    fn bucket_limit_refill_noop_same_or_older_time() {
        let limit = BucketLimit { capacity: 10, remaining: 4, last_refill: 100, refill_rate: 5 };
        let same = limit.refill(100);
        assert_eq!(same.remaining, 4);
        assert_eq!(same.last_refill, 100);

        let older = limit.refill(99);
        assert_eq!(older.remaining, 4);
        assert_eq!(older.last_refill, 100);
    }

    #[test]
    fn get_limits_eeth_to_eth_caps_by_bucket_remaining() {
        let state = sample_state();
        let info = state
            .eth_redemption_info
            .expect("redemption info");
        let limit = info.limit.refill(state.block_timestamp);
        let expected_max_in = U256::from(limit.remaining) * U256::from(BUCKET_UNIT_SCALE);

        let (max_in, max_out) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(expected_max_in));
        let eeth_shares = state
            .shares_for_amount(expected_max_in)
            .expect("shares");
        let net_shares = mul_div(
            eeth_shares,
            U256::from(BASIS_POINT_SCALE) - U256::from(info.exit_fee_in_bps),
            U256::from(BASIS_POINT_SCALE),
        )
        .expect("mul_div");
        let expected_out = state
            .amount_for_share(net_shares)
            .expect("amount");
        assert_eq!(max_out, u256_to_biguint(expected_out));
    }

    #[test]
    fn get_limits_eeth_to_eth_returns_liquid_amount_when_below_low_watermark() {
        let mut state = sample_state();
        let info = state
            .eth_redemption_info
            .expect("redemption info");
        let total_pooled = state.total_value_in_lp + state.total_value_out_of_lp;
        let low_watermark = mul_div(
            total_pooled,
            U256::from(info.low_watermark_in_bps_of_tvl),
            U256::from(BASIS_POINT_SCALE),
        )
        .expect("low watermark");
        let locked = state
            .eth_amount_locked_for_withdrawl
            .expect("locked");
        state.liquidity_pool_native_balance = Some(locked + low_watermark - U256::ONE);

        let (max_in, max_out) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(low_watermark - U256::ONE));
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn get_limits_weeth_to_eeth_uses_total_shares() {
        let state = sample_state();
        let max_weeth = state
            .shares_for_amount(state.total_shares)
            .expect("shares");
        let (max_in, max_out) = state
            .get_limits(Bytes::from(WEETH_ADDRESS), Bytes::from(EETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(max_weeth));
        assert_eq!(max_out, u256_to_biguint(state.total_shares));
    }
}
