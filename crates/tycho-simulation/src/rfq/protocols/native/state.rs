use std::{any::Any, borrow::Cow, collections::HashMap, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::FromPrimitive;
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
        levels::Levels,
        sim::{self, SwapDirection},
    },
    rfq::protocols::native::{client::NativeClient, models::NativePriceData},
};

/// Approximate gas for one Native Relay swap.
const NATIVE_SWAP_GAS: u64 = 134_000;

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct NativeState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug(skip)]
    pub(super) book: NativePriceData,
    #[debug(skip)]
    pub(super) client: Arc<NativeClient>,
}

impl NativeState {
    /// The pair's base token (the token whose amounts the price levels are quoted in).
    pub fn base_token(&self) -> &Token {
        &self.base_token
    }

    /// The pair's quote token.
    pub fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    fn enforce_minimum(
        amount: &BigUint,
        minimum: f64,
        amount_kind: &str,
    ) -> Result<(), SimulationError> {
        if minimum == 0.0 {
            return Ok(())
        }
        // Native Relay reports minimums in atomic units, matching `amount`. Round up defensively
        // because the JSON number is deserialized into an f64 by the orderbook model.
        let minimum = BigUint::from_f64(minimum.ceil()).ok_or_else(|| {
            SimulationError::FatalError(format!(
                "Can't convert Native minimum {amount_kind} amount to BigUint"
            ))
        })?;
        if amount < &minimum {
            return Err(SimulationError::RecoverableError(format!(
                "Amount below minimum {amount_kind}. Amount: {amount}, min amount: {minimum}"
            )))
        }
        Ok(())
    }

    fn direction(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<SwapDirection, SimulationError> {
        SwapDirection::require(
            &self.base_token.address,
            &self.quote_token.address,
            token_in,
            token_out,
        )
    }

    /// The ladder a swap in `direction` consumes, in the units of the token sold: the bids as
    /// published for selling base, the asks re-expressed per quote unit for selling quote.
    fn ladder(&self, direction: SwapDirection) -> Cow<'_, Levels> {
        match direction {
            SwapDirection::BaseToQuote => Cow::Borrowed(&self.book.bids),
            SwapDirection::QuoteToBase => Cow::Owned(self.book.asks.invert()),
        }
    }

    fn decimals(&self, direction: SwapDirection) -> (u32, u32) {
        match direction {
            SwapDirection::BaseToQuote => (self.base_token.decimals, self.quote_token.decimals),
            SwapDirection::QuoteToBase => (self.quote_token.decimals, self.base_token.decimals),
        }
    }
}

#[typetag::serde]
impl ProtocolSim for NativeState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = self.direction(&base.address, &quote.address)?;
        let best_bid = self
            .book
            .bids
            .first()
            .map(|lvl| lvl.price);
        let best_ask = self
            .book
            .asks
            .first()
            .map(|lvl| lvl.price);
        let average_price = match (best_bid, best_ask) {
            (Some(bid), Some(ask)) => bid.midpoint(ask),
            (Some(bid), None) => bid,
            (None, Some(ask)) => ask,
            (None, None) => {
                return Err(SimulationError::RecoverableError("No liquidity".to_string()))
            }
        };
        let spot_price = match direction {
            SwapDirection::BaseToQuote => average_price,
            SwapDirection::QuoteToBase => average_price.recip(),
        };
        if !spot_price.is_finite() || spot_price <= 0.0 {
            return Err(SimulationError::RecoverableError(
                "Native spot price is not positive and finite".to_string(),
            ))
        }
        Ok(spot_price)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let direction = self.direction(&token_in.address, &token_out.address)?;
        if amount_in == BigUint::ZERO {
            return Err(SimulationError::InvalidInput(
                "Native swap amount must be greater than zero".to_string(),
                None,
            ));
        }
        let (minimum_in, minimum_out) = match direction {
            SwapDirection::BaseToQuote => (self.book.minimum_in_base, self.book.minimum_out_quote),
            SwapDirection::QuoteToBase => (self.book.minimum_in_quote, self.book.minimum_out_base),
        };
        Self::enforce_minimum(&amount_in, minimum_in, "input")?;
        let ladder = self.ladder(direction);
        if ladder.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let amount_in = sim::to_human(&amount_in, token_in.decimals)?;
        let fill = ladder.fill(amount_in);
        let result = sim::fill_result(
            fill,
            amount_in,
            token_out.decimals,
            NATIVE_SWAP_GAS,
            self.clone_box(),
        )?;
        Self::enforce_minimum(&result.amount, minimum_out, "output")?;
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction = self.direction(&sell_token, &buy_token)?;
        let ladder = self.ladder(direction);
        if ladder.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let (sell_decimals, buy_decimals) = self.decimals(direction);
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
            .downcast_ref::<NativeState>()
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
impl IndicativelyPriced for NativeState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        Ok(self
            .client
            .request_binding_quote(&params)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{book::levels::PriceLevel, rfq::protocols::native::models::NativeSupportedChain};

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[],
            Chain::Ethereum,
            100,
        )
    }

    fn state() -> NativeState {
        let base_token = token("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", "WETH", 18);
        let quote_token = token("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "USDC", 6);
        let book = NativePriceData {
            base_address: base_token.address.clone(),
            quote_address: quote_token.address.clone(),
            minimum_in_base: 100_000_000_000.0,
            minimum_in_quote: 100.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap(),
            asks: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap(),
        };
        let client = Arc::new(NativeClient::new(
            NativeSupportedChain::Ethereum,
            "https://native.example".to_string(),
            String::new(),
            Duration::from_secs(5),
        ));

        NativeState { base_token, quote_token, book, client }
    }

    /// Selling base at the atomic input minimum is accepted, one unit below it is not; the same
    /// for selling quote against its own minimum.
    #[rstest]
    #[case::base_sell_at_minimum(true, 100_000_000_000, true)]
    #[case::base_sell_below_minimum(true, 99_999_999_999, false)]
    #[case::quote_sell_at_minimum(false, 100, true)]
    #[case::quote_sell_below_minimum(false, 99, false)]
    fn enforces_atomic_input_minimums(
        #[case] sell_base: bool,
        #[case] amount_in: u64,
        #[case] expect_ok: bool,
    ) {
        let state = state();
        let (token_in, token_out) = if sell_base {
            (&state.base_token, &state.quote_token)
        } else {
            (&state.quote_token, &state.base_token)
        };

        let result = state.get_amount_out(BigUint::from(amount_in), token_in, token_out);

        if expect_ok {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
        }
    }

    #[test]
    fn calculates_midpoint_spot_price_in_both_directions() {
        let mut state = state();
        state.book.bids = Levels::new(vec![PriceLevel { quantity: 1.0, price: 1_900.0 }]).unwrap();
        state.book.asks = Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_100.0 }]).unwrap();

        let direct = state
            .spot_price(&state.base_token, &state.quote_token)
            .unwrap();
        let inverse = state
            .spot_price(&state.quote_token, &state.base_token)
            .unwrap();

        assert_eq!(direct, 2_000.0);
        assert_eq!(inverse, direct.recip());
    }

    /// The spot price must be positive and finite: a huge finite midpoint passes in the direct
    /// direction, a subnormal one overflows to infinity when inverted and is rejected.
    #[test]
    fn requires_a_finite_spot_price() {
        let mut state = state();
        let extreme = Levels::new(vec![PriceLevel { quantity: 1e-306, price: 1e308 }]).unwrap();
        state.book.bids = extreme.clone();
        state.book.asks = extreme;
        assert_eq!(
            state
                .spot_price(&state.base_token, &state.quote_token)
                .unwrap(),
            1e308
        );

        let subnormal =
            Levels::new(vec![PriceLevel { quantity: 1.0, price: f64::from_bits(1) }]).unwrap();
        state.book.bids = subnormal.clone();
        state.book.asks = subnormal;
        assert!(matches!(
            state.spot_price(&state.quote_token, &state.base_token),
            Err(SimulationError::RecoverableError(message))
                if message.contains("not positive and finite")
        ));
    }

    #[rstest]
    #[case::sell_base(true, 2_500_000_000_000_000_000, 3_500_000_000)]
    #[case::sell_quote(false, 8_192_000_000, 2_500_000_000_000_000_000)]
    fn consumes_multiple_price_levels(
        #[case] sell_base: bool,
        #[case] amount_in: u64,
        #[case] expected_amount_out: u64,
    ) {
        let mut state = state();
        state.book.bids = Levels::new(vec![
            PriceLevel { quantity: 1.0, price: 2_000.0 },
            PriceLevel { quantity: 2.0, price: 1_000.0 },
        ])
        .unwrap();
        state.book.asks = Levels::new(vec![
            PriceLevel { quantity: 1.0, price: 2_048.0 },
            PriceLevel { quantity: 2.0, price: 4_096.0 },
        ])
        .unwrap();
        // Sell base: 1 * 2000 + 1.5 * 1000 = 3500 USDC.
        // Sell quote: 2048 buys the first WETH; the remaining 6144 buys 1.5 WETH.
        let (token_in, token_out) = if sell_base {
            (&state.base_token, &state.quote_token)
        } else {
            (&state.quote_token, &state.base_token)
        };

        let result = state
            .get_amount_out(BigUint::from(amount_in), token_in, token_out)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(expected_amount_out));
    }

    #[test]
    fn enforces_base_sell_atomic_output_minimum() {
        let mut state = state();
        state.book.minimum_out_quote = 1_000_000_000.0;
        let amount_in = BigUint::from(500_000_000_000_000_000u64);

        assert!(state
            .get_amount_out(amount_in.clone(), &state.base_token, &state.quote_token)
            .is_ok());

        state.book.minimum_out_quote = 1_000_000_001.0;
        assert!(matches!(
            state.get_amount_out(amount_in, &state.base_token, &state.quote_token),
            Err(SimulationError::RecoverableError(message)) if message.contains("minimum output")
        ));
    }

    #[test]
    fn enforces_quote_sell_atomic_output_minimum() {
        let mut state = state();
        state.book.minimum_out_base = 500_000_000_000_000.0;
        let amount_in = BigUint::from(1_000_000u64);

        assert!(state
            .get_amount_out(amount_in.clone(), &state.quote_token, &state.base_token)
            .is_ok());

        state.book.minimum_out_base = 500_000_000_000_001.0;
        assert!(matches!(
            state.get_amount_out(amount_in, &state.quote_token, &state.base_token),
            Err(SimulationError::RecoverableError(message)) if message.contains("minimum output")
        ));
    }

    #[test]
    fn rejects_sub_unit_partial_fill() {
        let mut state = state();
        state.book.minimum_in_base = 0.0;
        state.book.bids =
            Levels::new(vec![PriceLevel { quantity: 0.5e-18, price: 2_000.0 }]).unwrap();

        let result =
            state.get_amount_out(BigUint::from(1u64), &state.base_token, &state.quote_token);

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, Some(_)))));
    }

    /// A zero amount is rejected before any ladder is consulted; Native's quote API refuses it.
    #[test]
    fn rejects_zero_amount() {
        let state = state();

        let result = state.get_amount_out(BigUint::ZERO, &state.base_token, &state.quote_token);

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    /// Limits follow the ladder of the swap direction and the decimals of the tokens on each side.
    #[rstest]
    #[case::sell_base(true, 1_000_000_000_000_000_000, 2_000_000_000)]
    #[case::sell_quote(false, 2_000_000_000, 1_000_000_000_000_000_000)]
    fn gets_limits_in_both_directions(
        #[case] sell_base: bool,
        #[case] expected_sell_limit: u64,
        #[case] expected_buy_limit: u64,
    ) {
        let state = state();
        let (token_in, token_out) = if sell_base {
            (&state.base_token, &state.quote_token)
        } else {
            (&state.quote_token, &state.base_token)
        };

        let limits = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();

        assert_eq!(limits, (BigUint::from(expected_sell_limit), BigUint::from(expected_buy_limit)));
    }

    #[test]
    fn rejects_invalid_pair() {
        let mut state = state();
        let other = token("0x1111111111111111111111111111111111111111", "OTHER", 18);

        assert!(matches!(
            state.get_amount_out(BigUint::from(1u64), &other, &state.quote_token),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert!(matches!(
            state.get_limits(other.address.clone(), state.quote_token.address.clone()),
            Err(SimulationError::InvalidInput(_, None))
        ));

        // Direction validation must win even when the book has no liquidity.
        state.book.bids = Levels::default();
        state.book.asks = Levels::default();
        assert!(matches!(
            state.spot_price(&other, &state.quote_token),
            Err(SimulationError::InvalidInput(message, None))
                if message.contains("Invalid token addresses")
        ));
    }

    #[test]
    fn reports_no_liquidity_for_empty_direction() {
        let mut state = state();
        state.book.bids = Levels::default();

        assert!(matches!(
            state.get_amount_out(
                BigUint::from(500_000_000_000_000_000u64),
                &state.base_token,
                &state.quote_token,
            ),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            state.get_limits(state.base_token.address.clone(), state.quote_token.address.clone()),
            Err(SimulationError::RecoverableError(_))
        ));
    }
}
