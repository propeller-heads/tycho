use std::str::FromStr;

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::rfq::errors::RFQError;

/// PropAMM level prices are fixed-point integers scaled by 1e18: tokenOut wei per tokenIn wei,
/// multiplied by 10^18. All level sizes are denominated in tokenIn wei.
pub fn propamm_price_scale() -> BigUint {
    BigUint::from(10u32).pow(18)
}

/// Response of `GET {base_url}/v1/levels?chainId&tokenIn&tokenOut`.
///
/// Indicative price levels for selling `token_in` into `token_out`. The feed is one-directional:
/// the reverse direction is a separate levels request (and a separate protocol component).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropAmmLevelsResponse {
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    #[serde(rename = "tokenIn")]
    pub token_in: Bytes,
    #[serde(rename = "tokenOut")]
    pub token_out: Bytes,
    /// Cross-maker merged ladder, sorted best-price-first, with per-segment maker attribution
    /// and running cumulative sizes. Kept for display/debugging; simulation uses `makers`.
    #[serde(default)]
    pub merged: Vec<PropAmmMergedLevel>,
    /// Per-maker cumulative ladders. This is the source of truth for the sweep math.
    #[serde(default)]
    pub makers: Vec<PropAmmMakerLevels>,
    /// Unix timestamp (seconds) at which this snapshot was assembled.
    #[serde(rename = "asOf")]
    pub as_of: u64,
}

/// One marginal segment of the merged cross-maker ladder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropAmmMergedLevel {
    /// Maker that provides this segment.
    pub mm: Bytes,
    /// 1e18-scaled price (tokenOut wei per tokenIn wei).
    pub price: String,
    /// Marginal segment size in tokenIn wei.
    pub size: String,
    /// Cumulative size in tokenIn wei up to and including this segment.
    #[serde(rename = "cumulativeSize")]
    pub cumulative_size: String,
}

/// A single maker's ladder for the pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropAmmMakerLevels {
    /// Maker address.
    pub mm: Bytes,
    /// Inventory contract the maker settles through.
    #[serde(rename = "inventoryContract")]
    pub inventory_contract: Bytes,
    /// Cumulative levels: each level's `size` is the total tokenIn wei tradable up to and
    /// including that level's `price`.
    pub levels: Vec<PropAmmLevel>,
    /// Maker nonce as a decimal string.
    pub nonce: String,
    /// Unix timestamp (seconds) after which this ladder is no longer valid.
    #[serde(rename = "expiresAt")]
    pub expires_at: u64,
}

/// One cumulative level of a maker's ladder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropAmmLevel {
    /// Cumulative size in tokenIn wei up to and including this level.
    pub size: String,
    /// 1e18-scaled price (tokenOut wei per tokenIn wei) applied to this level's tranche.
    pub price: String,
}

/// Response of `GET {base_url}/v1/firm-quote?chainId&tokenIn&tokenOut&amountIn&receiver`.
///
/// IMPORTANT: a firm quote is a short-lived, single-use response. `valid_until` is a hard
/// deadline enforced on-chain; consumers must refetch a firm quote immediately before broadcast
/// and must never replay a stale response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropAmmFirmQuoteResponse {
    #[serde(rename = "quoteId")]
    pub quote_id: Bytes,
    /// Estimated execution gas as a decimal string.
    #[serde(rename = "gasEstimate")]
    pub gas_estimate: String,
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    /// tokenIn wei, echoes the requested amount.
    #[serde(rename = "amountIn")]
    pub amount_in: String,
    /// tokenOut wei the maker(s) commit to deliver.
    #[serde(rename = "amountOut")]
    pub amount_out: String,
    pub receiver: Bytes,
    /// Settlement calls to execute in order.
    pub calls: Vec<PropAmmCall>,
    /// Unix timestamp (seconds); hard on-chain expiry of this quote.
    #[serde(rename = "validUntil")]
    pub valid_until: u64,
}

/// One settlement call of a PropAMM firm quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropAmmCall {
    pub to: Bytes,
    pub value: String,
    pub data: Bytes,
}

impl PropAmmFirmQuoteResponse {
    /// Structural validation of a firm quote against the request parameters.
    ///
    /// Note: `valid_until` is deliberately not checked against a clock here. Freshness is the
    /// consumer's responsibility at broadcast time: refetch immediately before broadcast and
    /// never replay a stale response.
    pub fn validate(
        &self,
        params: &GetAmountOutParams,
        expected_chain_id: u64,
    ) -> Result<(), RFQError> {
        if self.chain_id != expected_chain_id {
            return Err(RFQError::FatalError(format!(
                "Chain id mismatch: expected {}, got {}",
                expected_chain_id, self.chain_id
            )));
        }
        if self.receiver != params.receiver {
            return Err(RFQError::FatalError(format!(
                "Receiver address mismatch: expected {}, got {}",
                params.receiver, self.receiver
            )));
        }
        let amount_in = params.amount_in.to_string();
        if self.amount_in != amount_in {
            return Err(RFQError::FatalError(format!(
                "Amount in mismatch: expected {}, got {}",
                amount_in, self.amount_in
            )));
        }
        if self.calls.is_empty() {
            return Err(RFQError::FatalError("Firm quote contains no settlement calls".into()));
        }
        Ok(())
    }
}

/// Parses a PropAMM decimal string field into a `BigUint`.
pub fn parse_biguint(value: &str, field: &str) -> Result<BigUint, RFQError> {
    BigUint::from_str(value)
        .map_err(|_| RFQError::ParsingError(format!("Failed to parse {field}: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn firm_quote() -> PropAmmFirmQuoteResponse {
        let json =
            std::fs::read_to_string("src/rfq/protocols/propamm/test_responses/firm_quote.json")
                .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from_str("15000000000000000000").unwrap(),
            token_in: Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap(),
            token_out: Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap(),
            sender: Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap(),
            receiver: Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap(),
        }
    }

    #[test]
    fn test_deserialize_levels_response() {
        let json = std::fs::read_to_string("src/rfq/protocols/propamm/test_responses/levels.json")
            .unwrap();
        let levels: PropAmmLevelsResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(levels.chain_id, 8453);
        assert_eq!(levels.as_of, 1784889534);
        assert_eq!(levels.makers.len(), 2);
        assert_eq!(levels.merged.len(), 3);
        assert_eq!(levels.makers[0].levels.len(), 2);
        assert_eq!(levels.makers[0].levels[0].price, "1878000000");
        assert_eq!(levels.makers[0].levels[1].size, "30000000000000000000");
        assert_eq!(levels.makers[1].nonce, "12");
        assert_eq!(levels.merged[0].cumulative_size, "10000000000000000000");
        // Round-trips through serde so state attributes can store the raw ladders as JSON.
        let reencoded = serde_json::to_string(&levels.makers).unwrap();
        let decoded: Vec<PropAmmMakerLevels> = serde_json::from_str(&reencoded).unwrap();
        assert_eq!(decoded, levels.makers);
    }

    #[test]
    fn test_deserialize_firm_quote_response() {
        let quote = firm_quote();

        assert_eq!(quote.chain_id, 8453);
        assert_eq!(quote.gas_estimate, "265000");
        assert_eq!(quote.amount_in, "15000000000000000000");
        assert_eq!(quote.amount_out, "28164999999");
        assert_eq!(quote.valid_until, 1751536030);
        assert_eq!(quote.calls.len(), 2);
        assert_eq!(quote.calls[0].value, "0");
    }

    #[test]
    fn test_validate_firm_quote_success() {
        assert!(firm_quote()
            .validate(&params(), 8453)
            .is_ok());
    }

    #[test]
    fn test_validate_firm_quote_chain_id_mismatch() {
        let err = firm_quote()
            .validate(&params(), 1)
            .unwrap_err();
        assert!(format!("{err:?}").contains("Chain id mismatch"));
    }

    #[test]
    fn test_validate_firm_quote_receiver_mismatch() {
        let mut params = params();
        params.receiver = Bytes::from_str("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();
        let err = firm_quote()
            .validate(&params, 8453)
            .unwrap_err();
        assert!(format!("{err:?}").contains("Receiver address mismatch"));
    }

    #[test]
    fn test_validate_firm_quote_amount_in_mismatch() {
        let mut params = params();
        params.amount_in = BigUint::from(1u32);
        let err = firm_quote()
            .validate(&params, 8453)
            .unwrap_err();
        assert!(format!("{err:?}").contains("Amount in mismatch"));
    }

    #[test]
    fn test_validate_firm_quote_no_calls() {
        let mut quote = firm_quote();
        quote.calls.clear();
        let err = quote
            .validate(&params(), 8453)
            .unwrap_err();
        assert!(format!("{err:?}").contains("no settlement calls"));
    }

    #[test]
    fn test_parse_biguint() {
        assert_eq!(parse_biguint("1878000000", "price").unwrap(), BigUint::from(1878000000u64));
        assert!(parse_biguint("not a number", "price").is_err());
    }
}
