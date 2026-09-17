use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::rfq::errors::RFQError;

/// One pair entry in Euclid's levels frame. Prices are quote-per-base, sizes
/// are base units (human), best level first. The gateway serializes level
/// numbers as strings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EuclidPairLevels {
    pub base_symbol: String,
    pub quote_symbol: String,
    pub base_address: String,
    pub quote_address: String,
    pub bids: Vec<(String, String)>,
    pub asks: Vec<(String, String)>,
    pub timestamp: u64,
}

/// Full-state levels frame — pushed over the WebSocket and served by the HTTP
/// snapshot endpoint. Snapshots are absolute (no deltas): every frame carries
/// the complete current grid, and pairs missing from a frame are gone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EuclidLevelsFrame {
    pub chain_id: u64,
    pub timestamp: u64,
    pub pairs: Vec<EuclidPairLevels>,
}

/// Parsed price levels for one pair, mirroring the Bebop level-walk math.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EuclidPriceData {
    pub base: Vec<u8>,
    pub quote: Vec<u8>,
    pub last_update_ts: u64,
    /// (price quote-per-base, size in base), best first.
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

impl EuclidPriceData {
    pub fn from_pair(
        pair: &EuclidPairLevels,
        base: Vec<u8>,
        quote: Vec<u8>,
    ) -> Result<Self, RFQError> {
        let parse = |levels: &Vec<(String, String)>| -> Result<Vec<(f64, f64)>, RFQError> {
            levels
                .iter()
                .map(|(p, s)| {
                    let price = p
                        .parse::<f64>()
                        .map_err(|_| RFQError::ParsingError(format!("Invalid level price: {p}")))?;
                    let size = s
                        .parse::<f64>()
                        .map_err(|_| RFQError::ParsingError(format!("Invalid level size: {s}")))?;
                    Ok((price, size))
                })
                .collect()
        };
        Ok(Self {
            base,
            quote,
            last_update_ts: pair.timestamp,
            bids: parse(&pair.bids)?,
            asks: parse(&pair.asks)?,
        })
    }

    /// Sum of the bid side in quote units — the conservative TVL proxy used
    /// for the stream's threshold filter (levels are published stable-quoted).
    pub fn quote_tvl(&self) -> f64 {
        self.bids
            .iter()
            .map(|(price, size)| price * size)
            .sum()
    }

    /// Walks price levels consuming liquidity level by level. Returns
    /// (amount_out, remaining_amount_in); no error on partial consumption.
    pub fn get_amount_out_from_levels(amount_in: f64, price_levels: &[(f64, f64)]) -> (f64, f64) {
        let mut remaining_amount_in = amount_in;
        let mut amount_out = 0.0;
        for (price, tokens_available) in price_levels.iter() {
            if remaining_amount_in <= 0.0 {
                break;
            }
            let tradable = remaining_amount_in.min(*tokens_available);
            amount_out += tradable * price;
            remaining_amount_in -= tradable;
        }
        (amount_out, remaining_amount_in)
    }

    /// Converts (quote_per_base, base_size) levels into (base_per_quote,
    /// quote_size) so the same walk covers quote-in trades.
    pub fn invert_price_levels(price_levels: &[(f64, f64)]) -> Vec<(f64, f64)> {
        price_levels
            .iter()
            .filter(|(price, _)| *price > 0.0)
            .map(|(price, base_size)| (1.0 / price, base_size * price))
            .collect()
    }
}

/// Firm-quote response from `POST <base>/firm`. `calldata` is a ready-to-send
/// `fillOrderRFQTo` call embedding the maker-signed order; `tx_to` is the
/// settlement contract it targets; `partial_fill_offset` is the word index of
/// the fill amount the executor may patch downward.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EuclidFirmResponse {
    Success(EuclidFirmQuote),
    Error(EuclidFirmError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EuclidFirmQuote {
    pub tx_to: String,
    pub calldata: String,
    pub partial_fill_offset: u64,
    pub amount_in: String,
    pub amount_out: String,
    pub expiry: u64,
    pub order_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EuclidFirmError {
    pub error: String,
}

impl EuclidFirmQuote {
    pub fn validate(&self, params: &GetAmountOutParams) -> Result<(), RFQError> {
        let amount_in = self
            .amount_in
            .parse::<num_bigint::BigUint>()
            .map_err(|_| {
                RFQError::ParsingError(format!("Invalid amount_in: {}", self.amount_in))
            })?;
        if amount_in != params.amount_in {
            return Err(RFQError::InvalidInput(format!(
                "Firm quote amount_in {} does not match requested {}",
                amount_in, params.amount_in
            )));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_err(|_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".into()))?
            .as_secs();
        if self.expiry <= now {
            return Err(RFQError::InvalidInput(format!(
                "Firm quote already expired: expiry={} now={now}",
                self.expiry
            )));
        }
        let calldata = self
            .calldata
            .strip_prefix("0x")
            .unwrap_or(&self.calldata);
        // The executor patches the 32-byte word at byte 4 + offset*32 — the
        // calldata must actually contain it.
        let min_len_bytes = 4 + (self.partial_fill_offset as usize + 1) * 32;
        if calldata.len() < min_len_bytes * 2 {
            return Err(RFQError::InvalidInput(format!(
                "Calldata too short ({} bytes) for partial_fill_offset {}",
                calldata.len() / 2,
                self.partial_fill_offset
            )));
        }
        Ok(())
    }

    pub fn tx_to_bytes(&self) -> Result<Bytes, RFQError> {
        parse_hex(&self.tx_to, 20)
    }

    pub fn calldata_bytes(&self) -> Result<Bytes, RFQError> {
        parse_hex(&self.calldata, 0)
    }
}

fn parse_hex(value: &str, expected_len: usize) -> Result<Bytes, RFQError> {
    let stripped = value
        .strip_prefix("0x")
        .unwrap_or(value);
    let decoded = hex::decode(stripped)
        .map_err(|_| RFQError::ParsingError(format!("Invalid hex value: {value}")))?;
    if expected_len > 0 && decoded.len() != expected_len {
        return Err(RFQError::ParsingError(format!(
            "Expected {expected_len} bytes, got {} in {value}",
            decoded.len()
        )));
    }
    Ok(Bytes::from(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_levels_frame() {
        let json = r#"{
            "chain_id": 1,
            "timestamp": 1757500000000,
            "pairs": [{
                "base_symbol": "ETH",
                "quote_symbol": "USDC",
                "base_address": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "quote_address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "bids": [["3499.50000000", "0.00057151"], ["3499.50000000", "0.49442849"]],
                "asks": [["3501.55000000", "0.00057118"], ["3501.55000000", "0.49442882"]],
                "timestamp": 1757500000000
            }]
        }"#;
        let frame: EuclidLevelsFrame = serde_json::from_str(json).expect("parse frame");
        assert_eq!(frame.chain_id, 1);
        assert_eq!(frame.pairs.len(), 1);
        let data = EuclidPriceData::from_pair(&frame.pairs[0], vec![0u8; 20], vec![1u8; 20])
            .expect("parse levels");
        assert_eq!(data.bids.len(), 2);
        assert!((data.bids[0].0 - 3499.5).abs() < 1e-9);
    }

    #[test]
    fn test_level_walk() {
        let levels = vec![(3000.0, 2.0), (2900.0, 2.5)];
        let (out, rem) = EuclidPriceData::get_amount_out_from_levels(3.0, &levels);
        assert!((out - 8900.0).abs() < 1e-9);
        assert!(rem.abs() < 1e-12);

        let inverted = EuclidPriceData::invert_price_levels(&levels);
        // (1/3000, 6000), (1/2900, 7250)
        assert!((inverted[0].1 - 6000.0).abs() < 1e-9);
        let (out, rem) = EuclidPriceData::get_amount_out_from_levels(7000.0, &inverted);
        assert!((out - (2.0 + 1000.0 / 2900.0)).abs() < 1e-9);
        assert!(rem.abs() < 1e-12);
    }

    #[test]
    fn test_firm_response_parsing_and_validation() {
        let json = r#"{
            "tx_to": "0x1111111254eeb25477b68fb85ed929f73a960582",
            "calldata": "0x5a0998430000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000500000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000007000000000000000000000000000000000000000000000000000000000000012000000000000000000000000000000000000000000000000000000000000f4240000000000000000000000000000000000000000000000000000000000000000900000000000000000000000000000000000000000000000000000000000000411111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111100000000000000000000000000000000000000000000000000000000000000",
            "partial_fill_offset": 8,
            "amount_in": "1000000",
            "amount_out": "3496000000",
            "expiry": 99999999999,
            "order_hash": "0xabcdef0000000000000000000000000000000000000000000000000000000000"
        }"#;
        let resp: EuclidFirmResponse = serde_json::from_str(json).expect("parse response");
        let quote = match resp {
            EuclidFirmResponse::Success(q) => q,
            EuclidFirmResponse::Error(e) => panic!("unexpected error: {e:?}"),
        };
        let params = GetAmountOutParams {
            amount_in: 1000000u64.into(),
            token_in: Bytes::from(vec![0u8; 20]),
            token_out: Bytes::from(vec![1u8; 20]),
            sender: Bytes::from(vec![2u8; 20]),
            receiver: Bytes::from(vec![2u8; 20]),
        };
        quote
            .validate(&params)
            .expect("valid quote");
        assert_eq!(quote.tx_to_bytes().unwrap().len(), 20);

        // Mismatched amount rejected
        let bad = GetAmountOutParams { amount_in: 42u64.into(), ..params };
        assert!(quote.validate(&bad).is_err());
    }

    #[test]
    fn test_validate_rejects_expired_quote() {
        let mut quote = sample_quote("1000000");
        quote.expiry = 1; // long past
        assert!(quote
            .validate(&sample_params("1000000"))
            .is_err());
    }

    #[test]
    fn test_validate_rejects_offset_beyond_calldata() {
        let mut quote = sample_quote("1000000");
        quote.partial_fill_offset = 200; // patch position far past calldata end
        assert!(quote
            .validate(&sample_params("1000000"))
            .is_err());
    }

    #[test]
    fn test_tx_to_rejects_bad_hex_and_wrong_length() {
        let mut quote = sample_quote("1");
        quote.tx_to = "0xzznothex".to_string();
        assert!(quote.tx_to_bytes().is_err());
        quote.tx_to = "0x1234".to_string(); // 2 bytes, not 20
        assert!(quote.tx_to_bytes().is_err());
    }

    #[test]
    fn test_from_pair_rejects_malformed_level_numbers() {
        let pair = EuclidPairLevels {
            base_symbol: "ETH".to_string(),
            quote_symbol: "USDC".to_string(),
            base_address: "0x".to_string(),
            quote_address: "0x".to_string(),
            bids: vec![("not-a-number".to_string(), "1.0".to_string())],
            asks: vec![],
            timestamp: 0,
        };
        assert!(EuclidPriceData::from_pair(&pair, vec![0u8; 20], vec![1u8; 20]).is_err());
    }

    #[test]
    fn test_quote_tvl_sums_bid_side_in_quote_units() {
        let data = EuclidPriceData {
            base: vec![],
            quote: vec![],
            last_update_ts: 0,
            bids: vec![(3000.0, 2.0), (2900.0, 1.0)],
            asks: vec![(9999.0, 9999.0)], // asks must not count
        };
        assert!((data.quote_tvl() - 8900.0).abs() < 1e-9);
    }

    #[test]
    fn test_invert_price_levels_drops_zero_prices() {
        let inverted = EuclidPriceData::invert_price_levels(&[(0.0, 5.0), (2.0, 3.0)]);
        assert_eq!(inverted.len(), 1);
        assert!((inverted[0].0 - 0.5).abs() < 1e-12);
        assert!((inverted[0].1 - 6.0).abs() < 1e-12);
    }

    #[test]
    fn test_level_walk_partial_consumption() {
        let levels = vec![(3000.0, 1.0)];
        let (out, rem) = EuclidPriceData::get_amount_out_from_levels(2.5, &levels);
        assert!((out - 3000.0).abs() < 1e-9);
        assert!((rem - 1.5).abs() < 1e-9);
    }

    fn sample_params(amount: &str) -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: amount.parse().unwrap(),
            token_in: Bytes::from(vec![0u8; 20]),
            token_out: Bytes::from(vec![1u8; 20]),
            sender: Bytes::from(vec![2u8; 20]),
            receiver: Bytes::from(vec![2u8; 20]),
        }
    }

    fn sample_quote(amount_in: &str) -> EuclidFirmQuote {
        EuclidFirmQuote {
            tx_to: "0x1111111254eeb25477b68fb85ed929f73a960582".to_string(),
            calldata: format!("0x5a099843{}", "00".repeat(9 * 32)),
            partial_fill_offset: 8,
            amount_in: amount_in.to_string(),
            amount_out: "1".to_string(),
            expiry: u64::MAX,
            order_hash: "0xabc".to_string(),
        }
    }

    #[test]
    fn test_firm_error_parsing() {
        let resp: EuclidFirmResponse =
            serde_json::from_str(r#"{"error": "insufficient_liquidity"}"#).expect("parse error");
        assert!(
            matches!(resp, EuclidFirmResponse::Error(e) if e.error == "insufficient_liquidity")
        );
    }
}
