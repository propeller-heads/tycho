use std::{any::Any, collections::HashMap};

use alloy::primitives::{aliases::U112, U256};
use num_bigint::BigUint;
use num_traits::Zero;
use ruint::UintTryFrom;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::evm::protocol::{
    cpmm::protocol::{
        cpmm_fee, cpmm_get_amount_out, cpmm_spot_price, cpmm_swap_to_price, ProtocolFee,
    },
    u256_num::{biguint_to_uint, u256_to_biguint},
    utils::add_fee_markup,
};

const SWAP_BASE_GAS: u64 = 90_000;
const UNISWAP_V2_FEE_BPS: u32 = 30; // 0.3% fee
const FEE_PRECISION: U256 = U256::from_limbs([10000, 0, 0, 0]);
const FEE_NUMERATOR: U256 = U256::from_limbs([9970, 0, 0, 0]);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniswapV2State {
    pub reserve0: U112,
    pub reserve1: U112,
}

impl UniswapV2State {
    /// Creates a new instance of `UniswapV2State` with the given reserves.
    ///
    /// # Arguments
    ///
    /// * `reserve0` - Reserve of token 0.
    /// * `reserve1` - Reserve of token 1.
    pub fn new(reserve0: U112, reserve1: U112) -> Self {
        UniswapV2State { reserve0, reserve1 }
    }

    fn reserves(&self, zero_for_one: bool) -> (U112, U112) {
        if zero_for_one {
            (self.reserve0, self.reserve1)
        } else {
            (self.reserve1, self.reserve0)
        }
    }
}

/// Reads a reserve attribute, refusing one wider than the pair's `uint112`. The package writes
/// it as `BigInt::to_signed_bytes_be`, which prefixes a zero sign byte to a reserve whose top
/// bit is set, so leading zeros are skipped.
pub(super) fn decode_reserve(name: &str, value: &Bytes) -> Result<U112, String> {
    let magnitude = &value[value
        .iter()
        .take_while(|byte| **byte == 0)
        .count()..];
    U112::try_from_be_slice(magnitude)
        .ok_or_else(|| format!("{name} {value} does not fit the pair's uint112 reserve"))
}

/// What the pair pays for `amount_in`, after its 0.3% fee.
fn amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
) -> Result<U256, SimulationError> {
    cpmm_get_amount_out(
        amount_in,
        reserve_in,
        reserve_out,
        ProtocolFee::new(FEE_NUMERATOR, FEE_PRECISION),
    )
}

#[typetag::serde]
impl ProtocolSim for UniswapV2State {
    fn fee(&self) -> f64 {
        cpmm_fee(UNISWAP_V2_FEE_BPS)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let price =
            cpmm_spot_price(base, quote, U256::from(self.reserve0), U256::from(self.reserve1))?;
        Ok(add_fee_markup(price, self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in: U112 = biguint_to_uint(&amount_in)?;
        let zero2one = token_in.address < token_out.address;
        let (reserve_in, reserve_out) = self.reserves(zero2one);
        let new_reserve_in = reserve_in
            .checked_add(amount_in)
            .ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!(
                        "Amount in {amount_in} takes reserve {reserve_in} past uint112, where \
                         the pair reverts"
                    ),
                    None,
                )
            })?;
        let amount_out =
            amount_out(U256::from(amount_in), U256::from(reserve_in), U256::from(reserve_out))?;
        // The constant-product output is below `reserve_out`, so it fits and the subtraction
        // cannot underflow.
        let new_reserve_out = U112::uint_try_from(amount_out)
            .ok()
            .and_then(|amount_out| reserve_out.checked_sub(amount_out))
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "Amount out {amount_out} exceeds reserve {reserve_out}"
                ))
            })?;
        let mut new_state = self.clone();
        if zero2one {
            new_state.reserve0 = new_reserve_in;
            new_state.reserve1 = new_reserve_out;
        } else {
            new_state.reserve0 = new_reserve_out;
            new_state.reserve1 = new_reserve_in;
        };
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(SWAP_BASE_GAS),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        // The pair takes any input until the input reserve would pass uint112, so the limit is the
        // room left below it.
        let (reserve_in, reserve_out) = self.reserves(sell_token < buy_token);
        // FIXME: the pair checks its token balances against uint112, not its reserves, so tokens
        // sent to it without a `sync` take up part of this room. The state holds only the reserves
        // (the package indexes `Sync` events), so with such an excess the limit is too high by it,
        // and `get_amount_out` accepts amounts the pair refuses with `UniswapV2: OVERFLOW`.
        let room = U112::MAX - reserve_in;
        if reserve_in.is_zero() || reserve_out.is_zero() || room.is_zero() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let room = U256::from(room);
        let max_out = amount_out(room, U256::from(reserve_in), U256::from(reserve_out))?;
        Ok((u256_to_biguint(room), u256_to_biguint(max_out)))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // Both reserves are in every delta the package emits.
        let reserve = |name: &str| {
            let value = delta
                .updated_attributes
                .get(name)
                .ok_or_else(|| TransitionError::MissingAttribute(name.to_string()))?;
            decode_reserve(name, value).map_err(TransitionError::DecodeError)
        };
        let (reserve0, reserve1) = (reserve("reserve0")?, reserve("reserve1")?);
        self.reserve0 = reserve0;
        self.reserve1 = reserve1;
        Ok(())
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
                let (reserve_in, reserve_out) = self.reserves(zero2one);
                let (reserve_in, reserve_out) = (U256::from(reserve_in), U256::from(reserve_out));

                let fee = ProtocolFee::new(FEE_NUMERATOR, FEE_PRECISION);
                let (amount_in, _) = cpmm_swap_to_price(reserve_in, reserve_out, price, fee)?;
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
                "UniswapV2State does not support TradeLimitPrice constraint in query_pool_swap"
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
            let (self_reserve0, self_reserve1) = (self.reserve0, self.reserve1);
            let (other_reserve0, other_reserve1) = (other_state.reserve0, other_state.reserve1);
            self_reserve0 == other_reserve0 &&
                self_reserve1 == other_reserve1 &&
                self.fee() == other_state.fee()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use approx::assert_ulps_eq;
    use num_bigint::BigUint;
    use num_traits::One;
    use rstest::rstest;
    use tycho_common::{
        dto::ProtocolStateDelta,
        hex_bytes::Bytes,
        models::{token::Token, Chain},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, Price, ProtocolSim},
        },
    };

    use super::*;
    use crate::evm::protocol::u256_num::biguint_to_uint;

    fn token_0() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            "T0",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn token_1() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "T1",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    #[rstest]
    #[case::same_dec(
        U112::from_str("6770398782322527849696614").unwrap(),
        U112::from_str("5124813135806900540214").unwrap(),
        18,
        18,
    BigUint::from_str("10000000000000000000000").unwrap(),
    BigUint::from_str("7535635391574243447").unwrap()
    )]
    #[case::diff_dec(
        U112::from_str("33372357002392258830279").unwrap(),
        U112::from_str("43356945776493").unwrap(),
        18,
        6,
    BigUint::from_str("10000000000000000000").unwrap(),
    BigUint::from_str("12949029867").unwrap()
    )]
    fn test_get_amount_out(
        #[case] r0: U112,
        #[case] r1: U112,
        #[case] token_0_decimals: u32,
        #[case] token_1_decimals: u32,
        #[case] amount_in: BigUint,
        #[case] exp: BigUint,
    ) {
        let t0 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            "T0",
            token_0_decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let t1 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "T0",
            token_1_decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let state = UniswapV2State::new(r0, r1);

        let res = state
            .get_amount_out(amount_in.clone(), &t0, &t1)
            .unwrap();

        assert_eq!(res.amount, exp);
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<UniswapV2State>()
            .unwrap();
        assert_eq!(new_state.reserve0, r0 + biguint_to_uint(&amount_in).unwrap());
        assert_eq!(new_state.reserve1, r1 - biguint_to_uint(&exp).unwrap());
        // Assert that the old state is unchanged
        assert_eq!(state.reserve0, r0);
        assert_eq!(state.reserve1, r1);
    }

    #[rstest]
    #[case(true, 0.000823442321727627)] // 0.0008209719947624441 / 0.997
    #[case(false, 1221.7335469177287)] // 1218.0683462769755 / 0.997
    fn test_spot_price(#[case] zero_to_one: bool, #[case] exp: f64) {
        let state = UniswapV2State::new(
            U112::from_str("36925554990922").unwrap(),
            U112::from_str("30314846538607556521556").unwrap(),
        );
        let usdc = Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let res = if zero_to_one {
            state.spot_price(&usdc, &weth).unwrap()
        } else {
            state.spot_price(&weth, &usdc).unwrap()
        };

        assert_ulps_eq!(res, exp);
    }

    #[test]
    fn test_fee() {
        let state = UniswapV2State::new(
            U112::from_str("36925554990922").unwrap(),
            U112::from_str("30314846538607556521556").unwrap(),
        );

        let res = state.fee();

        assert_ulps_eq!(res, 0.003);
    }

    #[test]
    fn test_delta_transition() {
        let mut state =
            UniswapV2State::new(U112::from_str("1000").unwrap(), U112::from_str("1000").unwrap());
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve0".to_string(), Bytes::from(1500_u64.to_be_bytes().to_vec())),
            ("reserve1".to_string(), Bytes::from(2000_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(), // usv2 doesn't have any deletable attributes
        };

        let res = state.delta_transition(delta, &HashMap::new(), &Balances::default());

        assert!(res.is_ok());
        assert_eq!(state.reserve0, U112::from_str("1500").unwrap());
        assert_eq!(state.reserve1, U112::from_str("2000").unwrap());
    }

    #[test]
    fn test_delta_transition_missing_attribute() {
        let mut state =
            UniswapV2State::new(U112::from_str("1000").unwrap(), U112::from_str("1000").unwrap());
        let attributes: HashMap<String, Bytes> =
            vec![("reserve0".to_string(), Bytes::from(1500_u64.to_be_bytes().to_vec()))]
                .into_iter()
                .collect();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        let res = state.delta_transition(delta, &HashMap::new(), &Balances::default());

        assert!(res.is_err());
        // assert it errors for the missing reserve1 attribute delta
        match res {
            Err(e) => {
                assert!(matches!(e, TransitionError::MissingAttribute(ref x) if x=="reserve1"))
            }
            _ => panic!("Test failed: was expecting an Err value"),
        };
    }

    #[rstest]
    // The smallest amount wider than uint112, which no reserve has room for.
    #[case::two_pow_112(BigUint::one() << 112)]
    #[case::two_pow_256(BigUint::one() << 256)]
    fn test_get_amount_out_refuses_amount_wider_than_uint112(#[case] amount_in: BigUint) {
        let state = UniswapV2State::new(U112::ZERO, U112::from(2000u32));

        let res = state.get_amount_out(amount_in, &token_0(), &token_1());

        assert!(matches!(res, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn test_get_amount_out_refuses_reserve_past_uint112() {
        // uint112 max is 5192296858534827628530496329220095; 1000 units of room are left.
        let state = UniswapV2State::new(
            U112::from_str("5192296858534827628530496329219095").unwrap(),
            U112::from_str("2596148429267413814265248164610048").unwrap(),
        );

        // floor(1000 * 9970 * 2^111 / (reserve0 * 10000 + 1000 * 9970))
        //   = floor(25883599839796115728224524201162178560000
        //           / 51922968585348276285304963292200920000) = 498
        let at_room = state
            .get_amount_out(BigUint::from(1000u32), &token_0(), &token_1())
            .unwrap();
        assert_eq!(at_room.amount, BigUint::from(498u32));

        let past_room = state.get_amount_out(BigUint::from(1001u32), &token_0(), &token_1());
        assert!(matches!(past_room, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn test_get_limits_is_room_below_uint112() {
        let state = UniswapV2State::new(U112::from(1000u32), U112::from(2000u32));

        // uint112 max is 5192296858534827628530496329220095.
        // token 0 -> token 1, reserve_in = 1000, reserve_out = 2000:
        // max_in = uint112 max - 1000 = 5192296858534827628530496329219095
        // max_out = floor(max_in * 9970 * 2000 / (1000 * 10000 + max_in * 9970))
        //         = floor(103534399359184462912898096804628754300000
        //                 / 51767199679592231456449048402324377150) = 1999
        let max_in = BigUint::from_str("5192296858534827628530496329219095").unwrap();
        let limits = state
            .get_limits(token_0().address, token_1().address)
            .unwrap();
        assert_eq!(limits, (max_in.clone(), BigUint::from(1999u32)));
        let quote = state
            .get_amount_out(max_in.clone(), &token_0(), &token_1())
            .unwrap();
        assert_eq!(quote.amount, BigUint::from(1999u32));
        let past = state.get_amount_out(max_in + 1u32, &token_0(), &token_1());
        assert!(matches!(past, Err(SimulationError::InvalidInput(..))));

        // token 1 -> token 0, reserve_in = 2000, reserve_out = 1000:
        // max_in = uint112 max - 2000 = 5192296858534827628530496329218095
        // max_out = floor(max_in * 9970 * 1000 / (2000 * 10000 + max_in * 9970))
        //         = floor(51767199679592231456449048402304407150000
        //                 / 51767199679592231456449048402324407150) = 999
        let max_in = BigUint::from_str("5192296858534827628530496329218095").unwrap();
        let limits = state
            .get_limits(token_1().address, token_0().address)
            .unwrap();
        assert_eq!(limits, (max_in.clone(), BigUint::from(999u32)));
        let past = state.get_amount_out(max_in + 1u32, &token_1(), &token_0());
        assert!(matches!(past, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn test_get_limits_is_zero_when_a_reserve_is_empty_or_full() {
        let max = U112::from_str("5192296858534827628530496329220095").unwrap();
        for (reserve0, reserve1) in [(U112::ZERO, U112::from(2000u32)), (max, U112::from(2000u32))]
        {
            let limits = UniswapV2State::new(reserve0, reserve1)
                .get_limits(token_0().address, token_1().address)
                .unwrap();
            assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO), "{reserve0} / {reserve1}");
        }
    }

    #[test]
    fn test_swap_to_price_below_spot() {
        // Pool with reserve0=2000000, reserve1=1000000
        // Current price: reserve1/reserve0 = 1000000/2000000 = 0.5 token_out per token_in
        let state = UniswapV2State::new(U112::from(2_000_000u32), U112::from(1_000_000u32));

        let token_in = token_0();
        let token_out = token_1();

        // Target price: 2/5 = 0.4 token_out per token_in (lower than current 0.5)
        // Selling token_in decreases price from 0.5 down to 0.4
        let target_price = Price::new(BigUint::from(2u32), BigUint::from(5u32));
        let params = &QueryPoolSwapParams::new(
            token_in,
            token_out,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let pool_swap = state.query_pool_swap(params).unwrap();

        assert_eq!(
            *pool_swap.amount_in(),
            BigUint::from(232711u32),
            "Should require some input amount"
        );
        assert_eq!(*pool_swap.amount_out(), BigUint::from(103947u32));

        // Verify that swapping this amount brings us close to the target price
        let new_state = pool_swap
            .new_state()
            .as_any()
            .downcast_ref::<UniswapV2State>()
            .unwrap();

        // The new reserves should reflect the target price
        // Price = reserve1/reserve0, so if price = 0.4, then reserve0/reserve1 = 2.5
        let new_reserve_ratio =
            new_state.reserve0.to::<u128>() as f64 / new_state.reserve1.to::<u128>() as f64;
        let expected_ratio = 2.5;

        // Allow for some difference due to fees and rounding
        assert!(
            (new_reserve_ratio - expected_ratio).abs() < 0.01,
            "New reserve ratio {new_reserve_ratio} should be close to expected {expected_ratio}"
        );
    }

    #[test]
    fn test_swap_to_price_unreachable() {
        // Pool with 2:1 ratio
        let state = UniswapV2State::new(U112::from(2_000_000u32), U112::from(1_000_000u32));

        let token_in = token_0();
        let token_out = token_1();
        // Target price is unreachable (should return error)
        // Current pool price: reserve_out/reserve_in = 1000000/2000000 = 0.5 token_out per
        // token_in Target: 1/1 = 1.0 token_out per token_in
        // Selling token_in decreases pool price, so we can't reach 1.0 from 0.5
        let target_price = Price::new(BigUint::from(1u32), BigUint::from(1u32));

        let result = state.query_pool_swap(&QueryPoolSwapParams::new(
            token_in,
            token_out,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));

        assert!(result.is_err(), "Should return error when target price is unreachable");
    }

    #[test]
    fn test_swap_to_price_at_spot_price() {
        let state = UniswapV2State::new(U112::from(2_000_000u32), U112::from(1_000_000u32));

        let token_in = token_0();
        let token_out = token_1();

        // Calculate spot price with fee (token_out/token_in):
        // Marginal price = (FEE_NUMERATOR * reserve_out) / (FEE_PRECISION * reserve_in)
        let spot_price_num = U256::from(1_000_000u32) * FEE_NUMERATOR;
        let spot_price_den = U256::from(2_000_000u32) * FEE_PRECISION;

        let target_price =
            Price::new(u256_to_biguint(spot_price_num), u256_to_biguint(spot_price_den));

        let pool_swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_in,
                token_out,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();

        // At exact spot price, we should return zero amount
        assert_eq!(
            *pool_swap.amount_in(),
            BigUint::ZERO,
            "At spot price should require zero input amount"
        );
        assert_eq!(
            *pool_swap.amount_out(),
            BigUint::ZERO,
            "At spot price should return zero output amount"
        );
    }

    #[test]
    fn test_swap_to_price_slightly_below_spot() {
        let state = UniswapV2State::new(U112::from(2_000_000u32), U112::from(1_000_000u32));

        let token_in = token_0();
        let token_out = token_1();

        // Calculate spot price with fee and subtract small amount to move slightly below
        // Current spot (token_out/token_in with fees): (FEE_NUMERATOR * reserve_out) /
        // (FEE_PRECISION * reserve_in) Target: slightly below current spot (multiply numerator by
        // 99999/100000)
        let spot_price_num = U256::from(1_000_000u32) * FEE_NUMERATOR * U256::from(99_999u32);
        let spot_price_den = U256::from(2_000_000u32) * FEE_PRECISION * U256::from(100_000u32);

        let target_price =
            Price::new(u256_to_biguint(spot_price_num), u256_to_biguint(spot_price_den));

        let pool_swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_in,
                token_out,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();

        assert!(
            *pool_swap.amount_in() > BigUint::ZERO,
            "Should return non-zero amount for target slightly below spot"
        );
    }

    #[test]
    fn test_swap_to_price_large_pool() {
        // Test with realistic large reserves
        let state = UniswapV2State::new(
            U112::from_str("6770398782322527849696614").unwrap(),
            U112::from_str("5124813135806900540214").unwrap(),
        );

        let token_in = token_0();
        let token_out = token_1();

        // Current price (token_out/token_in) = reserve1/reserve0
        // To target a slightly lower price (move price down 10%), we can use:
        // target_price = (reserve1 * 9) / (reserve0 * 10)
        // This avoids floating point precision issues with large numbers
        let price_numerator = BigUint::from(state.reserve1) * BigUint::from(9u32);
        let price_denominator = BigUint::from(state.reserve0) * BigUint::from(10u32);

        let target_price = Price::new(price_numerator, price_denominator);

        let pool_swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_in,
                token_out,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();

        assert!(pool_swap.amount_in().clone() > BigUint::ZERO, "Should require some input amount");
        assert!(pool_swap.amount_out().clone() > BigUint::ZERO, "Should get some output");
    }

    #[test]
    fn test_swap_to_price_basic() {
        let state = UniswapV2State::new(U112::from(1_000_000u32), U112::from(2_000_000u32));

        let token_in = token_0();
        let token_out = token_1();

        let target_price = Price::new(BigUint::from(2u32), BigUint::from(3u32));

        let pool_swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_in,
                token_out,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();
        assert!(*pool_swap.amount_in() > BigUint::ZERO, "Amount in should be non-zero");
        assert!(*pool_swap.amount_out() > BigUint::ZERO, "Amount out should be non-zero");
    }

    #[test]
    fn test_swap_to_price_validates_actual_output() {
        // Test that query_supply validates actual_output >= expected_output
        let state = UniswapV2State::new(
            U112::from(1_000_000u128) * U112::from(1_000_000_000_000_000_000u128),
            U112::from(2_000_000u128) * U112::from(1_000_000_000_000_000_000u128),
        );

        let token_in = token_0();
        let token_out = token_1();

        // Current pool price: 2M/1M = 2.0 token_out per token_in
        // Target: slightly lower (e.g., 1.95:1 = 1_950_000/1_000_000)
        // This is reachable by selling token_in
        let target_price = Price::new(BigUint::from(1_950_000u128), BigUint::from(1_000_000u128));

        let pool_swap = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_in,
                token_out,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();
        assert!(
            *pool_swap.amount_out() > BigUint::ZERO,
            "Should return amount out for valid price"
        );
        assert!(*pool_swap.amount_in() > BigUint::ZERO, "Should return amount in for valid price");
    }

    #[test]
    fn test_swap_around_spot_price() {
        let usdc = Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let dai = Token::new(
            &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            "DAI",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let reserve_0 = U112::from_str("735952457913070155214197").unwrap();
        let reserve_1 = U112::from_str("735997725943000000000000").unwrap();

        let pool = UniswapV2State::new(reserve_0, reserve_1);

        // Reserves: reserve_0 = DAI, reserve_1 = USDC (DAI address < USDC address)
        let reserve_usdc = reserve_1;
        let reserve_dai = reserve_0;

        // Calculate spot price (USDC/DAI with fee)
        let spot_price_dai_per_usdc_num = reserve_dai
            .checked_mul(U112::from(1000u32))
            .unwrap();
        let spot_price_dai_per_usdc_den = reserve_usdc
            .checked_mul(U112::from(1003u32))
            .unwrap();

        // Test 1: Price above reachable limit (more DAI per USDC than pool can provide) -should
        // return error Multiply by 1001/1000 to go above reachable limit
        let above_limit_num = spot_price_dai_per_usdc_num
            .checked_mul(U112::from(1001u32))
            .unwrap();
        let above_limit_den = spot_price_dai_per_usdc_den
            .checked_mul(U112::from(1000u32))
            .unwrap();
        let target_price =
            Price::new(BigUint::from(above_limit_num), BigUint::from(above_limit_den));

        let swap_above_limit = pool.query_pool_swap(&QueryPoolSwapParams::new(
            usdc.clone(),
            dai.clone(),
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));
        assert!(swap_above_limit.is_err(), "Should return error for price above reachable limit");

        // Test 2: Price just below reachable limit - should return non-zero
        // Multiply by 100_000/100_001 to go slightly below (more reachable)
        let below_limit_num = spot_price_dai_per_usdc_num
            .checked_mul(U112::from(100_000u32))
            .unwrap();
        let below_limit_den = spot_price_dai_per_usdc_den
            .checked_mul(U112::from(100_001u32))
            .unwrap();
        let target_price =
            Price::new(BigUint::from(below_limit_num), BigUint::from(below_limit_den));

        let swap_below_limit = pool
            .query_pool_swap(&QueryPoolSwapParams::new(
                usdc.clone(),
                dai.clone(),
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();

        assert!(
            swap_below_limit.amount_out().clone() > BigUint::ZERO,
            "Should return non-zero for reachable price"
        );

        // Verify with actual swap
        let actual_result = pool
            .get_amount_out(swap_below_limit.amount_in().clone(), &usdc, &dai)
            .unwrap();

        assert_eq!(
            biguint_to_uint(&actual_result.amount).unwrap(),
            U112::from(366839007208379339u128),
            "Should return non-zero amount"
        );
        assert!(
            actual_result.amount >= swap_below_limit.amount_out().clone(),
            "Actual swap should give at least predicted amount"
        );
    }
}
