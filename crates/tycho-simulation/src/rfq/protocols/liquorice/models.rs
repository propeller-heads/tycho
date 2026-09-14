use std::{collections::HashMap, str::FromStr};

use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::{book::levels::Levels, rfq::errors::RFQError, serde_helpers::checksummed_address};

/// Response from GET /price-levels?chainId=<id>
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquoricePriceLevelsResponse {
    pub prices: HashMap<String, Vec<LiquoriceTokenPairPrice>>,
}

/// A market maker's pricing for a token pair with price levels
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiquoriceTokenPairPrice {
    #[serde(rename = "baseToken", deserialize_with = "checksummed_address::deserialize")]
    pub base_token: Bytes,
    #[serde(rename = "quoteToken", deserialize_with = "checksummed_address::deserialize")]
    pub quote_token: Bytes,
    #[serde(with = "liquorice_levels")]
    pub levels: Levels,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<u64>,
}

/// Liquorice's wire form of a ladder: `["<price>", "<quantity>"]` decimal-string pairs.
mod liquorice_levels {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{serde_as, DisplayFromStr};

    use crate::book::levels::{Levels, PriceLevel};

    #[serde_as]
    #[derive(Serialize, Deserialize)]
    struct WireLevels(#[serde_as(as = "Vec<(DisplayFromStr, DisplayFromStr)>")] Vec<(f64, f64)>);

    pub fn serialize<S: Serializer>(levels: &Levels, serializer: S) -> Result<S::Ok, S::Error> {
        WireLevels(
            levels
                .iter()
                .map(|level| (level.price, level.quantity))
                .collect(),
        )
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Levels, D::Error> {
        let levels = WireLevels::deserialize(deserializer)?
            .0
            .into_iter()
            .map(|(price, quantity)| PriceLevel { price, quantity })
            .collect();
        Levels::new(levels).map_err(serde::de::Error::custom)
    }
}

impl LiquoriceTokenPairPrice {
    /// The quantity-weighted average price over the whole ladder; `None` without any level.
    pub fn average_price(&self) -> Option<f64> {
        let (total_quantity, total_value) = self.levels.totals();
        (total_quantity > 0.0).then(|| total_value / total_quantity)
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

        let json = serde_json::to_string(&mm_levels[0]).unwrap();
        assert!(json.contains(r#"[["1.00115","100"],["1.00125","500"]]"#), "{json}");
    }

    #[test]
    fn rejects_malformed_wire_levels() {
        for levels in [r#"[["1.0"]]"#, r#"[["abc","1"]]"#, r#"[["0","1"]]"#] {
            let json = format!(
                r#"{{"baseToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","quoteToken":"0xdac17f958d2ee523a2206206994597c13d831ec7","levels":{levels}}}"#
            );
            assert!(
                serde_json::from_str::<LiquoriceTokenPairPrice>(&json).is_err(),
                "{levels} should be rejected"
            );
        }
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
