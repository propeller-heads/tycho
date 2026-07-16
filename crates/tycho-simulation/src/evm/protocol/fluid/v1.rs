/// FluidV1 simulation logic.
///
/// This implementation is a port from the [Kyberswap reference implementation](https://github.com/KyberNetwork/kyberswap-dex-lib/blob/main/pkg/liquidity-source/fluid/dex-t1/pool_simulator.go)
/// functions and errors are ported equivalently and then used to implement the ProtocolSim
/// interface.
///
/// ## Differences
/// - Native ETH: Tycho uses a zero-byte address while Fluid uses 0xeee... address
/// - Limits: Tycho uses binary search to find limits that will actually execute
/// - State: Tycho uses the local VM to retrieve and update the state of each pool
use std::{
    any::Any,
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::{U256, U512};
use num_bigint::{BigUint, ToBigUint};
use num_traits::Euclid;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, Price, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        fluid::{v1::constant::RESERVES_RESOLVER, vm},
        safe_math::sqrt_u512,
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        utils::add_fee_markup,
    },
    query_pool_swap::price_to_f64_with_decimals,
};

mod constant {
    use alloy::{hex, primitives::U256};

    pub const MAX_PRICE_DIFF: U256 = U256::from_limbs([5, 0, 0, 0]); // 5
    pub const MIN_SWAP_LIQUIDITY: U256 = U256::from_limbs([8500, 0, 0, 0]); // 8500
    pub const SIX_DECIMALS: U256 = U256::from_limbs([1000000, 0, 0, 0]); // 1e6
    pub const TWO_DECIMALS: U256 = U256::from_limbs([100, 0, 0, 0]); // 1e2
    pub const B_I1E18: U256 = U256::from_limbs([0x0DE0B6B3A7640000, 0, 0, 0]); // 1e18
    pub const B_I1E27: U256 = U256::from_limbs([0x9fd0803ce8000000, 0x33b2e3c, 0, 0]); // 1e27
    pub const DEX_AMOUNT_DECIMALS: i64 = 12;
    pub const FEE_PERCENT_PRECISION: U256 = U256::from_limbs([10000, 0, 0, 0]);
    pub const ZERO_ADDRESS: &[u8] = &hex!("0x0000000000000000000000000000000000000000");
    pub const NATIVE_ADDRESS: &[u8] = &hex!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
    pub const RESERVES_RESOLVER: &[u8] = &hex!("0xc93876c0eed99645dd53937b25433e311881a27c");
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FluidV1 {
    pool_address: Bytes,
    token0: Token,
    token1: Token,
    collateral_reserves: CollateralReserves,
    debt_reserves: DebtReserves,
    dex_limits: DexLimits,
    center_price: U256,
    fee: U256,
    sync_time: u64,
    pool_reserve0: U256,
    pool_reserve1: U256,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct CollateralReserves {
    pub(super) token0_real_reserves: U256,
    pub(super) token1_real_reserves: U256,
    pub(super) token0_imaginary_reserves: U256,
    pub(super) token1_imaginary_reserves: U256,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct DebtReserves {
    pub(super) token0_real_reserves: U256,
    pub(super) token1_real_reserves: U256,
    pub(super) token0_imaginary_reserves: U256,
    pub(super) token1_imaginary_reserves: U256,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct DexLimits {
    pub(super) borrowable_token0: TokenLimit,
    pub(super) borrowable_token1: TokenLimit,
    pub(super) withdrawable_token0: TokenLimit,
    pub(super) withdrawable_token1: TokenLimit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct TokenLimit {
    pub(super) available: U256,
    pub(super) expands_to: U256,
    pub(super) expand_duration: U256,
}

#[derive(Debug, Error)]
enum SwapError {
    #[error("Insufficient reserve: tokenOut amount exceeds reserve")]
    InsufficientReserve,
    #[error("Insufficient reserve: tokenOut amount exceeds borrowable limit")]
    InsufficientBorrowable,
    #[error("Insufficient reserve: tokenOut amount exceeds withdrawable limit")]
    InsufficientWithdrawable,
    #[error("Insufficient reserve: tokenOut amount exceeds max price limit")]
    InsufficientMaxPrice,
    #[error("Invalid reserves ratio")]
    VerifyReservesRatiosInvalid,
    #[error("No pools are enabled")]
    NoPoolsEnabled,
    #[error("InvalidAmountIn: Amount too low")]
    InvalidAmountIn,
}

impl From<SwapError> for SimulationError {
    fn from(value: SwapError) -> Self {
        Self::FatalError(value.to_string())
    }
}
impl FluidV1 {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        pool_address: &Bytes,
        token0: &Token,
        token1: &Token,
        collateral_reserves: CollateralReserves,
        debt_reserves: DebtReserves,
        dex_limits: DexLimits,
        center_price: U256,
        fee: U256,
        sync_time: u64,
    ) -> Self {
        let pool_reserve0 = get_max_reserves(
            token0.decimals as u8,
            &dex_limits.withdrawable_token0,
            &dex_limits.borrowable_token0,
            &collateral_reserves.token0_real_reserves,
            &debt_reserves.token0_real_reserves,
        );
        let pool_reserve1 = get_max_reserves(
            token1.decimals as u8,
            &dex_limits.withdrawable_token1,
            &dex_limits.borrowable_token1,
            &collateral_reserves.token1_real_reserves,
            &debt_reserves.token1_real_reserves,
        );

        // potentially flip token0 and token1 since ETH address is different from our eth marker
        // address
        let (token0_normalized, token1_normalized) =
            if FluidV1::normalize_native_address(&token0.address) <
                FluidV1::normalize_native_address(&token1.address)
            {
                (token0.clone(), token1.clone())
            } else {
                (token1.clone(), token0.clone())
            };
        Self {
            pool_address: pool_address.clone(),
            token0: token0_normalized,
            token1: token1_normalized,
            collateral_reserves,
            debt_reserves,
            dex_limits,
            center_price,
            fee,
            sync_time,
            pool_reserve0,
            pool_reserve1,
        }
    }

    fn normalize_native_address(address: &Bytes) -> &[u8] {
        if address == constant::ZERO_ADDRESS {
            constant::NATIVE_ADDRESS
        } else {
            address
        }
    }

    /// Converts a target pool price (raw `token_out/token_in` units) into the equivalent
    /// imaginary-reserve ratio in the DEX's 12-decimal adjusted space, with the fee markup
    /// stripped (`spot_price` reports `ratio / (1 - fee)`, so the reserves must reach
    /// `target * (1 - fee)`).
    fn adjusted_target_ratio(
        &self,
        target: &Price,
        decimals_in: i64,
        decimals_out: i64,
    ) -> Result<(U512, U512), SimulationError> {
        if target.numerator.bits() > 256 || target.denominator.bits() > 256 {
            return Err(SimulationError::InvalidInput(
                "Target price fraction exceeds 256 bits".to_string(),
                None,
            ));
        }
        let mut numerator = U512::from(biguint_to_u256(&target.numerator));
        let mut denominator = U512::from(biguint_to_u256(&target.denominator));

        let out_shift = constant::DEX_AMOUNT_DECIMALS - decimals_out;
        if out_shift >= 0 {
            numerator = checked_mul_512(numerator, ten_pow_512(out_shift))?;
        } else {
            denominator = checked_mul_512(denominator, ten_pow_512(-out_shift))?;
        }
        let in_shift = constant::DEX_AMOUNT_DECIMALS - decimals_in;
        if in_shift >= 0 {
            denominator = checked_mul_512(denominator, ten_pow_512(in_shift))?;
        } else {
            numerator = checked_mul_512(numerator, ten_pow_512(-in_shift))?;
        }

        numerator = checked_mul_512(numerator, U512::from(constant::SIX_DECIMALS - self.fee))?;
        denominator = checked_mul_512(denominator, U512::from(constant::SIX_DECIMALS))?;
        Ok((numerator, denominator))
    }

    /// Computes the swap that moves the pool's marginal price down to `target` in closed form.
    ///
    /// Each enabled sub-pool (collateral, debt) is a constant-product AMM over its imaginary
    /// reserves and Fluid's router equalizes their marginal prices, so the input required to
    /// reach a target price is the sum of the per-pool closed forms
    /// `sqrt(k / target) - imaginary_reserve_in`, computed in the 12-decimal adjusted space.
    ///
    /// The result is finalized through [`ProtocolSim::get_amount_out`], which enforces all pool
    /// limits (reserve caps, borrowable/withdrawable, the 5% max price move). If the sub-pools
    /// start at materially different prices the closed form can drift from the router's actual
    /// split; in that case this falls back to the numerical search.
    fn swap_to_price(
        &self,
        params: &QueryPoolSwapParams,
        target: &Price,
        tolerance: f64,
    ) -> Result<PoolSwap, SimulationError> {
        let token_in = params.token_in();
        let token_out = params.token_out();
        let zero2one = token_in.address == self.token0.address;
        if !((zero2one && token_out.address == self.token1.address) ||
            (token_in.address == self.token1.address &&
                token_out.address == self.token0.address))
        {
            return Err(SimulationError::InvalidInput(
                format!("Tokens do not match pool {}", self.pool_address),
                None,
            ));
        }

        let spot = self.spot_price(token_in, token_out)?;
        let target_f64 = price_to_f64_with_decimals(target, token_in.decimals, token_out.decimals)?;
        if target_f64 > spot {
            return Err(SimulationError::InvalidInput(
                format!("Target {target_f64} > spot {spot}"),
                None,
            ));
        }

        let (ratio_num, ratio_den) = self.adjusted_target_ratio(
            target,
            token_in.decimals as i64,
            token_out.decimals as i64,
        )?;

        let col = &self.collateral_reserves;
        let debt = &self.debt_reserves;
        let col_enabled = col.token0_real_reserves > U256::ZERO &&
            col.token1_real_reserves > U256::ZERO &&
            col.token0_imaginary_reserves > U256::ZERO &&
            col.token1_imaginary_reserves > U256::ZERO;
        let debt_enabled = debt.token0_real_reserves > U256::ZERO &&
            debt.token1_real_reserves > U256::ZERO &&
            debt.token0_imaginary_reserves > U256::ZERO &&
            debt.token1_imaginary_reserves > U256::ZERO;
        if !col_enabled && !debt_enabled {
            return Err(SimulationError::from(SwapError::NoPoolsEnabled));
        }

        let sub_pools = if zero2one {
            [
                (col_enabled, col.token0_imaginary_reserves, col.token1_imaginary_reserves),
                (debt_enabled, debt.token0_imaginary_reserves, debt.token1_imaginary_reserves),
            ]
        } else {
            [
                (col_enabled, col.token1_imaginary_reserves, col.token0_imaginary_reserves),
                (debt_enabled, debt.token1_imaginary_reserves, debt.token0_imaginary_reserves),
            ]
        };

        let mut amount_in_adjusted = U512::ZERO;
        for (enabled, reserve_in, reserve_out) in sub_pools {
            if !enabled {
                continue;
            }
            let reserve_in = U512::from(reserve_in);
            let k = checked_mul_512(reserve_in, U512::from(reserve_out))?;
            // Constant product with marginal price r = reserve_out' / reserve_in' = k /
            // reserve_in'^2, so reserve_in' = sqrt(k / r).
            let new_reserve_in = sqrt_u512(checked_mul_512(k, ratio_den)? / ratio_num);
            amount_in_adjusted += new_reserve_in.saturating_sub(reserve_in);
        }

        // Below the swap dust threshold the target is already (nearly) met; report a no-op.
        if amount_in_adjusted < U512::from(constant::SIX_DECIMALS) {
            return Ok(PoolSwap::new(BigUint::ZERO, BigUint::ZERO, self.clone_box(), None));
        }

        let amount_after_fee =
            from_adjusted_amount_512(amount_in_adjusted, token_in.decimals as i64);
        // Gross the skimmed fee back up: get_amount_out charges fee = amount * fee / 1e6 first.
        let amount_in = checked_mul_512(amount_after_fee, U512::from(constant::SIX_DECIMALS))? /
            U512::from(constant::SIX_DECIMALS - self.fee);
        let amount_in = u512_to_biguint_checked(amount_in)?;

        let result = self
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .map_err(|err| {
                SimulationError::InvalidInput(
                    format!("Target price unreachable within pool limits: {err}"),
                    None,
                )
            })?;

        // The closed form assumes both sub-pools start at the same marginal price. When they
        // don't, the router's equalizing split lands elsewhere — detect it and fall back to the
        // numerical search.
        let new_spot = result
            .new_state
            .spot_price(token_in, token_out)?;
        let deviation = ((new_spot - target_f64) / target_f64).abs();
        if deviation > tolerance.max(SWAP_TO_PRICE_MAX_DEVIATION) {
            return crate::evm::query_pool_swap::query_pool_swap(self, params);
        }

        Ok(PoolSwap::new(amount_in, result.amount, result.new_state, None))
    }
}

/// Relative price deviation above which the closed-form result is distrusted and the numerical
/// search is used instead.
const SWAP_TO_PRICE_MAX_DEVIATION: f64 = 1e-4;

fn ten_pow_512(exponent: i64) -> U512 {
    U512::from(10u64).pow(U512::from(exponent as u64))
}

fn checked_mul_512(a: U512, b: U512) -> Result<U512, SimulationError> {
    a.checked_mul(b).ok_or_else(|| {
        SimulationError::FatalError("Overflow in fluid swap_to_price math".to_string())
    })
}

/// Same rescaling as [`from_adjusted_amount`] but over `U512`.
fn from_adjusted_amount_512(adjusted_amount: U512, decimals: i64) -> U512 {
    let diff = decimals - constant::DEX_AMOUNT_DECIMALS;
    if diff == 0 {
        adjusted_amount
    } else if diff < 0 {
        adjusted_amount / ten_pow_512(-diff)
    } else {
        adjusted_amount * ten_pow_512(diff)
    }
}

fn u512_to_biguint_checked(value: U512) -> Result<BigUint, SimulationError> {
    let limbs = value.as_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return Err(SimulationError::FatalError(
            "Overflow converting fluid swap_to_price amount to U256".to_string(),
        ));
    }
    Ok(u256_to_biguint(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])))
}

#[typetag::serde]
impl ProtocolSim for FluidV1 {
    fn fee(&self) -> f64 {
        let fee = u256_to_f64(self.fee).expect("Fluid fee values are safe to convert");
        let precision =
            u256_to_f64(constant::FEE_PERCENT_PRECISION).expect("FEE_PERCENT_PRECISION is safe");
        // Fee is in basis points: fee / FEE_PERCENT_PRECISION / 100
        // e.g., fee=68 means 68/10000/100 = 0.000068 = 0.0068%
        fee / precision / 100.0
    }

    fn spot_price(&self, base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        let price_f64 = if !self
            .collateral_reserves
            .token0_imaginary_reserves
            .is_zero()
        {
            u256_to_f64(
                self.collateral_reserves
                    .token1_imaginary_reserves,
            )? / u256_to_f64(
                self.collateral_reserves
                    .token0_imaginary_reserves,
            )?
        } else {
            u256_to_f64(
                self.debt_reserves
                    .token1_imaginary_reserves,
            )? / u256_to_f64(
                self.debt_reserves
                    .token0_imaginary_reserves,
            )?
        };
        let oriented_price_f64 =
            if base.address == self.token0.address { price_f64 } else { 1.0 / price_f64 };

        Ok(add_fee_markup(oriented_price_f64, self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::from(0u32) {
            return Ok(GetAmountOutResult {
                amount: BigUint::from(0u32),
                gas: BigUint::from(155433u32),
                new_state: Box::new(self.clone()),
            });
        }
        let zero2one = self.token0.address == token_in.address;

        let (token_in_decimals, token_out_decimals) = (token_in.decimals, token_out.decimals);

        let amount_in = biguint_to_u256(&amount_in);
        let fee = amount_in * self.fee / constant::SIX_DECIMALS;

        let amount_in_after_fee = amount_in - fee;
        let amount_in_adjusted = to_adjusted_amount(amount_in_after_fee, token_in_decimals as i64);

        if amount_in_adjusted < constant::SIX_DECIMALS ||
            amount_in_after_fee < constant::TWO_DECIMALS
        {
            return Err(SwapError::InvalidAmountIn.into());
        }
        let mut new_col_reserves = self.collateral_reserves.clone();
        let mut new_debt_reserves = self.debt_reserves.clone();
        let mut new_limits = self.dex_limits.clone();

        let amount_out = swap_in_adjusted(
            zero2one,
            amount_in_adjusted,
            &mut new_col_reserves,
            &mut new_debt_reserves,
            token_out_decimals as i64,
            &mut new_limits,
            self.center_price,
            self.sync_time,
        )?;

        let reserve = if zero2one { self.pool_reserve1 } else { self.pool_reserve0 };
        if amount_out > reserve {
            return Err(SwapError::InsufficientReserve.into());
        }

        let result = GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            155433.to_biguint().expect("infallible"),
            Box::new(Self {
                pool_address: self.pool_address.clone(),
                token0: self.token0.clone(),
                token1: self.token1.clone(),
                collateral_reserves: new_col_reserves,
                debt_reserves: new_debt_reserves,
                dex_limits: new_limits,
                center_price: self.center_price,
                fee: self.fee,
                sync_time: self.sync_time,
                pool_reserve0: self.pool_reserve0,
                pool_reserve1: self.pool_reserve1,
            }),
        );
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let zero2one = sell_token == self.token0.address;

        let (upper_bound_out, out_decimals, in_decimals) = if zero2one {
            (
                to_adjusted_amount(
                    self.dex_limits
                        .withdrawable_token0
                        .available +
                        self.dex_limits
                            .borrowable_token0
                            .available,
                    self.token0.decimals as i64,
                ),
                self.token1.decimals,
                self.token0.decimals,
            )
        } else {
            (
                to_adjusted_amount(
                    self.dex_limits
                        .withdrawable_token1
                        .available +
                        self.dex_limits
                            .borrowable_token1
                            .available,
                    self.token1.decimals as i64,
                ),
                self.token0.decimals,
                self.token1.decimals,
            )
        };
        if upper_bound_out == U256::ZERO {
            trace!("Upper bound is zero for {}", self.pool_address);
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let delta = U256::from(10).pow(U256::from(2));
        let (max_valid, res) = find_max_valid_u256(upper_bound_out, delta, |amount| {
            let mut col_clone = self.collateral_reserves.clone();
            let mut debt_clone = self.debt_reserves.clone();
            let mut limits_clone = self.dex_limits.clone();
            swap_in_adjusted(
                zero2one,
                amount,
                &mut col_clone,
                &mut debt_clone,
                out_decimals as i64,
                &mut limits_clone,
                self.center_price,
                self.sync_time,
            )
        });
        Ok((
            u256_to_biguint(from_adjusted_amount(max_valid, in_decimals as i64)),
            u256_to_biguint(res.unwrap_or_else(|| {
                trace!(
                    "All evaluations errored during limit search for {} -> {}",
                    sell_token,
                    buy_token
                );
                U256::ZERO
            })),
        ))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("Infallible");

        let state = vm::decode_from_vm(
            &self.pool_address,
            &self.token0,
            &self.token1,
            RESERVES_RESOLVER,
            engine,
        )?;

        trace!(?state, "Calling delta transition for {}", &self.pool_address);

        self.collateral_reserves = state.collateral_reserves;
        self.debt_reserves = state.debt_reserves;
        self.dex_limits = state.dex_limits;
        self.center_price = state.center_price;
        self.fee = state.fee;
        self.sync_time = state.sync_time;

        self.pool_reserve0 = get_max_reserves(
            self.token0.decimals as u8,
            &self.dex_limits.withdrawable_token0,
            &self.dex_limits.borrowable_token0,
            &self
                .collateral_reserves
                .token0_real_reserves,
            &self.debt_reserves.token0_real_reserves,
        );
        self.pool_reserve1 = get_max_reserves(
            self.token1.decimals as u8,
            &self.dex_limits.withdrawable_token1,
            &self.dex_limits.borrowable_token1,
            &self
                .collateral_reserves
                .token1_real_reserves,
            &self.debt_reserves.token1_real_reserves,
        );
        Ok(())
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
        if let Some(other_state) = other.as_any().downcast_ref::<Self>() {
            self == other_state
        } else {
            false
        }
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::PoolTargetPrice { target, tolerance, .. } => {
                self.swap_to_price(params, target, *tolerance)
            }
            SwapConstraint::TradeLimitPrice { .. } => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
        }
    }
}

/// Generic binary search for the largest `U256` input that doesn't return an error.
///
/// # Parameters
/// - `upper_bound`: The maximum value to test.
/// - `delta`: Stop searching when `high - low < delta`.
/// - `f`: A closure that takes a `U256` input and returns `Result<T, E>`.
///
/// # Returns
/// The largest input value for which `f(input)` succeeded.
pub fn find_max_valid_u256<T, E, F>(upper_bound: U256, delta: U256, mut f: F) -> (U256, Option<T>)
where
    F: FnMut(U256) -> Result<T, E>,
    E: std::fmt::Debug,
{
    let mut low = U256::ZERO;
    let mut high = upper_bound;
    let mut best = U256::ZERO;
    let mut best_result: Option<T> = None;

    while high > low + delta {
        let mid = (low + high) / U256::from(2);

        match f(mid) {
            Ok(result) => {
                best = mid;
                best_result = Some(result);
                low = mid;
            }
            Err(_) => {
                high = mid;
            }
        }
    }

    (best, best_result)
}

#[allow(clippy::too_many_arguments)]
fn swap_in_adjusted(
    swap0_to_1: bool,
    amount_to_swap: U256,
    col_reserves: &mut CollateralReserves,
    debt_reserves: &mut DebtReserves,
    out_decimals: i64,
    current_limits: &mut DexLimits,
    center_price: U256,
    sync_time: u64,
) -> Result<U256, SwapError> {
    let (
        col_reserve_in,
        col_reserve_out,
        col_i_reserve_in,
        col_i_reserve_out,
        debt_reserve_in,
        debt_reserve_out,
        debt_i_reserve_in,
        debt_i_reserve_out,
        borrowable,
        withdrawable,
    ) = if swap0_to_1 {
        (
            col_reserves.token0_real_reserves,
            col_reserves.token1_real_reserves,
            col_reserves.token0_imaginary_reserves,
            col_reserves.token1_imaginary_reserves,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_real_reserves,
            debt_reserves.token0_imaginary_reserves,
            debt_reserves.token1_imaginary_reserves,
            get_expanded_limit(sync_time, &current_limits.borrowable_token1),
            get_expanded_limit(sync_time, &current_limits.withdrawable_token1),
        )
    } else {
        (
            col_reserves.token1_real_reserves,
            col_reserves.token0_real_reserves,
            col_reserves.token1_imaginary_reserves,
            col_reserves.token0_imaginary_reserves,
            debt_reserves.token1_real_reserves,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_imaginary_reserves,
            debt_reserves.token0_imaginary_reserves,
            get_expanded_limit(sync_time, &current_limits.borrowable_token0),
            get_expanded_limit(sync_time, &current_limits.withdrawable_token0),
        )
    };

    // Adjust borrowable and withdrawable amounts to match output decimals
    let borrowable = to_adjusted_amount(borrowable, out_decimals);
    let withdrawable = to_adjusted_amount(withdrawable, out_decimals);

    // Check if all reserves are greater than 0
    let col_pool_enabled = col_reserves.token0_real_reserves > U256::ZERO &&
        col_reserves.token1_real_reserves > U256::ZERO &&
        col_reserves.token0_imaginary_reserves > U256::ZERO &&
        col_reserves.token1_imaginary_reserves > U256::ZERO;

    let debt_pool_enabled = debt_reserves.token0_real_reserves > U256::ZERO &&
        debt_reserves.token1_real_reserves > U256::ZERO &&
        debt_reserves.token0_imaginary_reserves > U256::ZERO &&
        debt_reserves.token1_imaginary_reserves > U256::ZERO;

    if !col_pool_enabled && !debt_pool_enabled {
        return Err(SwapError::NoPoolsEnabled);
    }

    let a = if col_pool_enabled && debt_pool_enabled {
        swap_routing_in(
            amount_to_swap,
            col_i_reserve_out,
            col_i_reserve_in,
            debt_i_reserve_out,
            debt_i_reserve_in,
        )
    } else if debt_pool_enabled {
        U256::MAX // Route from debt pool
    } else if col_pool_enabled {
        amount_to_swap + U256::ONE // Route from collateral pool
    } else {
        return Err(SwapError::NoPoolsEnabled);
    };

    let (amount_in_collateral, amount_out_collateral, amount_in_debt, amount_out_debt) = if a ==
        U256::ZERO ||
        a == U256::MAX
    {
        // Entire trade routes through debt pool
        let amount_out_debt = get_amount_out(amount_to_swap, debt_i_reserve_in, debt_i_reserve_out);
        (U256::ZERO, U256::ZERO, amount_to_swap, amount_out_debt)
    } else if a >= amount_to_swap {
        // Entire trade routes through collateral pool
        let amount_out_collateral =
            get_amount_out(amount_to_swap, col_i_reserve_in, col_i_reserve_out);
        (amount_to_swap, amount_out_collateral, U256::ZERO, U256::ZERO)
    } else {
        // Trade routes through both pools
        let amount_in_debt = amount_to_swap - a;
        let amount_out_debt = get_amount_out(amount_in_debt, debt_i_reserve_in, debt_i_reserve_out);
        let amount_out_collateral = get_amount_out(a, col_i_reserve_in, col_i_reserve_out);
        (a, amount_out_collateral, amount_in_debt, amount_out_debt)
    };

    if amount_out_debt > debt_reserve_out {
        return Err(SwapError::InsufficientReserve);
    }

    if amount_out_collateral > col_reserve_out {
        return Err(SwapError::InsufficientReserve);
    }

    if amount_out_debt > borrowable {
        return Err(SwapError::InsufficientBorrowable);
    }

    if amount_out_collateral > withdrawable {
        return Err(SwapError::InsufficientWithdrawable);
    }

    if amount_in_collateral > U256::ZERO {
        let reserves_ratio_valid = if swap0_to_1 {
            verify_token1_reserves(
                col_reserve_in + amount_in_collateral,
                col_reserve_out - amount_out_collateral,
                center_price,
            )
        } else {
            verify_token0_reserves(
                col_reserve_out - amount_out_collateral,
                col_reserve_in + amount_in_collateral,
                center_price,
            )
        };
        if !reserves_ratio_valid {
            return Err(SwapError::VerifyReservesRatiosInvalid);
        }
    }

    if amount_in_debt > U256::ZERO {
        let reserves_ratio_valid = if swap0_to_1 {
            verify_token1_reserves(
                debt_reserve_in + amount_in_debt,
                debt_reserve_out - amount_out_debt,
                center_price,
            )
        } else {
            verify_token0_reserves(
                debt_reserve_out - amount_out_debt,
                debt_reserve_in + amount_in_debt,
                center_price,
            )
        };
        if !reserves_ratio_valid {
            return Err(SwapError::VerifyReservesRatiosInvalid);
        }
    }

    let (old_price, new_price) = if amount_in_collateral > amount_in_debt {
        if swap0_to_1 {
            (
                col_i_reserve_out * constant::B_I1E27 / col_i_reserve_in,
                (col_i_reserve_out - amount_out_collateral) * constant::B_I1E27 /
                    (col_i_reserve_in + amount_in_collateral),
            )
        } else {
            (
                col_i_reserve_in * constant::B_I1E27 / col_i_reserve_out,
                (col_i_reserve_in + amount_in_collateral) * constant::B_I1E27 /
                    (col_i_reserve_out - amount_out_collateral),
            )
        }
    } else if swap0_to_1 {
        (
            debt_i_reserve_out * constant::B_I1E27 / debt_i_reserve_in,
            (debt_i_reserve_out - amount_out_debt) * constant::B_I1E27 /
                (debt_i_reserve_in + amount_in_debt),
        )
    } else {
        (
            debt_i_reserve_in * constant::B_I1E27 / debt_i_reserve_out,
            (debt_i_reserve_in + amount_in_debt) * constant::B_I1E27 /
                (debt_i_reserve_out - amount_out_debt),
        )
    };

    let price_diff = old_price.abs_diff(new_price);
    let max_price_diff = old_price * constant::MAX_PRICE_DIFF / constant::TWO_DECIMALS;

    if price_diff > max_price_diff {
        return Err(SwapError::InsufficientMaxPrice);
    }

    if amount_in_collateral > U256::ZERO {
        update_collateral_reserves_and_limits(
            swap0_to_1,
            amount_in_collateral,
            amount_out_collateral,
            col_reserves,
            current_limits,
            out_decimals,
        );
    }

    if amount_in_debt > U256::ZERO {
        update_debt_reserves_and_limits(
            swap0_to_1,
            amount_in_debt,
            amount_out_debt,
            debt_reserves,
            current_limits,
            out_decimals,
        );
    }

    Ok(from_adjusted_amount(amount_out_collateral + amount_out_debt, out_decimals))
}

#[allow(clippy::too_many_arguments, dead_code)]
fn swap_out_adjusted(
    swap0_to_1: bool,
    amount_to_receive: U256,
    col_reserves: &mut CollateralReserves,
    debt_reserves: &mut DebtReserves,
    in_decimals: i64,
    out_decimals: i64,
    current_limits: &mut DexLimits,
    center_price: U256,
    sync_time: u64,
) -> Result<U256, SwapError> {
    let (
        col_reserve_in,
        col_reserve_out,
        col_i_reserve_in,
        col_i_reserve_out,
        debt_reserve_in,
        debt_reserve_out,
        debt_i_reserve_in,
        debt_i_reserve_out,
        borrowable,
        withdrawable,
    ) = if swap0_to_1 {
        (
            col_reserves.token0_real_reserves,
            col_reserves.token1_real_reserves,
            col_reserves.token0_imaginary_reserves,
            col_reserves.token1_imaginary_reserves,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_real_reserves,
            debt_reserves.token0_imaginary_reserves,
            debt_reserves.token1_imaginary_reserves,
            get_expanded_limit(sync_time, &current_limits.borrowable_token1),
            get_expanded_limit(sync_time, &current_limits.withdrawable_token1),
        )
    } else {
        (
            col_reserves.token1_real_reserves,
            col_reserves.token0_real_reserves,
            col_reserves.token1_imaginary_reserves,
            col_reserves.token0_imaginary_reserves,
            debt_reserves.token1_real_reserves,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_imaginary_reserves,
            debt_reserves.token0_imaginary_reserves,
            get_expanded_limit(sync_time, &current_limits.borrowable_token0),
            get_expanded_limit(sync_time, &current_limits.withdrawable_token0),
        )
    };

    let borrowable = to_adjusted_amount(borrowable, out_decimals);
    let withdrawable = to_adjusted_amount(withdrawable, out_decimals);

    let col_pool_enabled = col_reserves.token0_real_reserves > U256::ZERO &&
        col_reserves.token1_real_reserves > U256::ZERO &&
        col_reserves.token0_imaginary_reserves > U256::ZERO &&
        col_reserves.token1_imaginary_reserves > U256::ZERO;

    let debt_pool_enabled = debt_reserves.token0_real_reserves > U256::ZERO &&
        debt_reserves.token1_real_reserves > U256::ZERO &&
        debt_reserves.token0_imaginary_reserves > U256::ZERO &&
        debt_reserves.token1_imaginary_reserves > U256::ZERO;

    if !col_pool_enabled && !debt_pool_enabled {
        return Err(SwapError::NoPoolsEnabled);
    }

    let a = if col_pool_enabled && debt_pool_enabled {
        swap_routing_out(
            amount_to_receive,
            col_i_reserve_out,
            col_i_reserve_in,
            debt_i_reserve_out,
            debt_i_reserve_in,
        )
    } else if debt_pool_enabled {
        U256::MAX
    } else if col_pool_enabled {
        amount_to_receive + U256::ONE
    } else {
        return Err(SwapError::NoPoolsEnabled);
    };

    let mut trigger_update_debt_reserves = false;
    let mut trigger_update_col_reserves = false;

    let (amount_in_collateral, amount_out_collateral, amount_in_debt, amount_out_debt) =
        if a == U256::ZERO || a == U256::MAX {
            let amount_in_debt =
                get_amount_in(amount_to_receive, debt_i_reserve_in, debt_i_reserve_out);
            if amount_to_receive > debt_reserve_out {
                return Err(SwapError::InsufficientReserve);
            }

            trigger_update_debt_reserves = true;
            (U256::ZERO, U256::ZERO, amount_in_debt, amount_to_receive)
        } else if a >= amount_to_receive {
            let amount_in_collateral =
                get_amount_in(amount_to_receive, col_i_reserve_in, col_i_reserve_out);

            if amount_to_receive > col_reserve_out {
                return Err(SwapError::InsufficientReserve);
            }

            trigger_update_col_reserves = true;
            (amount_in_collateral, amount_to_receive, U256::ZERO, U256::ZERO)
        } else {
            let amount_out_collateral = a;
            let amount_in_collateral =
                get_amount_in(amount_out_collateral, col_i_reserve_in, col_i_reserve_out);
            let amount_out_debt = amount_to_receive - amount_out_collateral;
            let amount_in_debt =
                get_amount_in(amount_out_debt, debt_i_reserve_in, debt_i_reserve_out);

            if amount_out_debt > debt_reserve_out || amount_out_collateral > col_reserve_out {
                return Err(SwapError::InsufficientReserve);
            }

            (amount_in_collateral, amount_out_collateral, amount_in_debt, amount_out_debt)
        };

    if amount_in_debt > borrowable {
        return Err(SwapError::InsufficientBorrowable);
    }

    if amount_in_collateral > withdrawable {
        return Err(SwapError::InsufficientWithdrawable);
    }

    if amount_in_collateral > U256::ZERO {
        let reserves_ratio_valid = if swap0_to_1 {
            verify_token1_reserves(
                col_reserve_in + amount_in_collateral,
                col_reserve_out - amount_out_collateral,
                center_price,
            )
        } else {
            verify_token0_reserves(
                col_reserve_out - amount_out_collateral,
                col_reserve_in + amount_in_collateral,
                center_price,
            )
        };
        if !reserves_ratio_valid {
            return Err(SwapError::VerifyReservesRatiosInvalid);
        }
    }

    if amount_in_debt > U256::ZERO {
        let reserves_ratio_valid = if swap0_to_1 {
            verify_token1_reserves(
                debt_reserve_in + amount_in_debt,
                debt_reserve_out - amount_out_debt,
                center_price,
            )
        } else {
            verify_token0_reserves(
                debt_reserve_out - amount_out_debt,
                debt_reserve_in + amount_in_debt,
                center_price,
            )
        };
        if !reserves_ratio_valid {
            return Err(SwapError::VerifyReservesRatiosInvalid);
        }
    }

    let (old_price, new_price) = if amount_in_collateral > amount_in_debt {
        if swap0_to_1 {
            (
                col_i_reserve_out * constant::B_I1E27 / col_i_reserve_in,
                (col_i_reserve_out - amount_out_collateral) * constant::B_I1E27 /
                    (col_i_reserve_in + amount_in_collateral),
            )
        } else {
            (
                col_i_reserve_in * constant::B_I1E27 / col_i_reserve_out,
                (col_i_reserve_in + amount_in_collateral) * constant::B_I1E27 /
                    (col_i_reserve_out - amount_out_collateral),
            )
        }
    } else if swap0_to_1 {
        (
            debt_i_reserve_out * constant::B_I1E27 / debt_i_reserve_in,
            (debt_i_reserve_out - amount_out_debt) * constant::B_I1E27 /
                (debt_i_reserve_in + amount_in_debt),
        )
    } else {
        (
            debt_i_reserve_in * constant::B_I1E27 / debt_i_reserve_out,
            (debt_i_reserve_in + amount_in_debt) * constant::B_I1E27 /
                (debt_i_reserve_out - amount_out_debt),
        )
    };

    let price_diff = old_price.abs_diff(new_price);
    let max_price_diff = old_price * constant::MAX_PRICE_DIFF / constant::TWO_DECIMALS;

    if price_diff > max_price_diff {
        return Err(SwapError::InsufficientMaxPrice);
    }

    if trigger_update_col_reserves {
        update_collateral_reserves_and_limits(
            swap0_to_1,
            amount_in_collateral,
            amount_out_collateral,
            col_reserves,
            current_limits,
            out_decimals,
        );
    }

    if trigger_update_debt_reserves {
        update_debt_reserves_and_limits(
            swap0_to_1,
            amount_in_debt,
            amount_out_debt,
            debt_reserves,
            current_limits,
            out_decimals,
        );
    }

    Ok(from_adjusted_amount(amount_in_collateral + amount_in_debt, in_decimals))
}

/// Calculates how much of a swap should go through the collateral pool.
///
/// # Parameters
/// - `t`: Total amount in.
/// - `x`: Imaginary reserves of token out of collateral.
/// - `y`: Imaginary reserves of token in of collateral.
/// - `x2`: Imaginary reserves of token out of debt.
/// - `y2`: Imaginary reserves of token in of debt.
///
/// # Returns
/// - `a`: How much of the swap should go through the collateral pool. The remaining amount will go
///   through the debt pool.
///
/// # Notes
/// - If `a < 0`, the entire trade routes through the debt pool and debt pool arbitrages with
///   collateral pool.
/// - If `a > t`, the entire trade routes through the collateral pool and collateral pool arbitrages
///   with debt pool.
/// - If `a > 0 && a < t`, the swap will route through both pools.
fn swap_routing_in(t: U256, x: U256, y: U256, x2: U256, y2: U256) -> U256 {
    let xy_root = (x * y * constant::B_I1E18).root(2);
    let x2y2_root = (x2 * y2 * constant::B_I1E18).root(2);

    let numerator = y2 * xy_root + t * xy_root - y * x2y2_root;
    let denominator = xy_root + x2y2_root;
    numerator / denominator
}

/// Calculates how much of a swap should go through the collateral pool for an output amount.
///
/// # Notes
/// - If `a < 0` → entire trade goes through debt pool.
/// - If `a > t` → entire trade goes through collateral pool.
/// - If `0 < a < t` → swap routes through both pools.
#[allow(dead_code)]
fn swap_routing_out(t: U256, x: U256, y: U256, x2: U256, y2: U256) -> U256 {
    let xy_root = (x * y * constant::B_I1E18).root(2);
    let x2y2_root = (x2 * y2 * constant::B_I1E18).root(2);

    let numerator = t * xy_root + y * x2y2_root - y2 * xy_root;
    let denominator = xy_root + x2y2_root;

    numerator / denominator
}

fn get_amount_out(amount_in: U256, i_reserve_in: U256, i_reserve_out: U256) -> U256 {
    amount_in * i_reserve_out / (i_reserve_in + amount_in)
}

/// Given an output amount of asset and reserves, returns the input amount of the other asset.
///
/// Formula: (amount_out * iReserveIn) / (iReserveOut - amount_out)
#[allow(dead_code)]
fn get_amount_in(amount_out: U256, i_reserve_in: U256, i_reserve_out: U256) -> U256 {
    amount_out * i_reserve_in / (i_reserve_out - amount_out)
}

fn to_adjusted_amount(amount: U256, decimals: i64) -> U256 {
    let diff = decimals - constant::DEX_AMOUNT_DECIMALS;
    if diff == 0 {
        amount
    } else if diff > 0 {
        amount / ten_pow(diff)
    } else {
        amount * ten_pow(-diff)
    }
}

/// Converts an adjusted amount to the original precision by compensating for decimal differences.
///
/// # Arguments
/// * `adjusted_amount` - The amount adjusted to DexAmountsDecimals.
/// * `decimals` - The original token decimals.
/// * `dex_amounts_decimals` - The reference decimals used by DEX amounts.
///
/// # Returns
/// * The amount scaled back to the original decimals.
fn from_adjusted_amount(adjusted_amount: U256, decimals: i64) -> U256 {
    let diff = decimals - constant::DEX_AMOUNT_DECIMALS;

    if diff == 0 {
        adjusted_amount
    } else if diff < 0 {
        // Divide by 10^(-diff)
        let divisor = ten_pow(-diff);
        adjusted_amount / divisor
    } else {
        // Multiply by 10^(diff)
        let multiplier = ten_pow(diff);
        adjusted_amount * multiplier
    }
}

fn ten_pow(v: i64) -> U256 {
    U256::from(10u64).pow(U256::from((v) as u64))
}

/// Checks if token0 reserves are sufficient compared to token1 reserves.
///
/// This prevents reserve imbalance and ensures price calculations remain stable and precise.
///
/// # Arguments
/// * `token0_reserves` - Reserves of token0.
/// * `token1_reserves` - Reserves of token1.
/// * `price` - Current price used in the reserve validation.
///
/// # Returns
/// Returns `false` if token0 reserves are too low, `true` otherwise.
///
/// # Formula
/// ```text
/// token0_reserves >= (token1_reserves * 1e27) / (price * MIN_SWAP_LIQUIDITY)
/// ```
fn verify_token0_reserves(token0_reserves: U256, token1_reserves: U256, price: U256) -> bool {
    let numerator = token1_reserves.saturating_mul(constant::B_I1E27);
    let denominator = price.saturating_mul(constant::MIN_SWAP_LIQUIDITY);
    token0_reserves >=
        numerator
            .checked_div(denominator)
            .unwrap_or(U256::ZERO)
}

/// Checks if token1 reserves are sufficient compared to token0 reserves.
///
/// This prevents reserve imbalance and ensures price calculations remain stable and precise.
///
/// # Arguments
/// * `token0_reserves` - Reserves of token0.
/// * `token1_reserves` - Reserves of token1.
/// * `price` - Current price used in the reserve validation.
///
/// # Returns
/// `false` if token1 reserves are too low, `true` otherwise.
///
/// # Formula
/// ```text
/// token1_reserves >= (token0_reserves * price) / (1e27 * MIN_SWAP_LIQUIDITY)
/// ```
fn verify_token1_reserves(token0_reserves: U256, token1_reserves: U256, price: U256) -> bool {
    let numerator = token0_reserves.saturating_mul(price);
    let denominator = constant::B_I1E27.saturating_mul(constant::MIN_SWAP_LIQUIDITY);
    token1_reserves >= numerator.div_euclid(&denominator)
}

/// Calculates the currently available swappable amount for a token limit,
/// considering how much it has expanded since the last synchronization.
///
/// This models gradual limit recovery over time.
///
/// # Arguments
/// * `sync_time` — UNIX timestamp (in seconds) of the last synchronization.
/// * `limit` — The token limit definition.
///
/// # Returns
/// Returns the currently effective limit as a `U256`.
fn get_expanded_limit(sync_time: u64, limit: &TokenLimit) -> U256 {
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before UNIX_EPOCH")
        .as_secs();

    let elapsed_time = current_time.saturating_sub(sync_time);
    let elapsed = U256::from(elapsed_time);

    if elapsed_time < 10 {
        // If almost no time has elapsed, return available amount
        return limit.available;
    }

    if elapsed >= limit.expand_duration {
        // If full duration has passed, return max amount
        return limit.expands_to;
    }

    // Linear interpolation:
    // expanded = available + (expands_to - available) * elapsed / expand_duration
    let delta = limit
        .expands_to
        .saturating_sub(limit.available);
    limit
        .available
        .saturating_add(delta.saturating_mul(elapsed) / limit.expand_duration)
}

/// Returns updated copies of `CollateralReserves` and `DexLimits` based on swap direction.
///
/// # Note
/// Updates reserves and limits in-place.
fn update_collateral_reserves_and_limits(
    swap0_to_1: bool,
    amount_in: U256,
    amount_out: U256,
    col_reserves: &mut CollateralReserves,
    limits: &mut DexLimits,
    out_decimals: i64,
) {
    let unadjusted_amount_out = from_adjusted_amount(amount_out, out_decimals);

    if swap0_to_1 {
        // token0 → token1 swap
        col_reserves.token0_real_reserves = col_reserves
            .token0_real_reserves
            .saturating_add(amount_in);
        col_reserves.token0_imaginary_reserves = col_reserves
            .token0_imaginary_reserves
            .saturating_add(amount_in);
        col_reserves.token1_real_reserves = col_reserves
            .token1_real_reserves
            .saturating_sub(amount_out);
        col_reserves.token1_imaginary_reserves = col_reserves
            .token1_imaginary_reserves
            .saturating_sub(amount_out);

        limits.withdrawable_token1.available = limits
            .withdrawable_token1
            .available
            .saturating_sub(unadjusted_amount_out);
        limits.withdrawable_token1.expands_to = limits
            .withdrawable_token1
            .expands_to
            .saturating_sub(unadjusted_amount_out);
    } else {
        // token1 → token0 swap
        col_reserves.token0_real_reserves = col_reserves
            .token0_real_reserves
            .saturating_sub(amount_out);
        col_reserves.token0_imaginary_reserves = col_reserves
            .token0_imaginary_reserves
            .saturating_sub(amount_out);
        col_reserves.token1_real_reserves = col_reserves
            .token1_real_reserves
            .saturating_add(amount_in);
        col_reserves.token1_imaginary_reserves = col_reserves
            .token1_imaginary_reserves
            .saturating_add(amount_in);

        limits.withdrawable_token0.available = limits
            .withdrawable_token0
            .available
            .saturating_sub(unadjusted_amount_out);
        limits.withdrawable_token0.expands_to = limits
            .withdrawable_token0
            .expands_to
            .saturating_sub(unadjusted_amount_out);
    }
}

fn update_debt_reserves_and_limits(
    swap0_to1: bool,
    amount_in: U256,
    amount_out: U256,
    debt_reserves: &mut DebtReserves,
    limits: &mut DexLimits,
    out_decimals: i64,
) {
    let unadjusted_amount_out = from_adjusted_amount(amount_out, out_decimals);

    if swap0_to1 {
        debt_reserves.token0_real_reserves += amount_in;
        debt_reserves.token0_imaginary_reserves += amount_in;
        debt_reserves.token1_real_reserves -= amount_out;
        debt_reserves.token1_imaginary_reserves -= amount_out;

        // Comment Ref #4327563287
        // if expandTo for borrowable and withdrawable match, that means they are a hard limit like
        // liquidity layer balance or utilization limit. In that case, the available swap
        // amount should increase by `amountIn` but it's not guaranteed because the actual
        // borrow limit / withdrawal limit could be the limiting factor now, which could be even
        // only +1 bigger. So not updating in amount to avoid any revert. The same applies on all
        // other similar cases in the code below. Note a swap would anyway trigger an event,
        // so the proper limits will be fetched shortly after the swap.
        limits.borrowable_token1.available -= unadjusted_amount_out;
        limits.borrowable_token1.expands_to -= unadjusted_amount_out;
    } else {
        debt_reserves.token0_real_reserves -= amount_out;
        debt_reserves.token0_imaginary_reserves -= amount_out;
        debt_reserves.token1_real_reserves += amount_in;
        debt_reserves.token1_imaginary_reserves += amount_in;

        limits.borrowable_token0.available -= unadjusted_amount_out;
        limits.borrowable_token0.expands_to -= unadjusted_amount_out;
    }
}

fn get_max_reserves(
    decimals: u8,
    withdrawable_limit: &TokenLimit,
    borrowable_limit: &TokenLimit,
    real_col_reserves: &U256,
    real_debt_reserves: &U256,
) -> U256 {
    // Step 1: Determine maxLimitReserves
    let mut max_limit_reserves = borrowable_limit.expands_to;

    if borrowable_limit.expands_to != withdrawable_limit.expands_to {
        max_limit_reserves += withdrawable_limit.expands_to;
    }

    // Step 2: Calculate maxRealReserves
    let mut max_real_reserves = *real_col_reserves + *real_debt_reserves;

    if decimals > constant::DEX_AMOUNT_DECIMALS as u8 {
        let diff = decimals as i64 - constant::DEX_AMOUNT_DECIMALS;
        max_real_reserves *= ten_pow(diff);
    } else if decimals < constant::DEX_AMOUNT_DECIMALS as u8 {
        let diff = constant::DEX_AMOUNT_DECIMALS - decimals as i64;
        max_real_reserves /= ten_pow(diff);
    }

    // Step 3: Return the smaller of the two
    if max_real_reserves < max_limit_reserves {
        max_real_reserves
    } else {
        max_limit_reserves
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use alloy::primitives::I256;
    use anyhow::bail;
    use num_traits::{Num, ToPrimitive};
    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;

    fn setup_fluid_pool(center_price: U256) -> (Token, Token, FluidV1) {
        let wsteth = Token::new(
            &Bytes::from_str("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0").unwrap(),
            "wsteth",
            18,
            0,
            &[Some(20000)],
            Chain::Ethereum,
            100,
        );
        let eth = Token::new(
            &Bytes::from_str("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE").unwrap(),
            "ETH",
            18,
            0,
            &[Some(2000)],
            Chain::Ethereum,
            100,
        );

        let pool = FluidV1::new(
            &Bytes::from_str("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7").unwrap(),
            &wsteth,
            &eth,
            CollateralReserves {
                token0_real_reserves: U256::from_str("2169934539358").unwrap(),
                token1_real_reserves: U256::from_str("19563846299171").unwrap(),
                token0_imaginary_reserves: U256::from_str("62490032619260838").unwrap(),
                token1_imaginary_reserves: U256::from_str("73741038977020279").unwrap(),
            },
            DebtReserves {
                token0_real_reserves: U256::from_str("2169108220421").unwrap(),
                token1_real_reserves: U256::from_str("19572550738602").unwrap(),
                token0_imaginary_reserves: U256::from_str("62511862774117387").unwrap(),
                token1_imaginary_reserves: U256::from_str("73766803277429176").unwrap(),
            },
            limits_wide(),
            center_price,
            U256::from_str("100").unwrap(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() -
                10,
        );
        (wsteth, eth, pool)
    }

    fn limits_wide() -> DexLimits {
        let limit_wide = U256::from_str("34242332879776515083099999").unwrap();
        DexLimits {
            withdrawable_token0: TokenLimit {
                available: limit_wide,
                expands_to: limit_wide,
                expand_duration: U256::ZERO,
            },
            withdrawable_token1: TokenLimit {
                available: limit_wide,
                expands_to: limit_wide,
                expand_duration: U256::from(22),
            },
            borrowable_token0: TokenLimit {
                available: limit_wide,
                expands_to: limit_wide,
                expand_duration: U256::ZERO,
            },
            borrowable_token1: TokenLimit {
                available: limit_wide,
                expands_to: limit_wide,
                expand_duration: U256::from(22),
            },
        }
    }

    fn limits_tight() -> DexLimits {
        let limit_expand_tight = U256::from_str("711907234052361388866").unwrap();

        DexLimits {
            withdrawable_token0: TokenLimit {
                available: U256::from_str("456740438880263").unwrap(),
                expands_to: limit_expand_tight,
                expand_duration: U256::from(600),
            },
            withdrawable_token1: TokenLimit {
                available: U256::from_str("825179383432029").unwrap(),
                expands_to: limit_expand_tight,
                expand_duration: U256::from(600),
            },
            borrowable_token0: TokenLimit {
                available: U256::from_str("941825058374170").unwrap(),
                expands_to: limit_expand_tight,
                expand_duration: U256::from(600),
            },
            borrowable_token1: TokenLimit {
                available: U256::from_str("941825058374170").unwrap(),
                expands_to: limit_expand_tight,
                expand_duration: U256::from(600),
            },
        }
    }
    fn new_col_reserves_one() -> CollateralReserves {
        CollateralReserves {
            token0_real_reserves: U256::from_str("20000000006000000").unwrap(),
            token1_real_reserves: U256::from_str("20000000000500000").unwrap(),
            token0_imaginary_reserves: U256::from_str("389736659726997981").unwrap(),
            token1_imaginary_reserves: U256::from_str("389736659619871949").unwrap(),
        }
    }

    fn new_col_reserves_empty() -> CollateralReserves {
        CollateralReserves {
            token0_real_reserves: U256::ZERO,
            token1_real_reserves: U256::ZERO,
            token0_imaginary_reserves: U256::ZERO,
            token1_imaginary_reserves: U256::ZERO,
        }
    }

    fn new_debt_reserves_empty() -> DebtReserves {
        DebtReserves {
            token0_real_reserves: U256::ZERO,
            token1_real_reserves: U256::ZERO,
            token0_imaginary_reserves: U256::ZERO,
            token1_imaginary_reserves: U256::ZERO,
        }
    }

    fn new_debt_reserves_one() -> DebtReserves {
        DebtReserves {
            token0_real_reserves: U256::from_str("9486832995556050").unwrap(),
            token1_real_reserves: U256::from_str("9486832993079885").unwrap(),
            token0_imaginary_reserves: U256::from_str("184868330099560759").unwrap(),
            token1_imaginary_reserves: U256::from_str("184868330048879109").unwrap(),
        }
    }

    pub fn get_approx_center_price_in(
        amount_to_swap: U256,
        swap0_to_1: bool,
        col_reserves: &CollateralReserves,
        debt_reserves: &DebtReserves,
    ) -> Result<U256, anyhow::Error> {
        let col_pool_enabled = !col_reserves
            .token0_real_reserves
            .is_zero() &&
            !col_reserves
                .token1_real_reserves
                .is_zero() &&
            !col_reserves
                .token0_imaginary_reserves
                .is_zero() &&
            !col_reserves
                .token1_imaginary_reserves
                .is_zero();

        let debt_pool_enabled = !debt_reserves
            .token0_real_reserves
            .is_zero() &&
            !debt_reserves
                .token1_real_reserves
                .is_zero() &&
            !debt_reserves
                .token0_imaginary_reserves
                .is_zero() &&
            !debt_reserves
                .token1_imaginary_reserves
                .is_zero();

        let (col_i_reserve_in, col_i_reserve_out, debt_i_reserve_in, debt_i_reserve_out) =
            if swap0_to_1 {
                (
                    col_reserves.token0_imaginary_reserves,
                    col_reserves.token1_imaginary_reserves,
                    debt_reserves.token0_imaginary_reserves,
                    debt_reserves.token1_imaginary_reserves,
                )
            } else {
                (
                    col_reserves.token1_imaginary_reserves,
                    col_reserves.token0_imaginary_reserves,
                    debt_reserves.token1_imaginary_reserves,
                    debt_reserves.token0_imaginary_reserves,
                )
            };

        let a = if col_pool_enabled && debt_pool_enabled {
            swap_routing_in(
                amount_to_swap,
                col_i_reserve_out,
                col_i_reserve_in,
                debt_i_reserve_out,
                debt_i_reserve_in,
            )
        } else if debt_pool_enabled {
            U256::MAX // equivalent to -1 in Go logic for error handling
        } else if col_pool_enabled {
            amount_to_swap
                .checked_add(U256::from(1))
                .unwrap()
        } else {
            bail!("No pools are enabled");
        };

        let (amount_in_collateral, amount_in_debt) = if a == U256::MAX || a == U256::ZERO {
            (U256::ZERO, amount_to_swap)
        } else if a >= amount_to_swap {
            (amount_to_swap, U256::ZERO)
        } else {
            (a, amount_to_swap - a)
        };

        let price = if amount_in_collateral > amount_in_debt {
            if swap0_to_1 {
                col_i_reserve_out
                    .checked_mul(constant::B_I1E27)
                    .unwrap() /
                    col_i_reserve_in
            } else {
                col_i_reserve_in
                    .checked_mul(constant::B_I1E27)
                    .unwrap() /
                    col_i_reserve_out
            }
        } else if swap0_to_1 {
            debt_i_reserve_out
                .checked_mul(constant::B_I1E27)
                .unwrap() /
                debt_i_reserve_in
        } else {
            debt_i_reserve_in
                .checked_mul(constant::B_I1E27)
                .unwrap() /
                debt_i_reserve_out
        };

        Ok(price)
    }

    pub fn get_approx_center_price_out(
        amount_out: U256,
        swap0_to_1: bool,
        col_reserves: &CollateralReserves,
        debt_reserves: &DebtReserves,
    ) -> Result<U256, SwapError> {
        let col_pool_enabled = col_reserves.token0_real_reserves > U256::ZERO &&
            col_reserves.token1_real_reserves > U256::ZERO &&
            col_reserves.token0_imaginary_reserves > U256::ZERO &&
            col_reserves.token1_imaginary_reserves > U256::ZERO;

        let debt_pool_enabled = debt_reserves.token0_real_reserves > U256::ZERO &&
            debt_reserves.token1_real_reserves > U256::ZERO &&
            debt_reserves.token0_imaginary_reserves > U256::ZERO &&
            debt_reserves.token1_imaginary_reserves > U256::ZERO;

        let (col_i_reserve_in, col_i_reserve_out, debt_i_reserve_in, debt_i_reserve_out) =
            if swap0_to_1 {
                (
                    col_reserves.token0_imaginary_reserves,
                    col_reserves.token1_imaginary_reserves,
                    debt_reserves.token0_imaginary_reserves,
                    debt_reserves.token1_imaginary_reserves,
                )
            } else {
                (
                    col_reserves.token1_imaginary_reserves,
                    col_reserves.token0_imaginary_reserves,
                    debt_reserves.token1_imaginary_reserves,
                    debt_reserves.token0_imaginary_reserves,
                )
            };

        let a = if col_pool_enabled && debt_pool_enabled {
            swap_routing_out(
                amount_out,
                col_i_reserve_in,
                col_i_reserve_out,
                debt_i_reserve_in,
                debt_i_reserve_out,
            )
        } else if debt_pool_enabled {
            U256::MAX // Special case: Route entirely from debt pool
        } else if col_pool_enabled {
            amount_out + U256::ONE // Special case: Route entirely from collateral pool
        } else {
            return Err(SwapError::NoPoolsEnabled);
        };

        let mut amount_in_collateral = U256::ZERO;
        let mut amount_in_debt = U256::ZERO;

        if a <= U256::ZERO {
            amount_in_debt = get_amount_in(amount_out, debt_i_reserve_in, debt_i_reserve_out);
        } else if a >= amount_out {
            amount_in_collateral = get_amount_in(amount_out, col_i_reserve_in, col_i_reserve_out);
        } else {
            amount_in_collateral = get_amount_in(a, col_i_reserve_in, col_i_reserve_out);
            amount_in_debt = get_amount_in(amount_out - a, debt_i_reserve_in, debt_i_reserve_out);
        }

        let price = if amount_in_collateral > amount_in_debt {
            if swap0_to_1 {
                col_i_reserve_out * constant::B_I1E27 / col_i_reserve_in
            } else {
                col_i_reserve_in * constant::B_I1E27 / col_i_reserve_out
            }
        } else if swap0_to_1 {
            debt_i_reserve_out * constant::B_I1E27 / debt_i_reserve_in
        } else {
            debt_i_reserve_in * constant::B_I1E27 / debt_i_reserve_out
        };

        Ok(price)
    }

    #[test]
    fn test_calc_amount_out_zero2one() {
        let (wsteth, eth, pool) = setup_fluid_pool(U256::ONE);
        let cases = [
            ("1000000000000000000", "1179917402128000000"),
            ("500000000000000000", "589961060629000000"),
        ];
        for (amount_in_str, exp_out_str) in cases.into_iter() {
            let exp_out = BigUint::from_str_radix(exp_out_str, 10).unwrap();
            let res = pool
                .get_amount_out(BigUint::from_str_radix(amount_in_str, 10).unwrap(), &wsteth, &eth)
                .unwrap();

            assert_eq!(res.amount, exp_out);
        }
    }

    #[test]
    fn test_calc_amount_out_one2zero() {
        let center_price = U256::from_str("1200000000000000000000000000").unwrap();
        let (wsteth, eth, pool) = setup_fluid_pool(center_price);
        let cases = [("800000000000000000", "677868867152000000")];
        for (amount_in_str, exp_out_str) in cases.into_iter() {
            let exp_out = BigUint::from_str_radix(exp_out_str, 10).unwrap();
            let res = pool
                .get_amount_out(BigUint::from_str_radix(amount_in_str, 10).unwrap(), &eth, &wsteth)
                .unwrap();

            assert_eq!(res.amount, exp_out);
        }
    }

    fn target_price(spot: f64, multiplier: f64) -> Price {
        // Both fixture tokens use 18 decimals, so no decimal adjustment is needed.
        Price::new(BigUint::from((spot * multiplier * 1e18) as u128), BigUint::from(10u128.pow(18)))
    }

    fn target_price_params(
        token_in: &Token,
        token_out: &Token,
        target: Price,
    ) -> QueryPoolSwapParams {
        QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: 0.001,
                min_amount_in: None,
                max_amount_in: None,
            },
        )
    }

    // The fixture mirrors on-chain data where real reserves (~2 wstETH / ~19.5 ETH) are far
    // smaller than the imaginary reserves (~62k tokens), so only price moves of a few bps are
    // reachable before the output exceeds the withdrawable real reserves.
    #[rstest]
    #[case::zero2one_half_bp(true, 0.99995)]
    #[case::zero2one_2bps(true, 0.9998)]
    #[case::zero2one_4bps(true, 0.9996)]
    // One2zero output is wstETH, whose real reserves (~2.17 tokens) cap the move below ~0.7bp.
    #[case::one2zero_half_bp(false, 0.99995)]
    #[case::one2zero_06_bp(false, 0.99994)]
    fn test_swap_to_price_matches_numerical(#[case] zero2one: bool, #[case] multiplier: f64) {
        let center_price = if zero2one {
            U256::ONE
        } else {
            U256::from_str("1200000000000000000000000000").unwrap()
        };
        let (wsteth, eth, pool) = setup_fluid_pool(center_price);
        let (token_in, token_out) = if zero2one { (wsteth, eth) } else { (eth, wsteth) };

        let spot = pool
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target_f64 = spot * multiplier;
        let params = target_price_params(&token_in, &token_out, target_price(spot, multiplier));

        let native = pool.query_pool_swap(&params).unwrap();
        let numerical = crate::evm::query_pool_swap::query_pool_swap(&pool, &params).unwrap();

        // The numerical path always records price points; their absence proves the closed form
        // answered (no fallback).
        assert!(native.price_points().is_none(), "closed form should not have fallen back");

        let native_spot = native
            .new_state()
            .spot_price(&token_in, &token_out)
            .unwrap();
        let error = ((native_spot - target_f64) / target_f64).abs();
        assert!(
            error <= SWAP_TO_PRICE_MAX_DEVIATION,
            "native spot {native_spot} deviates {error} from target {target_f64}"
        );

        let native_in = native.amount_in().to_f64().unwrap();
        let numerical_in = numerical.amount_in().to_f64().unwrap();
        let relative_diff = ((native_in - numerical_in) / numerical_in).abs();
        assert!(
            relative_diff < 0.02,
            "native amount_in {native_in} vs numerical {numerical_in} differ by {relative_diff}"
        );
        assert!(native.amount_out() > &BigUint::ZERO);
    }

    #[test]
    fn test_swap_to_price_debt_pool_only() {
        let (wsteth, eth, mut pool) = setup_fluid_pool(U256::ONE);
        pool.collateral_reserves = new_col_reserves_empty();

        let spot = pool.spot_price(&wsteth, &eth).unwrap();
        let target_f64 = spot * 0.9998;
        let params = target_price_params(&wsteth, &eth, target_price(spot, 0.9998));

        let native = pool.query_pool_swap(&params).unwrap();
        assert!(native.price_points().is_none(), "closed form should not have fallen back");
        let native_spot = native
            .new_state()
            .spot_price(&wsteth, &eth)
            .unwrap();
        let error = ((native_spot - target_f64) / target_f64).abs();
        assert!(error <= SWAP_TO_PRICE_MAX_DEVIATION);
    }

    #[test]
    fn test_swap_to_price_target_above_spot_errors() {
        let (wsteth, eth, pool) = setup_fluid_pool(U256::ONE);
        let spot = pool.spot_price(&wsteth, &eth).unwrap();
        let params = target_price_params(&wsteth, &eth, target_price(spot, 1.01));

        let result = pool.query_pool_swap(&params);
        assert!(result.is_err(), "target above spot must be rejected");
    }

    #[test]
    fn test_swap_to_price_beyond_max_price_move_errors() {
        let (wsteth, eth, pool) = setup_fluid_pool(U256::ONE);
        let spot = pool.spot_price(&wsteth, &eth).unwrap();
        // Fluid rejects single swaps that move the marginal price by more than 5%.
        let params = target_price_params(&wsteth, &eth, target_price(spot, 0.90));

        let result = pool.query_pool_swap(&params);
        assert!(result.is_err(), "target beyond the 5% price move cap must be rejected");
    }

    #[test]
    fn test_swap_to_price_at_spot_is_noop() {
        // Use a single sub-pool: with both pools enabled their marginal prices differ by a few
        // hundredths of a bp, so a target at spot still implies a small equalizing swap.
        let (wsteth, eth, mut pool) = setup_fluid_pool(U256::ONE);
        pool.collateral_reserves = new_col_reserves_empty();
        let spot = pool.spot_price(&wsteth, &eth).unwrap();
        let params = target_price_params(&wsteth, &eth, target_price(spot, 1.0 - 1e-12));

        let native = pool.query_pool_swap(&params).unwrap();
        assert_eq!(native.amount_in(), &BigUint::ZERO);
        assert_eq!(native.amount_out(), &BigUint::ZERO);
    }

    #[test]
    fn test_amount_out_exceeds_reserve() {
        let (wsteth, eth, mut pool) = setup_fluid_pool(U256::ONE);
        // set custom reserves to trigger the error
        pool.pool_reserve0 = U256::from_str("18760613183894").unwrap();
        pool.pool_reserve1 = U256::from_str("22123580158026").unwrap();
        let amount_in = BigUint::from_str_radix("30000000000000000000", 10).unwrap(); // 300 wstETH
        let result = pool.get_amount_out(amount_in, &wsteth, &eth);

        assert!(result.is_err(), "Expected an error for exceeding reserves");
        assert_eq!(
            result.unwrap_err().to_string(),
            SimulationError::from(SwapError::InsufficientReserve).to_string()
        );
    }

    #[test]
    fn test_swap_in() {
        let sync_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_swap_in_result(
            true,
            U256::from(1_000_000_000_000_000u128), // 1e15
            new_col_reserves_one(),
            new_debt_reserves_one(),
            "998262697204710000000",
            12,
            18,
            limits_wide(),
            sync_time - 10,
        );

        assert_swap_in_result(
            true,
            U256::from(1_000_000_000_000_000u128),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "994619847016724000000",
            12,
            18,
            limits_wide(),
            sync_time - 10,
        );

        assert_swap_in_result(
            true,
            U256::from(1_000_000_000_000_000u128),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "997440731289905000000",
            12,
            18,
            limits_wide(),
            sync_time - 10,
        );

        assert_swap_in_result(
            false,
            U256::from(1_000_000_000_000_000u128),
            new_col_reserves_one(),
            new_debt_reserves_one(),
            "998262697752553000000",
            12,
            18,
            limits_wide(),
            sync_time - 10,
        );

        assert_swap_in_result(
            false,
            U256::from(1_000_000_000_000_000u128),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "994619847560607000000",
            12,
            18,
            limits_wide(),
            sync_time - 10,
        );

        assert_swap_in_result(
            false,
            U256::from(1_000_000_000_000_000u128),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "997440731837532000000",
            12,
            18,
            limits_wide(),
            sync_time - 10,
        );
    }

    /// Asserts that a swap produces the expected output amount.
    ///
    /// # Arguments
    /// - `swap0_to_1`: Direction of the swap.
    /// - `amount_in`: Total amount in.
    /// - `col_reserves`: Collateral reserves.
    /// - `debt_reserves`: Debt reserves.
    /// - `expected_amount_out`: Expected output amount as a string.
    /// - `in_decimals`: Decimals for the input token.
    /// - `out_decimals`: Decimals for the output token.
    /// - `limits`: Dex limits.
    /// - `sync_time`: Timestamp for syncing.
    #[allow(clippy::too_many_arguments)]
    fn assert_swap_in_result(
        swap0_to_1: bool,
        amount_in: U256,
        mut col_reserves: CollateralReserves,
        mut debt_reserves: DebtReserves,
        expected_amount_out: &str,
        in_decimals: i64,
        out_decimals: i64,
        mut limits: DexLimits,
        sync_time: u64,
    ) {
        let price =
            get_approx_center_price_in(amount_in, swap0_to_1, &col_reserves, &debt_reserves)
                .expect("Failed to get approx center price");

        let adjusted_amount_in = to_adjusted_amount(amount_in, in_decimals);
        let out_amt = swap_in_adjusted(
            swap0_to_1,
            adjusted_amount_in,
            &mut col_reserves,
            &mut debt_reserves,
            out_decimals,
            &mut limits,
            price,
            sync_time,
        )
        .expect("Failed to calculate swap in adjusted");

        assert_eq!(expected_amount_out, out_amt.to_string(), "Amount out mismatch");
    }

    #[allow(clippy::too_many_arguments)]
    fn assert_swap_out_result(
        swap0_to_1: bool,
        amount_out: U256,
        mut col_reserves: CollateralReserves,
        mut debt_reserves: DebtReserves,
        expected_amount_in: &str,
        in_decimals: i64,
        out_decimals: i64,
        mut limits: DexLimits,
        sync_time: i64,
    ) {
        let price =
            get_approx_center_price_out(amount_out, swap0_to_1, &col_reserves, &debt_reserves)
                .expect("failed to get approx center price");

        let in_amt = swap_out_adjusted(
            swap0_to_1,
            to_adjusted_amount(amount_out, out_decimals),
            &mut col_reserves,
            &mut debt_reserves,
            in_decimals,
            out_decimals,
            &mut limits,
            price,
            sync_time as u64,
        )
        .expect("swap_out_adjusted failed");

        assert_eq!(expected_amount_in, from_adjusted_amount(in_amt, in_decimals).to_string());
    }

    #[test]
    fn test_swap_in_limits() {
        let sync_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // when limits hit
        let price = get_approx_center_price_in(
            U256::from(1_000_000_000_000_000u128),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let res = swap_in_adjusted(
            true,
            U256::from(1_000_000_000_000_000u128),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            18,
            &mut limits_tight(),
            price,
            sync_time - 10,
        );

        assert_eq!(res.unwrap_err().to_string(), SwapError::InsufficientBorrowable.to_string());

        // when expanded
        let price = get_approx_center_price_out(
            U256::from(1_000_000_000_000_000u128),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let out_amt = swap_in_adjusted(
            true,
            U256::from(1_000_000_000_000_000u128),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            18,
            &mut limits_tight(),
            price,
            sync_time - 6000,
        )
        .unwrap();

        assert_eq!(out_amt.to_string(), "998262697204710000000");

        // when price diff hit
        let price = get_approx_center_price_out(
            U256::from(30_000_000_000_000_000u128),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let res = swap_in_adjusted(
            true,
            U256::from(30_000_000_000_000_000u128),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            18,
            &mut limits_wide(),
            price,
            sync_time - 10,
        );

        assert_eq!(res.unwrap_err().to_string(), SwapError::InsufficientMaxPrice.to_string());

        // when reserves limit is hit
        let price = get_approx_center_price_out(
            U256::from(50_000_000_000_000_000u128),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let res = swap_in_adjusted(
            true,
            U256::from(50_000_000_000_000_000u128),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            18,
            &mut limits_wide(),
            price,
            sync_time - 10,
        );

        assert_eq!(res.unwrap_err().to_string(), SwapError::InsufficientReserve.to_string());
    }

    #[test]
    fn test_swap_in_adjusted_compare_estimate_in() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expected_amount_out = U256::from_str("1180035404724000000").unwrap();
        let mut col_reserves = CollateralReserves {
            token0_real_reserves: U256::from_str("2169934539358").unwrap(),
            token1_real_reserves: U256::from_str("19563846299171").unwrap(),
            token0_imaginary_reserves: U256::from_str("62490032619260838").unwrap(),
            token1_imaginary_reserves: U256::from_str("73741038977020279").unwrap(),
        };
        let mut debt_reserves = DebtReserves {
            token0_real_reserves: U256::from_str("2169108220421").unwrap(),
            token1_real_reserves: U256::from_str("19572550738602").unwrap(),
            token0_imaginary_reserves: U256::from_str("62511862774117387").unwrap(),
            token1_imaginary_reserves: U256::from_str("73766803277429176").unwrap(),
        };
        let amount_in = U256::from(1000000000000u128); // 1e12
        let price = get_approx_center_price_in(amount_in, true, &col_reserves, &debt_reserves)
            .expect("Failed to get approximate center price");

        let out_amt = swap_in_adjusted(
            true,
            amount_in,
            &mut col_reserves,
            &mut debt_reserves,
            18,
            &mut limits_wide(),
            price,
            now - 10,
        )
        .expect("Failed to swap in adjusted");

        assert_eq!(expected_amount_out, out_amt);
    }

    #[test]
    fn test_swap_in_debt_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_swap_in_result(
            true,
            U256::from_str("1000000000000000").unwrap(),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "994619847016724",
            12,
            12,
            limits_wide(),
            now - 10,
        );

        assert_swap_in_result(
            false,
            U256::from_str("1000000000000000").unwrap(),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "994619847560607",
            12,
            12,
            limits_wide(),
            now - 10,
        )
    }

    #[test]
    fn test_swap_in_col_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_swap_in_result(
            true,
            U256::from_str("1000000000000000").unwrap(),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "997440731289905",
            12,
            12,
            limits_wide(),
            now - 10,
        );

        assert_swap_in_result(
            false,
            U256::from_str("1000000000000000").unwrap(),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "997440731837532",
            12,
            12,
            limits_wide(),
            now - 10,
        )
    }

    #[test]
    fn test_swap_out() {
        let sync_time = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64) -
            10;

        assert_swap_out_result(
            true,
            U256::from(1_000_000_000_000_000u64),
            new_col_reserves_one(),
            new_debt_reserves_one(),
            "1001743360284199",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        assert_swap_out_result(
            true,
            U256::from(1_000_000_000_000_000u64),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "1005438674786548",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        assert_swap_out_result(
            true,
            U256::from(1_000_000_000_000_000u64),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "1002572435818386",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        assert_swap_out_result(
            false,
            U256::from(1_000_000_000_000_000u64),
            new_col_reserves_one(),
            new_debt_reserves_one(),
            "1001743359733488",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        assert_swap_out_result(
            false,
            U256::from(1_000_000_000_000_000u64),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "1005438674233767",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        assert_swap_out_result(
            false,
            U256::from(1_000_000_000_000_000u64),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "1002572435266527",
            12,
            12,
            limits_wide(),
            sync_time,
        );
    }

    #[test]
    fn test_swap_out_limits() {
        let sync_time_recent = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()) -
            10;

        let sync_time_expanded = sync_time_recent - 5990; // ~6000 seconds earlier

        // --- when limits hit ---
        let price = get_approx_center_price_out(
            U256::from(1_000_000_000_000_000u64),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let result = swap_out_adjusted(
            true,
            U256::from(1_000_000_000_000_000u64),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            12,
            18,
            &mut limits_tight(),
            price,
            sync_time_recent,
        );

        assert!(matches!(result, Err(SwapError::InsufficientBorrowable)));

        // --- when expanded ---
        let price = get_approx_center_price_out(
            U256::from(1_000_000_000_000_000u64),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let result = swap_out_adjusted(
            true,
            U256::from(1_000_000_000_000_000u64),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            12,
            18,
            &mut limits_tight(),
            price,
            sync_time_expanded,
        )
        .unwrap();

        assert_eq!(from_adjusted_amount(result, 12).to_string(), "1001743360284199");

        // --- when price diff hit ---
        let price = get_approx_center_price_out(
            U256::from(20_000_000_000_000_000u64),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let result = swap_out_adjusted(
            true,
            U256::from(20_000_000_000_000_000u64),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            12,
            18,
            &mut limits_wide(),
            price,
            sync_time_recent,
        );

        assert!(matches!(result, Err(SwapError::InsufficientMaxPrice)));

        // --- when reserves limit is hit ---
        let price = get_approx_center_price_out(
            U256::from(30_000_000_000_000_000u64),
            true,
            &new_col_reserves_one(),
            &new_debt_reserves_one(),
        )
        .unwrap();

        let result = swap_out_adjusted(
            true,
            U256::from(30_000_000_000_000_000u64),
            &mut new_col_reserves_one(),
            &mut new_debt_reserves_one(),
            12,
            18,
            &mut limits_wide(),
            price,
            sync_time_recent,
        );

        assert!(matches!(result, Err(SwapError::InsufficientReserve)));
    }

    #[test]
    fn test_swap_out_empty_debt() {
        let sync_time = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64) -
            10;

        // swap0To1 = true
        assert_swap_out_result(
            true,
            U256::from(994_619_847_016_724u64),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "999999999999999",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        // swap0To1 = false
        assert_swap_out_result(
            false,
            U256::from(994_619_847_560_607u64),
            new_col_reserves_empty(),
            new_debt_reserves_one(),
            "999999999999999",
            12,
            12,
            limits_wide(),
            sync_time,
        );
    }

    #[test]
    fn test_swap_out_empty_collateral() {
        let sync_time = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64) -
            10;

        // swap0To1 = true
        assert_swap_out_result(
            true,
            U256::from(997_440_731_289_905u64),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "999999999999999",
            12,
            12,
            limits_wide(),
            sync_time,
        );

        // swap0To1 = false
        assert_swap_out_result(
            false,
            U256::from(997_440_731_837_532u64),
            new_col_reserves_one(),
            new_debt_reserves_empty(),
            "999999999999999",
            12,
            12,
            limits_wide(),
            sync_time,
        );
    }

    pub fn new_verify_ratio_col_reserves() -> CollateralReserves {
        CollateralReserves {
            token0_real_reserves: U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(12)),
            token1_real_reserves: U256::from(15_000u64) * U256::from(10u64).pow(U256::from(12)),
            token0_imaginary_reserves: U256::ZERO,
            token1_imaginary_reserves: U256::ZERO,
        }
    }

    pub fn new_verify_ratio_debt_reserves() -> DebtReserves {
        DebtReserves {
            token0_real_reserves: U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(12)),
            token1_real_reserves: U256::from(15_000u64) * U256::from(10u64).pow(U256::from(12)),
            token0_imaginary_reserves: U256::ZERO,
            token1_imaginary_reserves: U256::ZERO,
        }
    }

    /// Calculate reserves outside a price range
    pub fn calculate_reserves_outside_range(
        geometric_mean_price: U256,
        price_at_range: U256,
        reserve_x: U256,
        reserve_y: U256,
    ) -> (I256, I256) {
        let geometric_mean_price = I256::from(geometric_mean_price);
        let price_at_range = I256::from(price_at_range);
        let reserve_x = I256::from(reserve_x);
        let reserve_y = I256::from(reserve_y);

        let one_e27 = I256::from(constant::B_I1E27);
        let two = I256::try_from(2i8).unwrap();

        // part1 = priceAtRange - geometricMeanPrice
        let part1 = price_at_range
            .checked_sub(geometric_mean_price)
            .expect("priceAtRange must be >= geometricMeanPrice");

        // part2 = (geometricMeanPrice * reserveX + reserveY * 1e27) / (2 * part1)
        let part2 = geometric_mean_price
            .checked_mul(reserve_x)
            .unwrap()
            .checked_add(reserve_y.checked_mul(one_e27).unwrap())
            .unwrap()
            .checked_div(two.checked_mul(part1).unwrap())
            .unwrap();

        // part3 = reserveX * reserveY
        let mut part3 = reserve_x
            .checked_mul(reserve_y)
            .unwrap();

        let one_e50 = I256::try_from(10)
            .unwrap()
            .pow(U256::from(50));

        // Handle overflow
        if part3 < one_e50 {
            part3 = part3
                .checked_mul(one_e27)
                .unwrap()
                .checked_div(part1)
                .unwrap();
        } else {
            part3 = part3
                .checked_div(part1)
                .unwrap()
                .checked_mul(one_e27)
                .unwrap();
        }

        // reserveXOutside = part2 + sqrt(part3 + part2^2)
        let part2_squared = part2.checked_mul(part2).unwrap();
        let inside_sqrt = part3
            .checked_add(part2_squared)
            .unwrap();
        let sqrt_value = I256::from(
            U256::try_from(inside_sqrt)
                .unwrap()
                .root(2),
        );

        let reserve_x_outside = part2.checked_add(sqrt_value).unwrap();

        // reserveYOutside = (reserveXOutside * geometricMeanPrice) / 1e27
        let reserve_y_outside = reserve_x_outside
            .checked_mul(geometric_mean_price)
            .unwrap()
            .checked_div(one_e27)
            .unwrap();

        (reserve_x_outside, reserve_y_outside)
    }

    #[test]
    fn test_swap_in_verify_reserves_in_range() {
        let decimals: i64 = 6;
        let mut col_reserves = new_verify_ratio_col_reserves();
        let mut debt_reserves = new_verify_ratio_debt_reserves();

        let mut price = U256::from_str("1000001000000000000000000000").unwrap();

        // Calculate imaginary reserves for colReserves
        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            col_reserves.token0_real_reserves,
            col_reserves.token1_real_reserves,
        );

        col_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(col_reserves.token0_real_reserves));
        col_reserves.token1_imaginary_reserves = U256::from(
            I256::from(reserve_y_outside) + I256::from(col_reserves.token1_real_reserves),
        );

        // Calculate imaginary reserves for debtReserves
        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_real_reserves,
        );

        debt_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(debt_reserves.token0_real_reserves));
        debt_reserves.token1_imaginary_reserves = U256::from(
            I256::from(reserve_y_outside) + I256::from(debt_reserves.token1_real_reserves),
        );

        let sync_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() -
            10;

        // --- Case: Swap amount triggers revert (14_905)
        let swap_amount = U256::from(14_905) * U256::from(10).pow(U256::from(12)); // decimals factor
        price = get_approx_center_price_in(
            swap_amount,
            true,
            &col_reserves,
            &new_debt_reserves_empty(),
        )
        .unwrap();
        let result = swap_in_adjusted(
            true,
            swap_amount,
            &mut col_reserves,
            &mut new_debt_reserves_empty(),
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(
            result.is_err(),
            "FAIL: reserves ratio revert NOT hit for col reserves when swap amount 14_905"
        );

        price = get_approx_center_price_in(
            swap_amount,
            true,
            &new_col_reserves_empty(),
            &debt_reserves,
        )
        .unwrap();
        let result = swap_in_adjusted(
            true,
            swap_amount,
            &mut new_col_reserves_empty(),
            &mut debt_reserves,
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(
            result.is_err(),
            "FAIL: reserves ratio revert NOT hit for debt reserves when swap amount 14_905"
        );

        // --- Refresh reserves
        col_reserves = new_verify_ratio_col_reserves();
        debt_reserves = new_verify_ratio_debt_reserves();

        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            col_reserves.token0_real_reserves,
            col_reserves.token1_real_reserves,
        );

        col_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(col_reserves.token0_real_reserves));
        col_reserves.token1_imaginary_reserves = U256::from(
            I256::from(reserve_y_outside) + I256::from(col_reserves.token1_real_reserves),
        );

        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            // The test relies on this price value, obtained by the previous failing calls setup
            //  note that this value is < B_I1E27 so the returned reserves here will be negative
            //  it's unclear if this is expected by the Kyberswap implementation but it seems
            //  more like the value 14_895 was found with this unwanted side effect in place.
            price,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_real_reserves,
        );
        debt_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(debt_reserves.token0_real_reserves));
        debt_reserves.token1_imaginary_reserves = U256::from(
            I256::from(reserve_y_outside) + I256::from(debt_reserves.token1_real_reserves),
        );

        // --- Case: Swap amount should succeed (14_895)
        let swap_amount = U256::from(14_895) * U256::from(10).pow(U256::from(12));

        price = get_approx_center_price_in(
            swap_amount,
            true,
            &col_reserves,
            &new_debt_reserves_empty(),
        )
        .unwrap();
        let result = swap_in_adjusted(
            true,
            swap_amount,
            &mut col_reserves,
            &mut new_debt_reserves_empty(),
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(
            result.is_ok(),
            "FAIL: reserves ratio revert hit for col reserves when swap amount 14_895"
        );

        price = get_approx_center_price_in(
            swap_amount,
            true,
            &new_col_reserves_empty(),
            &debt_reserves,
        )
        .unwrap();
        let result = swap_in_adjusted(
            true,
            swap_amount,
            &mut new_col_reserves_empty(),
            &mut debt_reserves,
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(
            result.is_ok(),
            "FAIL: reserves ratio revert hit for debt reserves when swap amount 14_895"
        );
    }

    pub fn new_verify_ratio_col_reserves_swap_out() -> CollateralReserves {
        CollateralReserves {
            token0_real_reserves: U256::from(15_000u64) * U256::from(10u64).pow(U256::from(12)), /* 15_000 * 1e12 */
            token1_real_reserves: U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(12)), /* 2_000_000 * 1e12 */
            token0_imaginary_reserves: U256::ZERO,
            token1_imaginary_reserves: U256::ZERO,
        }
    }

    pub fn new_verify_ratio_debt_reserves_swap_out() -> DebtReserves {
        DebtReserves {
            token0_real_reserves: U256::from(15_000u64) * U256::from(10u64).pow(U256::from(12)),
            token1_real_reserves: U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(12)),
            token0_imaginary_reserves: U256::ZERO,
            token1_imaginary_reserves: U256::ZERO,
        }
    }

    #[test]
    fn test_swap_out_verify_reserves_in_range() {
        let decimals: i64 = 6;
        let sync_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() -
            10;

        let mut col_reserves = new_verify_ratio_col_reserves_swap_out();
        let mut debt_reserves = new_verify_ratio_debt_reserves_swap_out();

        // price = 1.000001 * 1e27
        let price = U256::from_str("1000001000000000000000000000").unwrap();

        // First reserves calculation
        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            col_reserves.token0_real_reserves,
            col_reserves.token1_real_reserves,
        );
        col_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(col_reserves.token0_real_reserves));
        col_reserves.token1_imaginary_reserves =
            U256::from(reserve_y_outside + I256::from(col_reserves.token1_real_reserves));

        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_real_reserves,
        );
        debt_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(debt_reserves.token0_real_reserves));
        debt_reserves.token1_imaginary_reserves =
            U256::from(reserve_y_outside + I256::from(debt_reserves.token1_real_reserves));

        // Swap amount where revert should hit
        let swap_amount = U256::from(14_766u64) * U256::from(10u64).pow(U256::from(12));

        let price = get_approx_center_price_out(
            swap_amount,
            false,
            &col_reserves,
            &new_debt_reserves_empty(),
        )
        .unwrap();
        let result = swap_out_adjusted(
            false,
            swap_amount,
            &mut col_reserves,
            &mut new_debt_reserves_empty(),
            decimals,
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(result.is_err(), "FAIL: reserves ratio verification revert NOT hit for col reserves when swap amount 14_766");

        let price = get_approx_center_price_out(
            swap_amount,
            false,
            &new_col_reserves_empty(),
            &debt_reserves,
        )
        .unwrap();
        let result = swap_out_adjusted(
            false,
            swap_amount,
            &mut new_col_reserves_empty(),
            &mut debt_reserves,
            decimals,
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(result.is_err(), "FAIL: reserves ratio verification revert NOT hit for debt reserves when swap amount 14_766");

        // Refresh reserves
        col_reserves = new_verify_ratio_col_reserves_swap_out();
        debt_reserves = new_verify_ratio_debt_reserves_swap_out();

        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            col_reserves.token0_real_reserves,
            col_reserves.token1_real_reserves,
        );
        col_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(col_reserves.token0_real_reserves));
        col_reserves.token1_imaginary_reserves =
            U256::from(reserve_y_outside + I256::from(col_reserves.token1_real_reserves));

        let (reserve_x_outside, reserve_y_outside) = calculate_reserves_outside_range(
            constant::B_I1E27,
            price,
            debt_reserves.token0_real_reserves,
            debt_reserves.token1_real_reserves,
        );
        debt_reserves.token0_imaginary_reserves =
            U256::from(reserve_x_outside + I256::from(debt_reserves.token0_real_reserves));
        debt_reserves.token1_imaginary_reserves =
            U256::from(reserve_y_outside + I256::from(debt_reserves.token1_real_reserves));

        // Swap amount where revert should NOT hit
        let swap_amount = U256::from(14_762u64) * U256::from(10u64).pow(U256::from(12));

        let price = get_approx_center_price_out(
            swap_amount,
            false,
            &col_reserves,
            &new_debt_reserves_empty(),
        )
        .unwrap();
        let result = swap_out_adjusted(
            false,
            swap_amount,
            &mut col_reserves,
            &mut new_debt_reserves_empty(),
            decimals,
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(
            result.is_ok(),
            "FAIL: reserves ratio verification revert hit for col reserves when swap amount 14_762"
        );

        let price = get_approx_center_price_out(
            swap_amount,
            false,
            &new_col_reserves_empty(),
            &debt_reserves,
        )
        .unwrap();
        let result = swap_out_adjusted(
            false,
            swap_amount,
            &mut new_col_reserves_empty(),
            &mut debt_reserves,
            decimals,
            decimals,
            &mut limits_wide(),
            price,
            sync_time,
        );
        assert!(result.is_ok(), "FAIL: reserves ratio verification revert hit for debt reserves when swap amount 14_762");
    }

    // Use this command to retrieve state for fluid pools:
    // ```bash
    // cast call 0xC93876C0EEd99645DD53937b25433e311881A27C \
    //  'getPoolReservesAdjusted(address)(address,address,address,uint256,uint256,(uint256,uint256,uint256),(uint256,uint256,uint256,uint256,uint256,uint256),((uint256,uint256,uint256),(uint256,uint256,uint256),(uint256,uint256,uint256),(uint256,uint256,uint256)))' \
    //  '0x0B1a513ee24972DAEf112bC777a5610d4325C9e7'
    // ```
    //
    // Use this command to get onchain estimates:
    //
    // ```bash
    // cast call -b 23526115 \
    //  0xC93876C0EEd99645DD53937b25433e311881A27C \
    //  'estimateSwapIn(address,bool,uint,uint)(uint)' \
    //  0x0B1a513ee24972DAEf112bC777a5610d4325C9e7 true 100000000000000 0
    // ```

    fn hard_limit(l: u128) -> TokenLimit {
        TokenLimit {
            available: U256::from(l),
            expands_to: U256::from(l),
            expand_duration: U256::ZERO,
        }
    }

    fn wsteth_eth_pool_23526115() -> (Token, Token, FluidV1) {
        let wsteth = Token::new(
            &Bytes::from_str("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0").unwrap(),
            "wsteth",
            18,
            0,
            &[Some(20000)],
            Chain::Ethereum,
            100,
        );
        let eth = Token::new(
            &Bytes::from_str("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE").unwrap(),
            "ETH",
            18,
            0,
            &[Some(2000)],
            Chain::Ethereum,
            100,
        );
        let pool = FluidV1::new(
            &Bytes::from_str("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7").unwrap(),
            &wsteth,
            &eth,
            CollateralReserves {
                token0_real_reserves: U256::from(4431191840536456u128),
                token1_real_reserves: U256::from(13105569017021951u128),
                token0_imaginary_reserves: U256::from(20263646714209556492u128),
                token1_imaginary_reserves: U256::from(24624319733997222300u128),
            },
            DebtReserves {
                token0_real_reserves: U256::from(3958052320699256u128),
                token1_real_reserves: U256::from(11706224851989005u128),
                token0_imaginary_reserves: U256::from(18100000404581051720u128),
                token1_imaginary_reserves: U256::from(21995063545785045888u128),
            },
            DexLimits {
                borrowable_token0: hard_limit(4431191840536456767040),
                borrowable_token1: hard_limit(6552784508510975527319),
                withdrawable_token0: hard_limit(4819160955805377144139),
                withdrawable_token1: hard_limit(6126272539623278413525),
            },
            U256::from_str("1215727283480584508000000000").unwrap(),
            U256::from(68),
            1759795200,
        );
        (wsteth, eth, pool)
    }

    #[test]
    fn test_spot_price() {
        let (wsteth, eth, pool) = wsteth_eth_pool_23526115();
        // derived via numerical estimates from onchain quotes
        let exp_spot0 = 1.21519682; // 1.21511419 adjusted by 0.0068% fee
        let exp_spot1 = 0.82291191; // 0.82228559 adjusted by 0.0068% fee

        let spot0 = pool.spot_price(&wsteth, &eth).unwrap();
        let spot1 = pool.spot_price(&eth, &wsteth).unwrap();

        let rel_err0 = (spot0 - exp_spot0).abs() / exp_spot0;
        let rel_err1 = (spot1 - exp_spot1).abs() / exp_spot1;

        assert!(
            rel_err0 < 1e-4,
            "spot0 mismatch: got {spot0}, expected {exp_spot0}, relative error: {rel_err0}"
        );
        assert!(
            rel_err1 < 1e-4,
            "spot1 mismatch: got {spot1}, expected {exp_spot1}, relative error: {rel_err1}"
        );
    }

    #[test]
    fn test_get_amount_out_zero2one() {
        let (wsteth, eth, pool) = wsteth_eth_pool_23526115();
        let amount_in = BigUint::from_str_radix("100000000000000", 10).unwrap();
        // onchain we get 121511419000000
        let exp_amount_out = BigUint::from_str_radix("121511421000000", 10).unwrap();

        let res = pool
            .get_amount_out(amount_in, &wsteth, &eth)
            .unwrap();

        assert_eq!(res.amount, exp_amount_out);
    }

    #[test]
    fn test_get_amount_out_one2zero() {
        let (wsteth, eth, pool) = wsteth_eth_pool_23526115();
        let amount_in = BigUint::from_str_radix("100000000000000", 10).unwrap();
        // onchain we get 82285596000000
        let exp_amount_out = BigUint::from_str_radix("82285598000000", 10).unwrap();

        let res = pool
            .get_amount_out(amount_in, &eth, &wsteth)
            .unwrap();
        assert_eq!(res.amount, exp_amount_out);
    }

    #[test]
    fn get_limits_zero2one() {
        let (wsteth, eth, pool) = wsteth_eth_pool_23526115();

        let (max_amount_in, _) = pool
            .get_limits(wsteth.address.clone(), eth.address.clone())
            .unwrap();
        let max_amount_onchain_test =
            // 10.2k wsteth
            BigUint::from_str_radix("10200000000000000000000", 10).unwrap();

        let _ = pool
            .get_amount_out(max_amount_in.clone(), &wsteth, &eth)
            .unwrap();
        assert!(max_amount_in < max_amount_onchain_test);
    }

    #[test]
    fn get_limits_one2zero() {
        let (wsteth, eth, pool) = wsteth_eth_pool_23526115();

        let (max_amount_in, _) = pool
            .get_limits(eth.address.clone(), wsteth.address.clone())
            .unwrap();
        let max_amount_onchain_test =
            // 10.2k wsteth
            BigUint::from_str_radix("10192694739404003000000", 10).unwrap();

        let _ = pool
            .get_amount_out(max_amount_in.clone(), &eth, &wsteth)
            .unwrap();

        assert!(max_amount_in < max_amount_onchain_test);
    }
}
