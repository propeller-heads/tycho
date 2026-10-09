use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
    sync::Arc,
};

use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};
use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::rfq::{
    errors::RFQError,
    models::{fill_levels, PriceLevel, QuoteRule},
    protocols::component,
};

/// One market maker's levels on one directed pair: the levels take `base_token` in and pay
/// `quote_token` out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MakerPriceLevels {
    #[serde(rename = "mm")]
    pub market_maker: String,
    pub base_token: Bytes,
    pub quote_token: Bytes,
    pub levels: Vec<PriceLevel>,
}

impl MakerPriceLevels {
    /// The order `MakerPriceLevels` are kept in: by base token, quote token, then market maker.
    pub fn sort_key(&self) -> (&Bytes, &Bytes, &str) {
        (&self.base_token, &self.quote_token, &self.market_maker)
    }

    /// The volume-weighted average price of these levels. `None` when there are no levels.
    fn average_price(&self) -> Option<f64> {
        let mut quantity = 0.0;
        let mut value = 0.0;
        for level in &self.levels {
            quantity += level.quantity;
            value += level.quantity * level.price;
        }
        (quantity > 0.0).then(|| value / quantity)
    }
}

/// One maker's levels on one pair, with the TVL that cleared the client's threshold.
pub struct MakerLevelsWithTvl<'a, L> {
    pub market_maker: &'a str,
    pub levels: &'a L,
    pub tvl: f64,
}

/// How a venue's makers price: the smallest amount a maker accepts, and the price a maker's
/// levels quote as their spot price.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MakerPricing {
    /// A maker declines an amount below its first level's quantity. A maker's spot price is its
    /// first level's price.
    Hashflow,
    /// A maker fills any amount up to its depth, as in the per-pair Liquorice state. A maker's
    /// spot price is its volume-weighted average price, as in the per-pair Liquorice state.
    Liquorice,
}

/// A venue's market makers' price levels on one chain, and the makers a route has taken quotes
/// from.
///
/// The price levels and tokens are shared between the states a route threads, so a swap copies only
/// the used set.
#[derive(Clone, Serialize, Deserialize)]
pub struct AllMakerPriceLevels {
    /// Sorted by [`MakerPriceLevels::sort_key`].
    price_levels: Arc<Vec<MakerPriceLevels>>,
    tokens: Arc<HashMap<Bytes, Token>>,
    used_market_makers: HashSet<String>,
    pricing: MakerPricing,
    rule: QuoteRule,
}

impl fmt::Debug for AllMakerPriceLevels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllMakerPriceLevels")
            .field("price_levels", &self.price_levels.len())
            .field("tokens", &self.tokens.len())
            .field("used_market_makers", &self.used_market_makers)
            .field("pricing", &self.pricing)
            .field("rule", &self.rule)
            .finish()
    }
}

impl PartialEq for AllMakerPriceLevels {
    fn eq(&self, other: &Self) -> bool {
        self.price_levels == other.price_levels &&
            self.used_market_makers == other.used_market_makers &&
            self.pricing == other.pricing &&
            self.rule == other.rule
    }
}

/// What one market maker pays for an amount.
pub struct Fill<'a> {
    pub maker_levels: &'a MakerPriceLevels,
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

impl AllMakerPriceLevels {
    /// Fails when a maker's price levels name a token `tokens` does not carry.
    pub fn new(
        mut price_levels: Vec<MakerPriceLevels>,
        tokens: HashMap<Bytes, Token>,
        pricing: MakerPricing,
        rule: QuoteRule,
    ) -> Result<Self, SimulationError> {
        for maker_levels in &price_levels {
            for address in [&maker_levels.base_token, &maker_levels.quote_token] {
                // The client filters levels to its tokens; an unknown token means corrupt data.
                if !tokens.contains_key(address) {
                    return Err(SimulationError::FatalError(format!(
                        "Price levels of {} name token {address}, which the state does not carry",
                        maker_levels.market_maker
                    )));
                }
            }
        }
        price_levels.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        Ok(Self {
            price_levels: Arc::new(price_levels),
            tokens: Arc::new(tokens),
            used_market_makers: HashSet::new(),
            pricing,
            rule,
        })
    }

    pub fn token(&self, address: &Bytes) -> Result<&Token, SimulationError> {
        self.tokens.get(address).ok_or_else(|| {
            SimulationError::InvalidInput(format!("No market maker quotes token {address}"), None)
        })
    }

    /// Every maker's price levels on the directed pair.
    pub fn pair_price_levels(&self, token_in: &Bytes, token_out: &Bytes) -> &[MakerPriceLevels] {
        let start = self
            .price_levels
            .partition_point(|maker_levels| {
                (&maker_levels.base_token, &maker_levels.quote_token) < (token_in, token_out)
            });
        let end = self
            .price_levels
            .partition_point(|maker_levels| {
                (&maker_levels.base_token, &maker_levels.quote_token) <= (token_in, token_out)
            });
        &self.price_levels[start..end]
    }

    /// The smallest amount any maker on the pair accepts, in atomic units, rounded up. `None` when
    /// the pricing sets no minimum or no maker on the pair has levels.
    pub fn minimum_amount_in(&self, token_in: &Bytes, token_out: &Bytes) -> Option<BigUint> {
        if self.pricing != MakerPricing::Hashflow {
            return None;
        }
        let decimals = self.tokens.get(token_in)?.decimals;
        let mut minimum: Option<f64> = None;
        for maker_levels in self.pair_price_levels(token_in, token_out) {
            let Some(first_level) = maker_levels.levels.first() else { continue };
            minimum = Some(minimum.map_or(first_level.quantity, |m| m.min(first_level.quantity)));
        }
        BigUint::from_f64((minimum? * 10f64.powi(decimals as i32)).ceil())
    }

    /// The pair's price levels that still quote: those with levels whose maker the quote rule
    /// allows.
    ///
    /// A pair no maker quotes is an invalid input. A pair whose makers with levels the quote rule
    /// all refuse already quoted in this route. A pair with no levels has no liquidity.
    fn quotable_price_levels(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Vec<&MakerPriceLevels>, SimulationError> {
        let pair_price_levels = self.pair_price_levels(token_in, token_out);
        if pair_price_levels.is_empty() {
            return Err(SimulationError::InvalidInput(
                format!("No market maker quotes {token_in} -> {token_out}"),
                None,
            ));
        }
        let mut price_levels = Vec::new();
        let mut refused_by_rule = false;
        for maker_levels in pair_price_levels {
            if maker_levels.levels.is_empty() {
                continue;
            }
            if self
                .rule
                .allows(&self.used_market_makers, &maker_levels.market_maker)
            {
                price_levels.push(maker_levels);
            } else {
                refused_by_rule = true;
            }
        }
        if price_levels.is_empty() && refused_by_rule {
            return Err(SimulationError::RecoverableError(format!(
                "Every market maker on {token_in} -> {token_out} already quoted in this route"
            )));
        }
        if price_levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(price_levels)
    }

    /// The best spot price any quotable maker offers, read as the pricing reads a maker's price.
    pub fn spot_price(&self, base: &Bytes, quote: &Bytes) -> Result<f64, SimulationError> {
        let mut best = 0.0_f64;
        for maker_levels in self.quotable_price_levels(base, quote)? {
            let price = match self.pricing {
                MakerPricing::Hashflow => Some(maker_levels.levels[0].price),
                MakerPricing::Liquorice => maker_levels.average_price(),
            };
            best = best.max(price.unwrap_or_default());
        }
        Ok(best)
    }

    /// The fill from the market maker that pays most for `amount_in`. A maker that fills the
    /// whole amount beats one that fills part of it, whatever the two pay.
    pub fn best_fill(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Fill<'_>, SimulationError> {
        let token_in = self.token(token_in)?;
        let token_out = self.token(token_out)?;
        let amount_in = to_whole_units(amount_in, token_in.decimals)?;
        let first_level_is_minimum = self.pricing == MakerPricing::Hashflow;

        let mut best: Option<Fill<'_>> = None;
        let mut smallest_minimum = f64::MAX;
        for maker_levels in self.quotable_price_levels(&token_in.address, &token_out.address)? {
            let minimum = maker_levels.levels[0].quantity;
            if first_level_is_minimum && amount_in < minimum {
                smallest_minimum = smallest_minimum.min(minimum);
                continue;
            }
            let (amount_out, remaining_amount_in) = fill_levels(&maker_levels.levels, amount_in);
            let fill = Fill {
                maker_levels,
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
        sell_token: &Bytes,
        buy_token: &Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell_decimals = self.token(sell_token)?.decimals;
        let buy_decimals = self.token(buy_token)?.decimals;
        let mut best = (0.0, 0.0);
        for maker_levels in self.quotable_price_levels(sell_token, buy_token)? {
            let mut sell_total = 0.0;
            let mut buy_total = 0.0;
            for level in &maker_levels.levels {
                sell_total += level.quantity;
                buy_total += level.quantity * level.price;
            }
            if buy_total > best.1 {
                best = (sell_total, buy_total);
            }
        }
        Ok((to_atomic_units(best.0, sell_decimals)?, to_atomic_units(best.1, buy_decimals)?))
    }

    /// These price levels after a swap took `market_maker`'s quote.
    pub fn with_used(&self, market_maker: &str) -> Self {
        let mut next = self.clone();
        next.used_market_makers
            .insert(market_maker.to_string());
        next
    }
}

/// The all-pairs component for one poll of a venue that names its makers: every maker's price
/// levels in `price_levels` with its TVL. `None` when `price_levels` is empty.
pub fn all_pairs_component(
    protocol_system: &str,
    protocol_type_name: &str,
    chain: Chain,
    mut price_levels: Vec<(MakerPriceLevels, f64)>,
    rule: QuoteRule,
) -> Result<Option<ComponentWithState>, RFQError> {
    if price_levels.is_empty() {
        return Ok(None);
    }
    price_levels.sort_by(|(a, _), (b, _)| a.sort_key().cmp(&b.sort_key()));
    let mut swap_directions = BTreeSet::new();
    let mut tvl = 0.0;
    for (maker_levels, levels_tvl) in &price_levels {
        swap_directions.insert((maker_levels.base_token.clone(), maker_levels.quote_token.clone()));
        tvl += levels_tvl;
    }
    let price_levels: Vec<MakerPriceLevels> = price_levels
        .into_iter()
        .map(|(maker_levels, _)| maker_levels)
        .collect();
    let component = component::all_pairs_component(
        protocol_system,
        protocol_type_name,
        chain,
        &swap_directions,
        &price_levels,
        tvl,
        rule,
    )?;
    Ok(Some(component))
}

fn to_whole_units(amount: &BigUint, decimals: u32) -> Result<f64, SimulationError> {
    let amount = amount
        .to_f64()
        .ok_or_else(|| SimulationError::RecoverableError("Can't convert amount to f64".into()))?;
    Ok(amount / 10f64.powi(decimals as i32))
}

fn to_atomic_units(amount: f64, decimals: u32) -> Result<BigUint, SimulationError> {
    BigUint::from_f64(amount * 10f64.powi(decimals as i32))
        .ok_or_else(|| SimulationError::RecoverableError("Can't convert amount to BigUint".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfq::protocols::test_utils::{
        maker_price_levels, usdc, usdc_amount, wbtc, weth, weth_amount,
    };

    fn tokens() -> HashMap<Bytes, Token> {
        HashMap::from([
            (weth().address, weth()),
            (usdc().address, usdc()),
            (wbtc().address, wbtc()),
        ])
    }

    /// Two makers on WETH/USDC and one on WBTC/USDC. `test_mm_2` pays more for a small amount
    /// and runs out at 2 WETH; `test_mm` holds 7 WETH.
    fn price_levels(pricing: MakerPricing, rule: QuoteRule) -> AllMakerPriceLevels {
        AllMakerPriceLevels::new(
            vec![
                maker_price_levels("test_mm", &wbtc(), &usdc(), &[(1.0, 65000.0)]),
                maker_price_levels("test_mm_2", &weth(), &usdc(), &[(0.5, 3010.0), (1.5, 2990.0)]),
                maker_price_levels(
                    "test_mm",
                    &weth(),
                    &usdc(),
                    &[(0.5, 3000.0), (1.5, 3000.0), (5.0, 2999.0)],
                ),
            ],
            tokens(),
            pricing,
            rule,
        )
        .unwrap()
    }

    fn per_maker() -> AllMakerPriceLevels {
        price_levels(MakerPricing::Liquorice, QuoteRule::OncePerMaker)
    }

    fn weth_usdc_fill(
        price_levels: &AllMakerPriceLevels,
        amount_in: BigUint,
    ) -> Result<Fill<'_>, SimulationError> {
        price_levels.best_fill(&amount_in, &weth().address, &usdc().address)
    }

    #[test]
    fn new_rejects_book_naming_unknown_token() {
        let result = AllMakerPriceLevels::new(
            vec![maker_price_levels("mm", &weth(), &usdc(), &[(1.0, 3000.0)])],
            HashMap::from([(weth().address, weth())]),
            MakerPricing::Liquorice,
            QuoteRule::OncePerMaker,
        );
        assert!(
            matches!(result, Err(SimulationError::FatalError(msg)) if msg.contains("does not carry"))
        );
    }

    #[test]
    fn eq_reads_used_makers_and_rule() {
        let price_levels = per_maker();
        assert!(price_levels == per_maker());
        assert!(price_levels != price_levels.with_used("test_mm"));
        assert!(
            price_levels != self::price_levels(MakerPricing::Liquorice, QuoteRule::OncePerVenue)
        );
    }

    mod minimum_amount_in {
        use super::*;

        #[test]
        fn smallest_first_level_rounded_up() {
            let price_levels = price_levels(MakerPricing::Hashflow, QuoteRule::OncePerMaker);
            let minimum = price_levels.minimum_amount_in(&weth().address, &usdc().address);
            assert_eq!(minimum, Some(weth_amount(0.5)));
        }

        #[test]
        fn no_minimum_for_liquorice() {
            let minimum = per_maker().minimum_amount_in(&weth().address, &usdc().address);
            assert_eq!(minimum, None);
        }

        #[test]
        fn unquoted_pair() {
            let price_levels = price_levels(MakerPricing::Hashflow, QuoteRule::OncePerMaker);
            assert_eq!(price_levels.minimum_amount_in(&usdc().address, &weth().address), None);
        }
    }

    mod spot_price {
        use super::*;

        #[test]
        fn hashflow_best_first_level_across_makers() {
            let price_levels = price_levels(MakerPricing::Hashflow, QuoteRule::OncePerMaker);
            let price = price_levels
                .spot_price(&weth().address, &usdc().address)
                .unwrap();
            assert_eq!(price, 3010.0);
        }

        #[test]
        fn liquorice_best_average_across_makers() {
            // test_mm_2: (0.5 * 3010 + 1.5 * 2990) / 2 = 2995. test_mm: 20995 / 7 = 2999.29.
            let price = per_maker()
                .spot_price(&weth().address, &usdc().address)
                .unwrap();
            assert_eq!(price, 20995.0 / 7.0);
        }

        #[test]
        fn used_maker_is_skipped() {
            let price_levels = price_levels(MakerPricing::Hashflow, QuoteRule::OncePerMaker)
                .with_used("test_mm_2");
            let price = price_levels
                .spot_price(&weth().address, &usdc().address)
                .unwrap();
            assert_eq!(price, 3000.0);
        }

        #[test]
        fn unquoted_pair() {
            let result = per_maker().spot_price(&usdc().address, &weth().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("No market maker quotes"))
            );
        }

        #[test]
        fn every_maker_used() {
            let price_levels = per_maker()
                .with_used("test_mm")
                .with_used("test_mm_2");
            let result = price_levels.spot_price(&weth().address, &usdc().address);
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("already quoted in this route"))
            );
        }

        #[test]
        fn empty_levels() {
            let price_levels = AllMakerPriceLevels::new(
                vec![maker_price_levels("test_mm", &weth(), &usdc(), &[])],
                tokens(),
                MakerPricing::Liquorice,
                QuoteRule::OncePerMaker,
            )
            .unwrap();
            let result = price_levels.spot_price(&weth().address, &usdc().address);
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }
    }

    mod best_fill {
        use super::*;

        #[test]
        fn best_paying_maker() {
            // test_mm_2: 0.5 * 3010 = 1505. test_mm: 0.5 * 3000 = 1500.
            let price_levels = per_maker();
            let fill = weth_usdc_fill(&price_levels, weth_amount(0.5)).unwrap();
            assert_eq!(fill.maker_levels.market_maker, "test_mm_2");
            assert_eq!(fill.amount_out, usdc_amount(1505.0));
            assert_eq!(fill.remaining_amount_in, 0.0);
        }

        #[test]
        fn used_maker_is_skipped() {
            let price_levels = per_maker().with_used("test_mm_2");
            let fill = weth_usdc_fill(&price_levels, weth_amount(0.5)).unwrap();
            assert_eq!(fill.maker_levels.market_maker, "test_mm");
            assert_eq!(fill.amount_out, usdc_amount(1500.0));
        }

        #[test]
        fn used_maker_on_another_pair() {
            let price_levels = per_maker().with_used("test_mm");
            let result = price_levels.best_fill(
                &BigUint::from(100_000_000u64),
                &wbtc().address,
                &usdc().address,
            );
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("already quoted in this route"))
            );
        }

        #[test]
        fn once_per_venue_after_a_swap() {
            let price_levels = price_levels(MakerPricing::Liquorice, QuoteRule::OncePerVenue)
                .with_used("test_mm_2");
            let result = weth_usdc_fill(&price_levels, weth_amount(0.5));
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("already quoted in this route"))
            );
        }

        #[test]
        fn full_fill_beats_partial_fill() {
            // test_mm_2 fills 2 of 3 WETH at better prices; test_mm fills all 3:
            // 0.5 * 3000 + 1.5 * 3000 + 1.0 * 2999 = 8999.
            let price_levels = per_maker();
            let fill = weth_usdc_fill(&price_levels, weth_amount(3.0)).unwrap();
            assert_eq!(fill.maker_levels.market_maker, "test_mm");
            assert_eq!(fill.amount_out, usdc_amount(8999.0));
        }

        #[test]
        fn every_maker_partial() {
            // test_mm: 7 WETH for 20995. test_mm_2: 2 WETH for 5990.
            let price_levels = per_maker();
            let fill = weth_usdc_fill(&price_levels, weth_amount(8.0)).unwrap();
            assert_eq!(fill.maker_levels.market_maker, "test_mm");
            assert_eq!(fill.amount_out, usdc_amount(20995.0));
            assert_eq!(fill.remaining_amount_in, 1.0);
        }

        #[test]
        fn hashflow_declines_amount_below_every_first_level() {
            let price_levels = price_levels(MakerPricing::Hashflow, QuoteRule::OncePerMaker);
            let result = weth_usdc_fill(&price_levels, weth_amount(0.25));
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("Amount below minimum"))
            );
        }

        #[test]
        fn liquorice_fills_amount_below_every_first_level() {
            let price_levels = per_maker();
            let fill = weth_usdc_fill(&price_levels, weth_amount(0.25)).unwrap();
            assert_eq!(fill.amount_out, usdc_amount(752.5));
        }

        #[test]
        fn unquoted_pair() {
            let price_levels = per_maker();
            let result =
                price_levels.best_fill(&usdc_amount(10_000.0), &usdc().address, &weth().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("No market maker quotes"))
            );
        }
    }

    mod get_limits {
        use super::*;

        #[test]
        fn largest_maker() {
            // test_mm: 7 WETH for 20995 USDC. test_mm_2: 2 WETH for 5990 USDC.
            let (sell_limit, buy_limit) = per_maker()
                .get_limits(&weth().address, &usdc().address)
                .unwrap();
            assert_eq!(sell_limit, weth_amount(7.0));
            assert_eq!(buy_limit, usdc_amount(20995.0));
        }

        #[test]
        fn used_maker_is_skipped() {
            let (sell_limit, buy_limit) = per_maker()
                .with_used("test_mm")
                .get_limits(&weth().address, &usdc().address)
                .unwrap();
            assert_eq!(sell_limit, weth_amount(2.0));
            assert_eq!(buy_limit, usdc_amount(5990.0));
        }

        #[test]
        fn unquoted_pair() {
            let result = per_maker().get_limits(&wbtc().address, &weth().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("No market maker quotes"))
            );
        }
    }
}
