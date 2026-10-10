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

use crate::rfq::protocols::bebop::{
    client::BebopClient, models::BebopPriceData, state::BebopState,
};

/// Bebop's liquidity on one chain: one per-pair state per book, which prices every swap.
///
/// Bebop names no market maker and picks the makers behind a firm quote itself, so a swap marks
/// the whole venue used and a later swap on that state finds no liquidity.
#[derive(Clone, Serialize, Deserialize)]
pub struct BebopAllPairsState {
    /// Sorted by base token address, then quote token address.
    pairs: Arc<Vec<BebopState>>,
    /// Whether a swap on this state already took Bebop's quote.
    used: bool,
}

impl fmt::Debug for BebopAllPairsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BebopAllPairsState")
            .field("pairs", &self.pairs.len())
            .field("used", &self.used)
            .finish()
    }
}

impl BebopAllPairsState {
    /// Fails when a book names a token `tokens` does not carry.
    pub fn new(
        books: Vec<BebopPriceData>,
        tokens: HashMap<Bytes, Token>,
        client: BebopClient,
    ) -> Result<Self, SimulationError> {
        let mut pairs = Vec::with_capacity(books.len());
        for book in books {
            let base_token = book_token(&tokens, &book.base)?;
            let quote_token = book_token(&tokens, &book.quote)?;
            pairs.push(BebopState::new(base_token, quote_token, book, client.clone()));
        }
        pairs.sort_by(|a, b| {
            (&a.base_token.address, &a.quote_token.address)
                .cmp(&(&b.base_token.address, &b.quote_token.address))
        });
        Ok(Self { pairs: Arc::new(pairs), used: false })
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

    fn check_unused(&self) -> Result<(), SimulationError> {
        if self.used {
            return Err(SimulationError::RecoverableError(
                "Bebop already quoted in this route".into(),
            ));
        }
        Ok(())
    }

    fn used_state(&self) -> Self {
        Self { pairs: self.pairs.clone(), used: true }
    }
}

/// The token at `address`. Fails when `tokens` does not carry it: the client filters books to its
/// tokens, so an unknown token means corrupt data.
fn book_token(tokens: &HashMap<Bytes, Token>, address: &[u8]) -> Result<Token, SimulationError> {
    tokens
        .get(&Bytes::from(address.to_vec()))
        .cloned()
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "Bebop book names token 0x{}, which the state does not carry",
                hex::encode(address)
            ))
        })
}

#[typetag::serde]
impl ProtocolSim for BebopAllPairsState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let pair_state = self.pair_state(&base.address, &quote.address)?;
        self.check_unused()?;
        pair_state.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let pair_state = self.pair_state(&token_in.address, &token_out.address)?;
        self.check_unused()?;
        // The per-pair state compares whole tokens, so it gets its own.
        let (token_in, token_out) = if pair_state.base_token.address == token_in.address {
            (&pair_state.base_token, &pair_state.quote_token)
        } else {
            (&pair_state.quote_token, &pair_state.base_token)
        };
        match pair_state.get_amount_out(amount_in, token_in, token_out) {
            Ok(mut res) => {
                res.new_state = Box::new(self.used_state());
                Ok(res)
            }
            Err(SimulationError::InvalidInput(msg, Some(mut res))) => {
                res.new_state = Box::new(self.used_state());
                Err(SimulationError::InvalidInput(msg, Some(res)))
            }
            Err(e) => Err(e),
        }
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let pair_state = self.pair_state(&sell_token, &buy_token)?;
        self.check_unused()?;
        pair_state.get_limits(sell_token, buy_token)
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
        self.used == other.used &&
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
        .unwrap()
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
        let state = create_test_bebop_state().used_state();
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
        assert!(after_first.used);

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
