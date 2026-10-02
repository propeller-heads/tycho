use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};
use tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::rfq::models::{fill_levels, PriceLevel, QuoteRule};

/// One market maker's levels on one directed pair: the levels sell `base_token` for
/// `quote_token`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MakerBook {
    #[serde(rename = "mm")]
    pub market_maker: String,
    pub base_token: Bytes,
    pub quote_token: Bytes,
    pub levels: Vec<PriceLevel>,
}

/// A venue's market makers' books on one chain, and the makers a route has taken quotes from.
///
/// The books and tokens are shared between the states a route threads, so a swap copies only
/// the used set.
#[derive(Clone, Serialize, Deserialize)]
pub struct MakerBooks {
    /// Sorted by base token, quote token and market maker.
    pub books: Arc<Vec<MakerBook>>,
    pub tokens: Arc<HashMap<Bytes, Token>>,
    pub used_market_makers: HashSet<String>,
    /// Whether a maker declines an amount below its first level's quantity.
    first_level_is_minimum: bool,
}

impl fmt::Debug for MakerBooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MakerBooks")
            .field("books", &self.books.len())
            .field("tokens", &self.tokens.len())
            .field("used_market_makers", &self.used_market_makers)
            .finish_non_exhaustive()
    }
}

/// What one market maker pays for an amount.
pub struct Fill<'a> {
    pub book: &'a MakerBook,
    /// In whole units.
    pub amount_in: f64,
    /// In atomic units.
    pub amount_out: BigUint,
    /// The input the levels could not fill, in whole units.
    pub remaining_amount_in: f64,
}

impl Fill<'_> {
    /// The result, or the partial-fill error carrying it when the maker filled only part.
    pub fn result(
        self,
        gas: u64,
        new_state: Box<dyn ProtocolSim>,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let res =
            GetAmountOutResult { amount: self.amount_out, gas: BigUint::from(gas), new_state };
        if self.remaining_amount_in > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Pool has not enough liquidity to support complete swap. Input amount: {}, consumed amount: {}",
                    self.amount_in,
                    self.amount_in - self.remaining_amount_in
                ),
                Some(res),
            ));
        }
        Ok(res)
    }

    fn beats(&self, other: &Fill<'_>) -> bool {
        (self.remaining_amount_in == 0.0, &self.amount_out) >
            (other.remaining_amount_in == 0.0, &other.amount_out)
    }
}

impl MakerBooks {
    /// Fails when a book names a token `tokens` does not carry.
    pub fn new(
        mut books: Vec<MakerBook>,
        tokens: HashMap<Bytes, Token>,
        first_level_is_minimum: bool,
    ) -> Result<Self, SimulationError> {
        for book in &books {
            for address in [&book.base_token, &book.quote_token] {
                if !tokens.contains_key(address) {
                    return Err(SimulationError::FatalError(format!(
                        "Book of {} names token {address}, which the state does not carry",
                        book.market_maker
                    )));
                }
            }
        }
        books.sort_by(|a, b| {
            (&a.base_token, &a.quote_token, &a.market_maker).cmp(&(
                &b.base_token,
                &b.quote_token,
                &b.market_maker,
            ))
        });
        Ok(Self {
            books: Arc::new(books),
            tokens: Arc::new(tokens),
            used_market_makers: HashSet::new(),
            first_level_is_minimum,
        })
    }

    pub fn token(&self, address: &Bytes) -> Result<&Token, SimulationError> {
        self.tokens.get(address).ok_or_else(|| {
            SimulationError::InvalidInput(format!("No market maker quotes token {address}"), None)
        })
    }

    /// Every book on the directed pair.
    pub fn pair_books(&self, token_in: &Bytes, token_out: &Bytes) -> &[MakerBook] {
        let start = self
            .books
            .partition_point(|book| (&book.base_token, &book.quote_token) < (token_in, token_out));
        let end = self
            .books
            .partition_point(|book| (&book.base_token, &book.quote_token) <= (token_in, token_out));
        &self.books[start..end]
    }

    /// The pair's books that still quote: those with levels whose maker the quote rule allows.
    ///
    /// A pair no book names is an invalid input. A pair with no book left has no liquidity.
    fn quotable_books(
        &self,
        rule: QuoteRule,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Vec<&MakerBook>, SimulationError> {
        let pair_books = self.pair_books(token_in, token_out);
        if pair_books.is_empty() {
            return Err(SimulationError::InvalidInput(
                format!("No market maker quotes {token_in} -> {token_out}"),
                None,
            ));
        }
        let mut books = Vec::new();
        for book in pair_books {
            if !book.levels.is_empty() && rule.allows(&self.used_market_makers, &book.market_maker)
            {
                books.push(book);
            }
        }
        if books.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(books)
    }

    /// The best first-level price any quotable maker offers.
    pub fn spot_price(
        &self,
        rule: QuoteRule,
        base: &Bytes,
        quote: &Bytes,
    ) -> Result<f64, SimulationError> {
        let mut best = 0.0_f64;
        for book in self.quotable_books(rule, base, quote)? {
            best = best.max(book.levels[0].price);
        }
        Ok(best)
    }

    /// The fill from the market maker that pays most for `amount_in`. A maker that fills the
    /// whole amount beats one that fills part of it, whatever the two pay.
    pub fn best_fill(
        &self,
        rule: QuoteRule,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Fill<'_>, SimulationError> {
        let token_in = self.token(token_in)?;
        let token_out = self.token(token_out)?;
        let amount_in = to_whole_units(amount_in, token_in.decimals)?;

        let mut best: Option<Fill<'_>> = None;
        let mut smallest_minimum = f64::MAX;
        for book in self.quotable_books(rule, &token_in.address, &token_out.address)? {
            let minimum = book.levels[0].quantity;
            if self.first_level_is_minimum && amount_in < minimum {
                smallest_minimum = smallest_minimum.min(minimum);
                continue;
            }
            let (amount_out, remaining_amount_in) = fill_levels(&book.levels, amount_in);
            let fill = Fill {
                book,
                amount_in,
                amount_out: to_atomic_units(amount_out, token_out.decimals)?,
                remaining_amount_in,
            };
            if best
                .as_ref()
                .is_none_or(|current| fill.beats(current))
            {
                best = Some(fill);
            }
        }
        best.ok_or_else(|| {
            SimulationError::RecoverableError(format!(
                "Amount below minimum. Input amount: {amount_in}, min amount: {smallest_minimum}"
            ))
        })
    }

    /// The limits of the quotable market maker that pays most in total: a swap fills from one
    /// maker.
    pub fn get_limits(
        &self,
        rule: QuoteRule,
        sell_token: &Bytes,
        buy_token: &Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell_decimals = self.token(sell_token)?.decimals;
        let buy_decimals = self.token(buy_token)?.decimals;
        let mut best = (0.0, 0.0);
        for book in self.quotable_books(rule, sell_token, buy_token)? {
            let mut sell_total = 0.0;
            let mut buy_total = 0.0;
            for level in &book.levels {
                sell_total += level.quantity;
                buy_total += level.quantity * level.price;
            }
            if buy_total > best.1 {
                best = (sell_total, buy_total);
            }
        }
        Ok((to_atomic_units(best.0, sell_decimals)?, to_atomic_units(best.1, buy_decimals)?))
    }

    /// These books after a swap took `market_maker`'s quote.
    pub fn with_used(&self, market_maker: &str) -> Self {
        let mut next = self.clone();
        next.used_market_makers
            .insert(market_maker.to_string());
        next
    }
}

pub fn to_whole_units(amount: &BigUint, decimals: u32) -> Result<f64, SimulationError> {
    let amount = amount
        .to_f64()
        .ok_or_else(|| SimulationError::RecoverableError("Can't convert amount to f64".into()))?;
    Ok(amount / 10f64.powi(decimals as i32))
}

pub fn to_atomic_units(amount: f64, decimals: u32) -> Result<BigUint, SimulationError> {
    BigUint::from_f64(amount * 10f64.powi(decimals as i32))
        .ok_or_else(|| SimulationError::RecoverableError("Can't convert amount to BigUint".into()))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::rfq::protocols::test_utils::{usdc, wbtc, weth};

    fn book(market_maker: &str, base: &Token, quote: &Token, levels: &[(f64, f64)]) -> MakerBook {
        MakerBook {
            market_maker: market_maker.to_string(),
            base_token: base.address.clone(),
            quote_token: quote.address.clone(),
            levels: levels
                .iter()
                .map(|&(quantity, price)| PriceLevel { quantity, price })
                .collect(),
        }
    }

    /// Two makers on WETH/USDC and one on WBTC/USDC. `test_mm_2` pays more for a small amount
    /// and runs out at 2 WETH; `test_mm` holds 7 WETH.
    fn books(first_level_is_minimum: bool) -> MakerBooks {
        MakerBooks::new(
            vec![
                book("test_mm", &wbtc(), &usdc(), &[(1.0, 65000.0)]),
                book("test_mm_2", &weth(), &usdc(), &[(0.5, 3010.0), (1.5, 2990.0)]),
                book("test_mm", &weth(), &usdc(), &[(0.5, 3000.0), (1.5, 3000.0), (5.0, 2999.0)]),
            ],
            HashMap::from([
                (weth().address, weth()),
                (usdc().address, usdc()),
                (wbtc().address, wbtc()),
            ]),
            first_level_is_minimum,
        )
        .unwrap()
    }

    fn weth_amount(whole: f64) -> BigUint {
        BigUint::from((whole * 1e18) as u128)
    }

    fn usdc_amount(whole: f64) -> BigUint {
        BigUint::from((whole * 1e6) as u128)
    }

    fn best_fill(
        books: &MakerBooks,
        rule: QuoteRule,
        amount_in: BigUint,
    ) -> Result<Fill<'_>, SimulationError> {
        books.best_fill(rule, &amount_in, &weth().address, &usdc().address)
    }

    #[test]
    fn new_rejects_book_naming_unknown_token() {
        let result = MakerBooks::new(
            vec![book("mm", &weth(), &usdc(), &[(1.0, 3000.0)])],
            HashMap::from([(weth().address, weth())]),
            false,
        );
        assert!(
            matches!(result, Err(SimulationError::FatalError(msg)) if msg.contains("does not carry"))
        );
    }

    #[test]
    fn pair_books_sorted_by_maker() {
        let books = books(false);
        let makers: Vec<_> = books
            .pair_books(&weth().address, &usdc().address)
            .iter()
            .map(|book| book.market_maker.as_str())
            .collect();
        assert_eq!(makers, ["test_mm", "test_mm_2"]);
        assert!(books
            .pair_books(&usdc().address, &weth().address)
            .is_empty());
    }

    mod spot_price {
        use super::*;

        #[test]
        fn best_first_level_across_makers() {
            let books = books(false);
            let price = books
                .spot_price(QuoteRule::OncePerMaker, &weth().address, &usdc().address)
                .unwrap();
            assert_eq!(price, 3010.0);
        }

        #[test]
        fn used_maker_is_skipped() {
            let books = books(false).with_used("test_mm_2");
            let price = books
                .spot_price(QuoteRule::OncePerMaker, &weth().address, &usdc().address)
                .unwrap();
            assert_eq!(price, 3000.0);
        }

        #[test]
        fn unquoted_pair() {
            let result =
                books(false).spot_price(QuoteRule::OncePerMaker, &usdc().address, &weth().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("No market maker quotes"))
            );
        }

        #[test]
        fn every_maker_used() {
            let books = books(false)
                .with_used("test_mm")
                .with_used("test_mm_2");
            let result =
                books.spot_price(QuoteRule::OncePerMaker, &weth().address, &usdc().address);
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }

        #[test]
        fn empty_levels() {
            let mut books = books(false);
            let mut emptied = (*books.books).clone();
            for book in &mut emptied {
                book.levels.clear();
            }
            books.books = Arc::new(emptied);
            let result =
                books.spot_price(QuoteRule::OncePerMaker, &weth().address, &usdc().address);
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }
    }

    mod best_fill {
        use super::*;

        #[test]
        fn best_paying_maker() {
            let books = books(false);
            // test_mm_2: 0.5 * 3010 = 1505. test_mm: 0.5 * 3000 = 1500.
            let fill = best_fill(&books, QuoteRule::OncePerMaker, weth_amount(0.5)).unwrap();
            assert_eq!(fill.book.market_maker, "test_mm_2");
            assert_eq!(fill.amount_out, usdc_amount(1505.0));
            assert_eq!(fill.remaining_amount_in, 0.0);
        }

        #[test]
        fn used_maker_is_skipped() {
            let books = books(false).with_used("test_mm_2");
            let fill = best_fill(&books, QuoteRule::OncePerMaker, weth_amount(0.5)).unwrap();
            assert_eq!(fill.book.market_maker, "test_mm");
            assert_eq!(fill.amount_out, usdc_amount(1500.0));
        }

        #[test]
        fn used_maker_on_another_pair() {
            let books = books(false).with_used("test_mm");
            let result = books.best_fill(
                QuoteRule::OncePerMaker,
                &BigUint::from(100_000_000u64),
                &wbtc().address,
                &usdc().address,
            );
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }

        #[test]
        fn once_per_venue_after_a_swap() {
            let books = books(false).with_used("test_mm_2");
            let result = best_fill(&books, QuoteRule::OncePerVenue, weth_amount(0.5));
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }

        #[test]
        fn full_fill_beats_partial_fill() {
            let books = books(false);
            // test_mm_2 fills 2 of 3 WETH at better prices; test_mm fills all 3:
            // 0.5 * 3000 + 1.5 * 3000 + 1.0 * 2999 = 8999.
            let fill = best_fill(&books, QuoteRule::OncePerMaker, weth_amount(3.0)).unwrap();
            assert_eq!(fill.book.market_maker, "test_mm");
            assert_eq!(fill.amount_out, usdc_amount(8999.0));
        }

        #[test]
        fn every_maker_partial() {
            let books = books(false);
            // test_mm: 7 WETH for 20995. test_mm_2: 2 WETH for 5990.
            let fill = best_fill(&books, QuoteRule::OncePerMaker, weth_amount(8.0)).unwrap();
            assert_eq!(fill.book.market_maker, "test_mm");
            assert_eq!(fill.amount_out, usdc_amount(20995.0));
            assert_eq!(fill.remaining_amount_in, 1.0);
        }

        #[test]
        fn tie_keeps_the_first_maker() {
            let books = MakerBooks::new(
                vec![
                    book("mm_b", &weth(), &usdc(), &[(1.0, 3000.0)]),
                    book("mm_a", &weth(), &usdc(), &[(1.0, 3000.0)]),
                ],
                HashMap::from([(weth().address, weth()), (usdc().address, usdc())]),
                false,
            )
            .unwrap();
            let fill = best_fill(&books, QuoteRule::OncePerMaker, weth_amount(1.0)).unwrap();
            assert_eq!(fill.book.market_maker, "mm_a");
        }

        #[rstest]
        #[case::first_level_is_minimum(true)]
        #[case::no_minimum(false)]
        fn below_every_first_level(#[case] first_level_is_minimum: bool) {
            let books = books(first_level_is_minimum);
            let result = best_fill(&books, QuoteRule::OncePerMaker, weth_amount(0.25));
            if first_level_is_minimum {
                assert!(
                    matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("Amount below minimum"))
                );
            } else {
                assert_eq!(result.unwrap().amount_out, usdc_amount(752.5));
            }
        }

        #[test]
        fn unquoted_pair() {
            let books = books(false);
            let result = books.best_fill(
                QuoteRule::OncePerMaker,
                &usdc_amount(10_000.0),
                &usdc().address,
                &weth().address,
            );
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("No market maker quotes"))
            );
        }
    }

    mod get_limits {
        use super::*;

        #[test]
        fn largest_maker() {
            let books = books(false);
            let (sell_limit, buy_limit) = books
                .get_limits(QuoteRule::OncePerMaker, &weth().address, &usdc().address)
                .unwrap();
            // test_mm: 7 WETH for 20995 USDC. test_mm_2: 2 WETH for 5990 USDC.
            assert_eq!(sell_limit, weth_amount(7.0));
            assert_eq!(buy_limit, usdc_amount(20995.0));
        }

        #[test]
        fn used_maker_is_skipped() {
            let books = books(false).with_used("test_mm");
            let (sell_limit, buy_limit) = books
                .get_limits(QuoteRule::OncePerMaker, &weth().address, &usdc().address)
                .unwrap();
            assert_eq!(sell_limit, weth_amount(2.0));
            assert_eq!(buy_limit, usdc_amount(5990.0));
        }

        #[test]
        fn unquoted_pair() {
            let result =
                books(false).get_limits(QuoteRule::OncePerMaker, &wbtc().address, &weth().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("No market maker quotes"))
            );
        }
    }
}
