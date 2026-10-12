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

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        curve::{
            adapter::{build_pool, CurveVariant},
            math::Pool,
            swap_to_price::{exchange, swap_to_price, SwapToPriceError},
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
        Self { pool_address, tokens, decimals, variant, pool, admin_fee }
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
        let i = self.coin_index(&token_in.address)?;
        let j = self.coin_index(&token_out.address)?;
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
                    10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
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
                let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("Infallible");
                let pool_address = AlloyAddress::from_slice(self.pool_address.as_ref());
                vm::read_raw_pool_state(
                    &engine,
                    &pool_address,
                    self.variant,
                    &self.decimals,
                    &Default::default(),
                )?
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
            .map(|k| token(k as u8, decimals[k] as u32).address)
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

    #[test]
    fn pending_state_preserves_admin_fee_for_exchange() {
        let (mut state, token_in, token_out) = v1_two_coin_state();
        let pool = state.pool.clone();
        let Pool::StableSwapV1 { balances, rates: _, amp, fee } = pool else { unreachable!() };
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
        let i = state
            .coin_index(&token_in.address)
            .expect("token_in index");
        let j = state
            .coin_index(&token_out.address)
            .expect("token_out index");
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
