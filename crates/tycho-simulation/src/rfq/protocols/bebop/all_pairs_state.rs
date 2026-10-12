use std::{any::Any, collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::debug;
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
        bebop::{client::BebopClient, models::BebopPriceData, state::BebopState},
        fill::{continued_amount_out, continued_limits, filled_on_pair, with_new_state, PairFill},
    },
};

/// Bebop's liquidity on one chain: one per-pair state per book, which prices every swap. A swap
/// on another pair or direction than an earlier fill fails: Bebop already quoted in this route.
#[derive(Clone, Serialize, Deserialize)]
pub struct BebopAllPairsState {
    /// Sorted by base token address, then quote token address.
    pairs: Arc<Vec<BebopState>>,
    /// The pair an earlier swap quoted through Bebop on this state.
    fill: Option<PairFill>,
}

impl fmt::Debug for BebopAllPairsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BebopAllPairsState")
            .field("pairs", &self.pairs.len())
            .field("fill", &self.fill)
            .finish()
    }
}

impl BebopAllPairsState {
    /// Skips books with a token missing from `tokens` and logs each with `debug!`. The stream
    /// decoder already logged the token.
    pub fn new(
        books: Vec<BebopPriceData>,
        tokens: HashMap<Bytes, Token>,
        client: BebopClient,
    ) -> Self {
        let mut pairs = Vec::with_capacity(books.len());
        for book in books {
            let (Some(base_token), Some(quote_token)) =
                (book_token(&tokens, &book.base), book_token(&tokens, &book.quote))
            else {
                debug!(
                    "Skipping Bebop book 0x{}/0x{}: a token is missing from the state.",
                    hex::encode(&book.base),
                    hex::encode(&book.quote)
                );
                continue;
            };
            pairs.push(BebopState::new(base_token, quote_token, book, client.clone()));
        }
        pairs.sort_by(|a, b| {
            (&a.base_token.address, &a.quote_token.address)
                .cmp(&(&b.base_token.address, &b.quote_token.address))
        });
        Self { pairs: Arc::new(pairs), fill: None }
    }

    /// The state of the book with base `base` and quote `quote`, if any.
    fn find(&self, base: &Bytes, quote: &Bytes) -> Option<&BebopState> {
        let index = self.pairs.partition_point(|pair| {
            (&pair.base_token.address, &pair.quote_token.address) < (base, quote)
        });
        self.pairs
            .get(index)
            .filter(|pair| &pair.base_token.address == base && &pair.quote_token.address == quote)
    }

    /// The state that trades `token_in` for `token_out`. A book quoting the pair as given beats
    /// one quoting it the other way round.
    fn pair_state(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<&BebopState, SimulationError> {
        self.find(token_in, token_out)
            .or_else(|| self.find(token_out, token_in))
            .ok_or_else(|| {
                SimulationError::RecoverableError(format!(
                    "Invalid token addresses: {token_in}, {token_out}"
                ))
            })
    }

    /// This state after a swap of `amount_in`. `filled` is what an earlier swap filled on the pair.
    fn with_fill(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
        filled: Option<&BigUint>,
        amount_in: &BigUint,
    ) -> Self {
        Self {
            pairs: self.pairs.clone(),
            fill: Some(PairFill::after(filled, token_in, token_out, amount_in)),
        }
    }
}

/// The book's own tokens in input/output order for a swap from `token_in`.
fn pair_tokens<'a>(pair: &'a BebopState, token_in: &Bytes) -> (&'a Token, &'a Token) {
    if &pair.base_token.address == token_in {
        (&pair.base_token, &pair.quote_token)
    } else {
        (&pair.quote_token, &pair.base_token)
    }
}

/// The token at `address`, if `tokens` carries it.
fn book_token(tokens: &HashMap<Bytes, Token>, address: &[u8]) -> Option<Token> {
    tokens
        .get(&Bytes::from(address.to_vec()))
        .cloned()
}

#[typetag::serde]
impl ProtocolSim for BebopAllPairsState {
    fn fee(&self) -> f64 {
        0.0
    }

    /// After a fill on the pair, the marginal price at the filled amount.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let pair_state = self.pair_state(&base.address, &quote.address)?;
        let Some(filled) = filled_on_pair(&self.fill, &base.address, &quote.address, "Bebop")?
        else {
            return pair_state.spot_price(base, quote);
        };
        let filled = to_whole_units(filled, base.decimals)?;
        marginal_price(&pair_state.swap_levels(&base.address), filled)
            .ok_or_else(|| SimulationError::RecoverableError("No liquidity".into()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let pair_state = self.pair_state(&token_in.address, &token_out.address)?;
        let filled = filled_on_pair(&self.fill, &token_in.address, &token_out.address, "Bebop")?;
        // The per-pair state compares whole tokens, so it gets its own.
        let (pair_in, pair_out) = pair_tokens(pair_state, &token_in.address);
        let amount_out = |amount| pair_state.get_amount_out(amount, pair_in, pair_out);
        let result = match filled {
            Some(filled) => continued_amount_out(amount_out, filled, &amount_in),
            None => amount_out(amount_in.clone()),
        };
        let new_state = self.with_fill(&token_in.address, &token_out.address, filled, &amount_in);
        with_new_state(result, Box::new(new_state))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let pair_state = self.pair_state(&sell_token, &buy_token)?;
        let filled = filled_on_pair(&self.fill, &sell_token, &buy_token, "Bebop")?;
        let limits = pair_state.get_limits(sell_token.clone(), buy_token.clone())?;
        let Some(filled) = filled else {
            return Ok(limits);
        };
        let (pair_in, pair_out) = pair_tokens(pair_state, &sell_token);
        let amount_out = |amount| pair_state.get_amount_out(amount, pair_in, pair_out);
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
            .downcast_ref::<BebopAllPairsState>()
        else {
            return false;
        };
        self.fill == other.fill &&
            self.pairs.len() == other.pairs.len() &&
            self.pairs
                .iter()
                .zip(other.pairs.iter())
                .all(|(a, b)| a.price_data == b.price_data)
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for BebopAllPairsState {
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
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::test_utils::{usdc, wbtc, weth};

    fn empty_bebop_client() -> BebopClient {
        BebopClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            "".to_string(),
            HashSet::new(),
            Duration::from_secs(30),
            None,
            None,
            None,
        )
        .unwrap()
    }

    fn book(base: &Token, quote: &Token, bids: &[f32], asks: &[f32]) -> BebopPriceData {
        BebopPriceData {
            base: base.address.to_vec(),
            quote: quote.address.to_vec(),
            last_update_ts: 1703097600,
            bids: bids.to_vec(),
            asks: asks.to_vec(),
        }
    }

    fn state(books: Vec<BebopPriceData>) -> BebopAllPairsState {
        BebopAllPairsState::new(
            books,
            HashMap::from([
                (wbtc().address, wbtc()),
                (usdc().address, usdc()),
                (weth().address, weth()),
            ]),
            empty_bebop_client(),
        )
    }

    /// WBTC/USDC and WETH/USDC books.
    fn create_test_bebop_state() -> BebopAllPairsState {
        state(vec![
            book(
                &wbtc(),
                &usdc(),
                &[65000.0, 1.5, 64950.0, 2.0, 64900.0, 0.5],
                &[65100.0, 1.0, 65150.0, 2.5, 65200.0, 1.5],
            ),
            book(&weth(), &usdc(), &[3000.0, 2.0, 2900.0, 2.5], &[3100.0, 1.5, 3000.0, 3.0]),
        ])
    }

    #[test]
    fn test_book_quoting_the_direction_beats_the_inverted_one() {
        // The USDC/WETH book pays 1/2048 WETH per USDC; the WETH/USDC book's asks sell 1 WETH for
        // 3100 USDC.
        let state = state(vec![
            book(&weth(), &usdc(), &[3000.0, 2.0], &[3100.0, 1.5]),
            book(&usdc(), &weth(), &[0.00048828125, 2000.0], &[]),
        ]);
        let result = state
            .get_amount_out(BigUint::from(2_000_000_000u64), &usdc(), &weth())
            .unwrap();
        assert_eq!(result.amount, BigUint::from_str("0_976562500000000000").unwrap());
    }

    #[test]
    fn test_get_limits_used_venue() {
        let state = create_test_bebop_state().with_fill(
            &weth().address,
            &usdc().address,
            None,
            &BigUint::from(1u8),
        );
        let result = state.get_limits(wbtc().address, usdc().address);
        assert!(
            matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "Bebop already quoted in this route")
        );
    }

    #[test]
    fn test_get_limits_invalid_token_pair() {
        let state = create_test_bebop_state();
        let result = state.get_limits(wbtc().address, weth().address);
        assert!(
            matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("Invalid token addresses"))
        );
    }

    #[test]
    fn test_get_amount_out() {
        let state = create_test_bebop_state();

        // swap 3 WETH -> USDC: 6000 from level 1 + 2900 from level 2 = 8900 USDC
        let amount_out_result = state
            .get_amount_out(BigUint::from_str("3_000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        assert_eq!(amount_out_result.amount, BigUint::from_str("8900_000_000").unwrap());

        // swap 7000 USDC -> WETH: 1.5 from level 1 + 0.78333 from level 2 = 2.283333 WETH
        let amount_out_result = state
            .get_amount_out(BigUint::from_str("7000_000_000").unwrap(), &usdc(), &weth())
            .unwrap();
        assert_eq!(amount_out_result.amount, BigUint::from_str("2_283333333333333248").unwrap());
    }

    #[test]
    fn test_get_amount_out_once_per_venue() {
        let state = create_test_bebop_state();
        let first = state
            .get_amount_out(BigUint::from_str("1_000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        let after_first = first
            .new_state
            .as_any()
            .downcast_ref::<BebopAllPairsState>()
            .unwrap();
        assert!(after_first.fill.is_some());

        let second = after_first.get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc());
        assert!(
            matches!(second, Err(SimulationError::RecoverableError(msg)) if msg == "Bebop already quoted in this route")
        );
        let spot_price = after_first.spot_price(&wbtc(), &usdc());
        assert!(
            matches!(spot_price, Err(SimulationError::RecoverableError(msg)) if msg == "Bebop already quoted in this route")
        );
    }
}
