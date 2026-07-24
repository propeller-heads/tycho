//! Loads token prices and uses them to bound swap input amounts in tests.
//!
//! [`load_token_prices`] downloads the latest price snapshot for a chain;
//! [`cap_amount_to_eth_value`] uses it to cap an amount to a target ETH value.
//!
//! Prices come from a weekly CSV snapshot published to S3. Each price is denominated in raw token
//! units per 1 ETH (the raw amount of a token worth one ETH), so
//! `value_in_eth(raw_amount) = raw_amount / price`.

use std::{collections::HashMap, str::FromStr};

use miette::{IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use num_traits::{FromPrimitive, Zero};
use tracing::warn;
use tycho_common::{models::Chain, Bytes};

const S3_BASE_URL: &str =
    "https://s3.eu-central-1.amazonaws.com/repo.propellerheads-propellerheads/token-prices";

/// Downloads the latest token-price snapshot for `chain` and returns a map from token address to
/// its price in raw token units per 1 ETH.
pub async fn load_token_prices(chain: Chain) -> miette::Result<HashMap<Bytes, f64>> {
    let url = format!("{S3_BASE_URL}/{chain}/latest/token-prices.csv");
    let body = reqwest::get(&url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to download token prices from {url}"))?
        .text()
        .await
        .into_diagnostic()?;

    Ok(parse_token_prices(&body))
}

/// Parses the `address,price` CSV (with a header row) into an address-keyed price map. Rows with an
/// unparseable address or price are skipped with a warning rather than failing the whole load.
fn parse_token_prices(csv: &str) -> HashMap<Bytes, f64> {
    let mut prices = HashMap::new();
    for line in csv.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.splitn(2, ',');
        let (Some(address), Some(price)) = (fields.next(), fields.next()) else {
            warn!("Skipping malformed token price row: {line}");
            continue;
        };
        let Ok(address) = Bytes::from_str(address.trim()) else {
            warn!("Skipping token price row with invalid address: {address}");
            continue;
        };
        let Ok(price) = price.trim().parse::<f64>() else {
            warn!("Skipping token price row with invalid price for {address}: {price}");
            continue;
        };
        prices.insert(address, price);
    }
    prices
}

/// Caps `amount` so that its value does not exceed `max_value_eth` ETH, using `prices` (raw token
/// units per 1 ETH, keyed by token address).
///
/// Returns `amount` unchanged when the token has no known price, when the price is not a positive
/// number, or when `amount` is already below the cap. This keeps callers safe for tokens missing
/// from the snapshot, leaving them to apply their own fallback bounds.
pub fn cap_amount_to_eth_value(
    amount: BigUint,
    token: &Bytes,
    prices: &HashMap<Bytes, f64>,
    max_value_eth: f64,
) -> BigUint {
    let Some(price) = prices.get(token) else {
        return amount;
    };
    if *price <= 0.0 || !price.is_finite() {
        return amount;
    }
    // value_in_eth = amount / price, so the raw amount worth `max_value_eth` ETH is
    // max_value_eth * price.
    let Some(cap) = BigUint::from_f64(max_value_eth * price) else {
        return amount;
    };
    if cap.is_zero() {
        return amount;
    }
    amount.min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(hex: &str) -> Bytes {
        Bytes::from_str(hex).unwrap()
    }

    #[test]
    fn test_parses_valid_rows() {
        let csv = "address,price\n\
            0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48,1734922791\n\
            0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2,1e+18\n\
            not_an_address,123\n\
            0xdac17f958d2ee523a2206206994597c13d831ec7,not_a_number\n\
            \n";
        let prices = parse_token_prices(csv);

        assert_eq!(prices.len(), 2);
        assert_eq!(
            prices.get(&addr("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")),
            Some(&1734922791.0)
        );
        assert_eq!(prices.get(&addr("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")), Some(&1e18));
    }

    #[test]
    fn test_caps_amount_above_eth_value() {
        // WETH: 1e18 raw units per ETH. A 100 ETH amount capped to 5 ETH -> 5e18 raw units.
        let weth = addr("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let prices = HashMap::from([(weth.clone(), 1e18)]);

        let capped = cap_amount_to_eth_value(
            BigUint::from(100u128) * BigUint::from(10u128).pow(18),
            &weth,
            &prices,
            5.0,
        );

        assert_eq!(capped, BigUint::from(5u128) * BigUint::from(10u128).pow(18));
    }

    #[test]
    fn test_leaves_amount_below_cap_untouched() {
        let weth = addr("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let prices = HashMap::from([(weth.clone(), 1e18)]);

        let amount = BigUint::from(10u128).pow(18); // 1 ETH worth
        let capped = cap_amount_to_eth_value(amount.clone(), &weth, &prices, 5.0);

        assert_eq!(capped, amount);
    }
}
