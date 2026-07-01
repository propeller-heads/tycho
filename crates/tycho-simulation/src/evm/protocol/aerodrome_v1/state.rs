use std::{any::Any, collections::HashMap, sync::Arc};

use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
        swap::{
            LimitsParams, MarginalPrice, MarginalPriceParams, QuerySwapParams, Quote, QuoteAmount,
            QuoteParams, Range, SimulationResult, Swap, SwapFee, SwapLimits, SwapQuoter,
            Transition, TransitionParams,
        },
    },
    Bytes,
};

use super::solidly_stable::{
    get_amount_out as solidly_stable_get_amount_out, get_limits as solidly_stable_get_limits,
    get_limits_u256 as solidly_stable_get_limits_u256,
};
use crate::evm::protocol::{
    cpmm::protocol::{
        cpmm_fee, cpmm_get_limits, cpmm_get_limits_u256, cpmm_spot_price, cpmm_swap_to_price,
        ProtocolFee,
    },
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint},
    utils::add_fee_markup,
};

const FEE_PRECISION_BPS: u32 = 10_000;
const FEE_PRECISION: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const AERODROME_V1_STABLE_FEE_BPS: u32 = 5;
const AERODROME_V1_VOLATILE_FEE_BPS: u32 = 30;
const AERODROME_V1_ZERO_FEE_INDICATOR_BPS: u32 = 420;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AerodromeV1State {
    pub reserve0: U256,
    pub reserve1: U256,
    pub stable: bool,
    pub fee: u32,
    pub decimals0: u8,
    pub decimals1: u8,
    #[serde(skip)]
    component: Option<Arc<ProtocolComponent<Arc<Token>>>>,
}

impl PartialEq for AerodromeV1State {
    fn eq(&self, other: &Self) -> bool {
        self.reserve0 == other.reserve0 &&
            self.reserve1 == other.reserve1 &&
            self.stable == other.stable &&
            self.fee == other.fee &&
            self.decimals0 == other.decimals0 &&
            self.decimals1 == other.decimals1
    }
}

impl Eq for AerodromeV1State {}

impl AerodromeV1State {
    /// Creates a new instance of `AerodromeV1State` with the given reserves and raw fee.
    pub fn new(
        reserve0: U256,
        reserve1: U256,
        stable: bool,
        fee: u32,
        decimals0: u8,
        decimals1: u8,
    ) -> Self {
        Self { reserve0, reserve1, stable, fee, decimals0, decimals1, component: None }
    }

    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        self.component = Some(component);
        self
    }

    fn default_fee_bps(&self) -> u32 {
        if self.stable {
            AERODROME_V1_STABLE_FEE_BPS
        } else {
            AERODROME_V1_VOLATILE_FEE_BPS
        }
    }

    fn resolved_fee_bps(&self) -> u32 {
        if self.fee == AERODROME_V1_ZERO_FEE_INDICATOR_BPS {
            0
        } else if self.fee != 0 {
            self.fee
        } else {
            self.default_fee_bps()
        }
    }

    fn protocol_fee(&self) -> Result<ProtocolFee, SimulationError> {
        let fee_bps = self.resolved_fee_bps();

        if fee_bps > FEE_PRECISION_BPS {
            return Err(SimulationError::FatalError(format!(
                "Invalid resolved fee value {}, expected <= {} bps",
                fee_bps, FEE_PRECISION_BPS
            )));
        }

        Ok(ProtocolFee::new(U256::from(FEE_PRECISION_BPS - fee_bps), FEE_PRECISION))
    }

    /// Aerodrome V1 volatile pools do not match our generic CPMM helper exactly.
    ///
    /// The on-chain pool first computes `floor(amount_in * fee / 10000)`, subtracts that fee from
    /// `amount_in`, and only then applies the constant-product formula. Reusing
    /// `cpmm_get_amount_out` would instead fold the fee into the numerator/denominator as
    /// `amount_in * (10000 - fee) / 10000`, which is algebraically equivalent over reals but not
    /// under Solidity integer division. That rounding difference is observable on-chain, so we
    /// mirror the contract implementation here.
    fn volatile_get_amount_out(
        &self,
        amount_in: U256,
        reserve_in: U256,
        reserve_out: U256,
    ) -> Result<U256, SimulationError> {
        if amount_in == U256::ZERO {
            return Err(SimulationError::InvalidInput("Amount in cannot be zero".to_string(), None));
        }

        if reserve_in == U256::ZERO || reserve_out == U256::ZERO {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }

        let fee_bps = self.resolved_fee_bps();
        let fee_amount =
            safe_div_u256(safe_mul_u256(amount_in, U256::from(fee_bps))?, FEE_PRECISION)?;
        let amount_in_after_fee = safe_sub_u256(amount_in, fee_amount)?;
        let numerator = safe_mul_u256(amount_in_after_fee, reserve_out)?;
        let denominator = safe_add_u256(reserve_in, amount_in_after_fee)?;

        safe_div_u256(numerator, denominator)
    }

    /// Applies a state delta (reserves and optional fee). Shared by the `ProtocolSim` and
    /// `SwapQuoter` transition methods.
    fn apply_delta(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        if let Some(reserve0) = delta.updated_attributes.get("reserve0") {
            self.reserve0 = U256::from_be_slice(reserve0);
        }
        if let Some(reserve1) = delta.updated_attributes.get("reserve1") {
            self.reserve1 = U256::from_be_slice(reserve1);
        }
        if let Some(fee) = delta.updated_attributes.get("fee") {
            self.fee = u32::from(fee.clone());
            let resolved_fee_bps = self.resolved_fee_bps();
            if resolved_fee_bps > FEE_PRECISION_BPS {
                return Err(TransitionError::DecodeError(format!(
                    "Invalid resolved fee value {}, expected <= {} bps",
                    resolved_fee_bps, FEE_PRECISION_BPS
                )));
            }
        }
        Ok(())
    }

    /// Computes `amount_out` in `U256` for the given direction, handling both stable and volatile
    /// pools. Shared core for `get_amount_out` and `SwapQuoter::quote`.
    fn amount_out_u256(
        &self,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<U256, SimulationError> {
        if self.stable {
            solidly_stable_get_amount_out(
                amount_in,
                zero_for_one,
                self.reserve0,
                self.reserve1,
                self.resolved_fee_bps(),
                self.decimals0,
                self.decimals1,
            )
        } else {
            let (reserve_in, reserve_out) = if zero_for_one {
                (self.reserve0, self.reserve1)
            } else {
                (self.reserve1, self.reserve0)
            };
            self.volatile_get_amount_out(amount_in, reserve_in, reserve_out)
        }
    }
}

#[typetag::serde]
impl ProtocolSim for AerodromeV1State {
    fn fee(&self) -> f64 {
        cpmm_fee(self.resolved_fee_bps())
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let price = cpmm_spot_price(base, quote, self.reserve0, self.reserve1)?;
        Ok(add_fee_markup(price, ProtocolSim::fee(self)))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        let zero2one = token_in.address < token_out.address;
        let amount_out = self.amount_out_u256(amount_in, zero2one)?;
        let mut new_state = self.clone();
        let (reserve0_mut, reserve1_mut) = (&mut new_state.reserve0, &mut new_state.reserve1);
        if zero2one {
            *reserve0_mut = safe_add_u256(self.reserve0, amount_in)?;
            *reserve1_mut = safe_sub_u256(self.reserve1, amount_out)?;
        } else {
            *reserve0_mut = safe_sub_u256(self.reserve0, amount_out)?;
            *reserve1_mut = safe_add_u256(self.reserve1, amount_in)?;
        };
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(120_000u32),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if self.stable {
            solidly_stable_get_limits(
                sell_token,
                buy_token,
                self.reserve0,
                self.reserve1,
                self.decimals0,
                self.decimals1,
            )
        } else {
            cpmm_get_limits(
                sell_token,
                buy_token,
                self.reserve0,
                self.reserve1,
                self.resolved_fee_bps(),
            )
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.apply_delta(&delta)
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::PoolTargetPrice {
                target: price,
                tolerance: _,
                min_amount_in: _,
                max_amount_in: _,
            } => {
                let zero2one = params.token_in().address < params.token_out().address;
                let (reserve_in, reserve_out) = if zero2one {
                    (self.reserve0, self.reserve1)
                } else {
                    (self.reserve1, self.reserve0)
                };

                let (amount_in, _) =
                    cpmm_swap_to_price(reserve_in, reserve_out, price, self.protocol_fee()?)?;
                if amount_in.is_zero() {
                    return Ok(PoolSwap::new(
                        BigUint::ZERO,
                        BigUint::ZERO,
                        Box::new(self.clone()),
                        None,
                    ));
                }

                let res =
                    self.get_amount_out(amount_in.clone(), params.token_in(), params.token_out())?;
                Ok(PoolSwap::new(amount_in, res.amount, res.new_state, None))
            }
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "AerodromeV1State does not support TradeLimitPrice constraint in query_pool_swap"
                    .to_string(),
                None,
            )),
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
        if let Some(other_state) = other.as_any().downcast_ref::<Self>() {
            self.reserve0 == other_state.reserve0 &&
                self.reserve1 == other_state.reserve1 &&
                self.stable == other_state.stable &&
                self.fee == other_state.fee &&
                self.decimals0 == other_state.decimals0 &&
                self.decimals1 == other_state.decimals1
        } else {
            false
        }
    }
}

#[typetag::serde]
impl SwapQuoter for AerodromeV1State {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component.clone().ok_or_else(|| {
            SimulationError::FatalError(
                "AerodromeV1State: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Ok(SwapFee::new(cpmm_fee(self.resolved_fee_bps())))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component.as_ref().ok_or_else(|| {
            SimulationError::FatalError("AerodromeV1State: component not set".to_string())
        })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;
        let price = cpmm_spot_price(base.as_ref(), quote.as_ref(), self.reserve0, self.reserve1)?;
        Ok(MarginalPrice::new(add_fee_markup(price, ProtocolSim::fee(self))))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "AerodromeV1State does not yet support exact-out (FixedOut) quoting"
                        .to_string(),
                ))
            }
        };

        let zero_for_one = params.token_in() < params.token_out();
        let amount_out = self.amount_out_u256(amount_in, zero_for_one)?;

        let new_state = if params.should_return_new_state() {
            let mut new_state = self.clone();
            if zero_for_one {
                new_state.reserve0 = safe_add_u256(self.reserve0, amount_in)?;
                new_state.reserve1 = safe_sub_u256(self.reserve1, amount_out)?;
            } else {
                new_state.reserve0 = safe_sub_u256(self.reserve0, amount_out)?;
                new_state.reserve1 = safe_add_u256(self.reserve1, amount_in)?;
            }
            Some(Arc::new(new_state) as Arc<dyn SwapQuoter>)
        } else {
            None
        };

        Ok(Quote::new(amount_out, 120_000, new_state))
    }

    fn swap_limits(&self, params: LimitsParams) -> SimulationResult<SwapLimits> {
        let zero_for_one = params.token_in() < params.token_out();
        let (max_in, max_out) = if self.stable {
            solidly_stable_get_limits_u256(
                zero_for_one,
                self.reserve0,
                self.reserve1,
                self.decimals0,
                self.decimals1,
            )?
        } else {
            cpmm_get_limits_u256(
                zero_for_one,
                self.reserve0,
                self.reserve1,
                self.resolved_fee_bps(),
            )?
        };
        Ok(SwapLimits::new(Range::new(U256::ZERO, max_in)?, Range::new(U256::ZERO, max_out)?))
    }

    fn query_swap(&self, params: QuerySwapParams) -> SimulationResult<Swap> {
        use tycho_common::simulation::swap::SwapConstraint as QuerySwapConstraint;
        match params.swap_constraint() {
            QuerySwapConstraint::PoolTargetPrice { target, .. } => {
                let zero_for_one = params.token_in() < params.token_out();
                let (reserve_in, reserve_out) = if zero_for_one {
                    (self.reserve0, self.reserve1)
                } else {
                    (self.reserve1, self.reserve0)
                };
                let legacy_price = tycho_common::simulation::protocol_sim::Price::new(
                    u256_to_biguint(target.numerator),
                    u256_to_biguint(target.denominator),
                );
                let (amount_in, _) = cpmm_swap_to_price(
                    reserve_in,
                    reserve_out,
                    &legacy_price,
                    self.protocol_fee()?,
                )?;
                if amount_in.is_zero() {
                    return Ok(Swap::new(U256::ZERO, U256::ZERO, None, None));
                }
                let amount_in_u256 = biguint_to_u256(&amount_in);
                let quote = self.quote(
                    QuoteParams::fixed_in(params.token_in(), params.token_out(), amount_in_u256)?
                        .with_new_state(),
                )?;
                Ok(Swap::new(amount_in_u256, quote.amount_out(), quote.new_state(), None))
            }
            QuerySwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::RecoverableError(
                "AerodromeV1State does not support TradeLimitPrice constraint in query_swap"
                    .to_string(),
            )),
        }
    }

    fn delta_transition(
        &mut self,
        params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        self.apply_delta(params.delta())?;
        Ok(Transition::default())
    }

    fn clone_box(&self) -> Box<dyn SwapQuoter> {
        Box::new(self.clone())
    }

    #[allow(deprecated)]
    fn to_protocol_sim(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use num_traits::One;
    use tycho_common::{
        dto::ProtocolStateDelta,
        hex_bytes::Bytes,
        models::{token::Token, Chain},
        simulation::{
            errors::TransitionError,
            protocol_sim::{Balances, ProtocolSim},
        },
    };

    use super::{AerodromeV1State, AERODROME_V1_ZERO_FEE_INDICATOR_BPS};

    fn token_0() -> Token {
        Token::new(&Bytes::from([0_u8; 20]), "T0", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
    }

    fn token_1() -> Token {
        let mut addr = [0_u8; 20];
        addr[19] = 1;
        Token::new(&Bytes::from(addr), "T1", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
    }

    fn base_usdc() -> Token {
        Token::new(
            &Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn base_usdt() -> Token {
        Token::new(
            &Bytes::from_str("0xfde4c96c8593536e31f229ea8f37b2ada2699bb2").unwrap(),
            "USDT",
            6,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn base_aero() -> Token {
        Token::new(
            &Bytes::from_str("0x940181a94a35a4569e4529a3cdfb74e38fd98631").unwrap(),
            "AERO",
            18,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    #[test]
    fn test_get_amount_out_matches_real_volatile_pool_on_chain() {
        // Base Aerodrome volatile USDC/AERO pool 0x6cDcb1C4A4D1C3C6d054b27AC5B77e89eAFb971d
        // at block 44,628,997:
        // - fee: 30 bps
        // - reserves: 12_130_133_468_200 / 33_517_464_576_714_176_786_208_401
        // - getAmountOut(26_225_348_558, USDC) = 72_091_968_892_551_547_616_192
        let state = AerodromeV1State::new(
            U256::from_str("12130133468200").unwrap(),
            U256::from_str("33517464576714176786208401").unwrap(),
            false,
            30,
            6,
            18,
        );
        let out = state
            .get_amount_out(BigUint::from_str("26225348558").unwrap(), &base_usdc(), &base_aero())
            .expect("swap should succeed");

        assert_eq!(out.amount, BigUint::from_str("72091968892551547616192").unwrap());
    }

    #[test]
    fn test_delta_transition_supports_fee_only_update() {
        let mut state = AerodromeV1State::new(
            U256::from(2_000_000u32),
            U256::from(1_000_000u32),
            false,
            30,
            18,
            18,
        );
        let delta = ProtocolStateDelta {
            component_id: "pool".to_string(),
            updated_attributes: HashMap::from([(
                "fee".to_string(),
                Bytes::from(5_u32.to_be_bytes().to_vec()),
            )]),
            deleted_attributes: HashSet::new(),
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .expect("fee-only update should succeed");
        assert_eq!(state.fee, 5);
        assert_eq!(state.reserve0, U256::from(2_000_000u32));
        assert_eq!(state.reserve1, U256::from(1_000_000u32));
    }

    #[test]
    fn test_delta_transition_rejects_invalid_fee() {
        let mut state = AerodromeV1State::new(U256::ONE, U256::ONE, false, 30, 18, 18);
        let delta = ProtocolStateDelta {
            component_id: "pool".to_string(),
            updated_attributes: HashMap::from([(
                "fee".to_string(),
                Bytes::from(10_101_u32.to_be_bytes().to_vec()),
            )]),
            deleted_attributes: HashSet::new(),
        };

        let err = state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .expect_err("invalid fee should fail");
        assert!(matches!(err, TransitionError::DecodeError(_)));
    }

    #[test]
    fn test_fee_fn_returns_fraction() {
        let state = AerodromeV1State::new(U256::ONE, U256::ONE, false, 30, 18, 18);
        assert_eq!(state.fee(), 0.003);
        let state = AerodromeV1State::new(U256::ONE, U256::ONE, true, 5, 18, 18);
        assert_eq!(state.fee(), 0.0005);
    }

    #[test]
    fn test_protocol_fee_accepts_zero_fee_indicator() {
        let state = AerodromeV1State::new(
            U256::ONE,
            U256::ONE,
            true,
            AERODROME_V1_ZERO_FEE_INDICATOR_BPS,
            18,
            18,
        );
        assert!(state.protocol_fee().is_ok());
        assert_eq!(state.fee(), 0.0);
    }

    #[test]
    fn test_protocol_fee_rejects_out_of_range() {
        let state = AerodromeV1State::new(U256::ONE, U256::ONE, false, 10_001, 18, 18);
        assert!(state.protocol_fee().is_err());
    }

    #[test]
    fn test_fee_defaults_when_custom_fee_missing() {
        let state = AerodromeV1State::new(U256::ONE, U256::ONE, false, 0, 18, 18);
        assert_eq!(state.fee(), 0.003);
        let stable_state = AerodromeV1State::new(U256::ONE, U256::ONE, true, 0, 18, 18);
        assert_eq!(stable_state.fee(), 0.0005);
    }

    #[test]
    fn test_get_amount_out_no_fee() {
        let state = AerodromeV1State::new(
            U256::from(10_000u32),
            U256::from(10_000u32),
            false,
            AERODROME_V1_ZERO_FEE_INDICATOR_BPS,
            18,
            18,
        );
        let out = state
            .get_amount_out(BigUint::one(), &token_0(), &token_1())
            .expect("swap should succeed");
        assert_eq!(
            out.amount,
            BigUint::one() * BigUint::from(10_000u32) / BigUint::from(10_001u32)
        );
    }

    #[test]
    fn test_get_amount_out_stable_uses_cfmm_curve() {
        let state = AerodromeV1State::new(
            U256::from(2_642_455_102_346_776_307_825u128),
            U256::from(3_320_301_880_379_841_502_303u128),
            true,
            5,
            18,
            18,
        );

        let out = state
            .get_amount_out(BigUint::from(2_000_000_000_000_000_000u128), &token_0(), &token_1())
            .expect("stable swap should succeed");

        assert_eq!(out.amount, BigUint::from(2_004_830_151_166_915_124u128));
    }

    #[test]
    fn test_get_amount_out_matches_real_stable_pool_on_chain() {
        // Base Aerodrome stable USDC/USDT pool 0x96508AE8037c6bD16162620187691F1c1e3e07C1
        // at block 44,629,732:
        // - fee: 5 bps
        // - reserves: 2_170_141_538 / 2_029_164_659
        // - getAmountOut(123_456_789, USDC) = 123_320_126
        let state = AerodromeV1State::new(
            U256::from(2_170_141_538u32),
            U256::from(2_029_164_659u32),
            true,
            5,
            6,
            6,
        );

        let out = state
            .get_amount_out(BigUint::from(123_456_789u32), &base_usdc(), &base_usdt())
            .expect("stable swap should succeed");

        assert_eq!(out.amount, BigUint::from(123_320_126u32));
    }

    #[test]
    fn test_marginal_price_matches_spot_price() {
        use tycho_common::simulation::swap::{MarginalPriceParams, SwapQuoter};

        let usdc = base_usdc();
        let aero = base_aero();
        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![usdc.address.clone(), aero.address.clone()];
        let all_tokens = HashMap::from([
            (usdc.address.clone(), usdc.clone()),
            (aero.address.clone(), aero.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = AerodromeV1State::new(
            U256::from_str("12130133468200").unwrap(),
            U256::from_str("33517464576714176786208401").unwrap(),
            false,
            30,
            6,
            18,
        )
        .with_component(component);

        for (base, quote) in [(&usdc, &aero), (&aero, &usdc)] {
            let spot = state.spot_price(base, quote).unwrap();
            let marginal = state
                .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
                .unwrap();
            approx::assert_ulps_eq!(marginal.price(), spot);
        }
    }

    #[rstest::rstest]
    #[case::volatile_zero_for_one(false, true)]
    #[case::volatile_one_for_zero(false, false)]
    #[case::stable_zero_for_one(true, true)]
    #[case::stable_one_for_zero(true, false)]
    fn test_swap_quoter_matches_protocol_sim(#[case] stable: bool, #[case] zero_for_one: bool) {
        use tycho_common::simulation::swap::{LimitsParams, QuoteParams, SwapQuoter};

        use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint};

        let fee = if stable { 5 } else { 30 };
        let state = AerodromeV1State::new(
            U256::from_str("2642455102346776307825").unwrap(),
            U256::from_str("3320301880379841502303").unwrap(),
            stable,
            fee,
            18,
            18,
        );
        let (token_in, token_out) =
            if zero_for_one { (token_0(), token_1()) } else { (token_1(), token_0()) };

        for amount in ["1000000000000000000", "50000000000000000000"] {
            let amount = BigUint::from_str(amount).unwrap();

            let legacy = state
                .get_amount_out(amount.clone(), &token_in, &token_out)
                .unwrap();

            let quote = state
                .quote(
                    QuoteParams::fixed_in(
                        &token_in.address,
                        &token_out.address,
                        biguint_to_u256(&amount),
                    )
                    .unwrap()
                    .with_new_state(),
                )
                .unwrap();

            assert_eq!(u256_to_biguint(quote.amount_out()), legacy.amount);
            assert_eq!(BigUint::from(quote.gas()), legacy.gas);

            let legacy_new = legacy
                .new_state
                .as_any()
                .downcast_ref::<AerodromeV1State>()
                .unwrap();
            #[allow(deprecated)]
            let quote_new = quote
                .new_state()
                .unwrap()
                .to_protocol_sim();
            let quote_new = quote_new
                .as_any()
                .downcast_ref::<AerodromeV1State>()
                .unwrap();
            assert_eq!(quote_new.reserve0, legacy_new.reserve0);
            assert_eq!(quote_new.reserve1, legacy_new.reserve1);
        }

        let (legacy_in, legacy_out) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();
        let limits = state
            .swap_limits(LimitsParams::new(&token_in.address, &token_out.address))
            .unwrap();
        assert_eq!(u256_to_biguint(limits.range_in().upper()), legacy_in);
        assert_eq!(u256_to_biguint(limits.range_out().upper()), legacy_out);
    }
}
