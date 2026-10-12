use std::{any::Any, collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::GetAmountOutParams, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::rfq::protocols::{
    liquorice::client::LiquoriceClient,
    maker_price_levels::{AllMakerPriceLevels, MakerPriceLevels, MakerPricing},
};

/// Liquorice's liquidity on one chain: every market maker's levels on every pair it quotes.
///
/// A swap takes its quote from one market maker and marks that maker used in the state it
/// returns. A maker's second quote does not account for its first fill, so by default a later
/// swap on that state goes to another maker.
#[derive(Clone, Serialize, Deserialize)]
pub struct LiquoriceAllPairsState {
    pub price_levels: AllMakerPriceLevels,
    pub client: Arc<LiquoriceClient>,
}

impl fmt::Debug for LiquoriceAllPairsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiquoriceAllPairsState")
            .field("price_levels", &self.price_levels)
            .finish_non_exhaustive()
    }
}

impl LiquoriceAllPairsState {
    pub fn new(
        price_levels: Vec<MakerPriceLevels>,
        tokens: HashMap<Bytes, Token>,
        client: LiquoriceClient,
    ) -> Result<Self, SimulationError> {
        let rule = client.quote_rule();
        let price_levels =
            AllMakerPriceLevels::new(price_levels, tokens, MakerPricing::Liquorice, rule)?;
        Ok(Self { price_levels, client: Arc::new(client) })
    }
}

#[typetag::serde]
impl ProtocolSim for LiquoriceAllPairsState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.price_levels
            .spot_price(&base.address, &quote.address)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let fill =
            self.price_levels
                .best_fill(&amount_in, &token_in.address, &token_out.address)?;
        let new_state = Self {
            price_levels: self
                .price_levels
                .with_used(&fill.maker_levels.market_maker),
            client: self.client.clone(),
        };
        fill.result(134_000, Box::new(new_state))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.price_levels
            .get_limits(&sell_token, &buy_token)
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError("Not implemented".into()))
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
        let Some(other) = other
            .as_any()
            .downcast_ref::<LiquoriceAllPairsState>()
        else {
            return false;
        };
        self.price_levels == other.price_levels
    }
}

#[async_trait]
impl IndicativelyPriced for LiquoriceAllPairsState {
    /// Takes the level of the market maker `get_amount_out` picks on this state for the same
    /// amount, and no other: another maker's level could be one the route already fills against.
    ///
    /// Call it on the state the swap was simulated on, with the simulated amount. On the state
    /// that swap returned, its maker is used, so the request names another maker or fails.
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        let fill =
            self.price_levels
                .best_fill(&params.amount_in, &params.token_in, &params.token_out)?;
        Ok(self
            .client
            .request_quote(&params, Some(&fill.maker_levels.market_maker))
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::{
        models::{ComponentLayout, QuoteRule},
        protocols::test_utils::{
            maker_price_levels, mock_quote_server, quote_params, usdc, wbtc, weth, weth_amount,
            LIQUORICE_QUOTE_RESPONSE,
        },
    };

    fn client(quote_rule: QuoteRule, quote_endpoint: String) -> LiquoriceClient {
        LiquoriceClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(0),
            Duration::from_secs(1),
            300,
        )
        .unwrap()
        .with_component_layout(ComponentLayout::AllPairs)
        .with_quote_rule(quote_rule)
        .with_quote_endpoint(quote_endpoint)
    }

    /// `test_mm` holds 7 WETH, `test_mm_2` 1 WETH at a lower price.
    fn test_state(quote_rule: QuoteRule) -> LiquoriceAllPairsState {
        LiquoriceAllPairsState::new(
            vec![
                maker_price_levels(
                    "test_mm",
                    &weth(),
                    &usdc(),
                    &[(0.5, 3000.0), (1.5, 3000.0), (5.0, 2999.0)],
                ),
                maker_price_levels("test_mm_2", &weth(), &usdc(), &[(1.0, 2998.0)]),
            ],
            HashMap::from([(weth().address, weth()), (usdc().address, usdc())]),
            client(quote_rule, String::new()),
        )
        .unwrap()
    }

    #[test]
    fn get_amount_out_marks_the_maker_used() {
        let state = test_state(QuoteRule::OncePerMaker);
        let result = state
            .get_amount_out(weth_amount(1.0), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(3_000_000_000u64));
        assert_eq!(result.gas, BigUint::from(134_000u64));
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<LiquoriceAllPairsState>()
            .unwrap();
        assert_eq!(new_state.price_levels, state.price_levels.with_used("test_mm"));
    }

    #[tokio::test]
    async fn request_signed_quote_takes_the_picked_makers_level() {
        let (addr, _) = mock_quote_server(0, LIQUORICE_QUOTE_RESPONSE).await;
        // The mock quote holds one level, from `test-maker`, for 1 WETH -> WBTC.
        let state = LiquoriceAllPairsState::new(
            vec![
                maker_price_levels("test-maker", &weth(), &wbtc(), &[(1.0, 0.051)]),
                maker_price_levels("other", &weth(), &wbtc(), &[(1.0, 0.05)]),
            ],
            HashMap::from([(weth().address, weth()), (wbtc().address, wbtc())]),
            client(QuoteRule::OncePerMaker, format!("http://127.0.0.1:{}/rfq", addr.port())),
        )
        .unwrap();
        let quote = state
            .request_signed_quote(quote_params())
            .await
            .unwrap();
        assert_eq!(quote.amount_out, BigUint::from(3329502u64));

        let after_first = LiquoriceAllPairsState {
            price_levels: state
                .price_levels
                .with_used("test-maker"),
            client: state.client.clone(),
        };
        let missing = after_first
            .request_signed_quote(quote_params())
            .await
            .unwrap_err();
        assert!(
            matches!(missing, SimulationError::FatalError(msg) if msg.contains("quote not found"))
        );
    }
}
