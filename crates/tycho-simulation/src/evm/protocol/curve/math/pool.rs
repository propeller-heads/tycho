//! Unified `Pool` enum covering all Curve pool variants.
//!
//! # Variant identification
//!
//! | Variant          | Era / Template                    | Key traits                                   | Example pools                        |
//! |------------------|-----------------------------------|----------------------------------------------|--------------------------------------|
//! | `StableSwapV0`   | Earliest (2020)                   | A_PRECISION=1, no -1, fee after denorm       | sUSD, Compound, USDT, y, BUSD        |
//! | `StableSwapV1`   | 3pool era (2021)                  | A_PRECISION=1, -1 offset, fee after denorm   | 3pool, ren, sbtc, hbtc               |
//! | `StableSwapV2`   | Base/plain template               | A_PRECISION=100, -1, fee before denorm       | FRAX/USDC, stETH, factory plain      |
//! | `StableSwapALend`| Aave lending template             | A_PRECISION=100, no -1, dynamic fee, PREC_MUL| Aave, sAAVE, IB, aETH               |
//! | `StableSwapNG`   | StableSwap-NG factory             | A_PRECISION=100, -1, dynamic fee, before dnrm| NG plain + meta pools                |
//! | `StableSwapMeta` | Meta pool template                | A_PRECISION=100, -1, fee before denorm       | GUSD, HUSD, factory meta             |
//! | `TwoCryptoV1`    | Legacy CurveCryptoSwap2           | 2-coin, Newton y, price_scale                | CRV/ETH                              |
//! | `TwoCryptoNG`    | twocrypto-ng factory              | 2-coin, Cardano cubic y, price_scale         | crvUSD/FXN, most new 2-coin crypto   |
//! | `TriCryptoV1`    | Legacy tricrypto                  | 3-coin, Newton y, 2 price_scales             | tricrypto2 (USDT/WBTC/WETH)          |
//! | `TriCryptoNG`    | tricrypto-ng factory              | 3-coin, hybrid cubic y, 2 price_scales       | tricrypto-ng (USDC/WBTC/WETH)        |
//!
//! **How to identify which variant to use:**
//! - Pools from a known factory → the factory determines the variant (NG, twocrypto-ng,
//!   tricrypto-ng)
//! - Legacy pools → match by pool address against known deployments
//! - If the pool has `gamma()` → CryptoSwap; otherwise StableSwap
//! - If StableSwap has `stored_rates()` → NG (including v5+ crvUSD factory pools without offpeg)
//!   **Note:** `stored_rates()` return encoding varies by pool version: v5+ returns fixed-size
//!   `uint256[N_COINS]` (no ABI length prefix), v6+ returns dynamic `uint256[]` (with offset +
//!   length prefix). Consumers reading rates on-chain must handle both formats.
//! - If StableSwap has `offpeg_fee_multiplier()` without `stored_rates()` → ALend
//! - If StableSwap has `version()` → NG (v6+ crvUSD factory pools without stored_rates or offpeg)
//! - Check `A_PRECISION` (1 for V0/V1, 100 for V2/Meta/ALend/NG)

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::swap;

/// Error returned by Pool setter methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    /// Field does not exist on this pool variant.
    NotApplicable,
    /// Index exceeds the number of coins or price scales.
    IndexOutOfRange,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApplicable => f.write_str("field not applicable to this pool variant"),
            Self::IndexOutOfRange => f.write_str("index out of range"),
        }
    }
}

impl std::error::Error for PoolError {}

/// All known Curve pool variants with their parameters.
///
/// # State update frequency for indexer/adapter implementations
///
/// Each field falls into one of three categories:
///
/// | Category | When to update | Fields |
/// |----------|---------------|--------|
/// | **Per-block** | Every block (or on every swap event) | `balances`, `rates` (if oracle), `d`, `price_scale` |
/// | **Per-event** | On admin events (rare, days/weeks) | `amp` (during A ramping) |
/// | **Static** | Once at pool creation | `fee`, `offpeg_fee_multiplier`, `precision_mul` |
///
/// For `rates`: plain tokens have static rates (`10^(36-decimals)`), but ERC4626/oracle
/// tokens have dynamic rates that change per-block. Read `stored_rates()` from the pool
/// contract to get current rates for oracle-enabled pools.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pool {
    /// Oldest StableSwap pools (sUSD, Compound, USDT, y, BUSD).
    /// A_PRECISION=1, no -1 offset, fee after denormalize.
    StableSwapV0 { balances: Vec<U256>, rates: Vec<U256>, amp: U256, fee: U256 },

    /// 3pool-era StableSwap (3pool, ren, sbtc, hbtc).
    /// A_PRECISION=1, -1 offset, fee after denormalize.
    StableSwapV1 { balances: Vec<U256>, rates: Vec<U256>, amp: U256, fee: U256 },

    /// Base/plain template StableSwap (FRAX/USDC, stETH, factory plain).
    /// A_PRECISION=100, -1 offset, fee before denormalize.
    StableSwapV2 { balances: Vec<U256>, rates: Vec<U256>, amp: U256, fee: U256 },

    /// Lido stETH/ETH custom StableSwap (0xDC24316b9AE028F1497c275EB9192a3Ea0f67022).
    /// A_PRECISION=100, -1 offset, fee before denormalize. Same shape as `StableSwapV2`, but
    /// `get_d` uses a `+1` divisor guard (see `core::stableswap_steth`).
    StableSwapSTETH { balances: Vec<U256>, rates: Vec<U256>, amp: U256, fee: U256 },

    /// Aave-style lending StableSwap (Aave, sAAVE, IB, aETH).
    /// A_PRECISION=100, no -1 offset, dynamic fee, uses precision_mul.
    StableSwapALend {
        balances: Vec<U256>,
        precision_mul: Vec<U256>,
        amp: U256,
        fee: U256,
        offpeg_fee_multiplier: U256,
    },

    /// StableSwap-NG (plain + meta NG pools).
    /// A_PRECISION=100, -1 offset, dynamic fee, fee before denormalize.
    StableSwapNG {
        balances: Vec<U256>,
        rates: Vec<U256>,
        amp: U256,
        fee: U256,
        offpeg_fee_multiplier: U256,
    },

    /// Meta pool StableSwap (GUSD/3CRV, HUSD, factory meta).
    /// A_PRECISION=100, -1 offset, fee before denormalize.
    /// `rates[1]` must be set to fresh `virtual_price` from base pool.
    StableSwapMeta { balances: Vec<U256>, rates: Vec<U256>, amp: U256, fee: U256 },

    /// Legacy 2-coin CryptoSwap (CurveCryptoSwap2).
    /// Newton iteration for y. `eth_variant` selects the deployed Vyper flavour's `mul2`
    /// grouping in the Newton solver (ETH pool vs non-ETH legacy direct deploy).
    TwoCryptoV1 {
        balances: [U256; 2],
        precisions: [U256; 2],
        price_scale: U256,
        d: U256,
        ann: U256,
        gamma: U256,
        mid_fee: U256,
        out_fee: U256,
        fee_gamma: U256,
        eth_variant: bool,
    },

    /// Next-gen 2-coin CryptoSwap (twocrypto-ng).
    /// Cardano cubic formula for y.
    TwoCryptoNG {
        balances: [U256; 2],
        precisions: [U256; 2],
        price_scale: U256,
        d: U256,
        ann: U256,
        gamma: U256,
        mid_fee: U256,
        out_fee: U256,
        fee_gamma: U256,
    },

    /// TwoCryptoNG pools using StableSwap MATH (v0.1.0).
    /// CryptoSwap interface but StableSwap invariant (gamma ignored).
    /// Detect via: pool.MATH().version() == "v0.1.0"
    TwoCryptoStable {
        balances: [U256; 2],
        precisions: [U256; 2],
        price_scale: U256,
        d: U256,
        ann: U256,
        mid_fee: U256,
        out_fee: U256,
        fee_gamma: U256,
    },

    /// Legacy 3-coin CryptoSwap (tricrypto2).
    /// Newton iteration for y.
    TriCryptoV1 {
        balances: [U256; 3],
        precisions: [U256; 3],
        price_scale: [U256; 2],
        d: U256,
        ann: U256,
        gamma: U256,
        mid_fee: U256,
        out_fee: U256,
        fee_gamma: U256,
    },

    /// Next-gen 3-coin CryptoSwap (tricrypto-ng).
    /// Hybrid cubic + Newton for y.
    TriCryptoNG {
        balances: [U256; 3],
        precisions: [U256; 3],
        price_scale: [U256; 2],
        d: U256,
        ann: U256,
        gamma: U256,
        mid_fee: U256,
        out_fee: U256,
        fee_gamma: U256,
    },
}

impl Pool {
    /// Token balances in native units (wei).
    pub fn balances(&self) -> &[U256] {
        match self {
            Pool::StableSwapV0 { balances, .. } |
            Pool::StableSwapV1 { balances, .. } |
            Pool::StableSwapV2 { balances, .. } |
            Pool::StableSwapSTETH { balances, .. } |
            Pool::StableSwapALend { balances, .. } |
            Pool::StableSwapNG { balances, .. } |
            Pool::StableSwapMeta { balances, .. } => balances,
            Pool::TwoCryptoV1 { balances, .. } |
            Pool::TwoCryptoNG { balances, .. } |
            Pool::TwoCryptoStable { balances, .. } => balances,
            Pool::TriCryptoV1 { balances, .. } | Pool::TriCryptoNG { balances, .. } => balances,
        }
    }

    /// Compute output amount for swapping `dx` of coin `i` into coin `j`.
    ///
    /// Returns `None` if the swap would fail (zero input, out of range, etc.).
    /// Matches Curve's on-chain `get_dy` at wei-level precision.
    ///
    /// # Arguments
    /// * `i` — index of input coin
    /// * `j` — index of output coin
    /// * `dx` — amount of coin `i` to swap (in coin's native decimals)
    pub fn get_amount_out(&self, i: usize, j: usize, dx: U256) -> Option<U256> {
        match self {
            Pool::StableSwapV0 { balances, rates, amp, fee } => {
                swap::stableswap_v0::get_amount_out(balances, rates, *amp, *fee, i, j, dx)
            }
            Pool::StableSwapV1 { balances, rates, amp, fee } => {
                swap::stableswap_v1::get_amount_out(balances, rates, *amp, *fee, i, j, dx)
            }
            Pool::StableSwapV2 { balances, rates, amp, fee } => {
                swap::stableswap_v2::get_amount_out(balances, rates, *amp, *fee, i, j, dx)
            }
            Pool::StableSwapSTETH { balances, rates, amp, fee } => {
                swap::stableswap_steth::get_amount_out(balances, rates, *amp, *fee, i, j, dx)
            }
            Pool::StableSwapALend { balances, precision_mul, amp, fee, offpeg_fee_multiplier } => {
                swap::stableswap_alend::get_amount_out(
                    balances,
                    precision_mul,
                    *amp,
                    *fee,
                    *offpeg_fee_multiplier,
                    i,
                    j,
                    dx,
                )
            }
            Pool::StableSwapNG { balances, rates, amp, fee, offpeg_fee_multiplier } => {
                swap::stableswap_ng::get_amount_out(
                    balances,
                    rates,
                    *amp,
                    *fee,
                    *offpeg_fee_multiplier,
                    i,
                    j,
                    dx,
                )
            }
            Pool::StableSwapMeta { balances, rates, amp, fee } => {
                swap::stableswap_meta::get_amount_out(balances, rates, *amp, *fee, i, j, dx)
            }
            Pool::TwoCryptoV1 {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
                eth_variant,
            } => swap::twocrypto_v1::get_amount_out(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                *eth_variant,
                i,
                j,
                dx,
            ),
            Pool::TwoCryptoNG {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::twocrypto_ng::get_amount_out(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                dx,
            ),
            Pool::TwoCryptoStable {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::twocrypto_stable::get_amount_out(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                dx,
            ),
            Pool::TriCryptoV1 {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::tricrypto_v1::get_amount_out(
                balances,
                precisions,
                price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                dx,
            ),
            Pool::TriCryptoNG {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::tricrypto_ng::get_amount_out(
                balances,
                precisions,
                price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                dx,
            ),
        }
    }

    /// Compute minimum input of coin `i` to receive at least `desired_output` of coin `j`.
    ///
    /// Returns `None` if the swap is not feasible.
    /// Guaranteed: `get_amount_out(i, j, get_amount_in(i, j, dy)) >= dy`.
    pub fn get_amount_in(&self, i: usize, j: usize, desired_output: U256) -> Option<U256> {
        match self {
            Pool::StableSwapV0 { balances, rates, amp, fee } => swap::stableswap_v0::get_amount_in(
                balances,
                rates,
                *amp,
                *fee,
                i,
                j,
                desired_output,
            ),
            Pool::StableSwapV1 { balances, rates, amp, fee } => swap::stableswap_v1::get_amount_in(
                balances,
                rates,
                *amp,
                *fee,
                i,
                j,
                desired_output,
            ),
            Pool::StableSwapV2 { balances, rates, amp, fee } => swap::stableswap_v2::get_amount_in(
                balances,
                rates,
                *amp,
                *fee,
                i,
                j,
                desired_output,
            ),
            Pool::StableSwapSTETH { balances, rates, amp, fee } => {
                swap::stableswap_steth::get_amount_in(
                    balances,
                    rates,
                    *amp,
                    *fee,
                    i,
                    j,
                    desired_output,
                )
            }
            Pool::StableSwapALend { balances, precision_mul, amp, fee, offpeg_fee_multiplier } => {
                swap::stableswap_alend::get_amount_in(
                    balances,
                    precision_mul,
                    *amp,
                    *fee,
                    *offpeg_fee_multiplier,
                    i,
                    j,
                    desired_output,
                )
            }
            Pool::StableSwapNG { balances, rates, amp, fee, offpeg_fee_multiplier } => {
                swap::stableswap_ng::get_amount_in(
                    balances,
                    rates,
                    *amp,
                    *fee,
                    *offpeg_fee_multiplier,
                    i,
                    j,
                    desired_output,
                )
            }
            Pool::StableSwapMeta { balances, rates, amp, fee } => {
                swap::stableswap_meta::get_amount_in(
                    balances,
                    rates,
                    *amp,
                    *fee,
                    i,
                    j,
                    desired_output,
                )
            }
            Pool::TwoCryptoV1 {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
                eth_variant,
            } => swap::twocrypto_v1::get_amount_in(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                *eth_variant,
                i,
                j,
                desired_output,
            ),
            Pool::TwoCryptoNG {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::twocrypto_ng::get_amount_in(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                desired_output,
            ),
            Pool::TwoCryptoStable {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::twocrypto_stable::get_amount_in(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                desired_output,
            ),
            Pool::TriCryptoV1 {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::tricrypto_v1::get_amount_in(
                balances,
                precisions,
                price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                desired_output,
            ),
            Pool::TriCryptoNG {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::tricrypto_ng::get_amount_in(
                balances,
                precisions,
                price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
                desired_output,
            ),
        }
    }

    /// Marginal spot price dy/dx as `(numerator, denominator)`, fee-inclusive.
    ///
    /// For a small swap of coin `i` → coin `j`, the price is approximately
    /// `numerator / denominator` in units of coin `j` per coin `i`.
    pub fn spot_price(&self, i: usize, j: usize) -> Option<(U256, U256)> {
        match self {
            Pool::StableSwapV0 { balances, rates, amp, fee } => {
                swap::stableswap_v0::spot_price(balances, rates, *amp, *fee, i, j)
            }
            Pool::StableSwapV1 { balances, rates, amp, fee } => {
                swap::stableswap_v1::spot_price(balances, rates, *amp, *fee, i, j)
            }
            Pool::StableSwapV2 { balances, rates, amp, fee } => {
                swap::stableswap_v2::spot_price(balances, rates, *amp, *fee, i, j)
            }
            Pool::StableSwapSTETH { balances, rates, amp, fee } => {
                swap::stableswap_steth::spot_price(balances, rates, *amp, *fee, i, j)
            }
            Pool::StableSwapALend { balances, precision_mul, amp, fee, offpeg_fee_multiplier } => {
                swap::stableswap_alend::spot_price(
                    balances,
                    precision_mul,
                    *amp,
                    *fee,
                    *offpeg_fee_multiplier,
                    i,
                    j,
                )
            }
            Pool::StableSwapNG { balances, rates, amp, fee, offpeg_fee_multiplier } => {
                swap::stableswap_ng::spot_price(
                    balances,
                    rates,
                    *amp,
                    *fee,
                    *offpeg_fee_multiplier,
                    i,
                    j,
                )
            }
            Pool::StableSwapMeta { balances, rates, amp, fee } => {
                swap::stableswap_meta::spot_price(balances, rates, *amp, *fee, i, j)
            }
            Pool::TwoCryptoV1 {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
                eth_variant,
            } => swap::twocrypto_v1::spot_price(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                *eth_variant,
                i,
                j,
            ),
            Pool::TwoCryptoNG {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::twocrypto_ng::spot_price(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
            ),
            Pool::TwoCryptoStable {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::twocrypto_stable::spot_price(
                balances,
                precisions,
                *price_scale,
                *d,
                *ann,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
            ),
            Pool::TriCryptoV1 {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::tricrypto_v1::spot_price(
                balances,
                precisions,
                price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
            ),
            Pool::TriCryptoNG {
                balances,
                precisions,
                price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
            } => swap::tricrypto_ng::spot_price(
                balances,
                precisions,
                price_scale,
                *d,
                *ann,
                *gamma,
                *mid_fee,
                *out_fee,
                *fee_gamma,
                i,
                j,
            ),
        }
    }

    /// Amplification parameter (`amp` for StableSwap, `ann` for CryptoSwap).
    pub fn amp(&self) -> U256 {
        match self {
            Pool::StableSwapV0 { amp, .. } |
            Pool::StableSwapV1 { amp, .. } |
            Pool::StableSwapV2 { amp, .. } |
            Pool::StableSwapSTETH { amp, .. } |
            Pool::StableSwapALend { amp, .. } |
            Pool::StableSwapNG { amp, .. } |
            Pool::StableSwapMeta { amp, .. } => *amp,
            Pool::TwoCryptoV1 { ann, .. } |
            Pool::TwoCryptoNG { ann, .. } |
            Pool::TwoCryptoStable { ann, .. } => *ann,
            Pool::TriCryptoV1 { ann, .. } | Pool::TriCryptoNG { ann, .. } => *ann,
        }
    }

    /// Fee for StableSwap variants. Returns `None` for CryptoSwap.
    pub fn fee(&self) -> Option<U256> {
        match self {
            Pool::StableSwapV0 { fee, .. } |
            Pool::StableSwapV1 { fee, .. } |
            Pool::StableSwapV2 { fee, .. } |
            Pool::StableSwapSTETH { fee, .. } |
            Pool::StableSwapALend { fee, .. } |
            Pool::StableSwapNG { fee, .. } |
            Pool::StableSwapMeta { fee, .. } => Some(*fee),
            _ => None,
        }
    }

    /// CryptoSwap fee parameters. Returns `None` for StableSwap.
    pub fn crypto_fees(&self) -> Option<(U256, U256, U256)> {
        match self {
            Pool::TwoCryptoV1 { mid_fee, out_fee, fee_gamma, .. } |
            Pool::TwoCryptoNG { mid_fee, out_fee, fee_gamma, .. } |
            Pool::TwoCryptoStable { mid_fee, out_fee, fee_gamma, .. } |
            Pool::TriCryptoV1 { mid_fee, out_fee, fee_gamma, .. } |
            Pool::TriCryptoNG { mid_fee, out_fee, fee_gamma, .. } => {
                Some((*mid_fee, *out_fee, *fee_gamma))
            }
            _ => None,
        }
    }

    /// Rates for StableSwap variants (V0/V1/V2/NG/Meta).
    /// Returns `None` for ALend (uses precision_mul) and CryptoSwap.
    pub fn rates(&self) -> Option<&[U256]> {
        match self {
            Pool::StableSwapV0 { rates, .. } |
            Pool::StableSwapV1 { rates, .. } |
            Pool::StableSwapV2 { rates, .. } |
            Pool::StableSwapSTETH { rates, .. } |
            Pool::StableSwapNG { rates, .. } |
            Pool::StableSwapMeta { rates, .. } => Some(rates),
            _ => None,
        }
    }

    /// Pool invariant D. Returns `None` for StableSwap (D is computed, not stored).
    pub fn d(&self) -> Option<U256> {
        match self {
            Pool::TwoCryptoV1 { d, .. } |
            Pool::TwoCryptoNG { d, .. } |
            Pool::TwoCryptoStable { d, .. } => Some(*d),
            Pool::TriCryptoV1 { d, .. } | Pool::TriCryptoNG { d, .. } => Some(*d),
            _ => None,
        }
    }

    /// Gamma parameter. Returns `None` for StableSwap and TwoCryptoStable.
    pub fn gamma(&self) -> Option<U256> {
        match self {
            Pool::TwoCryptoV1 { gamma, .. } | Pool::TwoCryptoNG { gamma, .. } => Some(*gamma),
            Pool::TriCryptoV1 { gamma, .. } | Pool::TriCryptoNG { gamma, .. } => Some(*gamma),
            _ => None,
        }
    }

    /// Precision multipliers for ALend. Returns `None` for all other variants.
    pub fn precision_mul(&self) -> Option<&[U256]> {
        match self {
            Pool::StableSwapALend { precision_mul, .. } => Some(precision_mul),
            _ => None,
        }
    }

    /// Precisions for CryptoSwap variants. Returns `None` for StableSwap.
    pub fn precisions(&self) -> Option<&[U256]> {
        match self {
            Pool::TwoCryptoV1 { precisions, .. } |
            Pool::TwoCryptoNG { precisions, .. } |
            Pool::TwoCryptoStable { precisions, .. } => Some(precisions),
            Pool::TriCryptoV1 { precisions, .. } | Pool::TriCryptoNG { precisions, .. } => {
                Some(precisions)
            }
            _ => None,
        }
    }

    /// Offpeg fee multiplier. Returns `None` for variants without dynamic fee.
    pub fn offpeg_fee_multiplier(&self) -> Option<U256> {
        match self {
            Pool::StableSwapNG { offpeg_fee_multiplier, .. } |
            Pool::StableSwapALend { offpeg_fee_multiplier, .. } => Some(*offpeg_fee_multiplier),
            _ => None,
        }
    }

    /// Price scale(s). Returns `None` for StableSwap.
    pub fn price_scale(&self) -> Option<&[U256]> {
        match self {
            Pool::TwoCryptoV1 { price_scale, .. } |
            Pool::TwoCryptoNG { price_scale, .. } |
            Pool::TwoCryptoStable { price_scale, .. } => std::slice::from_ref(price_scale).into(),
            Pool::TriCryptoV1 { price_scale, .. } | Pool::TriCryptoNG { price_scale, .. } => {
                Some(price_scale)
            }
            _ => None,
        }
    }

    /// Set balance for coin at `index`. **Per-block update.**
    pub fn set_balance(&mut self, index: usize, value: U256) -> Result<(), PoolError> {
        let bal = match self {
            Pool::StableSwapV0 { balances, .. } |
            Pool::StableSwapV1 { balances, .. } |
            Pool::StableSwapV2 { balances, .. } |
            Pool::StableSwapSTETH { balances, .. } |
            Pool::StableSwapALend { balances, .. } |
            Pool::StableSwapNG { balances, .. } |
            Pool::StableSwapMeta { balances, .. } => balances.get_mut(index),
            Pool::TwoCryptoV1 { balances, .. } |
            Pool::TwoCryptoNG { balances, .. } |
            Pool::TwoCryptoStable { balances, .. } => balances.get_mut(index),
            Pool::TriCryptoV1 { balances, .. } | Pool::TriCryptoNG { balances, .. } => {
                balances.get_mut(index)
            }
        };
        *bal.ok_or(PoolError::IndexOutOfRange)? = value;
        Ok(())
    }

    /// Set rate for coin at `index`. **Per-block for oracle tokens, static otherwise.**
    ///
    /// Applies to StableSwap variants with rates (V0/V1/V2/NG/Meta).
    pub fn set_rate(&mut self, index: usize, value: U256) -> Result<(), PoolError> {
        match self {
            Pool::StableSwapV0 { rates, .. } |
            Pool::StableSwapV1 { rates, .. } |
            Pool::StableSwapV2 { rates, .. } |
            Pool::StableSwapSTETH { rates, .. } |
            Pool::StableSwapNG { rates, .. } |
            Pool::StableSwapMeta { rates, .. } => {
                *rates
                    .get_mut(index)
                    .ok_or(PoolError::IndexOutOfRange)? = value;
                Ok(())
            }
            _ => Err(PoolError::NotApplicable),
        }
    }

    /// Set pool invariant D. **Per-block update for CryptoSwap.**
    pub fn set_d(&mut self, value: U256) -> Result<(), PoolError> {
        match self {
            Pool::TwoCryptoV1 { d, .. } |
            Pool::TwoCryptoNG { d, .. } |
            Pool::TwoCryptoStable { d, .. } => {
                *d = value;
                Ok(())
            }
            Pool::TriCryptoV1 { d, .. } | Pool::TriCryptoNG { d, .. } => {
                *d = value;
                Ok(())
            }
            _ => Err(PoolError::NotApplicable),
        }
    }

    /// Set price_scale at `index`. **Per-block update for CryptoSwap.**
    pub fn set_price_scale(&mut self, index: usize, value: U256) -> Result<(), PoolError> {
        match self {
            Pool::TwoCryptoV1 { price_scale, .. } |
            Pool::TwoCryptoNG { price_scale, .. } |
            Pool::TwoCryptoStable { price_scale, .. } => {
                if index != 0 {
                    return Err(PoolError::IndexOutOfRange);
                }
                *price_scale = value;
                Ok(())
            }
            Pool::TriCryptoV1 { price_scale, .. } | Pool::TriCryptoNG { price_scale, .. } => {
                *price_scale
                    .get_mut(index)
                    .ok_or(PoolError::IndexOutOfRange)? = value;
                Ok(())
            }
            _ => Err(PoolError::NotApplicable),
        }
    }

    /// Set amplification parameter. **Semi-static (changes during A ramping).**
    ///
    /// Applicable to all variants.
    pub fn set_amp(&mut self, value: U256) {
        match self {
            Pool::StableSwapV0 { amp, .. } |
            Pool::StableSwapV1 { amp, .. } |
            Pool::StableSwapV2 { amp, .. } |
            Pool::StableSwapSTETH { amp, .. } |
            Pool::StableSwapALend { amp, .. } |
            Pool::StableSwapNG { amp, .. } |
            Pool::StableSwapMeta { amp, .. } => *amp = value,
            Pool::TwoCryptoV1 { ann, .. } |
            Pool::TwoCryptoNG { ann, .. } |
            Pool::TwoCryptoStable { ann, .. } => *ann = value,
            Pool::TriCryptoV1 { ann, .. } | Pool::TriCryptoNG { ann, .. } => *ann = value,
        }
    }

    /// Set gamma parameter. **Semi-static (changes during gamma ramping).**
    ///
    /// Returns `Err` for StableSwap and TwoCryptoStable variants.
    pub fn set_gamma(&mut self, value: U256) -> Result<(), PoolError> {
        match self {
            Pool::TwoCryptoV1 { gamma, .. } | Pool::TwoCryptoNG { gamma, .. } => {
                *gamma = value;
                Ok(())
            }
            Pool::TriCryptoV1 { gamma, .. } | Pool::TriCryptoNG { gamma, .. } => {
                *gamma = value;
                Ok(())
            }
            _ => Err(PoolError::NotApplicable),
        }
    }
}
