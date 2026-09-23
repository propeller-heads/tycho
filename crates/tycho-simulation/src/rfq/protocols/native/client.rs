use std::{collections::HashMap, str::FromStr, time::SystemTime};

use alloy::primitives::Address;
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::{timeout, Duration};
use tracing::{instrument, warn};
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote, Bytes,
};

use crate::{
    evm::protocol::utils::bytes_to_address,
    rfq::{
        errors::RFQError,
        protocols::native::models::{
            FirmQuoteRequest, FirmQuoteResponse, NativeApiErrorResponse, NativeOrderbookEntry,
            NativeSupportedChain,
        },
    },
    snapshot_feed::{errors::FeedError, http::fetch_bytes},
};

const MAX_QUOTE_ATTEMPTS: u32 = 3;
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(100);
const NATIVE_API_RETRY_DELAY: Duration = Duration::from_secs(1);
// V6 tradeRFQT: a dynamic quote tuple and two uint256 overrides, i.e. three ABI head words.
const TRADE_RFQT_SELECTOR: [u8; 4] = [0x70, 0x83, 0x52, 0x7c];
const MIN_TRADE_RFQT_CALLDATA_LEN: usize = 4 + 3 * 32;
const ACTUAL_SELLER_AMOUNT_OFFSET: usize = 4 + 32;
const ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET: usize = 4 + 2 * 32;

enum QuoteAttemptError {
    Retry { error: RFQError, delay: Duration },
    Fatal(RFQError),
}

/// Requests binding Native Relay quotes. One instance is shared (via `Arc`) by every state a
/// [`NativeFeed`](super::feed::NativeFeed) emits, so all of them reuse the same HTTP connection
/// pool.
///
/// Serialization keeps the configuration but skips the credential and the HTTP client: a
/// deserialized client gets a fresh connection pool and an empty key, so binding quotes fail at
/// call time until re-configured. `Debug` output omits the credential as well.
#[derive(derive_more::Debug, Serialize, Deserialize)]
pub struct NativeClient {
    chain: NativeSupportedChain,
    /// Base URL of the swap API; the firm-quote path is appended per request.
    endpoint: String,
    #[serde(skip_serializing, default)]
    #[debug(skip)]
    api_key: String,
    quote_timeout: Duration,
    #[serde(skip)]
    http: Client,
}

impl NativeClient {
    pub fn new(
        chain: NativeSupportedChain,
        endpoint: String,
        api_key: String,
        quote_timeout: Duration,
    ) -> Self {
        NativeClient { chain, endpoint, api_key, quote_timeout, http: Client::new() }
    }

    /// Every failure Native reports in an error envelope, including a refused key: a credential
    /// can be fixed at the venue while the feed keeps polling, and the poll that then succeeds
    /// restores the book without a restart. Codes are documented at
    /// <https://docs.native.org/native-dev/build-with-native/swap-aggregators/firmquote-swap-apis/miscellaneous/error-handling#error-codes>.
    fn orderbook_api_error(error: &NativeApiErrorResponse) -> FeedError {
        FeedError::Connection(format!("Native API error {}: {}", error.code, error.message))
    }

    /// The venue's aggregated orderbook: one entry per pair, side and orientation its makers
    /// quote.
    pub async fn fetch_orderbook(&self) -> Result<Vec<NativeOrderbookEntry>, FeedError> {
        let request = self
            .http
            .get(format!("{}/orderbook", self.endpoint))
            // `showNative` is not boolean: its value selects the address used for native-token
            // books. Request address(0) so the response matches Tycho's internal representation.
            .query(&[("chain", self.chain.as_str()), ("showNative", "0x0")])
            .header("accept", "application/json")
            .header("apikey", &self.api_key);

        let body = fetch_bytes(request, "Native Relay orderbook").await?;

        // Native reports an authentication or request failure as an error envelope under HTTP
        // 200, so a successful response still has two possible shapes. The orderbook is tried
        // first: it is what almost every poll returns, and when neither shape matches, its parse
        // error is the one that says what the response actually looked like.
        serde_json::from_slice(&body).map_err(|orderbook_error| {
            match serde_json::from_slice::<NativeApiErrorResponse>(&body) {
                Ok(api_error) => Self::orderbook_api_error(&api_error),
                Err(_) => FeedError::Parsing(format!(
                    "Failed to parse Native Relay orderbook: {orderbook_error}"
                )),
            }
        })
    }

    #[instrument(
        name = "quote_request",
        level = "error",
        skip_all,
        fields(token_in = %params.token_in, token_out = %params.token_out, amount_in = %params.amount_in)
    )]
    pub async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let receiver = bytes_to_address(&params.receiver)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?;
        let token_in = bytes_to_address(&params.token_in)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?;
        let token_out = bytes_to_address(&params.token_out)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?;

        let request_data = FirmQuoteRequest {
            src_chain: self.chain,
            dst_chain: self.chain,
            from_address: receiver.to_string(),
            amount_wei: params.amount_in.to_string(),
            token_in: token_in.to_string(),
            token_out: token_out.to_string(),
            version: 6,
            allow_multihop: false,
        };
        let mut last_error = None;

        let attempts = async {
            for attempt in 1..=MAX_QUOTE_ATTEMPTS {
                match self
                    .try_quote(&request_data, params)
                    .await
                {
                    Ok(quote) => return Ok(quote),
                    Err(QuoteAttemptError::Fatal(error)) => return Err(error),
                    Err(QuoteAttemptError::Retry { error, delay }) => {
                        warn!(
                            attempt,
                            max_attempts = MAX_QUOTE_ATTEMPTS,
                            error = %error,
                            "quote attempt failed, retrying"
                        );
                        last_error = Some(error);

                        if attempt < MAX_QUOTE_ATTEMPTS {
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }

            Err(last_error.take().unwrap_or_else(|| {
                RFQError::ConnectionError(
                    "Native quote request failed after all attempts".to_string(),
                )
            }))
        };

        // Bind the timeout result before inspecting last_error so the attempts future — and its
        // mutable borrow — has been dropped.
        let result = timeout(self.quote_timeout, attempts).await;
        match result {
            Ok(result) => result,
            Err(_) => Err(last_error.unwrap_or_else(|| {
                RFQError::ConnectionError(format!(
                    "Native quote request timed out after {:?}",
                    self.quote_timeout
                ))
            })),
        }
    }

    async fn try_quote(
        &self,
        request_data: &FirmQuoteRequest,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, QuoteAttemptError> {
        let response = self
            .http
            .get(format!("{}/firm-quote", self.endpoint))
            .query(request_data)
            .header("apikey", &self.api_key)
            .send()
            .await
            .map_err(|e| QuoteAttemptError::Retry {
                error: RFQError::ConnectionError(format!(
                    "Failed to make Native quote request: {e}"
                )),
                delay: TRANSIENT_RETRY_DELAY,
            })?;

        let status = response.status();
        let response_body = response
            .bytes()
            .await
            .map_err(|e| QuoteAttemptError::Retry {
                error: RFQError::ConnectionError(format!(
                    "Failed to read Native quote response: {e}"
                )),
                delay: TRANSIENT_RETRY_DELAY,
            })?;

        // Native returns documented API error codes in the body, including with HTTP 200.
        if let Ok(api_error) = serde_json::from_slice::<NativeApiErrorResponse>(&response_body) {
            return Err(Self::classify_api_error(&api_error));
        }

        if !status.is_success() {
            let response_text = String::from_utf8_lossy(&response_body);
            if status.is_server_error() {
                return Err(QuoteAttemptError::Retry {
                    error: RFQError::ConnectionError(format!(
                        "Native quote server error ({status}): {response_text}"
                    )),
                    delay: TRANSIENT_RETRY_DELAY,
                });
            }

            return Err(QuoteAttemptError::Fatal(RFQError::ConnectionError(format!(
                "Unexpected Native quote HTTP response ({status}): {response_text}"
            ))));
        }

        let quote_response =
            serde_json::from_slice::<FirmQuoteResponse>(&response_body).map_err(|e| {
                QuoteAttemptError::Retry {
                    error: RFQError::ParsingError(format!(
                        "Failed to parse Native quote response: {e}"
                    )),
                    delay: TRANSIENT_RETRY_DELAY,
                }
            })?;

        Self::process_quote_response(quote_response, params).map_err(QuoteAttemptError::Fatal)
    }

    // Native API error codes:
    // <https://docs.native.org/native-dev/build-with-native/swap-aggregators/firmquote-swap-apis/miscellaneous/error-handling#error-codes>
    fn classify_api_error(error: &NativeApiErrorResponse) -> QuoteAttemptError {
        let message = format!("Native API error {}: {}", error.code, error.message);
        match error.code {
            // Native documents these as temporary risk/rate-limit failures.
            301016 | 405030 => QuoteAttemptError::Retry {
                error: RFQError::QuoteNotFound(message),
                delay: NATIVE_API_RETRY_DELAY,
            },
            201005 => QuoteAttemptError::Retry {
                error: RFQError::ConnectionError(message),
                delay: NATIVE_API_RETRY_DELAY,
            },
            // The requested quote is unavailable for the current orderbook/liquidity.
            // 171055 is not in the public table, but Native returns it when the seller amount is
            // below the current book minimum.
            101010 | 171037 | 171011 | 171015 | 171055 | 101007 => {
                QuoteAttemptError::Fatal(RFQError::QuoteNotFound(message))
            }
            // The request itself must be corrected before another attempt can succeed.
            131003 | 131004 | 131011 | 171018 | 171053 | 131005 => {
                QuoteAttemptError::Fatal(RFQError::InvalidInput(message))
            }
            201001 => QuoteAttemptError::Fatal(RFQError::FatalError(message)),
            _ => QuoteAttemptError::Fatal(RFQError::FatalError(format!(
                "Unknown Native API error {}: {}",
                error.code, error.message
            ))),
        }
    }

    fn process_quote_response(
        quote_response: FirmQuoteResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        if !quote_response.success {
            return Err(RFQError::QuoteNotFound(format!(
                "Native Relay quote request failed: {}",
                quote_response.error_message
            )));
        }

        if quote_response.router_version != "6" {
            return Err(RFQError::ParsingError(format!(
                "Unexpected Native router version: expected 6, got {}",
                quote_response.router_version
            )));
        }

        let order = quote_response
            .orders
            .first()
            .ok_or_else(|| {
                RFQError::QuoteNotFound(format!(
                    "No Native Relay orders for {} {} -> {}",
                    params.amount_in, params.token_in, params.token_out,
                ))
            })?;

        // Prevents silently accepting a mismatched/malicious quote.
        let seller_token = bytes_to_address(&params.token_in)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?;
        let buyer_token = bytes_to_address(&params.token_out)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?;
        let order_seller_token = Address::from_str(&order.seller_token).map_err(|e| {
            RFQError::ParsingError(format!(
                "Invalid Native seller token {}: {e}",
                order.seller_token
            ))
        })?;
        let order_buyer_token = Address::from_str(&order.buyer_token).map_err(|e| {
            RFQError::ParsingError(format!("Invalid Native buyer token {}: {e}", order.buyer_token))
        })?;
        if order_seller_token != seller_token || order_buyer_token != buyer_token {
            return Err(RFQError::ParsingError(format!(
                "Native Relay quote token mismatch: expected {}/{}, got {}/{}",
                seller_token, buyer_token, order_seller_token, order_buyer_token
            )));
        }

        let receiver = bytes_to_address(&params.receiver)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?;
        let order_recipient = Address::from_str(&order.recipient).map_err(|e| {
            RFQError::ParsingError(format!(
                "Invalid Native order recipient {}: {e}",
                order.recipient
            ))
        })?;
        if order_recipient != receiver {
            return Err(RFQError::ParsingError(format!(
                "Native Relay quote recipient mismatch: expected {receiver}, got {order_recipient}"
            )));
        }

        // Reject already-expired quotes before building a SignedQuote.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".to_string()))?
            .as_secs();

        if order.deadline_timestamp <= now {
            return Err(RFQError::QuoteNotFound(format!(
                "Native Relay quote already expired: deadline {} <= now {}",
                order.deadline_timestamp, now
            )));
        }

        // Bind Native's top-level amountIn and signed sellerTokenAmount to the requested quote
        // baseline. The encoder stores this baseline as signedAmountIn; the executor supplies
        // actualSellerAmount when execution receives a different amount from the preceding hop.
        let quoted_amount_in = BigUint::from_str(&quote_response.amount_in).map_err(|_| {
            RFQError::ParsingError(format!(
                "Failed to parse amount_in: {}",
                quote_response.amount_in
            ))
        })?;
        if quoted_amount_in != params.amount_in {
            return Err(RFQError::ParsingError(format!(
                "Native Relay quote input amount mismatch: expected {}, got {}",
                params.amount_in, quoted_amount_in
            )));
        }

        let signed_amount_in = BigUint::from_str(&order.seller_token_amount).map_err(|_| {
            RFQError::ParsingError(format!(
                "Failed to parse signed seller token amount: {}",
                order.seller_token_amount
            ))
        })?;
        if signed_amount_in != params.amount_in {
            return Err(RFQError::ParsingError(format!(
                "Native Relay signed input amount mismatch: expected {}, got {}",
                params.amount_in, signed_amount_in
            )));
        }

        // effectiveSellerTokenAmount may differ from the requested gross input for
        // fee-on-transfer tokens, so it is not an equality invariant here. amountIn and
        // sellerTokenAmount still bind the quote to Tycho's requested input.
        // Native requires txRequest.value for native-token quotes. Validate the quoted value here,
        // while it still describes the original signed amount. During execution, the preceding hop
        // may deliver either less or more; the executor handles that through actualSellerAmount.
        let quoted_value = BigUint::from_str(&quote_response.tx_request.value).map_err(|_| {
            RFQError::ParsingError(format!(
                "Failed to parse Native txRequest.value: {}",
                quote_response.tx_request.value
            ))
        })?;
        let expected_value =
            if seller_token == Address::ZERO { quoted_amount_in.clone() } else { BigUint::ZERO };
        if quoted_value != expected_value {
            return Err(RFQError::ParsingError(format!(
                "Native Relay payable value mismatch: expected {}, got {}",
                expected_value, quoted_value
            )));
        }

        if quote_response
            .tx_request
            .calldata
            .is_empty()
        {
            return Err(RFQError::QuoteNotFound(
                "Native Relay quote did not include calldata".to_string(),
            ));
        }
        // Calldata is pre-built by Native Relay, ready to submit as-is.
        let calldata = hex::decode(
            quote_response
                .tx_request
                .calldata
                .trim_start_matches("0x"),
        )
        .map_err(|e| RFQError::ParsingError(format!("Failed to decode calldata: {e}")))?;

        if calldata.len() < MIN_TRADE_RFQT_CALLDATA_LEN {
            return Err(RFQError::ParsingError(format!(
                "Native tradeRFQT calldata too short: expected at least {} bytes, got {}",
                MIN_TRADE_RFQT_CALLDATA_LEN,
                calldata.len()
            )));
        }

        if calldata[..TRADE_RFQT_SELECTOR.len()] != TRADE_RFQT_SELECTOR {
            return Err(RFQError::ParsingError(format!(
                "Unexpected Native V6 selector: expected 0x{}, got 0x{}",
                hex::encode(TRADE_RFQT_SELECTOR),
                hex::encode(&calldata[..TRADE_RFQT_SELECTOR.len()]),
            )));
        }

        // These offsets are fixed by V6's tradeRFQT ABI (a dynamic quote tuple and
        // two uint256 overrides). Rejecting any other value catches an incompatible or malformed
        // API response before it reaches the encoder; the executor independently
        // hardcodes the same positions rather than trusting route data.
        if quote_response.amount_in_offset as usize != ACTUAL_SELLER_AMOUNT_OFFSET ||
            quote_response.amount_out_minimum_offset as usize != ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET
        {
            return Err(RFQError::ParsingError(format!(
                "Unexpected Native V6 override offsets: expected {}/{} but got {}/{}",
                ACTUAL_SELLER_AMOUNT_OFFSET,
                ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET,
                quote_response.amount_in_offset,
                quote_response.amount_out_minimum_offset,
            )));
        }

        if calldata[ACTUAL_SELLER_AMOUNT_OFFSET..ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(RFQError::ParsingError(
                "Native actualSellerAmount override must be zero".to_string(),
            ));
        }
        if calldata[ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET..MIN_TRADE_RFQT_CALLDATA_LEN]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(RFQError::ParsingError(
                "Native actualMinOutputAmount override must be zero".to_string(),
            ));
        }

        let target = Bytes::from_str(&quote_response.tx_request.target).map_err(|_| {
            RFQError::ParsingError(format!(
                "Failed to parse router target address: {}",
                quote_response.tx_request.target
            ))
        })?;

        let mut quote_attributes: HashMap<String, Bytes> = HashMap::new();
        quote_attributes.insert("target".to_string(), target);
        quote_attributes.insert("calldata".to_string(), Bytes::from(calldata));
        quote_attributes.insert(
            "deadline_timestamp".to_string(),
            Bytes::from(
                order
                    .deadline_timestamp
                    .to_be_bytes()
                    .to_vec(),
            ),
        );

        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: quoted_amount_in,
            amount_out: BigUint::from_str(&quote_response.amount_out).map_err(|_| {
                RFQError::ParsingError(format!(
                    "Failed to parse amount_out: {}",
                    quote_response.amount_out
                ))
            })?,
            quote_attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use rstest::rstest;

    use super::*;
    use crate::snapshot_feed::http::test_support::{
        spawn_http_server, MockHttpServer, MockResponse,
    };

    fn successful_quote_json(amount_in: &str) -> serde_json::Value {
        let calldata = format!("0x7083527c{:064x}{:064x}{:064x}", 0x60u8, 0u8, 0u8);
        serde_json::json!({
            "success": true,
            "orders": [{
                "pool": "0x1111111111111111111111111111111111111111",
                "signer": "0x2222222222222222222222222222222222222222",
                "recipient": "0x4444444444444444444444444444444444444444",
                "sellerToken": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "buyerToken": "0xdAC17F958D2ee523a2206206994597C13D831ec7",
                "effectiveSellerTokenAmount": amount_in,
                "sellerTokenAmount": amount_in,
                "buyerTokenAmount": "2",
                "deadlineTimestamp": u64::MAX,
                "nonce": 1,
                "quoteId": "test-quote",
                "multiHop": false,
                "signature": "",
                "externalSwapCalldata": "",
                "amountOutMinimum": "2",
                "widgetFee": {
                    "signer": "0x0000000000000000000000000000000000000000",
                    "feeRecipient": "0x0000000000000000000000000000000000000000",
                    "feeRate": 0.0
                },
                "widgetFeeSignature": ""
            }],
            "widgetFee": {
                "signer": "0x0000000000000000000000000000000000000000",
                "feeRecipient": "0x0000000000000000000000000000000000000000",
                "feeRate": 0.0
            },
            "widgetFeeSignature": "",
            "recipient": "0x4444444444444444444444444444444444444444",
            "amountIn": amount_in,
            "amountOut": "2",
            "amountOutBeforeFee": "2",
            "fallbackSwapDataArray": null,
            "tokenTransferFeeOnPercent": 0.0,
            "txRequest": {
                "target": "0x4777A6B3A9A889ABfd4C7666Bdd2a7AB633293be",
                "calldata": calldata,
                "value": "0"
            },
            "source": [6],
            "errorMessage": "",
            "router_version": "6",
            "toWrap": false,
            "toUnwrap": false,
            "amountInOffset": 36,
            "amountOutMinimumOffset": 68
        })
    }

    fn successful_quote_response(amount_in: &str) -> FirmQuoteResponse {
        serde_json::from_value(successful_quote_json(amount_in)).unwrap()
    }

    fn create_test_client(endpoint: String) -> NativeClient {
        NativeClient::new(
            NativeSupportedChain::Ethereum,
            endpoint,
            "secret_key".to_string(),
            Duration::from_secs(5),
        )
    }

    fn create_test_quote_params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from(1u64),
            token_in: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            token_out: Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(),
            sender: Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap(),
            receiver: Bytes::from_str("0x4444444444444444444444444444444444444444").unwrap(),
        }
    }

    #[test]
    fn serialization_keeps_config_and_drops_the_key() {
        let client = create_test_client("https://native.example".to_string());

        let serialized = serde_json::to_string(&client).unwrap();
        let deserialized: NativeClient = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.chain, client.chain);
        assert_eq!(deserialized.endpoint, client.endpoint);
        assert_eq!(deserialized.quote_timeout, client.quote_timeout);
        assert!(deserialized.api_key.is_empty());
    }

    #[test]
    fn debug_output_omits_credentials() {
        let rendered = format!("{:?}", create_test_client("https://native.example".to_string()));

        assert!(!rendered.contains("secret_key"));
        assert!(rendered.contains("native.example"));
    }

    #[test]
    fn accepts_quote_with_requested_input_amount() {
        let params = create_test_quote_params();
        let response = successful_quote_response(&params.amount_in.to_string());

        let quote = NativeClient::process_quote_response(response, &params).unwrap();

        assert_eq!(quote.amount_in, params.amount_in);
        assert_eq!(
            quote
                .quote_attributes
                .get("deadline_timestamp")
                .unwrap()
                .as_ref(),
            u64::MAX.to_be_bytes()
        );
    }

    #[test]
    fn rejects_quote_with_mismatched_input_amount() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        // Keep sellerTokenAmount correct so only the top-level amountIn check can reject this.
        response.amount_in = "2".to_string();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("Native Relay quote input amount mismatch")
        ));
    }

    #[test]
    fn rejects_quote_with_mismatched_signed_input_amount() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.orders[0].seller_token_amount = "2".to_string();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("signed input amount mismatch")
        ));
    }

    #[test]
    fn accepts_quote_with_different_effective_input_amount() {
        let mut params = create_test_quote_params();
        params.amount_in = BigUint::from(100u64);
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.orders[0].effective_seller_token_amount = "99".to_string();

        let quote = NativeClient::process_quote_response(response, &params).unwrap();

        assert_eq!(quote.amount_in, params.amount_in);
    }

    #[rstest]
    #[case::seller_token(true)]
    #[case::buyer_token(false)]
    fn rejects_quote_with_mismatched_token(#[case] mutate_seller_token: bool) {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        let mismatched_token = "0x5555555555555555555555555555555555555555".to_string();
        if mutate_seller_token {
            response.orders[0].seller_token = mismatched_token;
        } else {
            response.orders[0].buyer_token = mismatched_token;
        }

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("quote token mismatch")
        ));
    }

    #[test]
    fn rejects_quote_with_mismatched_recipient() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.orders[0].recipient = "0x5555555555555555555555555555555555555555".to_string();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("recipient mismatch")
        ));
    }

    #[test]
    fn rejects_quote_without_calldata() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.tx_request.calldata.clear();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::QuoteNotFound(message)) if message.contains("did not include calldata")
        ));
    }

    #[test]
    fn rejects_truncated_trade_calldata() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.tx_request.calldata = format!("0x7083527c{}", "00".repeat(95));

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("calldata too short")
        ));
    }

    #[test]
    fn rejects_quote_with_wrong_trade_rfqt_selector() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        let mut calldata = hex::decode(
            response
                .tx_request
                .calldata
                .trim_start_matches("0x"),
        )
        .unwrap();
        // V4 also exposes tradeRFQT, but its quote tuple has a different ABI.
        calldata[..4].copy_from_slice(&[0x09, 0x47, 0xc2, 0xd9]);
        response.tx_request.calldata = format!("0x{}", hex::encode(calldata));

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("selector")
        ));
    }

    #[rstest]
    #[case::v4("4")]
    #[case::unknown("7")]
    fn rejects_quote_with_wrong_router_version(#[case] version: &str) {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.router_version = version.to_string();

        assert!(matches!(
            NativeClient::process_quote_response(response, &params),
            Err(RFQError::ParsingError(message)) if message.contains("Unexpected Native router version")
        ));
    }

    #[rstest]
    #[case::seller_offset(68, 68)]
    #[case::minimum_offset(36, 36)]
    fn rejects_quote_with_noncanonical_override_offsets(
        #[case] amount_in_offset: u32,
        #[case] amount_out_minimum_offset: u32,
    ) {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.amount_in_offset = amount_in_offset;
        response.amount_out_minimum_offset = amount_out_minimum_offset;

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains(
                "Unexpected Native V6 override offsets"
            )
        ));
    }

    #[rstest]
    #[case::seller(ACTUAL_SELLER_AMOUNT_OFFSET, "actualSellerAmount")]
    #[case::minimum(ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET, "actualMinOutputAmount")]
    fn rejects_quote_with_preset_override(
        #[case] override_offset: usize,
        #[case] field_name: &str,
    ) {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        let mut calldata = hex::decode(
            response
                .tx_request
                .calldata
                .trim_start_matches("0x"),
        )
        .unwrap();
        calldata[override_offset + 31] = 1;
        response.tx_request.calldata = format!("0x{}", hex::encode(calldata));

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains(field_name)
        ));
    }

    #[test]
    fn accepts_native_eth_response_using_zero_address() {
        let mut params = create_test_quote_params();
        params.token_in = Bytes::zero(20);
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.orders[0].seller_token = "0x0000000000000000000000000000000000000000".to_string();
        response.tx_request.value = params.amount_in.to_string();

        let quote = NativeClient::process_quote_response(response, &params).unwrap();

        assert_eq!(quote.amount_in, params.amount_in);
    }

    #[test]
    fn rejects_native_quote_with_mismatched_payable_value() {
        let mut params = create_test_quote_params();
        params.token_in = Bytes::zero(20);
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.orders[0].seller_token = "0x0000000000000000000000000000000000000000".to_string();
        response.tx_request.value = "2".to_string();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("payable value mismatch")
        ));
    }

    #[test]
    fn rejects_malformed_payable_value() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.tx_request.value = "not-a-number".to_string();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("txRequest.value")
        ));
    }

    #[test]
    fn rejects_erc20_quote_with_nonzero_payable_value() {
        let params = create_test_quote_params();
        let mut response = successful_quote_response(&params.amount_in.to_string());
        response.tx_request.value = "1".to_string();

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("payable value mismatch")
        ));
    }

    #[tokio::test]
    async fn requests_v6_firm_quote() {
        let seen_target = Arc::new(Mutex::new(None));
        let record = Arc::clone(&seen_target);
        let server = spawn_http_server(move |target| {
            *record.lock().unwrap() = Some(target.to_string());
            Some(("200 OK", successful_quote_json("1").to_string()))
        })
        .await;
        let client = create_test_client(server.url());
        let params = create_test_quote_params();

        let quote = client
            .request_binding_quote(&params)
            .await
            .unwrap();
        let target = seen_target
            .lock()
            .unwrap()
            .clone()
            .expect("one request");
        let url = reqwest::Url::parse(&format!("{}{target}", server.url())).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.path(), "/firm-quote");
        assert_eq!(query.get("version").map(String::as_str), Some("6"));
        assert_eq!(
            query
                .get("allow_multihop")
                .map(String::as_str),
            Some("false")
        );
        assert_eq!(quote.amount_in, params.amount_in);
    }

    /// Answers every request only after an hour, so any client deadline fires first.
    async fn create_hanging_quote_server() -> MockHttpServer {
        spawn_http_server(|_| {
            Some(MockResponse {
                delay: Duration::from_secs(3600),
                ..MockResponse::from(("200 OK", String::new()))
            })
        })
        .await
    }

    #[tokio::test]
    async fn handles_documented_quote_error_without_retrying() {
        let server = spawn_http_server(|_| {
            Some((
                "200 OK",
                r#"{"code":171015,"message":"quoted token not available"}"#.to_string(),
            ))
        })
        .await;
        let client = create_test_client(server.url());

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        match result {
            Err(RFQError::QuoteNotFound(message)) => {
                assert!(message.contains("171015"));
                assert!(message.contains("quoted token not available"));
            }
            other => panic!("Expected Native API error, got {other:?}"),
        }
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn handles_success_false_as_quote_not_found_without_retrying() {
        let mut response = successful_quote_json("1");
        response["success"] = serde_json::Value::Bool(false);
        response["errorMessage"] = serde_json::Value::String("quote unavailable".to_string());
        let body = response.to_string();
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let client = create_test_client(server.url());

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(
            result,
            Err(RFQError::QuoteNotFound(message)) if message.contains("quote unavailable")
        ));
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn retries_documented_temporary_api_error() {
        let server = spawn_http_server(|_| {
            Some((
                "200 OK",
                r#"{"code":301016,"message":"quote invalid, risk management checks failed"}"#
                    .to_string(),
            ))
        })
        .await;
        let client = create_test_client(server.url());

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(result, Err(RFQError::QuoteNotFound(_))));
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn retries_server_error_without_native_error_envelope() {
        let server = spawn_http_server(|_| {
            Some(("503 Service Unavailable", "<html>upstream unavailable</html>".to_string()))
        })
        .await;
        let client = create_test_client(server.url());

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(
            result,
            Err(RFQError::ConnectionError(message)) if message.contains("503 Service Unavailable")
        ));
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn retries_malformed_success_response() {
        let server =
            spawn_http_server(|_| Some(("200 OK", r#"{"unexpected":true}"#.to_string()))).await;
        let client = create_test_client(server.url());

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(result, Err(RFQError::ParsingError(_))));
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn times_out_when_quote_response_stalls() {
        let server = create_hanging_quote_server().await;
        let client = NativeClient::new(
            NativeSupportedChain::Ethereum,
            server.url(),
            "secret_key".to_string(),
            Duration::from_millis(50),
        );

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.request_binding_quote(&create_test_quote_params()),
        )
        .await
        .expect("Native quote timeout did not terminate the request");

        assert!(matches!(
            result,
            Err(RFQError::ConnectionError(message)) if message.contains("timed out after 50ms")
        ));
    }

    #[tokio::test]
    async fn shares_quote_timeout_across_retries() {
        let params = create_test_quote_params();
        let success_body = successful_quote_json(&params.amount_in.to_string()).to_string();
        let quote_timeout = NATIVE_API_RETRY_DELAY * 2;

        let attempts = AtomicUsize::new(0);
        let server = spawn_http_server(move |_| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                let retry_body =
                    r#"{"code":301016,"message":"quote invalid, risk management checks failed"}"#
                        .to_string();
                return Some(MockResponse::from(("200 OK", retry_body)));
            }
            // This fits a fresh timeout, but not the time left after the retry backoff.
            Some(MockResponse {
                delay: quote_timeout * 3 / 4,
                ..MockResponse::from(("200 OK", success_body.clone()))
            })
        })
        .await;

        let client = NativeClient::new(
            NativeSupportedChain::Ethereum,
            server.url(),
            "secret_key".to_string(),
            quote_timeout,
        );

        let result = timeout(Duration::from_secs(5), client.request_binding_quote(&params))
            .await
            .expect("Native quote timeout did not terminate the retries");

        assert!(matches!(
            result,
            Err(RFQError::QuoteNotFound(message)) if message.contains("301016")
        ));
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn does_not_retry_documented_authentication_error() {
        let server = spawn_http_server(|_| {
            Some((
                "200 OK",
                r#"{"code":201001,"message":"auth get api key is invalid"}"#.to_string(),
            ))
        })
        .await;
        let client = create_test_client(server.url());

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(result, Err(RFQError::FatalError(_))));
        assert_eq!(server.request_count(), 1);
    }
}
