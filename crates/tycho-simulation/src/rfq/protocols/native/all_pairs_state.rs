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

use crate::rfq::protocols::native::{
    client::NativeClient, models::NativePriceData, state::NativeState,
};

/// Native Relay's liquidity on one chain: one per-pair state per book.
///
/// Native names no market maker, so a swap marks the whole venue used and a later swap on that
/// state finds no liquidity.
#[derive(Clone, Serialize, Deserialize)]
pub struct NativeAllPairsState {
    states: Arc<Vec<NativeState>>,
    /// Every direction a book quotes, sorted, with the index of its state in `states`. A book
    /// quoting the pair as given beats one quoting it the other way round.
    directions: Arc<Vec<((Bytes, Bytes), usize)>>,
    /// Whether a swap on this state already took Native's quote.
    used: bool,
}

impl fmt::Debug for NativeAllPairsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeAllPairsState")
            .field("books", &self.states.len())
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

impl NativeAllPairsState {
    /// Fails when a book names a token `tokens` does not carry, or when its per-pair state
    /// rejects it.
    pub fn new(
        books: Vec<NativePriceData>,
        tokens: HashMap<Bytes, Token>,
        client: NativeClient,
    ) -> Result<Self, SimulationError> {
        let mut states = Vec::with_capacity(books.len());
        for book in books {
            // The client filters books to its tokens, so an unknown token means corrupt data.
            let (Some(base_token), Some(quote_token)) =
                (tokens.get(&book.base_address), tokens.get(&book.quote_address))
            else {
                return Err(SimulationError::FatalError(
                    "Native book token addresses do not match state tokens".to_string(),
                ));
            };
            states.push(NativeState::new(
                base_token.clone(),
                quote_token.clone(),
                book,
                client.clone(),
            )?);
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
        Ok(Self { states: Arc::new(states), directions: Arc::new(directions), used: false })
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

    /// The per-pair state that trades `token_in` for `token_out`, on a state no swap used.
    fn quotable_pair_state(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<&NativeState, SimulationError> {
        let state = self.pair_state(token_in, token_out)?;
        if self.used {
            return Err(SimulationError::RecoverableError(
                "Native already quoted in this route".to_string(),
            ));
        }
        Ok(state)
    }

    fn used_state(&self) -> Box<dyn ProtocolSim> {
        Box::new(Self {
            states: self.states.clone(),
            directions: self.directions.clone(),
            used: true,
        })
    }
}

#[typetag::serde]
impl ProtocolSim for NativeAllPairsState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.quotable_pair_state(&base.address, &quote.address)?
            .spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let state = self.quotable_pair_state(&token_in.address, &token_out.address)?;
        match state.get_amount_out(amount_in, token_in, token_out) {
            Ok(mut res) => {
                res.new_state = self.used_state();
                Ok(res)
            }
            Err(SimulationError::InvalidInput(message, Some(mut res))) => {
                res.new_state = self.used_state();
                Err(SimulationError::InvalidInput(message, Some(res)))
            }
            Err(e) => Err(e),
        }
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.quotable_pair_state(&sell_token, &buy_token)?
            .get_limits(sell_token, buy_token)
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
        self.used == other.used &&
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

    fn state_with(books: Vec<NativePriceData>) -> Result<NativeAllPairsState, SimulationError> {
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
        state_with(vec![book()]).unwrap()
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
        assert!(after_first.used);
        assert!(matches!(
            after_first.get_amount_out(BigUint::from(1_000_000_000u64), &usdc(), &weth()),
            Err(SimulationError::RecoverableError(message)) if message == "Native already quoted in this route"
        ));
        assert!(matches!(
            after_first.spot_price(&weth(), &usdc()),
            Err(SimulationError::RecoverableError(message)) if message == "Native already quoted in this route"
        ));
        assert!(matches!(
            after_first.get_limits(weth().address, usdc().address),
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
        let state = state_with(vec![forward, reverse]).unwrap();
        let result = state
            .get_amount_out(BigUint::from(2_000_000_000u64), &usdc(), &weth())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(1_000_000_000_000_000_000u64));
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
        assert!(new_state.used);
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
        let state = state_with(vec![empty]).unwrap();
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
        let state = state_with(vec![book]).unwrap();
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
