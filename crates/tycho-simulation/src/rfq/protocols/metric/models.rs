use std::str::FromStr;

use alloy::primitives::Address;
use num_bigint::{BigInt, BigUint};
use num_traits::{Signed, ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::rfq::errors::RFQError;

const Q64_FLOAT: f64 = 18_446_744_073_709_551_616.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricMetadata {
    pub pair: String,
    #[serde(rename = "poolAddress", deserialize_with = "deserialize_address")]
    pub pool_address: Bytes,
    #[serde(
        rename = "priceProvider_address",
        alias = "priceProviderAddress",
        deserialize_with = "deserialize_address"
    )]
    pub price_provider_address: Bytes,
    #[serde(
        rename = "quoterAddress",
        alias = "routerAddress",
        deserialize_with = "deserialize_address"
    )]
    pub quoter_address: Bytes,
    #[serde(deserialize_with = "deserialize_address")]
    pub token0: Bytes,
    #[serde(deserialize_with = "deserialize_address")]
    pub token1: Bytes,
    #[serde(rename = "cexStep")]
    pub cex_step: Option<f64>,
    #[serde(rename = "dexStep")]
    pub dex_step: Option<f64>,
}

fn deserialize_address<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let address = Address::from_str(&s).map_err(serde::de::Error::custom)?;
    Bytes::from_str(&address.to_checksum(None)).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricBidAskResponse {
    pub pair: String,
    #[serde(rename = "bidAdj")]
    pub bid_adj: String,
    #[serde(rename = "askAdj")]
    pub ask_adj: String,
    #[serde(rename = "quoteAvailable")]
    pub quote_available: bool,
    #[serde(rename = "totalToken0Available")]
    pub total_token0_available: String,
    #[serde(rename = "totalToken1Available")]
    pub total_token1_available: String,
    #[serde(rename = "latestBlock")]
    pub latest_block: u64,
    #[serde(rename = "blockTs")]
    pub block_ts: u64,
    #[serde(rename = "serverTs")]
    pub server_ts: u64,
    #[serde(rename = "quoteExpiration")]
    pub quote_expiration: u64,
    #[serde(default)]
    pub depth: serde_json::Value,
}

impl MetricBidAskResponse {
    pub fn bid_price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.bid_adj)
    }

    pub fn ask_price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.ask_adj)
    }

    pub fn total_token0_available(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.total_token0_available, "totalToken0Available")
    }

    pub fn total_token1_available(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.total_token1_available, "totalToken1Available")
    }
}

// Metric's APIs return Q64 values as decimal strings. We only convert them for indicative
// routing; the binding quote path keeps the original Q64 strings for calldata encoding.
pub fn q64_decimal_to_f64(value: &str) -> Result<f64, RFQError> {
    let raw = parse_biguint(value, "Q64 price")?;
    let raw = raw
        .to_f64()
        .ok_or_else(|| RFQError::ParsingError(format!("Q64 price does not fit in f64: {value}")))?;
    Ok(raw / Q64_FLOAT)
}

fn parse_biguint(value: &str, field: &str) -> Result<BigUint, RFQError> {
    BigUint::from_str(value)
        .map_err(|_| RFQError::ParsingError(format!("Failed to parse {field}: {value}")))
}

fn parse_bigint(value: &str, field: &str) -> Result<BigInt, RFQError> {
    BigInt::from_str(value)
        .map_err(|_| RFQError::ParsingError(format!("Failed to parse {field}: {value}")))
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricQuoteRequest {
    pub pool: String,
    #[serde(rename = "zeroForOne")]
    pub zero_for_one: bool,
    #[serde(rename = "amountSpecified")]
    pub amount_specified: String,
    #[serde(rename = "priceLimitX64")]
    pub price_limit_x64: String,
    #[serde(rename = "bidPriceX64")]
    pub bid_price_x64: String,
    #[serde(rename = "askPriceX64")]
    pub ask_price_x64: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricQuoteResponse {
    #[serde(rename = "amount0Delta")]
    pub amount0_delta: String,
    #[serde(rename = "amount1Delta")]
    pub amount1_delta: String,
}

impl MetricQuoteResponse {
    pub fn amount_out(&self, zero_for_one: bool) -> Result<BigUint, RFQError> {
        // Deltas are from the pool's point of view, so the trader's output is negative.
        let output_delta = if zero_for_one {
            parse_bigint(&self.amount1_delta, "amount1Delta")?
        } else {
            parse_bigint(&self.amount0_delta, "amount0Delta")?
        };

        if output_delta >= BigInt::zero() {
            return Err(RFQError::ParsingError(format!(
                "Expected negative output delta for Metric quote, got {output_delta}"
            )));
        }

        output_delta
            .abs()
            .to_biguint()
            .ok_or_else(|| RFQError::ParsingError("Failed to convert output delta".to_string()))
    }

    pub fn validate(
        &self,
        params: &GetAmountOutParams,
        zero_for_one: bool,
    ) -> Result<(), RFQError> {
        let input_delta = if zero_for_one {
            parse_bigint(&self.amount0_delta, "amount0Delta")?
        } else {
            parse_bigint(&self.amount1_delta, "amount1Delta")?
        };
        let expected = BigInt::from(params.amount_in.clone());
        if input_delta != expected {
            return Err(RFQError::FatalError(format!(
                "Metric input delta mismatch: expected {}, got {}",
                params.amount_in, input_delta
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::*;

    #[test]
    fn test_q64_decimal_to_f64() {
        let one = "18446744073709551616";
        assert_eq!(q64_decimal_to_f64(one).unwrap(), 1.0);
    }

    #[test]
    fn test_metric_quote_response_amount_out_zero_for_one() {
        let quote =
            MetricQuoteResponse { amount0_delta: "1000".into(), amount1_delta: "-2000".into() };

        assert_eq!(quote.amount_out(true).unwrap(), BigUint::from(2000u64));
    }

    #[test]
    fn test_metric_quote_response_amount_out_one_for_zero() {
        let quote =
            MetricQuoteResponse { amount0_delta: "-3000".into(), amount1_delta: "4000".into() };

        assert_eq!(quote.amount_out(false).unwrap(), BigUint::from(3000u64));
    }
}
