use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
    time::SystemTime,
};

use alloy::primitives::{utils::keccak256, Address};
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::{interval, timeout, Duration};
use tracing::{error, info, warn};
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use super::models::{NativeOrderbookEntry, NativeOrderbookSide, NativePriceData, NativePriceLevel};
use crate::{
    rfq::{
        client::RFQClient,
        errors::RFQError,
        models::TimestampHeader,
        protocols::{
            native::models::{
                FirmQuoteRequest, FirmQuoteResponse, NativeApiErrorResponse, NativeSupportedChain,
            },
            utils::bytes_to_address,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState},
};

static NATIVE_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);
const MAX_QUOTE_ATTEMPTS: u32 = 3;
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(100);
const NATIVE_API_RETRY_DELAY: Duration = Duration::from_secs(1);
// tradeRFQT(RFQTQuote,uint256,uint256): 4-byte selector plus three ABI head words.
const TRADE_RFQT_SELECTOR: [u8; 4] = [0x09, 0x47, 0xc2, 0xd9];
const MIN_TRADE_RFQT_CALLDATA_LEN: usize = 4 + 3 * 32;
const ACTUAL_SELLER_AMOUNT_OFFSET: usize = 4 + 32;
const ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET: usize = 4 + 2 * 32;

enum QuoteAttemptError {
    Retry { error: RFQError, delay: Duration },
    Fatal(RFQError),
}

impl QuoteAttemptError {
    fn into_error(self) -> RFQError {
        match self {
            Self::Retry { error, .. } | Self::Fatal(error) => error,
        }
    }
}

#[derive(Default)]
struct AggregatedLevels {
    levels: Vec<NativePriceLevel>,
    // Maximum atomic minimum_in_base among the Native entries contributing these levels.
    minimum: f64,
}

impl AggregatedLevels {
    fn extend(&mut self, levels: Vec<NativePriceLevel>, minimum: f64) {
        self.levels.extend(levels);
        self.minimum = self.minimum.max(minimum);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeClient {
    chain: Chain,
    endpoint: String,
    #[serde(skip_serializing, default)]
    api_key: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_tokens: HashSet<Bytes>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl NativeClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:native";
    pub const DEFAULT_ENDPOINT: &'static str = "https://v2.api.native.org/swap-api-v2/v1";

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

    pub fn new(
        chain: Chain,
        api_key: String,
        tokens: HashSet<Bytes>,
        tvl: f64,
        quote_tokens: HashSet<Bytes>,
        poll_time: Duration,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        NativeSupportedChain::try_from(chain).map_err(RFQError::InvalidInput)?;
        if poll_time.is_zero() {
            return Err(RFQError::InvalidInput(
                "Native polling interval must be greater than zero".to_string(),
            ))
        }
        Ok(Self {
            chain,
            endpoint: Self::DEFAULT_ENDPOINT.to_string(),
            api_key,
            tokens,
            tvl,
            quote_tokens,
            poll_time,
            quote_timeout,
        })
    }

    fn select_tvl_conversion_book<'a>(
        &self,
        quote_address: &Bytes,
        books: &'a HashMap<String, NativePriceData>,
    ) -> Option<&'a NativePriceData> {
        books
            .values()
            .filter(|candidate| {
                // `group_orderbook` keeps the configured quote token on the quote side, so every
                // matching conversion book is valued in comparable approved-token units.
                candidate.base_address == *quote_address &&
                    self.quote_tokens
                        .contains(&candidate.quote_address)
            })
            .filter_map(|candidate| {
                candidate
                    .calculate_tvl(None)
                    .map(|liquidity| (candidate, liquidity))
            })
            .max_by(|(candidate_a, liquidity_a), (candidate_b, liquidity_b)| {
                liquidity_a
                    .total_cmp(liquidity_b)
                    // Prefer the smaller token address when liquidity is equal so the result does
                    // not depend on HashMap or HashSet iteration order.
                    .then_with(|| {
                        candidate_b
                            .quote_address
                            .as_ref()
                            .cmp(candidate_a.quote_address.as_ref())
                    })
            })
            .map(|(candidate, _)| candidate)
    }

    fn create_component_with_state(
        &self,
        component_id: String,
        tokens: Vec<Bytes>,
        book: NativePriceData,
        tvl: f64,
    ) -> ComponentWithState {
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "native_relay_pool".to_string(),
            chain: self.chain,
            tokens,
            contract_addresses: vec![],
            static_attributes: Default::default(),
            change: Default::default(),
            creation_tx: Default::default(),
            created_at: Default::default(),
        };

        let mut attributes = HashMap::new();

        let book_json = serde_json::to_string(&book).unwrap_or_default();
        attributes.insert("book".to_string(), book_json.as_bytes().to_vec().into());

        ComponentWithState {
            state: ProtocolComponentState::new(&component_id, attributes, HashMap::new()),
            component: protocol_component,
            component_tvl: Some(tvl),
            entrypoints: vec![],
        }
    }

    async fn fetch_orderbook(&self) -> Result<Vec<NativeOrderbookEntry>, RFQError> {
        let chain = NativeSupportedChain::try_from(self.chain).map_err(RFQError::InvalidInput)?;
        let response = NATIVE_HTTP_CLIENT
            .get(format!("{}/orderbook", self.endpoint))
            // `showNative` is not boolean: its value selects the address used for native-token
            // books. Request address(0) so the response matches Tycho's internal representation.
            .query(&[("chain", chain.as_str()), ("showNative", "0x0")])
            .header("accept", "application/json")
            .header("apikey", &self.api_key)
            .send()
            .await
            .map_err(|e| RFQError::ConnectionError(e.to_string()))?;

        let status = response.status();
        let response_body = response
            .bytes()
            .await
            .map_err(|e| RFQError::ConnectionError(e.to_string()))?;

        // Native can return an API error envelope with HTTP 200.
        if let Ok(api_error) = serde_json::from_slice::<NativeApiErrorResponse>(&response_body) {
            return Err(Self::classify_api_error(&api_error).into_error());
        }

        if !status.is_success() {
            return Err(RFQError::ConnectionError(format!(
                "Native Relay orderbook HTTP error {}: {}",
                status,
                String::from_utf8_lossy(&response_body)
            )));
        }

        serde_json::from_slice(&response_body).map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse Native Relay orderbook: {e}"))
        })
    }

    fn group_orderbook(
        &self,
        entries: Vec<NativeOrderbookEntry>,
    ) -> HashMap<String, NativePriceData> {
        let mut entries_by_pair: HashMap<(Bytes, Bytes), Vec<NativeOrderbookEntry>> =
            HashMap::new();

        for entry in entries {
            let pair = if entry.base_address.as_ref() <= entry.quote_address.as_ref() {
                (entry.base_address.clone(), entry.quote_address.clone())
            } else {
                (entry.quote_address.clone(), entry.base_address.clone())
            };
            entries_by_pair
                .entry(pair)
                .or_default()
                .push(entry);
        }

        let mut books = HashMap::new();
        for ((token0, token1), entries) in entries_by_pair {
            // Keep the book direction stable even when Native publishes only one direction of a
            // pair.
            // Prefer exactly one configured quote token; if both or neither are configured, use
            // the sorted pair order established by the grouping key above.
            let token0_is_quote = self.quote_tokens.contains(&token0);
            let token1_is_quote = self.quote_tokens.contains(&token1);
            let (base_address, quote_address) = match (token0_is_quote, token1_is_quote) {
                (true, false) => (token1.clone(), token0.clone()),
                (false, true) => (token0.clone(), token1.clone()),
                (true, true) | (false, false) => (token0.clone(), token1.clone()),
            };
            let mut direct_bids = AggregatedLevels::default();
            let mut direct_asks = AggregatedLevels::default();
            let mut mirrored_bids = AggregatedLevels::default();
            let mut mirrored_asks = AggregatedLevels::default();

            // Native's minimum_in_base is always denominated in entry.base_address. For a bid the
            // taker sells base, so it is an input minimum; for an ask the taker receives base, so
            // it is an output minimum. Mirroring swaps bid/ask and remaps that minimum into the
            // canonical direction.
            for entry in entries {
                // A zero-only direct entry must not suppress usable mirrored liquidity for the
                // same side. NativeState also filters zero quantities as a defensive measure for
                // deserialized states that do not pass through this grouping path.
                let levels: Vec<_> = entry
                    .levels
                    .into_iter()
                    .filter(|level| level.quantity != 0.0)
                    .collect();
                if levels.is_empty() {
                    continue
                }

                let is_direct =
                    entry.base_address == base_address && entry.quote_address == quote_address;
                if is_direct {
                    match entry.side {
                        NativeOrderbookSide::Bid => {
                            direct_bids.extend(levels, entry.minimum_in_base)
                        }
                        NativeOrderbookSide::Ask => {
                            direct_asks.extend(levels, entry.minimum_in_base)
                        }
                    }
                } else {
                    let levels = NativePriceData::invert_price_levels(&levels);
                    match entry.side {
                        NativeOrderbookSide::Bid => {
                            mirrored_asks.extend(levels, entry.minimum_in_base)
                        }
                        NativeOrderbookSide::Ask => {
                            mirrored_bids.extend(levels, entry.minimum_in_base)
                        }
                    }
                }
            }

            // Use mirrored levels only when direct ones are absent to avoid double-counting. Keep
            // the minimum from the selected representation so discarded levels cannot constrain
            // the surviving side.
            let (bids, minimum_in_base, minimum_out_quote) = if direct_bids.levels.is_empty() {
                (mirrored_bids.levels, 0.0, mirrored_bids.minimum)
            } else {
                (direct_bids.levels, direct_bids.minimum, 0.0)
            };
            let (asks, minimum_in_quote, minimum_out_base) = if direct_asks.levels.is_empty() {
                (mirrored_asks.levels, mirrored_asks.minimum, 0.0)
            } else {
                (direct_asks.levels, 0.0, direct_asks.minimum)
            };
            // Use the sorted pair key so the component ID remains stable if Native returns the
            // opposite book direction in a later poll.
            let pair = format!("native_{}/{}", hex::encode(&token0), hex::encode(&token1));
            let component_id = keccak256(pair.as_bytes()).to_string();
            books.insert(
                component_id,
                NativePriceData {
                    base_address,
                    quote_address,
                    minimum_in_base,
                    minimum_in_quote,
                    minimum_out_base,
                    minimum_out_quote,
                    bids,
                    asks,
                },
            );
        }

        books
    }

    fn process_quote_response(
        quote_response: FirmQuoteResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        // 1. Check API-level success
        if !quote_response.success {
            return Err(RFQError::QuoteNotFound(format!(
                "Native Relay quote request failed: {}",
                quote_response.error_message
            )));
        }

        // Ensure we actually got an order
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
        let seller_token = bytes_to_address(&params.token_in)?;
        let buyer_token = bytes_to_address(&params.token_out)?;
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

        let receiver = bytes_to_address(&params.receiver)?;
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

        // Security: reject already-expired quotes before we bother building a SignedQuote
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
        // Decode calldata (pre-built by Native Relay, ready to submit as-is)
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
                "Unexpected Native V4 selector: expected 0x{}, got 0x{}",
                hex::encode(TRADE_RFQT_SELECTOR),
                hex::encode(&calldata[..TRADE_RFQT_SELECTOR.len()]),
            )));
        }

        // These offsets are fixed by the V4 tradeRFQT(RFQTQuote,uint256,uint256)
        // ABI. Rejecting any other value catches an incompatible or malformed
        // API response before it reaches the encoder; the executor independently
        // hardcodes the same positions rather than trusting route data.
        if quote_response.amount_in_offset as usize != ACTUAL_SELLER_AMOUNT_OFFSET ||
            quote_response.amount_out_minimum_offset as usize != ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET
        {
            return Err(RFQError::ParsingError(format!(
                "Unexpected Native V4 override offsets: expected {}/{} but got {}/{}",
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

    async fn try_quote(
        &self,
        request_data: &FirmQuoteRequest,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, QuoteAttemptError> {
        let response = NATIVE_HTTP_CLIENT
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
}

#[async_trait]
impl RFQClient for NativeClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_components: HashMap<String, ComponentWithState> = HashMap::new();
            let mut ticker = interval(client.poll_time);

            loop {
                ticker.tick().await;

                // Native Relay publishes a complete RFQ orderbook once per request. Polling the
                // full book keeps component creation/removal deterministic and avoids per-pair REST
                // fan-out.
                let books = match client.fetch_orderbook().await {
                    Ok(entries) => client.group_orderbook(entries),
                    Err(e) => {
                        error!("Failed to fetch Native Relay orderbook: {}", e);
                        continue;
                    }
                };

                let mut new_components = HashMap::new();

                for (component_id, book) in &books {
                    // Keep unrequested books available for TVL conversion, but only emit requested
                    // markets as components.
                    if !client.tokens.contains(&book.base_address) ||
                        !client.tokens.contains(&book.quote_address)
                    {
                        continue;
                    }

                    let quote_price_data = if client.quote_tokens.contains(&book.quote_address) {
                        None
                    } else {
                        // TVL thresholds are applied in approved quote-token units. If Native
                        // quotes this market against another token, normalize through the most
                        // liquid available approved quote-token market before filtering.
                        client.select_tvl_conversion_book(&book.quote_address, &books)
                    };

                    if !client.quote_tokens.contains(&book.quote_address) &&
                        quote_price_data.is_none()
                    {
                        continue;
                    }

                    let Some(incoming_tvl) = book.calculate_tvl(quote_price_data) else {
                        warn!("Skipping Native Relay market {component_id} because its TVL is unavailable or non-finite");
                        continue;
                    };

                    if incoming_tvl < client.tvl {
                        info!("Filtering out Native Relay market {} due to low TVL: {:.2} < {:.2}", component_id, incoming_tvl, client.tvl);
                        continue;
                    }

                    let tokens = vec![book.base_address.clone(), book.quote_address.clone()];
                    let component_with_state = client.create_component_with_state(
                        component_id.clone(),
                        tokens,
                        book.clone(),
                        incoming_tvl,
                    );
                    new_components.insert(component_id.clone(), component_with_state);
                }

                // Emit removals for markets that disappeared from the Relay orderbook or no longer
                // pass token/TVL filtering.
                let removed_components: HashMap<String, ProtocolComponent> = current_components
                    .iter()
                    .filter(|&(id, _)| !new_components.contains_key(id))
                    .map(|(k, v)| (k.clone(), v.component.clone()))
                    .collect();

                current_components = new_components.clone();

                let snapshot = Snapshot {
                    states: new_components,
                    vm_storage: HashMap::new(),
                };

                // Native is off-chain and timestamped, not block-based. Downstream decoders use
                // this wall-clock header to build a normal Tycho state update.
                let timestamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                let msg = StateSyncMessage::<TimestampHeader> {
                    header: TimestampHeader { timestamp },
                    snapshots: snapshot,
                    deltas: None,
                    removed_components,
                };

                yield Ok(("native".to_string(), msg));
            }
        })
    }

    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let receiver = bytes_to_address(&params.receiver)?;
        let token_in = bytes_to_address(&params.token_in)?;
        let token_out = bytes_to_address(&params.token_out)?;

        let chain = NativeSupportedChain::try_from(self.chain).map_err(RFQError::FatalError)?;

        let request_data = FirmQuoteRequest {
            src_chain: chain,
            dst_chain: chain,
            from_address: receiver.to_string(),
            amount_wei: params.amount_in.to_string(),
            token_in: token_in.to_string(),
            token_out: token_out.to_string(),
            version: 4,
            allow_multihop: false, // Multihop not implemented yet
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
                            "Native quote attempt {}/{} failed: {}",
                            attempt, MAX_QUOTE_ATTEMPTS, error
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

        // Bind the timeout result before inspecting last_error so the attempts future—and its
        // mutable borrow—has been dropped.
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
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use futures::StreamExt;
    use rstest::rstest;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::native::client_builder::NativeClientBuilder;

    fn successful_quote_json(amount_in: &str) -> serde_json::Value {
        let calldata = format!("0x0947c2d9{:064x}{:064x}{:064x}", 0x60u8, 0u8, 0u8);
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
                "target": "0x8a2ddc0461Fcf96F81a05529Bed540d4f1eb2a00",
                "calldata": calldata,
                "value": "0"
            },
            "source": [6],
            "errorMessage": "",
            "router_version": "4",
            "toWrap": false,
            "toUnwrap": false,
            "amountInOffset": 36,
            "amountOutMinimumOffset": 68
        })
    }

    fn successful_quote_response(amount_in: &str) -> FirmQuoteResponse {
        serde_json::from_value(successful_quote_json(amount_in)).unwrap()
    }

    fn conversion_book(
        base_address: Bytes,
        quote_address: Bytes,
        quantity: f64,
        price: f64,
    ) -> NativePriceData {
        NativePriceData {
            base_address,
            quote_address,
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity, price }],
            asks: vec![],
        }
    }

    #[test]
    fn test_native_client_serialization() {
        let mut tokens = HashSet::new();
        tokens.insert(Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap());
        tokens.insert(Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap());

        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            tokens,
            10.0,
            HashSet::new(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let serialized = serde_json::to_string(&client).unwrap();
        let deserialized: NativeClient = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.chain, client.chain);
        assert_eq!(deserialized.endpoint, client.endpoint);
        assert_eq!(deserialized.tokens, client.tokens);
        assert_eq!(deserialized.tvl, client.tvl);
        assert!(deserialized.api_key.is_empty());
    }

    #[test]
    fn rejects_unsupported_chain_at_construction() {
        let result = NativeClient::new(
            Chain::Polygon,
            "test-api-key".to_string(),
            HashSet::new(),
            0.0,
            HashSet::new(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        );

        assert!(matches!(result, Err(RFQError::InvalidInput(_))));
    }

    #[test]
    fn builder_rejects_zero_poll_time() {
        let result = NativeClientBuilder::new(Chain::Ethereum, "test-api-key".to_string())
            .poll_time(Duration::ZERO)
            .build();

        assert!(matches!(
            result,
            Err(RFQError::InvalidInput(message))
                if message == "Native polling interval must be greater than zero"
        ));
    }

    #[test]
    fn selects_most_liquid_tvl_conversion_book() {
        let weth = Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap();
        let usdc = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let usdt = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let wbtc = Bytes::from_str("0x4444444444444444444444444444444444444444").unwrap();
        let unapproved = Bytes::from_str("0x5555555555555555555555555555555555555555").unwrap();
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([
                weth.clone(),
                usdc.clone(),
                usdt.clone(),
                wbtc.clone(),
                unapproved.clone(),
            ]),
            0.0,
            HashSet::from([usdc.clone(), usdt.clone()]),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();
        let books = HashMap::from([
            ("usdc".to_string(), conversion_book(weth.clone(), usdc.clone(), 1.0, 100.0)),
            ("usdt".to_string(), conversion_book(weth.clone(), usdt.clone(), 2.0, 100.0)),
            ("unrelated".to_string(), conversion_book(wbtc, usdc, 1_000.0, 100.0)),
            ("unapproved".to_string(), conversion_book(weth.clone(), unapproved, 2_000.0, 100.0)),
        ]);

        let selected = client
            .select_tvl_conversion_book(&weth, &books)
            .expect("one conversion book");

        assert_eq!(selected.quote_address, usdt);
    }

    #[test]
    fn selects_lower_quote_address_for_equal_tvl_conversion_liquidity() {
        let weth = Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap();
        let lower_quote = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let higher_quote = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([weth.clone(), lower_quote.clone(), higher_quote.clone()]),
            0.0,
            HashSet::from([lower_quote.clone(), higher_quote.clone()]),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();
        let books = HashMap::from([
            ("higher".to_string(), conversion_book(weth.clone(), higher_quote, 2.0, 100.0)),
            ("lower".to_string(), conversion_book(weth.clone(), lower_quote.clone(), 1.0, 200.0)),
        ]);

        let selected = client
            .select_tvl_conversion_book(&weth, &books)
            .expect("one conversion book");

        assert_eq!(selected.quote_address, lower_quote);
    }

    #[test]
    fn creates_indexer_compatible_component_from_relay_orderbook() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdt = Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap();
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([weth.clone(), usdt.clone()]),
            0.0,
            HashSet::from([usdt.clone()]),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let books = client.group_orderbook(vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 0.0,
                side: NativeOrderbookSide::Bid,
                levels: vec![NativePriceLevel { quantity: 0.0001, price: 3213.12345 }],
            },
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 0.0,
                side: NativeOrderbookSide::Ask,
                levels: vec![NativePriceLevel { quantity: 2.0, price: 3214.0 }],
            },
            NativeOrderbookEntry {
                base_address: usdt.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Bid,
                levels: vec![NativePriceLevel { quantity: 6428.0, price: 1.0 / 3214.0 }],
            },
            NativeOrderbookEntry {
                base_address: usdt.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Ask,
                levels: vec![NativePriceLevel { quantity: 0.321312345, price: 1.0 / 3213.12345 }],
            },
        ]);

        let (component_id, book) = books
            .into_iter()
            .next()
            .expect("one grouped book");
        let component = client.create_component_with_state(
            component_id.clone(),
            vec![book.base_address.clone(), book.quote_address.clone()],
            book.clone(),
            book.calculate_tvl(None)
                .expect("TVL should be finite"),
        );

        assert_eq!(component.component.id, component_id);
        assert_eq!(component.component.protocol_system, NativeClient::PROTOCOL_SYSTEM);
        assert_eq!(component.component.protocol_type_name, "native_relay_pool");
        assert_eq!(component.component.tokens, vec![weth, usdt]);
        assert_eq!(component.state.component_id, component_id);

        let encoded_book = component
            .state
            .attributes
            .get("book")
            .expect("book attribute");
        let decoded_book: NativePriceData = serde_json::from_slice(encoded_book).unwrap();
        assert_eq!(decoded_book.bids.len(), 1);
        assert_eq!(decoded_book.asks.len(), 1);
        assert_eq!(decoded_book.bids[0].quantity, 0.0001);
        assert_eq!(decoded_book.bids[0].price, 3213.12345);
    }

    #[test]
    fn uses_stable_component_id_and_direction_when_merging_mirrored_books() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([weth.clone(), usdc.clone()]),
            0.0,
            HashSet::from([usdc.clone()]),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();
        let entries = vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdc.clone(),
                minimum_in_base: 100_000_000_000.0,
                side: NativeOrderbookSide::Bid,
                levels: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
            },
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdc.clone(),
                minimum_in_base: 300_000_000_000.0,
                side: NativeOrderbookSide::Ask,
                levels: vec![NativePriceLevel { quantity: 1.0, price: 2_100.0 }],
            },
            NativeOrderbookEntry {
                base_address: usdc.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Bid,
                levels: vec![NativePriceLevel { quantity: 2_000.0, price: 0.0005 }],
            },
            NativeOrderbookEntry {
                base_address: usdc.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 250.0,
                side: NativeOrderbookSide::Bid,
                levels: vec![NativePriceLevel { quantity: 2_000.0, price: 0.0005 }],
            },
            NativeOrderbookEntry {
                base_address: usdc.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 400.0,
                side: NativeOrderbookSide::Ask,
                levels: vec![NativePriceLevel { quantity: 2.0, price: 0.5 }],
            },
        ];

        let forward_only = client.group_orderbook(entries[..2].to_vec());
        let reverse_entries = entries[2..].to_vec();
        let reverse_only = client.group_orderbook(reverse_entries.clone());
        let reversed_reverse_only = client.group_orderbook(
            reverse_entries
                .into_iter()
                .rev()
                .collect(),
        );
        assert_eq!(reverse_only, reversed_reverse_only);
        let pair = format!("native_{}/{}", hex::encode(&usdc), hex::encode(&weth));
        let component_id = keccak256(pair.as_bytes()).to_string();

        let forward_book = forward_only
            .get(&component_id)
            .expect("forward-only book uses the stable component ID");
        assert_eq!(forward_book.base_address, weth);
        assert_eq!(forward_book.quote_address, usdc);
        assert_eq!(forward_book.minimum_in_base, 100_000_000_000.0);
        assert_eq!(forward_book.minimum_in_quote, 0.0);
        assert_eq!(forward_book.minimum_out_base, 300_000_000_000.0);
        assert_eq!(forward_book.minimum_out_quote, 0.0);

        let reverse_book = reverse_only
            .get(&component_id)
            .expect("reverse-only book uses the stable component ID");
        assert_eq!(reverse_book.base_address, weth);
        assert_eq!(reverse_book.quote_address, usdc);
        assert_eq!(reverse_book.minimum_in_base, 0.0);
        assert_eq!(reverse_book.minimum_in_quote, 250.0);
        assert_eq!(reverse_book.minimum_out_base, 0.0);
        assert_eq!(reverse_book.minimum_out_quote, 400.0);
        assert_eq!(reverse_book.bids, vec![NativePriceLevel { quantity: 1.0, price: 2.0 }]);
        assert_eq!(
            reverse_book.asks,
            vec![
                NativePriceLevel { quantity: 1.0, price: 2_000.0 },
                NativePriceLevel { quantity: 1.0, price: 2_000.0 },
            ]
        );

        let mixed = client.group_orderbook(vec![entries[0].clone(), entries[2].clone()]);
        let mixed_book = mixed.get(&component_id).unwrap();
        assert_eq!(mixed_book.minimum_in_base, 100_000_000_000.0);
        assert_eq!(mixed_book.minimum_in_quote, 100.0);
        assert_eq!(mixed_book.minimum_out_base, 0.0);
        assert_eq!(mixed_book.minimum_out_quote, 0.0);
        assert_eq!(mixed_book.bids, vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }]);
        assert_eq!(mixed_book.asks, vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }]);

        let books = client.group_orderbook(entries.clone());
        let reversed_books = client.group_orderbook(entries.into_iter().rev().collect());

        assert_eq!(books, reversed_books);
        assert_eq!(books.len(), 1);
        let book = books.get(&component_id).unwrap();
        assert_eq!(book.base_address, weth);
        assert_eq!(book.quote_address, usdc);
        assert_eq!(book.minimum_in_base, 100_000_000_000.0);
        assert_eq!(book.minimum_in_quote, 0.0);
        assert_eq!(book.minimum_out_base, 300_000_000_000.0);
        assert_eq!(book.minimum_out_quote, 0.0);
        assert_eq!(book.bids, vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }]);
        assert_eq!(book.asks, vec![NativePriceLevel { quantity: 1.0, price: 2_100.0 }]);
    }

    #[test]
    fn zero_only_direct_side_does_not_suppress_mirrored_liquidity() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([weth.clone(), usdc.clone()]),
            0.0,
            HashSet::from([usdc.clone()]),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let books = client.group_orderbook(vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdc.clone(),
                minimum_in_base: 999.0,
                side: NativeOrderbookSide::Bid,
                levels: vec![NativePriceLevel { quantity: 0.0, price: 2_000.0 }],
            },
            NativeOrderbookEntry {
                base_address: usdc,
                quote_address: weth,
                minimum_in_base: 250.0,
                side: NativeOrderbookSide::Ask,
                levels: vec![NativePriceLevel { quantity: 2.0, price: 0.5 }],
            },
        ]);

        let book = books
            .values()
            .next()
            .expect("one grouped book");
        assert_eq!(book.bids, vec![NativePriceLevel { quantity: 1.0, price: 2.0 }]);
        assert_eq!(book.minimum_in_base, 0.0);
        assert_eq!(book.minimum_out_quote, 250.0);
    }

    #[rstest]
    #[case::token0_only(true, false, true)]
    #[case::token1_only(false, true, false)]
    #[case::both_tokens(true, true, false)]
    #[case::neither_token(false, false, false)]
    fn selects_stable_direction_for_quote_token_preferences(
        #[case] token0_is_quote: bool,
        #[case] token1_is_quote: bool,
        #[case] expected_quote_is_token0: bool,
    ) {
        let token0 = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let token1 = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        assert!(token0.as_ref() < token1.as_ref());

        let mut quote_tokens = HashSet::new();
        if token0_is_quote {
            quote_tokens.insert(token0.clone());
        }
        if token1_is_quote {
            quote_tokens.insert(token1.clone());
        }
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([token0.clone(), token1.clone()]),
            0.0,
            quote_tokens,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        // Native returned only the direction opposite to the sorted fallback.
        let books = client.group_orderbook(vec![NativeOrderbookEntry {
            base_address: token1.clone(),
            quote_address: token0.clone(),
            minimum_in_base: 0.0,
            side: NativeOrderbookSide::Bid,
            levels: vec![NativePriceLevel { quantity: 1.0, price: 1.0 }],
        }]);

        let book = books
            .values()
            .next()
            .expect("one grouped book");
        let (expected_base, expected_quote) =
            if expected_quote_is_token0 { (token1, token0) } else { (token0, token1) };
        assert_eq!(book.base_address, expected_base);
        assert_eq!(book.quote_address, expected_quote);
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
        let response = successful_quote_response("2");

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("input amount mismatch")
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
        response.tx_request.calldata = format!("0x0947c2d9{}", "00".repeat(95));

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
        calldata[0] ^= 0xff;
        response.tx_request.calldata = format!("0x{}", hex::encode(calldata));

        let result = NativeClient::process_quote_response(response, &params);

        assert!(matches!(
            result,
            Err(RFQError::ParsingError(message)) if message.contains("selector")
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
                "Unexpected Native V4 override offsets"
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

    #[test]
    fn accepts_native_eth_orderbook_using_zero_address() {
        let tycho_native_eth = Bytes::zero(20);
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::from([tycho_native_eth.clone(), usdc.clone()]),
            0.0,
            HashSet::from([usdc.clone()]),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let entry: NativeOrderbookEntry = serde_json::from_value(serde_json::json!({
            "base_address": tycho_native_eth.to_string(),
            "quote_address": usdc.to_string(),
            "minimum_in_base": 1.0,
            "side": "bid",
            "levels": [[1.0, 3_000.0]]
        }))
        .unwrap();
        let books = client.group_orderbook(vec![entry]);

        let book = books.values().next().unwrap();
        assert_eq!(book.base_address, tycho_native_eth);
    }

    fn create_test_client(endpoint: String) -> NativeClient {
        let mut client = NativeClient::new(
            Chain::Ethereum,
            "test-api-key".to_string(),
            HashSet::new(),
            0.0,
            HashSet::new(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();
        client.endpoint = endpoint;
        client
    }

    #[tokio::test]
    async fn requests_native_token_orderbooks() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .unwrap();
            let mut stream = reader.into_inner();
            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]";
            stream
                .write_all(response.as_bytes())
                .await
                .unwrap();
            request_line
        });
        let client = create_test_client(format!("http://{address}"));

        let orderbook = client.fetch_orderbook().await.unwrap();
        let request = server.await.unwrap();

        assert!(orderbook.is_empty());
        assert!(request.starts_with("GET /orderbook?"));
        assert!(request.contains("chain=ethereum"));
        assert!(request.contains("showNative=0x0"));
    }

    #[rstest]
    #[case::with_conversion_helper(true, 300.0, Some(400.0))]
    #[case::without_conversion_helper(false, 300.0, None)]
    #[case::below_normalized_tvl_threshold(true, 401.0, None)]
    #[tokio::test]
    async fn stream_uses_unrequested_books_only_for_tvl_conversion(
        #[case] include_helper: bool,
        #[case] tvl_threshold: f64,
        #[case] expected_tvl: Option<f64>,
    ) {
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let usdt = Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let mut entries = vec![serde_json::json!({
            "base_address": weth.to_string(),
            "quote_address": usdt.to_string(),
            "minimum_in_base": 0.0,
            "side": "bid",
            "levels": [[2.0, 100.0]]
        })];
        if include_helper {
            // Use a reversed helper with a non-unit price so the test distinguishes the market's
            // 200 USDT of liquidity from its normalized value of 400 USDC.
            entries.push(serde_json::json!({
                "base_address": usdc.to_string(),
                "quote_address": usdt.to_string(),
                "minimum_in_base": 0.0,
                "side": "bid",
                "levels": [[1_000.0, 0.5]]
            }));
        }
        let (address, _) =
            create_quote_server("200 OK", serde_json::to_string(&entries).unwrap()).await;
        let mut client = create_test_client(format!("http://{address}"));
        client.tokens = HashSet::from([weth.clone(), usdt.clone()]);
        client.quote_tokens = HashSet::from([usdc]);
        client.tvl = tvl_threshold;

        let (_, update) = timeout(Duration::from_secs(5), client.stream().next())
            .await
            .expect("orderbook poll timed out")
            .expect("stream ended")
            .expect("orderbook poll failed");

        if let Some(tvl) = expected_tvl {
            assert_eq!(update.snapshots.states.len(), 1, "helper must not be emitted");
            let component = update
                .snapshots
                .states
                .values()
                .next()
                .unwrap();
            assert_eq!(component.component.tokens, vec![weth, usdt]);
            assert_eq!(component.component_tvl, Some(tvl));
        } else {
            assert!(update.snapshots.states.is_empty());
        }
    }

    async fn create_quote_server(
        final_status: impl Into<String>,
        final_body: impl Into<String>,
    ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let final_status = final_status.into();
        let final_body = final_body.into();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_request_count = request_count.clone();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                server_request_count.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 {final_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{final_body}",
                    final_body.len()
                );
                let _ = stream
                    .write_all(response.as_bytes())
                    .await;
                let _ = stream.shutdown().await;
            }
        });

        (address, request_count)
    }

    async fn create_hanging_quote_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        address
    }

    #[tokio::test]
    async fn handles_orderbook_api_error_with_http_200() {
        let (address, request_count) = create_quote_server(
            "200 OK",
            r#"{"code":171015,"message":"quoted token not available"}"#,
        )
        .await;
        let client = create_test_client(format!("http://{address}"));

        let result = client.fetch_orderbook().await;

        assert!(matches!(
            result,
            Err(RFQError::QuoteNotFound(message)) if message.contains("171015")
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn handles_documented_quote_error_without_retrying() {
        let (address, request_count) = create_quote_server(
            "200 OK",
            r#"{"code":171015,"message":"quoted token not available"}"#,
        )
        .await;
        let client = create_test_client(format!("http://{address}"));

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
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn handles_success_false_as_quote_not_found_without_retrying() {
        let mut response = successful_quote_json("1");
        response["success"] = serde_json::Value::Bool(false);
        response["errorMessage"] = serde_json::Value::String("quote unavailable".to_string());
        let (address, request_count) = create_quote_server("200 OK", response.to_string()).await;
        let client = create_test_client(format!("http://{address}"));

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(
            result,
            Err(RFQError::QuoteNotFound(message)) if message.contains("quote unavailable")
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_documented_temporary_api_error() {
        let (address, request_count) = create_quote_server(
            "200 OK",
            r#"{"code":301016,"message":"quote invalid, risk management checks failed"}"#,
        )
        .await;
        let client = create_test_client(format!("http://{address}"));

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(result, Err(RFQError::QuoteNotFound(_))));
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_server_error_without_native_error_envelope() {
        let (address, request_count) =
            create_quote_server("503 Service Unavailable", "<html>upstream unavailable</html>")
                .await;
        let client = create_test_client(format!("http://{address}"));

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(
            result,
            Err(RFQError::ConnectionError(message)) if message.contains("503 Service Unavailable")
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_malformed_success_response() {
        let (address, request_count) =
            create_quote_server("200 OK", r#"{"unexpected":true}"#).await;
        let client = create_test_client(format!("http://{address}"));

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(result, Err(RFQError::ParsingError(_))));
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn applies_one_timeout_to_the_complete_quote_request() {
        let address = create_hanging_quote_server().await;
        let mut client = create_test_client(format!("http://{address}"));
        client.quote_timeout = Duration::from_millis(50);

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
    async fn does_not_retry_documented_authentication_error() {
        let (address, request_count) = create_quote_server(
            "200 OK",
            r#"{"code":201001,"message":"auth get api key is invalid"}"#,
        )
        .await;
        let client = create_test_client(format!("http://{address}"));

        let result = client
            .request_binding_quote(&create_test_quote_params())
            .await;

        assert!(matches!(result, Err(RFQError::FatalError(_))));
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }
}
