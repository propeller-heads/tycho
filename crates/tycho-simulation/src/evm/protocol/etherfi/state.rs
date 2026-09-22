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
    safe_math::{safe_add_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    utils::solidity_math::{mul_div, mul_div_rounding_up},
};

pub const EETH_ADDRESS: [u8; 20] = hex!("35fA164735182de50811E8e2E824cFb9B6118ac2");
pub const WEETH_ADDRESS: [u8; 20] = hex!("Cd5fE23C85820F7B72D0926FC9b05b43E359b7ee");
/// Native ETH as Tycho addresses it (`Chain::native_token`), not the router's 0xEeee..EEeE
/// sentinel: this has to match the token the indexer reports and the token the swap encoder
/// compares against.
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");

/// Slice views of the addresses above, so a swap direction can be matched as a tuple.
const ETH: &[u8] = &ETH_ADDRESS;
const EETH: &[u8] = &EETH_ADDRESS;
const WEETH: &[u8] = &WEETH_ADDRESS;

/// The venue is two components: the LiquidityPool keyed by eETH, and the wrapper keyed by weETH.
pub const POOL_COMPONENT_ID: &str = "0x35fa164735182de50811e8e2e824cfb9b6118ac2";
pub const WRAPPER_COMPONENT_ID: &str = "0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee";

// One attribute per value the protocol names. The substreams package unpacks the storage words,
// so each of these carries a single scalar.
pub const TOTAL_VALUE_OUT_OF_LP_ATTR: &str = "total_value_out_of_lp";
pub const TOTAL_VALUE_IN_LP_ATTR: &str = "total_value_in_lp";
pub const TOTAL_SHARES_ATTR: &str = "total_shares";
pub const WEETH_SHARES_ATTR: &str = "weeth_shares";
pub const EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR: &str = "exit_fee_split_to_treasury_bps";
pub const EXIT_FEE_BPS_ATTR: &str = "exit_fee_bps";
pub const LOW_WATERMARK_BPS_ATTR: &str = "low_watermark_bps";

/// The four attributes one `BucketLimiter.Limit` is reported as.
pub struct BucketAttributes {
    pub capacity: &'static str,
    pub remaining: &'static str,
    pub last_refill: &'static str,
    pub refill_rate: &'static str,
}

/// `EtherFiRedemptionManager.tokenToRedemptionInfo(ETH).limit`, in units of 10^12 wei.
pub const REDEMPTION_BUCKET: BucketAttributes = BucketAttributes {
    capacity: "redemption_bucket_capacity",
    remaining: "redemption_bucket_remaining",
    last_refill: "redemption_bucket_last_refill",
    refill_rate: "redemption_bucket_refill_rate",
};
/// `EtherFiRateLimiter.limits[EETH_MINT_LIMIT_ID]`, consumed by `eETH.mintShares`, in gwei.
pub const MINT_BUCKET: BucketAttributes = BucketAttributes {
    capacity: "mint_bucket_capacity",
    remaining: "mint_bucket_remaining",
    last_refill: "mint_bucket_last_refill",
    refill_rate: "mint_bucket_refill_rate",
};
/// `EtherFiRateLimiter.limits[EETH_BURN_LIMIT_ID]`, consumed by `eETH.burnShares`, in gwei.
pub const BURN_BUCKET: BucketAttributes = BucketAttributes {
    capacity: "burn_bucket_capacity",
    remaining: "burn_bucket_remaining",
    last_refill: "burn_bucket_last_refill",
    refill_rate: "burn_bucket_refill_rate",
};

const BASIS_POINT_SCALE: u64 = 10_000;
/// `EtherFiRedemptionManager.BUCKET_UNIT_SCALE`.
const REDEMPTION_BUCKET_UNIT: u64 = 1_000_000_000_000;
/// `eETH._toBucketUnit` counts in gwei.
const GWEI: u64 = 1_000_000_000;

const DEPOSIT_GAS: u64 = 46_886;
const REDEEM_GAS: u64 = 151_676;
const WRAP_GAS: u64 = 70_489;
const UNWRAP_GAS: u64 = 60_182;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtherfiState {
    /// Unix timestamp of the block a quote is expected to execute in, maintained by
    /// `apply_block`.
    ///
    /// Every rate-limit bucket refills per second, so this tracks the chain head: `apply_block`
    /// advances it on every message, including the blocks where EtherFi storage does not move.
    execution_block_timestamp: u64,
    /// `LiquidityPool.totalValueOutOfLp`: ether staked on the consensus layer.
    total_value_out_of_lp: U256,
    /// `LiquidityPool.totalValueInLp`: ether the pool holds, which redemptions are paid from.
    total_value_in_lp: U256,
    /// `eETH.totalShares`.
    total_shares: U256,
    venue: Venue,
}

/// Which of the two components this state is, and the state only that component needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Venue {
    /// The eETH component: `LiquidityPool.deposit` and `EtherFiRedemptionManager.redeemEEth`.
    Pool(PoolState),
    /// The weETH component: `wrap` and `unwrap`.
    Wrapper(WrapperState),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    pub redemption: RedemptionInfo,
    /// Bounds deposits: `eETH.mintShares` consumes the minted balance from it, in gwei.
    pub mint_limit: BucketLimit,
    /// Bounds redemptions: `eETH.burnShares` consumes the burnt balance from it, in gwei.
    pub burn_limit: BucketLimit,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrapperState {
    /// `eETH.shares(weETH)`: the shares the wrapper holds. Unwrapping pays out of them.
    pub weeth_shares: U256,
}

/// `EtherFiRedemptionManager.RedemptionInfo` for ETH.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedemptionInfo {
    /// In units of `REDEMPTION_BUCKET_UNIT` wei, consumed by the eETH amount redeemed.
    pub limit: BucketLimit,
    pub exit_fee_split_to_treasury_bps: u16,
    pub exit_fee_bps: u16,
    /// Redemptions may not take `totalValueInLp` below this share of `getTotalPooledEther()`.
    pub low_watermark_bps: u16,
}

impl RedemptionInfo {
    /// The share of a redemption that reaches the redeemer, in basis points.
    ///
    /// `EtherFiRedemptionManager` rejects an exit fee above the basis-point scale, so one here
    /// came off the wire malformed rather than off the chain, and subtracting it would wrap.
    fn net_of_exit_fee_bps(&self) -> Result<u64, SimulationError> {
        BASIS_POINT_SCALE
            .checked_sub(u64::from(self.exit_fee_bps))
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "exit fee of {} bps exceeds the basis-point scale",
                    self.exit_fee_bps
                ))
            })
    }
}

/// `BucketLimiter.Limit`: a token bucket that refills at `refill_rate` units per second up to
/// `capacity`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketLimit {
    pub capacity: u64,
    pub remaining: u64,
    pub last_refill: u64,
    pub refill_rate: u64,
}

impl BucketLimit {
    /// `BucketLimiter._refill`: the bucket as it stands at `now`.
    ///
    /// A block older than the last refill leaves the bucket as it is; the contract's unchecked
    /// subtraction would wrap there, but the chain never presents such a block.
    fn refilled(self, now: u64) -> Self {
        if now <= self.last_refill {
            return self;
        }
        let elapsed = u128::from(now - self.last_refill);
        let refilled = u128::from(self.remaining) + elapsed * u128::from(self.refill_rate);
        let remaining = u64::try_from(refilled.min(u128::from(self.capacity)))
            .expect("bounded by the u64 capacity");
        Self { remaining, last_refill: now, ..self }
    }

    /// `BucketLimiter.consumable`: what can be drawn at `now`.
    fn consumable(self, now: u64) -> u64 {
        self.refilled(now).remaining
    }

    /// `BucketLimiter.consume`: the bucket after drawing `units` at `now`, or `None` when the
    /// bucket cannot cover them.
    fn consume(self, units: u64, now: u64) -> Option<Self> {
        let refilled = self.refilled(now);
        let remaining = refilled.remaining.checked_sub(units)?;
        Some(Self { remaining, ..refilled })
    }
}

/// `eETH._toBucketUnit`: gwei, rounded up, saturating at `u64::MAX`.
fn gwei_units(amount: U256) -> u64 {
    let units = amount.div_ceil(U256::from(GWEI));
    u64::try_from(units).unwrap_or(u64::MAX)
}

/// `EtherFiRedemptionManager._convertToBucketUnit` rounding up. The contract rejects amounts of
/// `u64::MAX` units or more outright.
fn redemption_units(amount: U256) -> Result<u64, SimulationError> {
    let scale = U256::from(REDEMPTION_BUCKET_UNIT);
    if amount >= U256::from(u64::MAX) * scale {
        return Err(SimulationError::RecoverableError("AMOUNT_TOO_LARGE".to_string()));
    }
    Ok(amount.div_ceil(scale).to::<u64>())
}

/// Bytes an attribute occupies on the wire: the width of the storage field the substreams
/// package unpacks it from. The balances are `uint128` halves, the share counts whole words, the
/// bucket fields `uint64` and the basis-point fields `uint16`.
fn attribute_width(name: &str) -> Option<usize> {
    if name == TOTAL_VALUE_OUT_OF_LP_ATTR || name == TOTAL_VALUE_IN_LP_ATTR {
        return Some(16);
    }
    if name == TOTAL_SHARES_ATTR || name == WEETH_SHARES_ATTR {
        return Some(32);
    }
    if name == EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR ||
        name == EXIT_FEE_BPS_ATTR ||
        name == LOW_WATERMARK_BPS_ATTR
    {
        return Some(2);
    }
    for bucket in [&REDEMPTION_BUCKET, &MINT_BUCKET, &BURN_BUCKET] {
        if [bucket.capacity, bucket.remaining, bucket.last_refill, bucket.refill_rate]
            .contains(&name)
        {
            return Some(8);
        }
    }
    None
}

/// Reads a big-endian attribute, refusing one wider than its storage field. The package emits
/// minimal-length values, so a wider one is malformed, and the `Err` names it for the caller to
/// report.
pub(super) fn decode_attribute(name: &str, value: &[u8]) -> Result<U256, String> {
    let Some(width) = attribute_width(name) else {
        return Err(format!("{name} is not an EtherFi attribute"));
    };
    if value.len() > width {
        return Err(format!("{name} is {} bytes, wider than its {width}-byte field", value.len()));
    }
    Ok(U256::from_be_slice(value))
}

/// `decode_attribute` for the `uint64` bucket fields.
pub(super) fn decode_u64_attribute(name: &str, value: &[u8]) -> Result<u64, String> {
    let value = decode_attribute(name, value)?;
    u64::try_from(value).map_err(|_| format!("{name} does not fit in 64 bits"))
}

/// `decode_attribute` for the `uint16` basis-point fields.
pub(super) fn decode_u16_attribute(name: &str, value: &[u8]) -> Result<u16, String> {
    let value = decode_attribute(name, value)?;
    u16::try_from(value).map_err(|_| format!("{name} does not fit in 16 bits"))
}

impl EtherfiState {
    pub fn new(
        execution_block_timestamp: u64,
        total_value_out_of_lp: U256,
        total_value_in_lp: U256,
        total_shares: U256,
        venue: Venue,
    ) -> Self {
        Self {
            execution_block_timestamp,
            total_value_out_of_lp,
            total_value_in_lp,
            total_shares,
            venue,
        }
    }

    /// `LiquidityPool.getTotalPooledEther()`.
    fn total_pooled_ether(&self) -> Result<U256, SimulationError> {
        safe_add_u256(self.total_value_out_of_lp, self.total_value_in_lp)
    }

    /// `LiquidityPool.sharesForAmount`, rounded down.
    fn shares_for_amount(&self, amount: U256) -> Result<U256, SimulationError> {
        let total_pooled_ether = self.total_pooled_ether()?;
        if total_pooled_ether.is_zero() {
            return Ok(U256::ZERO);
        }
        mul_div(amount, self.total_shares, total_pooled_ether)
    }

    /// `LiquidityPool.amountForShare`, rounded down.
    fn amount_for_share(&self, share: U256) -> Result<U256, SimulationError> {
        if self.total_shares.is_zero() {
            return Ok(U256::ZERO);
        }
        mul_div(share, self.total_pooled_ether()?, self.total_shares)
    }

    /// `LiquidityPool.sharesForWithdrawalAmount`: rounded up, so rounding favours the pool.
    fn shares_for_withdrawal_amount(&self, amount: U256) -> Result<U256, SimulationError> {
        let total_pooled_ether = self.total_pooled_ether()?;
        if total_pooled_ether.is_zero() {
            return Ok(U256::ZERO);
        }
        mul_div_rounding_up(amount, self.total_shares, total_pooled_ether)
    }

    /// `EtherFiRedemptionManager.lowWatermarkInETH(ETH)`: the liquidity redemptions must leave.
    fn low_watermark(&self, pool: &PoolState) -> Result<U256, SimulationError> {
        mul_div(
            self.total_pooled_ether()?,
            U256::from(pool.redemption.low_watermark_bps),
            U256::from(BASIS_POINT_SCALE),
        )
    }

    /// ETH -> eETH through `LiquidityPool.deposit`.
    ///
    /// The deposit is credited to `totalValueInLp`, a `uint128`, and mints shares at the
    /// pre-deposit rate. The depositor's eETH balance is those shares at the post-deposit rate,
    /// which is what the caller trades; `eETH.mintShares` draws that balance from the mint bucket.
    fn amount_out_eth_to_eeth(
        &self,
        pool: &PoolState,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let total_value_in_lp = safe_add_u256(self.total_value_in_lp, amount_in)?;
        if total_value_in_lp > U256::from(u128::MAX) {
            return Err(SimulationError::RecoverableError("LIQUIDITY_POOL_CAPACITY".to_string()));
        }
        let shares = self.shares_for_amount(amount_in)?;
        if shares.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }

        let mut next = self.clone();
        next.total_value_in_lp = total_value_in_lp;
        next.total_shares = safe_add_u256(self.total_shares, shares)?;
        let amount_out = next.amount_for_share(shares)?;

        let mut pool = *pool;
        pool.mint_limit = pool
            .mint_limit
            .consume(gwei_units(amount_out), self.execution_block_timestamp)
            .ok_or_else(|| SimulationError::RecoverableError("MINT_RATE_LIMIT".to_string()))?;
        next.venue = Venue::Pool(pool);

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(DEPOSIT_GAS),
            Box::new(next),
        ))
    }

    fn redemption_amounts(
        &self,
        pool: &PoolState,
        amount_in: U256,
    ) -> Result<(U256, U256), SimulationError> {
        let eeth_shares = self.shares_for_amount(amount_in)?;
        let net_shares = mul_div(
            eeth_shares,
            U256::from(pool.redemption.net_of_exit_fee_bps()?),
            U256::from(BASIS_POINT_SCALE),
        )?;
        let amount_out = self.amount_for_share(net_shares)?;
        Ok((eeth_shares, amount_out))
    }

    /// eETH -> ETH through `EtherFiRedemptionManager.redeemEEth`.
    ///
    /// `canRedeem` bounds the amount by the liquidity above the low watermark and by the
    /// redemption bucket. `_calcRedemption` then pays the shares net of the exit fee: the fee
    /// shares split between the treasury, which keeps them, and the stakers, whose part is burnt
    /// and so raises the share rate. `eETH.burnShares` draws both burns from the burn bucket.
    fn amount_out_eeth_to_eth(
        &self,
        pool: &PoolState,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let liquid = self.total_value_in_lp;
        let low_watermark = self.low_watermark(pool)?;
        if liquid < low_watermark || safe_sub_u256(liquid, low_watermark)? < amount_in {
            return Err(SimulationError::RecoverableError("EXCEEDED_REDEEMABLE".to_string()));
        }
        let mut pool = *pool;
        pool.redemption.limit = pool
            .redemption
            .limit
            .consume(redemption_units(amount_in)?, self.execution_block_timestamp)
            .ok_or_else(|| {
                SimulationError::RecoverableError("REDEMPTION_RATE_LIMIT".to_string())
            })?;

        let (eeth_shares, amount_out) = self.redemption_amounts(&pool, amount_in)?;
        if amount_out.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }
        let shares_to_burn = self.shares_for_withdrawal_amount(amount_out)?;
        let fee_shares = safe_sub_u256(eeth_shares, shares_to_burn)?;
        let fee_shares_to_treasury = mul_div(
            fee_shares,
            U256::from(
                pool.redemption
                    .exit_fee_split_to_treasury_bps,
            ),
            U256::from(BASIS_POINT_SCALE),
        )?;
        let fee_shares_to_stakers = safe_sub_u256(fee_shares, fee_shares_to_treasury)?;

        // `burnShares` values each burn after removing its shares. The withdrawal first
        // reduces pooled ether; the stakers' fee then raises the rate for the second burn.
        let mut next = self.clone();
        next.total_value_in_lp = safe_sub_u256(liquid, amount_out)?;
        next.total_shares = safe_sub_u256(self.total_shares, shares_to_burn)?;
        pool.burn_limit = pool
            .burn_limit
            .consume(
                gwei_units(next.amount_for_share(shares_to_burn)?),
                self.execution_block_timestamp,
            )
            .ok_or_else(|| SimulationError::RecoverableError("BURN_RATE_LIMIT".to_string()))?;
        next.total_shares = safe_sub_u256(next.total_shares, fee_shares_to_stakers)?;
        pool.burn_limit = pool
            .burn_limit
            .consume(
                gwei_units(next.amount_for_share(fee_shares_to_stakers)?),
                self.execution_block_timestamp,
            )
            .ok_or_else(|| SimulationError::RecoverableError("BURN_RATE_LIMIT".to_string()))?;
        next.venue = Venue::Pool(pool);

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(REDEEM_GAS),
            Box::new(next),
        ))
    }

    /// eETH -> weETH through `weETH.wrap`: mints the shares the eETH is worth, and the wrapper
    /// takes those shares in.
    fn amount_out_eeth_to_weeth(
        &self,
        wrapper: &WrapperState,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let shares = self.shares_for_amount(amount_in)?;
        if shares.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }
        let mut next = self.clone();
        next.venue = Venue::Wrapper(WrapperState {
            weeth_shares: safe_add_u256(wrapper.weeth_shares, shares)?,
        });
        Ok(GetAmountOutResult::new(
            u256_to_biguint(shares),
            BigUint::from(WRAP_GAS),
            Box::new(next),
        ))
    }

    /// weETH -> eETH through `weETH.unwrap`: pays the eETH the shares are worth out of the
    /// wrapper's own balance. `eETH.transfer` moves the shares that amount is worth, which
    /// rounding can leave a share short of the weETH burnt.
    fn amount_out_weeth_to_eeth(
        &self,
        wrapper: &WrapperState,
        amount_in: U256,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in > wrapper.weeth_shares {
            return Err(SimulationError::RecoverableError("WRAPPER_BALANCE_EXCEEDED".to_string()));
        }
        let amount_out = self.amount_for_share(amount_in)?;
        if amount_out.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }
        let shares_moved = self.shares_for_amount(amount_out)?;
        let mut next = self.clone();
        next.venue = Venue::Wrapper(WrapperState {
            weeth_shares: safe_sub_u256(wrapper.weeth_shares, shares_moved)?,
        });
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(UNWRAP_GAS),
            Box::new(next),
        ))
    }

    /// What each bucket can pay at `now`; `None` for the wrapper, which has no bucket.
    fn consumable_units(&self, now: u64) -> Option<[u64; 3]> {
        match &self.venue {
            Venue::Pool(pool) => Some([
                pool.redemption.limit.consumable(now),
                pool.mint_limit.consumable(now),
                pool.burn_limit.consumable(now),
            ]),
            Venue::Wrapper(_) => None,
        }
    }
}

/// The values one delta carries for a component, decoded but not yet applied.
enum VenueFields {
    Pool {
        redemption_bucket: [Option<u64>; 4],
        mint_bucket: [Option<u64>; 4],
        burn_bucket: [Option<u64>; 4],
        exit_fee_split_to_treasury_bps: Option<u16>,
        exit_fee_bps: Option<u16>,
        low_watermark_bps: Option<u16>,
    },
    Wrapper {
        weeth_shares: Option<U256>,
    },
}

#[typetag::serde]
impl ProtocolSim for EtherfiState {
    /// Zero, for both components.
    ///
    /// The wrapper charges nothing in either direction. The pool charges the exit fee on
    /// `eETH -> ETH` and nothing on `ETH -> eETH`, and this interface carries one value with no
    /// direction to attach it to, so the exit fee reaches callers through `spot_price` and
    /// `get_amount_out`, which both apply it.
    fn fee(&self) -> f64 {
        0f64
    }

    /// Prices exactly the direction pairs each component performs.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let quote_unit_f64 = u256_to_f64(U256::from(10).pow(U256::from(quote.decimals)))?;
        let base_unit = U256::from(10).pow(U256::from(base.decimals));
        let to_price = |amount_out: U256| -> Result<f64, SimulationError> {
            Ok(u256_to_f64(amount_out)? / quote_unit_f64)
        };

        match (&self.venue, base.address.as_ref(), quote.address.as_ref()) {
            // Depositing mints shares worth the ETH, and the depositor holds the eETH balance
            // those shares are worth: near parity, not the share count.
            (Venue::Pool(_), ETH, EETH) => {
                to_price(self.amount_for_share(self.shares_for_amount(base_unit)?)?)
            }
            // Redeeming pays the shares net of the exit fee.
            (Venue::Pool(pool), EETH, ETH) => {
                let net_shares = mul_div(
                    self.shares_for_amount(base_unit)?,
                    U256::from(pool.redemption.net_of_exit_fee_bps()?),
                    U256::from(BASIS_POINT_SCALE),
                )?;
                to_price(self.amount_for_share(net_shares)?)
            }
            (Venue::Wrapper(_), EETH, WEETH) => to_price(self.shares_for_amount(base_unit)?),
            (Venue::Wrapper(_), WEETH, EETH) => to_price(self.amount_for_share(base_unit)?),
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
        // Every entry point reverts on a zero amount.
        if amount_in.is_zero() {
            return Err(SimulationError::RecoverableError("ZERO_AMOUNT".to_string()));
        }

        match (&self.venue, token_in.address.as_ref(), token_out.address.as_ref()) {
            (Venue::Pool(pool), ETH, EETH) => self.amount_out_eth_to_eeth(pool, amount_in),
            (Venue::Pool(pool), EETH, ETH) => self.amount_out_eeth_to_eth(pool, amount_in),
            (Venue::Wrapper(wrapper), EETH, WEETH) => {
                self.amount_out_eeth_to_weeth(wrapper, amount_in)
            }
            (Venue::Wrapper(wrapper), WEETH, EETH) => {
                self.amount_out_weeth_to_eeth(wrapper, amount_in)
            }
            _ => Err(SimulationError::FatalError("unsupported swap".to_string())),
        }
    }

    /// A conservative sell bound each direction settles, and what it pays. A pair this component
    /// does not hold is an error.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let now = self.execution_block_timestamp;
        match (&self.venue, sell_token.as_ref(), buy_token.as_ref()) {
            // Bounded by the mint bucket and by the `uint128` `totalValueInLp` is stored in.
            (Venue::Pool(pool), ETH, EETH) => {
                let headroom = U256::from(u128::MAX).saturating_sub(self.total_value_in_lp);
                let mint = U256::from(pool.mint_limit.consumable(now)) * U256::from(GWEI);
                let max_in = headroom.min(mint);
                if max_in.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                let max_out = self
                    .amount_out_eth_to_eeth(pool, max_in)?
                    .amount;
                Ok((u256_to_biguint(max_in), max_out))
            }
            // Bounded by the liquidity above the low watermark, the redemption bucket and the
            // burn bucket. Below the watermark nothing is redeemable, so capacity is zero.
            (Venue::Pool(pool), EETH, ETH) => {
                let low_watermark = self.low_watermark(pool)?;
                if self.total_value_in_lp <= low_watermark {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                // With pooled ether P and input x, both burns together cost at most
                // x*P/(P-x): at most x*S/P shares burn, and both use at least the final
                // remaining shares as denominator. Reserve one bucket unit for the two
                // separate round-ups, then solve x*P/(P-x) <= burn_budget directly.
                let burn_budget = U256::from(
                    pool.burn_limit
                        .consumable(now)
                        .saturating_sub(1),
                ) * U256::from(GWEI);
                let pooled = self.total_pooled_ether()?;
                let burn_max = mul_div(burn_budget, pooled, safe_add_u256(pooled, burn_budget)?)?;
                let max_in = (self.total_value_in_lp - low_watermark)
                    .min(
                        U256::from(pool.redemption.limit.consumable(now)) *
                            U256::from(REDEMPTION_BUCKET_UNIT),
                    )
                    .min(burn_max);
                if max_in.is_zero() ||
                    self.redemption_amounts(pool, max_in)?
                        .1
                        .is_zero()
                {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                let max_out = self
                    .amount_out_eeth_to_eth(pool, max_in)?
                    .amount;
                Ok((u256_to_biguint(max_in), max_out))
            }

            // Wrapping is not capped by the protocol; no more eETH can be wrapped than exists
            // outside the wrapper.
            (Venue::Wrapper(wrapper), EETH, WEETH) => {
                let max_in = self
                    .total_pooled_ether()?
                    .saturating_sub(self.amount_for_share(wrapper.weeth_shares)?);
                if max_in.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                Ok((u256_to_biguint(max_in), u256_to_biguint(self.shares_for_amount(max_in)?)))
            }
            // Unwrapping pays out of the eETH the wrapper holds, so the wrapper's shares bound it.
            (Venue::Wrapper(wrapper), WEETH, EETH) => {
                if wrapper.weeth_shares.is_zero() {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
                Ok((
                    u256_to_biguint(wrapper.weeth_shares),
                    u256_to_biguint(self.amount_for_share(wrapper.weeth_shares)?),
                ))
            }
            _ => Err(SimulationError::FatalError("unsupported swap".to_string())),
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        let read = |name: &str| -> Result<Option<U256>, TransitionError> {
            attributes
                .get(name)
                .map(|value| decode_attribute(name, value))
                .transpose()
                .map_err(TransitionError::DecodeError)
        };
        let read_u64 = |name: &str| -> Result<Option<u64>, TransitionError> {
            attributes
                .get(name)
                .map(|value| decode_u64_attribute(name, value))
                .transpose()
                .map_err(TransitionError::DecodeError)
        };
        let read_u16 = |name: &str| -> Result<Option<u16>, TransitionError> {
            attributes
                .get(name)
                .map(|value| decode_u16_attribute(name, value))
                .transpose()
                .map_err(TransitionError::DecodeError)
        };
        let read_bucket = |names: &BucketAttributes| -> Result<[Option<u64>; 4], TransitionError> {
            Ok([
                read_u64(names.capacity)?,
                read_u64(names.remaining)?,
                read_u64(names.last_refill)?,
                read_u64(names.refill_rate)?,
            ])
        };
        fn apply_bucket(bucket: &mut BucketLimit, fields: [Option<u64>; 4]) {
            let [capacity, remaining, last_refill, refill_rate] = fields;
            if let Some(value) = capacity {
                bucket.capacity = value;
            }
            if let Some(value) = remaining {
                bucket.remaining = value;
            }
            if let Some(value) = last_refill {
                bucket.last_refill = value;
            }
            if let Some(value) = refill_rate {
                bucket.refill_rate = value;
            }
        }

        // Every read that can fail happens before the first assignment, so a delta the decoder
        // rejects leaves the state as it was. Each component reads only its own attributes, and
        // `venue_fields` is decoded against `self.venue`, so the venue arms below only assign.
        let total_value_out_of_lp = read(TOTAL_VALUE_OUT_OF_LP_ATTR)?;
        let total_value_in_lp = read(TOTAL_VALUE_IN_LP_ATTR)?;
        let total_shares = read(TOTAL_SHARES_ATTR)?;
        let venue_fields = match &self.venue {
            Venue::Pool(_) => VenueFields::Pool {
                redemption_bucket: read_bucket(&REDEMPTION_BUCKET)?,
                mint_bucket: read_bucket(&MINT_BUCKET)?,
                burn_bucket: read_bucket(&BURN_BUCKET)?,
                exit_fee_split_to_treasury_bps: read_u16(EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR)?,
                exit_fee_bps: read_u16(EXIT_FEE_BPS_ATTR)?,
                low_watermark_bps: read_u16(LOW_WATERMARK_BPS_ATTR)?,
            },
            Venue::Wrapper(_) => VenueFields::Wrapper { weeth_shares: read(WEETH_SHARES_ATTR)? },
        };

        if let Some(value) = total_value_out_of_lp {
            self.total_value_out_of_lp = value;
        }
        if let Some(value) = total_value_in_lp {
            self.total_value_in_lp = value;
        }
        if let Some(value) = total_shares {
            self.total_shares = value;
        }
        match (&mut self.venue, venue_fields) {
            (
                Venue::Pool(pool),
                VenueFields::Pool {
                    redemption_bucket,
                    mint_bucket,
                    burn_bucket,
                    exit_fee_split_to_treasury_bps,
                    exit_fee_bps,
                    low_watermark_bps,
                },
            ) => {
                apply_bucket(&mut pool.redemption.limit, redemption_bucket);
                apply_bucket(&mut pool.mint_limit, mint_bucket);
                apply_bucket(&mut pool.burn_limit, burn_bucket);
                if let Some(value) = exit_fee_split_to_treasury_bps {
                    pool.redemption
                        .exit_fee_split_to_treasury_bps = value;
                }
                if let Some(value) = exit_fee_bps {
                    pool.redemption.exit_fee_bps = value;
                }
                if let Some(value) = low_watermark_bps {
                    pool.redemption.low_watermark_bps = value;
                }
            }
            (Venue::Wrapper(wrapper), VenueFields::Wrapper { weeth_shares }) => {
                if let Some(value) = weeth_shares {
                    wrapper.weeth_shares = value;
                }
            }
            // Unreachable: `venue_fields` was decoded against `self.venue`, which nothing above
            // changes. The arm exists so the match is exhaustive without a wildcard.
            (Venue::Pool(_), VenueFields::Wrapper { .. }) |
            (Venue::Wrapper(_), VenueFields::Pool { .. }) => {
                return Err(TransitionError::DecodeError(
                    "the decoded attributes belong to the other component".to_string(),
                ))
            }
        }
        Ok(())
    }

    /// Advances to the block a quote would execute in, so the three buckets keep refilling on
    /// the blocks where EtherFi's own storage did not move.
    ///
    /// Re-emits only when a bucket's capacity actually changed: a repeated block short-circuits,
    /// the wrapper has nothing time-dependent, and full buckets stay full.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let timestamp = block.timestamp();
        if timestamp == self.execution_block_timestamp {
            return false;
        }
        let before = self.consumable_units(self.execution_block_timestamp);
        self.execution_block_timestamp = timestamp;
        before != self.consumable_units(timestamp)
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
    /// The attributes the pool component carries, in the order the decoder reads them.
    pub(super) const POOL_ATTRS: [&str; 18] = [
        TOTAL_VALUE_OUT_OF_LP_ATTR,
        TOTAL_VALUE_IN_LP_ATTR,
        TOTAL_SHARES_ATTR,
        REDEMPTION_BUCKET.capacity,
        REDEMPTION_BUCKET.remaining,
        REDEMPTION_BUCKET.last_refill,
        REDEMPTION_BUCKET.refill_rate,
        EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR,
        EXIT_FEE_BPS_ATTR,
        LOW_WATERMARK_BPS_ATTR,
        MINT_BUCKET.capacity,
        MINT_BUCKET.remaining,
        MINT_BUCKET.last_refill,
        MINT_BUCKET.refill_rate,
        BURN_BUCKET.capacity,
        BURN_BUCKET.remaining,
        BURN_BUCKET.last_refill,
        BURN_BUCKET.refill_rate,
    ];

    /// The attributes the wrapper component carries.
    pub(super) const WRAPPER_ATTRS: [&str; 4] =
        [TOTAL_VALUE_OUT_OF_LP_ATTR, TOTAL_VALUE_IN_LP_ATTR, TOTAL_SHARES_ATTR, WEETH_SHARES_ATTR];
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

    /// Block 25940000, where the package starts.
    const BLOCK_TIMESTAMP: u64 = 1_788_959_135;

    fn u256_dec(value: &str) -> U256 {
        U256::from_str_radix(value, 10).expect("valid base-10 U256")
    }

    fn token(address: [u8; 20], symbol: &str) -> Token {
        Token::new(&Bytes::from(address), symbol, 18, 0, &[Some(0)], Chain::Ethereum, 100)
    }

    fn eeth_token() -> Token {
        token(EETH_ADDRESS, "eETH")
    }

    fn weeth_token() -> Token {
        token(WEETH_ADDRESS, "weETH")
    }

    fn eth_token() -> Token {
        token(ETH_ADDRESS, "ETH")
    }

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18u64))
    }

    /// `EtherFiRedemptionManager.tokenToRedemptionInfo(ETH)` at block 25940000.
    fn redemption_info() -> RedemptionInfo {
        RedemptionInfo {
            limit: BucketLimit {
                capacity: 2_000_000_000,
                remaining: 1_999_666_518,
                last_refill: 1_787_040_551,
                refill_rate: 23_148,
            },
            exit_fee_split_to_treasury_bps: 1000,
            exit_fee_bps: 30,
            low_watermark_bps: 100,
        }
    }

    /// `EtherFiRateLimiter.getLimit` for the two eETH buckets at block 25940000.
    fn pool_venue() -> PoolState {
        PoolState {
            redemption: redemption_info(),
            mint_limit: BucketLimit {
                capacity: 40_000_000_000_000,
                remaining: 39_997_892_494_750,
                last_refill: 1_788_957_011,
                refill_rate: 22_222_222_222,
            },
            burn_limit: BucketLimit {
                capacity: 25_000_000_000_000,
                remaining: 24_975_002_499_999,
                last_refill: 1_788_923_411,
                refill_rate: 1_736_111_111,
            },
        }
    }

    /// The pool component at block 25940000: 1051 ETH liquid against a 22079 ETH floor, so
    /// redemption is closed.
    fn pool_state() -> EtherfiState {
        EtherfiState::new(
            BLOCK_TIMESTAMP,
            u256_dec("2206910247995761361317226"),
            u256_dec("1051493289032982041238"),
            u256_dec("2001243491556134113932753"),
            Venue::Pool(pool_venue()),
        )
    }

    /// The pool component with 30,000 ETH liquid: ~7,600 ETH above the floor.
    fn pool_state_with_liquidity() -> EtherfiState {
        let mut state = pool_state();
        state.total_value_in_lp = U256::from(30_000u64) * one_eth();
        state
    }

    /// The wrapper component at block 25940000.
    fn wrapper_state() -> EtherfiState {
        EtherfiState::new(
            BLOCK_TIMESTAMP,
            u256_dec("2206910247995761361317226"),
            u256_dec("1051493289032982041238"),
            u256_dec("2001243491556134113932753"),
            Venue::Wrapper(WrapperState { weeth_shares: u256_dec("1934528716353929340955601") }),
        )
    }

    /// The names the component carries.
    fn attribute_names(state: &EtherfiState) -> &'static [&'static str] {
        match &state.venue {
            Venue::Pool(_) => &POOL_ATTRS,
            Venue::Wrapper(_) => &WRAPPER_ATTRS,
        }
    }

    fn pool_of(state: &EtherfiState) -> PoolState {
        let Venue::Pool(pool) = &state.venue else { panic!("not the pool component") };
        *pool
    }

    fn wrapper_of(state: &EtherfiState) -> WrapperState {
        let Venue::Wrapper(wrapper) = &state.venue else { panic!("not the wrapper component") };
        *wrapper
    }

    fn state_of(result: &GetAmountOutResult) -> EtherfiState {
        result
            .new_state
            .as_any()
            .downcast_ref::<EtherfiState>()
            .expect("etherfi state")
            .clone()
    }

    fn recoverable(err: SimulationError) -> String {
        let SimulationError::RecoverableError(message) = err else {
            panic!("expected a recoverable error, got {err:?}");
        };
        message
    }

    /// `eETH.balanceOf(weETH)` at block 25940000, to the wei: the wrapper's shares at the rate.
    #[test]
    fn wrapper_shares_are_worth_the_chain_balance() {
        let state = wrapper_state();
        let worth = state
            .amount_for_share(wrapper_of(&state).weeth_shares)
            .expect("amount");
        assert_eq!(worth, u256_dec("2134355669936453442791966"));
    }

    #[test]
    fn deposit_credits_an_eeth_balance_near_the_deposit() {
        let state = pool_state();

        let result = state
            .get_amount_out(u256_to_biguint(one_eth()), &eth_token(), &eeth_token())
            .expect("amount out");

        // Depositing 1 ETH credits an eETH balance worth ~1 ETH, above the share count.
        let shares = state
            .shares_for_amount(one_eth())
            .expect("shares");
        assert!(result.amount > u256_to_biguint(shares));
        let deposit = u256_to_biguint(one_eth());
        assert!(&deposit - &result.amount < u256_to_biguint(one_eth() / U256::from(1_000_000u32)));

        let next = state_of(&result);
        assert_eq!(next.total_value_in_lp, state.total_value_in_lp + one_eth());
        assert_eq!(next.total_shares, state.total_shares + shares);
    }

    #[test]
    fn deposit_draws_the_minted_balance_from_the_mint_bucket() {
        let state = pool_state();
        let before = pool_of(&state)
            .mint_limit
            .refilled(BLOCK_TIMESTAMP);

        let result = state
            .get_amount_out(u256_to_biguint(one_eth()), &eth_token(), &eeth_token())
            .expect("amount out");

        let after = pool_of(&state_of(&result)).mint_limit;
        let drawn = gwei_units(biguint_to_u256(&result.amount));
        assert_eq!(after.remaining, before.remaining - drawn);
        assert_eq!(after.last_refill, BLOCK_TIMESTAMP);
    }

    /// The bucket refilled to its 40,000 ETH capacity by block 25940000, so that is the cap.
    #[test]
    fn deposit_limit_is_the_mint_bucket() {
        let state = pool_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(ETH_ADDRESS), Bytes::from(EETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(U256::from(40_000u64) * one_eth()));
        assert!(max_out > &max_in * 999u32 / 1000u32);
        // The limit settles, and one bucket unit more does not.
        let quoted = state
            .get_amount_out(max_in.clone(), &eth_token(), &eeth_token())
            .expect("a quote at the limit");
        assert_eq!(quoted.amount, max_out);
        let err = state
            .get_amount_out(&max_in + BigUint::from(GWEI), &eth_token(), &eeth_token())
            .unwrap_err();
        assert_eq!(recoverable(err), "MINT_RATE_LIMIT");
    }

    /// `totalValueInLp` is a `uint128`; a deposit that overflows it reverts on chain.
    #[test]
    fn deposit_limit_respects_the_uint128_pool_value() {
        let mut state = pool_state();
        let headroom = U256::from(5u64) * U256::from(GWEI);
        state.total_value_in_lp = U256::from(u128::MAX) - headroom;
        // Keep the share rate at parity, so a deposit inside the headroom still mints shares.
        state.total_shares = state
            .total_pooled_ether()
            .expect("pooled");

        let (max_in, _) = state
            .get_limits(Bytes::from(ETH_ADDRESS), Bytes::from(EETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(headroom));
        let err = state
            .get_amount_out(&max_in + BigUint::from(1u8), &eth_token(), &eeth_token())
            .unwrap_err();
        assert_eq!(recoverable(err), "LIQUIDITY_POOL_CAPACITY");
    }

    #[test]
    fn redemption_is_closed_below_the_low_watermark() {
        let state = pool_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
        let err = state
            .get_amount_out(BigUint::from(1u64), &eeth_token(), &eth_token())
            .unwrap_err();
        assert_eq!(recoverable(err), "EXCEEDED_REDEEMABLE");
    }

    /// `_calcRedemption`: the receiver gets the shares net of the 30 bps fee, the treasury keeps
    /// a tenth of the fee shares, and the rest of the fee shares are burnt for the stakers.
    #[test]
    fn redemption_pays_net_of_the_exit_fee_and_burns_the_stakers_share() {
        let state = pool_state_with_liquidity();

        let result = state
            .get_amount_out(u256_to_biguint(one_eth()), &eeth_token(), &eth_token())
            .expect("amount out");

        // 30 bps off one eETH, and the payout is worth 891962367676204779 shares.
        assert_eq!(result.amount, BigUint::from(996_999_999_999_999_999u64));

        let next = state_of(&result);
        // The redemption is paid out of `totalValueInLp`.
        assert_eq!(next.total_value_in_lp, u256_dec("29999003000000000000001"));
        // `sharesForWithdrawalAmount` rounds up, so the pool keeps the odd wei-share. What
        // leaves `totalShares` is the payout's shares plus the stakers' nine tenths of the
        // 2683938919787979 fee shares; the treasury keeps the other tenth as eETH.
        assert_eq!(state.total_shares - next.total_shares, u256_dec("894377912704013961"));
        assert_eq!(next.total_shares, u256_dec("2001242597178221409918792"));

        // Both burns are metered in gwei, rounded up, against the eETH burn bucket, which has
        // refilled to its 25,000 ETH capacity by this block: 997000000 units for the payout and
        // 2700001 for the stakers' fee.
        let Venue::Pool(pool) = &next.venue else {
            panic!("the pool component stays a pool");
        };
        assert_eq!(pool.burn_limit.remaining, 24_999_000_299_999);
    }

    #[test]
    fn redemption_burn_metering_uses_each_post_burn_share_rate() {
        let state = pool_state_with_liquidity();
        let result = state
            .get_amount_out(
                u256_to_biguint(one_eth() * U256::from(1000)),
                &eeth_token(),
                &eth_token(),
            )
            .expect("redemption");
        let next = state_of(&result);
        let Venue::Pool(pool) = next.venue else { panic!("pool") };
        // Withdrawal reduces pooled ether, then each burn reduces
        // totalShares before valuing its shares for the burn bucket. The two burns consume
        // 997000000000 and 2700003261 gwei units respectively.
        assert_eq!(pool.burn_limit.remaining, 24_000_299_996_739);
    }

    #[test]
    fn redemption_rejects_burn_capacity_below_post_burn_charge() {
        let mut state = pool_state_with_liquidity();
        let Venue::Pool(ref mut pool) = state.venue else { panic!("pool") };
        pool.burn_limit.remaining = 999_700_000_001;
        pool.burn_limit.last_refill = BLOCK_TIMESTAMP;
        // The available burn capacity is 3260 units below the required on-chain charge.
        let err = state
            .get_amount_out(
                u256_to_biguint(one_eth() * U256::from(1000)),
                &eeth_token(),
                &eth_token(),
            )
            .unwrap_err();
        assert_eq!(recoverable(err), "BURN_RATE_LIMIT");
    }

    #[test]
    fn redemption_limit_remains_executable_when_all_fees_go_to_stakers() {
        let mut state = pool_state_with_liquidity();
        let Venue::Pool(ref mut pool) = state.venue else { panic!("pool") };
        pool.redemption
            .exit_fee_split_to_treasury_bps = 0;
        pool.burn_limit.capacity = 1_000_000_000_000;
        pool.burn_limit.remaining = 1_000_000_000_000;
        pool.burn_limit.last_refill = BLOCK_TIMESTAMP;
        let (limit, output) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("executable limit");
        assert!(limit > u256_to_biguint(one_eth() * U256::from(999)));
        assert!(limit < u256_to_biguint(one_eth() * U256::from(1000)));
        let quote = state
            .get_amount_out(limit.clone(), &eeth_token(), &eth_token())
            .expect("limit quotes");
        assert_eq!(quote.amount, output);
    }

    #[test]
    fn conservative_redemption_limits_settle_across_fee_and_burn_budgets() {
        for fee in [0, 30, 5000, 9999, 10000] {
            for treasury_split in [0, 1000, 10000] {
                for burn_units in [0, 1, 2, 100, 1_000_000_000_000] {
                    let mut state = pool_state_with_liquidity();
                    // A large burn budget relative to pooled ether exercises substantial
                    // share-rate changes during redemption.
                    state.total_value_in_lp = one_eth() * U256::from(100);
                    state.total_value_out_of_lp = U256::ZERO;
                    state.total_shares = one_eth() * U256::from(80);
                    let Venue::Pool(ref mut pool) = state.venue else { panic!("pool") };
                    pool.redemption.exit_fee_bps = fee;
                    pool.redemption
                        .exit_fee_split_to_treasury_bps = treasury_split;
                    pool.redemption.low_watermark_bps = 0;
                    pool.burn_limit.capacity = burn_units;
                    pool.burn_limit.remaining = burn_units;
                    pool.burn_limit.last_refill = BLOCK_TIMESTAMP;
                    let (limit, output) = state
                        .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
                        .expect("conservative limit");
                    if burn_units <= 1 || fee == 10000 {
                        assert_eq!((limit, output), (BigUint::ZERO, BigUint::ZERO));
                        continue;
                    }
                    assert!(limit > BigUint::ZERO);
                    let quote = state
                        .get_amount_out(limit, &eeth_token(), &eth_token())
                        .expect("limit settles within both burn charges");
                    assert_eq!(quote.amount, output);
                }
            }
        }
    }

    /// Both components report zero. The wrapper's is its real fee; the pool's understates the
    /// exit fee it charges one way, which this interface cannot express and `spot_price` and
    /// `get_amount_out` carry instead.
    #[test]
    fn fee_is_zero_until_the_interface_can_carry_a_direction() {
        assert_eq!(wrapper_state().fee(), 0.0);
        assert_eq!(pool_state().fee(), 0.0);

        // The exit fee is in the quote, which is where a caller has to read it.
        let quoted = pool_state_with_liquidity()
            .get_amount_out(u256_to_biguint(one_eth()), &eeth_token(), &eth_token())
            .expect("amount out");
        assert!(biguint_to_u256(&quoted.amount) < one_eth());
    }

    /// The redemption manager rejects an exit fee above the basis-point scale, so one that
    /// arrives anyway is malformed. Subtracting it would wrap in a release build and quote a
    /// payout many times the input.
    #[test]
    fn an_exit_fee_above_the_basis_point_scale_is_refused() {
        let mut state = pool_state_with_liquidity();
        if let Venue::Pool(pool) = &mut state.venue {
            pool.redemption.exit_fee_bps = 10_001;
        }

        let err = state
            .get_amount_out(u256_to_biguint(one_eth()), &eeth_token(), &eth_token())
            .unwrap_err();
        assert!(matches!(err, SimulationError::FatalError(_)), "{err:?}");
        assert!(state
            .spot_price(&eeth_token(), &eth_token())
            .is_err());
    }

    /// 7,600 ETH above the floor, but the redemption bucket holds 2,000 ETH.
    #[test]
    fn redemption_limit_is_bounded_by_the_redemption_bucket() {
        let state = pool_state_with_liquidity();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(U256::from(2_000u64) * one_eth()));
        let quoted = state
            .get_amount_out(max_in.clone(), &eeth_token(), &eth_token())
            .expect("a quote at the limit");
        assert_eq!(quoted.amount, max_out);
        let err = state
            .get_amount_out(
                &max_in + BigUint::from(REDEMPTION_BUCKET_UNIT),
                &eeth_token(),
                &eth_token(),
            )
            .unwrap_err();
        assert_eq!(recoverable(err), "REDEMPTION_RATE_LIMIT");
    }

    #[test]
    fn redemption_limit_is_bounded_by_the_liquidity_above_the_floor() {
        let mut state = pool_state();
        // The floor is 1% of getTotalPooledEther(). These two put it at 1,000 ETH and leave
        // exactly 10 ETH above it, which is far under both rate-limit buckets.
        state.total_value_in_lp = U256::from(1_010u64) * one_eth();
        state.total_value_out_of_lp = U256::from(98_990u64) * one_eth();

        let (max_in, _) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(U256::from(10u64) * one_eth()));
        state
            .get_amount_out(max_in, &eeth_token(), &eth_token())
            .expect("a quote at the reported limit");
    }

    #[test]
    fn redemption_bucket_refills_with_the_execution_block() {
        let state = pool_state_with_liquidity();
        let (max_in, _) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");
        let mut drained = state_of(
            &state
                .get_amount_out(max_in, &eeth_token(), &eth_token())
                .expect("amount out"),
        );

        let err = drained
            .get_amount_out(u256_to_biguint(one_eth()), &eeth_token(), &eth_token())
            .unwrap_err();
        assert_eq!(recoverable(err), "REDEMPTION_RATE_LIMIT");

        // 1000 seconds refill 1000 * 23148 units = 23.148 ETH.
        assert!(drained.apply_block(&BlockContext::new(25_940_100, BLOCK_TIMESTAMP + 1000)));
        let (max_in, _) = drained
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS))
            .expect("limits");
        assert_eq!(
            max_in,
            u256_to_biguint(U256::from(23_148_000u64) * U256::from(REDEMPTION_BUCKET_UNIT))
        );
        drained
            .get_amount_out(
                u256_to_biguint(U256::from(20u64) * one_eth()),
                &eeth_token(),
                &eth_token(),
            )
            .expect("a quote after refilling");
    }

    #[test]
    fn apply_block_is_idempotent_and_only_reports_a_changed_capacity() {
        let mut pool = pool_state();
        assert!(!pool.apply_block(&BlockContext::new(25_940_000, BLOCK_TIMESTAMP)));
        // Every bucket is full at this block, so time alone changes nothing.
        assert!(!pool.apply_block(&BlockContext::new(25_940_001, BLOCK_TIMESTAMP + 12)));
        assert_eq!(pool.execution_block_timestamp, BLOCK_TIMESTAMP + 12);

        let mut wrapper = wrapper_state();
        assert!(!wrapper.apply_block(&BlockContext::new(25_940_001, BLOCK_TIMESTAMP + 12)));
    }

    #[test]
    fn wrapping_and_unwrapping_move_the_wrapper_shares() {
        let state = wrapper_state();
        let shares_before = wrapper_of(&state).weeth_shares;

        let wrapped = state
            .get_amount_out(u256_to_biguint(one_eth()), &eeth_token(), &weeth_token())
            .expect("wrap");
        let weeth = biguint_to_u256(&wrapped.amount);
        assert_eq!(
            weeth,
            state
                .shares_for_amount(one_eth())
                .expect("shares")
        );
        let after_wrap = state_of(&wrapped);
        assert_eq!(wrapper_of(&after_wrap).weeth_shares, shares_before + weeth);

        let unwrapped = after_wrap
            .get_amount_out(wrapped.amount.clone(), &weeth_token(), &eeth_token())
            .expect("unwrap");
        let eeth = biguint_to_u256(&unwrapped.amount);
        assert_eq!(
            eeth,
            state
                .amount_for_share(weeth)
                .expect("amount")
        );
        // The transfer moves the shares the payout is worth, which rounding can leave a share
        // short of the weETH burnt.
        let moved = state
            .shares_for_amount(eeth)
            .expect("shares");
        assert!(moved <= weeth && weeth - moved <= U256::ONE);
        assert_eq!(wrapper_of(&state_of(&unwrapped)).weeth_shares, shares_before + weeth - moved);
    }

    #[test]
    fn unwrap_is_bounded_by_the_wrapper_shares() {
        let state = wrapper_state();
        let shares = wrapper_of(&state).weeth_shares;

        let (max_in, max_out) = state
            .get_limits(Bytes::from(WEETH_ADDRESS), Bytes::from(EETH_ADDRESS))
            .expect("limits");

        assert_eq!(max_in, u256_to_biguint(shares));
        assert_eq!(max_out, u256_to_biguint(u256_dec("2134355669936453442791966")));
        let quoted = state
            .get_amount_out(max_in.clone(), &weeth_token(), &eeth_token())
            .expect("a quote at the limit");
        assert_eq!(quoted.amount, max_out);
        let err = state
            .get_amount_out(&max_in + BigUint::from(1u8), &weeth_token(), &eeth_token())
            .unwrap_err();
        assert_eq!(recoverable(err), "WRAPPER_BALANCE_EXCEEDED");
    }

    #[test]
    fn unwrapping_everything_closes_the_direction() {
        let state = wrapper_state();
        let (max_in, _) = state
            .get_limits(Bytes::from(WEETH_ADDRESS), Bytes::from(EETH_ADDRESS))
            .expect("limits");
        let emptied = state_of(
            &state
                .get_amount_out(max_in, &weeth_token(), &eeth_token())
                .expect("unwrap"),
        );

        let (max_in, max_out) = emptied
            .get_limits(Bytes::from(WEETH_ADDRESS), Bytes::from(EETH_ADDRESS))
            .expect("limits");
        // Rounding may leave a share behind; at most that.
        assert!(max_in <= BigUint::from(1u8));
        assert!(max_out <= BigUint::from(2u8));
    }

    #[test]
    fn wrap_limit_is_the_eeth_outside_the_wrapper() {
        let state = wrapper_state();

        let (max_in, max_out) = state
            .get_limits(Bytes::from(EETH_ADDRESS), Bytes::from(WEETH_ADDRESS))
            .expect("limits");

        let outside = state
            .total_pooled_ether()
            .expect("pooled") -
            u256_dec("2134355669936453442791966");
        assert_eq!(max_in, u256_to_biguint(outside));
        assert_eq!(
            max_out,
            u256_to_biguint(
                state
                    .shares_for_amount(outside)
                    .expect("shares")
            )
        );
        let quoted = state
            .get_amount_out(max_in, &eeth_token(), &weeth_token())
            .expect("a quote at the limit");
        assert_eq!(quoted.amount, max_out);
    }

    #[test]
    fn zero_amount_is_refused_in_every_direction() {
        let pool = pool_state_with_liquidity();
        let wrapper = wrapper_state();
        for (state, token_in, token_out) in [
            (&pool, eth_token(), eeth_token()),
            (&pool, eeth_token(), eth_token()),
            (&wrapper, eeth_token(), weeth_token()),
            (&wrapper, weeth_token(), eeth_token()),
        ] {
            let err = state
                .get_amount_out(BigUint::ZERO, &token_in, &token_out)
                .unwrap_err();
            assert_eq!(recoverable(err), "ZERO_AMOUNT");
        }
    }

    #[test]
    fn spot_prices_cover_each_components_directions() {
        let pool = pool_state();
        let deposit = pool
            .spot_price(&eth_token(), &eeth_token())
            .expect("price");
        let redeem = pool
            .spot_price(&eeth_token(), &eth_token())
            .expect("price");
        // Both legs are ~1:1; only the 30 bps exit fee moves the redeem leg off parity.
        assert!((deposit - 1.0).abs() < 1e-6, "deposit off parity: {deposit}");
        assert!(redeem < 1.0 && redeem > 0.996, "redeem not fee-adjusted: {redeem}");

        let wrapper = wrapper_state();
        let wrap = wrapper
            .spot_price(&eeth_token(), &weeth_token())
            .expect("price");
        let unwrap = wrapper
            .spot_price(&weeth_token(), &eeth_token())
            .expect("price");
        // eETH.balanceOf(weETH) / eETH.shares(weETH) at block 25940000 is ~1.1033.
        assert!((unwrap - 1.1033).abs() < 1e-3, "unwrap rate: {unwrap}");
        assert!((wrap * unwrap - 1.0).abs() < 1e-6, "wrap and unwrap are not inverse");
    }

    /// A pair the component does not hold is a wiring error, not a direction with no capacity.
    #[test]
    fn a_pair_the_component_does_not_hold_is_an_error() {
        let pool = pool_state();
        let wrapper = wrapper_state();
        for (state, sell, buy) in [
            (&pool, WEETH_ADDRESS, EETH_ADDRESS),
            (&wrapper, ETH_ADDRESS, EETH_ADDRESS),
            (&pool, WEETH_ADDRESS, ETH_ADDRESS),
            (&wrapper, WEETH_ADDRESS, ETH_ADDRESS),
        ] {
            assert!(matches_fatal(state.get_limits(Bytes::from(sell), Bytes::from(buy))));
            assert!(matches_fatal(state.get_amount_out(
                BigUint::from(1u64),
                &token(sell, "in"),
                &token(buy, "out")
            )));
            assert!(matches_fatal(state.spot_price(&token(sell, "in"), &token(buy, "out"))));
        }
    }

    fn matches_fatal<T>(result: Result<T, SimulationError>) -> bool {
        match result {
            Err(SimulationError::FatalError(_)) => true,
            Err(_) | Ok(_) => false,
        }
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

    fn bucket_attributes(bucket: &BucketLimit, names: &BucketAttributes) -> Vec<(String, Bytes)> {
        vec![
            (names.capacity.to_string(), attribute(U256::from(bucket.capacity))),
            (names.remaining.to_string(), attribute(U256::from(bucket.remaining))),
            (names.last_refill.to_string(), attribute(U256::from(bucket.last_refill))),
            (names.refill_rate.to_string(), attribute(U256::from(bucket.refill_rate))),
        ]
    }

    fn common_attributes(state: &EtherfiState) -> Vec<(String, Bytes)> {
        vec![
            (TOTAL_VALUE_OUT_OF_LP_ATTR.to_string(), attribute(state.total_value_out_of_lp)),
            (TOTAL_VALUE_IN_LP_ATTR.to_string(), attribute(state.total_value_in_lp)),
            (TOTAL_SHARES_ATTR.to_string(), attribute(state.total_shares)),
        ]
    }

    fn pool_attributes(state: &EtherfiState) -> HashMap<String, Bytes> {
        let pool = pool_of(state);
        let mut attributes = common_attributes(state);
        attributes.extend(bucket_attributes(&pool.redemption.limit, &REDEMPTION_BUCKET));
        attributes.extend(bucket_attributes(&pool.mint_limit, &MINT_BUCKET));
        attributes.extend(bucket_attributes(&pool.burn_limit, &BURN_BUCKET));
        attributes.extend([
            (
                EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR.to_string(),
                attribute(U256::from(
                    pool.redemption
                        .exit_fee_split_to_treasury_bps,
                )),
            ),
            (EXIT_FEE_BPS_ATTR.to_string(), attribute(U256::from(pool.redemption.exit_fee_bps))),
            (
                LOW_WATERMARK_BPS_ATTR.to_string(),
                attribute(U256::from(pool.redemption.low_watermark_bps)),
            ),
        ]);
        attributes.into_iter().collect()
    }

    fn wrapper_attributes(state: &EtherfiState) -> HashMap<String, Bytes> {
        let mut attributes = common_attributes(state);
        attributes.push((WEETH_SHARES_ATTR.to_string(), attribute(wrapper_of(state).weeth_shares)));
        attributes.into_iter().collect()
    }

    fn snapshot(
        component_id: &str,
        attributes: HashMap<String, Bytes>,
    ) -> tycho_client::feed::synchronizer::ComponentWithState {
        tycho_client::feed::synchronizer::ComponentWithState {
            state: ProtocolComponentState {
                component_id: component_id.to_string(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: component_id.to_string(),
                protocol_system: "etherfi".to_string(),
                protocol_type_name: "ethereum_etherfi_pool".to_string(),
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

    async fn decode(
        snapshot: tycho_client::feed::synchronizer::ComponentWithState,
    ) -> Result<EtherfiState, InvalidSnapshotError> {
        EtherfiState::try_from_with_header(
            snapshot,
            BlockHeader { timestamp: BLOCK_TIMESTAMP, ..Default::default() },
            &HashMap::default(),
            &HashMap::default(),
            &Default::default(),
        )
        .await
    }

    #[tokio::test]
    async fn decoder_builds_the_pool_component() {
        let expected = pool_state();
        let decoded = decode(snapshot(POOL_COMPONENT_ID, pool_attributes(&expected)))
            .await
            .expect("decoded");
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn decoder_builds_the_wrapper_component() {
        let expected = wrapper_state();
        let decoded = decode(snapshot(WRAPPER_COMPONENT_ID, wrapper_attributes(&expected)))
            .await
            .expect("decoded");
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn decoder_accepts_a_checksummed_component_id() {
        let expected = wrapper_state();
        let decoded = decode(snapshot(
            "0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee",
            wrapper_attributes(&expected),
        ))
        .await
        .expect("decoded");
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn decoder_rejects_an_unknown_component_id() {
        let err = try_decode_snapshot_with_defaults::<EtherfiState>(snapshot(
            "0xdeadbeef",
            wrapper_attributes(&wrapper_state()),
        ))
        .await
        .unwrap_err();
        let InvalidSnapshotError::ValueError(message) = err else {
            panic!("expected a value error, got {err:?}");
        };
        assert!(message.contains("0xdeadbeef"), "{message}");
    }

    #[tokio::test]
    async fn decoder_rejects_a_missing_attribute() {
        let mut attributes = pool_attributes(&pool_state());
        attributes.remove(MINT_BUCKET.remaining);
        let err = decode(snapshot(POOL_COMPONENT_ID, attributes))
            .await
            .unwrap_err();
        let InvalidSnapshotError::MissingAttribute(name) = err else {
            panic!("expected a missing attribute, got {err:?}");
        };
        assert_eq!(name, MINT_BUCKET.remaining);
    }

    /// A whole word where the package emits a `uint128` half cannot be a value the package
    /// produced, so the decoder reports it by name.
    #[tokio::test]
    async fn decoder_rejects_an_attribute_wider_than_its_field() {
        let state = pool_state();
        let mut attributes = pool_attributes(&state);
        attributes.insert(
            TOTAL_VALUE_IN_LP_ATTR.to_string(),
            Bytes::from(
                state
                    .total_value_in_lp
                    .to_be_bytes_vec(),
            ),
        );
        let err = decode(snapshot(POOL_COMPONENT_ID, attributes))
            .await
            .unwrap_err();
        let InvalidSnapshotError::ValueError(message) = err else {
            panic!("expected a value error, got {err:?}");
        };
        assert!(message.contains(TOTAL_VALUE_IN_LP_ATTR), "{message}");
    }

    fn delta(component_id: &str, attributes: Vec<(String, Bytes)>) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: component_id.to_string(),
            updated_attributes: attributes.into_iter().collect(),
            deleted_attributes: Default::default(),
        }
    }

    #[test]
    fn delta_transition_updates_the_pool() {
        let mut state = pool_state();
        let redemption_limit =
            BucketLimit { capacity: 5, remaining: 4, last_refill: 3, refill_rate: 2 };
        let mut attributes = vec![
            (TOTAL_VALUE_IN_LP_ATTR.to_string(), attribute(U256::from(7u64))),
            (TOTAL_SHARES_ATTR.to_string(), attribute(U256::from(9u64))),
            (EXIT_FEE_BPS_ATTR.to_string(), attribute(U256::from(45u64))),
        ];
        attributes.extend(bucket_attributes(&redemption_limit, &REDEMPTION_BUCKET));

        state
            .delta_transition(
                delta(POOL_COMPONENT_ID, attributes),
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("transition");

        assert_eq!(state.total_value_in_lp, U256::from(7u64));
        assert_eq!(state.total_shares, U256::from(9u64));
        let pool = pool_of(&state);
        assert_eq!(pool.redemption.limit, redemption_limit);
        assert_eq!(pool.redemption.exit_fee_bps, 45);
        assert_eq!(pool.mint_limit, pool_venue().mint_limit);
    }

    #[test]
    fn delta_transition_updates_the_wrapper() {
        let mut state = wrapper_state();

        state
            .delta_transition(
                delta(
                    WRAPPER_COMPONENT_ID,
                    vec![(WEETH_SHARES_ATTR.to_string(), attribute(U256::from(11u64)))],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("transition");

        assert_eq!(wrapper_of(&state).weeth_shares, U256::from(11u64));
    }

    /// A bucket field is 64 bits. A nine-byte value cannot come from the package, and narrowing
    /// it would wrap the timestamp the bucket refills from.
    #[test]
    fn delta_transition_rejects_an_attribute_wider_than_its_field() {
        let mut state = pool_state();
        let before = state.clone();

        let err = state
            .delta_transition(
                delta(
                    POOL_COMPONENT_ID,
                    vec![
                        (TOTAL_SHARES_ATTR.to_string(), attribute(U256::from(1u64))),
                        (MINT_BUCKET.last_refill.to_string(), attribute(U256::from(1u64) << 64)),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap_err();

        let TransitionError::DecodeError(message) = err else {
            panic!("expected a decode error, got {err:?}");
        };
        assert!(message.contains(MINT_BUCKET.last_refill), "{message}");
        assert_eq!(state, before, "a rejected delta must leave the state untouched");
    }

    #[test]
    fn bucket_refill_caps_at_capacity() {
        let limit = BucketLimit { capacity: 10, remaining: 1, last_refill: 100, refill_rate: 5 };
        let refilled = limit.refilled(103);
        assert_eq!(refilled.remaining, 10);
        assert_eq!(refilled.last_refill, 103);
    }

    #[test]
    fn bucket_refill_is_a_noop_at_or_before_the_last_refill() {
        let limit = BucketLimit { capacity: 10, remaining: 4, last_refill: 100, refill_rate: 5 };
        assert_eq!(limit.refilled(100), limit);
        assert_eq!(limit.refilled(99), limit);
    }

    #[test]
    fn bucket_consume_draws_after_refilling() {
        let limit = BucketLimit { capacity: 10, remaining: 1, last_refill: 100, refill_rate: 2 };
        let after = limit
            .consume(4, 102)
            .expect("consumable");
        assert_eq!(after.remaining, 1);
        assert_eq!(after.last_refill, 102);
        assert!(limit.consume(6, 102).is_none());
    }

    #[test]
    fn gwei_units_round_up_and_saturate() {
        assert_eq!(gwei_units(U256::from(GWEI - 1)), 1);
        assert_eq!(gwei_units(U256::from(GWEI * 2)), 2);
        assert_eq!(gwei_units(U256::MAX), u64::MAX);
    }

    #[test]
    fn redemption_units_round_up_and_reject_oversized_amounts() {
        assert_eq!(redemption_units(U256::from(REDEMPTION_BUCKET_UNIT - 1)).unwrap(), 1);
        assert_eq!(redemption_units(U256::from(REDEMPTION_BUCKET_UNIT * 3)).unwrap(), 3);
        let too_large = U256::from(u64::MAX) * U256::from(REDEMPTION_BUCKET_UNIT);
        assert_eq!(recoverable(redemption_units(too_large).unwrap_err()), "AMOUNT_TOO_LARGE");
    }

    /// The pool with 100 gwei of burn capacity, far under the liquidity above the floor and
    /// under the redemption bucket, so the burn bucket is what bounds redemption.
    fn pool_state_bound_by_the_burn_bucket() -> EtherfiState {
        let mut state = pool_state_with_liquidity();
        if let Venue::Pool(pool) = &mut state.venue {
            pool.burn_limit = BucketLimit {
                capacity: 100,
                remaining: 100,
                last_refill: BLOCK_TIMESTAMP,
                refill_rate: 0,
            };
        }
        state
    }

    /// The pool with five units of redemption capacity left, so the redemption manager's own
    /// bucket is what bounds redemption.
    fn pool_state_bound_by_the_redemption_bucket() -> EtherfiState {
        let mut state = pool_state_with_liquidity();
        if let Venue::Pool(pool) = &mut state.venue {
            pool.redemption.limit = BucketLimit {
                capacity: 5,
                remaining: 5,
                last_refill: BLOCK_TIMESTAMP,
                refill_rate: 0,
            };
        }
        state
    }

    /// The pairs a component's venue performs, which are the pairs its three entry points
    /// answer for.
    fn supported_pairs(state: &EtherfiState) -> Vec<(Bytes, Bytes)> {
        match state.venue {
            Venue::Pool(_) => vec![
                (Bytes::from(ETH_ADDRESS), Bytes::from(EETH_ADDRESS)),
                (Bytes::from(EETH_ADDRESS), Bytes::from(ETH_ADDRESS)),
            ],
            Venue::Wrapper(_) => vec![
                (Bytes::from(EETH_ADDRESS), Bytes::from(WEETH_ADDRESS)),
                (Bytes::from(WEETH_ADDRESS), Bytes::from(EETH_ADDRESS)),
            ],
        }
    }

    fn every_token_pair() -> Vec<(Bytes, Bytes)> {
        let tokens = [ETH_ADDRESS, EETH_ADDRESS, WEETH_ADDRESS];
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
        for state in [
            pool_state_with_liquidity(),
            pool_state_bound_by_the_burn_bucket(),
            pool_state_bound_by_the_redemption_bucket(),
            wrapper_state(),
        ] {
            for (sell, buy) in supported_pairs(&state) {
                let (max_in, max_out) = state
                    .get_limits(sell.clone(), buy.clone())
                    .unwrap_or_else(|e| panic!("{sell:x} -> {buy:x} has no limit: {e:?}"));
                if max_in == BigUint::ZERO {
                    assert_eq!(
                        max_out,
                        BigUint::ZERO,
                        "{sell:x} -> {buy:x} pays out of a zero limit"
                    );
                    continue;
                }
                let token_in = token(sell.as_ref().try_into().unwrap(), "in");
                let token_out = token(buy.as_ref().try_into().unwrap(), "out");
                let quoted = state
                    .get_amount_out(max_in.clone(), &token_in, &token_out)
                    .unwrap_or_else(|e| panic!("{sell:x} -> {buy:x} limit does not quote: {e:?}"));
                assert_eq!(
                    quoted.amount, max_out,
                    "{sell:x} -> {buy:x} limit disagrees with quote"
                );
            }
        }
    }

    /// A pair the component does not hold is an error in all three entry points, so a token
    /// wired to the wrong component cannot read as a direction with no capacity.
    #[test]
    fn unsupported_pairs_are_refused_by_every_entry_point() {
        let supported = [
            (POOL_COMPONENT_ID, ETH_ADDRESS, EETH_ADDRESS),
            (POOL_COMPONENT_ID, EETH_ADDRESS, ETH_ADDRESS),
            (WRAPPER_COMPONENT_ID, EETH_ADDRESS, WEETH_ADDRESS),
            (WRAPPER_COMPONENT_ID, WEETH_ADDRESS, EETH_ADDRESS),
        ];
        for state in [pool_state_with_liquidity(), wrapper_state()] {
            let id = match state.venue {
                Venue::Pool(_) => POOL_COMPONENT_ID,
                Venue::Wrapper(_) => WRAPPER_COMPONENT_ID,
            };
            for (sell, buy) in every_token_pair() {
                let sell_bytes: [u8; 20] = sell.as_ref().try_into().unwrap();
                let buy_bytes: [u8; 20] = buy.as_ref().try_into().unwrap();
                if supported.contains(&(id, sell_bytes, buy_bytes)) {
                    continue;
                }
                assert!(matches_fatal(state.get_limits(sell.clone(), buy.clone())));
                assert!(matches_fatal(state.get_amount_out(
                    BigUint::from(1u64),
                    &token(sell_bytes, "in"),
                    &token(buy_bytes, "out")
                )));
                assert!(matches_fatal(
                    state.spot_price(&token(sell_bytes, "in"), &token(buy_bytes, "out"))
                ));
            }
        }
    }

    /// `sharesForAmount` and `amountForShare` are inverse up to their rounding, so neither can
    /// be applied to a figure already in the other unit without the round trip drifting.
    #[test]
    fn share_and_amount_round_trip() {
        let state = pool_state();
        // Each division truncates by under one unit, and the first loss is then scaled by
        // the share rate, so a round trip can lose the rate plus one.
        let tolerance = state
            .amount_for_share(U256::ONE)
            .expect("rate") +
            U256::from(2u8);
        for exponent in [15u32, 18, 21, 24] {
            let amount = U256::from(10u64).pow(U256::from(exponent));
            let back = state
                .amount_for_share(
                    state
                        .shares_for_amount(amount)
                        .expect("shares"),
                )
                .expect("amount");
            assert!(back <= amount && amount - back <= tolerance, "amount drifted at 1e{exponent}");

            let shares = U256::from(10u64).pow(U256::from(exponent));
            let back = state
                .shares_for_amount(
                    state
                        .amount_for_share(shares)
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
        for (id, names, build) in [
            (
                POOL_COMPONENT_ID,
                POOL_ATTRS.as_slice(),
                pool_attributes as fn(&EtherfiState) -> HashMap<String, Bytes>,
            ),
            (WRAPPER_COMPONENT_ID, WRAPPER_ATTRS.as_slice(), wrapper_attributes),
        ] {
            let state = if id == POOL_COMPONENT_ID { pool_state() } else { wrapper_state() };
            let full = build(&state);
            assert_eq!(full.len(), names.len(), "{id} carries a name outside its list");
            for name in names {
                let mut attributes = full.clone();
                attributes.remove(*name);
                let err = decode(snapshot(id, attributes))
                    .await
                    .unwrap_err();
                let InvalidSnapshotError::MissingAttribute(missing) = err else {
                    panic!("{name} removed but the decoder did not report it: {err:?}");
                };
                assert_eq!(&missing, name);
            }
        }
    }

    /// Every name the component carries moves the state, so a delta the package sends cannot be
    /// silently dropped and left frozen at the snapshot value.
    #[test]
    fn delta_transition_applies_every_attribute_the_component_carries() {
        for base in [pool_state(), wrapper_state()] {
            let id = match base.venue {
                Venue::Pool(_) => POOL_COMPONENT_ID,
                Venue::Wrapper(_) => WRAPPER_COMPONENT_ID,
            };
            for name in attribute_names(&base) {
                let mut state = base.clone();
                // 7 fits every field and differs from every value in the fixtures.
                state
                    .delta_transition(
                        delta(id, vec![(name.to_string(), attribute(U256::from(7u64)))]),
                        &HashMap::new(),
                        &Balances::default(),
                    )
                    .unwrap_or_else(|e| panic!("{name} was rejected: {e:?}"));
                assert_ne!(state, base, "{name} left the state untouched");
            }
        }
    }

    /// A name the other component carries is ignored, malformed or not. Each component decodes
    /// only its own names, so a package that starts emitting an attribute for one of them does
    /// not stop deltas reaching the other.
    #[test]
    fn delta_transition_ignores_the_other_components_names() {
        let mut pool = pool_state();
        let before = pool.clone();
        pool.delta_transition(
            delta(
                POOL_COMPONENT_ID,
                vec![(WEETH_SHARES_ATTR.to_string(), Bytes::from(vec![1u8; 33]))],
            ),
            &HashMap::new(),
            &Balances::default(),
        )
        .expect("a wrapper name leaves the pool alone");
        assert_eq!(pool, before);

        let mut wrapper = wrapper_state();
        let before = wrapper.clone();
        wrapper
            .delta_transition(
                delta(
                    WRAPPER_COMPONENT_ID,
                    vec![(MINT_BUCKET.capacity.to_string(), Bytes::from(vec![1u8; 9]))],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("a pool name leaves the wrapper alone");
        assert_eq!(wrapper, before);
    }

    /// The stream decoder puts the chain head in every delta. Those names are not EtherFi
    /// attributes and leave the state alone.
    #[test]
    fn delta_transition_accepts_the_injected_block_attributes() {
        let mut state = pool_state();
        state
            .delta_transition(
                delta(
                    POOL_COMPONENT_ID,
                    vec![
                        (
                            "block_number".to_string(),
                            Bytes::from(25_940_001u64.to_be_bytes().to_vec()),
                        ),
                        (
                            "block_timestamp".to_string(),
                            Bytes::from(BLOCK_TIMESTAMP.to_be_bytes().to_vec()),
                        ),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("the injected names are tolerated");
        assert_eq!(state, pool_state());
    }

    /// The ETH side has to be the address Tycho gives native ETH, not the router's
    /// 0xEeee..EEeE sentinel. The substreams package reports this address as a component token
    /// and `EtherfiSwapEncoder` compares against `Chain::native_token()`, so a mismatch here
    /// leaves the token unpriced and makes every ETH-side swap fail to encode.
    #[test]
    fn eth_address_is_the_chain_native_token() {
        assert_eq!(Bytes::from(ETH_ADDRESS), Chain::Ethereum.native_token().address);
    }
}
