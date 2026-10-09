use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};
use serde_with::{serde_as, skip_serializing_none};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::{
    book::{levels::Levels, wire::SkipInvalidEntries},
    rfq::errors::RFQError,
    serde_helpers::evm_address,
};

/// The error Hashflow reports on a rejected request, sent as an object alongside HTTP 200.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowError {
    pub code: u64,
    pub message: String,
}

impl HashflowError {
    /// Whether this is Hashflow stating it does not price the chain it was asked about.
    ///
    /// The venue publishes no catalogue of these codes, and the one it answers here with covers
    /// an unknown market maker and an unknown chain type too, so what it refused is in the
    /// message alone.
    pub fn refuses_the_chain(&self) -> bool {
        self.message
            .starts_with("Invalid chainId")
    }
}

/// The envelope Hashflow answers a request it would not serve with, on either book endpoint.
/// A body it does serve carries no `error` at all, so parsing one is what tells the two apart.
#[derive(Debug, Clone, Deserialize)]
pub struct HashflowFailure {
    pub error: HashflowError,
}

impl fmt::Display for HashflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashflowPriceLevelsResponse {
    pub status: String, // "success" or "fail"
    #[serde_as(deserialize_as = "Option<SkipInvalidEntries>")]
    pub levels: Option<HashMap<String, Vec<HashflowMarketMakerLevels>>>,
    pub error: Option<HashflowError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HashflowMarketMakerLevels {
    pub pair: HashflowPair,
    #[serde(with = "hashflow_levels")]
    pub levels: Levels,
}

/// Hashflow's wire form of a ladder: `{"q": "<quantity>", "p": "<price>"}` objects with decimal
/// strings.
mod hashflow_levels {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{serde_as, DisplayFromStr};

    use crate::book::levels::{Levels, PriceLevel};

    #[serde_as]
    #[derive(Serialize, Deserialize)]
    struct WireLevel {
        #[serde_as(as = "DisplayFromStr")]
        q: f64,
        #[serde_as(as = "DisplayFromStr")]
        p: f64,
    }

    pub fn serialize<S: Serializer>(levels: &Levels, serializer: S) -> Result<S::Ok, S::Error> {
        levels
            .iter()
            .map(|level| WireLevel { q: level.quantity, p: level.price })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Levels, D::Error> {
        let levels = Vec::<WireLevel>::deserialize(deserializer)?
            .into_iter()
            .map(|level| PriceLevel { price: level.p, quantity: level.q })
            .collect();
        Levels::new(levels).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowPair {
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub base_token: Bytes,
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub quote_token: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowMarketMakersResponse {
    pub market_makers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowQuoteRequest {
    pub source: String,
    pub base_chain: HashflowChain,
    pub quote_chain: HashflowChain,
    pub rfqs: Vec<HashflowRFQ>,
    pub calldata: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowChain {
    chain_type: String,
    chain_id: u64,
}

impl HashflowChain {
    /// The chain as Hashflow's quote requests name it: an EVM chain id.
    pub fn evm(chain_id: u64) -> Self {
        HashflowChain { chain_type: "evm".to_string(), chain_id }
    }
}

/// Optional fields are left out of the request rather than sent as `null`.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowRFQ {
    pub base_token: String,
    pub quote_token: String,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    pub base_token_amount: Option<String>,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    pub quote_token_amount: Option<String>,
    pub trader: String,
    pub effective_trader: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowQuoteResponse {
    pub status: String,
    pub error: Option<HashflowError>,
    rfq_id: String,
    internal_rfq_ids: Option<Vec<String>>,
    pub quotes: Option<Vec<HashflowQuote>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashflowQuote {
    pub quote_data: HashflowQuoteData,
    pub signature: Bytes,
    pub target_contract: Option<Bytes>,
    pub value: Option<String>,
}

impl HashflowQuote {
    pub fn validate(
        &self,
        params: &GetAmountOutParams,
        expected_effective_trader: &Bytes,
    ) -> Result<(), RFQError> {
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
        if &effective_trader != expected_effective_trader {
            return Err(RFQError::FatalError(format!(
                "Effective trader mismatch: expected {expected_effective_trader}, got {effective_trader}"
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
#[serde(rename_all = "camelCase")]
pub struct HashflowQuoteData {
    pub base_token: Bytes,
    pub quote_token: Bytes,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    pub base_token_amount: String,
    // Decimal amount (e.g. "1000000" for 1 USDT)
    pub quote_token_amount: String,
    pub trader: Bytes,
    pub effective_trader: Option<Bytes>,
    #[serde(rename = "txid")]
    pub tx_id: Bytes,
    pub pool: Bytes,
    pub quote_expiry: u64,
    pub nonce: u64,
    pub external_account: Option<Bytes>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::book::levels::PriceLevel;

    /// A maker quoting a ladder Hashflow got wrong goes quiet for the poll; the makers that
    /// priced their pairs correctly still publish.
    #[test]
    fn a_maker_with_a_malformed_ladder_does_not_cost_the_others() {
        let json = r#"{"status":"success","levels":{
                "mm1":[{"pair":{"baseToken":"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2","quoteToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"levels":[{"q":"1","p":"0"}]}],
                "mm2":[{"pair":{"baseToken":"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2","quoteToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"levels":[{"q":"1","p":"3000"}]}]
            }}"#;

        let response: HashflowPriceLevelsResponse = serde_json::from_str(json).unwrap();

        let levels = response.levels.unwrap();
        assert!(levels["mm1"].is_empty());
        assert_eq!(levels["mm2"].len(), 1);
    }

    /// Nothing readable in the response is a parsing failure, not an empty book: a published
    /// snapshot is complete, so an empty one withdraws every pair the venue had.
    #[test]
    fn a_response_with_no_readable_maker_is_an_error() {
        let json = r#"{"status":"success","levels":{"mm1":[{"pair":{"baseToken":"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2","quoteToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"levels":[{"q":"1","p":"0"}]}]}}"#;

        assert!(serde_json::from_str::<HashflowPriceLevelsResponse>(json).is_err());
    }

    /// Hashflow answers a rejected RFQ with HTTP 200 and an `error` object, so the quote response
    /// has to deserialize that shape rather than only the successful one.
    #[test]
    fn test_deserialize_rejected_quote_response() {
        let body = r#"{"status":"fail","rfqId":"0x2250000000000000000000000000000f",
            "error":{"code":82,"message":"No maker supports this request"}}"#;

        let response: HashflowQuoteResponse = serde_json::from_str(body).unwrap();

        assert_eq!(response.status, "fail");
        assert_eq!(response.error.unwrap().to_string(), "No maker supports this request (code 82)");
    }

    #[test]
    fn levels_round_trip_through_the_wire_form() {
        let json = r#"{"pair":{"baseToken":"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2","quoteToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"levels":[{"q":"1","p":"3000"},{"q":"2.5","p":"2999"}]}"#;

        let mm_levels: HashflowMarketMakerLevels = serde_json::from_str(json).unwrap();

        assert_eq!(
            &*mm_levels.levels,
            &[
                PriceLevel { quantity: 1.0, price: 3000.0 },
                PriceLevel { quantity: 2.5, price: 2999.0 },
            ]
        );
        assert_eq!(
            mm_levels.pair.base_token,
            Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()
        );
        let json = serde_json::to_string(&mm_levels).unwrap();
        assert!(json.contains(r#"{"q":"1","p":"3000"}"#), "{json}");

        // The adapter hands the decoded levels to `Levels::new`, so its validation applies.
        let invalid = json.replace(r#""p":"3000""#, r#""p":"0""#);
        assert!(serde_json::from_str::<HashflowMarketMakerLevels>(&invalid).is_err());
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

        fn expected_effective_trader() -> Bytes {
            hex_to_bytes("0x6666666666666666666666666666666666666666")
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
            assert!(quote
                .validate(&params, &expected_effective_trader())
                .is_ok());
        }

        #[test]
        fn test_validate_base_token_mismatch() {
            let mut quote = quote();
            quote.quote_data.base_token =
                hex_to_bytes("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
            let params = params();
            let err = quote
                .validate(&params, &expected_effective_trader())
                .unwrap_err();
            assert!(format!("{err:?}").contains("Base token mismatch"));
        }

        #[test]
        fn test_validate_quote_token_mismatch() {
            let mut quote = quote();
            quote.quote_data.quote_token =
                hex_to_bytes("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
            let params = params();
            let err = quote
                .validate(&params, &expected_effective_trader())
                .unwrap_err();
            assert!(format!("{err:?}").contains("Quote token mismatch"));
        }

        #[test]
        fn test_validate_trader_mismatch() {
            let mut quote = quote();
            quote.quote_data.trader = hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            let params = params();
            let err = quote
                .validate(&params, &expected_effective_trader())
                .unwrap_err();
            assert!(format!("{err:?}").contains("Trader address mismatch"));
        }

        #[test]
        fn test_validate_effective_trader_mismatch() {
            let mut quote = quote();
            quote.quote_data.effective_trader =
                Some(hex_to_bytes("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"));
            let params = params();
            let err = quote
                .validate(&params, &expected_effective_trader())
                .unwrap_err();
            assert!(format!("{err:?}").contains("Effective trader mismatch"));
        }

        #[test]
        fn test_validate_effective_trader_absent() {
            // An answer without an effectiveTrader is not tied to the address we requested
            // (Hashflow ties it to the trader instead), so validation must reject it.
            let mut quote = quote();
            quote.quote_data.effective_trader = None;
            let params = params();
            let err = quote
                .validate(&params, &expected_effective_trader())
                .unwrap_err();
            assert!(format!("{err:?}").contains("Effective trader mismatch"));
        }

        #[test]
        fn test_validate_base_token_amount_mismatch() {
            let mut quote = quote();
            quote.quote_data.base_token_amount = "9999".to_string();
            let params = params();
            let err = quote
                .validate(&params, &expected_effective_trader())
                .unwrap_err();
            assert!(format!("{err:?}").contains("Base token amount mismatch"));
        }
    }
}
