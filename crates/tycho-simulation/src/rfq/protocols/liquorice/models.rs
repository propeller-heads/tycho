use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::rfq::errors::RFQError;

/// Response from GET /price-levels?chainId=<id>
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoricePriceLevelsResponse {
    pub prices: HashMap<String, Vec<LiquoriceTokenPairPrice>>,
}

/// A market maker's pricing for a token pair with price levels
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiquoriceTokenPairPrice {
    #[serde(rename = "baseToken", deserialize_with = "deserialize_string_to_checksummed_bytes")]
    pub base_token: Bytes,
    #[serde(rename = "quoteToken", deserialize_with = "deserialize_string_to_checksummed_bytes")]
    pub quote_token: Bytes,
    /// Levels as [price, quantity] string pairs from the API, deserialized into
    /// LiquoricePriceLevel
    #[serde(
        deserialize_with = "deserialize_string_pair_to_price_levels",
        serialize_with = "serialize_price_levels_to_string_pairs"
    )]
    pub levels: Vec<LiquoricePriceLevel>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<u64>,
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

fn deserialize_string_pair_to_price_levels<'de, D>(
    deserializer: D,
) -> Result<Vec<LiquoricePriceLevel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let pairs: Vec<Vec<String>> = Vec::deserialize(deserializer)?;
    pairs
        .into_iter()
        .filter_map(|pair| {
            if pair.len() == 2 {
                let price = pair[0].parse::<f64>().ok()?;
                let quantity = pair[1].parse::<f64>().ok()?;
                Some(Ok(LiquoricePriceLevel { price, quantity }))
            } else {
                None
            }
        })
        .collect()
}

fn serialize_price_levels_to_string_pairs<S>(
    levels: &[LiquoricePriceLevel],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(levels.len()))?;
    for level in levels {
        seq.serialize_element(&[level.price.to_string(), level.quantity.to_string()])?;
    }
    seq.end()
}

/// Price level with price and quantity
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiquoricePriceLevel {
    #[serde(
        rename = "q",
        deserialize_with = "deserialize_string_to_f64",
        serialize_with = "serialize_f64_to_string"
    )]
    pub quantity: f64,
    #[serde(
        rename = "p",
        deserialize_with = "deserialize_string_to_f64",
        serialize_with = "serialize_f64_to_string"
    )]
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

impl LiquoriceTokenPairPrice {
    pub fn calculate_tvl(&self) -> f64 {
        self.levels
            .iter()
            .map(|level| level.quantity * level.price)
            .sum()
    }

    pub fn get_price(&self) -> Option<f64> {
        if self.levels.is_empty() {
            return None;
        }
        let total_quantity: f64 = self
            .levels
            .iter()
            .map(|l| l.quantity)
            .sum();
        let total_value: f64 = self
            .levels
            .iter()
            .map(|l| l.quantity * l.price)
            .sum();
        Some(total_value / total_quantity)
    }

    pub fn get_price_for_amount(&self, base_token_amount: f64) -> Option<f64> {
        if self.levels.is_empty() {
            return None;
        }

        let (total_quote_token, remaining_base_token) =
            self.get_amount_out_from_levels(base_token_amount);

        Some(total_quote_token / (base_token_amount - remaining_base_token))
    }

    pub fn get_amount_out_from_levels(&self, amount_in: f64) -> (f64, f64) {
        let mut remaining_amount_in = amount_in;
        let mut total_amount_out = 0.0;

        for level in &self.levels {
            if remaining_amount_in <= 0.0 {
                break;
            }

            let amount_to_fill = remaining_amount_in.min(level.quantity);
            total_amount_out += amount_to_fill * level.price;
            remaining_amount_in -= amount_to_fill;
        }

        (total_amount_out, remaining_amount_in)
    }
}

/// RFQ request body for POST /rfq
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoriceQuoteRequest {
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    #[serde(rename = "rfqId")]
    pub rfq_id: String,
    pub expiry: u64,
    #[serde(rename = "baseToken")]
    pub base_token: String,
    #[serde(rename = "quoteToken")]
    pub quote_token: String,
    pub trader: String,
    #[serde(rename = "effectiveTrader", skip_serializing_if = "Option::is_none")]
    pub effective_trader: Option<String>,
    #[serde(rename = "baseTokenAmount", skip_serializing_if = "Option::is_none")]
    pub base_token_amount: Option<String>,
    #[serde(rename = "quoteTokenAmount", skip_serializing_if = "Option::is_none")]
    pub quote_token_amount: Option<String>,
}

/// RFQ response from POST /rfq
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoriceQuoteResponse {
    #[serde(rename = "rfqId")]
    pub rfq_id: String,
    #[serde(rename = "liquidityAvailable")]
    pub liquidity_available: bool,
    pub levels: Vec<LiquoriceQuoteLevel>,
}

/// Individual quote level from RFQ response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoriceQuoteLevel {
    #[serde(rename = "makerRfqId")]
    pub maker_rfq_id: String,
    pub maker: String,
    pub expiry: u64,
    pub tx: LiquoriceTx,
    #[serde(rename = "baseToken")]
    pub base_token: String,
    #[serde(rename = "quoteToken")]
    pub quote_token: String,
    #[serde(rename = "baseTokenAmount")]
    pub base_token_amount: String,
    #[serde(rename = "quoteTokenAmount")]
    pub quote_token_amount: String,
    #[serde(rename = "partialFill")]
    pub partial_fill: Option<LiquoricePartialFill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoriceTx {
    pub to: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoricePartialFill {
    pub offset: u32,
    #[serde(rename = "minBaseTokenAmount")]
    pub min_base_token_amount: String,
}

impl LiquoriceQuoteLevel {
    pub fn validate(&self, params: &GetAmountOutParams) -> Result<(), RFQError> {
        let base_token = Bytes::from_str(&self.base_token)
            .map_err(|e| RFQError::ParsingError(format!("Invalid base token address: {e}")))?;
        let quote_token = Bytes::from_str(&self.quote_token)
            .map_err(|e| RFQError::ParsingError(format!("Invalid quote token address: {e}")))?;

        if base_token != params.token_in {
            return Err(RFQError::FatalError(format!(
                "Base token mismatch: expected {}, got {}",
                params.token_in, self.base_token
            )));
        }
        if quote_token != params.token_out {
            return Err(RFQError::FatalError(format!(
                "Quote token mismatch: expected {}, got {}",
                params.token_out, self.quote_token
            )));
        }
        if self.base_token_amount != params.amount_in.to_string() {
            return Err(RFQError::FatalError(format!(
                "Base token amount mismatch: expected {}, got {}",
                params.amount_in, self.base_token_amount
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liquorice_mm_levels() -> LiquoriceTokenPairPrice {
        LiquoriceTokenPairPrice {
            base_token: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            quote_token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            levels: vec![
                LiquoricePriceLevel { quantity: 1.0, price: 3000.0 },
                LiquoricePriceLevel { quantity: 2.0, price: 2999.0 },
            ],
            updated_at: Some(1234567890),
        }
    }

    #[test]
    fn test_get_price() {
        let levels = liquorice_mm_levels();

        let price = levels.get_price();
        assert!((price.unwrap() - 8998.0 / 3.0).abs() < 1e-10);

        let empty_levels = LiquoriceTokenPairPrice {
            base_token: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            quote_token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            levels: vec![],
            updated_at: None,
        };
        assert_eq!(empty_levels.get_price(), None);
    }

    #[test]
    fn test_get_price_for_amount() {
        let levels = liquorice_mm_levels();

        let price = levels.get_price_for_amount(1.0);
        assert_eq!(price, Some(3000.0));

        let multi_level_price = levels.get_price_for_amount(2.0);
        assert_eq!(multi_level_price, Some(2999.5));

        let empty_levels = LiquoriceTokenPairPrice {
            base_token: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            quote_token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            levels: vec![],
            updated_at: None,
        };
        assert_eq!(empty_levels.get_price_for_amount(1.0), None);
    }

    #[test]
    fn test_get_amount_out_from_levels() {
        let levels = liquorice_mm_levels();

        let (amount_out, remaining) = levels.get_amount_out_from_levels(1.0);
        assert_eq!(amount_out, 3000.0);
        assert_eq!(remaining, 0.0);

        let (amount_out, remaining) = levels.get_amount_out_from_levels(2.0);
        assert_eq!(amount_out, 5999.0);
        assert_eq!(remaining, 0.0);

        let (amount_out, remaining) = levels.get_amount_out_from_levels(5.0);
        assert_eq!(amount_out, 8998.0);
        assert_eq!(remaining, 2.0);
    }

    #[test]
    fn test_deserialize_price_levels_response() {
        let json = r#"{"prices":{"maker_0":[{"baseToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","quoteToken":"0xdac17f958d2ee523a2206206994597c13d831ec7","levels":[["1.00115000","100.000000000000000000"],["1.00125000","500.000000000000000000"]],"updatedAt":1769707860675}]}}"#;
        let response: LiquoricePriceLevelsResponse = serde_json::from_str(json).unwrap();
        let mm_levels = &response.prices["maker_0"];
        assert_eq!(mm_levels.len(), 1);
        assert_eq!(mm_levels[0].levels.len(), 2);
        assert_eq!(mm_levels[0].levels[0].price, 1.00115);
        assert_eq!(mm_levels[0].levels[0].quantity, 100.0);
        assert_eq!(mm_levels[0].levels[1].price, 1.00125);
        assert_eq!(mm_levels[0].levels[1].quantity, 500.0);
    }

    #[cfg(test)]
    mod liquorice_quote_validate_tests {
        use num_bigint::BigUint;
        use tycho_common::models::protocol::GetAmountOutParams;

        use super::*;

        fn hex_to_bytes(hex: &str) -> Bytes {
            Bytes::from_str(hex).unwrap()
        }

        fn quote_level() -> LiquoriceQuoteLevel {
            LiquoriceQuoteLevel {
                maker_rfq_id: "maker-rfq-1".to_string(),
                maker: "test-maker".to_string(),
                expiry: 123456,
                tx: LiquoriceTx {
                    to: "0x5555555555555555555555555555555555555555".to_string(),
                    data: "0xdeadbeef".to_string(),
                },
                base_token: "0x1111111111111111111111111111111111111111".to_string(),
                quote_token: "0x2222222222222222222222222222222222222222".to_string(),
                base_token_amount: "1000".to_string(),
                quote_token_amount: "2000".to_string(),
                partial_fill: None,
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

        #[test]
        fn test_validate_success() {
            let level = quote_level();
            let params = params();
            assert!(level.validate(&params).is_ok());
        }

        #[test]
        fn test_validate_rejects_mismatched_fields() {
            let params = params();

            let mut level = quote_level();
            level.base_token = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
            assert!(matches!(level.validate(&params), Err(RFQError::FatalError(_))));

            let mut level = quote_level();
            level.quote_token = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
            assert!(matches!(level.validate(&params), Err(RFQError::FatalError(_))));

            let mut level = quote_level();
            level.base_token_amount = "9999".to_string();
            assert!(matches!(level.validate(&params), Err(RFQError::FatalError(_))));
        }
    }
}
