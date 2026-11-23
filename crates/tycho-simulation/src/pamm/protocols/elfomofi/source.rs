//! One block-pinned helper response becomes one complete book snapshot.
use std::collections::{HashMap, HashSet};

use alloy::sol_types::SolCall;
use num_traits::ToPrimitive;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::{
    all_pairs::ElfomofiAllPairsState,
    client::{unix_now, ElfomofiClient},
    models::{decode_orderbooks, getAllOrderbooksCall, helper_for, Snapshot},
    state::ElfomofiState,
    PROTOCOL_SYSTEM, PROTOCOL_TYPE,
};
use crate::{
    book::{
        all_pairs::{BookCollector, ComponentLayout, SignedByVenue, Venue, VenueBook},
        component::pair_component_id,
        levels::{Levels, PriceLevel},
        quote_tokens::usd_stablecoins_for_chain,
        tvl::tvl_in_usd,
        BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

#[derive(Debug)]
pub(super) struct ElfomofiBookSource {
    book_config: BookFeedConfig,
    client: ElfomofiClient,
    max_age_seconds: u64,
    layout: ComponentLayout<()>,
}

impl ElfomofiBookSource {
    pub fn new(
        book_config: BookFeedConfig,
        client: ElfomofiClient,
        max_age_seconds: u64,
        layout: ComponentLayout<()>,
    ) -> Self {
        Self { book_config, client, max_age_seconds, layout }
    }
    pub fn chain(&self) -> Chain {
        self.book_config.chain
    }

    fn priced_pairs(
        snapshots: &[Snapshot],
        tokens: &HashMap<Bytes, Token>,
    ) -> Vec<(Bytes, Bytes, Levels)> {
        let mut pairs: Vec<_> = snapshots
            .iter()
            .flat_map(|snapshot| {
                let book = &snapshot.book;
                [
                    (&book.base_token, &book.quote_token, &book.bids),
                    (&book.quote_token, &book.base_token, &book.asks),
                ]
            })
            .filter_map(|(sell, buy, levels)| {
                let input_scale = 10_f64.powf(f64::from(tokens.get(sell)?.decimals));
                let output_scale = 10_f64.powf(f64::from(tokens.get(buy)?.decimals));
                let first = levels.first()?;
                let price = first.amount_out.to_f64()? / first.amount_in.to_f64()? * input_scale /
                    output_scale;
                let quantity = levels.last()?.amount_in.to_f64()? / input_scale;
                let levels = Levels::new(vec![PriceLevel { price, quantity }]).ok()?;
                Some((sell.clone(), buy.clone(), levels))
            })
            .collect();
        // A published side also values its assets in reverse. This is only TVL conversion;
        // PairState still advertises exactly the directions the helper can fill.
        let directions: HashSet<_> = pairs
            .iter()
            .map(|(sell, buy, _)| (sell.clone(), buy.clone()))
            .collect();
        let inverse: Vec<_> = pairs
            .iter()
            .filter(|(sell, buy, _)| !directions.contains(&(buy.clone(), sell.clone())))
            .map(|(sell, buy, levels)| (buy.clone(), sell.clone(), levels.invert()))
            .collect();
        pairs.extend(inverse);
        pairs
    }

    fn clears_tvl(
        &self,
        snapshot: &Snapshot,
        base: &Token,
        quote: &Token,
        priced_pairs: &[(Bytes, Bytes, Levels)],
    ) -> bool {
        if self.book_config.min_tvl_usd == 0.0 {
            return true;
        }
        let Some(usd_tokens) = usd_stablecoins_for_chain(self.chain()) else {
            return false;
        };
        let value = |balance: &num_bigint::BigUint, token: &Token| {
            let raw = balance.to_f64()? / 10_f64.powf(f64::from(token.decimals));
            if raw == 0.0 {
                return Some(0.0);
            }
            tvl_in_usd(
                raw,
                &token.address,
                &usd_tokens,
                priced_pairs
                    .iter()
                    .map(|(base, quote, levels)| (base, quote, levels)),
            )
        };
        let total = value(&snapshot.book.base_balance, base)
            .zip(value(&snapshot.book.quote_balance, quote))
            .map(|(base, quote)| base + quote)
            .filter(|value| value.is_finite())
            .unwrap_or(0.0);
        let (first, second) = if base.address < quote.address {
            (&base.address, &quote.address)
        } else {
            (&quote.address, &base.address)
        };
        self.book_config
            .clears_min_tvl(total, &pair_component_id(PROTOCOL_SYSTEM, first, second))
    }
}

impl HttpSource for ElfomofiBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    fn snapshot_deadline(&self, snapshot: &Self::Snapshot) -> Option<tokio::time::Instant> {
        let block_timestamp = snapshot
            .books
            .values()
            .filter_map(|book| {
                let state = book.state.as_any();
                state
                    .downcast_ref::<ElfomofiAllPairsState>()
                    .and_then(ElfomofiAllPairsState::block_timestamp)
                    .or_else(|| {
                        state
                            .downcast_ref::<ElfomofiState>()
                            .map(|state| state.snapshot().block_timestamp)
                    })
            })
            .min()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let deadline =
            std::time::Duration::from_secs(block_timestamp.saturating_add(self.max_age_seconds));
        Some(tokio::time::Instant::now() + deadline.saturating_sub(now))
    }

    async fn fetch(&self) -> Result<Self::Snapshot, FeedError> {
        self.client.verify_chain().await?;
        let header = self.client.header("latest").await?;
        super::models::ensure_timestamp_fresh(header.timestamp, unix_now()?, self.max_age_seconds)?;
        let raw = self
            .client
            .call_at(
                helper_for(self.chain())?,
                &getAllOrderbooksCall {}.abi_encode(),
                None,
                &header,
            )
            .await?;
        let mut snapshots = decode_orderbooks(
            self.chain(),
            &raw,
            header.number,
            &header.hash,
            header.timestamp,
            self.max_age_seconds,
        )?;
        snapshots.retain(|snapshot| {
            let book = &snapshot.book;
            let Some((base, quote)) = self
                .book_config
                .pair_tokens(&book.base_token, &book.quote_token)
            else {
                return false;
            };
            let valid = |address: &Bytes, token: &Token| {
                token.address == *address && token.chain == self.chain() && token.decimals <= 255
            };
            let valid = valid(&book.base_token, base) && valid(&book.quote_token, quote);
            if !valid {
                tracing::warn!(base = %book.base_token, quote = %book.quote_token,
                    "Skipping ElFomoFi pair with inconsistent configured token metadata");
            }
            valid
        });
        let known = &self.book_config.tokens;
        let priced_pairs = Self::priced_pairs(&snapshots, known);
        let mut collector = BookCollector::<ElfomofiState, SignedByVenue>::new(
            PROTOCOL_SYSTEM,
            PROTOCOL_TYPE,
            self.chain(),
            self.layout,
        );
        for snapshot in snapshots {
            let (Some(base), Some(quote)) =
                (known.get(&snapshot.book.base_token), known.get(&snapshot.book.quote_token))
            else {
                continue;
            };
            if !self.clears_tvl(&snapshot, base, quote, &priced_pairs) {
                continue;
            }
            let component_id = snapshot
                .book
                .component_id(self.chain())?;
            let state = match ElfomofiState::from_snapshot(snapshot, base.clone(), quote.clone()) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(base = %base.address, quote = %quote.address, %error,
                        "Skipping invalid ElFomoFi book state");
                    continue;
                }
            };
            collector.push(component_id, VenueBook { signer: Venue, state, updated_at: None });
        }
        let mut books = collector
            .finish::<ElfomofiAllPairsState>()
            .into_map();
        for book in books.values_mut() {
            book.component
                .static_attributes
                .remove(crate::book::all_pairs::QuoteRule::ATTRIBUTE);
            book.component
                .static_attributes
                .insert("max_swaps_per_solution".into(), Bytes::from(1u64.to_be_bytes().to_vec()));
        }
        super::models::ensure_timestamp_fresh(header.timestamp, unix_now()?, self.max_age_seconds)?;
        Ok(BookSnapshot {
            anchor: ReceivedAt(chrono::Utc::now()),
            books: std::sync::Arc::new(books),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use alloy::{
        primitives::{Address, B256, U256},
        sol_types::SolCall,
    };
    use futures::StreamExt;
    use serde_json::{json, Value};
    use tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::{
        super::{
            feed::ElfomofiFeedBuilder,
            models::{getAllOrderbooksReturn, CumulativeLevel, PairOrderbook},
        },
        *,
    };
    use crate::snapshot_feed::http::test_support::spawn_http_server;

    #[derive(Default)]
    struct Calls {
        chain: AtomicUsize,
        headers: AtomicUsize,
        helper: AtomicUsize,
    }

    fn addr(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }
    fn token(byte: u8) -> Token {
        Token::new(&addr(byte), "KNOWN", 0, 0, &[], Chain::Base, 100)
    }
    fn config() -> BookFeedConfig {
        BookFeedConfig {
            chain: Chain::Base,
            tokens: Arc::new(
                [(addr(1), token(1)), (addr(2), token(2)), (addr(3), token(3))].into(),
            ),
            min_tvl_usd: 0.0,
        }
    }
    fn helper(pairs: &[(u8, u8)], timestamp: u64) -> Value {
        let raw = getAllOrderbooksReturn {
            books: pairs
                .iter()
                .map(|(base, quote)| PairOrderbook {
                    base: Address::from_slice(&addr(*base)),
                    quote: Address::from_slice(&addr(*quote)),
                    askCumulativeLevels: vec![CumulativeLevel {
                        amountIn: U256::from(100),
                        amountOut: U256::from(40),
                    }],
                    bidCumulativeLevels: vec![CumulativeLevel {
                        amountIn: U256::from(100),
                        amountOut: U256::from(200),
                    }],
                    balanceBase: U256::from(1000),
                    balanceQuote: U256::from(1000),
                })
                .collect(),
            blockNumber: U256::from(100),
            blockTimestamp: U256::from(timestamp),
        };
        json!(format!("0x{}", hex::encode(getAllOrderbooksCall::abi_encode_returns(&raw))))
    }

    async fn mock(
        pairs: Arc<Mutex<Vec<(u8, u8)>>>,
        reorg: Arc<AtomicBool>,
        chain: Chain,
        timestamp_offset: i64,
        helper_delay: std::time::Duration,
    ) -> (String, Arc<Calls>) {
        let timestamp = AtomicU64::new(unix_now().unwrap());
        let calls = Arc::new(Calls::default());
        let recorded = calls.clone();
        let server = spawn_http_server(move |request| {
            let req: Value = serde_json::from_str(&request.body).unwrap();
            let result = match req["method"].as_str().unwrap() {
                "eth_chainId" => {
                    recorded.chain.fetch_add(1, Ordering::SeqCst);
                    json!(format!("0x{:x}", chain.id()))
                },
                "eth_getBlockByNumber" => {
                    recorded.headers.fetch_add(1, Ordering::SeqCst);
                    let now = unix_now().unwrap().saturating_add_signed(timestamp_offset);
                    timestamp.store(now, Ordering::SeqCst);
                    let mut block: alloy::rpc::types::Block = Default::default();
                    block.header.number = 100;
                    block.header.timestamp = now;
                    block.header.hash = B256::repeat_byte(1);
                    json!(block)
                }
                "eth_call" => {
                    recorded.helper.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(req["params"][1], json!({"blockHash":format!("0x{}", "01".repeat(32)), "requireCanonical":true}));
                    if reorg.load(Ordering::SeqCst) {
                        return Some(("200 OK", json!({"jsonrpc":"2.0", "id":req["id"], "error":{"code":-32000,"message":"not canonical"}}).to_string()));
                    }
                    let data = req["params"][0].get("input").or_else(|| req["params"][0].get("data")).and_then(Value::as_str).unwrap();
                    assert_eq!(data, format!("0x{}", hex::encode(getAllOrderbooksCall {}.abi_encode())), "only the public helper is read");
                    std::thread::sleep(helper_delay);
                    helper(&pairs.lock().unwrap(), timestamp.load(Ordering::SeqCst))
                }
                method => panic!("unexpected RPC {method}"),
            };
            Some(("200 OK", json!({"jsonrpc":"2.0", "id":req["id"],"result":result}).to_string()))
        }).await;
        (server.url(), calls)
    }

    fn source(config: BookFeedConfig, url: &str, max_age: u64) -> ElfomofiBookSource {
        let client = ElfomofiClient::new(config.chain, url).unwrap();
        ElfomofiBookSource::new(config, client, max_age, ComponentLayout::AllPairs(()))
    }

    fn published_pairs(
        snapshot: &BookSnapshot<ReceivedAt>,
    ) -> std::collections::BTreeSet<(Bytes, Bytes)> {
        use crate::book::{decode_swap_directions, SWAP_DIRECTIONS_ATTRIBUTE};
        snapshot
            .books
            .values()
            .flat_map(|book| {
                decode_swap_directions(&book.component.static_attributes[SWAP_DIRECTIONS_ATTRIBUTE])
                    .unwrap()
            })
            .map(|(sell, buy)| if sell < buy { (sell, buy) } else { (buy, sell) })
            .collect()
    }

    #[tokio::test]
    async fn complete_snapshots_add_remove_replace_and_readd_configured_pairs() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2)]));
        let (url, calls) = mock(
            pairs.clone(),
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::ZERO,
        )
        .await;
        let source = source(config(), &url, 30);
        let first = source.fetch().await.unwrap();
        assert_eq!(published_pairs(&first), [(addr(1), addr(2))].into());
        let first_id = first
            .books
            .keys()
            .next()
            .unwrap()
            .clone();
        *pairs.lock().unwrap() = vec![(1, 2), (1, 3), (1, 4)];
        let added = source.fetch().await.unwrap();
        assert_eq!(added.books.len(), 1);
        assert_eq!(published_pairs(&added), [(addr(1), addr(2)), (addr(1), addr(3))].into());
        *pairs.lock().unwrap() = vec![(2, 3)];
        let replaced = source.fetch().await.unwrap();
        assert!(replaced.books.contains_key(&first_id));
        assert_eq!(published_pairs(&replaced), [(addr(2), addr(3))].into());
        *pairs.lock().unwrap() = vec![];
        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
        *pairs.lock().unwrap() = vec![(2, 1)];
        let readded = source.fetch().await.unwrap();
        assert!(readded.books.contains_key(&first_id));
        assert_eq!(published_pairs(&readded), [(addr(1), addr(2))].into());
        assert_eq!(calls.chain.load(Ordering::SeqCst), 1);
        assert_eq!(calls.headers.load(Ordering::SeqCst), 5);
        assert_eq!(calls.helper.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn configured_token_universe_is_always_respected() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2), (1, 4)]));
        let (url, calls) = mock(
            pairs.clone(),
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::ZERO,
        )
        .await;
        let source = source(config(), &url, 30);
        assert_eq!(published_pairs(&source.fetch().await.unwrap()), [(addr(1), addr(2))].into());
        *pairs.lock().unwrap() = vec![(1, 4)];
        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
        assert_eq!(calls.headers.load(Ordering::SeqCst), 2);
        assert_eq!(calls.helper.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn malformed_or_duplicate_books_fail_only_when_every_book_is_invalid() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2), (0, 2), (3, 3)]));
        let (url, _) = mock(
            pairs.clone(),
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::ZERO,
        )
        .await;
        let source = source(config(), &url, 30);
        assert_eq!(published_pairs(&source.fetch().await.unwrap()), [(addr(1), addr(2))].into());
        for invalid in [vec![(0, 2), (3, 3)], vec![(1, 2), (2, 1)]] {
            *pairs.lock().unwrap() = invalid;
            let error = source.fetch().await.unwrap_err();
            assert!(error
                .to_string()
                .contains("none were valid"));
        }
        *pairs.lock().unwrap() = vec![];
        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
    }

    #[tokio::test]
    async fn bsc_publishes_with_bounded_sequencer_clock_skew() {
        for offset in [0, 1, 2] {
            let pairs = Arc::new(Mutex::new(vec![(1, 2)]));
            let (url, _) = mock(
                pairs,
                Arc::new(AtomicBool::new(false)),
                Chain::Bsc,
                offset,
                std::time::Duration::ZERO,
            )
            .await;
            let mut config = config();
            config.chain = Chain::Bsc;
            for token in Arc::make_mut(&mut config.tokens).values_mut() {
                token.chain = Chain::Bsc;
            }
            let source = source(config, &url, 2);
            assert_eq!(
                source
                    .fetch()
                    .await
                    .unwrap()
                    .books
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn a_far_future_or_stale_header_does_not_read_the_helper() {
        for offset in [-10, 10] {
            let pairs = Arc::new(Mutex::new(vec![(1, 2)]));
            let (url, calls) = mock(
                pairs,
                Arc::new(AtomicBool::new(false)),
                Chain::Base,
                offset,
                std::time::Duration::ZERO,
            )
            .await;
            assert!(source(config(), &url, 2)
                .fetch()
                .await
                .is_err());
            assert_eq!(calls.helper.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_helper_response_that_aged_during_the_rpc_is_not_published() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2)]));
        let (url, _) = mock(
            pairs,
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::from_millis(1100),
        )
        .await;
        let error = source(config(), &url, 1)
            .fetch()
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("freshness window"));
    }

    #[tokio::test]
    async fn canonical_pinning_rejects_a_reorg() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2)]));
        let reorg = Arc::new(AtomicBool::new(true));
        let (url, _) = mock(pairs, reorg.clone(), Chain::Base, 0, std::time::Duration::ZERO).await;
        let source = source(config(), &url, 30);
        assert!(source.fetch().await.is_err());
        reorg.store(false, Ordering::SeqCst);
        assert_eq!(
            source
                .fetch()
                .await
                .unwrap()
                .books
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn wrong_chain_metadata_only_filters_its_own_pairs() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2), (1, 3)]));
        let (url, _) = mock(
            pairs,
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::ZERO,
        )
        .await;
        let mut config = config();
        Arc::make_mut(&mut config.tokens)
            .get_mut(&addr(3))
            .unwrap()
            .chain = Chain::Bsc;
        assert_eq!(
            published_pairs(
                &source(config, &url, 30)
                    .fetch()
                    .await
                    .unwrap()
            ),
            [(addr(1), addr(2))].into()
        );
    }

    #[tokio::test]
    async fn per_pair_layout_publishes_pair_states_and_refuses_repeated_fills() {
        let pairs = Arc::new(Mutex::new(vec![(1, 2), (1, 3)]));
        let (url, _) = mock(
            pairs,
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::ZERO,
        )
        .await;
        let client = ElfomofiClient::new(Chain::Base, &url).unwrap();
        let source = ElfomofiBookSource::new(config(), client, 30, ComponentLayout::PerPair);
        let snapshot = source.fetch().await.unwrap();
        assert_eq!(snapshot.books.len(), 2);
        for book in snapshot.books.values() {
            let state = book
                .state
                .as_any()
                .downcast_ref::<ElfomofiState>()
                .unwrap();
            let (base, quote) = (&state.base_token, &state.quote_token);
            let next = state
                .get_amount_out(10u64.into(), base, quote)
                .unwrap()
                .new_state;
            assert!(next
                .get_amount_out(10u64.into(), base, quote)
                .is_err());
        }
    }

    #[tokio::test]
    async fn snapshot_deadline_follows_the_block_in_both_layouts() {
        for layout in [ComponentLayout::PerPair, ComponentLayout::AllPairs(())] {
            let pairs = Arc::new(Mutex::new(vec![(1, 2)]));
            let (url, _) = mock(
                pairs,
                Arc::new(AtomicBool::new(false)),
                Chain::Base,
                -1,
                std::time::Duration::ZERO,
            )
            .await;
            let client = ElfomofiClient::new(Chain::Base, &url).unwrap();
            let source = ElfomofiBookSource::new(config(), client, 30, layout);
            let snapshot = source.fetch().await.unwrap();
            let remaining = source
                .snapshot_deadline(&snapshot)
                .unwrap()
                .saturating_duration_since(tokio::time::Instant::now());
            assert!(remaining <= std::time::Duration::from_secs(29));
            assert!(remaining > std::time::Duration::from_secs(27));
        }
    }

    #[tokio::test]
    async fn default_feed_needs_only_the_http_endpoint_and_redacts_it() {
        use crate::snapshot_feed::{SnapshotFeedEvent, SnapshotFeedStream};
        let pairs = Arc::new(Mutex::new(vec![(1, 2), (1, 4)]));
        let (url, _) = mock(
            pairs,
            Arc::new(AtomicBool::new(false)),
            Chain::Base,
            0,
            std::time::Duration::ZERO,
        )
        .await;
        let feed = ElfomofiFeedBuilder::new(config(), url)
            .build()
            .unwrap();
        let mut stream = SnapshotFeedStream::spawn("test", feed);
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        let SnapshotFeedEvent::Published(snapshot) = event else {
            panic!("expected books");
        };
        assert_eq!(published_pairs(&snapshot), [(addr(1), addr(2))].into());
        assert!(snapshot
            .books
            .values()
            .all(|book| book.updated_at.is_none()));
        let feed =
            ElfomofiFeedBuilder::new(config(), "https://example.test/private-provider-key".into())
                .build()
                .unwrap();
        assert!(!format!("{feed:?}").contains("private-provider-key"));
    }

    #[test]
    fn usd_tvl_uses_shared_price_conversion_and_configured_floor() {
        let usd: Bytes = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
            .parse()
            .unwrap();
        let timestamp = unix_now().unwrap();
        let raw = helper(&[(1, 2)], timestamp);
        let bytes = hex::decode(
            raw.as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap();
        let mut snapshot =
            decode_orderbooks(Chain::Base, &bytes, 100, &Bytes::from(vec![1; 32]), timestamp, 30)
                .unwrap()
                .remove(0);
        snapshot.book.quote_token = usd.clone();
        let base = token(1);
        let quote = Token::new(&usd, "USDC", 0, 0, &[], Chain::Base, 100);
        let known = [(base.address.clone(), base.clone()), (usd, quote.clone())].into();
        let prices = ElfomofiBookSource::priced_pairs(std::slice::from_ref(&snapshot), &known);
        let mut config = config();
        config.min_tvl_usd = 3000.0;
        let mut source = source(config, "https://example.test", 30);
        assert!(source.clears_tvl(&snapshot, &base, &quote, &prices));
        source.book_config.min_tvl_usd = 3001.0;
        assert!(!source.clears_tvl(&snapshot, &base, &quote, &prices));
        assert!(!source.clears_tvl(&snapshot, &base, &quote, &[]));
    }

    #[test]
    fn an_asks_only_robinhood_book_clears_a_positive_usd_floor() {
        use crate::book::component::PairState;
        let usd: Bytes = "0x5fc5360d0400a0fd4f2af552add042d716f1d168"
            .parse()
            .unwrap();
        let snapshot = Snapshot {
            chain: Chain::Robinhood,
            block_number: 100,
            block_hash: Bytes::from(vec![1; 32]),
            block_timestamp: unix_now().unwrap(),
            max_age_seconds: 30,
            book: super::super::models::Book {
                base_token: addr(1),
                quote_token: usd.clone(),
                bids: vec![],
                asks: vec![super::super::models::Level {
                    amount_in: 200u64.into(),
                    amount_out: 100u64.into(),
                }],
                base_balance: 1000u64.into(),
                quote_balance: 1000u64.into(),
            },
        };
        let base = Token::new(&addr(1), "WETH", 0, 0, &[], Chain::Robinhood, 100);
        let quote = Token::new(&usd, "USDG", 0, 0, &[], Chain::Robinhood, 100);
        let tokens: HashMap<_, _> = [(addr(1), base.clone()), (usd.clone(), quote.clone())].into();
        let prices = ElfomofiBookSource::priced_pairs(std::slice::from_ref(&snapshot), &tokens);
        let source = source(
            BookFeedConfig {
                chain: Chain::Robinhood,
                tokens: Arc::new(tokens),
                min_tvl_usd: 3000.0,
            },
            "https://example.test",
            30,
        );
        assert!(source.clears_tvl(&snapshot, &base, &quote, &prices));
        let state = ElfomofiState::from_snapshot(snapshot, base, quote).unwrap();
        assert_eq!(state.directions(), vec![(usd, addr(1))]);
    }
}
