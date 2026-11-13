use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy::primitives::keccak256;
use chrono::{DateTime, NaiveDateTime};
use futures::future::join_all;
use num_bigint::BigUint;
use tracing::{debug, warn};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::{encode_swap_directions, SWAP_DIRECTIONS_ATTRIBUTE},
        Book, BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    protocol::models::ProtocolComponent,
    rfq::protocols::aqua0::{
        client::{backend_component_id, unix_now, Aqua0Client, Aqua0Pool, StateAnswer},
        models::{Aqua0Market, Aqua0StateResponse},
        state::Aqua0State,
        PROTOCOL_SYSTEM, PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// A market list this old is still used while `/markets` fails, so one failed read does not
/// empty the book.
const STALE_MARKETS_FACTOR: u32 = 5;

/// Fetches every Aqua0 market on the chain once per poll: the market list on its own slower
/// cadence, then each listed pool's state. Markets the backend stops listing, or that cannot be
/// quoted right now, are left out of the snapshot, which removes them.
#[derive(derive_more::Debug)]
pub(crate) struct Aqua0BookSource {
    pub(crate) client: Arc<Aqua0Client>,
    pub(crate) book_config: BookFeedConfig,
    /// Sample sizes by token address; a token missing here samples 1 to 10,000 whole units.
    #[debug(skip)]
    pub(crate) sample_sizes: Arc<HashMap<Bytes, Vec<BigUint>>>,
    pub(crate) markets_refresh: Duration,
    #[debug(skip)]
    pub(crate) markets: Mutex<Option<(Instant, Vec<Aqua0Market>)>>,
}

impl Aqua0BookSource {
    pub(crate) fn new(
        client: Arc<Aqua0Client>,
        book_config: BookFeedConfig,
        sample_sizes: Arc<HashMap<Bytes, Vec<BigUint>>>,
        markets_refresh: Duration,
    ) -> Self {
        Self { client, book_config, sample_sizes, markets_refresh, markets: Mutex::new(None) }
    }

    fn cached_markets(&self) -> Option<(Instant, Vec<Aqua0Market>)> {
        self.markets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn listed_markets(&self) -> Result<Vec<Aqua0Market>, FeedError> {
        let cached = self.cached_markets();
        if let Some((at, markets)) = &cached {
            if at.elapsed() < self.markets_refresh {
                return Ok(markets.clone());
            }
        }
        match self.client.fetch_markets().await {
            Ok(markets) => {
                *self
                    .markets
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((Instant::now(), markets.clone()));
                Ok(markets)
            }
            Err(error) if !error.is_fatal() => match cached {
                Some((at, markets))
                    if at.elapsed() < self.markets_refresh * STALE_MARKETS_FACTOR =>
                {
                    warn!(%error, "Aqua0 market list unavailable, using the last one");
                    Ok(markets)
                }
                _ => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    fn samples(&self, token: &Token) -> String {
        match self.sample_sizes.get(&token.address) {
            Some(sizes) if !sizes.is_empty() => sizes
                .iter()
                .map(BigUint::to_string)
                .collect::<Vec<_>>()
                .join(","),
            _ => {
                let unit = BigUint::from(10u32).pow(token.decimals);
                [1u32, 10, 100, 1_000, 10_000]
                    .iter()
                    .map(|whole| (&unit * *whole).to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            }
        }
    }
}

impl HttpSource for Aqua0BookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        // A paused client cannot be quoted, so none of its pools is published until it may be.
        if let Some(left) = self.client.paused_for() {
            debug!(paused_secs = left.as_secs(), "Aqua0 approvals paused, publishing no pools");
            return Ok(BookSnapshot::received_now(Books::new()));
        }
        let markets = self.listed_markets().await?;
        let wanted: Vec<(Aqua0Market, Token, Token)> = markets
            .into_iter()
            .filter(|market| market.chain_id == self.client.chain_id())
            .filter_map(|market| {
                let token0 = self
                    .book_config
                    .tokens
                    .get(&address(&market.tokens[0]).ok()?)?
                    .clone();
                let token1 = self
                    .book_config
                    .tokens
                    .get(&address(&market.tokens[1]).ok()?)?
                    .clone();
                Some((market, token0, token1))
            })
            .collect();

        let answers = join_all(
            wanted
                .iter()
                .map(|(market, token0, token1)| {
                    let (samples0, samples1) = (self.samples(token0), self.samples(token1));
                    async move {
                        self.client
                            .fetch_state(market, &samples0, &samples1)
                            .await
                    }
                }),
        )
        .await;

        let now = unix_now().map_err(|error| FeedError::Fatal(error.to_string()))?;
        let mut books = Books::new();
        let mut failed = 0;
        for ((market, token0, token1), answer) in wanted.iter().zip(answers) {
            match answer {
                Ok(StateAnswer::Quotable(state)) => {
                    match book(&self.client, market, *state, token0, token1, now) {
                        Ok(book) => books.insert(book),
                        Err(why) => warn!(pool = %market.pool_id, %why, "skipping Aqua0 pool"),
                    }
                }
                Ok(StateAnswer::NoLiquidity) => {
                    debug!(pool = %market.pool_id, "Aqua0 pool has no quotable liquidity")
                }
                Err(error) => {
                    failed += 1;
                    warn!(pool = %market.pool_id, %error, "Aqua0 pool state unavailable");
                }
            }
        }
        // Only a poll where every pool failed is a failed poll, which backs off.
        if failed > 0 && failed == wanted.len() {
            return Err(FeedError::Connection(format!(
                "every Aqua0 state request failed ({failed} pools)"
            )));
        }
        Ok(BookSnapshot::received_now(books))
    }
}

/// A 20-byte address from `0x` hex; anything else is refused up front.
fn address(value: &str) -> Result<Bytes, String> {
    let bytes =
        Bytes::from_str(value).map_err(|error| format!("invalid address {value}: {error}"))?;
    if bytes.len() != 20 {
        return Err(format!("{value} is not a 20-byte address"));
    }
    Ok(bytes)
}

/// `keccak256(abi.encode(PoolKey))`: the V4 pool id of the key a fill is encoded with.
pub(crate) fn v4_pool_id(
    currency0: &Bytes,
    currency1: &Bytes,
    fee: u32,
    tick_spacing: i32,
    hooks: &Bytes,
) -> [u8; 32] {
    let mut encoded = Vec::with_capacity(5 * 32);
    let mut word = |bytes: &[u8], fill: u8| {
        encoded.extend(std::iter::repeat_n(fill, 32 - bytes.len()));
        encoded.extend_from_slice(bytes);
    };
    word(currency0, 0);
    word(currency1, 0);
    word(&fee.to_be_bytes(), 0);
    word(&tick_spacing.to_be_bytes(), if tick_spacing < 0 { 0xff } else { 0 });
    word(hooks, 0);
    keccak256(encoded).0
}

/// The book for one pool, after checking that `/state` describes exactly the listed market and
/// that its pool key hashes to the listed pool id, so the fill goes to that pool and hook.
fn book(
    client: &Arc<Aqua0Client>,
    market: &Aqua0Market,
    state: Aqua0StateResponse,
    token0: &Token,
    token1: &Token,
    now: u64,
) -> Result<Book, String> {
    if state.schema_version != "aqua0-rfq-state-v1" ||
        state.protocol_system != PROTOCOL_SYSTEM ||
        state.protocol_type_name != PROTOCOL_TYPE
    {
        return Err("unsupported schema or protocol identity".into());
    }
    let class_id = BigUint::from_str(&state.class_id).map_err(|_| "invalid classId")?;
    if state.chain_id != client.chain_id() ||
        !state
            .pool_id
            .eq_ignore_ascii_case(&market.pool_id) ||
        BigUint::from_str(&market.class_id).ok() != Some(class_id)
    {
        return Err("state does not describe the listed market".into());
    }
    let component_id = backend_component_id(state.chain_id, &state.pool_id, &state.class_id)?;
    if !state
        .component_id
        .eq_ignore_ascii_case(&component_id)
    {
        return Err("componentId does not match the chain, pool and class".into());
    }
    let currency0 = address(&state.tokens[0])?;
    let currency1 = address(&state.tokens[1])?;
    if currency0 != token0.address || currency1 != token1.address {
        return Err("state tokens differ from the listed market".into());
    }
    let hooks = address(&state.hooks)?;
    let pool_manager = address(&state.pool_manager)?;
    if state.fee > 0xFF_FFFF || !(1..=0x7F_FFFF).contains(&state.tick_spacing) {
        return Err("fee or tickSpacing out of range".into());
    }
    let pool_id = Bytes::from_str(&state.pool_id).map_err(|error| error.to_string())?;
    if pool_id.as_ref() != v4_pool_id(&currency0, &currency1, state.fee, state.tick_spacing, &hooks)
    {
        return Err("pool key does not hash to the listed pool id".into());
    }
    if state.expires_at <= now {
        return Err("state is already expired".into());
    }

    let aqua0 = Aqua0State::new(
        token0.clone(),
        token1.clone(),
        state.fee,
        Aqua0Pool {
            component_id: component_id.clone(),
            chain_id: state.chain_id,
            pool_id: state.pool_id.clone(),
            class_id: state.class_id.clone(),
        },
        state.state_version,
        state.expires_at,
        state.directions.zero_for_one,
        state.directions.one_for_zero,
        Arc::clone(client),
    )?;

    let mut static_attributes = HashMap::from([
        ("key_lp_fee".to_string(), Bytes::from(state.fee.to_be_bytes().to_vec())),
        (
            "tick_spacing".to_string(),
            Bytes::from(
                state
                    .tick_spacing
                    .to_be_bytes()
                    .to_vec(),
            ),
        ),
        ("hooks".to_string(), hooks.clone()),
    ]);
    let directions = aqua0.directions();
    if directions.len() < 2 {
        static_attributes
            .insert(SWAP_DIRECTIONS_ATTRIBUTE.to_string(), encode_swap_directions(&directions));
    }
    let component = ProtocolComponent::new(
        Bytes::from_str(&component_id).map_err(|error| error.to_string())?,
        PROTOCOL_SYSTEM.to_string(),
        PROTOCOL_TYPE.to_string(),
        token0.chain,
        vec![token0.clone(), token1.clone()],
        vec![pool_manager, hooks],
        static_attributes,
        Bytes::default(),
        NaiveDateTime::default(),
    );
    Ok(Book {
        component,
        state: Arc::new(aqua0),
        updated_at: i64::try_from(state.generated_at)
            .ok()
            .and_then(|at| DateTime::from_timestamp(at, 0)),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        book::decode_swap_directions, snapshot_feed::http::test_support::spawn_http_server,
    };

    /// The backend's `componentId` in the recorded Polygon `/state` answer.
    pub(crate) const POLYGON_COMPONENT_ID: &str =
        "0x9e8a3c65a8e5a3c3bf90e59f2d41e5adf3264d39153b31d2da004ffc138b8b81";
    const POLYGON_STATE: &str = include_str!("testdata/polygon_state.json");
    const ARBITRUM_STATE: &str = include_str!("testdata/arbitrum_state.json");

    fn recorded(raw: &str) -> Aqua0StateResponse {
        let mut state: Aqua0StateResponse = serde_json::from_str(raw).unwrap();
        state.expires_at = u64::MAX;
        state
    }

    fn market_of(state: &Aqua0StateResponse) -> Aqua0Market {
        Aqua0Market {
            chain_id: state.chain_id,
            pool_id: state.pool_id.clone(),
            class_id: state.class_id.clone(),
            tokens: state.tokens.clone(),
        }
    }

    fn token(address: &str, chain: Chain) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), "T", 6, 0, &[Some(10_000)], chain, 100)
    }

    fn client(url: &str, chain_id: u64) -> Arc<Aqua0Client> {
        Arc::new(Aqua0Client::new(url, chain_id, Duration::from_secs(5), Duration::from_secs(5)))
    }

    fn polygon_book(state: Aqua0StateResponse) -> Result<Book, String> {
        let market = market_of(&recorded(POLYGON_STATE));
        book(
            &client("http://localhost", 137),
            &market,
            state,
            &token(&market.tokens[0], Chain::Polygon),
            &token(&market.tokens[1], Chain::Polygon),
            0,
        )
    }

    /// Production pool ids, as `/markets` lists them, from the key `/state` returns.
    #[test]
    fn pool_keys_hash_to_production_pool_ids() {
        for raw in [POLYGON_STATE, ARBITRUM_STATE] {
            let state = recorded(raw);
            let id = v4_pool_id(
                &address(&state.tokens[0]).unwrap(),
                &address(&state.tokens[1]).unwrap(),
                state.fee,
                state.tick_spacing,
                &address(&state.hooks).unwrap(),
            );
            assert_eq!(Bytes::from(id.to_vec()).to_string(), state.pool_id.to_lowercase());
        }
    }

    #[test]
    fn a_recorded_state_becomes_an_encodable_book() {
        let book = polygon_book(recorded(POLYGON_STATE)).unwrap();
        let attributes = &book.component.static_attributes;
        assert_eq!(book.component.id.to_string(), POLYGON_COMPONENT_ID);
        assert_eq!(book.component.protocol_system, "rfq:aqua0");
        assert_eq!(attributes["hooks"].to_string(), "0x246c81f41daaabd7e0f581112a4e072ef026a8c0");
        assert_eq!(attributes["key_lp_fee"].as_ref(), 500u32.to_be_bytes());
        assert_eq!(attributes["tick_spacing"].as_ref(), 10i32.to_be_bytes());
        // Only token1 -> token0 is backed in this recording.
        let tokens = &book.component.tokens;
        assert_eq!(
            decode_swap_directions(&attributes[SWAP_DIRECTIONS_ATTRIBUTE]).unwrap(),
            vec![(tokens[1].address.clone(), tokens[0].address.clone())]
        );
    }

    #[test]
    fn a_state_that_does_not_match_its_market_is_refused() {
        type Change = fn(&mut Aqua0StateResponse);
        let changes: [(&str, Change); 9] = [
            ("hook swapped", |s| s.hooks = format!("0x{}", "ab".repeat(20))),
            ("fee changed", |s| s.fee = 3000),
            ("tick spacing", |s| s.tick_spacing = 60),
            ("short hook", |s| s.hooks = "0x1234".into()),
            ("short manager", |s| s.pool_manager = "0x1234".into()),
            ("other pool", |s| s.pool_id = format!("0x{}", "11".repeat(32))),
            ("other class", |s| s.class_id = "7".into()),
            ("component id", |s| s.component_id = format!("0x{}", "22".repeat(32))),
            ("bad level", |s| s.directions.one_for_zero[0].amount_out = "x".into()),
        ];
        for (what, change) in changes {
            let mut state = recorded(POLYGON_STATE);
            change(&mut state);
            assert!(polygon_book(state).is_err(), "{what} was accepted");
        }
    }

    /// A poll publishes the quotable markets, leaves out the empty and the malformed ones, and
    /// keeps the market list between refreshes.
    #[tokio::test]
    async fn a_poll_publishes_quotable_markets_and_skips_the_rest() {
        let polygon = recorded(POLYGON_STATE);
        let good = market_of(&polygon);
        let empty_pool = format!("0x{}", "33".repeat(32));
        let broken_pool = format!("0x{}", "44".repeat(32));
        let markets = serde_json::json!({ "markets": [
            { "chainId": 137, "poolId": good.pool_id, "classId": good.class_id, "tokens": good.tokens },
            { "chainId": 137, "poolId": empty_pool, "classId": "1", "tokens": good.tokens },
            { "chainId": 137, "poolId": broken_pool, "classId": "1", "tokens": good.tokens },
        ]})
        .to_string();
        let mut state_json: serde_json::Value = serde_json::from_str(POLYGON_STATE).unwrap();
        state_json["expiresAt"] = u64::MAX.into();
        let state_json = state_json.to_string();
        let market_reads = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&market_reads);
        let server = spawn_http_server(move |request| {
            let target = &request.target;
            if target.starts_with("/markets") {
                reads.fetch_add(1, Ordering::SeqCst);
                return Some(("200 OK", markets.clone()));
            }
            if target.contains(&empty_pool) {
                return Some((
                    "400 Bad Request",
                    r#"{"error":"RFQ_REFUSED","code":"RFQ_REFUSED","message":"no quotable Aqua0 JIT liquidity for pool"}"#
                        .to_string(),
                ));
            }
            if target.contains(&broken_pool) {
                return Some(("200 OK", r#"{"schemaVersion":"aqua0-rfq-state-v1"}"#.to_string()));
            }
            Some(("200 OK", state_json.clone()))
        })
        .await;
        let tokens: HashMap<Bytes, Token> = good
            .tokens
            .iter()
            .map(|address| (Bytes::from_str(address).unwrap(), token(address, Chain::Polygon)))
            .collect();
        let source = Aqua0BookSource::new(
            client(&server.url(), 137),
            BookFeedConfig { chain: Chain::Polygon, tokens: Arc::new(tokens), min_tvl_usd: 0.0 },
            Arc::new(HashMap::new()),
            Duration::from_secs(60),
        );

        let books = source.fetch().await.unwrap().books;
        assert_eq!(
            books
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![POLYGON_COMPONENT_ID]
        );
        source.fetch().await.unwrap();
        assert_eq!(market_reads.load(Ordering::SeqCst), 1, "markets refresh on their own cadence");
    }

    #[tokio::test]
    async fn a_paused_client_publishes_nothing_and_reads_nothing() {
        let server = spawn_http_server(|_| {
            Some((
                "429 Too Many Requests",
                r#"{"code":"RATE_LIMITED","retryAfterSeconds":60}"#.to_string(),
            ))
        })
        .await;
        let client = client(&server.url(), 137);
        let params = tycho_common::models::protocol::GetAmountOutParams {
            amount_in: 1u32.into(),
            token_in: Bytes::zero(20),
            token_out: Bytes::zero(20),
            sender: Bytes::zero(20),
            receiver: Bytes::zero(20),
            origin: None,
        };
        let pool = Aqua0Pool {
            component_id: POLYGON_COMPONENT_ID.into(),
            chain_id: 137,
            pool_id: "0x00".into(),
            class_id: "0".into(),
        };
        assert!(client
            .request_binding_quote(&params, &pool)
            .await
            .is_err());
        let source = Aqua0BookSource::new(
            Arc::clone(&client),
            BookFeedConfig {
                chain: Chain::Polygon,
                tokens: Arc::new(HashMap::new()),
                min_tvl_usd: 0.0,
            },
            Arc::new(HashMap::new()),
            Duration::from_secs(60),
        );
        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
        assert_eq!(server.request_count(), 1, "only the refused quote reached the server");
    }

    #[test]
    fn default_samples_follow_token_decimals() {
        let source = Aqua0BookSource::new(
            client("http://localhost", 137),
            BookFeedConfig {
                chain: Chain::Polygon,
                tokens: Arc::new(HashMap::new()),
                min_tvl_usd: 0.0,
            },
            Arc::new(HashMap::from([(Bytes::zero(20), vec![BigUint::from(5u32)])])),
            Duration::from_secs(60),
        );
        assert_eq!(
            source.samples(&token("0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359", Chain::Polygon)),
            "1000000,10000000,100000000,1000000000,10000000000"
        );
        assert_eq!(
            source.samples(&Token::new(&Bytes::zero(20), "Z", 18, 0, &[], Chain::Polygon, 100)),
            "5"
        );
    }
}
