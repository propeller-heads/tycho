//! Construct `crate::evm::protocol::curve::math::Pool` from raw on-chain state.
//!
//! This module bridges the gap between raw on-chain data (balances, decimals,
//! storage values) and the typed `Pool` enum that curve-math needs for swap
//! computation.
//!
//! # Usage
//!
//! ```rust
//! use curve_adapter::{CurveVariant, RawPoolState, build_pool};
//! use alloy_primitives::U256;
//!
//! let state = RawPoolState {
//!     variant: CurveVariant::StableSwapV2,
//!     balances: vec![
//!         U256::from(1_000_000_000_000_000_000_000u128),
//!         U256::from(1_000_000_000_000u128),
//!     ],
//!     token_decimals: vec![18, 6],
//!     amp: U256::from(40_000u64), // A=400 * A_PRECISION=100
//!     fee: Some(U256::from(4_000_000u64)),
//!     ..Default::default()
//! };
//!
//! let pool = build_pool(&state).unwrap();
//! let dy = pool.get_amount_out(0, 1, U256::from(1_000_000_000_000_000_000u128));
//! ```

use alloy_primitives::U256;

use crate::evm::protocol::curve::{adapter::CurveVariant, math::Pool};

/// Errors from [`build_pool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// A required field is missing for this variant.
    MissingField { variant: CurveVariant, field: &'static str },
    /// The number of balances doesn't match the expected coin count.
    WrongCoinCount { variant: CurveVariant, expected: usize, actual: usize },
    /// Token decimals length doesn't match balances length.
    DecimalsMismatch { balances_len: usize, decimals_len: usize },
    /// Token decimals exceed the maximum (18 for CryptoSwap, 36 for StableSwap).
    DecimalsTooLarge { index: usize, decimals: u8, max: u8 },
    /// Dynamic rates length doesn't match balances length.
    DynamicRatesMismatch { balances_len: usize, rates_len: usize },
    /// Price scale has wrong number of elements.
    PriceScaleWrongLen { expected: usize, actual: usize },
    /// StableSwapMeta requires `dynamic_rates` with an explicit rate for
    /// the base pool LP token (last coin). Without it, rates[N-1] defaults
    /// to `10^(36-decimals)` which is incorrect — it must be
    /// `base_pool.get_virtual_price()`.
    MetaMissingVirtualPrice,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField { variant, field } => {
                write!(f, "{variant}: missing required field `{field}`")
            }
            Self::WrongCoinCount { variant, expected, actual } => {
                write!(f, "{variant}: expected {expected} coins, got {actual} balances")
            }
            Self::DecimalsMismatch { balances_len, decimals_len } => write!(
                f,
                "token_decimals length ({decimals_len}) != balances length ({balances_len})"
            ),
            Self::DecimalsTooLarge { index, decimals, max } => {
                write!(f, "token_decimals[{index}] = {decimals} exceeds maximum {max}")
            }
            Self::DynamicRatesMismatch { balances_len, rates_len } => {
                write!(f, "dynamic_rates length ({rates_len}) != balances length ({balances_len})")
            }
            Self::PriceScaleWrongLen { expected, actual } => {
                write!(f, "price_scale: expected {expected} elements, got {actual}")
            }
            Self::MetaMissingVirtualPrice => write!(
                f,
                "StableSwapMeta: dynamic_rates must provide an explicit rate for the last coin \
                 (base pool LP token virtual_price). Without it, swap calculations are incorrect."
            ),
        }
    }
}

impl std::error::Error for BuildError {}

/// Raw on-chain pool state, as collected by a transport (RPC, Substreams, etc.).
///
/// The consumer fills in the fields relevant to the pool's [`CurveVariant`],
/// then calls [`build_pool`] to get a `crate::evm::protocol::curve::math::Pool` ready for swap
/// computation.
#[derive(Debug, Clone)]
pub struct RawPoolState {
    /// Which Curve variant this pool is.
    pub variant: CurveVariant,

    /// Token balances in native token units (wei). **Updates every block.**
    ///
    /// Length determines coin count (2, 3, or 4+).
    ///
    /// # Gotchas
    ///
    /// **StableSwapNG:** `balances(i)` on-chain returns `balanceOf(pool) - admin_balances[i]`,
    /// excluding uncollected admin fees. An indexer tracking ERC20 Transfer events sees
    /// `balanceOf` changes, not `balances()`. The difference is small but causes wei-level
    /// mismatches. Use the `balances()` getter, not `balanceOf`.
    ///
    /// **Rebase tokens (e.g. stETH):** balances change without Transfer events — the token
    /// rebases in-place. Requires calling `balanceOf(pool)` on the rebase token each block.
    pub balances: Vec<U256>,

    /// Token decimals for each coin (e.g. `[18, 6]` for ETH/USDC).
    /// Must have the same length as `balances`.
    pub token_decimals: Vec<u8>,

    /// Amplification parameter — already interpolated for the target block.
    /// **Semi-static:** only changes during A ramping (rare admin events, days apart).
    ///
    /// This value must match what the on-chain `_A()` internal function returns
    /// at the target block's timestamp. It includes the variant's precision
    /// scaling (A_PRECISION=100 for StableSwap V2+, A_MULTIPLIER=10000 for
    /// CryptoSwap, 1 for V0/V1).
    ///
    /// # How to obtain this value
    ///
    /// **RPC consumer:**
    /// Call the pool's `A()` view function at the target block.
    /// Returns the already-interpolated value in a single RPC call.
    /// ```text
    /// let amp = pool_contract.A().block(block_number).call().await?;
    /// ```
    /// **Warning:** for all A_PRECISION=100 variants (V2, Meta, NG, ALend),
    /// `A()` returns `initial_A / A_PRECISION` via integer division, losing
    /// the remainder:
    /// ```text
    /// initial_A = 79258
    /// A() = 79258 / 100 = 792      (truncated)
    /// A() * 100 = 79200 ≠ 79258    (lost 58)
    /// ```
    /// For exact precision, read `initial_A()` directly when no ramping
    /// (`initial_A == future_A`), or use [`interpolate_a`] during ramps.
    ///
    /// **Substreams / storage-based consumer:**
    /// Read `initial_A`, `future_A`, `initial_A_time`, `future_A_time` from
    /// contract storage, then interpolate for the current block's timestamp:
    /// ```text
    /// let amp = curve_adapter::interpolate_a(
    ///     initial_a, future_a,
    ///     initial_a_time, future_a_time,
    ///     block_timestamp,
    /// );
    /// ```
    /// For V0/V1 pools that store a single `A` value (no ramping),
    /// pass that value directly.
    ///
    /// **Caution with factory deploy events:** `PlainPoolDeployed` and
    /// `MetaPoolDeployed` events emit A as the user-provided value (e.g. 400),
    /// NOT the raw on-chain value (e.g. 40000). Multiply by A_PRECISION
    /// (100 for V2/NG/Meta/ALend) before passing here.
    pub amp: U256,

    /// StableSwap fee. **Static** (set at pool creation).
    /// Required for all StableSwap variants.
    pub fee: Option<U256>,

    /// CryptoSwap mid fee. **Static.** Required for all CryptoSwap variants.
    pub mid_fee: Option<U256>,

    /// CryptoSwap out fee. **Static.** Required for all CryptoSwap variants.
    pub out_fee: Option<U256>,

    /// CryptoSwap fee gamma. **Static.** Required for all CryptoSwap variants.
    pub fee_gamma: Option<U256>,

    /// Dynamic fee multiplier. **Static.** Required for StableSwapNG and StableSwapALend.
    pub offpeg_fee_multiplier: Option<U256>,

    /// Price scale(s). **Updates every block.**
    /// TwoCrypto: 1 element, TriCrypto: 2 elements.
    /// Required for all CryptoSwap variants.
    pub price_scale: Option<Vec<U256>>,

    /// Pool invariant D. **Updates every block.** Required for all CryptoSwap variants.
    pub d: Option<U256>,

    /// Gamma parameter. **Semi-static** (only changes during admin ramp).
    /// Required for TwoCryptoV1, TwoCryptoNG, TriCryptoV1, TriCryptoNG.
    /// NOT required for TwoCryptoStable (gamma is ignored).
    pub gamma: Option<U256>,

    /// Per-token dynamic rates for StableSwap variants. **Depends on token type.**
    ///
    /// If `None`, rates are computed from `token_decimals` as `10^(36 - decimals)`.
    /// This is correct for v6+ crvUSD factory pools (balances variant) which
    /// use static decimal-based rates and have no `stored_rates()` getter.
    ///
    /// If `Some`, must have the same length as `balances`. Each element:
    /// - `Some(rate)` — use this dynamic rate (oracle, ERC4626, etc.)
    /// - `None` — compute from decimals as `10^(36 - decimals)`
    ///
    /// # When to provide dynamic rates
    ///
    /// **StableSwapNG with oracle tokens:** Call `stored_rates()` on the pool
    /// contract each block. Plain tokens return static rates, but ERC4626/oracle
    /// tokens return rates that change per-block.
    ///
    /// **MetaPools:** `rates[last]` must be `base_pool.get_virtual_price()`.
    /// To find the base pool address: try `pool.base_pool()` first. MetaPool
    /// Factory proxy pools lack this getter — use `factory.get_base_pool(pool)`
    /// instead. Do not assume the base pool is 3pool; BTC meta pools use sBTC.
    ///
    /// # Gotchas
    ///
    /// **Fee-on-transfer tokens:** actual received amount differs from the
    /// transfer parameter. Curve pools generally do not support these tokens,
    /// but if encountered, balances will be incorrect.
    ///
    /// **Tokens with 0 decimals:** rate becomes `10^36` — valid but rounding
    /// impact is outsized. Every wei of such a token is worth `10^36` in
    /// normalized space.
    pub dynamic_rates: Option<Vec<Option<U256>>>,

    /// On-chain precisions for CryptoSwap variants. **Immutable.**
    ///
    /// If `Some`, used directly instead of computing from `token_decimals`.
    /// Read from the pool contract's `precisions()` getter.
    ///
    /// This is important because some tokens report incorrect `decimals()`
    /// (e.g. Spectra PT tokens), and the factory computes the correct
    /// precisions at deployment time.
    ///
    /// If `None`, precisions are computed as `10^(18 - decimals)`.
    pub precisions: Option<Vec<U256>>,

    /// Selects the deployed Vyper flavour for **TwoCryptoV1 only**. **Immutable.**
    ///
    /// Legacy 2-coin CryptoSwap V1 pools ship in two flavours whose Newton `get_y` solver differs
    /// by a single `mul2` integer-division grouping (see
    /// [`crate::evm::protocol::curve::math::core::twocrypto_v1`]). `Some(true)` selects the ETH
    /// pool grouping (all factory/proxy and WETH-paired legacy pools); `Some(false)` selects the
    /// non-ETH legacy direct-deploy grouping.
    ///
    /// Required for `TwoCryptoV1` — [`build_pool`] returns [`BuildError::MissingField`] if it is
    /// `None`. Ignored for every other variant. Obtain it via
    /// [`detect_eth_variant`](crate::evm::protocol::curve::adapter::detect_eth_variant).
    pub eth_variant: Option<bool>,
}

impl Default for RawPoolState {
    fn default() -> Self {
        Self {
            variant: CurveVariant::StableSwapV2,
            balances: Vec::new(),
            token_decimals: Vec::new(),
            amp: U256::ZERO,
            fee: None,
            mid_fee: None,
            out_fee: None,
            fee_gamma: None,
            offpeg_fee_multiplier: None,
            price_scale: None,
            d: None,
            gamma: None,
            dynamic_rates: None,
            precisions: None,
            eth_variant: None,
        }
    }
}

/// Interpolate the amplification parameter for a given block timestamp.
///
/// This is a 1:1 port of the Vyper `_A()` internal function from Curve
/// StableSwap and CryptoSwap contracts. The formula is identical across all
/// Curve variants that support A ramping (V2, Meta, NG, ALend, all CryptoSwap).
///
/// V0/V1 pools do not support ramping — they store a single `A` value.
/// For those, pass `A` directly to [`RawPoolState::amp`] without calling this.
///
/// CryptoSwap contracts store `initial_A_gamma` and `future_A_gamma` as packed
/// values (A and gamma in a single uint256). The caller must unpack the A
/// component before passing it here.
///
/// # Arguments
///
/// * `initial_a` — raw start value from storage (includes A_PRECISION/A_MULTIPLIER)
/// * `future_a` — raw end value from storage (includes A_PRECISION/A_MULTIPLIER)
/// * `initial_a_time` — ramp start timestamp (seconds)
/// * `future_a_time` — ramp end timestamp (seconds)
/// * `block_timestamp` — target block's timestamp (seconds)
///
/// # Panics
///
/// Panics if `block_timestamp < initial_a_time` (block is before ramp start).
/// This should never happen with valid on-chain data.
pub fn interpolate_a(
    initial_a: U256,
    future_a: U256,
    initial_a_time: u64,
    future_a_time: u64,
    block_timestamp: u64,
) -> U256 {
    if block_timestamp >= future_a_time {
        return future_a;
    }

    // block_timestamp < initial_a_time should never happen with valid on-chain data.
    // The Vyper code does not guard against this either (would underflow and revert).
    let elapsed = U256::from(block_timestamp - initial_a_time);
    let duration = U256::from(future_a_time - initial_a_time);

    if future_a > initial_a {
        initial_a + (future_a - initial_a) * elapsed / duration
    } else {
        initial_a - (initial_a - future_a) * elapsed / duration
    }
}

/// Compute StableSwap rates from token decimals and optional dynamic rates.
///
/// For each token:
/// - If a dynamic rate is provided, use it directly.
/// - Otherwise, compute as `10^(36 - decimals)`.
///
/// This matches the on-chain `RATE_MULTIPLIER` / `rates` / `PRECISION * RATES`
/// pattern used across all StableSwap variants.
fn compute_stableswap_rates(
    token_decimals: &[u8],
    dynamic_rates: &Option<Vec<Option<U256>>>,
) -> Vec<U256> {
    token_decimals
        .iter()
        .enumerate()
        .map(|(i, &decimals)| {
            // Check if there's a dynamic rate for this token
            if let Some(ref rates) = dynamic_rates {
                if let Some(Some(rate)) = rates.get(i) {
                    return *rate;
                }
            }
            // Default: 10^(36 - decimals)
            U256::from(10u64).pow(U256::from(36 - decimals as u32))
        })
        .collect()
}

/// Compute CryptoSwap precisions from token decimals.
///
/// For each token: `10^(18 - decimals)`.
///
/// This matches the on-chain `precisions` storage variable in all CryptoSwap
/// contracts.
fn compute_crypto_precisions(token_decimals: &[u8]) -> Vec<U256> {
    token_decimals
        .iter()
        .map(|&decimals| U256::from(10u64).pow(U256::from(18 - decimals as u32)))
        .collect()
}

/// Helper to extract a required Option field or return a BuildError.
macro_rules! require {
    ($state:expr, $field:ident) => {
        $state
            .$field
            .ok_or(BuildError::MissingField {
                variant: $state.variant,
                field: stringify!($field),
            })?
    };
}

/// Construct a `crate::evm::protocol::curve::math::Pool` from raw on-chain state.
///
/// This function:
/// 1. Validates that all required fields are present for the given variant.
/// 2. Computes rates/precisions from `token_decimals` (with dynamic rate overrides).
/// 3. Constructs the appropriate `Pool` enum variant.
///
/// The returned `Pool` is ready for `get_amount_out()` / `get_amount_in()` calls.
pub fn build_pool(state: &RawPoolState) -> Result<Pool, BuildError> {
    // Common validation
    if state.balances.len() != state.token_decimals.len() {
        return Err(BuildError::DecimalsMismatch {
            balances_len: state.balances.len(),
            decimals_len: state.token_decimals.len(),
        });
    }

    // Validate dynamic_rates length if provided
    if let Some(ref dr) = state.dynamic_rates {
        if dr.len() != state.balances.len() {
            return Err(BuildError::DynamicRatesMismatch {
                balances_len: state.balances.len(),
                rates_len: dr.len(),
            });
        }
    }

    match state.variant {
        CurveVariant::StableSwapV0 |
        CurveVariant::StableSwapV1 |
        CurveVariant::StableSwapV2 |
        CurveVariant::StableSwapSTETH |
        CurveVariant::StableSwapMeta => build_stableswap_plain(state),
        CurveVariant::StableSwapNG => build_stableswap_ng(state),
        CurveVariant::StableSwapALend => build_stableswap_alend(state),
        CurveVariant::TwoCryptoV1 | CurveVariant::TwoCryptoNG | CurveVariant::TwoCryptoStable => {
            build_twocrypto(state)
        }
        CurveVariant::TriCryptoV1 | CurveVariant::TriCryptoNG => build_tricrypto(state),
    }
}

/// Build StableSwapV0, V1, V2, or Meta — all share the same field layout:
/// `{ balances, rates, amp, fee }`.
fn build_stableswap_plain(state: &RawPoolState) -> Result<Pool, BuildError> {
    let fee = require!(state, fee);

    for (i, &d) in state.token_decimals.iter().enumerate() {
        if d > 36 {
            return Err(BuildError::DecimalsTooLarge { index: i, decimals: d, max: 36 });
        }
    }

    // For Meta pools, the last coin is a base pool LP token whose rate must
    // be the base pool's virtual_price. Validate that the consumer provided it.
    if state.variant == CurveVariant::StableSwapMeta {
        let n = state.balances.len();
        let has_vp = state
            .dynamic_rates
            .as_ref()
            .and_then(|dr| dr.get(n - 1))
            .map(|r| r.is_some())
            .unwrap_or(false);
        if !has_vp {
            return Err(BuildError::MetaMissingVirtualPrice);
        }
    }

    let rates = compute_stableswap_rates(&state.token_decimals, &state.dynamic_rates);
    let balances = state.balances.clone();
    let amp = state.amp;

    Ok(match state.variant {
        CurveVariant::StableSwapV0 => Pool::StableSwapV0 { balances, rates, amp, fee },
        CurveVariant::StableSwapV1 => Pool::StableSwapV1 { balances, rates, amp, fee },
        CurveVariant::StableSwapV2 => Pool::StableSwapV2 { balances, rates, amp, fee },
        CurveVariant::StableSwapSTETH => Pool::StableSwapSTETH { balances, rates, amp, fee },
        CurveVariant::StableSwapMeta => Pool::StableSwapMeta { balances, rates, amp, fee },
        _ => unreachable!(),
    })
}

fn build_stableswap_ng(state: &RawPoolState) -> Result<Pool, BuildError> {
    let fee = require!(state, fee);
    // crvUSD factory pools (v5 and v6) lack offpeg_fee_multiplier.
    // FEE_DENOMINATOR makes dynamic_fee() return the static fee unchanged.
    // Consumers reading from substreams or other indexers should pass
    // offpeg_fee_multiplier: None for these pools — this default handles it.
    let offpeg = state
        .offpeg_fee_multiplier
        .unwrap_or(U256::from(10_000_000_000u64));

    for (i, &d) in state.token_decimals.iter().enumerate() {
        if d > 36 {
            return Err(BuildError::DecimalsTooLarge { index: i, decimals: d, max: 36 });
        }
    }

    let rates = compute_stableswap_rates(&state.token_decimals, &state.dynamic_rates);

    Ok(Pool::StableSwapNG {
        balances: state.balances.clone(),
        rates,
        amp: state.amp,
        fee,
        offpeg_fee_multiplier: offpeg,
    })
}

fn build_stableswap_alend(state: &RawPoolState) -> Result<Pool, BuildError> {
    let fee = require!(state, fee);
    let offpeg = require!(state, offpeg_fee_multiplier);

    for (i, &d) in state.token_decimals.iter().enumerate() {
        if d > 18 {
            return Err(BuildError::DecimalsTooLarge { index: i, decimals: d, max: 18 });
        }
    }

    let precision_mul = compute_crypto_precisions(&state.token_decimals);

    Ok(Pool::StableSwapALend {
        balances: state.balances.clone(),
        precision_mul,
        amp: state.amp,
        fee,
        offpeg_fee_multiplier: offpeg,
    })
}

fn build_twocrypto(state: &RawPoolState) -> Result<Pool, BuildError> {
    if state.balances.len() != 2 {
        return Err(BuildError::WrongCoinCount {
            variant: state.variant,
            expected: 2,
            actual: state.balances.len(),
        });
    }

    for (i, &d) in state.token_decimals.iter().enumerate() {
        if d > 18 {
            return Err(BuildError::DecimalsTooLarge { index: i, decimals: d, max: 18 });
        }
    }

    let mid_fee = require!(state, mid_fee);
    let out_fee = require!(state, out_fee);
    let fee_gamma = require!(state, fee_gamma);
    let d = require!(state, d);
    let ann = state.amp;

    let price_scale_vec = state
        .price_scale
        .as_ref()
        .ok_or(BuildError::MissingField { variant: state.variant, field: "price_scale" })?;
    if price_scale_vec.len() != 1 {
        return Err(BuildError::PriceScaleWrongLen { expected: 1, actual: price_scale_vec.len() });
    }
    let price_scale = price_scale_vec[0];

    let default_precs = compute_crypto_precisions(&state.token_decimals);
    let precisions = state
        .precisions
        .as_deref()
        .unwrap_or(&default_precs);
    let balances: [U256; 2] = [state.balances[0], state.balances[1]];
    let prec_arr: [U256; 2] = [precisions[0], precisions[1]];

    match state.variant {
        CurveVariant::TwoCryptoV1 => {
            let gamma = require!(state, gamma);
            // TwoCryptoV1 has two deployed Vyper flavours; the consumer must say which one via
            // `eth_variant` (typically from `detect_eth_variant`). Other variants ignore it.
            let eth_variant = require!(state, eth_variant);
            Ok(Pool::TwoCryptoV1 {
                balances,
                precisions: prec_arr,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
                eth_variant,
            })
        }
        CurveVariant::TwoCryptoNG => {
            let gamma = require!(state, gamma);
            Ok(Pool::TwoCryptoNG {
                balances,
                precisions: prec_arr,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            })
        }
        CurveVariant::TwoCryptoStable => Ok(Pool::TwoCryptoStable {
            balances,
            precisions: prec_arr,
            price_scale,
            d,
            ann,
            mid_fee,
            out_fee,
            fee_gamma,
        }),
        _ => unreachable!("build_twocrypto called for non-twocrypto variant"),
    }
}

fn build_tricrypto(state: &RawPoolState) -> Result<Pool, BuildError> {
    if state.balances.len() != 3 {
        return Err(BuildError::WrongCoinCount {
            variant: state.variant,
            expected: 3,
            actual: state.balances.len(),
        });
    }

    for (i, &d) in state.token_decimals.iter().enumerate() {
        if d > 18 {
            return Err(BuildError::DecimalsTooLarge { index: i, decimals: d, max: 18 });
        }
    }

    let mid_fee = require!(state, mid_fee);
    let out_fee = require!(state, out_fee);
    let fee_gamma = require!(state, fee_gamma);
    let d = require!(state, d);
    let gamma = require!(state, gamma);
    let ann = state.amp;

    let price_scale_vec = state
        .price_scale
        .as_ref()
        .ok_or(BuildError::MissingField { variant: state.variant, field: "price_scale" })?;
    if price_scale_vec.len() != 2 {
        return Err(BuildError::PriceScaleWrongLen { expected: 2, actual: price_scale_vec.len() });
    }
    let price_scale: [U256; 2] = [price_scale_vec[0], price_scale_vec[1]];

    let default_precs = compute_crypto_precisions(&state.token_decimals);
    let precisions = state
        .precisions
        .as_deref()
        .unwrap_or(&default_precs);
    let balances: [U256; 3] = [state.balances[0], state.balances[1], state.balances[2]];
    let prec_arr: [U256; 3] = [precisions[0], precisions[1], precisions[2]];

    match state.variant {
        CurveVariant::TriCryptoV1 => Ok(Pool::TriCryptoV1 {
            balances,
            precisions: prec_arr,
            price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
        }),
        CurveVariant::TriCryptoNG => Ok(Pool::TriCryptoNG {
            balances,
            precisions: prec_arr,
            price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
        }),
        _ => unreachable!("build_tricrypto called for non-tricrypto variant"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_a_no_ramp() {
        // initial_A == future_A, any timestamp → returns the value
        let a = U256::from(40_000u64);
        let result = interpolate_a(a, a, 1000, 2000, 1500);
        assert_eq!(result, a);
    }

    #[test]
    fn interpolate_a_ramp_complete() {
        // timestamp >= future_a_time → returns future_a
        let result = interpolate_a(U256::from(20_000u64), U256::from(40_000u64), 1000, 2000, 3000);
        assert_eq!(result, U256::from(40_000u64));
    }

    #[test]
    fn interpolate_a_ramp_exactly_at_end() {
        let result = interpolate_a(U256::from(20_000u64), U256::from(40_000u64), 1000, 2000, 2000);
        assert_eq!(result, U256::from(40_000u64));
    }

    #[test]
    fn interpolate_a_ramp_up_midpoint() {
        // Ramp from 20000 to 40000 over [1000, 2000]. At t=1500 (midpoint):
        // 20000 + (40000-20000) * (1500-1000) / (2000-1000) = 20000 + 10000 = 30000
        let result = interpolate_a(U256::from(20_000u64), U256::from(40_000u64), 1000, 2000, 1500);
        assert_eq!(result, U256::from(30_000u64));
    }

    #[test]
    fn interpolate_a_ramp_down_midpoint() {
        // Ramp from 40000 to 20000 over [1000, 2000]. At t=1500:
        // 40000 - (40000-20000) * 500 / 1000 = 40000 - 10000 = 30000
        let result = interpolate_a(U256::from(40_000u64), U256::from(20_000u64), 1000, 2000, 1500);
        assert_eq!(result, U256::from(30_000u64));
    }

    #[test]
    fn interpolate_a_ramp_up_quarter() {
        // Ramp from 10000 to 50000 over [0, 1000]. At t=250:
        // 10000 + (50000-10000) * 250 / 1000 = 10000 + 10000 = 20000
        let result = interpolate_a(U256::from(10_000u64), U256::from(50_000u64), 0, 1000, 250);
        assert_eq!(result, U256::from(20_000u64));
    }

    #[test]
    fn interpolate_a_ramp_at_start() {
        // timestamp == initial_a_time → returns initial_a
        let result = interpolate_a(U256::from(20_000u64), U256::from(40_000u64), 1000, 2000, 1000);
        assert_eq!(result, U256::from(20_000u64));
    }

    #[test]
    fn interpolate_a_integer_division_truncation() {
        // Verify integer division matches Vyper behavior (truncates, not rounds).
        // Ramp from 10000 to 10003 over [0, 1000]. At t=1:
        // 10000 + 3 * 1 / 1000 = 10000 + 0 = 10000 (truncated)
        let result = interpolate_a(U256::from(10_000u64), U256::from(10_003u64), 0, 1000, 1);
        assert_eq!(result, U256::from(10_000u64));
    }

    #[test]
    fn build_stableswap_v0_basic() {
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV0,
            balances: vec![
                U256::from(1_000_000_000_000_000_000_000u128), // 1000 DAI
                U256::from(1_000_000_000u128),                 // 1000 USDC (6 dec)
                U256::from(1_000_000_000u128),                 // 1000 USDT (6 dec)
                U256::from(1_000_000_000_000_000_000_000u128), // 1000 sUSD
            ],
            token_decimals: vec![18, 6, 6, 18],
            amp: U256::from(200u64), // A_PRECISION=1
            fee: Some(U256::from(4_000_000u64)),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        // Verify it's the right variant by attempting a swap
        let dy = pool.get_amount_out(0, 1, U256::from(1_000_000_000_000_000_000u128));
        assert!(dy.is_some());
    }

    #[test]
    fn build_stableswap_v2_rates_18_6() {
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV2,
            balances: vec![
                U256::from(1_000_000_000_000_000_000_000u128),
                U256::from(1_000_000_000u128),
            ],
            token_decimals: vec![18, 6],
            amp: U256::from(40_000u64), // 400 * A_PRECISION(100)
            fee: Some(U256::from(4_000_000u64)),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();

        // Check rates are correct: [10^18, 10^30]
        match &pool {
            Pool::StableSwapV2 { rates, .. } => {
                assert_eq!(rates[0], U256::from(10u64).pow(U256::from(18u64)));
                assert_eq!(rates[1], U256::from(10u64).pow(U256::from(30u64)));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_stableswap_ng_with_dynamic_rates() {
        let oracle_rate = U256::from(1_050_000_000_000_000_000u128); // 1.05 * 10^18
        let state = RawPoolState {
            variant: CurveVariant::StableSwapNG,
            balances: vec![
                U256::from(1_000_000_000_000_000_000_000u128),
                U256::from(1_000_000_000_000_000_000_000u128),
            ],
            token_decimals: vec![18, 18],
            amp: U256::from(150_000u64),
            fee: Some(U256::from(4_000_000u64)),
            offpeg_fee_multiplier: Some(U256::from(20_000_000_000u128)),
            dynamic_rates: Some(vec![
                None,              // computed from decimals
                Some(oracle_rate), // oracle rate
            ]),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        match &pool {
            Pool::StableSwapNG { rates, .. } => {
                assert_eq!(rates[0], U256::from(10u64).pow(U256::from(18u64)));
                assert_eq!(rates[1], oracle_rate);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_stableswap_alend_precision_mul() {
        let state = RawPoolState {
            variant: CurveVariant::StableSwapALend,
            balances: vec![
                U256::from(1_000_000_000_000_000_000_000u128),
                U256::from(1_000_000_000_000_000_000_000u128),
            ],
            token_decimals: vec![18, 18],
            amp: U256::from(10_000u64), // 100 * A_PRECISION(100)
            fee: Some(U256::from(4_000_000u64)),
            offpeg_fee_multiplier: Some(U256::from(20_000_000_000u128)),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        match &pool {
            Pool::StableSwapALend { precision_mul, .. } => {
                // 18 decimals → 10^(18-18) = 1
                assert_eq!(precision_mul[0], U256::from(1u64));
                assert_eq!(precision_mul[1], U256::from(1u64));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_stableswap_meta_virtual_price_rate() {
        let virtual_price = U256::from(1_020_000_000_000_000_000u128); // 1.02
        let state = RawPoolState {
            variant: CurveVariant::StableSwapMeta,
            balances: vec![
                U256::from(1_000_000_000u128),                 // GUSD (2 dec)
                U256::from(1_000_000_000_000_000_000_000u128), // 3CRV LP
            ],
            token_decimals: vec![2, 18],
            amp: U256::from(150_000u64),
            fee: Some(U256::from(4_000_000u64)),
            dynamic_rates: Some(vec![
                None,                // 10^(36-2) = 10^34
                Some(virtual_price), // virtual_price from base pool
            ]),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        match &pool {
            Pool::StableSwapMeta { rates, .. } => {
                assert_eq!(rates[0], U256::from(10u64).pow(U256::from(34u64)));
                assert_eq!(rates[1], virtual_price);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_twocrypto_ng_basic() {
        let state = RawPoolState {
            variant: CurveVariant::TwoCryptoNG,
            balances: vec![
                U256::from(1_000_000_000_000_000_000_000u128),
                U256::from(1_000_000_000_000_000_000_000u128),
            ],
            token_decimals: vec![18, 18],
            amp: U256::from(540_000u64 * 10_000u64), // A=540000 * A_MULTIPLIER
            mid_fee: Some(U256::from(3_000_000u64)),
            out_fee: Some(U256::from(30_000_000u64)),
            fee_gamma: Some(U256::from(500_000_000_000_000u128)),
            d: Some(U256::from(2_000_000_000_000_000_000_000u128)),
            gamma: Some(U256::from(10_000_000_000_000u128)),
            price_scale: Some(vec![U256::from(1_000_000_000_000_000_000u128)]),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        match &pool {
            Pool::TwoCryptoNG { precisions, ann, .. } => {
                assert_eq!(precisions[0], U256::from(1u64)); // 10^(18-18)
                assert_eq!(precisions[1], U256::from(1u64));
                assert_eq!(*ann, state.amp);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_twocrypto_stable_no_gamma() {
        let state = RawPoolState {
            variant: CurveVariant::TwoCryptoStable,
            balances: vec![U256::from(1_000_000_000u128), U256::from(1_000_000_000u128)],
            token_decimals: vec![6, 6],
            amp: U256::from(540_000u64 * 10_000u64),
            mid_fee: Some(U256::from(3_000_000u64)),
            out_fee: Some(U256::from(30_000_000u64)),
            fee_gamma: Some(U256::from(500_000_000_000_000u128)),
            d: Some(U256::from(2_000_000_000u128)),
            // gamma intentionally NOT set
            price_scale: Some(vec![U256::from(1_000_000_000_000_000_000u128)]),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        match &pool {
            Pool::TwoCryptoStable { precisions, .. } => {
                // 6 decimals → 10^(18-6) = 10^12
                assert_eq!(precisions[0], U256::from(10u64).pow(U256::from(12u64)));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_tricrypto_ng_basic() {
        let state = RawPoolState {
            variant: CurveVariant::TriCryptoNG,
            balances: vec![
                U256::from(1_000_000_000u128),               // USDC (6 dec)
                U256::from(50_000_000u128),                  // WBTC (8 dec)
                U256::from(500_000_000_000_000_000_000u128), // WETH (18 dec)
            ],
            token_decimals: vec![6, 8, 18],
            amp: U256::from(1_707_629u64 * 10_000u64),
            mid_fee: Some(U256::from(3_000_000u64)),
            out_fee: Some(U256::from(30_000_000u64)),
            fee_gamma: Some(U256::from(500_000_000_000_000u128)),
            d: Some(U256::from(3_000_000_000_000_000_000_000u128)),
            gamma: Some(U256::from(11_809_167_828_997u128)),
            price_scale: Some(vec![
                U256::from(60_000_000_000_000_000_000_000u128), // BTC price
                U256::from(3_000_000_000_000_000_000_000u128),  // ETH price
            ]),
            ..Default::default()
        };

        let pool = build_pool(&state).unwrap();
        match &pool {
            Pool::TriCryptoNG { precisions, price_scale, .. } => {
                assert_eq!(precisions[0], U256::from(10u64).pow(U256::from(12u64))); // 10^(18-6)
                assert_eq!(precisions[1], U256::from(10u64).pow(U256::from(10u64))); // 10^(18-8)
                assert_eq!(precisions[2], U256::from(1u64)); // 10^(18-18)
                assert_eq!(price_scale.len(), 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_missing_fee_returns_error() {
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV2,
            balances: vec![U256::from(1u64), U256::from(1u64)],
            token_decimals: vec![18, 18],
            amp: U256::from(40_000u64),
            // fee intentionally missing
            ..Default::default()
        };

        let err = match build_pool(&state) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(matches!(err, BuildError::MissingField { field: "fee", .. }));
    }

    #[test]
    fn build_decimals_mismatch_returns_error() {
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV2,
            balances: vec![U256::from(1u64), U256::from(1u64)],
            token_decimals: vec![18], // wrong length
            amp: U256::from(40_000u64),
            fee: Some(U256::from(4_000_000u64)),
            ..Default::default()
        };

        let err = match build_pool(&state) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(matches!(err, BuildError::DecimalsMismatch { .. }));
    }

    #[test]
    fn build_twocrypto_wrong_coin_count() {
        let state = RawPoolState {
            variant: CurveVariant::TwoCryptoNG,
            balances: vec![U256::from(1u64), U256::from(1u64), U256::from(1u64)],
            token_decimals: vec![18, 18, 18],
            amp: U256::from(1u64),
            mid_fee: Some(U256::from(1u64)),
            out_fee: Some(U256::from(1u64)),
            fee_gamma: Some(U256::from(1u64)),
            d: Some(U256::from(1u64)),
            gamma: Some(U256::from(1u64)),
            price_scale: Some(vec![U256::from(1u64)]),
            ..Default::default()
        };

        let err = match build_pool(&state) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(matches!(err, BuildError::WrongCoinCount { expected: 2, actual: 3, .. }));
    }

    #[test]
    fn build_tricrypto_wrong_price_scale_len() {
        let state = RawPoolState {
            variant: CurveVariant::TriCryptoNG,
            balances: vec![U256::from(1u64), U256::from(1u64), U256::from(1u64)],
            token_decimals: vec![6, 8, 18],
            amp: U256::from(1u64),
            mid_fee: Some(U256::from(1u64)),
            out_fee: Some(U256::from(1u64)),
            fee_gamma: Some(U256::from(1u64)),
            d: Some(U256::from(1u64)),
            gamma: Some(U256::from(1u64)),
            price_scale: Some(vec![U256::from(1u64)]), // needs 2, got 1
            ..Default::default()
        };

        let err = match build_pool(&state) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(matches!(err, BuildError::PriceScaleWrongLen { expected: 2, actual: 1 }));
    }

    #[test]
    fn build_ng_without_offpeg_defaults_to_fee_denominator() {
        // v5+ crvUSD factory pools lack offpeg_fee_multiplier
        let state = RawPoolState {
            variant: CurveVariant::StableSwapNG,
            balances: vec![U256::from(1u64), U256::from(1u64)],
            token_decimals: vec![18, 18],
            amp: U256::from(40_000u64),
            fee: Some(U256::from(4_000_000u64)),
            // offpeg_fee_multiplier intentionally missing — defaults to FEE_DENOMINATOR
            ..Default::default()
        };

        let pool = build_pool(&state).expect("should succeed with defaulted offpeg");
        // FEE_DENOMINATOR = 10_000_000_000
        assert_eq!(pool.offpeg_fee_multiplier(), Some(U256::from(10_000_000_000u64)));
    }

    #[test]
    fn build_ng_crvusd_sdai_matches_on_chain() {
        // Real on-chain state for pool 0x1539c2461d7432cc114b0903f1824079BfCA2C92
        // (crvUSD/sDAI, v5.0.0 from crvUSD StableSwap Factory, no offpeg_fee_multiplier)
        let state = RawPoolState {
            variant: CurveVariant::StableSwapNG,
            balances: vec![
                "3219009600398261994"
                    .parse::<U256>()
                    .expect("balance 0"),
                "311156701443769568"
                    .parse::<U256>()
                    .expect("balance 1"),
            ],
            token_decimals: vec![18, 18],
            amp: U256::from(150_000u64),
            fee: Some(U256::from(1_000_000u64)),
            // offpeg_fee_multiplier absent (v5+ pool) — defaults to FEE_DENOMINATOR
            dynamic_rates: Some(vec![
                Some(
                    "1000000000000000000"
                        .parse::<U256>()
                        .expect("rate 0"),
                ),
                Some(
                    "1173627645818786870"
                        .parse::<U256>()
                        .expect("rate 1"),
                ),
            ]),
            ..Default::default()
        };

        let pool = build_pool(&state).expect("should build with defaulted offpeg");
        let dy = pool
            .get_amount_out(0, 1, U256::from(3_219_009_600_398_261u64))
            .expect("swap should succeed");

        // On-chain get_dy(0, 1, 3219009600398261) = 2720818166217034
        let expected = U256::from(2_720_818_166_217_034u64);
        let diff = if dy > expected { dy - expected } else { expected - dy };
        assert!(diff <= U256::from(1u64), "mismatch: got {dy}, expected {expected}, diff {diff}");
    }

    #[test]
    fn build_meta_without_virtual_price_fails() {
        // Meta pool without dynamic_rates → must fail because rates[1]
        // would default to 10^(36-18)=10^18 instead of virtual_price.
        let state = RawPoolState {
            variant: CurveVariant::StableSwapMeta,
            balances: vec![
                U256::from(1_000_000_000u128),
                U256::from(1_000_000_000_000_000_000_000u128),
            ],
            token_decimals: vec![2, 18],
            amp: U256::from(150_000u64),
            fee: Some(U256::from(4_000_000u64)),
            // dynamic_rates NOT provided → should error
            ..Default::default()
        };

        let err = match build_pool(&state) {
            Err(e) => e,
            Ok(_) => panic!("expected MetaMissingVirtualPrice error"),
        };
        assert!(matches!(err, BuildError::MetaMissingVirtualPrice));
    }

    #[test]
    fn build_meta_with_partial_dynamic_rates_missing_vp_fails() {
        // dynamic_rates provided but last coin has None (no virtual_price)
        let state = RawPoolState {
            variant: CurveVariant::StableSwapMeta,
            balances: vec![
                U256::from(1_000_000_000u128),
                U256::from(1_000_000_000_000_000_000_000u128),
            ],
            token_decimals: vec![2, 18],
            amp: U256::from(150_000u64),
            fee: Some(U256::from(4_000_000u64)),
            dynamic_rates: Some(vec![None, None]), // vp not set for last coin
            ..Default::default()
        };

        let err = match build_pool(&state) {
            Err(e) => e,
            Ok(_) => panic!("expected MetaMissingVirtualPrice error"),
        };
        assert!(matches!(err, BuildError::MetaMissingVirtualPrice));
    }

    #[test]
    fn rates_match_fuzz_registry_18_dec() {
        // 18-decimal token → rate = 10^(36-18) = 10^18
        let rates = super::compute_stableswap_rates(&[18], &None);
        assert_eq!(rates[0], U256::from(10u64).pow(U256::from(18u64)));
    }

    #[test]
    fn rates_match_fuzz_registry_6_dec() {
        // 6-decimal token → rate = 10^(36-6) = 10^30
        let rates = super::compute_stableswap_rates(&[6], &None);
        assert_eq!(rates[0], U256::from(10u64).pow(U256::from(30u64)));
    }

    #[test]
    fn rates_match_fuzz_registry_8_dec() {
        // 8-decimal token → rate = 10^(36-8) = 10^28
        let rates = super::compute_stableswap_rates(&[8], &None);
        assert_eq!(rates[0], U256::from(10u64).pow(U256::from(28u64)));
    }

    #[test]
    fn rates_match_fuzz_registry_2_dec() {
        // 2-decimal token (GUSD) → rate = 10^(36-2) = 10^34
        let rates = super::compute_stableswap_rates(&[2], &None);
        assert_eq!(rates[0], U256::from(10u64).pow(U256::from(34u64)));
    }

    #[test]
    fn precisions_match_fuzz_registry() {
        // CryptoSwap: precision = 10^(18 - decimals)
        let precs = super::compute_crypto_precisions(&[6, 8, 18]);
        assert_eq!(precs[0], U256::from(10u64).pow(U256::from(12u64))); // USDC
        assert_eq!(precs[1], U256::from(10u64).pow(U256::from(10u64))); // WBTC
        assert_eq!(precs[2], U256::from(1u64)); // WETH
    }

    #[test]
    fn precision_mul_matches_fuzz_registry() {
        // ALend: precision_mul = 10^(18 - decimals)
        let pm = super::compute_crypto_precisions(&[18, 6]);
        assert_eq!(pm[0], U256::from(1u64));
        assert_eq!(pm[1], U256::from(10u64).pow(U256::from(12u64)));
    }

    #[test]
    fn build_all_11_variants_succeed() {
        // Smoke test: every variant can be built with minimal valid state.
        let stableswap_base = |variant: CurveVariant| -> RawPoolState {
            RawPoolState {
                variant,
                balances: vec![U256::from(1_000_000_000_000_000_000u128); 2],
                token_decimals: vec![18, 18],
                amp: U256::from(40_000u64),
                fee: Some(U256::from(4_000_000u64)),
                ..Default::default()
            }
        };

        // V0, V1, V2
        for v in
            [CurveVariant::StableSwapV0, CurveVariant::StableSwapV1, CurveVariant::StableSwapV2]
        {
            assert!(build_pool(&stableswap_base(v)).is_ok(), "failed for {v}");
        }

        // Meta (needs virtual_price)
        let mut meta = stableswap_base(CurveVariant::StableSwapMeta);
        meta.dynamic_rates = Some(vec![None, Some(U256::from(10u64).pow(U256::from(18u64)))]);
        assert!(build_pool(&meta).is_ok(), "failed for StableSwapMeta");

        // NG
        let mut ng = stableswap_base(CurveVariant::StableSwapNG);
        ng.offpeg_fee_multiplier = Some(U256::from(20_000_000_000u128));
        assert!(build_pool(&ng).is_ok(), "failed for StableSwapNG");

        // ALend
        let mut alend = stableswap_base(CurveVariant::StableSwapALend);
        alend.offpeg_fee_multiplier = Some(U256::from(20_000_000_000u128));
        assert!(build_pool(&alend).is_ok(), "failed for StableSwapALend");

        // CryptoSwap common fields
        let crypto_base = |variant: CurveVariant, n: usize| -> RawPoolState {
            RawPoolState {
                variant,
                balances: vec![U256::from(1_000_000_000_000_000_000u128); n],
                token_decimals: vec![18; n],
                amp: U256::from(540_000u64 * 10_000u64),
                mid_fee: Some(U256::from(3_000_000u64)),
                out_fee: Some(U256::from(30_000_000u64)),
                fee_gamma: Some(U256::from(500_000_000_000_000u128)),
                d: Some(U256::from(2_000_000_000_000_000_000_000u128)),
                gamma: Some(U256::from(10_000_000_000_000u128)),
                price_scale: Some(if n == 2 {
                    vec![U256::from(10u64).pow(U256::from(18u64))]
                } else {
                    vec![U256::from(10u64).pow(U256::from(18u64)); n - 1]
                }),
                // Required for TwoCryptoV1; ignored by every other crypto variant.
                eth_variant: Some(true),
                ..Default::default()
            }
        };

        // TwoCryptoV1, TwoCryptoNG
        for v in [CurveVariant::TwoCryptoV1, CurveVariant::TwoCryptoNG] {
            assert!(build_pool(&crypto_base(v, 2)).is_ok(), "failed for {v}");
        }

        // TwoCryptoStable (no gamma needed)
        let mut tcs = crypto_base(CurveVariant::TwoCryptoStable, 2);
        tcs.gamma = None;
        assert!(build_pool(&tcs).is_ok(), "failed for TwoCryptoStable");

        // TriCryptoV1, TriCryptoNG
        for v in [CurveVariant::TriCryptoV1, CurveVariant::TriCryptoNG] {
            assert!(build_pool(&crypto_base(v, 3)).is_ok(), "failed for {v}");
        }
    }

    //
    // These tests use hardcoded state from Ethereum mainnet at block 24722544.
    // For each pool variant:
    //   1. RawPoolState is populated with real on-chain values
    //   2. build_pool() constructs the Pool
    //   3. get_amount_out() is compared against on-chain get_dy()
    //
    // If any test fails, it means build_pool() constructs a Pool that doesn't
    // match the on-chain contract's behavior — either rates, amp, or fees are wrong.

    fn u(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    #[test]
    fn integration_stableswap_v0_susd() {
        // sUSD pool: DAI(18)/USDC(6)/USDT(6)/sUSD(18), A=256, A_PRECISION=1
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV0,
            balances: vec![
                u("1919848022082255699479"),
                u("1920322445"),
                u("1920171938"),
                u("21038816168255729764832232005"),
            ],
            token_decimals: vec![18, 6, 6, 18],
            amp: U256::from(256u64),
            fee: Some(U256::from(2_000_000u64)),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("19198480220822556994"))
            .unwrap();
        assert_eq!(dy, U256::from(19_009_291u64));
    }

    #[test]
    fn integration_stableswap_v1_3pool() {
        // 3pool: DAI(18)/USDC(6)/USDT(6), A=4000, A_PRECISION=1
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV1,
            balances: vec![
                u("45102835177280382580138407"),
                u("45853975278310"),
                u("72989152672276"),
            ],
            token_decimals: vec![18, 6, 6],
            amp: U256::from(4000u64),
            fee: Some(U256::from(1_500_000u64)),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("451028351772803825801384"))
            .unwrap();
        assert_eq!(dy, u("450961663745"));
    }

    #[test]
    fn integration_stableswap_v2_frax_usdc() {
        // FRAX/USDC: FRAX(18)/USDC(6), A=1500*100=150000
        let state = RawPoolState {
            variant: CurveVariant::StableSwapV2,
            balances: vec![u("6722234569994793202271485"), u("714493991383")],
            token_decimals: vec![18, 6],
            amp: U256::from(150_000u64),
            fee: Some(U256::from(1_000_000u64)),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("67222345699947932022714"))
            .unwrap();
        assert_eq!(dy, u("66561674655"));
    }

    #[test]
    fn integration_stableswap_alend_aave() {
        // Aave: aDAI(18)/aUSDC(6)/aUSDT(6), A=2000*100=200000
        let state = RawPoolState {
            variant: CurveVariant::StableSwapALend,
            balances: vec![u("968991099162993551077367"), u("1012448901351"), u("414282246850")],
            token_decimals: vec![18, 6, 6],
            amp: U256::from(200_000u64),
            fee: Some(U256::from(4_000_000u64)),
            offpeg_fee_multiplier: Some(u("20000000000")),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("9689910991629935510773"))
            .unwrap();
        assert_eq!(dy, u("9686201099"));
    }

    #[test]
    fn integration_stableswap_ng_usde_dai() {
        // USDe/DAI NG: USDe(18)/DAI(18), A=400*100=40000
        let state = RawPoolState {
            variant: CurveVariant::StableSwapNG,
            balances: vec![u("124403796536542495997070"), u("95031311223261676260348")],
            token_decimals: vec![18, 18],
            amp: U256::from(40_000u64),
            fee: Some(U256::from(4_000_000u64)),
            offpeg_fee_multiplier: Some(u("20000000000")),
            dynamic_rates: Some(vec![
                Some(u("1000000000000000000")),
                Some(u("1000000000000000000")),
            ]),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("1244037965365424959970"))
            .unwrap();
        assert_eq!(dy, u("1242635841481792448583"));
    }

    #[test]
    fn integration_stableswap_meta_gusd_3crv() {
        // GUSD/3CRV: GUSD(2)/3CRV(18), A=1000*100=100000, virtual_price from 3pool
        let state = RawPoolState {
            variant: CurveVariant::StableSwapMeta,
            balances: vec![u("59814423"), u("1210422553896217308280639")],
            token_decimals: vec![2, 18],
            amp: U256::from(100_000u64),
            fee: Some(U256::from(4_000_000u64)),
            dynamic_rates: Some(vec![
                None,                           // coin 0: 10^(36-2) = 10^34
                Some(u("1039823717145796146")), // virtual_price
            ]),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("598144"))
            .unwrap();
        assert_eq!(dy, u("5755338887370979902172"));
    }

    #[test]
    fn integration_twocrypto_v1_crv_eth() {
        // CRV/ETH: CRV(18)/WETH(18)
        let state = RawPoolState {
            variant: CurveVariant::TwoCryptoV1,
            balances: vec![u("33389428640766852909"), u("1538654846121127403001612563")],
            token_decimals: vec![18, 18],
            amp: U256::from(400_000u64),
            d: Some(u("3338917956478824050009")),
            gamma: Some(u("145000000000000")),
            price_scale: Some(vec![u("52805053500476")]),
            mid_fee: Some(U256::from(26_000_000u64)),
            out_fee: Some(U256::from(45_000_000u64)),
            fee_gamma: Some(u("230000000000000")),
            eth_variant: Some(true), // CRV/ETH is WETH-paired → ETH solver
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("333894286407668529"))
            .unwrap();
        assert_eq!(dy, u("15024547954512515366680912"));
    }

    #[test]
    fn integration_twocrypto_ng_crvusd_fxn() {
        // crvUSD/FXN: crvUSD(18)/FXN(18)
        let state = RawPoolState {
            variant: CurveVariant::TwoCryptoNG,
            balances: vec![u("575304877931995002539"), u("1286854862507061937737")],
            token_decimals: vec![18, 18],
            amp: U256::from(400_000u64),
            d: Some(u("1309807915207365083258")),
            gamma: Some(u("145000000000000")),
            price_scale: Some(vec![u("578321621819309618")]),
            mid_fee: Some(U256::from(26_000_000u64)),
            out_fee: Some(U256::from(45_000_000u64)),
            fee_gamma: Some(u("230000000000000")),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("5753048779319950025"))
            .unwrap();
        assert_eq!(dy, u("12553693226638615366"));
    }

    #[test]
    fn integration_twocrypto_stable_crvusd_weth() {
        // crvUSD/WETH TwoCryptoStable: crvUSD(18)/WETH(18)
        let state = RawPoolState {
            variant: CurveVariant::TwoCryptoStable,
            balances: vec![u("17087755783041929282185464"), u("13675635632110845893058")],
            token_decimals: vec![18, 18],
            amp: U256::from(25_000u64),
            d: Some(u("53892663239303863640675237")),
            price_scale: Some(vec![u("2783064941591876143844")]),
            mid_fee: Some(U256::from(60_000_000u64)),
            out_fee: Some(U256::from(220_000_000u64)),
            fee_gamma: Some(u("1395000000000000")),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("170877557830419292821854"))
            .unwrap();
        assert_eq!(dy, u("77522288630419592645"));
    }

    #[test]
    fn integration_tricrypto_v1_usdt_wbtc_weth() {
        // tricrypto2: USDT(6)/WBTC(8)/WETH(18)
        let state = RawPoolState {
            variant: CurveVariant::TriCryptoV1,
            balances: vec![u("3687737692530"), u("5185841754"), u("1696614171366863858308")],
            token_decimals: vec![6, 8, 18],
            amp: U256::from(1_707_629u64),
            d: Some(u("11006845200255249518958282")),
            gamma: Some(u("11809167828997")),
            price_scale: Some(vec![u("70578404679338064954709"), u("2156666095129214805267")]),
            mid_fee: Some(U256::from(3_000_000u64)),
            out_fee: Some(U256::from(30_000_000u64)),
            fee_gamma: Some(u("500000000000000")),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("36877376925"))
            .unwrap();
        assert_eq!(dy, U256::from(51_646_866u64));
    }

    #[test]
    fn integration_tricrypto_ng_usdc_wbtc_weth() {
        // tricrypto-ng: USDC(6)/WBTC(8)/WETH(18)
        let state = RawPoolState {
            variant: CurveVariant::TriCryptoNG,
            balances: vec![u("3323859056394"), u("4735137544"), u("1544027711277257449902")],
            token_decimals: vec![6, 8, 18],
            amp: U256::from(1_707_629u64),
            d: Some(u("10010654847128420517547506")),
            gamma: Some(u("11809167828997")),
            price_scale: Some(vec![u("70750968814053384159761"), u("2161000205852311064272")]),
            mid_fee: Some(U256::from(3_000_000u64)),
            out_fee: Some(U256::from(30_000_000u64)),
            fee_gamma: Some(u("500000000000000")),
            ..Default::default()
        };
        let pool = build_pool(&state).unwrap();
        let dy = pool
            .get_amount_out(0, 1, u("33238590563"))
            .unwrap();
        assert_eq!(dy, U256::from(46_932_317u64));
    }
}
