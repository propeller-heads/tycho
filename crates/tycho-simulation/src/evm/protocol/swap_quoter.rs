//! What the native states share to answer through [`SwapQuoter`].
//!
//! A state answers through [`SwapQuoter`] only once its decoder attached the pool's component:
//! [`SwapQuoter`] identifies tokens by address alone, and the component is where a state finds the
//! decimals its marginal price is scaled by.

use std::{collections::HashMap, fmt, sync::Arc};

use alloy::primitives::U256;
use num_bigint::BigUint;
use tycho_common::{
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::SimulationError,
        protocol_sim::{self, ProtocolSim, QueryPoolSwapParams},
        swap::{
            PricePoint, QuerySwapParams, QuoteAmount, QuoteParams, Range, SimulationResult, Swap,
            SwapConstraint, SwapLimits, SwapQuoter,
        },
    },
    Bytes,
};

use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint};

/// The component a [`SwapQuoter`] describes its pool with.
pub type QuoterComponent = ProtocolComponent<Arc<Token>>;

/// The component a state answers [`SwapQuoter::component`] with, shared by the state's clones.
///
/// It describes the pool rather than the pool's state, so equality ignores it and serialization
/// skips it: a state decoded without a component equals the same state decoded with one.
#[derive(Clone, Default)]
pub(crate) struct AttachedComponent(Option<Arc<QuoterComponent>>);

impl AttachedComponent {
    /// Builds the component of a pool from its snapshot, or none when a token is unknown.
    pub(crate) fn from_snapshot(
        component: &ProtocolComponent,
        all_tokens: &HashMap<Bytes, Token>,
    ) -> Self {
        let tokens = component
            .tokens
            .iter()
            .map(|address| {
                all_tokens
                    .get(address)
                    .map(|token| Arc::new(token.clone()))
            })
            .collect::<Option<Vec<_>>>();
        let Some(tokens) = tokens else {
            return Self(None);
        };
        Self(Some(Arc::new(ProtocolComponent::new(
            &component.id,
            &component.protocol_system,
            &component.protocol_type_name,
            component.chain,
            tokens,
            component.contract_addresses.clone(),
            component.static_attributes.clone(),
            component.change,
            component.creation_tx.clone(),
            component.created_at,
        ))))
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.0.is_some()
    }

    /// The attached component.
    ///
    /// # Panics
    ///
    /// Panics when no component is attached. [`ProtocolSim::as_swap_quoter`] hands out only
    /// states that have one.
    pub(crate) fn component(&self) -> Arc<QuoterComponent> {
        Arc::clone(
            self.0
                .as_ref()
                .expect("a state quotes through SwapQuoter only with a component attached"),
        )
    }

    /// The pool token at `address`.
    pub(crate) fn token(&self, address: &Bytes) -> SimulationResult<&Token> {
        self.0
            .as_ref()
            .and_then(|component| {
                component
                    .tokens
                    .iter()
                    .find(|token| &token.address == address)
            })
            .map(Arc::as_ref)
            .ok_or_else(|| {
                SimulationError::InvalidInput(format!("token {address} is not in this pool"), None)
            })
    }
}

impl PartialEq for AttachedComponent {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for AttachedComponent {}

impl fmt::Debug for AttachedComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(component) => write!(f, "AttachedComponent({})", component.id),
            None => f.write_str("AttachedComponent(None)"),
        }
    }
}

/// What a native state supplies for [`impl_native_swap_quoter`] to answer through [`SwapQuoter`].
pub(crate) trait NativeQuote: ProtocolSim + Clone {
    fn attached_component(&self) -> &AttachedComponent;

    /// The fee a swap in this direction pays, as a fraction of its input.
    fn swap_fee(&self, zero_for_one: bool) -> f64;

    /// Quotes selling `amount_in` of `token_in` for `token_out`: the amount out, the gas, and the
    /// state the swap leaves, built only when `with_state`.
    fn quote_exact_in(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
        with_state: bool,
    ) -> SimulationResult<(U256, U256, Option<Self>)>;

    /// [`quote_exact_in`](Self::quote_exact_in) for a [`BigUint`] amount, with [`BigUint`] results.
    ///
    /// # Panics
    ///
    /// Panics when `amount_in` exceeds 256 bits.
    fn quote_exact_in_biguint(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
        with_state: bool,
    ) -> SimulationResult<(BigUint, BigUint, Option<Self>)> {
        let (amount_out, gas, new_state) =
            self.quote_exact_in(biguint_to_u256(amount_in), token_in, token_out, with_state)?;
        Ok((u256_to_biguint(amount_out), u256_to_biguint(gas), new_state))
    }
}

/// Implements [`SwapQuoter`] for a [`NativeQuote`] state. A macro rather than a generic impl,
/// because `typetag` registers concrete types only.
macro_rules! impl_native_swap_quoter {
    ($state:ty) => {
        #[typetag::serde]
        impl tycho_common::simulation::swap::SwapQuoter for $state {
            fn component(
                &self,
            ) -> std::sync::Arc<$crate::evm::protocol::swap_quoter::QuoterComponent> {
                $crate::evm::protocol::swap_quoter::NativeQuote::attached_component(self)
                    .component()
            }

            fn fee(
                &self,
                params: tycho_common::simulation::swap::QuoteParams,
            ) -> tycho_common::simulation::swap::SimulationResult<
                tycho_common::simulation::swap::SwapFee,
            > {
                let zero_for_one = params.token_in() < params.token_out();
                Ok(tycho_common::simulation::swap::SwapFee::new(
                    $crate::evm::protocol::swap_quoter::NativeQuote::swap_fee(self, zero_for_one),
                ))
            }

            fn marginal_price(
                &self,
                params: tycho_common::simulation::swap::MarginalPriceParams,
            ) -> tycho_common::simulation::swap::SimulationResult<
                tycho_common::simulation::swap::MarginalPrice,
            > {
                let component =
                    $crate::evm::protocol::swap_quoter::NativeQuote::attached_component(self);
                let base = component.token(params.token_in())?;
                let quote = component.token(params.token_out())?;
                Ok(tycho_common::simulation::swap::MarginalPrice::new(
                    tycho_common::simulation::protocol_sim::ProtocolSim::spot_price(
                        self, base, quote,
                    )?,
                ))
            }

            fn quote(
                &self,
                params: tycho_common::simulation::swap::QuoteParams,
            ) -> tycho_common::simulation::swap::SimulationResult<
                tycho_common::simulation::swap::Quote,
            > {
                let (amount_out, gas, new_state) =
                    $crate::evm::protocol::swap_quoter::NativeQuote::quote_exact_in_biguint(
                        self,
                        $crate::evm::protocol::swap_quoter::exact_input(&params)?,
                        params.token_in(),
                        params.token_out(),
                        params.should_return_new_state(),
                    )?;
                Ok(tycho_common::simulation::swap::Quote::new(
                    amount_out,
                    gas,
                    new_state.map(|state| {
                        std::sync::Arc::new(state)
                            as std::sync::Arc<dyn tycho_common::simulation::swap::SwapQuoter>
                    }),
                ))
            }

            fn quote_exact_in_u256(
                &self,
                token_in: &tycho_common::simulation::swap::TokenAddress,
                token_out: &tycho_common::simulation::swap::TokenAddress,
                amount_in: alloy::primitives::U256,
                with_state: bool,
            ) -> tycho_common::simulation::swap::SimulationResult<
                tycho_common::simulation::swap::QuoteU256,
            > {
                let (amount_out, gas, new_state) =
                    $crate::evm::protocol::swap_quoter::NativeQuote::quote_exact_in(
                        self, amount_in, token_in, token_out, with_state,
                    )?;
                Ok(tycho_common::simulation::swap::QuoteU256 {
                    amount_out,
                    gas,
                    new_state: new_state.map(|state| {
                        std::sync::Arc::new(state)
                            as std::sync::Arc<dyn tycho_common::simulation::swap::SwapQuoter>
                    }),
                })
            }

            fn swap_limits(
                &self,
                params: tycho_common::simulation::swap::LimitsParams,
            ) -> tycho_common::simulation::swap::SimulationResult<
                tycho_common::simulation::swap::SwapLimits,
            > {
                let (max_in, max_out) =
                    tycho_common::simulation::protocol_sim::ProtocolSim::get_limits(
                        self,
                        params.token_in().clone(),
                        params.token_out().clone(),
                    )?;
                $crate::evm::protocol::swap_quoter::limits_from_maxima(max_in, max_out)
            }

            fn query_swap(
                &self,
                params: tycho_common::simulation::swap::QuerySwapParams,
            ) -> tycho_common::simulation::swap::SimulationResult<
                tycho_common::simulation::swap::Swap,
            > {
                $crate::evm::protocol::swap_quoter::query_swap_through_pool_swap(self, params)
            }

            fn delta_transition(
                &mut self,
                params: tycho_common::simulation::swap::TransitionParams,
            ) -> Result<
                tycho_common::simulation::swap::Transition,
                tycho_common::simulation::errors::TransitionError,
            > {
                tycho_common::simulation::protocol_sim::ProtocolSim::delta_transition(
                    self,
                    params.delta().clone(),
                    params.tokens(),
                    params.balances(),
                )?;
                Ok(tycho_common::simulation::swap::Transition::default())
            }

            fn clone_box(&self) -> Box<dyn tycho_common::simulation::swap::SwapQuoter> {
                Box::new(self.clone())
            }

            fn to_protocol_sim(
                &self,
            ) -> Box<dyn tycho_common::simulation::protocol_sim::ProtocolSim> {
                Box::new(self.clone())
            }
        }
    };
}

pub(crate) use impl_native_swap_quoter;

/// The input amount of an exact-input quote. The native states quote exact input only.
pub(crate) fn exact_input<'a>(params: &'a QuoteParams<'_>) -> SimulationResult<&'a BigUint> {
    match params.amount() {
        QuoteAmount::FixedIn(amount) => Ok(amount),
        QuoteAmount::FixedOut(_) => Err(SimulationError::InvalidInput(
            "exact-output quotes are not supported".to_string(),
            None,
        )),
    }
}

/// Swap limits from the maximum input and output [`ProtocolSim::get_limits`] reports.
pub(crate) fn limits_from_maxima(
    max_in: BigUint,
    max_out: BigUint,
) -> SimulationResult<SwapLimits> {
    Ok(SwapLimits::new(Range::new(BigUint::ZERO, max_in)?, Range::new(BigUint::ZERO, max_out)?))
}

/// Answers [`SwapQuoter::query_swap`] with the state's [`ProtocolSim::query_pool_swap`].
pub(crate) fn query_swap_through_pool_swap<S>(
    state: &S,
    params: QuerySwapParams,
) -> SimulationResult<Swap>
where
    S: NativeQuote + SwapQuoter,
{
    let component = state.attached_component();
    let constraint = match params.swap_constraint().clone() {
        SwapConstraint::TradeLimitPrice {
            limit, tolerance, min_amount_in, max_amount_in, ..
        } => protocol_sim::SwapConstraint::TradeLimitPrice {
            limit,
            tolerance,
            min_amount_in,
            max_amount_in,
        },
        SwapConstraint::PoolTargetPrice {
            target, tolerance, min_amount_in, max_amount_in, ..
        } => protocol_sim::SwapConstraint::PoolTargetPrice {
            target,
            tolerance,
            min_amount_in,
            max_amount_in,
        },
    };
    let pool_swap = state.query_pool_swap(&QueryPoolSwapParams::new(
        component
            .token(params.token_in())?
            .clone(),
        component
            .token(params.token_out())?
            .clone(),
        constraint,
    ))?;
    let new_state = pool_swap
        .new_state()
        .as_any()
        .downcast_ref::<S>()
        .ok_or_else(|| {
            SimulationError::FatalError("query_pool_swap returned another state type".to_string())
        })?
        .clone();
    let price_points = pool_swap
        .price_points()
        .as_ref()
        .map(|points| {
            points
                .iter()
                .map(|point| {
                    PricePoint::new(point.amount_in.clone(), point.amount_out.clone(), point.price)
                })
                .collect()
        });
    Ok(Swap::new(
        pool_swap.amount_in().clone(),
        pool_swap.amount_out().clone(),
        Some(Arc::new(new_state)),
        price_points,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::str::FromStr;

    use alloy::primitives::U256;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        models::{protocol::ProtocolComponentState, Chain},
        simulation::{
            protocol_sim::Price,
            swap::{LimitsParams, MarginalPriceParams},
        },
    };

    use super::*;
    use crate::{
        evm::protocol::{
            pancakeswap_v2::state::PancakeswapV2State,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
            uniswap_v4::state::{UniswapV4Fees, UniswapV4State},
            utils::uniswap::tick_list::TickInfo,
        },
        protocol::models::{DecoderContext, TryFromWithBlock},
    };

    pub(crate) fn token(address: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            "T",
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn tokens() -> (Token, Token) {
        (
            token("0x0000000000000000000000000000000000000001", 18),
            token("0x0000000000000000000000000000000000000002", 6),
        )
    }

    pub(crate) fn component(token_0: &Token, token_1: &Token) -> AttachedComponent {
        let component = ProtocolComponent {
            id: "pool".to_string(),
            tokens: vec![token_0.address.clone(), token_1.address.clone()],
            ..Default::default()
        };
        let all_tokens = HashMap::from([
            (token_0.address.clone(), token_0.clone()),
            (token_1.address.clone(), token_1.clone()),
        ]);
        AttachedComponent::from_snapshot(&component, &all_tokens)
    }

    fn clmm_ticks() -> Vec<TickInfo> {
        vec![
            TickInfo::new(-887_220, 2_000_000_000_000_000_000).unwrap(),
            TickInfo::new(-600, 30_000_000_000_000_000_000).unwrap(),
            TickInfo::new(600, -30_000_000_000_000_000_000).unwrap(),
            TickInfo::new(887_220, -2_000_000_000_000_000_000).unwrap(),
        ]
    }

    fn states(token_0: &Token, token_1: &Token) -> Vec<Box<dyn ProtocolSim>> {
        let attached = component(token_0, token_1);
        let reserve_0 = U256::from(5_000_000_000_000_000_000_000u128);
        let reserve_1 = U256::from(15_000_000_000_000u128);
        let mut v4 = UniswapV4State::new(
            32_000_000_000_000_000_000,
            U256::from(1u8) << 96,
            UniswapV4Fees::new(100, 90, 500),
            0,
            60,
            clmm_ticks(),
        )
        .unwrap();
        v4.set_component(attached.clone());
        vec![
            Box::new(UniswapV2State::new(reserve_0, reserve_1).with_component(attached.clone())),
            Box::new(
                PancakeswapV2State::new(reserve_0, reserve_1).with_component(attached.clone()),
            ),
            Box::new(
                UniswapV3State::new(
                    32_000_000_000_000_000_000,
                    U256::from(1u8) << 96,
                    FeeAmount::Low,
                    0,
                    clmm_ticks(),
                )
                .unwrap()
                .with_component(attached),
            ),
            Box::new(v4),
        ]
    }

    fn amounts() -> Vec<BigUint> {
        [1u128, 1_000, 10u128.pow(15), 10u128.pow(21), 10u128.pow(24), 10u128.pow(30)]
            .into_iter()
            .map(BigUint::from)
            .collect()
    }

    #[test]
    fn quoter_answers_match_protocol_sim() {
        let (token_0, token_1) = tokens();
        for state in states(&token_0, &token_1) {
            assert_quoter_matches(state.as_ref(), &token_0, &token_1, &amounts());
        }
    }

    /// Asserts every [`SwapQuoter`] answer of `state` equals its [`ProtocolSim`] counterpart in
    /// both directions, including the errors and the post-swap states.
    pub(crate) fn assert_quoter_matches(
        state: &dyn ProtocolSim,
        token_0: &Token,
        token_1: &Token,
        amounts: &[BigUint],
    ) {
        let quoter = state
            .as_swap_quoter()
            .expect("a state with a component quotes");
        assert_eq!(quoter.component().id, "pool");
        for (token_in, token_out) in [(token_0, token_1), (token_1, token_0)] {
            let price = quoter
                .marginal_price(MarginalPriceParams::new(&token_in.address, &token_out.address))
                .unwrap();
            assert_eq!(
                price.price(),
                state
                    .spot_price(token_in, token_out)
                    .unwrap()
            );

            let limits = quoter
                .swap_limits(LimitsParams::new(&token_in.address, &token_out.address))
                .unwrap();
            let (max_in, max_out) = state
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .unwrap();
            assert_eq!(
                (limits.range_in().upper(), limits.range_out().upper()),
                (&max_in, &max_out)
            );

            for amount in amounts {
                let expected = state.get_amount_out(amount.clone(), token_in, token_out);
                let params = || {
                    QuoteParams::fixed_in(&token_in.address, &token_out.address, amount.clone())
                        .unwrap()
                };
                let without_state = quoter.quote(params());
                let with_state = quoter.quote(params().with_new_state());
                match expected {
                    Ok(expected) => {
                        let without_state = without_state.unwrap();
                        assert_eq!(without_state.amount_out(), &expected.amount);
                        assert_eq!(without_state.gas(), &expected.gas);
                        assert!(without_state.new_state().is_none());

                        let native = quoter
                            .quote_exact_in_u256(
                                &token_in.address,
                                &token_out.address,
                                biguint_to_u256(amount),
                                true,
                            )
                            .unwrap();
                        assert_eq!(u256_to_biguint(native.amount_out), expected.amount);
                        assert_eq!(u256_to_biguint(native.gas), expected.gas);
                        #[allow(deprecated)]
                        let native_state = native
                            .new_state
                            .unwrap()
                            .to_protocol_sim();
                        assert!(expected
                            .new_state
                            .eq(native_state.as_ref()));

                        let with_state = with_state.unwrap();
                        assert_eq!(with_state.amount_out(), &expected.amount);
                        #[allow(deprecated)]
                        let new_state = with_state
                            .new_state()
                            .unwrap()
                            .to_protocol_sim();
                        assert!(expected
                            .new_state
                            .eq(new_state.as_ref()));
                        assert!(new_state.as_swap_quoter().is_some());
                    }
                    Err(expected) => {
                        for actual in [without_state, with_state] {
                            let actual = actual.err().unwrap();
                            assert_eq!(actual.to_string(), expected.to_string());
                            if let (
                                SimulationError::InvalidInput(_, Some(actual_partial)),
                                SimulationError::InvalidInput(_, Some(expected_partial)),
                            ) = (&actual, &expected)
                            {
                                assert_eq!(actual_partial.amount, expected_partial.amount);
                                assert_eq!(actual_partial.gas, expected_partial.gas);
                                assert!(actual_partial
                                    .new_state
                                    .eq(expected_partial.new_state.as_ref()));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Asserts a delta applied through [`SwapQuoter`] leaves the state [`ProtocolSim`] leaves.
    pub(crate) fn assert_delta_transition_matches(
        state: &dyn ProtocolSim,
        delta: tycho_common::dto::ProtocolStateDelta,
    ) {
        let (tokens, balances) = (HashMap::new(), Default::default());
        let mut expected = state.clone_box();
        expected
            .delta_transition(delta.clone(), &tokens, &balances)
            .unwrap();
        let mut actual = state
            .as_swap_quoter()
            .unwrap()
            .clone_box();
        actual
            .delta_transition(tycho_common::simulation::swap::TransitionParams::new(
                delta, &tokens, &balances,
            ))
            .unwrap();
        #[allow(deprecated)]
        let actual = actual.to_protocol_sim();
        assert!(expected.eq(actual.as_ref()));
    }

    #[test]
    fn quoter_query_swap_matches_query_pool_swap() {
        let (token_0, token_1) = tokens();
        for state in states(&token_0, &token_1) {
            let quoter = state.as_swap_quoter().unwrap();
            let spot = state
                .spot_price(&token_0, &token_1)
                .unwrap();
            let target = Price::new(
                BigUint::from((spot * 0.99 * 1e6) as u64) * BigUint::from(10u64).pow(6),
                BigUint::from(10u64).pow(18) * BigUint::from(10u64).pow(6),
            );
            let expected = state.query_pool_swap(&QueryPoolSwapParams::new(
                token_0.clone(),
                token_1.clone(),
                protocol_sim::SwapConstraint::PoolTargetPrice {
                    target: target.clone(),
                    tolerance: 0.0,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ));
            let actual = quoter.query_swap(QuerySwapParams::new(
                &token_0.address,
                &token_1.address,
                SwapConstraint::pool_target_price(target, 0.0),
            ));
            match expected {
                Ok(expected) => {
                    let actual = actual.unwrap();
                    assert_eq!(actual.amount_in(), expected.amount_in());
                    assert_eq!(actual.amount_out(), expected.amount_out());
                }
                Err(expected) => {
                    assert_eq!(actual.err().unwrap().to_string(), expected.to_string())
                }
            }
        }
    }

    #[test]
    fn state_without_component_does_not_quote() {
        let state = UniswapV2State::new(U256::from(1u8), U256::from(1u8));
        assert!(state.as_swap_quoter().is_none());
        assert_eq!(
            state,
            state
                .clone()
                .with_component(component(&tokens().0, &tokens().1))
        );
    }

    #[test]
    fn exact_output_is_refused() {
        let (token_0, token_1) = tokens();
        for state in states(&token_0, &token_1) {
            let params =
                QuoteParams::fixed_out(&token_0.address, &token_1.address, BigUint::from(1u8))
                    .unwrap();
            assert!(state
                .as_swap_quoter()
                .unwrap()
                .quote(params)
                .is_err());
        }
    }

    #[tokio::test]
    async fn decoder_attaches_the_component() {
        let (token_0, token_1) = tokens();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState::new(
                "pool",
                HashMap::from([
                    ("reserve0".to_string(), Bytes::from(vec![1; 32])),
                    ("reserve1".to_string(), Bytes::from(vec![1; 32])),
                ]),
                HashMap::new(),
            ),
            component: ProtocolComponent {
                id: "pool".to_string(),
                tokens: vec![token_0.address.clone(), token_1.address.clone()],
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        let all_tokens = HashMap::from([
            (token_0.address.clone(), token_0.clone()),
            (token_1.address.clone(), token_1.clone()),
        ]);
        let state = UniswapV2State::try_from_with_header(
            snapshot,
            BlockHeader::default(),
            &HashMap::new(),
            &all_tokens,
            &DecoderContext::default(),
        )
        .await
        .unwrap();
        let quoter = state.as_swap_quoter().unwrap();
        assert_eq!(quoter.component().tokens[1].decimals, 6);
    }
}
