//! [`CurveState`] — a hybrid Curve pool: pure-Rust quote math (`curve_math::Pool`) over state read
//! from the locally indexed VM storage.
use std::any::Any;

use alloy::primitives::{Address as AlloyAddress, U256};
use num_bigint::{BigUint, ToBigUint};
use serde::{Deserialize, Serialize};
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
        curve::{
            adapter::CurveVariant,
            math::{Pool, SwapToPriceError},
            vm,
        },
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    },
};

/// Curve fee denominator (`10^10`); both StableSwap `fee` and CryptoSwap `mid_fee` use it.
const FEE_DENOMINATOR: f64 = 1e10;
/// Representative gas cost of a StableSwap exchange.
const STABLESWAP_GAS: u64 = 150_000;
/// Representative gas cost of a CryptoSwap exchange (heavier math + price oracle update).
const CRYPTOSWAP_GAS: u64 = 350_000;

/// A single Curve pool quoted via `curve_math`.
///
/// `tokens` and `decimals` are ordered to match the pool's coin indices, so a token address maps
/// directly to a `curve_math` coin index. State (`pool`) is rebuilt from the VM on every
/// `delta_transition`.
///
/// Multi-hop limitation: the state returned by [`ProtocolSim::get_amount_out`] updates coin
/// balances only, holding `D` and `price_scale` fixed. This is exact for StableSwap (which
/// recomputes `D` from balances on every quote), but a route that re-quotes the *same* CryptoSwap
/// pool sees an approximation on the second hop, because CryptoSwap caches `D` and would update it
/// (via `tweak_price`) after an on-chain exchange.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurveState {
    /// Pool contract address (the Tycho component id).
    pool_address: Bytes,
    /// Coin addresses in pool index order.
    tokens: Vec<Bytes>,
    /// Coin decimals in pool index order.
    decimals: Vec<u8>,
    /// Resolved math variant.
    variant: CurveVariant,
    /// Constructed math pool used for quoting.
    pool: Pool,
}

impl CurveState {
    /// Construct a `CurveState` from a resolved variant and a built `curve_math::Pool`.
    pub(super) fn new(
        pool_address: Bytes,
        tokens: Vec<Bytes>,
        decimals: Vec<u8>,
        variant: CurveVariant,
        pool: Pool,
    ) -> Self {
        Self { pool_address, tokens, decimals, variant, pool }
    }

    fn coin_index(&self, token: &Bytes) -> Result<usize, SimulationError> {
        self.tokens
            .iter()
            .position(|t| t == token)
            .ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!("token {token} is not a coin of curve pool {}", self.pool_address),
                    None,
                )
            })
    }

    fn is_crypto(&self) -> bool {
        matches!(
            self.variant,
            CurveVariant::TwoCryptoV1 |
                CurveVariant::TwoCryptoNG |
                CurveVariant::TwoCryptoStable |
                CurveVariant::TriCryptoV1 |
                CurveVariant::TriCryptoNG
        )
    }

    fn gas_estimate(&self) -> u64 {
        if self.is_crypto() {
            CRYPTOSWAP_GAS
        } else {
            STABLESWAP_GAS
        }
    }

    /// Moves a StableSwap pool's marginal price to `target` with the native solver, falling back
    /// to the numerical search when the solver cannot handle the pool state.
    fn stable_swap_to_target_price(
        &self,
        params: &QueryPoolSwapParams,
        target: &Price,
        tolerance: f64,
    ) -> Result<PoolSwap, SimulationError> {
        let token_in = params.token_in();
        let token_out = params.token_out();
        let i = self.coin_index(&token_in.address)?;
        let j = self.coin_index(&token_out.address)?;
        if target.numerator.bits() > 256 || target.denominator.bits() > 256 {
            // The native solver works on U256 fractions; oversized targets go numerical.
            return crate::evm::query_pool_swap::query_pool_swap(self, params);
        }
        let target_num = biguint_to_u256(&target.numerator);
        let target_den = biguint_to_u256(&target.denominator);

        match self
            .pool
            .swap_to_price(i, j, target_num, target_den, tolerance)
        {
            Ok(dx) => {
                if dx.is_zero() {
                    return Ok(PoolSwap::new(BigUint::ZERO, BigUint::ZERO, self.clone_box(), None));
                }
                let result = self.get_amount_out(u256_to_biguint(dx), token_in, token_out)?;
                Ok(PoolSwap::new(u256_to_biguint(dx), result.amount, result.new_state, None))
            }
            Err(SwapToPriceError::TargetAboveSpot) => Err(SimulationError::InvalidInput(
                format!(
                    "Target price above spot price for curve pool {pool}",
                    pool = self.pool_address
                ),
                None,
            )),
            Err(SwapToPriceError::TargetBelowLimit) => Err(SimulationError::InvalidInput(
                format!(
                    "Target price below reachable limit for curve pool {pool}",
                    pool = self.pool_address
                ),
                None,
            )),
            Err(SwapToPriceError::UnsupportedVariant | SwapToPriceError::MathFailed) => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
        }
    }
}

#[typetag::serde]
impl ProtocolSim for CurveState {
    fn fee(&self) -> f64 {
        let fee = self.pool.fee().or_else(|| {
            self.pool
                .crypto_fees()
                .map(|(mid, _, _)| mid)
        });
        fee.and_then(|f| u256_to_f64(f).ok())
            .map(|f| f / FEE_DENOMINATOR)
            .unwrap_or(0.0)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let i = self.coin_index(&base.address)?;
        let j = self.coin_index(&quote.address)?;
        let (numerator, denominator) = self
            .pool
            .spot_price(i, j)
            .ok_or_else(|| {
                SimulationError::RecoverableError(format!(
                    "curve spot price unavailable for {}",
                    self.pool_address
                ))
            })?;
        // curve_math returns dy/dx (quote per base) in native token units and fee-inclusive;
        // rescale to human units of quote per 1 base.
        let ratio = u256_to_f64(numerator)? / u256_to_f64(denominator)?;
        let decimal_adjustment = 10f64.powi(base.decimals as i32 - quote.decimals as i32);
        Ok(ratio * decimal_adjustment)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let i = self.coin_index(&token_in.address)?;
        let j = self.coin_index(&token_out.address)?;
        let dx = biguint_to_u256(&amount_in);

        let dy = self
            .pool
            .get_amount_out(i, j, dx)
            .ok_or_else(|| {
                SimulationError::RecoverableError(format!(
                    "curve get_amount_out failed for {}",
                    self.pool_address
                ))
            })?;

        let mut new_pool = self.pool.clone();
        let (balance_in, balance_out) = {
            let balances = new_pool.balances();
            (balances[i], balances[j])
        };
        // Apply the swap to coin balances for multi-hop routing. Stored D / price_scale are kept
        // as-is (the invariant is preserved across a swap; price_scale only moves on rebalancing),
        // which is an approximation if the same crypto pool is hit twice within one route.
        new_pool
            .set_balance(i, balance_in + dx)
            .map_err(|e| SimulationError::FatalError(format!("curve set_balance failed: {e}")))?;
        new_pool
            .set_balance(j, balance_out.saturating_sub(dy))
            .map_err(|e| SimulationError::FatalError(format!("curve set_balance failed: {e}")))?;

        let new_state = Self { pool: new_pool, ..self.clone() };
        Ok(GetAmountOutResult::new(
            u256_to_biguint(dy),
            self.gas_estimate()
                .to_biguint()
                .expect("u64 fits in BigUint"),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let i = self.coin_index(&sell_token)?;
        let j = self.coin_index(&buy_token)?;
        let (balance_in, balance_out) = {
            let balances = self.pool.balances();
            (balances[i], balances[j])
        };
        if balance_in.is_zero() || balance_out.is_zero() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        // Soft limit: cap the input at the pool's own balance of the sell token. Beyond this the
        // solver math becomes unreliable and output approaches the available reserve.
        let max_out_reserve = balance_out.saturating_sub(U256::from(1));
        let max_out = self
            .pool
            .get_amount_out(i, j, balance_in)
            .ok_or_else(|| {
                SimulationError::RecoverableError(format!(
                    "curve get_limits: solver failed at max input for {}",
                    self.pool_address
                ))
            })?
            .min(max_out_reserve);
        Ok((u256_to_biguint(balance_in), u256_to_biguint(max_out)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &std::collections::HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("Infallible");
        let pool_address = AlloyAddress::from_slice(self.pool_address.as_ref());
        self.pool = vm::decode_from_vm(&engine, &pool_address, self.variant, &self.decimals)
            .map_err(TransitionError::SimulationError)?;
        Ok(())
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
            SwapConstraint::PoolTargetPrice { target, tolerance, .. } => {
                if self.is_crypto() {
                    // CryptoSwap price dynamics (price_scale, gamma) have no native solver yet.
                    return crate::evm::query_pool_swap::query_pool_swap(self, params);
                }
                self.stable_swap_to_target_price(params, target, *tolerance)
            }
        }
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
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_traits::ToPrimitive;
    use rstest::rstest;
    use tycho_common::{
        models::Chain,
        simulation::protocol_sim::{Price, QueryPoolSwapParams, SwapConstraint},
    };

    use super::*;

    const WAD: u128 = 1_000_000_000_000_000_000;
    const RATE_6_DEC: u128 = 1_000_000_000_000_000_000_000_000_000_000;

    fn token(index: u8, decimals: u32) -> Token {
        let address =
            Bytes::from_str(&format!("0x00000000000000000000000000000000000000{index:02x}"))
                .expect("valid address");
        Token::new(
            &address,
            &format!("T{index}"),
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn curve_state(pool: Pool, variant: CurveVariant, decimals: Vec<u8>) -> CurveState {
        let tokens: Vec<Bytes> = (0..decimals.len())
            .map(|k| token(k as u8, decimals[k] as u32).address)
            .collect();
        CurveState::new(
            Bytes::from_str("0x00000000000000000000000000000000000000ff").expect("valid address"),
            tokens,
            decimals,
            variant,
            pool,
        )
    }

    fn v1_two_coin_state() -> (CurveState, Token, Token) {
        let pool = Pool::StableSwapV1 {
            balances: vec![U256::from(50_000_000u128 * WAD), U256::from(48_000_000u128 * WAD)],
            rates: vec![U256::from(WAD), U256::from(WAD)],
            amp: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
        };
        (curve_state(pool, CurveVariant::StableSwapV1, vec![18, 18]), token(0, 18), token(1, 18))
    }

    fn v1_three_coin_mixed_state() -> (CurveState, Token, Token) {
        // 3pool state at block 24669924: DAI (18 dec) in, USDC (6 dec) out.
        let pool = Pool::StableSwapV1 {
            balances: vec![
                U256::from(63_975_337_809_806_329_031_583_135u128),
                U256::from(61_219_263_170_093u128),
                U256::from(37_832_425_459_809u128),
            ],
            rates: vec![U256::from(WAD), U256::from(RATE_6_DEC), U256::from(RATE_6_DEC)],
            amp: U256::from(4000u64),
            fee: U256::from(1_500_000u64),
        };
        (curve_state(pool, CurveVariant::StableSwapV1, vec![18, 6, 6]), token(0, 18), token(1, 6))
    }

    fn v1_three_coin_mixed_state_reverse() -> (CurveState, Token, Token) {
        let (state, token_out, token_in) = v1_three_coin_mixed_state();
        (state, token_in, token_out)
    }

    fn ng_dynamic_fee_state() -> (CurveState, Token, Token) {
        let pool = Pool::StableSwapNG {
            balances: vec![U256::from(1_500_000u128 * WAD), U256::from(700_000u128 * WAD)],
            rates: vec![U256::from(WAD), U256::from(WAD)],
            amp: U256::from(40_000u64),
            fee: U256::from(4_000_000u64),
            offpeg_fee_multiplier: U256::from(20_000_000_000u64),
        };
        (curve_state(pool, CurveVariant::StableSwapNG, vec![18, 18]), token(0, 18), token(1, 18))
    }

    fn meta_state() -> (CurveState, Token, Token) {
        let pool = Pool::StableSwapMeta {
            balances: vec![U256::from(500_000u128 * WAD), U256::from(480_000u128 * WAD)],
            rates: vec![U256::from(WAD), U256::from(1_030_000_000_000_000_000u128)],
            amp: U256::from(50_000u64),
            fee: U256::from(4_000_000u64),
        };
        (curve_state(pool, CurveVariant::StableSwapMeta, vec![18, 18]), token(0, 18), token(1, 18))
    }

    fn two_crypto_ng_state() -> (CurveState, Token, Token) {
        let wad = U256::from(WAD);
        let pool = Pool::TwoCryptoNG {
            balances: [U256::from(5000u64) * wad, U256::from(5000u64) * wad],
            precisions: [U256::from(1u64), U256::from(1u64)],
            price_scale: wad,
            d: U256::from(10000u64) * wad,
            ann: U256::from(540_000u64) * U256::from(10_000u64),
            gamma: U256::from(11_809_167_828_997u64),
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(230_000_000_000_000u64),
        };
        (curve_state(pool, CurveVariant::TwoCryptoNG, vec![18, 18]), token(0, 18), token(1, 18))
    }

    /// Builds a raw-atomic-unit `Price` for a decimal-scaled f64 price, as callers would.
    fn to_price(price_f64: f64, token_in: &Token, token_out: &Token) -> Price {
        let decimal_adj = 10_f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
        let price_no_decimals = price_f64 / decimal_adj;
        Price::new(BigUint::from((price_no_decimals * 1e18) as u128), BigUint::from(10u128.pow(18)))
    }

    fn target_price_params(
        token_in: &Token,
        token_out: &Token,
        target: Price,
        tolerance: f64,
    ) -> QueryPoolSwapParams {
        QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance,
                min_amount_in: None,
                max_amount_in: None,
            },
        )
    }

    /// Native and numerical searches must both land inside the tolerance band; the native
    /// result is designed to stay in the lower half of it.
    #[rstest]
    #[case::v1_two_coin_shallow(v1_two_coin_state(), 0.9999)]
    #[case::v1_two_coin_mid(v1_two_coin_state(), 0.999)]
    #[case::v1_two_coin_deep(v1_two_coin_state(), 0.99)]
    #[case::v1_mixed_decimals_18_to_6(v1_three_coin_mixed_state(), 0.99)]
    #[case::v1_mixed_decimals_6_to_18(v1_three_coin_mixed_state_reverse(), 0.99)]
    #[case::ng_dynamic_fee(ng_dynamic_fee_state(), 0.99)]
    #[case::ng_dynamic_fee_mid(ng_dynamic_fee_state(), 0.999)]
    #[case::meta_virtual_price(meta_state(), 0.99)]
    #[case::meta_virtual_price_mid(meta_state(), 0.999)]
    fn native_matches_numerical(
        #[case] setup: (CurveState, Token, Token),
        #[case] multiplier: f64,
    ) {
        let (state, token_in, token_out) = setup;
        let tolerance = 0.001;
        let spot = state
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let target_f64 = spot * multiplier;
        let params = target_price_params(
            &token_in,
            &token_out,
            to_price(target_f64, &token_in, &token_out),
            tolerance,
        );

        let native = state
            .query_pool_swap(&params)
            .expect("native query_pool_swap");
        let numerical = crate::evm::query_pool_swap::query_pool_swap(&state, &params)
            .expect("numerical query_pool_swap");

        // The native solver targets the lower half of the band; the numerical search is only
        // best-effort (it may exhaust its iterations and return the closest point at or above
        // the target), so it gets a looser sanity band.
        for (label, swap, band) in
            [("native", &native, tolerance / 2.0), ("numerical", &numerical, 5.0 * tolerance)]
        {
            assert!(swap.amount_in() > &BigUint::ZERO, "{label} amount_in should be > 0");
            let new_spot = swap
                .new_state()
                .spot_price(&token_in, &token_out)
                .expect("post-swap spot");
            let error = (new_spot - target_f64) / target_f64;
            assert!(
                error >= -1e-12,
                "{label} post-swap spot {new_spot} fell below target {target_f64}"
            );
            assert!(
                error <= band,
                "{label} post-swap spot {new_spot} outside band of target {target_f64}: {error}"
            );
        }

        // Amount agreement is only meaningful when the tolerance band is small relative to the
        // price move: StableSwap curves are so flat that a 10bps band around a 1bp move admits
        // wildly different input amounts.
        if (1.0 - multiplier) >= 5.0 * tolerance {
            let native_in = native
                .amount_in()
                .to_f64()
                .expect("native f64");
            let numerical_in = numerical
                .amount_in()
                .to_f64()
                .expect("numerical f64");
            let ratio = native_in / numerical_in;
            assert!(
                (0.5..=2.0).contains(&ratio),
                "native amount {native_in} vs numerical {numerical_in} differ too much"
            );
        }
    }

    /// CryptoSwap variants must delegate to the numerical helper instead of returning the
    /// default "not implemented" FatalError.
    ///
    /// The numerical search itself cannot currently satisfy a target on Curve crypto pools:
    /// `get_amount_out` retains the swap fee in the balances while keeping the stored `D`, and
    /// the crypto variants price via a finite-difference quote against that stored `D`, so the
    /// post-swap spot price absorbs the whole retained fee and the helper rejects every target
    /// as unreachable. Until the crypto `D` update (`tweak_price`) is ported, the delegation
    /// surfaces the helper's InvalidInput instead of hitting the target.
    #[test]
    fn crypto_variant_delegates_to_numerical() {
        let (state, token_in, token_out) = two_crypto_ng_state();
        let spot = state
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let target_f64 = spot * 0.995;
        let params = target_price_params(
            &token_in,
            &token_out,
            to_price(target_f64, &token_in, &token_out),
            0.001,
        );

        let result = state.query_pool_swap(&params);
        match result {
            Ok(swap) => {
                // If the numerical helper ever succeeds here, the delegation must have produced
                // a real swap.
                assert!(swap.amount_in() > &BigUint::ZERO);
            }
            Err(SimulationError::InvalidInput(msg, _)) => {
                assert!(
                    msg.contains("limit"),
                    "expected the numerical helper's reachability error, got: {msg}"
                );
            }
            Err(other) => panic!("crypto pools must delegate to the numerical search: {other:?}"),
        }
    }

    #[test]
    fn trade_limit_price_delegates_to_numerical() {
        let (state, token_in, token_out) = v1_two_coin_state();
        let spot = state
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let limit_f64 = spot * 0.999;
        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: to_price(limit_f64, &token_in, &token_out),
                tolerance: 0.001,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let swap = state
            .query_pool_swap(&params)
            .expect("trade limit query_pool_swap");
        assert!(swap.amount_in() > &BigUint::ZERO);
        assert!(swap.amount_out() > &BigUint::ZERO);
        let trade_price = swap
            .amount_out()
            .to_f64()
            .expect("out f64") /
            swap.amount_in()
                .to_f64()
                .expect("in f64");
        assert!(trade_price >= limit_f64, "trade price {trade_price} violates limit {limit_f64}");
    }

    #[test]
    fn target_above_spot_errors() {
        let (state, token_in, token_out) = v1_two_coin_state();
        let spot = state
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let params = target_price_params(
            &token_in,
            &token_out,
            to_price(spot * 1.01, &token_in, &token_out),
            0.001,
        );

        let result = state.query_pool_swap(&params);
        assert!(
            matches!(result, Err(SimulationError::InvalidInput(_, _))),
            "target above spot should be InvalidInput, got {result:?}"
        );
    }

    #[test]
    fn target_below_limit_errors() {
        let (state, token_in, token_out) = v1_two_coin_state();
        let spot = state
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let params = target_price_params(
            &token_in,
            &token_out,
            to_price(spot * 0.01, &token_in, &token_out),
            0.001,
        );

        let result = state.query_pool_swap(&params);
        assert!(
            matches!(result, Err(SimulationError::InvalidInput(_, _))),
            "target below limit should be InvalidInput, got {result:?}"
        );
    }
}
