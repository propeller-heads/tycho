use std::borrow::Cow;

use prost::Message;
use serde::{Deserialize, Serialize};
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::{
    book::levels::{InvalidLevel, Levels, PriceLevel},
    rfq::errors::RFQError,
};

/// Protobuf message for Bebop pricing updates
#[derive(Clone, PartialEq, Message)]
pub struct BebopPricingUpdate {
    #[prost(message, repeated, tag = "1")]
    pub pairs: Vec<BebopPriceData>,
}

#[derive(Clone, PartialEq, Message)]
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
    fn levels(flat: &[f32]) -> Result<Levels, InvalidLevel> {
        Levels::new(
            flat.as_chunks::<2>()
                .0
                .iter()
                .map(|[price, quantity]| PriceLevel {
                    price: f64::from(*price),
                    quantity: f64::from(*quantity),
                })
                .collect(),
        )
    }
}

/// One pair's two-sided book as decoded from a pricing frame: the flat `f32` arrays turned into
/// validated ladders once, so the state simulates without converting per call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BebopBook {
    pub base: Bytes,
    pub quote: Bytes,
    /// Bebop's own update time; milliseconds on the wire.
    pub last_update_ts: u64,
    /// Selling base: `quote` per base unit, base quantities.
    pub bids: Levels,
    /// Buying base: `quote` per base unit, base quantities.
    pub asks: Levels,
}

impl TryFrom<BebopPriceData> for BebopBook {
    type Error = InvalidLevel;

    fn try_from(price_data: BebopPriceData) -> Result<Self, InvalidLevel> {
        Ok(BebopBook {
            bids: BebopPriceData::levels(&price_data.bids)?,
            asks: BebopPriceData::levels(&price_data.asks)?,
            base: Bytes::from(price_data.base),
            quote: Bytes::from(price_data.quote),
            last_update_ts: price_data.last_update_ts,
        })
    }
}

impl BebopBook {
    /// Calculates Total Value Locked (TVL) based on bid/ask levels.
    ///
    /// TVL is calculated using the formula from Bebop's documentation:
    /// https://docs.bebop.xyz/bebop/bebop-api-pmm-rfq/rfq-api-endpoints/pricing#interpreting-price-levels
    ///
    /// Returns the average of bid and ask TVLs across all price levels.
    ///
    /// Note: This calculation normalizes the quote token in case quote_book is passed.
    ///
    /// # Parameters
    /// - `quote_book`: Optional book for converting the quote token to an approved token
    pub fn calculate_tvl(&self, quote_book: Option<&BebopBook>) -> f64 {
        let mut total_tvl = (self.bids.notional() + self.asks.notional()) / 2.0;
        // If a quote book is provided, we need to normalize the TVL to be in
        // one of the approved token (for example USDC)
        if let Some(quote_book) = quote_book {
            if let Some(price_of_quote_token) = quote_book.get_mid_price(total_tvl, &self.quote) {
                total_tvl *= price_of_quote_token;
            } else {
                // Quote token has no TVL in one of the approved tokens (for normalizations)
                return 0.0;
            }
        }
        total_tvl
    }

    /// The average of the bid-side and ask-side prices for selling `amount` of `sell_token`,
    /// each priced on the part of the ladder the amount consumes. `None` when `sell_token` is
    /// neither side of the pair or either side has no levels: a one-sided book is not
    /// considered tradeable for pricing purposes.
    pub fn get_mid_price(&self, amount: f64, sell_token: &Bytes) -> Option<f64> {
        if sell_token != &self.base && sell_token != &self.quote {
            return None;
        }
        let (bids, asks) = if sell_token == &self.quote {
            (Cow::Owned(self.bids.invert()), Cow::Owned(self.asks.invert()))
        } else {
            (Cow::Borrowed(&self.bids), Cow::Borrowed(&self.asks))
        };
        let asks_price = asks.average_price(amount)?;
        let bids_price = bids.average_price(amount)?;
        Some((asks_price + bids_price) / 2.0)
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
#[serde(rename_all = "camelCase")]
pub struct TxData {
    pub to: Bytes,
    pub data: Bytes,
    pub value: String,
    pub from: Bytes,
    pub gas: u64,
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

        let tvl = BebopBook::try_from(price_data.clone())
            .unwrap()
            .calculate_tvl(None);

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

        let tvl = BebopBook::try_from(price_data_eth_tamara)
            .unwrap()
            .calculate_tvl(Some(&BebopBook::try_from(price_data_tamara_usdc).unwrap()));

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

        let tvl = BebopBook::try_from(price_data_eth_tamara)
            .unwrap()
            .calculate_tvl(Some(&BebopBook::try_from(price_data_usdc_tamara).unwrap()));

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
    fn test_get_mid_price() {
        let weth_addr =
            Bytes::from(hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()); // WETH
        let usdc_addr =
            Bytes::from(hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()); // USDC

        let price_data = BebopPriceData {
            base: weth_addr.to_vec(),
            quote: usdc_addr.to_vec(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };

        // Test mid price for larger amount spanning multiple levels (selling WETH for USDC)
        let mid_price_large = BebopBook::try_from(price_data.clone())
            .unwrap()
            .get_mid_price(3.0, &weth_addr);
        // Sell 3.0 tokens: 2.0 at 2000.0 + 1.0 at 1999.0 = 5999.0 total, price = 5999/3 = 1999.67
        // Buy 3.0 tokens: 3.0 at 2001.0 = 6003.0 total, price = 6003/3 = 2001.0
        // Mid price = (1999.67 + 2001.0) / 2 = 2000.33 USDC per WETH
        assert!((mid_price_large.unwrap() - 2000.3333333333335).abs() < 0.01);

        // Inverted direction: selling USDC for WETH prices in WETH per USDC, roughly 1/2000.
        let weth_price = BebopBook::try_from(price_data.clone())
            .unwrap()
            .get_mid_price(6000.0, &usdc_addr)
            .unwrap();
        assert!((weth_price - 0.0005).abs() < 0.0001);

        // A token outside the pair has no price.
        let dai_addr =
            Bytes::from(hex::decode("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap());
        assert_eq!(
            BebopBook::try_from(price_data.clone())
                .unwrap()
                .get_mid_price(100.0, &dai_addr),
            None
        );

        // Test missing bids. Token considered untradeable.
        let price_data = BebopPriceData {
            base: weth_addr.to_vec(),
            quote: usdc_addr.to_vec(),
            last_update_ts: 1234567890,
            bids: vec![],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };
        assert_eq!(
            BebopBook::try_from(price_data.clone())
                .unwrap()
                .get_mid_price(3.0, &weth_addr),
            None
        );

        // Test missing asks. Token considered untradeable.
        let price_data = BebopPriceData {
            base: weth_addr.to_vec(),
            quote: usdc_addr.to_vec(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![],
        };
        assert_eq!(
            BebopBook::try_from(price_data.clone())
                .unwrap()
                .get_mid_price(3.0, &weth_addr),
            None
        );

        // Test not enough liquidity (give estimate based on existing liquidity)
        let price_data = BebopPriceData {
            base: weth_addr.to_vec(),
            quote: usdc_addr.to_vec(),
            last_update_ts: 1234567890,
            bids: vec![2000.0f32, 2.0f32, 1999.0f32, 3.0f32],
            asks: vec![2001.0f32, 3.0f32, 2002.0f32, 1.0f32],
        };
        let insufficient_mid = BebopBook::try_from(price_data.clone())
            .unwrap()
            .get_mid_price(10.0, &weth_addr);
        // With 10 WETH but only 5 WETH liquidity, we get partial fills
        // The price returned is still an average price
        assert_eq!(insufficient_mid, Some(2000.325));
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
