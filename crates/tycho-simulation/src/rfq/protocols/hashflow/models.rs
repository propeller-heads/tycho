use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use crate::rfq::errors::RFQError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowPriceLevelsResponse {
    pub status: String, // "success" or "fail"
    pub levels: Option<HashMap<String, Vec<HashflowMarketMakerLevels>>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HashflowMarketMakerLevels {
    pub pair: HashflowPair,
    pub levels: Vec<HashflowPriceLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HashflowPair {
    #[serde(rename = "baseToken", deserialize_with = "deserialize_string_to_checksummed_bytes")]
    pub base_token: Bytes,
    #[serde(rename = "quoteToken", deserialize_with = "deserialize_string_to_checksummed_bytes")]
    pub quote_token: Bytes,
}

fn deserialize_string_to_checksummed_bytes<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let address = Address::from_str(&s).map_err(serde::de::Error::custom)?;
    let checksum = address.to_checksum(None);
    let checksum_bytes = Bytes::from_str(&checksum).map_err(serde::de::Error::custom)?;
    Ok(checksum_bytes)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HashflowPriceLevel {
    #[serde(
        rename = "q",
        deserialize_with = "deserialize_string_to_f64",
        serialize_with = "serialize_f64_to_string"
    )]
    /// Quantity of tokens that can be traded at this level
    pub quantity: f64,
    #[serde(
        rename = "p",
        deserialize_with = "deserialize_string_to_f64",
        serialize_with = "serialize_f64_to_string"
    )]
    /// Price per token at this level
    pub price: f64,
}

fn deserialize_string_to_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse()
        .map_err(serde::de::Error::custom)
}

fn serialize_f64_to_string<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

impl HashflowMarketMakerLevels {
    /// Calculate Total Value Locked (TVL) for this market maker level
    pub fn calculate_tvl(&self) -> f64 {
        self.levels
            .iter()
            .map(|level| level.quantity * level.price)
            .sum()
    }

    /// Calculate quote token amount for trading base tokens using price levels
    ///
    /// NOTE: This method is meant just to be used as an estimate - as it does not
    /// error or return None if there is not enough liquidity to cover base token amount.
    /// This method will only return None if there are absolutely no price levels.
    ///
    /// # Parameters
    /// - `base_token_amount`: The amount of tokens to trade
    ///
    /// # Returns
    /// Estimated price based on available liquidity
    pub fn get_price(&self, base_token_amount: f64) -> Option<f64> {
        // We treat all levels as available liquidity regardless of sell/buy direction
        if self.levels.is_empty() {
            return None;
        }

        let (total_quote_token, remaining_base_token) =
            self.get_amount_out_from_levels(base_token_amount);

        // If we can't fill the whole order (ran out of liquidity), calculate the price based on
        // the amount that we could fill, in order to have at least some price estimate
        Some(total_quote_token / (base_token_amount - remaining_base_token))
    }

    /// Calculates the total token output for a given token input using available price levels.
    ///
    /// Iterates over the price levels, consuming as much liquidity as available at each
    /// price level until the input amount is fully consumed or liquidity runs out.
    ///
    /// # Parameters
    /// - `amount_in`: The amount of base tokens to trade.
    ///
    /// # Returns
    /// A tuple of (amount_out, remaining_amount_in) where:
    /// - `amount_out`: The total quote tokens that can be obtained
    /// - `remaining_amount_in`: Any remaining base tokens that couldn't be filled
    pub fn get_amount_out_from_levels(&self, amount_in: f64) -> (f64, f64) {
        let mut remaining_amount_in = amount_in;
        let mut total_amount_out = 0.0;

        for level in &self.levels {
            if remaining_amount_in <= 0.0 {
                break;
            };

            let amount_to_fill = remaining_amount_in.min(level.quantity);
            total_amount_out += amount_to_fill * level.price;
            remaining_amount_in -= amount_to_fill;
        }

        (total_amount_out, remaining_amount_in)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowMarketMakersResponse {
    #[serde(rename = "marketMakers")]
    pub market_makers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowQuoteRequest {
    pub source: String,
    #[serde(rename = "baseChain")]
    pub base_chain: HashflowChain,
    #[serde(rename = "quoteChain")]
    pub quote_chain: HashflowChain,
    pub rfqs: Vec<HashflowRFQ>,
    pub calldata: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowChain {
    #[serde(rename = "chainType")]
    chain_type: String,
    #[serde(rename = "chainId")]
    chain_id: u64,
}

impl From<Chain> for HashflowChain {
    fn from(value: Chain) -> Self {
        HashflowChain { chain_type: "evm".to_string(), chain_id: value.id() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowRFQ {
    #[serde(rename = "baseToken")]
    pub base_token: String,
    #[serde(rename = "quoteToken")]
    pub quote_token: String,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    #[serde(rename = "baseTokenAmount")]
    pub base_token_amount: Option<String>,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    #[serde(rename = "quoteTokenAmount", skip_serializing_if = "Option::is_none")]
    pub quote_token_amount: Option<String>,
    pub trader: String,
    #[serde(rename = "effectiveTrader")]
    pub effective_trader: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowQuoteResponse {
    pub status: String,
    pub error: Option<String>,
    #[serde(rename = "rfqId")]
    rfq_id: String,
    #[serde(rename = "internalRfqIds")]
    internal_rfq_ids: Option<Vec<String>>,
    pub quotes: Option<Vec<HashflowQuote>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowQuote {
    #[serde(rename = "quoteData")]
    pub quote_data: HashflowQuoteData,
    pub signature: Bytes,
    #[serde(rename = "targetContract")]
    pub target_contract: Option<Bytes>,
    pub value: Option<String>,
}

impl HashflowQuote {
    pub fn validate(&self, params: &GetAmountOutParams) -> Result<(), RFQError> {
        if self.quote_data.base_token != params.token_in {
            return Err(RFQError::FatalError(format!(
                "Base token mismatch: expected {}, got {}",
                params.token_in, self.quote_data.base_token
            )));
        }
        if self.quote_data.quote_token != params.token_out {
            return Err(RFQError::FatalError(format!(
                "Quote token mismatch: expected {}, got {}",
                params.token_out, self.quote_data.quote_token
            )));
        }
        if self.quote_data.trader != params.receiver {
            return Err(RFQError::FatalError(format!(
                "Trader address mismatch: expected {}, got {}",
                params.receiver, self.quote_data.trader
            )));
        }
        let effective_trader = self
            .quote_data
            .effective_trader
            .clone()
            .unwrap_or_else(|| self.quote_data.trader.clone());
        if effective_trader != params.sender {
            return Err(RFQError::FatalError(format!(
                "Effective trader mismatch: expected {}, got {effective_trader}",
                params.sender
            )));
        }
        if self.quote_data.base_token_amount != params.amount_in.to_string() {
            return Err(RFQError::FatalError(format!(
                "Base token amount mismatch: expected {}, got {}",
                params.amount_in, self.quote_data.base_token_amount
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowQuoteData {
    #[serde(rename = "baseToken")]
    pub base_token: Bytes,
    #[serde(rename = "quoteToken")]
    pub quote_token: Bytes,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    #[serde(rename = "baseTokenAmount")]
    pub base_token_amount: String,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    #[serde(rename = "quoteTokenAmount")]
    pub quote_token_amount: String,
    pub trader: Bytes,
    #[serde(rename = "effectiveTrader")]
    pub effective_trader: Option<Bytes>,
    #[serde(rename = "txid")]
    pub tx_id: Bytes,
    pub pool: Bytes,
    #[serde(rename = "quoteExpiry")]
    pub quote_expiry: u64,
    pub nonce: u64,
    #[serde(rename = "externalAccount")]
    pub external_account: Option<Bytes>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashflow_level() -> HashflowMarketMakerLevels {
        HashflowMarketMakerLevels {
            pair: HashflowPair {
                base_token: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                quote_token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            },
            levels: vec![
                HashflowPriceLevel { quantity: 1.0, price: 3000.0 },
                HashflowPriceLevel { quantity: 2.0, price: 2999.0 },
            ],
        }
    }

    #[test]
    fn test_market_maker_level_tvl() {
        let mm_level = hashflow_level();

        let tvl = mm_level.calculate_tvl();
        // 1.0 * 3000.0 + 2.0 * 2999.0 = 3000.0 + 5998.0 = 8998.0
        assert_eq!(tvl, 8998.0);
    }

    #[test]
    fn test_get_price() {
        let mm_level = hashflow_level();

        // Test single-level price
        let price = mm_level.get_price(1.0);
        assert_eq!(price, Some(3000.0));

        // Test larger amount spanning multiple levels
        let multi_level_price = mm_level.get_price(2.0);
        // 1.0 * 3000.0 + 1.0 * 2999.0 = 5999.0 total
        // 5999.0 / 2.0 = 2999.5
        assert_eq!(multi_level_price, Some(2999.5));

        // Test empty levels
        let empty_mm_level = HashflowMarketMakerLevels {
            pair: HashflowPair {
                base_token: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                quote_token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            },
            levels: vec![],
        };
        assert_eq!(empty_mm_level.get_price(1.0), None);
    }

    #[test]
    fn test_get_amount_out_from_levels() {
        let mm_level = hashflow_level();

        // Test exact amount that can be filled with a single level
        let (amount_out, remaining) = mm_level.get_amount_out_from_levels(1.0);
        assert_eq!(amount_out, 3000.0); // 1.0 * 3000.0
        assert_eq!(remaining, 0.0);

        // Test amount spanning multiple levels
        let (amount_out, remaining) = mm_level.get_amount_out_from_levels(2.0);
        assert_eq!(amount_out, 5999.0); // 1.0 * 3000.0 + 1.0 * 2999.0
        assert_eq!(remaining, 0.0);

        // Test amount exceeding available liquidity
        let (amount_out, remaining) = mm_level.get_amount_out_from_levels(5.0);
        assert_eq!(amount_out, 8998.0); // 1.0 * 3000.0 + 2.0 * 2999.0 = 3000.0 + 5998.0
        assert_eq!(remaining, 2.0); // 5.0 - 3.0 (total available)
    }

    #[cfg(test)]
    mod hashflow_quote_validate_tests {
        use num_bigint::BigUint;
        use tycho_common::models::protocol::GetAmountOutParams;

        use super::*;

        fn hex_to_bytes(hex: &str) -> Bytes {
            Bytes::from_str(hex).unwrap()
        }

        fn quote_data() -> HashflowQuoteData {
            HashflowQuoteData {
                base_token: hex_to_bytes("0x1111111111111111111111111111111111111111"),
                quote_token: hex_to_bytes("0x2222222222222222222222222222222222222222"),
                base_token_amount: "1000".to_string(),
                quote_token_amount: "2000".to_string(),
                trader: hex_to_bytes("0x3333333333333333333333333333333333333333"),
                effective_trader: Some(hex_to_bytes("0x6666666666666666666666666666666666666666")),
                tx_id: hex_to_bytes("0x4444444444444444444444444444444444444444"),
                pool: hex_to_bytes("0x5555555555555555555555555555555555555555"),
                quote_expiry: 123456,
                nonce: 1,
                external_account: None,
            }
        }

        fn params() -> GetAmountOutParams {
            GetAmountOutParams {
                amount_in: BigUint::from(1000u32),
                token_in: hex_to_bytes("0x1111111111111111111111111111111111111111"),
                token_out: hex_to_bytes("0x2222222222222222222222222222222222222222"),
                sender: hex_to_bytes("0x6666666666666666666666666666666666666666"),
                receiver: hex_to_bytes("0x3333333333333333333333333333333333333333"),
            }
        }

        fn quote() -> HashflowQuote {
            HashflowQuote {
                quote_data: quote_data(),
                signature: hex_to_bytes("0x7777777777777777777777777777777777777777"),
                target_contract: None,
                value: None,
            }
        }

        #[test]
        fn test_validate_success() {
            let quote = quote();
            let params = params();
            assert!(quote.validate(&params).is_ok());
        }

        #[test]
        fn test_validate_base_token_mismatch() {
            let mut quote = quote();
            quote.quote_data.base_token =
                hex_to_bytes("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Base token mismatch"));
        }

        #[test]
        fn test_validate_quote_token_mismatch() {
            let mut quote = quote();
            quote.quote_data.quote_token =
                hex_to_bytes("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Quote token mismatch"));
        }

        #[test]
        fn test_validate_trader_mismatch() {
            let mut quote = quote();
            quote.quote_data.trader = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Trader address mismatch"));
        }

        #[test]
        fn test_validate_effective_trader_mismatch() {
            let mut quote = quote();
            quote.quote_data.effective_trader =
                Some(hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"));
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Effective trader mismatch"));
        }

        #[test]
        fn test_validate_effective_trader_absent() {
            // Without an effective trader the quote defaults to the trader's scope, which only
            // passes when that is what was requested.
            let mut quote = quote();
            quote.quote_data.effective_trader = None;
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Effective trader mismatch"));
        }

        #[test]
        fn test_validate_base_token_amount_mismatch() {
            let mut quote = quote();
            quote.quote_data.base_token_amount = "9999".to_string();
            let params = params();
            let err = quote.validate(&params).unwrap_err();
            assert!(format!("{err:?}").contains("Base token amount mismatch"));
        }
    }
}
