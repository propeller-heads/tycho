//! [`CurveState`] — a hybrid Curve pool: pure-Rust quote math (`curve_math::Pool`) over state read
//! from the locally indexed VM storage.
use std::any::Any;

use alloy::primitives::{Address as AlloyAddress, U256};
use num_bigint::{BigUint, ToBigUint};
use serde::{Deserialize, Serialize};
use tracing::debug;
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

use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::{
            curve::{
                adapter::{build_pool, CurveVariant},
                math::Pool,
                swap_to_price::{exchange, swap_to_price, SwapToPriceError},
                underlying::BasePool,
                vm,
            },
            u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        },
    },
    utils::decimals_exponent,
};

/// Curve fee denominator (`10^10`); both StableSwap `fee` and CryptoSwap `mid_fee` use it.
const FEE_DENOMINATOR: f64 = 1e10;
/// Representative gas cost of a StableSwap exchange.
const STABLESWAP_GAS: u64 = 150_000;
/// Representative gas cost of a CryptoSwap exchange (heavier math + price oracle update).
const CRYPTOSWAP_GAS: u64 = 350_000;
/// Gas cost of a metapool `exchange_underlying`: the metapool's exchange plus a single-coin
/// deposit into or withdrawal from the base pool. `CurveExecutor.swap` on the USD1 metapool used
/// 341,457 gas to a base coin and 336,193 from one, on a mainnet fork at block 26434212.
const UNDERLYING_GAS: u64 = 340_000;
/// The share of the sell-side reserve the underlying spot price is probed at.
const SPOT_PROBE_DIVISOR: u64 = 1_000_000;
/// How often an underlying limit halves its input while the swap does not fit the reserves.
const LIMIT_HALVINGS: usize = 32;

/// How a pair of tokens trades through a pool.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// Between two of the pool's own coins, by coin index.
    Coins { i: usize, j: usize },
    /// From the metapool's coin to a coin of its base pool, by base pool coin index.
    ToBase { coin: usize },
    /// From a coin of the base pool to the metapool's coin, by base pool coin index.
    FromBase { coin: usize },
}

/// The result of an underlying swap: what it pays, and both pools afterwards.
struct UnderlyingSwap {
    amount: U256,
    pool: Pool,
    base_pool: BasePool,
}

/// A single Curve pool quoted via `curve_math`.
///
/// `tokens` and `decimals` are ordered to match the pool's coin indices, so a token address maps
/// directly to a `curve_math` coin index. State (`pool`) is rebuilt from the VM on every
/// `delta_transition`.
///
/// StableSwap exchanges update pricing balances net of admin fees and recompute `D` on the
/// next quote. CryptoSwap updates balances only, holding `D` and `price_scale` fixed: re-quoting
/// the same CryptoSwap pool is approximate because execution updates these via `tweak_price`.
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
    /// Admin share scaled by 1e10. Absent in old snapshots; legacy StableSwap must refresh.
    #[serde(default)]
    admin_fee: Option<U256>,
    /// The base pool of a metapool whose underlying swaps this state quotes.
    #[serde(default)]
    base_pool: Option<BasePool>,
}

impl CurveState {
    /// Construct a pool with its admin fee share (scaled by 1e10). Legacy StableSwap requires
    /// `Some(admin_fee)` to quote; NG uses its fixed 50% share and CryptoSwap ignores this field.
    pub fn new(
        pool_address: Bytes,
        tokens: Vec<Bytes>,
        decimals: Vec<u8>,
        variant: CurveVariant,
        pool: Pool,
        admin_fee: Option<U256>,
    ) -> Self {
        Self { pool_address, tokens, decimals, variant, pool, admin_fee, base_pool: None }
    }

    /// Makes this metapool quote swaps between its coin and the coins of `base_pool`.
    pub(super) fn with_base_pool(mut self, base_pool: BasePool) -> Self {
        self.base_pool = Some(base_pool);
        self
    }

    /// Resolves how `token_in` trades for `token_out`.
    ///
    /// A pair of the pool's own coins trades directly. A metapool with a base pool also trades its
    /// coin against the base pool's coins. The base pool's LP token against a base coin, and two
    /// base coins against each other, are refused: the base pool component serves them.
    fn route(&self, token_in: &Bytes, token_out: &Bytes) -> Result<Route, SimulationError> {
        let position = |token: &Bytes| {
            self.tokens
                .iter()
                .position(|coin| coin == token)
        };
        let (coin_in, coin_out) = (position(token_in), position(token_out));
        if let (Some(i), Some(j)) = (coin_in, coin_out) {
            return Ok(Route::Coins { i, j });
        }
        if let Some(base) = &self.base_pool {
            if coin_in == Some(base.meta_coin) {
                if let Some(coin) = base.coin_index(token_out) {
                    return Ok(Route::ToBase { coin });
                }
            }
            if coin_out == Some(base.meta_coin) {
                if let Some(coin) = base.coin_index(token_in) {
                    return Ok(Route::FromBase { coin });
                }
            }
        }
        Err(SimulationError::InvalidInput(
            format!("curve pool {} does not swap {token_in} for {token_out}", self.pool_address),
            None,
        ))
    }

    /// Whether `token` is one of the pool's coins or of its base pool's coins.
    fn holds_token(&self, token: &Bytes) -> bool {
        self.tokens.contains(token) ||
            self.base_pool
                .as_ref()
                .is_some_and(|base| base.coin_index(token).is_some())
    }

    /// Quotes an underlying swap of `dx`. `None` when the swap does not fit the reserves.
    fn swap_underlying(
        &self,
        route: Route,
        dx: U256,
    ) -> Result<Option<UnderlyingSwap>, SimulationError> {
        let Some(base) = &self.base_pool else {
            return Ok(None);
        };
        let admin_fee = self.admin_fee()?;
        let swap = match route {
            Route::Coins { .. } => None,
            Route::ToBase { coin } => {
                exchange(&self.pool, base.meta_coin, base.lp_coin, dx, admin_fee).and_then(|meta| {
                    let (amount, base_pool) = base.withdraw(meta.amount, coin)?;
                    Some((amount, meta.balances, base_pool))
                })
            }
            Route::FromBase { coin } => base
                .deposit(dx, coin)
                .and_then(|(minted, base_pool)| {
                    let meta =
                        exchange(&self.pool, base.lp_coin, base.meta_coin, minted, admin_fee)?;
                    Some((meta.amount, meta.balances, base_pool))
                }),
        };
        let Some((amount, balances, base_pool)) = swap else {
            return Ok(None);
        };
        let mut pool = self.pool.clone();
        for (index, balance) in balances.into_iter().enumerate() {
            pool.set_balance(index, balance)
                .map_err(|e| SimulationError::FatalError(e.to_string()))?;
        }
        Ok(Some(UnderlyingSwap { amount, pool, base_pool }))
    }

    /// The reserve an underlying swap sells into, in the sell token's units.
    ///
    /// A swap from a base coin sells LP tokens into the metapool, so its reserve is the
    /// metapool's LP balance, expressed as the base coin amount that balance redeems for. The base
    /// pool's own balance of that coin caps it.
    fn underlying_sell_reserve(&self, route: Route) -> U256 {
        match (&self.base_pool, route) {
            (Some(base), Route::ToBase { .. }) => self.pool.balances()[base.meta_coin],
            (Some(base), Route::FromBase { coin }) => {
                let lp_reserve = self.pool.balances()[base.lp_coin];
                let coin_reserve = base.balance(coin);
                base.withdraw(lp_reserve, coin)
                    .map_or(coin_reserve, |(amount, _)| amount.min(coin_reserve))
            }
            (_, Route::Coins { .. }) | (None, _) => U256::ZERO,
        }
    }

    /// The reserve an underlying swap pays out of, in the buy token's units.
    fn underlying_buy_reserve(&self, route: Route) -> U256 {
        match (&self.base_pool, route) {
            (Some(base), Route::ToBase { coin }) => base.balance(coin),
            (Some(base), Route::FromBase { .. }) => self.pool.balances()[base.meta_coin],
            (_, Route::Coins { .. }) | (None, _) => U256::ZERO,
        }
    }

    fn admin_fee(&self) -> Result<U256, SimulationError> {
        if self.variant == CurveVariant::StableSwapNG {
            return Ok(U256::from(5_000_000_000u64));
        }
        self.admin_fee.ok_or_else(|| {
            SimulationError::RecoverableError(format!(
                "Missing Curve admin fee for {}; refresh the pool state",
                self.pool_address
            ))
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

    /// Finds the swap to `target` with the native solver. Returns `None` when the target does not
    /// fit in U256 or the variant has no native solver. Also returns `None`, and logs the reason,
    /// when the solver's math fails.
    fn swap_to_target_price(
        &self,
        token_in: &Token,
        token_out: &Token,
        target: &Price,
        tolerance: f64,
    ) -> Result<Option<PoolSwap>, SimulationError> {
        let Route::Coins { i, j } = self.route(&token_in.address, &token_out.address)? else {
            return Ok(None);
        };
        if target.numerator.bits() > 256 || target.denominator.bits() > 256 {
            return Ok(None);
        }
        let target_num = biguint_to_u256(&target.numerator);
        let target_den = biguint_to_u256(&target.denominator);

        match swap_to_price(
            &self.pool,
            i,
            j,
            target_num,
            target_den,
            tolerance,
            if self.is_crypto() { U256::ZERO } else { self.admin_fee()? },
        ) {
            Ok(dx) => {
                if dx.is_zero() {
                    let swap = PoolSwap::new(BigUint::ZERO, BigUint::ZERO, self.clone_box(), None);
                    return Ok(Some(swap));
                }
                let result = self.get_amount_out(u256_to_biguint(dx), token_in, token_out)?;
                Ok(Some(PoolSwap::new(u256_to_biguint(dx), result.amount, result.new_state, None)))
            }
            Err(SwapToPriceError::TargetAboveSpot) => {
                let spot = self.spot_price(token_in, token_out)?;
                let decimal_adjustment =
                    10f64.powi(decimals_exponent(token_in.decimals, token_out.decimals)?);
                let target =
                    u256_to_f64(target_num)? / u256_to_f64(target_den)? * decimal_adjustment;
                Err(SimulationError::InvalidInput(
                    format!("Target price {target} is above spot price {spot}"),
                    None,
                ))
            }
            Err(SwapToPriceError::TargetBelowLimit) => Err(SimulationError::InvalidInput(
                format!(
                    "Target price below reachable limit for curve pool {pool}",
                    pool = self.pool_address
                ),
                None,
            )),
            Err(err @ SwapToPriceError::InvalidInput(_)) => Err(SimulationError::InvalidInput(
                format!("{err} for curve pool {pool}", pool = self.pool_address),
                None,
            )),
            Err(SwapToPriceError::UnsupportedVariant) => Ok(None),
            Err(err @ SwapToPriceError::MathFailed) => {
                debug!(
                    pool = %self.pool_address,
                    %err,
                    "Curve native swap-to-price failed; using the numerical search"
                );
                Ok(None)
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
        let route = self.route(&base.address, &quote.address)?;
        let Route::Coins { i, j } = route else {
            // Underlying swaps have no closed-form marginal price here: quote a small trade.
            let probe = (self.underlying_sell_reserve(route) / U256::from(SPOT_PROBE_DIVISOR))
                .max(U256::from(1));
            let swap = self
                .swap_underlying(route, probe)?
                .ok_or_else(|| {
                    SimulationError::RecoverableError(format!(
                        "curve underlying spot price unavailable for {}",
                        self.pool_address
                    ))
                })?;
            let ratio = u256_to_f64(swap.amount)? / u256_to_f64(probe)?;
            let decimal_adjustment = 10f64.powi(decimals_exponent(base.decimals, quote.decimals)?);
            return Ok(ratio * decimal_adjustment);
        };
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
        let decimal_adjustment = 10f64.powi(decimals_exponent(base.decimals, quote.decimals)?);
        Ok(ratio * decimal_adjustment)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let route = self.route(&token_in.address, &token_out.address)?;
        let dx = biguint_to_u256(&amount_in);
        let Route::Coins { i, j } = route else {
            let swap = self
                .swap_underlying(route, dx)?
                .ok_or_else(|| {
                    SimulationError::RecoverableError(format!(
                        "curve underlying exchange failed for {}",
                        self.pool_address
                    ))
                })?;
            let new_state =
                Self { pool: swap.pool, base_pool: Some(swap.base_pool), ..self.clone() };
            return Ok(GetAmountOutResult::new(
                u256_to_biguint(swap.amount),
                UNDERLYING_GAS
                    .to_biguint()
                    .expect("u64 fits in BigUint"),
                Box::new(new_state),
            ));
        };

        let mut new_pool = self.pool.clone();
        let dy = if self.is_crypto() {
            let dy = self
                .pool
                .get_amount_out(i, j, dx)
                .ok_or_else(|| {
                    SimulationError::RecoverableError(format!(
                        "curve get_amount_out failed for {}",
                        self.pool_address
                    ))
                })?;
            let balances = self.pool.balances();
            // CryptoSwap retains its existing balance-only approximation.
            new_pool
                .set_balance(i, balances[i] + dx)
                .map_err(|e| SimulationError::FatalError(e.to_string()))?;
            new_pool
                .set_balance(j, balances[j].saturating_sub(dy))
                .map_err(|e| SimulationError::FatalError(e.to_string()))?;
            dy
        } else {
            let result = exchange(&self.pool, i, j, dx, self.admin_fee()?).ok_or_else(|| {
                SimulationError::RecoverableError(format!(
                    "curve exchange accounting failed for {}",
                    self.pool_address
                ))
            })?;
            for (index, balance) in result.balances.into_iter().enumerate() {
                new_pool
                    .set_balance(index, balance)
                    .map_err(|e| SimulationError::FatalError(e.to_string()))?;
            }
            result.amount
        };

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
        let route = match self.route(&sell_token, &buy_token) {
            Ok(route) => route,
            // A pair of this component's tokens with no route is one the base pool serves.
            Err(_)
                if sell_token != buy_token &&
                    self.holds_token(&sell_token) &&
                    self.holds_token(&buy_token) =>
            {
                return Ok((BigUint::ZERO, BigUint::ZERO));
            }
            Err(error) => return Err(error),
        };
        let Route::Coins { i, j } = route else {
            // Soft limit, as for the pool's own coins: the sell-side reserve. An underlying swap
            // of that size can exceed what the other pool holds, so halve it until it fits.
            let mut max_in = self.underlying_sell_reserve(route);
            for _ in 0..LIMIT_HALVINGS {
                if max_in.is_zero() {
                    break;
                }
                if let Some(swap) = self.swap_underlying(route, max_in)? {
                    let reserve = self
                        .underlying_buy_reserve(route)
                        .saturating_sub(U256::from(1));
                    return Ok((u256_to_biguint(max_in), u256_to_biguint(swap.amount.min(reserve))));
                }
                max_in /= U256::from(2);
            }
            return Ok((BigUint::ZERO, BigUint::ZERO));
        };
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

    /// When `updated_attributes` carries [`vm::POOL_STATE_ADJUSTED`], the pool is rebuilt from
    /// those readings. Otherwise the view getters are read from the indexed VM storage.
    ///
    /// The attribute exists for pending blocks, whose state never reaches that storage: an
    /// indexer that has already read the pool under the pending block's overrides passes the
    /// readings through instead.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &std::collections::HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let state = match delta
            .updated_attributes
            .get(vm::POOL_STATE_ADJUSTED)
        {
            Some(encoded) => {
                let state = vm::decode_raw_state(encoded)?;
                if state.variant != self.variant {
                    return Err(SimulationError::FatalError(format!(
                        "Variant mismatch: expected {}, got {}",
                        self.variant, state.variant
                    ))
                    .into())
                }
                if state.token_decimals != self.decimals {
                    return Err(SimulationError::FatalError(format!(
                        "Token decimals mismatch: expected {:?}, got {:?}",
                        self.decimals, state.token_decimals
                    ))
                    .into())
                }
                state
            }
            None => {
                let engine = create_engine(SHARED_TYCHO_DB.clone(), false)?;
                let pool_address = AlloyAddress::from_slice(self.pool_address.as_ref());
                let state = vm::read_raw_pool_state(
                    &engine,
                    &pool_address,
                    self.variant,
                    &self.decimals,
                    &Default::default(),
                )?;
                // A pending block's readings carry the metapool only, so the base pool is
                // re-read on this path alone.
                if let Some(base_pool) = &mut self.base_pool {
                    base_pool.refresh(&engine)?;
                }
                state
            }
        };
        let pool = build_pool(&state)
            .map_err(|e| SimulationError::FatalError(format!("curve build_pool failed: {e}")))?;
        self.pool = pool;
        self.admin_fee = state.admin_fee;
        Ok(())
    }

    /// Answers [`SwapConstraint::PoolTargetPrice`] on StableSwap pools with the native solver,
    /// which ignores `min_amount_in` and `max_amount_in` and returns no `price_points`. All other
    /// cases use the numerical search.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
            SwapConstraint::PoolTargetPrice { target, tolerance, .. } => {
                let native = self.swap_to_target_price(
                    params.token_in(),
                    params.token_out(),
                    target,
                    *tolerance,
                )?;
                match native {
                    Some(swap) => Ok(swap),
                    None => crate::evm::query_pool_swap::query_pool_swap(self, params),
                }
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
    use std::{collections::HashMap, str::FromStr};

    use num_traits::ToPrimitive;
    use rstest::rstest;
    use tycho_common::{
        models::Chain,
        simulation::protocol_sim::{QueryPoolSwapParams, SwapConstraint},
    };

    use super::*;
    use crate::evm::{
        protocol::curve::{adapter::RawPoolState, vm::encode_raw_state},
        query_pool_swap::test_helpers::{target_price_params, to_price},
    };

    const VARIANT: CurveVariant = CurveVariant::TriCryptoNG;
    const DECIMALS: [u8; 3] = [6, 8, 18];

    fn u(s: &str) -> U256 {
        s.parse().unwrap()
    }

    /// The TriCryptoNG USDC/WBTC/WETH pool (0x7f86bf…), balances aside.
    fn raw_pool_state(balances: Vec<U256>) -> RawPoolState {
        RawPoolState {
            variant: VARIANT,
            balances,
            token_decimals: DECIMALS.to_vec(),
            amp: u("1707629"),
            mid_fee: Some(u("3000000")),
            out_fee: Some(u("30000000")),
            fee_gamma: Some(u("500000000000000")),
            d: Some(u("7457948167729606869978625")),
            gamma: Some(u("11809167828997")),
            price_scale: Some(vec![u("59372627314351316239076"), u("1565715369034455123313")]),
            ..Default::default()
        }
    }

    fn state(balances: Vec<U256>) -> CurveState {
        let raw_state = raw_pool_state(balances);
        let pool = build_pool(&raw_state).expect("build");
        CurveState::new(
            Bytes::from([7u8; 20]),
            vec![Bytes::from([1u8; 20]), Bytes::from([2u8; 20]), Bytes::from([3u8; 20])],
            DECIMALS.to_vec(),
            VARIANT,
            pool,
            Some(U256::ZERO),
        )
    }

    fn delta(attributes: HashMap<String, Bytes>) -> ProtocolStateDelta {
        ProtocolStateDelta { updated_attributes: attributes, ..Default::default() }
    }

    #[test]
    fn test_delta_transition_rebuilds_from_attribute() {
        let confirmed = vec![u("2466241139205"), u("4200057336"), u("1595469030050811720465")];
        let pending = vec![u("2470000000000"), u("4190000000"), u("1600000000000000000000")];
        let mut curve = state(confirmed.clone());
        let attribute = encode_raw_state(&raw_pool_state(pending.clone())).expect("encode");

        curve
            .delta_transition(
                delta(HashMap::from([(vm::POOL_STATE_ADJUSTED.to_string(), attribute)])),
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("delta transition from attribute failed");

        // The readings must come from the attribute. A VM read would fail here anyway: the
        // shared engine has no block set in this test.
        assert_eq!(
            curve.pool.balances()[..3],
            pending[..],
            "balances must come from the attribute"
        );
        assert_ne!(curve.pool.balances()[..3], confirmed[..]);
    }

    #[test]
    fn test_delta_transition_errors_on_variant_mismatch() {
        let mut curve = state(vec![u("1"), u("2"), u("3")]);
        let mut pending_state = raw_pool_state(curve.pool.balances().to_vec());
        pending_state.variant = CurveVariant::StableSwapMeta;
        let encoded = encode_raw_state(&pending_state).expect("encode");

        let result = curve.delta_transition(
            delta(HashMap::from([(vm::POOL_STATE_ADJUSTED.to_string(), encoded)])),
            &HashMap::new(),
            &Balances::default(),
        );

        assert!(matches!(result, Err(TransitionError::SimulationError(_))), "got {result:?}");
    }

    #[test]
    fn test_delta_transition_errors_on_decimals_mismatch() {
        let mut curve = state(vec![u("1"), u("2"), u("3")]);
        let mut pending_state = raw_pool_state(curve.pool.balances().to_vec());
        pending_state.token_decimals = vec![18, 18, 18];
        let encoded = encode_raw_state(&pending_state).expect("encode");

        let result = curve.delta_transition(
            delta(HashMap::from([(vm::POOL_STATE_ADJUSTED.to_string(), encoded)])),
            &HashMap::new(),
            &Balances::default(),
        );

        assert!(matches!(result, Err(TransitionError::SimulationError(_))), "got {result:?}");
    }

    #[test]
    fn test_delta_transition_rejects_malformed_attribute() {
        let mut curve = state(vec![u("1"), u("2"), u("3")]);

        let result = curve.delta_transition(
            delta(HashMap::from([(
                vm::POOL_STATE_ADJUSTED.to_string(),
                Bytes::from(b"not json".to_vec()),
            )])),
            &HashMap::new(),
            &Balances::default(),
        );

        // Falling back to the indexed VM state would silently price a pending block against
        // confirmed state, so a malformed attribute must fail instead.
        assert!(matches!(result, Err(TransitionError::SimulationError(_))), "got {result:?}");
    }

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
            .map(|k| token(u8::try_from(k).unwrap(), decimals[k] as u32).address)
            .collect();
        CurveState::new(
            Bytes::from_str("0x00000000000000000000000000000000000000ff").expect("valid address"),
            tokens,
            decimals,
            variant,
            pool,
            Some(U256::ZERO),
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

    /// The USD1 metapool (`0xC09e82f81Cb811DB0922dD48206fc2e212322caf`) and its base pool
    /// crv2pool (USDC/USDT) at block 26434212. Tokens: 0 = USD1, 1 = crv2pool, 2 = USDC, 3 = USDT.
    fn usd1_metapool() -> CurveState {
        let pool = Pool::StableSwapNG {
            balances: vec![
                U256::from(7_369_267_681_006_782_861_926_247u128),
                U256::from(2_648_075_667_563_217_394_185_872u128),
            ],
            rates: vec![U256::from(WAD), U256::from(1_019_626_599_605_111_854u128)],
            amp: U256::from(200_000u64),
            fee: U256::from(1_000_000u64),
            offpeg_fee_multiplier: U256::from(100_000_000_000u64),
        };
        let base_rate = U256::from(10u8).pow(U256::from(30u8));
        let base = Pool::StableSwapNG {
            balances: vec![U256::from(612_669_739_206u64), U256::from(4_493_527_459_173u64)],
            rates: vec![base_rate, base_rate],
            amp: U256::from(1_000_000u64),
            fee: U256::from(100_000u64),
            offpeg_fee_multiplier: U256::from(200_000_000_000u64),
        };
        let base_pool = BasePool::new(
            vec![token(2, 6).address, token(3, 6).address],
            base,
            U256::from(5_007_566_619_967_511_977_472_971u128),
            (0, 1),
        );
        curve_state(pool, CurveVariant::StableSwapNG, vec![18, 18]).with_base_pool(base_pool)
    }

    /// Expected values are `get_dy_underlying` on chain at block 26434212.
    #[rstest::rstest]
    #[case::usd1_to_usdc_small(0, 2, 1_000 * WAD, 998_431_508)]
    #[case::usd1_to_usdc(0, 2, 100_000 * WAD, 99_826_575_332)]
    #[case::usd1_to_usdt(0, 3, 100_000 * WAD, 99_925_522_977)]
    #[case::usd1_to_usdt_large(0, 3, 1_000_000 * WAD, 998_734_524_892)]
    #[case::usdc_to_usd1(2, 0, 100_000_000_000, 100_117_218_861_552_368_320_042)]
    #[case::usdt_to_usd1(3, 0, 100_000_000_000, 100_042_232_123_237_499_257_451)]
    #[case::usdt_to_usd1_large(3, 0, 1_000_000_000_000, 1_000_192_703_343_654_570_648_144)]
    fn underlying_quote_matches_chain(
        #[case] token_in: u8,
        #[case] token_out: u8,
        #[case] amount_in: u128,
        #[case] expected: u128,
    ) {
        let state = usd1_metapool();
        let decimals = |index: u8| if index < 2 { 18 } else { 6 };

        let result = state
            .get_amount_out(
                BigUint::from(amount_in),
                &token(token_in, decimals(token_in)),
                &token(token_out, decimals(token_out)),
            )
            .expect("underlying quote");

        assert_eq!(result.amount, BigUint::from(expected));
    }

    #[rstest::rstest]
    #[case::lp_to_base_coin(1, 2)]
    #[case::base_coin_to_lp(3, 1)]
    #[case::between_base_coins(2, 3)]
    fn underlying_refuses_pairs_the_base_pool_serves(#[case] token_in: u8, #[case] token_out: u8) {
        let state = usd1_metapool();

        let result = state.get_amount_out(
            BigUint::from(1_000_000u64),
            &token(token_in, 18),
            &token(token_out, 6),
        );

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, _))), "{result:?}");
    }

    #[rstest::rstest]
    #[case::lp_to_base_coin(1, 2)]
    #[case::base_coin_to_lp(3, 1)]
    #[case::between_base_coins(2, 3)]
    fn underlying_limits_are_zero_for_pairs_the_base_pool_serves(
        #[case] token_in: u8,
        #[case] token_out: u8,
    ) {
        let state = usd1_metapool();

        let limits = state
            .get_limits(token(token_in, 18).address, token(token_out, 6).address)
            .expect("limits");

        assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO));
    }

    #[test]
    fn limits_fail_for_a_token_the_pool_does_not_hold() {
        let state = usd1_metapool();

        let limits = state.get_limits(token(0, 18).address, token(9, 18).address);

        assert!(matches!(limits, Err(SimulationError::InvalidInput(_, _))), "{limits:?}");
    }

    #[test]
    fn underlying_swap_moves_both_pools() {
        let state = usd1_metapool();
        let amount = BigUint::from(100_000 * WAD);

        let first = state
            .get_amount_out(amount.clone(), &token(0, 18), &token(2, 6))
            .expect("first quote");
        let second = first
            .new_state
            .get_amount_out(amount, &token(0, 18), &token(2, 6))
            .expect("second quote");

        assert!(second.amount < first.amount, "the second swap gets a worse price");
    }

    #[rstest::rstest]
    #[case::sell_usd1_for_usdc(0, 2)]
    #[case::sell_usdt_for_usd1(3, 0)]
    fn underlying_limit_is_quotable(#[case] token_in: u8, #[case] token_out: u8) {
        let state = usd1_metapool();
        let decimals = |index: u8| if index < 2 { 18 } else { 6 };
        let (sell, buy) =
            (token(token_in, decimals(token_in)), token(token_out, decimals(token_out)));

        let (max_in, max_out) = state
            .get_limits(sell.address.clone(), buy.address.clone())
            .expect("limits");
        let quote = state
            .get_amount_out(max_in.clone(), &sell, &buy)
            .expect("quote at the limit");

        assert!(max_in > BigUint::ZERO);
        assert_eq!(quote.amount, max_out);
    }

    #[test]
    fn underlying_limit_pays_less_than_the_base_pool_reserve() {
        let state = usd1_metapool();

        let (_, max_out) = state
            .get_limits(token(0, 18).address, token(2, 6).address)
            .expect("limits");

        // The metapool holds 7.37M USD1 but the base pool only 612,669 USDC.
        assert!(max_out < BigUint::from(612_669_739_206u64));
    }

    #[test]
    fn underlying_limit_from_a_base_coin_follows_the_metapool() {
        let state = usd1_metapool();

        let (max_in, _) = state
            .get_limits(token(3, 6).address, token(0, 18).address)
            .expect("limits");

        // The base pool holds 4.49M USDT; the metapool's 2.65M crv2pool redeem for about 2.7M.
        assert!(max_in > BigUint::from(2_600_000_000_000u64), "{max_in}");
        assert!(max_in < BigUint::from(2_800_000_000_000u64), "{max_in}");
    }

    #[rstest::rstest]
    #[case::usd1_in_usdc(0, 2)]
    #[case::usdt_in_usd1(3, 0)]
    fn underlying_spot_price_is_near_parity(#[case] base: u8, #[case] quote: u8) {
        let state = usd1_metapool();
        let decimals = |index: u8| if index < 2 { 18 } else { 6 };

        let price = state
            .spot_price(&token(base, decimals(base)), &token(quote, decimals(quote)))
            .expect("spot price");

        assert!((0.99..1.01).contains(&price), "{price}");
    }

    #[test]
    fn pending_state_preserves_admin_fee_for_exchange() {
        let (mut state, token_in, token_out) = v1_two_coin_state();
        let pool = state.pool.clone();
        #[expect(clippy::unreachable, reason = "v1_two_coin_state builds a StableSwapV1 pool")]
        let Pool::StableSwapV1 { balances, rates: _, amp, fee } = pool
        else {
            unreachable!()
        };
        let raw = RawPoolState {
            variant: CurveVariant::StableSwapV1,
            balances,
            amp,
            fee: Some(fee),
            token_decimals: vec![18, 18],
            admin_fee: Some(U256::from(5_000_000_000u64)),
            ..Default::default()
        };
        state
            .delta_transition(
                delta(HashMap::from([(
                    vm::POOL_STATE_ADJUSTED.into(),
                    encode_raw_state(&raw).unwrap(),
                )])),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert_eq!(state.admin_fee, raw.admin_fee);
        let mut without_admin = state.clone();
        without_admin.admin_fee = Some(U256::ZERO);
        let amount = BigUint::from(WAD);
        let with_fee = state
            .get_amount_out(amount.clone(), &token_in, &token_out)
            .unwrap();
        let no_fee = without_admin
            .get_amount_out(amount, &token_in, &token_out)
            .unwrap();
        assert_eq!(with_fee.amount, no_fee.amount);
        let balance = |result: &GetAmountOutResult| {
            result
                .new_state
                .as_any()
                .downcast_ref::<CurveState>()
                .unwrap()
                .pool
                .balances()[1]
        };
        assert!(balance(&with_fee) < balance(&no_fee));
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

    const TOLERANCE: f64 = 0.001;

    /// Builds `PoolTargetPrice` params for a target of `spot * multiplier`, and returns the
    /// target as f64.
    fn spot_target_params(
        state: &CurveState,
        token_in: &Token,
        token_out: &Token,
        multiplier: f64,
    ) -> (QueryPoolSwapParams, f64) {
        let spot = state
            .spot_price(token_in, token_out)
            .expect("spot price");
        let target_f64 = spot * multiplier;
        let target = to_price(target_f64, token_in, token_out);
        (target_price_params(token_in, token_out, target, TOLERANCE), target_f64)
    }

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
    fn test_query_pool_swap_native_amounts(
        #[case] setup: (CurveState, Token, Token),
        #[case] multiplier: f64,
    ) {
        let (state, token_in, token_out) = setup;
        let (params, target) = spot_target_params(&state, &token_in, &token_out, multiplier);

        let swap = state
            .query_pool_swap(&params)
            .expect("native query_pool_swap");

        let price = swap
            .new_state()
            .spot_price(&token_in, &token_out)
            .unwrap();
        assert!(price >= target && price <= target * (1.0 + TOLERANCE));
        let executed = state
            .get_amount_out(swap.amount_in().clone(), &token_in, &token_out)
            .unwrap();
        assert_eq!(swap.amount_out(), &executed.amount);
        assert!(ProtocolSim::eq(swap.new_state(), executed.new_state.as_ref()));
    }

    /// The native result must land in the lower half of the tolerance band, and the numerical
    /// result within five times the band.
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
    fn test_query_pool_swap_numerical_comparison(
        #[case] setup: (CurveState, Token, Token),
        #[case] multiplier: f64,
    ) {
        let (state, token_in, token_out) = setup;
        let (params, target_f64) = spot_target_params(&state, &token_in, &token_out, multiplier);

        let native = state
            .query_pool_swap(&params)
            .expect("native query_pool_swap");
        let numerical = crate::evm::query_pool_swap::query_pool_swap(&state, &params)
            .expect("numerical query_pool_swap");

        for (label, swap, band) in
            [("native", &native, TOLERANCE / 2.0), ("numerical", &numerical, 5.0 * TOLERANCE)]
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
    }

    /// CryptoSwap pools delegate to the numerical search. That search rejects every target,
    /// because `get_amount_out` keeps the stored `D` (no `tweak_price` port).
    #[test]
    fn test_crypto_variant_delegates_to_numerical() {
        let (state, token_in, token_out) = two_crypto_ng_state();
        let (params, _) = spot_target_params(&state, &token_in, &token_out, 0.995);

        let result = state.query_pool_swap(&params);
        let Err(SimulationError::InvalidInput(msg, _)) = result else {
            panic!("crypto pools must delegate to the numerical search, got {result:?}");
        };
        assert!(msg.contains("< limit"), "expected the numerical search's limit error, got: {msg}");
    }

    #[test]
    fn test_query_pool_swap_target_wider_than_u256() {
        let (state, token_in, token_out) = v1_two_coin_state();
        let (params, target_f64) = spot_target_params(&state, &token_in, &token_out, 0.999);
        let SwapConstraint::PoolTargetPrice { target, .. } = params.swap_constraint() else {
            panic!("spot_target_params builds a PoolTargetPrice constraint");
        };
        let scale = BigUint::from(1u8) << 256;
        let wide_target = Price::new(&target.numerator * &scale, &target.denominator * &scale);
        let params = target_price_params(&token_in, &token_out, wide_target, TOLERANCE);

        let swap = state
            .query_pool_swap(&params)
            .expect("numerical query_pool_swap");

        assert!(swap.price_points().is_some(), "only the numerical search returns price points");
        let new_spot = swap
            .new_state()
            .spot_price(&token_in, &token_out)
            .expect("post-swap spot");
        let error = (new_spot - target_f64) / target_f64;
        assert!(
            (-1e-12..=5.0 * TOLERANCE).contains(&error),
            "post-swap spot {new_spot} missed {target_f64}"
        );
    }

    #[test]
    fn test_trade_limit_price_delegates_to_numerical() {
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
                tolerance: TOLERANCE,
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
            .expect("failed to convert the output amount to f64") /
            swap.amount_in()
                .to_f64()
                .expect("failed to convert the input amount to f64");
        assert!(trade_price >= limit_f64, "trade price {trade_price} violates limit {limit_f64}");
    }

    #[rstest]
    #[case::above_spot(1.01, "is above spot price")]
    #[case::below_limit(1e-9, "below reachable limit for curve pool")]
    fn test_query_pool_swap_unreachable_target(#[case] multiplier: f64, #[case] expected: &str) {
        let (state, token_in, token_out) = v1_two_coin_state();
        let (params, _) = spot_target_params(&state, &token_in, &token_out, multiplier);

        let result = state.query_pool_swap(&params);
        let Err(SimulationError::InvalidInput(msg, _)) = result else {
            panic!("expected InvalidInput, got {result:?}");
        };
        assert!(msg.contains(expected), "unexpected message: {msg}");
    }

    #[test]
    fn test_query_pool_swap_target_equal_to_spot() {
        let (state, token_in, token_out) = v1_two_coin_state();
        let route = state
            .route(&token_in.address, &token_out.address)
            .expect("the pool's own coins trade directly");
        assert_eq!(route, Route::Coins { i: 0, j: 1 });
        let (i, j) = (0, 1);
        let (num, den) = state
            .pool
            .spot_price(i, j)
            .expect("pool spot price");
        let target = Price::new(u256_to_biguint(num), u256_to_biguint(den));
        let params = target_price_params(&token_in, &token_out, target, TOLERANCE);

        let swap = state
            .query_pool_swap(&params)
            .expect("query_pool_swap at spot");
        assert_eq!(swap.amount_in(), &BigUint::ZERO);
        assert_eq!(swap.amount_out(), &BigUint::ZERO);
    }
}
