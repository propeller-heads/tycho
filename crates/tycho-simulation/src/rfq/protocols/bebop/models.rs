use alloy::primitives::Address;
use prost::Message;
use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::rfq::errors::RFQError;

/// Protobuf message for Bebop pricing updates
#[derive(Clone, PartialEq, Message)]
pub struct BebopPricingUpdate {
    #[prost(message, repeated, tag = "1")]
    pub pairs: Vec<BebopPriceData>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Message)]
pub struct BebopPriceData {
    #[prost(bytes, tag = "1")]
    pub base: Vec<u8>,
    #[prost(bytes, tag = "2")]
    pub quote: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub last_update_ts: u64,
    /// Flat array: [price1, size1, price2, size2, ...]
    #[prost(float, repeated, packed = "true", tag = "4")]
    pub bids: Vec<f32>,
    /// Flat array: [price1, size1, price2, size2, ...]
    #[prost(float, repeated, packed = "true", tag = "5")]
    pub asks: Vec<f32>,
}

impl BebopPriceData {
    /// Convert flat array to Vec<(f64, f64)> pairs
    /// Input: [price1, size1, price2, size2, ...]
    /// Output: [(price1, size1), (price2, size2), ...]
    pub fn to_price_size_pairs(array: &[f32]) -> Vec<(f64, f64)> {
        array
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| (chunk[0] as f64, chunk[1] as f64))
            .collect()
    }

    pub fn get_bids(&self) -> Vec<(f64, f64)> {
        Self::to_price_size_pairs(&self.bids)
    }

    pub fn get_asks(&self) -> Vec<(f64, f64)> {
        Self::to_price_size_pairs(&self.asks)
    }

    pub fn get_pair_key(&self) -> String {
        // Convert raw bytes to Address (which provides checksum formatting)
        let base_addr = Address::from_slice(&self.base);
        let quote_addr = Address::from_slice(&self.quote);
        format!("{base_addr}/{quote_addr}")
    }

    /// Calculates Total Value Locked (TVL) based on bid/ask levels.
    ///
    /// TVL is calculated using the formula from Bebop's documentation:
    /// https://docs.bebop.xyz/bebop/bebop-api-pmm-rfq/rfq-api-endpoints/pricing#interpreting-price-levels
    ///
    /// Returns the average of bid and ask TVLs across all price levels.
    ///
    /// Note: This calculation normalizes the quote token in case quote_price_data is passed.
    ///
    /// # Parameters
    /// - `quote_price_data`: Optional price data for converting the quote token to an approved
    ///   token
    pub fn calculate_tvl(&self, quote_price_data: Option<&BebopPriceData>) -> f64 {
        let bid_tvl: f64 = self
            .get_bids()
            .iter()
            .map(|(price, size)| price * size)
            .sum();

        let ask_tvl: f64 = self
            .get_asks()
            .iter()
            .map(|(price, size)| price * size)
            .sum();

        let mut total_tvl = (bid_tvl + ask_tvl) / 2.0;

        // If quote price data is provided, we need to normalize the TVL to be in
        // one of the approved token (for example USDC)
        if let Some(quote_data) = quote_price_data {
            if let Some(price_of_quote_token) = quote_data.get_mid_price(total_tvl, &self.quote) {
                total_tvl *= price_of_quote_token;
            } else {
                // Quote token has no TVL in one of the approved tokens (for normalizations)
                return 0.0;
            }
        }
        total_tvl
    }

    /// Gets the mid price by averaging bid and ask
    ///
    /// # Parameters
    /// - `amount`: The amount of tokens to convert
    /// - `sell_token`: The token we're selling
    ///
    /// # Returns
    /// The average price from using both bids and asks
    pub fn get_mid_price(&self, amount: f64, sell_token: &[u8]) -> Option<f64> {
        // Check if sell_token matches either base or quote
        if sell_token != self.base.as_slice() && sell_token != self.quote.as_slice() {
            return None;
        }

        let inverse = sell_token == self.quote.as_slice();
        let asks_price = self.get_price_for_levels(amount, self.get_asks(), inverse)?;
        let bids_price = self.get_price_for_levels(amount, self.get_bids(), inverse)?;
        Some((asks_price + bids_price) / 2.0)
    }

    /// Helper to calculate price from specific price levels
    ///
    /// # Parameters
    /// - `amount_in`: Amount of input tokens
    /// - `price_levels`: Price levels to use
    /// - `invert`: Whether to invert the price levels (for quote->base trades)
    ///
    /// # Returns
    /// Price (output per input)
    fn get_price_for_levels(
        &self,
        amount_in: f64,
        price_levels: Vec<(f64, f64)>,
        invert: bool,
    ) -> Option<f64> {
        if price_levels.is_empty() {
            return None;
        }

        let levels = if invert { Self::invert_price_levels(price_levels) } else { price_levels };

        let (amount_out, remaining_in) = self.get_amount_out_from_levels(amount_in, levels);
        Some(amount_out / (amount_in - remaining_in))
    }

    /// Calculates the total token output for a given token input using provided price levels.
    ///
    /// Iterates over the given `price_levels`, consuming as much liquidity as available at each
    /// price level until the input amount is fully consumed or liquidity runs out.
    ///
    /// This method assumes that the size of the price levels is already in the same token
    /// denomination as the `amount_in`. It does not return an error if liquidity is
    /// insufficient to fill the entire `amount_in`. Instead, it returns the partially filled
    /// `amount_out` along with the `remaining_amount_in`.
    ///
    ///
    /// # Parameters
    /// - `amount_in`: The amount of base tokens to trade.
    /// - `price_levels`: A vector of `(price, size)` tuples representing available liquidity at
    ///   each price level.
    ///
    /// # Returns
    /// A tuple `(amount_out, remaining_amount_in)`:
    /// - `amount_out`: The total quote token output from the trade.
    /// - `remaining_amount_in`: The portion of `amount_in` that could not be filled due to lack of
    ///   liquidity.
    pub fn get_amount_out_from_levels(
        &self,
        amount_in: f64,
        price_levels: Vec<(f64, f64)>,
    ) -> (f64, f64) {
        let mut remaining_amount_in = amount_in;
        let mut amount_out = 0.0;

        for (price, tokens_available) in price_levels.iter() {
            if remaining_amount_in <= 0.0 {
                break;
            }

            let amount_in_available_to_trade = remaining_amount_in.min(*tokens_available);

            amount_out += amount_in_available_to_trade * price;
            remaining_amount_in -= amount_in_available_to_trade;
        }
        (amount_out, remaining_amount_in)
    }

    /// Inverts price levels for quote-to-base conversions
    ///
    /// Converts price levels from `(quote_per_base, base_size)` format to `(base_per_quote,
    /// quote_size)` format. This allows reusing `get_amount_out_from_levels` for inverted
    /// trading directions.
    ///
    /// # Parameters
    /// - `price_levels`: Vector of `(quote_per_base, base_size)` tuples
    ///
    /// # Returns
    /// Vector of `(base_per_quote, quote_size)` tuples with zero prices filtered out
    ///
    /// # Example
    /// ```
    /// // Input: (0.11 TAMARA/USDC, 3000 USDC)
    /// // Output: (9.09 USDC/TAMARA, 330 TAMARA)
    /// ```
    fn invert_price_levels(price_levels: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
        price_levels
            .iter()
            .filter(|(price, _)| *price > 0.0)
            .map(|(price_quote_per_base, base_available)| {
                let price_base_per_quote = 1.0 / price_quote_per_base;
                let quote_size = base_available * price_quote_per_base;
                (price_base_per_quote, quote_size)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BebopQuoteResponse {
    Success(Box<BebopQuotePartial>),
    Error(BebopApiError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BebopApiError {
    pub error: BebopErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BebopErrorDetail {
    pub error_code: u32,
    pub message: String,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BebopQuotePartial {
    pub status: String,
    pub settlement_address: Bytes,
    pub tx: TxData,
    pub to_sign: BebopOrderToSign,
    pub partial_fill_offset: u64,
}

impl BebopQuotePartial {
    pub fn validate(&self, params: &GetAmountOutParams) -> Result<(), RFQError> {
        match &self.to_sign {
            BebopOrderToSign::Single(single) => {
                if single.taker_token != params.token_in {
                    return Err(RFQError::FatalError(format!(
                        "Base token mismatch: expected {}, got {}",
                        params.token_in, single.taker_token
                    )));
                }
                if single.maker_token != params.token_out {
                    return Err(RFQError::FatalError(format!(
                        "Quote token mismatch: expected {}, got {}",
                        params.token_out, single.maker_token
                    )));
                }
                self.validate_taker_and_receiver(&single.taker_address, &single.receiver, params)?;
                let amount_in = params.amount_in.to_string();
                if single.taker_amount != amount_in {
                    return Err(RFQError::FatalError(format!(
                        "Base token amount mismatch: expected {}, got {}",
                        amount_in, single.taker_amount
                    )));
                }
            }
            BebopOrderToSign::Aggregate(aggregate) => {
                self.validate_taker_and_receiver(
                    &aggregate.taker_address,
                    &aggregate.receiver,
                    params,
                )?;
            }
        }
        Ok(())
    }

    /// Validates the signed order's taker and receiver against the requested params.
    ///
    /// Depending on the API account configuration, Bebop returns orders in one of two
    /// settlement modes:
    /// - Settlement mode: the order settles directly on the Bebop settlement contract, so its taker
    ///   and receiver must be the requested sender and receiver.
    /// - Router mode: settlement is wrapped through the Bebop router contract, which is the
    ///   transaction target (`tx.to`) and also the order's taker and receiver — it fills the order
    ///   against itself and forwards the output to the caller. The requested sender and receiver
    ///   only appear inside the router calldata and cannot be checked here; the executor enforces
    ///   on-chain that only the known settlement/router contracts are called.
    fn validate_taker_and_receiver(
        &self,
        taker_address: &Bytes,
        receiver: &Bytes,
        params: &GetAmountOutParams,
    ) -> Result<(), RFQError> {
        if *taker_address == self.tx.to {
            if receiver != taker_address {
                return Err(RFQError::FatalError(format!(
                    "Receiver address mismatch for router-mode quote: expected {taker_address}, got {receiver}"
                )));
            }
        } else {
            if *taker_address != params.sender {
                return Err(RFQError::FatalError(format!(
                    "Taker address mismatch: expected {}, got {taker_address}",
                    params.sender
                )));
            }
            if *receiver != params.receiver {
                return Err(RFQError::FatalError(format!(
                    "Receiver address mismatch: expected {}, got {receiver}",
                    params.receiver
                )));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BebopOrderToSign {
    Single(Box<SingleOrderToSign>),
    Aggregate(Box<AggregateOrderToSign>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxData {
    pub to: Bytes,
    pub data: Bytes,
    pub value: String,
    pub from: Bytes,
    pub gas: u64,
    #[serde(rename = "gasPrice")]
    pub gas_price: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleOrderToSign {
    pub maker_address: Bytes,
    pub taker_address: Bytes,
    pub maker_token: Bytes,
    pub taker_token: Bytes,
    pub maker_amount: String,
    pub taker_amount: String,
    pub maker_nonce: String,
    pub expiry: u64,
    pub receiver: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateOrderToSign {
    pub taker_address: Bytes,
    pub maker_tokens: Vec<Vec<Bytes>>,
    pub taker_tokens: Vec<Vec<Bytes>>,
    pub maker_amounts: Vec<Vec<String>>,
    pub taker_amounts: Vec<Vec<String>>,
    pub expiry: u64,
    pub receiver: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_tvl_no_normalization() {
        let price_data = BebopPriceData {
            base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(), // WETH
            quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 1.0f32, 1999.0f32, 2.0f32],
            asks: vec![2001.0f32, 1.5f32, 2002.0f32, 1.0f32],
        };

        let tvl = price_data.calculate_tvl(None);

        // Expected calculation:
        // Bid TVL: (2000.0 * 1.0) + (1999.0 * 2.0) = 2000.0 + 3998.0 = 5998.0
        // Ask TVL: (2001.0 * 1.5) + (2002.0 * 1.0) = 3001.5 + 2002.0 = 5003.5
        // Total TVL: (5998.0 + 5003.5) / 2 = 5500.75
        assert!((tvl - 5500.75).abs() < 0.01);
    }

    #[test]
    fn test_calculate_tvl_with_normalization() {
        // Scenario: We have price data for ETH/TAMARA. One ETH is normally around 100 TAMARA,
        // and one TAMARA is around 10 USDC.
        let price_data_eth_tamara = BebopPriceData {
            base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(), // WETH
            quote: hex::decode("1234567890123456789012345678901234567890").unwrap(), // Mock TAMARA
            last_update_ts: 1234567890,
            bids: vec![99.0f32, 1.0f32, 98.0f32, 2.0f32],
            asks: vec![101.0f32, 1.0f32, 102.0f32, 2.0f32],
        };
        let price_data_tamara_usdc = BebopPriceData {
            base: hex::decode("1234567890123456789012345678901234567890").unwrap(), // Mock TAMARA
            quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            last_update_ts: 1234567890,
            bids: vec![9.0f32, 300.0f32, 8.0f32, 300.0f32],
            asks: vec![11.0f32, 300.0f32, 12.0f32, 300.0f32],
        };

        let tvl = price_data_eth_tamara.calculate_tvl(Some(&price_data_tamara_usdc));

        // Expected calculation:
        // TVL of ETH in TAMARA = (99 * 1 + 98 * 2 + 101 * 1 + 102 * 2) / 2 = 300
        // Price of TAMARA in USDC = around 10
        // TVL of ETH in USDC = 300 * 10 = 3000
        assert_eq!(tvl, 3000.0);
    }

    #[test]
    fn test_calculate_tvl_with_inverted_normalization() {
        // Scenario: We have price data for ETH/TAMARA. One ETH is normally around 100 TAMARA,
        // and we have price data for USDC/TAMARA (inverted - normally we'd want TAMARA/USDC).
        // One USDC is around 0.1 TAMARA (so one TAMARA is around 10 USDC).
        let price_data_eth_tamara = BebopPriceData {
            base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(), // WETH
            quote: hex::decode("1234567890123456789012345678901234567890").unwrap(), // Mock TAMARA
            last_update_ts: 1234567890,
            bids: vec![99.0f32, 1.0f32, 98.0f32, 2.0f32],
            asks: vec![101.0f32, 1.0f32, 102.0f32, 2.0f32],
        };
        // This is USDC/TAMARA - base=USDC, quote=TAMARA
        // To sell USDC for TAMARA: use bids (price in TAMARA per USDC)
        // To buy USDC with TAMARA: use asks (price in TAMARA per USDC)
        let price_data_usdc_tamara = BebopPriceData {
            base: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            quote: hex::decode("1234567890123456789012345678901234567890").unwrap(), // Mock TAMARA
            last_update_ts: 1234567890,
            // Price in TAMARA per USDC
            // 1 USDC = ~0.1 TAMARA, so we use smaller numbers
            bids: vec![0.09f32, 3000.0f32, 0.08f32, 3000.0f32], /* Selling USDC gets us 0.09-0.08
                                                                 * TAMARA per USDC */
            asks: vec![0.11f32, 3000.0f32, 0.12f32, 3000.0f32], /* Buying USDC costs us 0.11-0.12
                                                                 * TAMARA per USDC */
        };

        let tvl = price_data_eth_tamara.calculate_tvl(Some(&price_data_usdc_tamara));

        // Expected calculation:
        // TVL of ETH in TAMARA = (99 * 1 + 98 * 2 + 101 * 1 + 102 * 2) / 2 = 300 TAMARA
        // We have 300 TAMARA and want to convert to USDC
        // Using the inverted pair (USDC/TAMARA), we want to buy USDC with TAMARA
        // Using asks (price in TAMARA per USDC):
        //   First level: 0.11 TAMARA/USDC, 3000 USDC available
        //   We need 330 TAMARA to buy all 3000 USDC, but we only have 300 TAMARA
        //   So we can buy: 300 / 0.11 = 2727.27 USDC
        // Using bids (price in TAMARA per USDC):
        //   First level: 0.09 TAMARA/USDC, 3000 USDC available
        //   We need 270 TAMARA to buy all 3000 USDC, we have 300, so we buy all 3000
        //   Remaining: 30 TAMARA
        //   Second level: 0.08 TAMARA/USDC, 3000 USDC available
        //   We can buy: 30 / 0.08 = 375 USDC
        //   Total from bids: 3000 + 375 = 3375 USDC
        // Mid price: (2727.27 + 3375) / 2 = 3051.14 USDC
        assert!((tvl - 3051.14).abs() < 1.0);
    }

    #[test]
    fn test_get_mid_price_bidirectional() {
        // Test normal direction: base -> quote
        let weth_addr = hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc_addr = hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();

        let price_data = BebopPriceData {
            base: weth_addr.clone(),
            quote: usdc_addr.clone(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };

        // Get price for selling 3 WETH for USDC
        let usdc_price = price_data.get_mid_price(3.0, &weth_addr);
        // Sell 3.0 tokens: 2.0 at 2000.0 + 1.0 at 1999.0 = 5999.0 total, price = 5999/3 = 1999.67
        // Buy 3.0 tokens: 3.0 at 2001.0 = 6003.0 total, price = 6003/3 = 2001.0
        // Mid price = (1999.67 + 2001.0) / 2 = 2000.33 USDC per WETH
        assert!((usdc_price.unwrap() - 2000.3333333333335).abs() < 0.01);

        // Test inverted direction: quote -> base
        // Get price for selling USDC for WETH
        let weth_price = price_data.get_mid_price(6000.0, &usdc_addr);
        // This should be roughly 1/2000 = 0.0005 WETH per USDC
        assert!(weth_price.is_some());
        let price = weth_price.unwrap();
        assert!((price - 0.0005).abs() < 0.0001);

        // Test with non-matching tokens
        let dai_addr = hex::decode("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let result = price_data.get_mid_price(100.0, &dai_addr);
        assert_eq!(result, None);
    }

    #[test]
    fn test_get_mid_price() {
        let weth_addr = hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(); // WETH
        let usdc_addr = hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(); // USDC

        let price_data = BebopPriceData {
            base: weth_addr.clone(),
            quote: usdc_addr.clone(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };

        // Test mid price for larger amount spanning multiple levels (selling WETH for USDC)
        let mid_price_large = price_data.get_mid_price(3.0, &weth_addr);
        // Sell 3.0 tokens: 2.0 at 2000.0 + 1.0 at 1999.0 = 5999.0 total, price = 5999/3 = 1999.67
        // Buy 3.0 tokens: 3.0 at 2001.0 = 6003.0 total, price = 6003/3 = 2001.0
        // Mid price = (1999.67 + 2001.0) / 2 = 2000.33 USDC per WETH
        assert!((mid_price_large.unwrap() - 2000.3333333333335).abs() < 0.01);

        // Test missing bids. Token considered untradeable.
        let price_data = BebopPriceData {
            base: weth_addr.clone(),
            quote: usdc_addr.clone(),
            last_update_ts: 1234567890,
            bids: vec![],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };
        assert_eq!(price_data.get_mid_price(3.0, &weth_addr), None);

        // Test missing asks. Token considered untradeable.
        let price_data = BebopPriceData {
            base: weth_addr.clone(),
            quote: usdc_addr.clone(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![],
        };
        assert_eq!(price_data.get_mid_price(3.0, &weth_addr), None);

        // Test not enough liquidity (give estimate based on existing liquidity)
        let price_data = BebopPriceData {
            base: weth_addr.clone(),
            quote: usdc_addr.clone(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };
        let insufficient_mid = price_data.get_mid_price(10.0, &weth_addr);
        // With 10 WETH but only 5 WETH liquidity, we get partial fills
        // The price returned is still an average price
        assert_eq!(insufficient_mid, Some(2000.325));
    }

    #[test]
    fn test_invert_price_levels() {
        // Test case: USDC/TAMARA pair with asks
        // Original: (0.11 TAMARA/USDC, 3000 USDC available)
        // Inverted: (9.09 USDC/TAMARA, 330 TAMARA available)
        let price_levels = vec![(0.11, 3000.0), (0.12, 3000.0)];

        let inverted = BebopPriceData::invert_price_levels(price_levels);

        assert_eq!(inverted.len(), 2);

        // First level: 1/0.11 = 9.09 USDC/TAMARA, 3000 * 0.11 = 330 TAMARA
        assert!((inverted[0].0 - 9.090909090909092).abs() < 0.0001);
        assert!((inverted[0].1 - 330.0).abs() < 0.0001);

        // Second level: 1/0.12 = 8.33 USDC/TAMARA, 3000 * 0.12 = 360 TAMARA
        assert!((inverted[1].0 - 8.333333333333334).abs() < 0.0001);
        assert!((inverted[1].1 - 360.0).abs() < 0.0001);
    }

    #[cfg(test)]
    mod bebop_quote_partial_validate_tests {
        use std::str::FromStr;

        use num_bigint::BigUint;
        use tycho_common::models::protocol::GetAmountOutParams;

        use super::*;

        fn hex_to_bytes(hex: &str) -> Bytes {
            Bytes::from_str(hex).unwrap()
        }

        fn single_order() -> SingleOrderToSign {
            SingleOrderToSign {
                maker_address: hex_to_bytes("0x1111111111111111111111111111111111111111"),
                taker_address: hex_to_bytes("0x2222222222222222222222222222222222222222"),
                maker_token: hex_to_bytes("0x3333333333333333333333333333333333333333"),
                taker_token: hex_to_bytes("0x4444444444444444444444444444444444444444"),
                maker_amount: "2000".to_string(),
                taker_amount: "1000".to_string(),
                maker_nonce: "1".to_string(),
                expiry: 123456,
                receiver: hex_to_bytes("0x5555555555555555555555555555555555555555"),
            }
        }

        fn aggregate_order() -> AggregateOrderToSign {
            AggregateOrderToSign {
                taker_address: hex_to_bytes("0x2222222222222222222222222222222222222222"),
                maker_tokens: vec![vec![hex_to_bytes(
                    "0x3333333333333333333333333333333333333333",
                )]],
                taker_tokens: vec![vec![hex_to_bytes(
                    "0x4444444444444444444444444444444444444444",
                )]],
                maker_amounts: vec![vec!["2000".to_string()]],
                taker_amounts: vec![vec!["1000".to_string()]],
                expiry: 123456,
                receiver: hex_to_bytes("0x5555555555555555555555555555555555555555"),
            }
        }

        fn params() -> GetAmountOutParams {
            GetAmountOutParams {
                amount_in: BigUint::from(1000u32),
                token_in: hex_to_bytes("0x4444444444444444444444444444444444444444"),
                token_out: hex_to_bytes("0x3333333333333333333333333333333333333333"),
                sender: hex_to_bytes("0x2222222222222222222222222222222222222222"),
                receiver: hex_to_bytes("0x5555555555555555555555555555555555555555"),
            }
        }

        fn quote_partial_single() -> BebopQuotePartial {
            BebopQuotePartial {
                status: "success".to_string(),
                settlement_address: hex_to_bytes("0x9999999999999999999999999999999999999999"),
                tx: TxData {
                    to: hex_to_bytes("0x8888888888888888888888888888888888888888"),
                    data: hex_to_bytes("0x1234"),
                    value: "0".to_string(),
                    from: hex_to_bytes("0x7777777777777777777777777777777777777777"),
                    gas: 21000,
                    gas_price: 100,
                },
                to_sign: BebopOrderToSign::Single(Box::new(single_order())),
                partial_fill_offset: 0,
            }
        }

        fn quote_partial_aggregate() -> BebopQuotePartial {
            BebopQuotePartial {
                status: "success".to_string(),
                settlement_address: hex_to_bytes("0x9999999999999999999999999999999999999999"),
                tx: TxData {
                    to: hex_to_bytes("0x8888888888888888888888888888888888888888"),
                    data: hex_to_bytes("0x1234"),
                    value: "0".to_string(),
                    from: hex_to_bytes("0x7777777777777777777777777777777777777777"),
                    gas: 21000,
                    gas_price: 100,
                },
                to_sign: BebopOrderToSign::Aggregate(Box::new(aggregate_order())),
                partial_fill_offset: 0,
            }
        }

        #[test]
        fn test_validate_single_success() {
            let quote = quote_partial_single();
            let params = params();
            assert!(quote.validate(&params).is_ok());
        }

        #[test]
        fn test_validate_single_base_token_mismatch() {
            let mut quote = quote_partial_single();
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.taker_token = hex_to_bytes("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Base token mismatch"));
        }

        #[test]
        fn test_validate_single_quote_token_mismatch() {
            let mut quote = quote_partial_single();
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.maker_token = hex_to_bytes("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Quote token mismatch"));
        }

        #[test]
        fn test_validate_single_taker_address_mismatch() {
            let mut quote = quote_partial_single();
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.taker_address = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Taker address mismatch"));
        }

        #[test]
        fn test_validate_single_receiver_mismatch() {
            let mut quote = quote_partial_single();
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.receiver = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Receiver address mismatch"));
        }

        #[test]
        fn test_validate_single_base_token_amount_mismatch() {
            let mut quote = quote_partial_single();
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.taker_amount = "9999".to_string();
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Base token amount mismatch"));
        }

        #[test]
        fn test_validate_aggregate_success() {
            let quote = quote_partial_aggregate();
            let params = params();
            assert!(quote.validate(&params).is_ok());
        }

        #[test]
        fn test_validate_aggregate_taker_address_mismatch() {
            let mut quote = quote_partial_aggregate();
            if let BebopOrderToSign::Aggregate(ref mut agg) = quote.to_sign {
                agg.taker_address = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Taker address mismatch"));
        }

        #[test]
        fn test_validate_aggregate_receiver_mismatch() {
            let mut quote = quote_partial_aggregate();
            if let BebopOrderToSign::Aggregate(ref mut agg) = quote.to_sign {
                agg.receiver = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Receiver address mismatch"));
        }

        const BEBOP_ROUTER: &str = "0xBeb0009ACa35087ce7cCF11637E24dd1Aad3bf2A";

        fn make_router_mode(quote: &mut BebopQuotePartial) {
            quote.tx.to = hex_to_bytes(BEBOP_ROUTER);
            match quote.to_sign {
                BebopOrderToSign::Single(ref mut single) => {
                    single.taker_address = hex_to_bytes(BEBOP_ROUTER);
                    single.receiver = hex_to_bytes(BEBOP_ROUTER);
                }
                BebopOrderToSign::Aggregate(ref mut agg) => {
                    agg.taker_address = hex_to_bytes(BEBOP_ROUTER);
                    agg.receiver = hex_to_bytes(BEBOP_ROUTER);
                }
            }
        }

        #[test]
        fn test_validate_single_router_mode_success() {
            let mut quote = quote_partial_single();
            make_router_mode(&mut quote);
            let params = params();
            assert!(quote.validate(&params).is_ok());
        }

        #[test]
        fn test_validate_aggregate_router_mode_success() {
            let mut quote = quote_partial_aggregate();
            make_router_mode(&mut quote);
            let params = params();
            assert!(quote.validate(&params).is_ok());
        }

        #[test]
        fn test_validate_single_router_mode_receiver_mismatch() {
            let mut quote = quote_partial_single();
            make_router_mode(&mut quote);
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.receiver = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Receiver address mismatch for router-mode quote"));
        }

        #[test]
        fn test_validate_single_taker_neither_sender_nor_tx_to() {
            let mut quote = quote_partial_single();
            make_router_mode(&mut quote);
            if let BebopOrderToSign::Single(ref mut single) = quote.to_sign {
                single.taker_address = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            }
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Taker address mismatch"));
        }
    }
}
