//! Head-triggered full-block helper snapshots outside the Substreams execution path.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::{Duration, SystemTime},
};

use alloy::{
    primitives::{Address, U256},
    sol_types::SolCall,
};
use futures::{stream::BoxStream, StreamExt};
use num_bigint::BigUint;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{interval, sleep, timeout, MissedTickBehavior},
};
use tycho_common::{
    models::{protocol::GetAmountOutParams, token::Token, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use super::{
    heads::{head_events, HeadEvent},
    models::{
        decode_orderbooks, ensure_timestamp_fresh, getAllOrderbooksCall, getAmountOutCall,
        helper_for, pricing_for, router_for, validate_address, Snapshot as OrderbookSnapshot,
        PARTNER_ID,
    },
    state::ElfomofiState,
    tokens, ElfomofiBlock,
};
use crate::{
    book::{Book, BookSnapshot},
    protocol::models::ProtocolComponent,
    snapshot_feed::errors::FeedError,
};

const MAX_RPC_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

struct FetchedComponents {
    anchor: ElfomofiBlock,
    components: HashMap<String, Book>,
    new_tokens: HashMap<Bytes, Token>,
}

fn publication(
    current: &mut HashMap<String, Book>,
    known_tokens: &mut HashMap<Bytes, Token>,
    fetched: FetchedComponents,
) -> Result<BookSnapshot<ElfomofiBlock>, FeedError> {
    *current = fetched.components.clone();
    known_tokens.extend(fetched.new_tokens);
    Ok(BookSnapshot { anchor: fetched.anchor, books: Arc::new(fetched.components) })
}

/// Fetches all orderbooks when a subscribed head arrives, with optional HTTP polling fallback.
///
/// The execution quote method refreshes an unsigned on-chain quote. ElFomoFi does not reserve
/// liquidity or promise execution at that price; route slippage and deadline checks still apply.
#[derive(Clone)]
pub struct ElfomofiClient {
    chain: Chain,
    rpc_url: String,
    ws_rpc_url: Option<String>,
    http_client: Client,
    tokens: HashSet<Bytes>,
    poll_time: Duration,
    quote_timeout: Duration,
    max_age_seconds: u64,
}

impl fmt::Debug for ElfomofiClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ElfomofiClient")
            .field("chain", &self.chain)
            .field("rpc_url", &"[redacted]")
            .field(
                "ws_rpc_url",
                &self
                    .ws_rpc_url
                    .as_ref()
                    .map(|_| "[redacted]"),
            )
            .field("tokens", &self.tokens)
            .field("poll_time", &self.poll_time)
            .field("quote_timeout", &self.quote_timeout)
            .field("max_age_seconds", &self.max_age_seconds)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RpcHeader {
    pub(super) number: u64,
    pub(super) hash: Bytes,
    pub(super) timestamp: u64,
}

/// Only the newest head is retained, even while a slow helper call or consumer is pending.
#[derive(Clone, Default)]
struct HeadUpdates {
    epoch: u64,
    version: u64,
    connected: bool,
    reorg_version: u64,
    head: Option<RpcHeader>,
}

struct HeadPump(JoinHandle<()>);

impl Drop for HeadPump {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn pump_heads(
    mut events: BoxStream<'static, Result<HeadEvent, FeedError>>,
) -> (watch::Receiver<HeadUpdates>, HeadPump) {
    let (sender, receiver) = watch::channel(HeadUpdates::default());
    let task = tokio::spawn(async move {
        let mut updates = HeadUpdates::default();
        while let Some(event) = events.next().await {
            updates.version = updates.version.wrapping_add(1);
            match event {
                Ok(HeadEvent::Connected) => {
                    updates.epoch = updates.epoch.wrapping_add(1);
                    updates.connected = true;
                    updates.head = None;
                }
                Ok(HeadEvent::Head(header)) => {
                    if updates
                        .head
                        .as_ref()
                        .is_some_and(|previous| {
                            header.number <= previous.number && header != *previous
                        })
                    {
                        // Keep the invalidation even if a subsequent higher head overwrites the
                        // replacement in the bounded latest-head slot.
                        updates.reorg_version = updates.reorg_version.wrapping_add(1);
                    }
                    updates.head = Some(header);
                }
                Err(error) => {
                    tracing::warn!("ElFomoFi head subscription failed: {error}");
                    updates.connected = false;
                    updates.head = None;
                }
            }
            if sender.send(updates.clone()).is_err() {
                return;
            }
        }
        updates.version = updates.version.wrapping_add(1);
        updates.connected = false;
        updates.head = None;
        let _ = sender.send(updates);
    });
    (receiver, HeadPump(task))
}

fn expiry_delay(header: Option<&RpcHeader>, max_age_seconds: u64) -> Duration {
    header
        .and_then(|header| {
            header
                .timestamp
                .checked_add(max_age_seconds)
        })
        .and_then(|expiry| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(expiry)))
        .and_then(|expiry| {
            expiry
                .duration_since(SystemTime::now())
                .ok()
        })
        .unwrap_or(Duration::ZERO)
}

/// Base can announce a head milliseconds before its integer-second timestamp reaches the local
/// clock. Wait only for that next-second boundary, without allowing future timestamps into state.
async fn await_head_timestamp(timestamp: u64, max_age_seconds: u64) -> Result<(), FeedError> {
    let now = SystemTime::now();
    let since_epoch = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| FeedError::Fatal("System clock precedes UNIX epoch".into()))?;
    if timestamp > since_epoch.as_secs() {
        if timestamp > since_epoch.as_secs().saturating_add(1) {
            return ensure_timestamp_fresh(timestamp, since_epoch.as_secs(), max_age_seconds);
        }
        let boundary = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(timestamp))
            .ok_or_else(|| FeedError::Parsing("ElFomoFi head timestamp overflow".into()))?;
        if let Ok(delay) = boundary.duration_since(now) {
            sleep(delay).await;
        }
    }
    ensure_timestamp_fresh(timestamp, unix_now()?, max_age_seconds)
}

fn withdrawal(
    current: &mut HashMap<String, Book>,
) -> Result<BookSnapshot<ElfomofiBlock>, FeedError> {
    current.clear();
    Ok(BookSnapshot { anchor: ElfomofiBlock::default(), books: Arc::new(HashMap::new()) })
}

impl ElfomofiClient {
    pub const PROTOCOL_SYSTEM: &'static str = "elfomofi";

    pub fn new(
        chain: Chain,
        rpc_url: String,
        tokens: HashSet<Bytes>,
        poll_time: Duration,
        quote_timeout: Duration,
        max_age: Duration,
    ) -> Result<Self, FeedError> {
        helper_for(chain)?;
        router_for(chain)?;
        let url = reqwest::Url::parse(&rpc_url)
            .map_err(|_| FeedError::InvalidInput("Invalid ElFomoFi RPC URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(FeedError::InvalidInput("ElFomoFi RPC URL must use HTTP or HTTPS".into()));
        }
        if poll_time.is_zero() || quote_timeout.is_zero() || max_age.as_secs() == 0 {
            return Err(FeedError::InvalidInput(
                "ElFomoFi durations must be positive (TTL >= 1s)".into(),
            ));
        }
        for token in &tokens {
            validate_address(token)?;
        }
        let http_client = Client::builder()
            .build()
            .map_err(|error| FeedError::Connection(error.without_url().to_string()))?;
        Ok(Self {
            chain,
            rpc_url,
            ws_rpc_url: None,
            http_client,
            tokens,
            poll_time,
            quote_timeout,
            max_age_seconds: max_age.as_secs(),
        })
    }

    /// Uses `eth_subscribe("newHeads")` to start each helper read as soon as the head arrives.
    /// HTTP remains the transport for the pinned helper and execution-quote calls.
    pub fn with_ws_rpc_url(mut self, url: impl Into<String>) -> Result<Self, FeedError> {
        let url = url.into();
        let parsed = reqwest::Url::parse(&url)
            .map_err(|_| FeedError::InvalidInput("Invalid ElFomoFi WebSocket RPC URL".into()))?;
        if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
            return Err(FeedError::InvalidInput(
                "ElFomoFi WebSocket RPC URL must use WS or WSS".into(),
            ));
        }
        self.ws_rpc_url = Some(url);
        Ok(self)
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, FeedError> {
        let operation = async {
            let mut response = self
                .http_client
                .post(&self.rpc_url)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .send()
                .await
                .map_err(|error| FeedError::Connection(error.without_url().to_string()))?;
            if !response.status().is_success() {
                return Err(FeedError::Connection(format!(
                    "ElFomoFi RPC {method} returned HTTP {}",
                    response.status()
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| FeedError::Connection(error.without_url().to_string()))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_RPC_RESPONSE_BYTES {
                    return Err(FeedError::Parsing("ElFomoFi RPC response exceeds 16 MiB".into()));
                }
                bytes.extend_from_slice(&chunk);
            }
            let response: Value = serde_json::from_slice(&bytes).map_err(|error| {
                FeedError::Parsing(format!("Invalid ElFomoFi RPC JSON: {error}"))
            })?;
            if response.get("jsonrpc") != Some(&json!("2.0")) ||
                response.get("id") != Some(&json!(1))
            {
                return Err(FeedError::Parsing("Invalid ElFomoFi JSON-RPC envelope".into()));
            }
            if let Some(error) = response.get("error") {
                // Provider messages can contain their credential-bearing URL. Only expose code.
                return Err(FeedError::Connection(format!(
                    "ElFomoFi RPC {method} failed with code {}",
                    error
                        .get("code")
                        .unwrap_or(&Value::Null)
                )));
            }
            response
                .get("result")
                .cloned()
                .filter(|result| !result.is_null())
                .ok_or_else(|| FeedError::Parsing(format!("Missing ElFomoFi RPC {method} result")))
        };
        timeout(self.quote_timeout, operation)
            .await
            .map_err(|_| {
                FeedError::Connection(format!(
                    "ElFomoFi RPC {method} timed out after {}ms",
                    self.quote_timeout.as_millis()
                ))
            })?
    }

    async fn verify_chain(&self) -> Result<(), FeedError> {
        let result = self
            .rpc("eth_chainId", json!([]))
            .await?;
        if parse_quantity(&result, "chain ID")? != self.chain.id() {
            return Err(FeedError::InvalidInput(format!(
                "ElFomoFi RPC must serve {} chain ID {}",
                self.chain,
                self.chain.id()
            )));
        }
        Ok(())
    }

    async fn header(&self, tag: &str) -> Result<RpcHeader, FeedError> {
        let result = self
            .rpc("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        let number = parse_quantity(&result["number"], "block number")?;
        let timestamp = parse_quantity(&result["timestamp"], "block timestamp")?;
        let hash = parse_hex_bytes(&result["hash"], "block hash")?;
        if hash.len() != 32 {
            return Err(FeedError::Parsing("ElFomoFi block hash must have 32 bytes".into()));
        }
        Ok(RpcHeader { number, hash, timestamp })
    }

    pub(super) async fn call_at(
        &self,
        address: &str,
        data: &[u8],
        sender: Option<&Bytes>,
        header: &RpcHeader,
    ) -> Result<Vec<u8>, FeedError> {
        let mut call = json!({"to": address, "data": format!("0x{}", hex::encode(data))});
        if let Some(sender) = sender {
            call["from"] = json!(sender.to_string());
        }
        let result = self
            .rpc("eth_call", json!([call, format!("0x{:x}", header.number)]))
            .await?;
        Ok(parse_hex_bytes(&result, "eth_call output")?.to_vec())
    }

    async fn verify_header(&self, expected: &RpcHeader) -> Result<(), FeedError> {
        let actual = self
            .header(&format!("0x{:x}", expected.number))
            .await?;
        if actual != *expected {
            return Err(FeedError::Connection(
                "ElFomoFi canonical header changed during RPC quote".into(),
            ));
        }
        ensure_timestamp_fresh(expected.timestamp, unix_now()?, self.max_age_seconds)
    }

    async fn read_snapshots_at(
        &self,
        header: &RpcHeader,
    ) -> Result<Vec<OrderbookSnapshot>, FeedError> {
        ensure_timestamp_fresh(header.timestamp, unix_now()?, self.max_age_seconds)?;
        let raw = self
            .call_at(helper_for(self.chain)?, &getAllOrderbooksCall {}.abi_encode(), None, header)
            .await?;
        let mut snapshots = decode_orderbooks(
            self.chain,
            &raw,
            header.number,
            header.hash.clone(),
            header.timestamp,
            unix_now()?,
            self.max_age_seconds,
        )?;
        if !self.tokens.is_empty() {
            snapshots.retain(|snapshot| {
                self.tokens
                    .contains(&snapshot.book.base_token) &&
                    self.tokens
                        .contains(&snapshot.book.quote_token)
            });
        }
        Ok(snapshots)
    }

    /// Fetches every configured pair in a single helper call pinned to one verified full block.
    #[cfg(test)]
    pub async fn fetch_snapshots(&self) -> Result<Vec<OrderbookSnapshot>, FeedError> {
        self.verify_chain().await?;
        let header = self.header("latest").await?;
        let snapshots = self.read_snapshots_at(&header).await?;
        self.verify_header(&header).await?;
        Ok(snapshots)
    }

    async fn fetch_components_at(
        &self,
        header: &RpcHeader,
        known_tokens: &HashMap<Bytes, Token>,
    ) -> Result<FetchedComponents, FeedError> {
        let snapshots = self.read_snapshots_at(header).await?;
        let new_tokens =
            tokens::discover(self, self.chain, header, &snapshots, known_tokens).await?;
        self.verify_header(header).await?;
        let all_tokens = known_tokens
            .iter()
            .chain(new_tokens.iter())
            .map(|(address, token)| (address.clone(), token.clone()))
            .collect();
        let components = snapshots
            .iter()
            .map(|snapshot| {
                let book = self.create_book(snapshot, &all_tokens)?;
                Ok((book.component.id.to_string(), book))
            })
            .collect::<Result<_, FeedError>>()?;
        Ok(FetchedComponents {
            anchor: ElfomofiBlock {
                number: header.number,
                hash: header.hash.clone(),
                timestamp: header.timestamp,
            },
            components,
            new_tokens,
        })
    }

    pub(super) fn create_book(
        &self,
        snapshot: &OrderbookSnapshot,
        tokens: &HashMap<Bytes, Token>,
    ) -> Result<Book, FeedError> {
        snapshot.validate()?;
        if snapshot.chain != self.chain {
            return Err(FeedError::InvalidInput(
                "ElFomoFi snapshot chain differs from client".into(),
            ));
        }
        let pair_tokens = [&snapshot.book.base_token, &snapshot.book.quote_token]
            .into_iter()
            .map(|address| {
                tokens
                    .get(address)
                    .filter(|token| token.chain == self.chain)
                    .cloned()
                    .ok_or_else(|| {
                        FeedError::Parsing(format!(
                            "missing ElFomoFi token metadata on {}: {address}",
                            self.chain
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let router: Bytes = router_for(self.chain)?
            .parse()
            .map_err(|_| FeedError::Fatal("Invalid compiled ElFomoFi router address".into()))?;
        let component = ProtocolComponent::new(
            Bytes::from(
                hex::decode(snapshot.book.component_id(self.chain)?)
                    .map_err(|_| FeedError::Parsing("invalid ElFomoFi component ID".into()))?,
            ),
            Self::PROTOCOL_SYSTEM.into(),
            "elfomofi_pool".into(),
            self.chain,
            pair_tokens,
            vec![router.clone()],
            HashMap::from([("router".into(), router)]),
            Default::default(),
            Default::default(),
        );
        let state = ElfomofiState::from_snapshot(snapshot.clone())
            .map_err(|error| FeedError::Parsing(error.to_string()))?;
        Ok(Book {
            component,
            state: Arc::new(state),
            updated_at: chrono::DateTime::from_timestamp(snapshot.block_timestamp as i64, 0),
        })
    }

    fn stream_from_heads(
        &self,
        events: BoxStream<'static, Result<HeadEvent, FeedError>>,
    ) -> BoxStream<'static, Result<BookSnapshot<ElfomofiBlock>, FeedError>> {
        let client = self.clone();
        Box::pin(async_stream::stream! {
            let (mut heads, _pump) = pump_heads(events);
            let mut current: HashMap<String, Book> = HashMap::new();
            let mut known_tokens = HashMap::new();
            let mut previous_header: Option<RpcHeader> = None;
            let mut last_verified_header: Option<RpcHeader> = None;
            let mut handled_version = 0;
            let mut handled_reorg_version = 0;
            let mut active_epoch = 0;
            let mut verified_epoch = 0;
            loop {
                let updates = heads.borrow_and_update().clone();
                if updates.version == handled_version {
                    // This timer only expires local liquidity. It never polls the HTTP head.
                    tokio::select! {
                        changed = heads.changed() => {
                            if changed.is_err() {
                                if !current.is_empty() {
                                    yield withdrawal(&mut current);
                                }
                                break;
                            }
                        }
                        _ = sleep(expiry_delay(previous_header.as_ref(), client.max_age_seconds)),
                            if !current.is_empty() => {
                            previous_header = None;
                            yield withdrawal(&mut current);
                        }
                    }
                    continue;
                }
                handled_version = updates.version;
                if !updates.connected {
                    previous_header = None;
                    active_epoch = 0;
                    if !current.is_empty() {
                        yield withdrawal(&mut current);
                    }
                    continue;
                }

                let catch_up = active_epoch != updates.epoch;
                active_epoch = updates.epoch;
                let replaces_published = previous_header.as_ref().is_some_and(|previous| {
                    updates.head.as_ref().is_some_and(|head| {
                        head.number <= previous.number && head != previous
                    })
                });
                if (catch_up || updates.reorg_version != handled_reorg_version || replaces_published) && !current.is_empty() {
                    // An error followed by a quick reconnect can be coalesced into one watch
                    // update. The connection epoch still forces withdrawal of the old books.
                    previous_header = None;
                    yield withdrawal(&mut current);
                }
                handled_reorg_version = updates.reorg_version;
                let expected = if catch_up { None } else { updates.head.clone() };
                let previous = previous_header.clone();
                let watermark = last_verified_header.clone();
                let verify_chain = verified_epoch != updates.epoch;
                let result = {
                let read = async {
                    if verify_chain {
                        client.verify_chain().await?;
                    }
                    // Subscriptions have no historical replay. Catch up after each successful
                    // connection, after the subscription is ready to retain heads arriving here.
                    let mut header = match &expected {
                        Some(header) => header.clone(),
                        None => client.header("latest").await?,
                    };
                    if watermark.as_ref().is_some_and(|previous| header.number < previous.number) {
                        // A queued notification can precede the catch-up HTTP head. A genuine
                        // shorter-chain reorg is still accepted when it is the current head.
                        header = client.header("latest").await?;
                    }
                    await_head_timestamp(header.timestamp, client.max_age_seconds).await?;
                    if previous.as_ref() == Some(&header) {
                        return Ok::<_, FeedError>((header, None));
                    }
                    let next = client.fetch_components_at(&header, &known_tokens).await?;
                    Ok((header, Some(next)))
                };
                tokio::pin!(read);
                loop {
                    tokio::select! {
                        result = &mut read => break result,
                        changed = heads.changed() => {
                            if changed.is_err() {
                                break Err(FeedError::Connection(
                                    "ElFomoFi head subscription ended during helper read".into(),
                                ));
                            }
                            let newest = heads.borrow().clone();
                            let replaced = expected.as_ref().is_some_and(|expected| {
                                newest.head.as_ref().is_some_and(|head| {
                                    head.number <= expected.number && head != expected
                                })
                            });
                            if !newest.connected || newest.epoch != updates.epoch ||
                                newest.reorg_version != updates.reorg_version || replaced {
                                break Err(FeedError::Connection(
                                    "ElFomoFi head changed during helper read".into(),
                                ));
                            }
                            // Retain the latest higher head in the watch slot, while allowing the
                            // canonical in-flight read to finish instead of starving slow helpers.
                        }
                        _ = sleep(expiry_delay(previous_header.as_ref(), client.max_age_seconds)),
                            if !current.is_empty() => {
                            break Err(FeedError::Connection(
                                "ElFomoFi previous snapshot expired during helper read".into(),
                            ));
                        }
                    }
                }
                };
                match result {
                    Ok((header, next)) => {
                        // Chain verification remains valid for this connection even when a
                        // queued replacement makes us discard the otherwise completed read.
                        verified_epoch = updates.epoch;
                        let newest = heads.borrow().clone();
                        let replaced = newest.version != updates.version &&
                            newest.head.as_ref().is_some_and(|head| {
                                head.number <= header.number && head != &header
                            });
                        last_verified_header = Some(header.clone());
                        if !newest.connected || newest.epoch != updates.epoch ||
                            newest.reorg_version != updates.reorg_version || replaced {
                            previous_header = None;
                            if !current.is_empty() {
                                yield withdrawal(&mut current);
                            }
                            continue;
                        }
                        if let Some(next) = next {
                            previous_header = Some(header);
                            yield publication(&mut current, &mut known_tokens, next);
                        }
                    }
                    Err(error) => {
                        tracing::warn!("ElFomoFi helper update failed; withdrawing indicative liquidity: {error}");
                        previous_header = None;
                        if !current.is_empty() {
                            yield withdrawal(&mut current);
                        }
                    }
                }
            }
        })
    }
}

impl ElfomofiClient {
    pub(super) fn stream(
        &self,
    ) -> BoxStream<'static, Result<BookSnapshot<ElfomofiBlock>, FeedError>> {
        let client = self.clone();
        if let Some(ws_url) = client.ws_rpc_url.clone() {
            let events = head_events(
                ws_url,
                client.chain.id(),
                client.quote_timeout,
                Duration::from_secs(client.max_age_seconds),
                Duration::from_secs(1),
            );
            return client.stream_from_heads(events);
        }
        Box::pin(async_stream::stream! {
            let mut current: HashMap<String, Book> = HashMap::new();
            let mut known_tokens = HashMap::new();
            let mut previous_header: Option<RpcHeader> = None;
            let mut chain_verified = false;
            let mut ticker = interval(client.poll_time);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let result = async {
                    if !chain_verified {
                        client.verify_chain().await?;
                        chain_verified = true;
                    }
                    let header = client.header("latest").await?;
                    ensure_timestamp_fresh(header.timestamp, unix_now()?, client.max_age_seconds)?;
                    if previous_header.as_ref() == Some(&header) {
                        return Ok(None);
                    }
                    let next = client.fetch_components_at(&header, &known_tokens).await?;
                    Ok::<_, FeedError>(Some((header, next)))
                }.await;
                match result {
                    Ok(None) => continue,
                    Ok(Some((header, next))) => {
                        previous_header = Some(header);
                        yield publication(&mut current, &mut known_tokens, next);
                    }
                    Err(error) => {
                        tracing::warn!("ElFomoFi polling failed; withdrawing indicative liquidity: {error}");
                        previous_header = None;
                        if current.is_empty() {
                            continue;
                        }
                        yield withdrawal(&mut current);
                    }
                }
            }
        })
    }

    /// Refreshes pricing for the configured partner and execution sender at a verified block.
    pub(super) async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, FeedError> {
        for address in [&params.token_in, &params.token_out, &params.sender, &params.receiver] {
            validate_address(address)?;
        }
        if params.token_in == params.token_out ||
            params.amount_in == BigUint::default() ||
            params.amount_in.bits() > 256
        {
            return Err(FeedError::InvalidInput(
                "ElFomoFi requires distinct tokens and positive uint256 input".into(),
            ));
        }
        if !self.tokens.is_empty() &&
            (!self.tokens.contains(&params.token_in) || !self.tokens.contains(&params.token_out))
        {
            return Err(FeedError::InvalidInput(
                "ElFomoFi quote tokens excluded by client filter".into(),
            ));
        }
        self.verify_chain().await?;
        let header = self.header("latest").await?;
        ensure_timestamp_fresh(header.timestamp, unix_now()?, self.max_age_seconds)?;
        let call = getAmountOutCall {
            from: Address::from_slice(&params.token_in),
            to: Address::from_slice(&params.token_out),
            amount: U256::from_be_slice(&params.amount_in.to_bytes_be()),
            executor: Address::from_slice(&params.sender),
            partnerId: U256::from(PARTNER_ID),
        };
        let settlement: Bytes = router_for(self.chain)?
            .parse()
            .map_err(|_| FeedError::Fatal("Invalid compiled ElFomoFi router address".into()))?;
        let raw = self
            .call_at(pricing_for(self.chain)?, &call.abi_encode(), Some(&settlement), &header)
            .await?;
        self.verify_header(&header).await?;
        let result = getAmountOutCall::abi_decode_returns_validate(&raw)
            .map_err(|error| FeedError::Parsing(format!("Invalid ElFomoFi quote ABI: {error}")))?;
        let amount_out = BigUint::from_bytes_be(&result.toAmount.to_be_bytes::<32>());
        if amount_out == BigUint::default() {
            return Err(FeedError::Connection("ElFomoFi pricing returned zero output".into()));
        }
        let valid_until = header
            .timestamp
            .checked_add(self.max_age_seconds)
            .ok_or_else(|| FeedError::Parsing("ElFomoFi quote expiry overflow".into()))?;
        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: params.amount_in.clone(),
            amount_out,
            quote_attributes: HashMap::from([(
                "valid_until".into(),
                Bytes::from(valid_until.to_be_bytes().to_vec()),
            )]),
        })
    }
}

pub(crate) fn unix_now() -> Result<u64, FeedError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| FeedError::Fatal("System clock precedes UNIX epoch".into()))
}

fn parse_quantity(value: &Value, name: &str) -> Result<u64, FeedError> {
    let digits = value
        .as_str()
        .and_then(|value| value.strip_prefix("0x"))
        .filter(|digits| !digits.is_empty())
        .ok_or_else(|| FeedError::Parsing(format!("Invalid ElFomoFi {name} quantity")))?;
    u64::from_str_radix(digits, 16)
        .map_err(|_| FeedError::Parsing(format!("Invalid ElFomoFi {name} quantity")))
}

fn parse_hex_bytes(value: &Value, name: &str) -> Result<Bytes, FeedError> {
    let digits = value
        .as_str()
        .and_then(|value| value.strip_prefix("0x"))
        .ok_or_else(|| FeedError::Parsing(format!("Invalid ElFomoFi {name} hex")))?;
    hex::decode(digits)
        .map(Bytes::from)
        .map_err(|_| FeedError::Parsing(format!("Invalid ElFomoFi {name} hex")))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;
    use crate::pamm::protocols::elfomofi::{
        client_builder::ElfomofiClientBuilder,
        models::{
            getAllOrderbooksReturn, getAmountOutReturn, CumulativeLevel, PairOrderbook, BASE_HELPER,
        },
    };

    fn address(value: u8) -> Bytes {
        Bytes::from(vec![value; 20])
    }

    fn header(number: u64, timestamp: u64, hash: u8) -> Value {
        json!({"number": format!("0x{number:x}"), "timestamp": format!("0x{timestamp:x}"),
            "hash": format!("0x{}", hex::encode([hash; 32]))})
    }

    fn raw_books(number: u64, timestamp: u64, duplicate: bool) -> Value {
        let pairs = if duplicate { vec![(1, 2, 90), (1, 2, 90)] } else { vec![(1, 2, 90)] };
        raw_pairs(number, timestamp, &pairs)
    }

    fn raw_pairs(number: u64, timestamp: u64, pairs: &[(u8, u8, u64)]) -> Value {
        let books = pairs
            .iter()
            .map(|&(base, quote, bid_output)| PairOrderbook {
                base: Address::from_slice(&address(base)),
                quote: Address::from_slice(&address(quote)),
                askCumulativeLevels: vec![CumulativeLevel {
                    amountIn: U256::from(100),
                    amountOut: U256::from(10),
                }],
                bidCumulativeLevels: vec![CumulativeLevel {
                    amountIn: U256::from(10),
                    amountOut: U256::from(bid_output),
                }],
                balanceBase: U256::from(100),
                balanceQuote: U256::from(1000),
            })
            .collect();
        let raw = getAllOrderbooksCall::abi_encode_returns(&getAllOrderbooksReturn {
            books,
            blockNumber: U256::from(number),
            blockTimestamp: U256::from(timestamp),
        });
        json!(format!("0x{}", hex::encode(raw)))
    }

    #[tokio::test]
    async fn stream_builder_discovers_quotes_and_reconciles_complete_pair_lists() {
        use tycho_common::simulation::protocol_sim::ProtocolSim;

        use super::super::{tokens::symbolCall, ElfomofiFeed};
        use crate::book::BookFeedStream;

        let phases = [
            vec![],
            vec![(1, 2, 90)],
            vec![(1, 2, 80), (2, 3, 70)],
            vec![(2, 3, 60)],
            vec![(4, 5, 50)],
            vec![],
            vec![(1, 2, 40), (2, 3, 30)],
        ];
        for heads_mode in [true, false] {
            let now = unix_now().unwrap();
            let mut responses = vec![(json!("0x2105"), None)];
            let mut release = Vec::new();
            for (phase, pairs) in phases.iter().enumerate() {
                let number = 100 + phase as u64;
                let gate = if !heads_mode && phase > 0 {
                    let (send, recv) = tokio::sync::oneshot::channel();
                    release.push(send);
                    Some(recv)
                } else {
                    None
                };
                if phase == 0 || !heads_mode {
                    responses.push((header(number, now, phase as u8 + 1), gate));
                }
                responses.extend([
                    (raw_pairs(number, now, pairs), None),
                    (header(number, now, phase as u8 + 1), None),
                ]);
            }
            let (url, server, _) = mock_rpc_gated_with_metadata(
                responses,
                |request| {
                    let result = default_metadata(request)?;
                    if request["params"][0]["data"] == "0x95d89b41" {
                        if request["params"][0]["to"] == address(4).to_string() {
                            let mut legacy = [0u8; 32];
                            legacy[..6].copy_from_slice(b"LEGACY");
                            return Some(json!(format!("0x{}", hex::encode(legacy))));
                        }
                        if request["params"][0]["to"] == address(5).to_string() {
                            return Some(
                                json!({"error": {"code": -32000, "message": "symbol unavailable"}}),
                            );
                        }
                        assert_eq!(
                            result,
                            json!(format!(
                                "0x{}",
                                hex::encode(symbolCall::abi_encode_returns(&"TEST".to_owned()))
                            ))
                        );
                    }
                    Some(result)
                },
                true,
            )
            .await;
            let websocket = if heads_mode { Some(mock_ws().await) } else { None };
            let mut client = ElfomofiClientBuilder::new(Chain::Base).rpc_url(url);
            client = if let Some((url, _, _)) = &websocket {
                client.ws_rpc_url(url)
            } else {
                client.poll_time(Duration::from_millis(5))
            };
            let mut rx = BookFeedStream::spawn(
                "elfomofi",
                ElfomofiFeed { client: client.build().unwrap(), max_age: Duration::from_secs(10) },
            );
            let mut known = HashMap::new();
            let mut release = release.into_iter();
            for (phase, pairs) in phases.iter().enumerate() {
                if phase > 0 {
                    if let Some((_, commands, _)) = &websocket {
                        commands
                            .send(Some(header(100 + phase as u64, now, phase as u8 + 1)))
                            .await
                            .unwrap();
                    } else {
                        release
                            .next()
                            .unwrap()
                            .send(())
                            .unwrap();
                    }
                }
                let update = next_published(&mut rx).await;
                assert_eq!(update.books.len(), pairs.len(), "phase {phase}, heads={heads_mode}");
                for (id, book) in update.books.iter() {
                    let component = &book.component;
                    assert_eq!(id, &component.id.to_string());
                    for token in &component.tokens {
                        known.insert(token.address.clone(), token.clone());
                    }
                }
                for &(base, quote, output) in pairs {
                    let state = update
                        .books
                        .values()
                        .find_map(|book| {
                            let state = book
                                .state
                                .as_any()
                                .downcast_ref::<ElfomofiState>()?;
                            (state.snapshot().book.base_token == address(base) &&
                                state.snapshot().book.quote_token == address(quote))
                            .then_some(state)
                        })
                        .unwrap();
                    assert_eq!(state.snapshot().block_number, 100 + phase as u64);
                    let base = &known[&address(base)];
                    let quote = &known[&address(quote)];
                    assert_eq!(
                        state
                            .get_amount_out(5u64.into(), base, quote)
                            .unwrap()
                            .amount,
                        BigUint::from(output / 2)
                    );
                    assert_eq!(
                        state
                            .get_amount_out(100u64.into(), quote, base)
                            .unwrap()
                            .amount,
                        BigUint::from(10u8)
                    );
                }
            }
            assert_eq!(known[&address(4)].symbol, "LEGACY");
            assert_eq!(known[&address(5)].symbol, address(5).to_string());
            drop(rx);
            if let Some((_, _, task)) = websocket {
                task.abort();
            }
            let requests = server.await.unwrap();
            let metadata: Vec<_> = requests
                .iter()
                .filter(|request| default_metadata(request).is_some())
                .collect();
            assert_eq!(metadata.len(), 10, "each of five token addresses is fetched once");
            for token in 1..=5 {
                let calls: Vec<_> = metadata
                    .iter()
                    .filter(|request| request["params"][0]["to"] == address(token).to_string())
                    .collect();
                assert_eq!(calls.len(), 2);
                let expected_block = match token {
                    1 | 2 => "0x65",
                    3 => "0x66",
                    _ => "0x68",
                };
                for call in calls {
                    assert_eq!(call["params"][1], expected_block);
                }
            }
        }
    }

    #[tokio::test]
    async fn missing_new_token_decimals_withdraws_and_retries_without_losing_pairs() {
        use super::super::ElfomofiFeed;
        use crate::book::BookFeedStream;
        for heads_mode in [true, false] {
            let now = unix_now().unwrap();
            let mut responses = vec![
                (json!("0x2105"), None),
                (header(100, now, 1), None),
                (raw_pairs(100, now, &[(1, 2, 90)]), None),
                (header(100, now, 1), None),
            ];
            let mut release = Vec::new();
            for number in [101, 102] {
                if !heads_mode {
                    let (send, recv) = tokio::sync::oneshot::channel();
                    release.push(send);
                    responses.push((header(number, now, number as u8), Some(recv)));
                }
                responses.push((raw_pairs(number, now, &[(1, 2, 90), (2, 3, 70)]), None));
                if number == 102 {
                    responses.push((header(number, now, number as u8), None));
                }
            }
            let mut rejected = false;
            let (url, server, _) = mock_rpc_gated_with_metadata(
                responses,
                move |request| {
                    let response = default_metadata(request)?;
                    if request["params"][0]["to"] == address(3).to_string() &&
                        request["params"][0]["data"] == "0x313ce567" &&
                        !rejected
                    {
                        rejected = true;
                        return Some(json!("0x"));
                    }
                    Some(response)
                },
                true,
            )
            .await;
            let websocket = if heads_mode { Some(mock_ws().await) } else { None };
            let mut client = ElfomofiClientBuilder::new(Chain::Base).rpc_url(url);
            client = if let Some((url, _, _)) = &websocket {
                client.ws_rpc_url(url)
            } else {
                client.poll_time(Duration::from_millis(5))
            };
            let mut rx = BookFeedStream::spawn(
                "elfomofi",
                ElfomofiFeed { client: client.build().unwrap(), max_age: Duration::from_secs(10) },
            );
            let initial = next_published(&mut rx).await;
            assert_eq!(initial.books.len(), 1);
            let mut release = release.into_iter();
            for number in [101, 102] {
                if let Some((_, commands, _)) = &websocket {
                    commands
                        .send(Some(header(number, now, number as u8)))
                        .await
                        .unwrap();
                } else {
                    release
                        .next()
                        .unwrap()
                        .send(())
                        .unwrap();
                }
                let update = next_published(&mut rx).await;
                if number == 101 {
                    assert!(update.books.is_empty());
                } else {
                    assert_eq!(update.books.len(), 2);
                }
            }
            drop(rx);
            if let Some((_, _, task)) = websocket {
                task.abort();
            }
            let requests = server.await.unwrap();
            for token in 1..=3 {
                let decimals: Vec<_> = requests
                    .iter()
                    .filter(|request| {
                        request["params"][0]["to"] == address(token).to_string() &&
                            request["params"][0]["data"] == "0x313ce567"
                    })
                    .collect();
                assert_eq!(decimals.len(), if token == 3 { 2 } else { 1 });
            }
        }
    }

    #[tokio::test]
    async fn replacement_head_during_metadata_discards_unpublished_token_cache() {
        use super::super::{tokens::decimalsCall, ElfomofiFeed};
        use crate::book::BookFeedStream;
        let now = unix_now().unwrap();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let decimals = json!(format!("0x{}", hex::encode(decimalsCall::abi_encode_returns(&0))));
        let (url, server, mut observed) = mock_rpc_gated_with_metadata(
            vec![
                (json!("0x2105"), None),
                (header(100, now, 1), None),
                (raw_pairs(100, now, &[(1, 2, 90)]), None),
                (header(100, now, 1), None),
                (raw_pairs(101, now, &[(1, 2, 90), (2, 3, 70)]), None),
                (decimals.clone(), Some(blocked)),
                (raw_pairs(101, now, &[(1, 2, 80), (2, 3, 60)]), None),
                (decimals, None),
                (header(101, now, 3), None),
            ],
            |request| {
                if request["params"][0]["to"] == address(3).to_string() &&
                    request["params"][0]["data"] == "0x313ce567"
                {
                    return None;
                }
                default_metadata(request)
            },
            true,
        )
        .await;
        let (ws, commands, ws_task) = mock_ws().await;
        let client = ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url(url)
            .ws_rpc_url(ws)
            .build()
            .unwrap();
        let mut rx = BookFeedStream::spawn(
            "elfomofi",
            ElfomofiFeed { client, max_age: Duration::from_secs(10) },
        );
        assert_eq!(
            next_published(&mut rx)
                .await
                .books
                .len(),
            1
        );
        commands
            .send(Some(header(101, now, 2)))
            .await
            .unwrap();
        loop {
            let request = timeout(Duration::from_secs(3), observed.recv())
                .await
                .unwrap()
                .unwrap();
            if request["params"][0]["to"] == address(3).to_string() &&
                request["params"][0]["data"] == "0x313ce567"
            {
                break;
            }
        }
        commands
            .send(Some(header(101, now, 3)))
            .await
            .unwrap();
        let removed = next_published(&mut rx).await;
        assert!(removed.books.is_empty());

        release.send(()).unwrap();
        let replacement = next_published(&mut rx).await;
        assert_eq!(replacement.books.len(), 2);

        for book in replacement.books.values() {
            let state = book
                .state
                .as_any()
                .downcast_ref::<ElfomofiState>()
                .unwrap();
            assert_eq!(state.snapshot().block_hash, Bytes::from(vec![3; 32]));
        }
        drop(rx);
        ws_task.abort();
        let requests = server.await.unwrap();
        let decimals: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["params"][0]["to"] == address(3).to_string() &&
                    request["params"][0]["data"] == "0x313ce567"
            })
            .collect();
        assert_eq!(decimals.len(), 2, "discarded metadata must be fetched and delivered again");
    }

    fn raw_quote(amount: U256) -> Vec<u8> {
        getAmountOutCall::abi_encode_returns(&getAmountOutReturn {
            toAmount: amount,
            quoteId: U256::from(71),
            data: vec![0xab, 0xcd].into(),
        })
    }

    fn assert_pricing_request(
        request: &Value,
        params: &GetAmountOutParams,
        chain: Chain,
    ) -> getAmountOutCall {
        assert_eq!(request["method"], "eth_call");
        assert_eq!(request["params"][0]["from"], router_for(chain).unwrap());
        assert_eq!(request["params"][0]["to"], pricing_for(chain).unwrap());
        let data = parse_hex_bytes(&request["params"][0]["data"], "test data").unwrap();
        assert_eq!(&data[..4], &[0x56, 0x1f, 0xa9, 0x7d]);
        let call = getAmountOutCall::abi_decode_validate(&data).unwrap();
        assert_eq!(call.from.as_slice(), params.token_in.as_ref());
        assert_eq!(call.to.as_slice(), params.token_out.as_ref());
        assert_eq!(call.amount, U256::from_be_slice(&params.amount_in.to_bytes_be()));
        assert_eq!(call.executor.as_slice(), params.sender.as_ref());
        assert_eq!(call.partnerId, U256::from(299));
        call
    }

    /// Responds to the RPC boundary and returns the decoded requests for behavioral assertions.
    async fn mock_rpc(results: Vec<Value>) -> (String, JoinHandle<Vec<Value>>) {
        let (url, task, _) = mock_rpc_gated(
            results
                .into_iter()
                .map(|result| (result, None))
                .collect(),
        )
        .await;
        (url, task)
    }

    async fn mock_rpc_gated(
        results: Vec<(Value, Option<tokio::sync::oneshot::Receiver<()>>)>,
    ) -> (String, JoinHandle<Vec<Value>>, tokio::sync::mpsc::Receiver<Value>) {
        mock_rpc_gated_with_metadata(results, default_metadata, false).await
    }

    fn default_metadata(request: &Value) -> Option<Value> {
        use super::super::tokens::{decimalsCall, symbolCall};
        if request["method"] != "eth_call" {
            return None;
        }
        match request["params"][0]["data"].as_str()? {
            "0x313ce567" => {
                Some(json!(format!("0x{}", hex::encode(decimalsCall::abi_encode_returns(&0)))))
            }
            "0x95d89b41" => Some(json!(format!(
                "0x{}",
                hex::encode(symbolCall::abi_encode_returns(&"TEST".to_owned()))
            ))),
            _ => None,
        }
    }

    // Legacy head tests inspect helper/quote requests. Metadata-focused tests opt into recording
    // all calls; the server always exercises the real ERC20 RPC discovery path.
    async fn mock_rpc_gated_with_metadata(
        results: Vec<(Value, Option<tokio::sync::oneshot::Receiver<()>>)>,
        mut metadata: impl FnMut(&Value) -> Option<Value> + Send + 'static,
        record_metadata: bool,
    ) -> (String, JoinHandle<Vec<Value>>, tokio::sync::mpsc::Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (observed, requests_rx) = tokio::sync::mpsc::channel(64);
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (result, gate) in results {
                let mut gate = gate;
                loop {
                    let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .expect("mock RPC expected another request within five seconds")
                        .unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut content_length = None;
                    loop {
                        let mut line = String::new();
                        reader
                            .read_line(&mut line)
                            .await
                            .unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(length) = line
                            .to_lowercase()
                            .strip_prefix("content-length:")
                        {
                            content_length = Some(length.trim().parse::<usize>().unwrap());
                        }
                    }
                    let mut body = vec![0; content_length.unwrap()];
                    reader
                        .read_exact(&mut body)
                        .await
                        .unwrap();
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let token_result = metadata(&request);
                    let is_metadata = token_result.is_some();
                    let response_result = token_result.as_ref().unwrap_or(&result);
                    if !is_metadata || record_metadata {
                        let _ = observed.send(request.clone()).await;
                        requests.push(request);
                    }
                    if !is_metadata {
                        if let Some(gate) = gate.take() {
                            let _ = timeout(Duration::from_secs(5), gate)
                                .await
                                .expect("mock RPC response gate was not released");
                        }
                    }
                    let response = if response_result.get("error").is_some() {
                        json!({"jsonrpc": "2.0", "id": 1, "error": response_result["error"]})
                    } else {
                        json!({"jsonrpc": "2.0", "id": 1, "result": response_result})
                    }
                    .to_string();
                    let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len());
                    reader
                        .into_inner()
                        .write_all(response.as_bytes())
                        .await
                        .unwrap_or_else(|error| {
                            assert!(
                                matches!(
                                    error.kind(),
                                    std::io::ErrorKind::BrokenPipe |
                                        std::io::ErrorKind::ConnectionReset
                                ),
                                "{error}"
                            );
                        });
                    if !is_metadata {
                        break;
                    }
                }
            }
            requests
        });
        (url, task, requests_rx)
    }

    fn controlled_heads() -> (
        tokio::sync::mpsc::Sender<Result<HeadEvent, FeedError>>,
        BoxStream<'static, Result<HeadEvent, FeedError>>,
    ) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        (
            sender,
            Box::pin(async_stream::stream! {
                while let Some(event) = receiver.recv().await {
                    yield event;
                }
            }),
        )
    }

    fn announced(number: u64, timestamp: u64, hash: u8) -> Result<HeadEvent, FeedError> {
        Ok(HeadEvent::Head(RpcHeader { number, timestamp, hash: Bytes::from(vec![hash; 32]) }))
    }

    fn snapshot(message: &BookSnapshot<ElfomofiBlock>) -> OrderbookSnapshot {
        message
            .books
            .values()
            .next()
            .unwrap()
            .state
            .as_any()
            .downcast_ref::<ElfomofiState>()
            .unwrap()
            .snapshot()
            .clone()
    }

    fn test_tokens(snapshot: &OrderbookSnapshot) -> HashMap<Bytes, Token> {
        [&snapshot.book.base_token, &snapshot.book.quote_token]
            .into_iter()
            .map(|address| {
                (address.clone(), Token::new(address, "TEST", 0, 0, &[], snapshot.chain, 100))
            })
            .collect()
    }

    async fn next_published(
        stream: &mut crate::book::BookFeedStream<ElfomofiBlock>,
    ) -> BookSnapshot<ElfomofiBlock> {
        match timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
        {
            crate::book::BookFeedEvent::Published(snapshot) => snapshot,
            event => panic!("expected complete book snapshot, got {event:?}"),
        }
    }

    async fn next_message(
        stream: &mut BoxStream<'static, Result<BookSnapshot<ElfomofiBlock>, FeedError>>,
    ) -> BookSnapshot<ElfomofiBlock> {
        timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    /// A real WebSocket boundary, with notifications controlled by the test.
    async fn mock_ws() -> (String, tokio::sync::mpsc::Sender<Option<Value>>, JoinHandle<()>) {
        mock_ws_on(Chain::Base).await
    }

    async fn mock_ws_on(
        chain: Chain,
    ) -> (String, tokio::sync::mpsc::Sender<Option<Value>>, JoinHandle<()>) {
        use futures::SinkExt;
        use tokio_tungstenite::{accept_async, tungstenite::Message};
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (commands, mut receiver) = tokio::sync::mpsc::channel::<Option<Value>>(16);
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(socket).await.unwrap();
                for (method, result) in [
                    ("eth_chainId", json!(format!("0x{:x}", chain.id()))),
                    ("eth_subscribe", json!("heads")),
                ] {
                    let request: Value = serde_json::from_str(
                        socket
                            .next()
                            .await
                            .unwrap()
                            .unwrap()
                            .to_text()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(request["method"], method);
                    socket
                        .send(Message::Text(
                            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
                while let Some(command) = receiver.recv().await {
                    match command {
                        Some(header) => socket
                            .send(Message::Text(
                                json!({
                                    "jsonrpc": "2.0", "method": "eth_subscription",
                                    "params": {"subscription": "heads", "result": header}
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap(),
                        None => {
                            let _ = socket.close(None).await;
                            break;
                        }
                    }
                }
                if receiver.is_closed() {
                    return;
                }
            }
        });
        (url, commands, task)
    }

    fn client(url: String) -> ElfomofiClient {
        client_on(Chain::Base, url)
    }

    fn client_on(chain: Chain, url: String) -> ElfomofiClient {
        ElfomofiClientBuilder::new(chain)
            .rpc_url(url)
            .poll_time(Duration::from_millis(1))
            .build()
            .unwrap()
    }

    fn params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from(7u8),
            token_in: address(1),
            token_out: address(2),
            sender: address(3),
            receiver: address(4),
            origin: None,
        }
    }

    #[tokio::test]
    async fn all_chain_snapshots_use_their_helper_and_component_deployment() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let now = unix_now().unwrap();
            let (url, server) = mock_rpc(vec![
                json!(format!("0x{:x}", chain.id())),
                header(123, now, 1),
                raw_books(123, now, false),
                header(123, now, 1),
            ])
            .await;
            let client = client_on(chain, url);
            let snapshots = client.fetch_snapshots().await.unwrap();
            assert_eq!(snapshots.len(), 1);
            assert_eq!(snapshots[0].chain, chain);
            let component = client
                .create_book(&snapshots[0], &test_tokens(&snapshots[0]))
                .unwrap();
            let router: Bytes = router_for(chain)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(component.component.chain, chain);
            assert_eq!(component.component.contract_ids, vec![router.clone()]);
            assert_eq!(component.component.static_attributes["router"], router);
            assert_eq!(
                hex::encode(component.component.id),
                snapshots[0]
                    .book
                    .component_id(chain)
                    .unwrap()
            );
            let serialized = component
                .state
                .as_any()
                .downcast_ref::<ElfomofiState>()
                .unwrap()
                .snapshot()
                .clone();
            assert_eq!(serialized, snapshots[0]);
            assert!(client
                .create_book(&snapshots[0], &HashMap::new())
                .is_err());
            let mut wrong_tokens = test_tokens(&snapshots[0]);
            wrong_tokens
                .get_mut(&snapshots[0].book.base_token)
                .unwrap()
                .chain = Chain::Ethereum;
            assert!(client
                .create_book(&snapshots[0], &wrong_tokens)
                .is_err());
            let requests = server.await.unwrap();
            assert_eq!(requests[2]["params"][0]["to"], helper_for(chain).unwrap());
            assert_eq!(requests[2]["params"][1], "0x7b");
            let mut wrong = snapshots[0].clone();
            wrong.chain = if chain == Chain::Bsc { Chain::Base } else { Chain::Bsc };
            assert!(client
                .create_book(&wrong, &test_tokens(&wrong))
                .is_err());
        }
    }

    #[tokio::test]
    async fn native_feed_withdraws_at_publisher_ttl_without_polling() {
        use super::super::ElfomofiFeed;
        use crate::book::{BookFeedEvent, BookFeedStream};
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
        ])
        .await;
        let client = ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url(url)
            .poll_time(Duration::from_secs(60))
            .build()
            .unwrap();
        let mut feed = BookFeedStream::spawn(
            "elfomofi",
            ElfomofiFeed { client, max_age: Duration::from_millis(50) },
        );
        let published = next_published(&mut feed).await;
        assert_eq!(published.anchor.number, 123);
        assert_eq!(published.anchor.hash, Bytes::from(vec![1; 32]));
        assert_eq!(published.anchor.timestamp, now);
        assert_eq!(published.books.len(), 1);
        assert!(matches!(
            timeout(Duration::from_secs(1), feed.next())
                .await
                .unwrap(),
            Some(BookFeedEvent::Withdrawn)
        ));
        assert_eq!(server.await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn dropping_native_feed_stops_future_rpc_reads() {
        use super::super::ElfomofiFeed;
        use crate::book::BookFeedStream;
        let now = unix_now().unwrap();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            (header(124, now, 2), None),
        ])
        .await;
        let client = ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url(url)
            .poll_time(Duration::from_millis(100))
            .build()
            .unwrap();
        let mut feed = BookFeedStream::spawn(
            "elfomofi",
            ElfomofiFeed { client, max_age: Duration::from_secs(10) },
        );
        next_published(&mut feed).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        drop(feed);
        assert!(timeout(Duration::from_millis(200), requests.recv())
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn every_chain_rejects_mismatched_http_endpoint_before_fetching_or_quoting() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            for quote in [false, true] {
                let wrong_chain = if chain == Chain::Base { Chain::Bsc } else { Chain::Base };
                let (url, server) =
                    mock_rpc(vec![json!(format!("0x{:x}", wrong_chain.id()))]).await;
                let client = client_on(chain, url);
                let error = if quote {
                    client
                        .request_binding_quote(&params())
                        .await
                        .unwrap_err()
                } else {
                    client
                        .fetch_snapshots()
                        .await
                        .unwrap_err()
                };
                assert!(matches!(error, FeedError::InvalidInput(_)));
                assert!(error
                    .to_string()
                    .contains(&chain.id().to_string()));
                let requests = server.await.unwrap();
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0]["method"], "eth_chainId");
            }
        }
    }

    #[tokio::test]
    async fn websocket_head_refreshes_immediately_without_http_head_polling() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
            raw_books(124, now, false),
            header(124, now, 2),
        ])
        .await;
        let (ws_url, heads, ws_server) = mock_ws().await;
        let client = client(url)
            .with_ws_rpc_url(ws_url)
            .unwrap();
        let mut stream = client.stream();
        assert_eq!(snapshot(&next_message(&mut stream).await).block_number, 123);
        assert!(timeout(Duration::from_millis(100), stream.next())
            .await
            .is_err());
        heads
            .send(Some(header(124, now, 2)))
            .await
            .unwrap();
        let updated = timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(snapshot(&updated).block_number, 124);
        let requests = server.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "eth_call")
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "eth_getBlockByNumber" &&
                    request["params"][0] == "latest")
                .count(),
            1
        );
        drop(stream);
        ws_server.abort();
    }

    #[tokio::test]
    async fn next_second_announced_head_waits_for_its_timestamp_before_helper_read() {
        let now = unix_now().unwrap();
        let future = now + 1;
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            (raw_books(124, future, false), None),
            (header(124, future, 2), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(124, future, 2))
            .await
            .unwrap();
        let fetch = tokio::spawn(async move { next_message(&mut stream).await });
        let request = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request["method"], "eth_call");
        assert!(unix_now().unwrap() >= future, "Future head must wait before its helper call");
        let updated = snapshot(&fetch.await.unwrap());
        assert_eq!(updated.block_timestamp, future);
        assert!(updated.observed_at >= future);
        updated
            .ensure_fresh(unix_now().unwrap())
            .unwrap();
        assert_eq!(server.await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn next_second_catch_up_head_waits_before_publication() {
        let future = unix_now().unwrap() + 1;
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, future, 1),
            raw_books(123, future, false),
            header(123, future, 1),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        let updated = snapshot(&next_message(&mut stream).await);
        assert_eq!(updated.block_timestamp, future);
        assert!(updated.observed_at >= future);
        assert!(unix_now().unwrap() >= future);
        assert_eq!(server.await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn far_future_head_withdraws_promptly_without_helper_call() {
        let now = unix_now().unwrap();
        let future = now + 60;
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            // Keep the listener available to detect an erroneous helper read.
            (raw_books(124, future, false), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(124, future, 2))
            .await
            .unwrap();
        let removed = timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(removed.books.is_empty());
        assert!(timeout(Duration::from_millis(100), requests.recv())
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn head_subscription_skips_duplicates_and_accepts_same_height_reorgs() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 2),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        let initial = snapshot(&next_message(&mut stream).await);
        heads
            .send(announced(123, now, 1))
            .await
            .unwrap();
        assert!(timeout(Duration::from_millis(100), stream.next())
            .await
            .is_err());
        heads
            .send(announced(123, now, 2))
            .await
            .unwrap();
        let removed = next_message(&mut stream).await;
        assert!(removed.books.is_empty());
        let updated = snapshot(&next_message(&mut stream).await);
        assert_eq!(initial.block_number, updated.block_number);
        assert_ne!(initial.block_hash, updated.block_hash);
        assert_eq!(server.await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn websocket_disconnect_withdraws_and_reconnect_catches_up_latest_head() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
            json!("0x2105"),
            header(125, now, 5),
            raw_books(125, now, false),
            header(125, now, 5),
        ])
        .await;
        let (ws_url, heads, ws_server) = mock_ws().await;
        let mut stream = client(url)
            .with_ws_rpc_url(ws_url)
            .unwrap()
            .stream();
        assert_eq!(snapshot(&next_message(&mut stream).await).block_number, 123);
        heads.send(None).await.unwrap();
        let removed = next_message(&mut stream).await;
        assert!(removed.books.is_empty());
        assert_eq!(snapshot(&next_message(&mut stream).await).block_number, 125);
        assert_eq!(server.await.unwrap().len(), 8);
        drop(stream);
        ws_server.abort();
    }

    #[tokio::test]
    async fn stalled_subscription_expires_books_without_rpc_head_or_helper_polls() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
        ])
        .await;
        let client = ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url(url)
            .poll_time(Duration::from_secs(60))
            .max_age(Duration::from_secs(2))
            .build()
            .unwrap();
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client.stream_from_heads(events);
        assert_eq!(
            next_message(&mut stream)
                .await
                .books
                .len(),
            1
        );
        let removed = next_message(&mut stream).await;
        assert!(removed.books.is_empty());
        assert_eq!(server.await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn keeps_head_arriving_during_startup_catch_up() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), Some(gate)),
            (header(123, now, 1), None),
            (raw_books(124, now, false), None),
            (header(124, now, 2), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        let fetch = tokio::spawn(async move {
            let message = next_message(&mut stream).await;
            (stream, message)
        });
        for _ in 0..3 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(124, now, 2))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        let (mut stream, initial) = fetch.await.unwrap();
        assert_eq!(snapshot(&initial).block_number, 123);
        assert_eq!(snapshot(&next_message(&mut stream).await).block_number, 124);
        let requests = server.await.unwrap();
        assert_eq!(requests[4]["params"][1], "0x7c");
        assert_eq!(requests.len(), 6);
    }

    #[tokio::test]
    async fn queued_older_head_never_rolls_back_a_newer_catch_up_snapshot() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(124, now, 2), None),
            (raw_books(124, now, false), Some(gate)),
            (header(124, now, 2), None),
            (header(124, now, 2), None),
            (raw_books(124, now, false), None),
            (header(124, now, 2), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        let fetch = tokio::spawn(async move {
            let message = next_message(&mut stream).await;
            (stream, message)
        });
        for _ in 0..3 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(123, now, 1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        let (mut stream, initial) = fetch.await.unwrap();
        assert_eq!(snapshot(&initial).block_number, 124);
        // The in-flight catch-up is discarded conservatively. Resolve the queued older head
        // against latest, then publish the verified current block without rolling back to 123.
        assert_eq!(initial.books.len(), 1);
        assert!(timeout(Duration::from_millis(50), stream.next())
            .await
            .is_err());
        let requests = server.await.unwrap();
        assert_eq!(requests[4]["params"][0], "latest");
        assert_eq!(requests[5]["params"][1], "0x7c");
    }

    #[tokio::test]
    async fn first_lower_head_discards_an_in_flight_orphaned_catch_up() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(124, now, 2), None),
            (raw_books(124, now, false), Some(gate)),
            (header(124, now, 2), None),
            (header(123, now, 9), None),
            (raw_books(123, now, false), None),
            (header(123, now, 9), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        let fetch = tokio::spawn(async move { next_message(&mut stream).await });
        for _ in 0..3 {
            requests.recv().await.unwrap();
        }
        // The first notification after subscription has no previous WS head for the pump to
        // compare. It must still invalidate the older fork's catch-up before publication.
        heads
            .send(announced(123, now, 9))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        let current = snapshot(&fetch.await.unwrap());
        assert_eq!(current.block_number, 123);
        assert_eq!(current.block_hash, Bytes::from(vec![9; 32]));
        let requests = server.await.unwrap();
        assert_eq!(requests[4]["params"][0], "latest");
        assert_eq!(requests[5]["params"][1], "0x7b");
    }

    #[tokio::test]
    async fn published_reorg_liquidity_is_removed_before_replacement_helper_read() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), Some(gate)),
            (header(123, now, 2), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(123, now, 2))
            .await
            .unwrap();
        let removed = next_message(&mut stream).await;
        assert!(removed.books.is_empty());
        assert!(requests.try_recv().is_err(), "Removal must precede the helper read");
        let fetch = tokio::spawn(async move { next_message(&mut stream).await });
        assert_eq!(requests.recv().await.unwrap()["method"], "eth_call");
        assert!(!fetch.is_finished());
        release.send(()).unwrap();
        assert_eq!(snapshot(&fetch.await.unwrap()).block_hash, Bytes::from(vec![2; 32]));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_cancels_blocked_helper_and_removes_existing_liquidity() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            (raw_books(124, now, false), Some(gate)),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(124, now, 2))
            .await
            .unwrap();
        let fetch = tokio::spawn(async move { next_message(&mut stream).await });
        requests.recv().await.unwrap();
        heads
            .send(Err(FeedError::Connection("Test disconnect".into())))
            .await
            .unwrap();
        let removed = timeout(Duration::from_millis(500), fetch)
            .await
            .unwrap()
            .unwrap();
        assert!(removed.books.is_empty());
        release.send(()).unwrap();
        assert_eq!(server.await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn replacement_fence_survives_a_coalesced_higher_head() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            (raw_books(124, now, false), Some(gate)),
            (raw_books(125, now, false), None),
            (header(125, now, 5), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(124, now, 2))
            .await
            .unwrap();
        let fetch = tokio::spawn(async move {
            let message = next_message(&mut stream).await;
            (stream, message)
        });
        requests.recv().await.unwrap();
        heads
            .send(announced(124, now, 9))
            .await
            .unwrap();
        heads
            .send(announced(125, now, 5))
            .await
            .unwrap();
        let (mut stream, removed) = timeout(Duration::from_millis(500), fetch)
            .await
            .unwrap()
            .unwrap();
        assert!(removed.books.is_empty());
        release.send(()).unwrap();
        assert_eq!(snapshot(&next_message(&mut stream).await).block_number, 125);
        assert_eq!(server.await.unwrap().len(), 7);
    }

    #[tokio::test]
    async fn dropping_stream_aborts_the_background_head_pump() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        drop(stream);
        timeout(Duration::from_millis(500), heads.closed())
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn coalesces_pending_heads_while_helper_is_slow() {
        let now = unix_now().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (url, server, mut requests) = mock_rpc_gated(vec![
            (json!("0x2105"), None),
            (header(123, now, 1), None),
            (raw_books(123, now, false), None),
            (header(123, now, 1), None),
            (raw_books(124, now, false), Some(gate)),
            (header(124, now, 2), None),
            (raw_books(126, now, false), None),
            (header(126, now, 4), None),
        ])
        .await;
        let (heads, events) = controlled_heads();
        heads
            .send(Ok(HeadEvent::Connected))
            .await
            .unwrap();
        let mut stream = client(url).stream_from_heads(events);
        next_message(&mut stream).await;
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        heads
            .send(announced(124, now, 2))
            .await
            .unwrap();
        let fetch = tokio::spawn(async move {
            let message = next_message(&mut stream).await;
            (stream, message)
        });
        let pending = requests.recv().await.unwrap();
        assert_eq!(pending["params"][1], "0x7c");
        heads
            .send(announced(125, now, 3))
            .await
            .unwrap();
        heads
            .send(announced(126, now, 4))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        let (mut stream, completed) = fetch.await.unwrap();
        assert_eq!(snapshot(&completed).block_number, 124);
        assert_eq!(snapshot(&next_message(&mut stream).await).block_number, 126);
        let requests = server.await.unwrap();
        assert_eq!(requests[6]["params"][1], "0x7e");
        assert_eq!(requests.len(), 8);
    }

    #[tokio::test]
    async fn fetches_atomic_all_books_at_one_verified_block() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
        ])
        .await;
        let client = client(url);
        let snapshots = client.fetch_snapshots().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].block_number, 123);
        assert_eq!(snapshots[0].book.bids[0].amount_out, BigUint::from(90u8));
        let component = client
            .create_book(&snapshots[0], &test_tokens(&snapshots[0]))
            .unwrap();
        assert_eq!(component.component.protocol_system, ElfomofiClient::PROTOCOL_SYSTEM);
        let decoded = component
            .state
            .as_any()
            .downcast_ref::<ElfomofiState>()
            .unwrap()
            .snapshot()
            .clone();
        assert_eq!(decoded, snapshots[0]);
        let requests = server.await.unwrap();
        assert_eq!(requests[2]["method"], "eth_call");
        assert_eq!(requests[2]["params"][0]["to"], BASE_HELPER);
        assert_eq!(requests[2]["params"][1], "0x7b");
        assert_eq!(requests[3]["params"][0], "0x7b");
    }

    #[tokio::test]
    async fn rejects_reorg_between_read_and_verification() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 2),
        ])
        .await;
        assert!(client(url)
            .fetch_snapshots()
            .await
            .unwrap_err()
            .to_string()
            .contains("header changed"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_helper_metadata_mismatch_and_duplicate_pairs_atomically() {
        let now = unix_now().unwrap();
        for raw in [raw_books(122, now, false), raw_books(123, now, true), json!("0xdeadbeef")] {
            let (url, server) = mock_rpc(vec![json!("0x2105"), header(123, now, 1), raw]).await;
            assert!(client(url)
                .fetch_snapshots()
                .await
                .is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn skips_same_block_then_withdraws_on_error_and_recovers() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
            header(123, now, 1),
            header(124, now, 2),
            json!({"error": {"code": -32000, "message": "Unavailable"}}),
            header(124, now, 2),
            raw_books(124, now, false),
            header(124, now, 2),
        ])
        .await;
        let mut stream = client(url).stream();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.books.len(), 1);
        let removed = stream.next().await.unwrap().unwrap();
        assert!(removed.books.is_empty());
        let recovered = stream.next().await.unwrap().unwrap();
        assert_eq!(recovered.books.len(), 1);
        assert_eq!(recovered.books.len(), 1);
        let requests = server.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "eth_call")
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn replaces_same_height_snapshot_when_canonical_hash_changes() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
            header(123, now, 2),
            raw_books(123, now, false),
            header(123, now, 2),
        ])
        .await;
        let mut stream = client(url).stream();
        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        assert_ne!(snapshot(&first).block_hash, snapshot(&second).block_hash);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn withdraws_a_stalled_head_at_ttl_without_repolling_helper() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            raw_books(123, now, false),
            header(123, now, 1),
            header(123, now, 1),
        ])
        .await;
        let client = ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url(url)
            .poll_time(Duration::from_millis(1))
            .max_age(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut stream = client.stream();
        let initial = stream.next().await.unwrap().unwrap();
        assert_eq!(initial.books.len(), 1);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let removed = stream.next().await.unwrap().unwrap();
        assert!(removed.books.is_empty());
        let requests = server.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "eth_call")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn rejects_unexecutable_exact_inputs_before_contacting_rpc() {
        let client = client("http://127.0.0.1:1".into());
        let mut invalid = params();
        invalid.amount_in = BigUint::default();
        assert!(matches!(
            client
                .request_binding_quote(&invalid)
                .await,
            Err(FeedError::InvalidInput(_))
        ));
        invalid.amount_in = BigUint::from(1u8) << 256;
        assert!(matches!(
            client
                .request_binding_quote(&invalid)
                .await,
            Err(FeedError::InvalidInput(_))
        ));
        invalid = params();
        invalid.token_out = invalid.token_in.clone();
        assert!(matches!(
            client
                .request_binding_quote(&invalid)
                .await,
            Err(FeedError::InvalidInput(_))
        ));
        invalid = params();
        invalid.sender = Bytes::from(vec![0; 20]);
        assert!(matches!(
            client
                .request_binding_quote(&invalid)
                .await,
            Err(FeedError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn enabled_stream_and_serialized_state_use_regular_rpc_configuration() {
        use tycho_common::simulation::indicatively_priced::IndicativelyPriced;

        use super::super::{ElfomofiFeedBuilder, ElfomofiOptions};
        use crate::book::BookFeedStream;
        const CHILD: &str = "TYCHO_ELFOMOFI_RPC_TEST_CHILD";
        if let Ok(configuration) = std::env::var(CHILD) {
            let (mode, chain) = configuration.split_once(':').unwrap();
            let chain: Chain = chain.parse().unwrap();
            let mut options = ElfomofiOptions::default();
            if mode != "heads" {
                options = options
                    .poll_time(Duration::from_secs(60))
                    .max_age(Duration::from_secs(7));
            }
            let feed = ElfomofiFeedBuilder::new(chain)
                .options(options)
                .build()
                .unwrap();
            let mut stream = BookFeedStream::spawn("elfomofi", feed);
            let update = next_published(&mut stream).await;
            assert_eq!(update.books.len(), 1);
            let book = update.books.values().next().unwrap();
            for token in &book.component.tokens {
                assert_eq!(token.chain, chain);
                assert_eq!(token.decimals, 0);
            }
            let state = book
                .state
                .as_any()
                .downcast_ref::<ElfomofiState>()
                .unwrap();
            assert_eq!(state.snapshot().block_number, 123);
            assert_eq!(state.snapshot().chain, chain);
            assert_eq!(state.snapshot().max_age_seconds, if mode == "heads" { 10 } else { 7 });
            let serialized = serde_json::to_string(state).unwrap();
            assert!(!serialized.contains(&std::env::var("RPC_URL").unwrap()));
            assert!(!serialized.contains("client"));
            let quote = state
                .request_signed_quote(params())
                .await
                .unwrap();
            assert_eq!(quote.amount_out, BigUint::from(42u8));
            let restored: ElfomofiState = serde_json::from_str(&serialized).unwrap();
            let restored_quote = restored
                .request_signed_quote(params())
                .await
                .unwrap();
            assert_eq!(restored_quote.amount_out, quote.amount_out);
            assert_eq!(restored_quote.quote_attributes, quote.quote_attributes);
            drop(stream);
            return;
        }

        for (chain, mode) in [Chain::Base, Chain::Bsc, Chain::Robinhood]
            .into_iter()
            .flat_map(|chain| {
                ["heads", "polling"]
                    .into_iter()
                    .map(move |mode| (chain, mode))
            })
        {
            let now = unix_now().unwrap();
            let output = raw_quote(U256::from(42));
            let mut responses = vec![
                json!(format!("0x{:x}", chain.id())),
                header(123, now, 1),
                raw_books(123, now, false),
                header(123, now, 1),
            ];
            for _ in 0..2 {
                responses.extend([
                    json!(format!("0x{:x}", chain.id())),
                    header(124, now, 2),
                    json!(format!("0x{}", hex::encode(&output))),
                    header(124, now, 2),
                ]);
            }
            let (url, server) = mock_rpc(responses).await;
            let websocket = if mode == "heads" { Some(mock_ws_on(chain).await) } else { None };
            let ws_url = websocket
                .as_ref()
                .map(|(url, _, _)| url.clone());
            // A child process isolates RPC_URL from the other concurrently running socket tests.
            let child = tokio::task::spawn_blocking(move || {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                .args([
                    "--exact",
                    "pamm::protocols::elfomofi::client::tests::enabled_stream_and_serialized_state_use_regular_rpc_configuration",
                    "--nocapture",
                ])
                .env(CHILD, format!("{mode}:{chain}"))
                .env("RPC_URL", &url)
                .env_remove("WS_RPC_URL");
                if let Some(ws_url) = ws_url {
                    command.env("WS_RPC_URL", ws_url);
                }
                command.output().unwrap()
            });
            let result = timeout(Duration::from_secs(15), child)
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            let requests = server.await.unwrap();
            assert_eq!(requests.len(), 12);
            assert_eq!(requests[2]["params"][0]["to"], helper_for(chain).unwrap());
            for index in [6, 10] {
                assert_pricing_request(&requests[index], &params(), chain);
                assert_eq!(requests[index]["params"][1], "0x7c");
            }
            if let Some((_, _, server)) = websocket {
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn refresh_quote_preserves_uint256_max_sender_block_and_expiry() {
        let now = unix_now().unwrap();
        let output = raw_quote(U256::from(42));
        let (url, server) = mock_rpc(vec![
            json!("0x2105"),
            header(123, now, 1),
            json!(format!("0x{}", hex::encode(output))),
            header(123, now, 1),
        ])
        .await;
        let mut params = params();
        params.amount_in = BigUint::from_bytes_be(&U256::MAX.to_be_bytes::<32>());
        let quote = client(url)
            .request_binding_quote(&params)
            .await
            .unwrap();
        assert_eq!(quote.amount_in, params.amount_in);
        assert_eq!(quote.amount_out, BigUint::from(42u8));
        assert_eq!(quote.quote_attributes["valid_until"].as_ref(), &(now + 10).to_be_bytes());
        let requests = server.await.unwrap();
        let call = assert_pricing_request(&requests[2], &params, Chain::Base);
        assert_eq!(requests[2]["params"][1], "0x7b");
        assert_eq!(call.amount, U256::MAX);
    }

    #[tokio::test]
    async fn pricing_quotes_forward_execution_sender_separately_from_rpc_caller() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            for sender in [address(3), address(5)] {
                let now = unix_now().unwrap();
                let output = raw_quote(U256::from(42));
                let (url, server) = mock_rpc(vec![
                    json!(format!("0x{:x}", chain.id())),
                    header(123, now, 1),
                    json!(format!("0x{}", hex::encode(output))),
                    header(123, now, 1),
                ])
                .await;
                let params = GetAmountOutParams { sender, ..params() };
                let quote = client_on(chain, url)
                    .request_binding_quote(&params)
                    .await
                    .unwrap();
                assert_eq!(quote.amount_out, BigUint::from(42u8));
                let requests = server.await.unwrap();
                assert_pricing_request(&requests[2], &params, chain);
                assert_ne!(requests[2]["params"][0]["from"], params.sender.to_string());
                assert_eq!(requests[2]["params"][1], "0x7b");
            }
        }
    }

    #[tokio::test]
    async fn rejects_old_quote_abi_zero_output_and_context_reverts_without_fallback() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            for response in [
                json!(format!("0x{}", hex::encode(U256::from(42).to_be_bytes::<32>()))),
                json!(format!("0x{}", hex::encode(raw_quote(U256::ZERO)))),
                json!({"error": {"code": -32000, "message": "onlySwapContract secret-provider-key"}}),
            ] {
                let now = unix_now().unwrap();
                let context_revert = response.get("error").is_some();
                let mut responses =
                    vec![json!(format!("0x{:x}", chain.id())), header(123, now, 1), response];
                if !context_revert {
                    responses.push(header(123, now, 1));
                }
                let (url, server) = mock_rpc(responses).await;
                let error = client_on(chain, url)
                    .request_binding_quote(&params())
                    .await
                    .unwrap_err();
                assert!(!error
                    .to_string()
                    .contains("secret-provider-key"));
                if context_revert {
                    assert!(matches!(error, FeedError::Connection(_)));
                } else {
                    assert!(matches!(error, FeedError::Parsing(_) | FeedError::Connection(_)));
                }
                let requests = server.await.unwrap();
                assert_eq!(requests.len(), if context_revert { 3 } else { 4 });
                assert_pricing_request(&requests[2], &params(), chain);
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request["method"] == "eth_call")
                        .count(),
                    1
                );
            }
        }
    }

    #[tokio::test]
    async fn rejects_wrong_rpc_chain_and_stale_head_before_helper_call() {
        let now = unix_now().unwrap();
        let (url, server) = mock_rpc(vec![json!("0x1")]).await;
        assert!(client(url)
            .fetch_snapshots()
            .await
            .unwrap_err()
            .to_string()
            .contains("8453"));
        assert_eq!(server.await.unwrap().len(), 1);
        let (url, server) = mock_rpc(vec![json!("0x2105"), header(123, now - 11, 1)]).await;
        assert!(client(url)
            .fetch_snapshots()
            .await
            .unwrap_err()
            .to_string()
            .contains("freshness"));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rpc_timeout_covers_waiting_for_response_and_redacts_provider_errors() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let client = ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url(format!("http://{}", listener.local_addr().unwrap()))
            .poll_time(Duration::from_secs(2))
            .quote_timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert!(client
            .fetch_snapshots()
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        server.abort();
        let (url, server) = mock_rpc(vec![json!({"error": {"code": -32000,
            "message": "https://private.example/token=secret"}})])
        .await;
        let error = super::tests::client(url)
            .fetch_snapshots()
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("-32000"));
        assert!(!error.contains("secret"));
        server.await.unwrap();
    }

    #[test]
    fn validates_configuration_and_keeps_endpoint_out_of_debug() {
        assert!(ElfomofiClientBuilder::new(Chain::Ethereum)
            .rpc_url("http://localhost")
            .build()
            .is_err());
        assert!(ElfomofiClientBuilder::new(Chain::Base)
            .build()
            .is_err());
        assert!(ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url("file:///tmp/test")
            .build()
            .is_err());
        assert!(ElfomofiClientBuilder::new(Chain::Base)
            .rpc_url("http://localhost")
            .max_age(Duration::ZERO)
            .build()
            .is_err());
        let client = client("https://rpc.example/secret-token".into());
        assert!(!format!("{client:?}").contains("secret-token"));
    }

    #[tokio::test]
    #[ignore = "Requires RPC_URL for Base; makes read-only RPC calls"]
    async fn live_base_helper_snapshot() {
        let client = ElfomofiClientBuilder::from_env(Chain::Base)
            .poll_time(Duration::from_secs(2))
            .quote_timeout(Duration::from_secs(15))
            .max_age(Duration::from_secs(30))
            .build()
            .unwrap();
        let snapshots = client.fetch_snapshots().await.unwrap();
        assert!(!snapshots.is_empty());
        for snapshot in snapshots {
            snapshot.validate().unwrap();
            snapshot
                .ensure_fresh(unix_now().unwrap())
                .unwrap();
        }
        let quote = client
            .request_binding_quote(&GetAmountOutParams {
                token_in: "0x4200000000000000000000000000000000000006"
                    .parse()
                    .unwrap(),
                token_out: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
                    .parse()
                    .unwrap(),
                amount_in: BigUint::from(1_000_000_000_000_000u64),
                sender: "0xaba5b53b03eafad1c5fc8bd5fc765fc85bb3de67"
                    .parse()
                    .unwrap(),
                receiver: address(4),
                origin: None,
            })
            .await
            .unwrap();
        assert!(quote.amount_out > BigUint::default());
    }
}
