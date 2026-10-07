//! Pricing a book's liquidity in USD, for venues that publish neither.

use std::collections::HashSet;

use tycho_common::Bytes;

use crate::book::levels::Levels;

/// `raw_tvl`, a notional quoted in `quote_token`, expressed in USD quote-token units: unchanged
/// when `quote_token` is one of them, and otherwise converted at the price of a pair that sells
/// `quote_token` for one. `None` when `priced_pairs` holds no such pair — the venue's response
/// does not say what the book is worth, so it cannot be measured against a USD floor.
///
/// `priced_pairs` are the `(base, quote, levels)` of everything the venue priced in the same
/// response; the first pair that both converts and has a price is the one used, so a venue that
/// prices its quote token against several USD tokens may pick either.
pub fn in_usd_quote_tokens<'a>(
    raw_tvl: f64,
    quote_token: &Bytes,
    usd_quote_tokens: &HashSet<Bytes>,
    priced_pairs: impl IntoIterator<Item = (&'a Bytes, &'a Bytes, &'a Levels)>,
) -> Option<f64> {
    if usd_quote_tokens.contains(quote_token) {
        return Some(raw_tvl);
    }
    priced_pairs
        .into_iter()
        .filter(|(base, quote, _)| *base == quote_token && usd_quote_tokens.contains(*quote))
        .find_map(|(_, _, levels)| levels.average_price(1.0))
        .map(|price| raw_tvl * price)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;
    use crate::book::levels::PriceLevel;

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    fn address(hex: &str) -> Bytes {
        Bytes::from_str(hex).unwrap()
    }

    /// One WETH buys 3000 USDC.
    fn weth_usdc() -> (Bytes, Bytes, Levels) {
        (
            address(WETH),
            address(USDC),
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 3000.0 }]).unwrap(),
        )
    }

    #[rstest]
    // A book already quoted in USDC is worth its notional, whether or not anything else is
    // priced in the same response.
    #[case::quoted_in_a_usd_token(USDC, true, Some(500.0))]
    #[case::quoted_in_a_usd_token_without_any_pair(USDC, false, Some(500.0))]
    // 500 WETH of notional, at 3000 USDC each.
    #[case::converted_through_a_priced_pair(WETH, true, Some(1_500_000.0))]
    #[case::no_pair_prices_the_quote_token(WETH, false, None)]
    fn tvl_is_converted_into_usd_quote_tokens(
        #[case] quote_token: &str,
        #[case] with_weth_usdc: bool,
        #[case] expected: Option<f64>,
    ) {
        let usd_quote_tokens = HashSet::from([address(USDC)]);
        let pairs: Vec<_> = with_weth_usdc
            .then(weth_usdc)
            .into_iter()
            .collect();

        let normalized = in_usd_quote_tokens(
            500.0,
            &address(quote_token),
            &usd_quote_tokens,
            pairs
                .iter()
                .map(|(base, quote, levels)| (base, quote, levels)),
        );

        assert_eq!(normalized, expected);
    }
}
