use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeOrderbookSide {
    /// The market maker buys base; the taker sells base and receives quote.
    Bid,
    /// The market maker sells base; the taker sells quote and receives base.
    Ask,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativePriceLevel {
    pub quantity: f64,
    pub price: f64,
}

fn deserialize_level<'de, D>(deserializer: D) -> Result<NativePriceLevel, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let [quantity, price] = <[f64; 2]>::deserialize(deserializer)?;
    if !quantity.is_finite() || quantity < 0.0 {
        return Err(serde::de::Error::custom(
            "price level quantity must be a non-negative finite number",
        ))
    }
    // Native may use zero-quantity placeholders such as [0, 0]. They are discarded when books are
    // grouped, so their price is irrelevant.
    if quantity == 0.0 {
        return Ok(NativePriceLevel { quantity, price })
    }
    if !price.is_finite() || price <= 0.0 {
        return Err(serde::de::Error::custom("price level price must be a positive finite number"))
    }

    Ok(NativePriceLevel { quantity, price })
}

fn deserialize_levels<'de, D>(deserializer: D) -> Result<Vec<NativePriceLevel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Level(#[serde(deserialize_with = "deserialize_level")] NativePriceLevel);

    Vec::<Level>::deserialize(deserializer).map(|levels| {
        levels
            .into_iter()
            .map(|level| level.0)
            .collect()
    })
}

fn deserialize_non_negative_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if !value.is_finite() || value < 0.0 {
        return Err(serde::de::Error::custom("expected a non-negative finite number"))
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NativeOrderbookEntry {
    pub base_address: Bytes,
    pub quote_address: Bytes,
    /// Minimum base-token amount in atomic units.
    ///
    /// Unlike `levels`, whose quantities are expressed in normal token units, Native Relay's
    /// aggregated orderbook returns this field in atomic units. It constrains input for a bid and
    /// output for an ask.
    #[serde(deserialize_with = "deserialize_non_negative_f64")]
    pub minimum_in_base: f64,
    pub side: NativeOrderbookSide,
    #[serde(deserialize_with = "deserialize_levels")]
    pub levels: Vec<NativePriceLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativePriceData {
    pub base_address: Bytes,
    pub quote_address: Bytes,
    /// Atomic base-token minimum input when selling base into bids.
    pub minimum_in_base: f64,
    /// Atomic quote-token minimum input when selling quote into asks.
    pub minimum_in_quote: f64,
    /// Atomic base-token minimum output when selling quote into asks.
    pub minimum_out_base: f64,
    /// Atomic quote-token minimum output when selling base into bids.
    pub minimum_out_quote: f64,
    pub bids: Vec<NativePriceLevel>,
    pub asks: Vec<NativePriceLevel>,
}

impl NativePriceData {
    /// Returns `None` when the book cannot be valued or the calculation is non-finite.
    pub fn calculate_tvl(&self, quote_price_data: Option<&NativePriceData>) -> Option<f64> {
        let bid_tvl: f64 = self
            .bids
            .iter()
            .map(|level: &NativePriceLevel| level.quantity * level.price)
            .sum();
        let ask_tvl: f64 = self
            .asks
            .iter()
            .map(|level: &NativePriceLevel| level.quantity * level.price)
            .sum();

        let total_tvl = match (self.bids.is_empty(), self.asks.is_empty()) {
            (false, false) => bid_tvl.midpoint(ask_tvl),
            (false, true) => bid_tvl,
            (true, false) => ask_tvl,
            (true, true) => 0.0,
        };
        if !total_tvl.is_finite() {
            return None
        }

        if let Some(quote_data) = quote_price_data {
            let price_of_quote_token = quote_data.get_mid_price(total_tvl, &self.quote_address)?;
            let converted_tvl = total_tvl * price_of_quote_token;
            return converted_tvl
                .is_finite()
                .then_some(converted_tvl)
        }

        Some(total_tvl)
    }

    pub fn get_mid_price(&self, amount: f64, sell_token: &Bytes) -> Option<f64> {
        if sell_token != &self.base_address && sell_token != &self.quote_address {
            return None;
        }

        let inverse = sell_token == &self.quote_address;
        let asks_price = Self::get_price_for_levels(amount, &self.asks, inverse);
        let bids_price = Self::get_price_for_levels(amount, &self.bids, inverse);

        match (bids_price, asks_price) {
            (Some(bid), Some(ask)) => Some(bid.midpoint(ask)),
            (Some(bid), None) => Some(bid),
            (None, Some(ask)) => Some(ask),
            (None, None) => None,
        }
    }

    fn get_price_for_levels(
        amount_in: f64,
        price_levels: &[NativePriceLevel],
        invert: bool,
    ) -> Option<f64> {
        if price_levels.is_empty() || amount_in <= 0.0 {
            return None;
        }

        let levels =
            if invert { Self::invert_price_levels(price_levels) } else { price_levels.to_vec() };

        let (amount_out, remaining_in) = Self::get_amount_out_from_levels(amount_in, &levels);
        let consumed_amount_in = amount_in - remaining_in;
        if consumed_amount_in <= 0.0 {
            return None;
        }

        Some(amount_out / consumed_amount_in)
    }

    pub fn get_amount_out_from_levels(
        amount_in: f64,
        price_levels: &[NativePriceLevel],
    ) -> (f64, f64) {
        let mut remaining_amount_in = amount_in;
        let mut amount_out = 0.0;

        for level in price_levels {
            if remaining_amount_in <= 0.0 {
                break;
            }

            let amount_in_available_to_trade = remaining_amount_in.min(level.quantity);
            amount_out += amount_in_available_to_trade * level.price;
            remaining_amount_in -= amount_in_available_to_trade;
        }

        (amount_out, remaining_amount_in)
    }

    pub fn invert_price_levels(price_levels: &[NativePriceLevel]) -> Vec<NativePriceLevel> {
        price_levels
            .iter()
            .filter(|level| level.price > 0.0)
            .map(|level| NativePriceLevel {
                quantity: level.quantity * level.price,
                price: 1.0 / level.price,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FirmQuoteRequest {
    pub from_address: String,
    pub src_chain: NativeSupportedChain,
    pub dst_chain: NativeSupportedChain,
    pub token_in: String,
    pub token_out: String,
    pub amount_wei: String,
    pub version: u32,
    pub allow_multihop: bool,
}

// --- Response ---

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetFee {
    pub signer: String,
    pub fee_recipient: String,
    pub fee_rate: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TxRequest {
    pub target: String,
    pub calldata: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirmQuoteOrder {
    pub pool: String,
    pub signer: String,
    pub recipient: String,
    pub seller_token: String,
    pub buyer_token: String,
    pub effective_seller_token_amount: String,
    pub seller_token_amount: String,
    pub buyer_token_amount: String,
    pub deadline_timestamp: u64,
    pub nonce: u64,
    pub quote_id: String,
    pub multi_hop: bool,
    pub signature: String,
    pub external_swap_calldata: String,
    pub amount_out_minimum: String,
    pub widget_fee: WidgetFee,
    pub widget_fee_signature: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirmQuoteResponse {
    pub success: bool,
    pub orders: Vec<FirmQuoteOrder>,
    pub widget_fee: WidgetFee,
    pub widget_fee_signature: String,
    pub recipient: String,
    pub amount_in: String,
    pub amount_out: String,
    pub amount_out_before_fee: String,
    pub fallback_swap_data_array: Option<serde_json::Value>,
    pub token_transfer_fee_on_percent: f64,
    pub tx_request: TxRequest,
    pub source: Vec<u32>,
    pub error_message: String,
    #[serde(rename = "router_version")]
    pub router_version: String,
    pub amount_in_offset: u32,
    pub amount_out_minimum_offset: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NativeApiErrorResponse {
    pub code: u64,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeSupportedChain {
    Ethereum,
    Bsc,
    Arbitrum,
    Base,
}

impl TryFrom<Chain> for NativeSupportedChain {
    type Error = String;

    fn try_from(chain: Chain) -> Result<Self, Self::Error> {
        match chain {
            Chain::Ethereum => Ok(NativeSupportedChain::Ethereum),
            Chain::Bsc => Ok(NativeSupportedChain::Bsc),
            Chain::Arbitrum => Ok(NativeSupportedChain::Arbitrum),
            Chain::Base => Ok(NativeSupportedChain::Base),
            unsupported => Err(format!("Chain {unsupported:?} not supported by Native API")),
        }
    }
}

impl NativeSupportedChain {
    pub fn as_str(&self) -> &'static str {
        match self {
            NativeSupportedChain::Ethereum => "ethereum",
            NativeSupportedChain::Bsc => "bsc",
            NativeSupportedChain::Arbitrum => "arbitrum",
            NativeSupportedChain::Base => "base",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn addr(address: &str) -> Bytes {
        Bytes::from_str(address).unwrap()
    }

    #[test]
    fn deserializes_native_relay_orderbook_entry() {
        let json = r#"{
            "base_symbol": "WETH",
            "quote_symbol": "USDT",
            "base_address": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            "quote_address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
            "minimum_in_base": 0,
            "side": "bid",
            "levels": [[0, 0], [0.0001, 3213.12345], [12.75786733219471, 3210.15]]
        }"#;

        let entry: NativeOrderbookEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.side, NativeOrderbookSide::Bid);
        assert_eq!(entry.levels[0].quantity, 0.0);
        assert_eq!(entry.levels[1].quantity, 0.0001);
        assert_eq!(entry.levels[1].price, 3213.12345);
    }

    #[test]
    fn rejects_negative_orderbook_minimum() {
        let result = serde_json::from_value::<NativeOrderbookEntry>(serde_json::json!({
            "base_address": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            "quote_address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
            "minimum_in_base": -1,
            "side": "bid",
            "levels": [[1, 2_000]],
        }));

        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_orderbook_levels() {
        for level in [[-1.0, 2_000.0], [1.0, 0.0], [1.0, -2_000.0]] {
            let result = serde_json::from_value::<NativeOrderbookEntry>(serde_json::json!({
                "base_address": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "quote_address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                "minimum_in_base": 0,
                "side": "bid",
                "levels": [level],
            }));

            assert!(result.is_err(), "level {level:?} should be rejected");
        }
    }

    #[test]
    fn calculates_tvl_as_average_bid_ask_quote_value() {
        let price_data = NativePriceData {
            base_address: addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            quote_address: addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![
                NativePriceLevel { quantity: 1.0, price: 2000.0 },
                NativePriceLevel { quantity: 2.0, price: 1999.0 },
            ],
            asks: vec![
                NativePriceLevel { quantity: 1.5, price: 2001.0 },
                NativePriceLevel { quantity: 1.0, price: 2002.0 },
            ],
        };

        let tvl = price_data
            .calculate_tvl(None)
            .expect("TVL should be finite");
        assert!((tvl - 5500.75).abs() < 0.01);
    }

    #[test]
    fn normalizes_tvl_through_quote_token_market() {
        let tamara = addr("0x1234567890123456789012345678901234567890");
        let usdc = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let price_data_eth_tamara = NativePriceData {
            base_address: addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            quote_address: tamara.clone(),
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 3.0, price: 100.0 }],
            asks: vec![NativePriceLevel { quantity: 3.0, price: 100.0 }],
        };
        let price_data_tamara_usdc = NativePriceData {
            base_address: tamara,
            quote_address: usdc,
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 300.0, price: 9.0 }],
            asks: vec![NativePriceLevel { quantity: 300.0, price: 11.0 }],
        };

        assert_eq!(
            price_data_eth_tamara.calculate_tvl(Some(&price_data_tamara_usdc)),
            Some(3000.0)
        );
    }

    #[test]
    fn calculates_and_normalizes_tvl_for_one_sided_bid_books() {
        let tamara = addr("0x1234567890123456789012345678901234567890");
        let price_data_eth_tamara = NativePriceData {
            base_address: addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            quote_address: tamara.clone(),
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 3.0, price: 100.0 }],
            asks: vec![],
        };
        let price_data_tamara_usdc = NativePriceData {
            base_address: tamara,
            quote_address: addr("0xA0b86991c6218b36c1d19d4a2e9Eb0cE3606eB48"),
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 300.0, price: 10.0 }],
            asks: vec![],
        };

        assert_eq!(price_data_eth_tamara.calculate_tvl(None), Some(300.0));
        assert_eq!(
            price_data_eth_tamara.calculate_tvl(Some(&price_data_tamara_usdc)),
            Some(3000.0)
        );
    }

    #[test]
    fn rejects_non_finite_derived_tvl() {
        let weth = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let tamara = addr("0x1234567890123456789012345678901234567890");
        let usdc = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let book = |base_address, quote_address, quantity, price| NativePriceData {
            base_address,
            quote_address,
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity, price }],
            asks: vec![],
        };
        let overflowing_book = book(weth.clone(), usdc.clone(), 1e308, 2.0);
        let finite_book = book(weth, tamara.clone(), 1.0, 2.0);
        let overflowing_conversion_book = book(tamara, usdc, 2.0, 1e308);

        assert_eq!(overflowing_book.calculate_tvl(None), None);
        assert_eq!(finite_book.calculate_tvl(Some(&overflowing_conversion_book)), None);
    }
}
