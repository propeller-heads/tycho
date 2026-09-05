use std::{any::Any, collections::HashMap, fmt};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
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

use crate::rfq::{
    client::RFQClient,
    protocols::native::{client::NativeClient, models::NativePriceData},
};

// `Deserialize` bypasses `new`, so it does not validate the book or remove zero-quantity levels.
// This is harmless while `delta_transition` cannot update the book; use `TryFrom`-based
// deserialization before supporting state deltas.
#[derive(Clone, Serialize, Deserialize)]
pub struct NativeState {
    pub base_token: Token,
    pub quote_token: Token,
    pub book: NativePriceData,
    pub client: NativeClient,
}

impl fmt::Debug for NativeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .finish_non_exhaustive()
    }
}

impl NativeState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        mut book: NativePriceData,
        client: NativeClient,
    ) -> Result<Self, SimulationError> {
        // Zero-quantity levels carry no liquidity and must not influence spot prices or limits.
        book.bids
            .retain(|level| level.quantity != 0.0);
        book.asks
            .retain(|level| level.quantity != 0.0);
        let state = NativeState { base_token, quote_token, book, client };
        state.validate_book()?;
        Ok(state)
    }

    fn validate_book(&self) -> Result<(), SimulationError> {
        if self.book.base_address != self.base_token.address ||
            self.book.quote_address != self.quote_token.address
        {
            return Err(SimulationError::FatalError(
                "Native book token addresses do not match state tokens".to_string(),
            ));
        }
        let minimums = [
            self.book.minimum_in_base,
            self.book.minimum_in_quote,
            self.book.minimum_out_base,
            self.book.minimum_out_quote,
        ];
        if minimums
            .iter()
            .any(|minimum| !minimum.is_finite() || *minimum < 0.0)
        {
            return Err(SimulationError::FatalError(
                "Native book contains an invalid minimum amount".to_string(),
            ));
        }
        if self
            .book
            .bids
            .iter()
            .chain(self.book.asks.iter())
            .any(|level| {
                !level.quantity.is_finite() ||
                    level.quantity < 0.0 ||
                    !level.price.is_finite() ||
                    level.price <= 0.0
            })
        {
            return Err(SimulationError::FatalError(
                "Native book contains an invalid price level".to_string(),
            ));
        }

        Ok(())
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
}

#[typetag::serde]
impl ProtocolSim for NativeState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let inverse = if base.address == self.base_token.address &&
            quote.address == self.quote_token.address
        {
            false
        } else if base.address == self.quote_token.address &&
            quote.address == self.base_token.address
        {
            true
        } else {
            return Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {}, {}",
                base.address, quote.address
            )))
        };

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

        let spot_price = if inverse { average_price.recip() } else { average_price };

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
        let is_sell_base = token_in.address == self.base_token.address &&
            token_out.address == self.quote_token.address;
        let is_sell_quote = token_in.address == self.quote_token.address &&
            token_out.address == self.base_token.address;

        if !is_sell_base && !is_sell_quote {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Invalid token addresses. Got in={}, out={}",
                    token_in.address, token_out.address
                ),
                None,
            ));
        }

        if amount_in == BigUint::ZERO {
            return Err(SimulationError::InvalidInput(
                "Native swap amount must be greater than zero".to_string(),
                None,
            ));
        }

        let (minimum_in, minimum_out) = if is_sell_base {
            (self.book.minimum_in_base, self.book.minimum_out_quote)
        } else {
            (self.book.minimum_in_quote, self.book.minimum_out_base)
        };
        Self::enforce_minimum(&amount_in, minimum_in, "input")?;

        let amount_in_f64 = amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10f64.powi(token_in.decimals as i32);

        let levels = if is_sell_base {
            self.book.bids.clone()
        } else {
            NativePriceData::invert_price_levels(&self.book.asks)
        };

        if levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let (amount_out_f64, remaining) =
            NativePriceData::get_amount_out_from_levels(amount_in_f64, &levels);

        let res = GetAmountOutResult {
            amount: BigUint::from_f64(amount_out_f64 * 10f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUint".into())
                })?,
            gas: BigUint::from(134_000u64), // Approximate standard gas for Native swap
            new_state: self.clone_box(),
        };

        if remaining > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!("Pool has not enough liquidity to support complete swap. Input amount: {}, consumed: {}", amount_in_f64, amount_in_f64 - remaining),
                Some(res),
            ));
        }

        Self::enforce_minimum(&res.amount, minimum_out, "output")?;

        Ok(res)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let is_sell_base =
            sell_token == self.base_token.address && buy_token == self.quote_token.address;
        let is_sell_quote =
            sell_token == self.quote_token.address && buy_token == self.base_token.address;

        if !is_sell_base && !is_sell_quote {
            return Err(SimulationError::InvalidInput(
                format!("Invalid token addresses. Got sell={}, buy={}", sell_token, buy_token),
                None,
            ));
        }

        let levels = if is_sell_base {
            self.book.bids.clone()
        } else {
            NativePriceData::invert_price_levels(&self.book.asks)
        };

        if levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let (total_sell_amount, total_buy_amount) =
            levels
                .iter()
                .fold((0.0, 0.0), |(sell_sum, buy_sum), level| {
                    (sell_sum + level.quantity, buy_sum + level.quantity * level.price)
                });

        let sell_decimals =
            if is_sell_base { self.base_token.decimals } else { self.quote_token.decimals };
        let buy_decimals =
            if is_sell_base { self.quote_token.decimals } else { self.base_token.decimals };

        let sell_limit = BigUint::from_f64(total_sell_amount * 10f64.powi(sell_decimals as i32))
            .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert limit to BigUInt".into())
            })?;
        let buy_limit = BigUint::from_f64(total_buy_amount * 10f64.powi(buy_decimals as i32))
            .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert limit to BigUInt".into())
            })?;

        Ok((sell_limit, buy_limit))
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
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::native::models::NativePriceLevel;

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
            bids: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
            asks: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
        };
        let client = NativeClient::new(
            Chain::Ethereum,
            String::new(),
            HashSet::new(),
            0.0,
            HashSet::new(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();

        NativeState::new(base_token, quote_token, book, client).unwrap()
    }

    #[test]
    fn accepts_base_sell_at_atomic_input_minimum() {
        let state = state();

        let result = state.get_amount_out(
            BigUint::from(100_000_000_000u64),
            &state.base_token,
            &state.quote_token,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn rejects_base_sell_below_atomic_input_minimum() {
        let state = state();

        let result = state.get_amount_out(
            BigUint::from(99_999_999_999u64),
            &state.base_token,
            &state.quote_token,
        );

        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn accepts_quote_sell_at_atomic_input_minimum() {
        let state = state();

        let result =
            state.get_amount_out(BigUint::from(100u64), &state.quote_token, &state.base_token);

        assert!(result.is_ok());
    }

    #[test]
    fn rejects_quote_sell_below_atomic_input_minimum() {
        let state = state();

        let result =
            state.get_amount_out(BigUint::from(99u64), &state.quote_token, &state.base_token);

        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn calculates_amount_out_for_base_sell() {
        let state = state();

        let result = state
            .get_amount_out(
                BigUint::from(500_000_000_000_000_000u64),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap();

        assert_eq!(result.amount, BigUint::from(1_000_000_000u64));
    }

    #[test]
    fn ignores_zero_quantity_levels() {
        let mut state = state();
        state
            .book
            .bids
            .insert(0, NativePriceLevel { quantity: 0.0, price: 1_000.0 });
        let state = NativeState::new(state.base_token, state.quote_token, state.book, state.client)
            .unwrap();

        assert_eq!(state.book.bids.len(), 1);
        assert_eq!(
            state
                .spot_price(&state.base_token, &state.quote_token)
                .unwrap(),
            2_000.0
        );

        let result = state
            .get_amount_out(
                BigUint::from(500_000_000_000_000_000u64),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap();

        assert_eq!(result.amount, BigUint::from(1_000_000_000u64));
    }

    #[test]
    fn returns_finite_spot_price_for_large_finite_levels() {
        let mut state = state();
        state.book.bids[0] = NativePriceLevel { quantity: 1e-306, price: 1e308 };
        state.book.asks[0] = NativePriceLevel { quantity: 1e-306, price: 1e308 };
        let state = NativeState::new(state.base_token, state.quote_token, state.book, state.client)
            .unwrap();

        let price = state
            .spot_price(&state.base_token, &state.quote_token)
            .unwrap();

        assert_eq!(price, 1e308);
    }

    #[test]
    fn calculates_midpoint_spot_price_in_both_directions() {
        let mut state = state();
        state.book.bids[0].price = 1_900.0;
        state.book.asks[0].price = 2_100.0;

        let direct = state
            .spot_price(&state.base_token, &state.quote_token)
            .unwrap();
        let inverse = state
            .spot_price(&state.quote_token, &state.base_token)
            .unwrap();

        assert_eq!(direct, 2_000.0);
        assert_eq!(inverse, direct.recip());
    }

    #[test]
    fn rejects_non_finite_inverted_spot_price() {
        let mut state = state();
        let smallest_positive_price = f64::from_bits(1);
        state.book.bids[0].price = smallest_positive_price;
        state.book.asks[0].price = smallest_positive_price;
        let state = NativeState::new(state.base_token, state.quote_token, state.book, state.client)
            .unwrap();

        let result = state.spot_price(&state.quote_token, &state.base_token);

        assert!(matches!(
            result,
            Err(SimulationError::RecoverableError(message))
                if message.contains("not positive and finite")
        ));
    }

    #[test]
    fn calculates_amount_out_for_quote_sell() {
        let state = state();

        let result = state
            .get_amount_out(BigUint::from(1_000_000_000u64), &state.quote_token, &state.base_token)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(500_000_000_000_000_000u64));
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
    fn returns_partial_result_when_amount_exceeds_depth() {
        let state = state();

        let result = state.get_amount_out(
            BigUint::from(2_000_000_000_000_000_000u64),
            &state.base_token,
            &state.quote_token,
        );

        match result {
            Err(SimulationError::InvalidInput(_, Some(partial))) => {
                assert_eq!(partial.amount, BigUint::from(2_000_000_000u64));
            }
            other => panic!("Expected insufficient-liquidity result, got {other:?}"),
        }
    }

    #[test]
    fn rejects_sub_unit_partial_fill() {
        let mut state = state();
        state.book.minimum_in_base = 0.0;
        state.book.bids[0].quantity = 0.5e-18;

        let result =
            state.get_amount_out(BigUint::from(1u64), &state.base_token, &state.quote_token);

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, Some(_)))));
    }

    #[test]
    fn rejects_zero_amount() {
        let state = state();

        let result = state.get_amount_out(BigUint::ZERO, &state.base_token, &state.quote_token);

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn gets_base_sell_limits() {
        let state = state();

        let limits = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        assert_eq!(limits.0, BigUint::from(1_000_000_000_000_000_000u64));
        assert_eq!(limits.1, BigUint::from(2_000_000_000u64));
    }

    #[test]
    fn gets_quote_sell_limits() {
        let state = state();

        let limits = state
            .get_limits(state.quote_token.address.clone(), state.base_token.address.clone())
            .unwrap();

        assert_eq!(limits.0, BigUint::from(2_000_000_000u64));
        assert_eq!(limits.1, BigUint::from(1_000_000_000_000_000_000u64));
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
        state.book.bids.clear();
        state.book.asks.clear();
        assert!(matches!(
            state.spot_price(&other, &state.quote_token),
            Err(SimulationError::RecoverableError(message))
                if message.contains("Invalid token addresses")
        ));
    }

    #[test]
    fn rejects_invalid_book_state() {
        let mut state = state();
        state.book.bids[0].price = 0.0;

        assert!(matches!(
            NativeState::new(state.base_token, state.quote_token, state.book, state.client),
            Err(SimulationError::FatalError(_))
        ));
    }

    #[test]
    fn rejects_invalid_output_minimum() {
        let mut state = state();
        state.book.minimum_out_base = -1.0;

        assert!(matches!(
            NativeState::new(state.base_token, state.quote_token, state.book, state.client),
            Err(SimulationError::FatalError(_))
        ));
    }

    #[test]
    fn rejects_mismatched_book_tokens() {
        let mut state = state();
        state.book.base_address = Bytes::zero(20);

        assert!(matches!(
            NativeState::new(state.base_token, state.quote_token, state.book, state.client),
            Err(SimulationError::FatalError(_))
        ));
    }

    #[test]
    fn reports_no_liquidity_for_empty_direction() {
        let mut state = state();
        state.book.bids.clear();

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
