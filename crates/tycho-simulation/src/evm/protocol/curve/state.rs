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
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        curve::{adapter::CurveVariant, math::Pool, vm},
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

    /// When `updated_attributes` carries [`vm::POOL_STATE_ADJUSTED`], the pool is rebuilt from
    /// those readings. Otherwise the view getters are read from the indexed VM storage.
    ///
    /// The attribute exists for pending blocks, whose state never reaches that storage: an
    /// indexer that has already read the pool under the pending block's overrides passes the
    /// readings through instead. Only the readings travel — the variant and the coin decimals are
    /// static, so this state supplies them.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &std::collections::HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.pool = match delta
            .updated_attributes
            .get(vm::POOL_STATE_ADJUSTED)
        {
            Some(encoded) => {
                let readings = vm::decode_readings(encoded)?;
                vm::build_from_readings(&readings, self.variant, &self.decimals)?
            }
            None => {
                let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("Infallible");
                let pool_address = AlloyAddress::from_slice(self.pool_address.as_ref());
                vm::decode_from_vm(&engine, &pool_address, self.variant, &self.decimals)?
            }
        };
        Ok(())
    }

    /// True only when the delta carries `pool_state_adjusted`. Without it the transition falls
    /// back to reading the view getters against the confirmed VM storage.
    fn transitions_from_delta_alone(&self, delta: &ProtocolStateDelta) -> bool {
        delta
            .updated_attributes
            .contains_key(vm::POOL_STATE_ADJUSTED)
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
    use std::collections::HashMap;

    use super::*;
    use crate::evm::protocol::curve::vm::{
        build_from_readings, encode_readings, CurvePoolReadings,
    };

    const VARIANT: CurveVariant = CurveVariant::TriCryptoNG;
    const DECIMALS: [u8; 3] = [6, 8, 18];

    fn u(s: &str) -> U256 {
        s.parse().unwrap()
    }

    /// The TriCryptoNG USDC/WBTC/WETH pool (0x7f86bf…), balances aside.
    fn readings(balances: Vec<U256>) -> CurvePoolReadings {
        CurvePoolReadings {
            balances,
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
        let pool = build_from_readings(&readings(balances), VARIANT, &DECIMALS).expect("build");
        CurveState::new(
            Bytes::from([7u8; 20]),
            vec![Bytes::from([1u8; 20]), Bytes::from([2u8; 20]), Bytes::from([3u8; 20])],
            DECIMALS.to_vec(),
            VARIANT,
            pool,
        )
    }

    fn delta(attributes: HashMap<String, Bytes>) -> ProtocolStateDelta {
        ProtocolStateDelta { updated_attributes: attributes, ..Default::default() }
    }

    /// Without `pool_state_adjusted` the transition reads the view getters against the confirmed
    /// VM storage, which a pending block never reaches.
    #[test]
    fn test_delta_alone_suffices_only_with_the_readings_attribute() {
        let curve = state(vec![u("1"), u("1"), u("1")]);
        let attribute = encode_readings(&readings(vec![u("2"), u("2"), u("2")])).expect("encode");

        let with_readings =
            delta(HashMap::from([(vm::POOL_STATE_ADJUSTED.to_string(), attribute)]));

        assert!(curve.transitions_from_delta_alone(&with_readings));
        assert!(!curve.transitions_from_delta_alone(&delta(HashMap::new())));
    }

    #[test]
    fn test_delta_transition_rebuilds_from_attribute() {
        let confirmed = vec![u("2466241139205"), u("4200057336"), u("1595469030050811720465")];
        let pending = vec![u("2470000000000"), u("4190000000"), u("1600000000000000000000")];
        let mut curve = state(confirmed.clone());
        let attribute = encode_readings(&readings(pending.clone())).expect("encode");

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
}
