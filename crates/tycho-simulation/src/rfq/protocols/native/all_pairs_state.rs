use std::{any::Any, collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
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
    models::{marginal_price, to_whole_units},
    protocols::{
        fill::{continued_amount_out, continued_limits, filled_on_pair, with_new_state, PairFill},
        native::{client::NativeClient, models::NativePriceData, state::NativeState},
    },
};

/// Native Relay's liquidity on one chain: one per-pair state per book. A swap on another pair or
/// direction than an earlier fill fails: Native already quoted in this route.
#[derive(Clone, Serialize, Deserialize)]
pub struct NativeAllPairsState {
    states: Arc<Vec<NativeState>>,
    /// Every direction a book quotes, sorted, with the index of its state in `states`. A book
    /// quoting the pair as given beats one quoting it the other way round.
    directions: Arc<Vec<((Bytes, Bytes), usize)>>,
    /// The pair an earlier swap quoted through Native on this state.
    fill: Option<PairFill>,
}

impl fmt::Debug for NativeAllPairsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeAllPairsState")
            .field("books", &self.states.len())
            .field("fill", &self.fill)
            .finish_non_exhaustive()
    }
}

impl NativeAllPairsState {
    /// Skips books with a token missing from `tokens` and logs each with `debug!`, as the stream
    /// decoder already logged the token. Skips books their per-pair state rejects and logs each
    /// with `warn!`.
    pub fn new(
        books: Vec<NativePriceData>,
        tokens: HashMap<Bytes, Token>,
        client: NativeClient,
    ) -> Self {
        let mut states = Vec::with_capacity(books.len());
        for book in books {
            let (base_address, quote_address) =
                (book.base_address.clone(), book.quote_address.clone());
            let (Some(base_token), Some(quote_token)) =
                (tokens.get(&base_address), tokens.get(&quote_address))
            else {
                debug!(
                    "Skipping Native book {base_address}/{quote_address}: a token is missing from \
                     the state."
                );
                continue;
            };
            match NativeState::new(base_token.clone(), quote_token.clone(), book, client.clone()) {
                Ok(state) => states.push(state),
                Err(e) => warn!(
                    "Native book {base_address}/{quote_address} is invalid: {e}. Skipping it."
                ),
            }
        }
        let mut directions = HashMap::new();
        for (index, state) in states.iter().enumerate() {
            directions
                .entry((state.book.base_address.clone(), state.book.quote_address.clone()))
                .or_insert(index);
        }
        for (index, state) in states.iter().enumerate() {
            directions
                .entry((state.book.quote_address.clone(), state.book.base_address.clone()))
                .or_insert(index);
        }
        let mut directions: Vec<_> = directions.into_iter().collect();
        directions.sort();
        Self { states: Arc::new(states), directions: Arc::new(directions), fill: None }
    }

    /// The per-pair state that trades `token_in` for `token_out`.
    fn pair_state(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<&NativeState, SimulationError> {
        let index = self
            .directions
            .binary_search_by(|((a, b), _)| (a, b).cmp(&(token_in, token_out)))
            .map_err(|_| {
                SimulationError::InvalidInput(
                    format!("Invalid token addresses. Got in={token_in}, out={token_out}"),
                    None,
                )
            })?;
        Ok(&self.states[self.directions[index].1])
    }

    /// This state after a swap of `amount_in`. `filled` is what an earlier swap filled on the pair.
    fn with_fill(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
        filled: Option<&BigUint>,
        amount_in: &BigUint,
    ) -> Box<dyn ProtocolSim> {
        Box::new(Self {
            states: self.states.clone(),
            directions: self.directions.clone(),
            fill: Some(PairFill::after(filled, token_in, token_out, amount_in)),
        })
    }
}

/// The book's own tokens in input/output order for a swap from `token_in`.
fn pair_tokens<'a>(state: &'a NativeState, token_in: &Bytes) -> (&'a Token, &'a Token) {
    if &state.base_token.address == token_in {
        (&state.base_token, &state.quote_token)
    } else {
        (&state.quote_token, &state.base_token)
    }
}

#[typetag::serde]
impl ProtocolSim for NativeAllPairsState {
    fn fee(&self) -> f64 {
        0.0
    }

    /// After a fill on the pair, the marginal price at the filled amount.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let state = self.pair_state(&base.address, &quote.address)?;
        let Some(filled) = filled_on_pair(&self.fill, &base.address, &quote.address, "Native")?
        else {
            return state.spot_price(base, quote);
        };
        let filled = to_whole_units(filled, base.decimals)?;
        marginal_price(&state.swap_levels(&base.address), filled)
            .ok_or_else(|| SimulationError::RecoverableError("No liquidity".into()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let state = self.pair_state(&token_in.address, &token_out.address)?;
        let filled = filled_on_pair(&self.fill, &token_in.address, &token_out.address, "Native")?;
        let amount_out = |amount| state.get_amount_out(amount, token_in, token_out);
        let result = match filled {
            Some(filled) => continued_amount_out(amount_out, filled, &amount_in),
            None => amount_out(amount_in.clone()),
        };
        let new_state = self.with_fill(&token_in.address, &token_out.address, filled, &amount_in);
        with_new_state(result, new_state)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let state = self.pair_state(&sell_token, &buy_token)?;
        let filled = filled_on_pair(&self.fill, &sell_token, &buy_token, "Native")?;
        let limits = state.get_limits(sell_token.clone(), buy_token.clone())?;
        let Some(filled) = filled else {
            return Ok(limits);
        };
        let (token_in, token_out) = pair_tokens(state, &sell_token);
        let amount_out = |amount| state.get_amount_out(amount, token_in, token_out);
        continued_limits(amount_out, limits, filled)
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
            .downcast_ref::<NativeAllPairsState>()
        else {
            return false;
        };
        self.fill == other.fill &&
            self.states.len() == other.states.len() &&
            self.states
                .iter()
                .zip(other.states.iter())
                .all(|(a, b)| a.book == b.book)
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for NativeAllPairsState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        self.pair_state(&params.token_in, &params.token_out)?
            .request_signed_quote(params)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::{
        models::ComponentLayout,
        protocols::{
            native::models::NativePriceLevel,
            test_utils::{token, usdc, weth},
        },
    };

    fn book() -> NativePriceData {
        NativePriceData {
            base_address: weth().address,
            quote_address: usdc().address,
            minimum_in_base: 100_000_000_000.0,
            minimum_in_quote: 100.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
            asks: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
        }
    }

    fn state_with(books: Vec<NativePriceData>) -> NativeAllPairsState {
        let client = NativeClient::new(
            Chain::Ethereum,
            String::new(),
            HashSet::new(),
            0.0,
            HashSet::new(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_component_layout(ComponentLayout::AllPairs);
        NativeAllPairsState::new(
            books,
            HashMap::from([(weth().address, weth()), (usdc().address, usdc())]),
            client,
        )
    }

    fn state() -> NativeAllPairsState {
        state_with(vec![book()])
    }

    #[test]
    fn once_per_venue() {
        let state = state();
        let first = state
            .get_amount_out(BigUint::from(500_000_000_000_000_000u64), &weth(), &usdc())
            .unwrap();
        let after_first = first
            .new_state
            .as_any()
            .downcast_ref::<NativeAllPairsState>()
            .unwrap();
        assert!(after_first.fill.is_some());
        assert!(matches!(
            after_first.get_amount_out(BigUint::from(1_000_000_000u64), &usdc(), &weth()),
            Err(SimulationError::RecoverableError(message)) if message == "Native already quoted in this route"
        ));
        assert!(matches!(
            after_first.spot_price(&usdc(), &weth()),
            Err(SimulationError::RecoverableError(message)) if message == "Native already quoted in this route"
        ));
        assert!(matches!(
            after_first.get_limits(usdc().address, weth().address),
            Err(SimulationError::RecoverableError(message)) if message == "Native already quoted in this route"
        ));
    }

    #[test]
    fn book_quoting_the_direction_beats_the_inverted_one() {
        // The USDC/WETH book pays 1 WETH for 2000 USDC; the WETH/USDC book's asks sell 1 WETH for
        // 2000 USDC too, but its minimum output would reject the swap.
        let mut forward = book();
        forward.minimum_out_base = 2_000_000_000_000_000_000.0;
        let mut reverse = book();
        reverse.base_address = usdc().address;
        reverse.quote_address = weth().address;
        reverse.minimum_in_base = 0.0;
        reverse.bids = vec![NativePriceLevel { quantity: 2_000.0, price: 0.0005 }];
        reverse.asks = vec![];
        let state = state_with(vec![forward, reverse]);
        let result = state
            .get_amount_out(BigUint::from(2_000_000_000u64), &usdc(), &weth())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn skips_book_its_state_rejects() {
        let mut invalid = book();
        invalid.minimum_in_base = f64::NAN;
        let state = state_with(vec![invalid]);
        let price = state.spot_price(&weth(), &usdc());
        assert!(
            matches!(price, Err(SimulationError::InvalidInput(msg, None)) if msg == format!("Invalid token addresses. Got in={}, out={}", weth().address, usdc().address))
        );
    }

    #[test]
    fn returns_partial_result_when_amount_exceeds_depth() {
        let state = state();
        let result =
            state.get_amount_out(BigUint::from(2_000_000_000_000_000_000u64), &weth(), &usdc());
        let Err(SimulationError::InvalidInput(_, Some(partial))) = result else {
            panic!("Expected insufficient-liquidity result, got {result:?}");
        };
        assert_eq!(partial.amount, BigUint::from(2_000_000_000u64));
        let new_state = partial
            .new_state
            .as_any()
            .downcast_ref::<NativeAllPairsState>()
            .unwrap();
        assert!(new_state.fill.is_some());
    }

    #[test]
    fn rejects_invalid_pair() {
        let other = token("0x1111111111111111111111111111111111111111", "OTHER", 18);
        let state = state();
        assert!(matches!(
            state.get_amount_out(BigUint::from(1u64), &other, &usdc()),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert!(matches!(
            state.get_limits(other.address.clone(), usdc().address),
            Err(SimulationError::InvalidInput(_, None))
        ));
        // Direction validation must win even when the book has no liquidity.
        let mut empty = book();
        empty.bids.clear();
        empty.asks.clear();
        let state = state_with(vec![empty]);
        assert!(matches!(
            state.spot_price(&other, &usdc()),
            Err(SimulationError::InvalidInput(message, None))
                if message.contains("Invalid token addresses")
        ));
    }

    #[test]
    fn reports_no_liquidity_for_empty_direction() {
        let mut book = book();
        book.bids.clear();
        let state = state_with(vec![book]);
        assert!(matches!(
            state.get_amount_out(BigUint::from(500_000_000_000_000_000u64), &weth(), &usdc()),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            state.get_limits(weth().address, usdc().address),
            Err(SimulationError::RecoverableError(_))
        ));
    }
}
