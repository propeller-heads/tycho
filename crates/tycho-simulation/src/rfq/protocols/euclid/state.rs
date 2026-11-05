use std::{any::Any, borrow::Cow, collections::HashMap, sync::Arc};

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

use crate::{
    book::{
        all_pairs::{all_pairs_state, RefusesQuote, SignedByVenue},
        component::PairState,
        levels::Levels,
        sim::{self, SwapDirection},
    },
    rfq::protocols::euclid::{client::EuclidClient, models::EuclidBook},
};

/// Gas for one Euclid settlement: `fillOrderRFQTo` hashes the order, recovers the maker's
/// signature, sets a bit in the maker's nonce bitmap and moves both tokens. A fill measures
/// 120k on a mainnet fork with the tokens and the maker cold, which a route only makes cheaper.
const EUCLID_SWAP_GAS: u64 = 120_000;

/// The smallest trade Euclid's gateway quotes, in US dollars on the pair's quote side, the
/// stablecoin of every pair it publishes. The gateway publishes it as the size of each side's
/// first level and declines binding quotes below it, so a book refuses to price what the venue
/// would not sign.
const MIN_TRADE_USD: f64 = 2.0;

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct EuclidState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug(skip)]
    pub(super) book: EuclidBook,
    /// What one quote token is worth in US dollars when the book was published. It turns the
    /// venue's dollar floor into an amount of a token.
    pub(super) usd_per_quote_token: f64,
    #[serde(skip)]
    #[debug(skip)]
    client: Option<Arc<EuclidClient>>,
}

impl EuclidState {
    /// A book this venue prices, with the client its binding quotes go to.
    pub(super) fn new(
        base_token: Token,
        quote_token: Token,
        book: EuclidBook,
        usd_per_quote_token: f64,
        client: Arc<EuclidClient>,
    ) -> Self {
        EuclidState { base_token, quote_token, book, usd_per_quote_token, client: Some(client) }
    }

    /// The ladder a swap in `direction` consumes, in the units of the token sold: the bids as
    /// published for selling base, the asks re-expressed per quote unit for selling quote.
    fn ladder(&self, direction: SwapDirection) -> Cow<'_, Levels> {
        match direction {
            SwapDirection::BaseToQuote => Cow::Borrowed(&self.book.bids),
            SwapDirection::QuoteToBase => Cow::Owned(self.book.asks.invert()),
        }
    }
}

impl PairState for EuclidState {
    fn base_token(&self) -> &Token {
        &self.base_token
    }

    fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    fn directions(&self) -> Vec<(Bytes, Bytes)> {
        sim::directions(
            &self.base_token,
            &self.quote_token,
            !self.book.bids.is_empty(),
            !self.book.asks.is_empty(),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for EuclidState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &base.address,
            &quote.address,
        )?;
        // The mid of the best bid and ask, since this method does not care about the direction
        // of a trade; one side alone stands for the price when the other is empty.
        let best_bid = self
            .book
            .bids
            .first()
            .map(|level| level.price);
        let best_ask = self
            .book
            .asks
            .first()
            .map(|level| level.price);
        let mid = match (best_bid, best_ask) {
            (Some(bid), Some(ask)) => (bid + ask) / 2.0,
            (Some(bid), None) => bid,
            (None, Some(ask)) => ask,
            (None, None) => {
                return Err(SimulationError::RecoverableError("No liquidity available".to_string()))
            }
        };
        match direction {
            SwapDirection::BaseToQuote => Ok(mid),
            SwapDirection::QuoteToBase => Ok(1.0 / mid),
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        sim::require_positive_amount(&amount_in)?;

        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &token_in.address,
            &token_out.address,
        )?;
        let ladder = self.ladder(direction);
        let amount_in = sim::to_human(&amount_in, token_in.decimals);
        let fill = ladder.fill(amount_in);
        // The trade's worth is read on the quote side: the amount sold when quote is sold, and
        // what the ladder pays otherwise. A side with no levels takes nothing at any size, which
        // the fill below reports as the absence of liquidity it is.
        let quote_amount = match direction {
            SwapDirection::QuoteToBase => amount_in,
            SwapDirection::BaseToQuote => fill.amount_out,
        };
        let worth_usd = quote_amount * self.usd_per_quote_token;
        if fill.remaining_in < amount_in && worth_usd < MIN_TRADE_USD {
            return Err(SimulationError::RecoverableError(format!(
                "Trade is worth {worth_usd} USD, below the {MIN_TRADE_USD} USD Euclid quotes"
            )));
        }
        // The state doesn't change after a swap.
        sim::fill_result(fill, amount_in, token_out.decimals, EUCLID_SWAP_GAS, self)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction =
            SwapDirection::require(&self.base_token, &self.quote_token, &sell_token, &buy_token)?;
        let ladder = self.ladder(direction);
        // Nothing on this side is no swap at any size, so there is no limit to report.
        if ladder.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let (sell_decimals, buy_decimals) = direction.decimals(&self.base_token, &self.quote_token);
        sim::limits(&ladder, sell_decimals, buy_decimals)
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
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<EuclidState>()
        {
            self.base_token == other_state.base_token &&
                self.quote_token == other_state.quote_token &&
                self.book == other_state.book
        } else {
            false
        }
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for EuclidState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        Ok(sim::quoting_client(self.client.as_ref(), "Euclid")?
            .request_binding_quote(&params)
            .await?)
    }
}

all_pairs_state! {
    /// Euclid's books on one chain behind one component.
    ///
    /// Euclid signs its quotes itself from one maker inventory, so all of its books share one
    /// turn: a route takes one quote from the venue, and the swap that takes it spends every
    /// book behind the component.
    EuclidAllPairsState(EuclidState, SignedByVenue);
}

impl RefusesQuote for EuclidAllPairsState {}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        book::levels::PriceLevel,
        rfq::protocols::euclid::{client::EUCLID_API_URL, models::EuclidSupportedChain},
    };

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn weth() -> Token {
        token("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", "WETH", 18)
    }

    fn usdc() -> Token {
        token("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "USDC", 6)
    }

    fn ladder(levels: &[(f64, f64)]) -> Levels {
        Levels::new(
            levels
                .iter()
                .map(|&(price, quantity)| PriceLevel { price, quantity })
                .collect(),
        )
        .unwrap()
    }

    fn test_client() -> Arc<EuclidClient> {
        Arc::new(EuclidClient::new(
            EuclidSupportedChain::Ethereum,
            EUCLID_API_URL,
            None,
            Duration::from_secs(5),
        ))
    }

    /// WETH/USDC with bids 3000 x 2.0, 2900 x 2.5 and asks 3100 x 1.5, 3200 x 3.0.
    fn test_state() -> EuclidState {
        EuclidState::new(
            weth(),
            usdc(),
            EuclidBook {
                base: weth().address,
                quote: usdc().address,
                updated_at_ms: 1_757_500_000_000,
                bids: ladder(&[(3000.0, 2.0), (2900.0, 2.5)]),
                asks: ladder(&[(3100.0, 1.5), (3200.0, 3.0)]),
            },
            1.0,
            test_client(),
        )
    }

    #[test]
    fn spot_price_is_the_mid_of_the_best_levels_either_way_round() {
        let state = test_state();
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            3050.0
        );
        let inverted = state
            .spot_price(&usdc(), &weth())
            .unwrap();
        assert!((inverted - 1.0 / 3050.0).abs() < 1e-15);
    }

    #[test]
    fn spot_price_falls_back_to_the_side_that_has_levels() {
        let mut state = test_state();
        state.book.asks = Levels::default();
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            3000.0
        );
        state.book.bids = Levels::default();
        assert!(state
            .spot_price(&weth(), &usdc())
            .is_err());
    }

    #[test]
    fn get_amount_out_walks_the_ladder_on_both_sides() {
        let state = test_state();

        // 3 WETH -> 3000 * 2 + 2900 * 1 = 8900 USDC
        let sold_base = state
            .get_amount_out(BigUint::from_str("3000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        assert_eq!(sold_base.amount, BigUint::from_str("8900000000").unwrap());
        assert_eq!(sold_base.gas, BigUint::from(EUCLID_SWAP_GAS));

        // 7000 USDC -> 4650 / 3100 = 1.5 WETH, then 2350 / 3200 WETH
        let sold_quote = state
            .get_amount_out(BigUint::from_str("7000000000").unwrap(), &usdc(), &weth())
            .unwrap();
        let expected = 1.5 + 2350.0 / 3200.0;
        let got = sold_quote
            .amount
            .to_string()
            .parse::<f64>()
            .unwrap() /
            1e18;
        assert!((got - expected).abs() < 1e-12, "got {got}, expected {expected}");
    }

    #[test]
    fn more_than_the_ladder_holds_reports_the_partial_fill() {
        let result = test_state().get_amount_out(
            BigUint::from_str("10000000000000000000").unwrap(),
            &weth(),
            &usdc(),
        );
        // All 4.5 WETH of bids fill for 13250 USDC; the rest goes unfilled.
        match result {
            Err(SimulationError::InvalidInput(_, Some(partial))) => {
                assert_eq!(partial.amount, BigUint::from_str("13250000000").unwrap());
            }
            other => panic!("expected a partial fill, got {other:?}"),
        }
    }

    #[test]
    fn a_trade_below_the_dollar_floor_is_refused() {
        // 0.0005 WETH sells for 1.5 USDC, under the 2 USD the gateway quotes.
        let result = test_state().get_amount_out(
            BigUint::from_str("500000000000000").unwrap(),
            &weth(),
            &usdc(),
        );
        assert!(
            matches!(result, Err(SimulationError::RecoverableError(ref m)) if m.contains("below the 2 USD")),
            "{result:?}"
        );
        // 1 USDC buys, which is under the floor too.
        assert!(test_state()
            .get_amount_out(BigUint::from(1_000_000u64), &usdc(), &weth())
            .is_err());
    }

    #[test]
    fn rejects_a_zero_amount_and_tokens_outside_the_pair() {
        let state = test_state();
        assert!(state
            .get_amount_out(BigUint::ZERO, &weth(), &usdc())
            .is_err());
        let dai = token("0x6b175474e89094c44da98b954eedeac495271d0f", "DAI", 18);
        assert!(state
            .get_amount_out(BigUint::from(1u64), &dai, &usdc())
            .is_err());
        assert!(state
            .get_limits(dai.address.clone(), usdc().address)
            .is_err());
    }

    #[test]
    fn get_limits_sum_the_ladder_a_swap_consumes() {
        let state = test_state();
        // Selling WETH walks the bids: 4.5 WETH in for 3000 * 2 + 2900 * 2.5 = 13250 USDC.
        let (weth_in, usdc_out) = state
            .get_limits(weth().address, usdc().address)
            .unwrap();
        assert_eq!(weth_in, BigUint::from_str("4500000000000000000").unwrap());
        assert_eq!(usdc_out, BigUint::from_str("13250000000").unwrap());
        // Buying WETH walks the asks: 3100 * 1.5 + 3200 * 3 = 14250 USDC in for 4.5 WETH.
        let (usdc_in, weth_out) = state
            .get_limits(usdc().address, weth().address)
            .unwrap();
        assert_eq!(usdc_in, BigUint::from_str("14250000000").unwrap());
        assert_eq!(weth_out, BigUint::from_str("4500000000000000000").unwrap());
    }

    #[test]
    fn a_side_with_no_levels_has_nothing_to_offer() {
        let mut state = test_state();
        state.book.bids = Levels::default();
        assert_eq!(state.directions(), vec![(usdc().address, weth().address)]);
        assert!(state
            .get_limits(weth().address, usdc().address)
            .is_err());
        assert!(state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &weth(), &usdc())
            .is_err());
    }

    #[test]
    fn eq_compares_the_pair_and_its_book() {
        let state = test_state();
        assert!(ProtocolSim::eq(&state, &test_state()));
        let mut moved = test_state();
        moved.book.bids = ladder(&[(2999.0, 2.0)]);
        assert!(!ProtocolSim::eq(&state, &moved));
    }

    #[tokio::test]
    async fn a_replayed_state_prices_but_cannot_quote() {
        let state = test_state();
        let replayed: EuclidState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();

        assert!(ProtocolSim::eq(&state, &replayed));
        assert!(replayed
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &weth(), &usdc())
            .is_ok());

        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("1000000000000000000").unwrap(),
            token_in: weth().address,
            token_out: usdc().address,
            sender: Bytes::zero(20),
            receiver: Bytes::zero(20),
            origin: None,
        };
        assert!(replayed
            .request_signed_quote(params)
            .await
            .is_err());
    }
}
