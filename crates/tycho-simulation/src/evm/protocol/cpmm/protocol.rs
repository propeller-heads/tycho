use alloy::primitives::{U256, U512};
use num_bigint::BigUint;
use num_traits::Zero;
use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::Price,
    },
    Bytes,
};

use super::reserve_price::spot_price_from_reserves;
use crate::{
    evm::protocol::{
        safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256, sqrt_u512},
        u256_num::{biguint_to_u256, u256_to_biguint},
        utils::solidity_math::mul_div,
    },
    protocol::errors::InvalidSnapshotError,
};

pub fn cpmm_try_from_with_header(
    snapshot: ComponentWithState,
) -> Result<(U256, U256), InvalidSnapshotError> {
    let reserve0 = U256::from_be_slice(
        snapshot
            .state
            .attributes
            .get("reserve0")
            .ok_or(InvalidSnapshotError::MissingAttribute("reserve0".to_string()))?,
    );

    let reserve1 = U256::from_be_slice(
        snapshot
            .state
            .attributes
            .get("reserve1")
            .ok_or(InvalidSnapshotError::MissingAttribute("reserve1".to_string()))?,
    );
    Ok((reserve0, reserve1))
}

pub fn cpmm_fee(fee_bps: u32) -> f64 {
    fee_bps as f64 / 10000.0
}

pub fn cpmm_spot_price(
    base: &Token,
    quote: &Token,
    reserve0: U256,
    reserve1: U256,
) -> Result<f64, SimulationError> {
    if base < quote {
        spot_price_from_reserves(reserve0, reserve1, base.decimals, quote.decimals)
    } else {
        spot_price_from_reserves(reserve1, reserve0, base.decimals, quote.decimals)
    }
}

pub fn cpmm_get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee: ProtocolFee,
) -> Result<U256, SimulationError> {
    if amount_in == U256::from(0u64) {
        return Err(SimulationError::InvalidInput("Amount in cannot be zero".to_string(), None));
    }

    if reserve_in == U256::from(0u64) || reserve_out == U256::from(0u64) {
        return Err(SimulationError::RecoverableError("No liquidity".to_string()));
    }

    let amount_in_with_fee = safe_mul_u256(amount_in, fee.numerator)?;
    let numerator = safe_mul_u256(amount_in_with_fee, reserve_out)?;
    // SAFETY: callers (uniswap_v2, pancakeswap_v2) decode reserves from on-chain uint112
    // storage, so reserve_in < 2^112, and fee.precision <= 10^4 < 2^14, keeping the product
    // below 2^126 -- it cannot wrap U256.
    let reserve_in_scaled = reserve_in.wrapping_mul(fee.precision);
    debug_assert!(
        fee.precision.is_zero() || reserve_in_scaled / fee.precision == reserve_in,
        "reserve_in * fee.precision exceeded the proven uint112 x 10^4 bound"
    );
    let denominator = safe_add_u256(reserve_in_scaled, amount_in_with_fee)?;

    safe_div_u256(numerator, denominator)
}

/// Returns the soft limit for amount_in to achieve approximately 90% price impact.
///
/// This calculation assumes a fee-less constant product formula:
/// - 90% price impact: `(reserve_out - y) / (reserve_in + x) = 0.1 × (reserve_out / reserve_in)`
/// - Constant product: `(reserve_in + x) × (reserve_out - y) = reserve_in × reserve_out`
///
/// However, in practice, swaps apply fees to the input amount, with the fee portion then added to
/// the pool's liquidity. So the constant formula becomes:
/// - `(reserve_in + x × (1 - fee)) × (reserve_out - y) = reserve_in × reserve_out`
///
/// This asymmetry (full `x` added to reserves, but `y` based on fee-adjusted input) causes
/// the actual price impact to be slightly less than 90%. For typical fees (0.25%-0.3%), the
/// actual price impact is approximately 89.9% instead of exactly 90%.
pub fn cpmm_get_limits(
    sell_token: Bytes,
    buy_token: Bytes,
    reserve0: U256,
    reserve1: U256,
    fee_bps: u32,
) -> Result<(BigUint, BigUint), SimulationError> {
    if reserve0 == U256::from(0u64) || reserve1 == U256::from(0u64) {
        return Ok((BigUint::zero(), BigUint::zero()));
    }

    let zero_for_one = sell_token < buy_token;
    let (reserve_in, reserve_out) =
        if zero_for_one { (reserve0, reserve1) } else { (reserve1, reserve0) };

    // Soft limit for amount in is the amount to get approximately 90% price impact.
    // Solving the fee-less equations yields: x = (√10 - 1) × reserve_in ≈ 2.162 × reserve_in
    let amount_in = mul_div(reserve_in, U256::from(2162u128), U256::from(1e3))?;

    // Calculate amount_out using the actual swap formula that accounts for fees
    // amount_in_with_fee = amount_in * (10000 - fee_bps) / 10000
    // amount_out = (amount_in_with_fee * reserve_out) / (reserve_in + amount_in_with_fee)
    let fee_multiplier = U256::from(10000 - fee_bps);
    let amount_in_with_fee = safe_mul_u256(amount_in, fee_multiplier)?;

    let amount_out =
        mul_div(reserve_out, amount_in_with_fee, safe_add_u256(reserve_in, amount_in)?)?;

    Ok((u256_to_biguint(amount_in), u256_to_biguint(amount_out)))
}

pub fn cpmm_delta_transition(
    delta: ProtocolStateDelta,
    reserve0_mut: &mut U256,
    reserve1_mut: &mut U256,
) -> Result<(), TransitionError> {
    // reserve0 and reserve1 are considered required attributes and are expected in every delta
    // we process
    let reserve0 = U256::from_be_slice(
        delta
            .updated_attributes
            .get("reserve0")
            .ok_or(TransitionError::MissingAttribute("reserve0".to_string()))?,
    );
    let reserve1 = U256::from_be_slice(
        delta
            .updated_attributes
            .get("reserve1")
            .ok_or(TransitionError::MissingAttribute("reserve1".to_string()))?,
    );
    *reserve0_mut = reserve0;
    *reserve1_mut = reserve1;
    Ok(())
}

/// Represents a protocol fee as a numerator and precision.
pub struct ProtocolFee {
    pub numerator: U256,
    pub precision: U256,
}

impl ProtocolFee {
    pub fn new(numerator: U256, precision: U256) -> Self {
        ProtocolFee { numerator, precision }
    }
}

/// Calculates the exact amount of token_in required to move the pool's marginal price down to
/// a target price.
///
/// # Algorithm
///
/// Derives how much to swap to reach a target price using the constant product formula.
/// **Note**: This method assumes k remains constant, but in reality fees accrue to the pool,
/// causing k to increase slightly. This simplification leads to a conservative
/// underestimation of the pool's supply capacity.
///
/// ## Base equations
/// 1. Constant product: `x * y = k` where x = reserve_in, y = reserve_out
/// 2. Swap with 0.3% fee: Only 99.7% of input affects price
/// 3. Marginal price after swap: `price = (x' * 10000) / (y' * 9970)`
///
/// ## Derivation
/// We want the pool to reach target price: `price = sell_price / buy_price`
///
/// From marginal price formula:
/// ```text,no_run
/// x' / y' = (sell_price * 9970) / (buy_price * 10000)  [call this target_price_w_fee]
/// ```
///
/// From constant product:
/// ```text,no_run
/// x' * y' = k
/// ```
///
///
/// Substituting the first into the second:
/// ```text,no_run
/// x' = target_price_w_fee * y'
/// (target_price_w_fee * y') * y' = k
/// y'^2 = k / target_price_w_fee
/// y' = sqrt(k / target_price_w_fee)
/// ```
///
/// Therefore:
/// ```text,no_run
/// x' = target_price_w_fee * y'
///    = target_price_w_fee * sqrt(k / target_price_w_fee)
///    = sqrt(k * target_price_w_fee)
/// ```
///
/// Amount to swap in:
/// ```text,no_run
/// amount_in = x' - x = sqrt(k * target_price_w_fee) - reserve_in
/// ```
///
/// where `target_price_w_fee = (sell_price * 9970) / (buy_price * 10000)`
/// Then swap to get amount_out.
///
/// # Returns
/// * `Ok((amount_in, implied_amount_out))` - The implied amount out is computed analytically and
///   will be smaller than actually swapping against the pool.
/// * `Err(SimulationError)` - If an error occurs during calculation.
pub fn cpmm_swap_to_price(
    reserve_in: U256,
    reserve_out: U256,
    target_price: &Price,
    fee: ProtocolFee,
) -> Result<(BigUint, BigUint), SimulationError> {
    if reserve_in == U256::ZERO || reserve_out == U256::ZERO {
        return Err(SimulationError::FatalError("Reserves cannot be zero".to_string()));
    }

    // Flip target pool price to swap price
    let swap_price_num = biguint_to_u256(&target_price.denominator);
    let swap_price_den = biguint_to_u256(&target_price.numerator);

    // Check reachability: target price must be above the spot price (with fees)
    // swap_price_num/swap_price_den >= (reserve_in * FEE_PRECISION) / (reserve_out *
    // FEE_NUMERATOR)
    // Cross-multiply to avoid division: swap_price_num * reserve_out * FEE_NUMERATOR >=
    // swap_price_den * reserve_in * FEE_PRECISION
    // Use U512 precision to match the calculation of new reserves
    // SAFETY: each first factor pair is two widened U256 values, so the product is
    // < (2^256 - 1)^2 < 2^512 and the wrapping mul cannot wrap. The subsequent fee factor stays
    // checked: aerodrome_v1 reserves are uint256 on-chain, so the triple product has no provable
    // headroom there.
    let target_two_factor = U512::from(swap_price_num).wrapping_mul(U512::from(reserve_out));
    let current_two_factor = U512::from(swap_price_den).wrapping_mul(U512::from(reserve_in));
    debug_assert!(
        (reserve_out.is_zero() ||
            target_two_factor / U512::from(reserve_out) == U512::from(swap_price_num)) &&
            (reserve_in.is_zero() ||
                current_two_factor / U512::from(reserve_in) == U512::from(swap_price_den)),
        "U256 x U256 product wrapped past 2^512"
    );
    let target_price_cross_mult = target_two_factor
        .checked_mul(U512::from(fee.numerator))
        .ok_or_else(|| SimulationError::FatalError("Overflow in price check".to_string()))?;
    let current_price_cross_mult = current_two_factor
        .checked_mul(U512::from(fee.precision))
        .ok_or_else(|| SimulationError::FatalError("Overflow in price check".to_string()))?;

    if target_price_cross_mult < current_price_cross_mult {
        return Err(SimulationError::InvalidInput(
            "Target price is unreachable (already below current spot price)".to_string(),
            None,
        ));
    }

    // Calculate new reserve_in: x' = sqrt(k * price_num * FEE_NUMERATOR / (price_den *
    // FEE_PRECISION))
    let k = U512::from(reserve_in) * U512::from(reserve_out);
    let k_times_price = k * U512::from(swap_price_num) * U512::from(fee.numerator) /
        (U512::from(swap_price_den) * U512::from(fee.precision));
    let x_prime_u512 = sqrt_u512(k_times_price);

    // Convert back to U256 and calculate amount_in
    let limbs = x_prime_u512.as_limbs();
    let x_prime = U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]);

    if x_prime <= reserve_in {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }

    let amount_in = safe_sub_u256(x_prime, reserve_in)?;
    if amount_in == U256::ZERO {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }

    let implied_amount_out = mul_div(amount_in, swap_price_den, swap_price_num)?;

    Ok((u256_to_biguint(amount_in), u256_to_biguint(implied_amount_out)))
}
