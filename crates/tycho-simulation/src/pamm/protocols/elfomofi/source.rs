//! One block-pinned helper response becomes one complete book snapshot.
use std::collections::{HashMap, HashSet};

use alloy::sol_types::SolCall;
use num_traits::ToPrimitive;
use tokio::sync::Mutex;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::{
    client::{unix_now, ElfomofiClient},
    models::{decode_orderbooks, getAllOrderbooksCall, helper_for, Snapshot},
    state::ElfomofiState,
    tokens, PROTOCOL_SYSTEM, PROTOCOL_TYPE,
};
use crate::{
    book::{
        component::{pair_book, pair_component_id},
        levels::{Levels, PriceLevel},
        quote_tokens::usd_stablecoins_for_chain,
        tvl::tvl_in_usd,
        BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

#[derive(Debug)]
pub(super) struct ElfomofiBookSource {
    book_config: BookFeedConfig,
    client: ElfomofiClient,
    max_age_seconds: u64,
    discover_tokens: bool,
    discovered: Mutex<HashMap<Bytes, Token>>,
}

impl ElfomofiBookSource {
    pub fn new(
        book_config: BookFeedConfig,
        client: ElfomofiClient,
        max_age_seconds: u64,
        discover_tokens: bool,
    ) -> Self {
        Self {
            book_config,
            client,
            max_age_seconds,
            discover_tokens,
            discovered: Mutex::new(HashMap::new()),
        }
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
                let input_scale = 10_f64.powi(tokens.get(sell)?.decimals as i32);
                let output_scale = 10_f64.powi(tokens.get(buy)?.decimals as i32);
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
            let raw = balance.to_f64()? / 10_f64.powi(token.decimals as i32);
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

    async fn fetch(&self) -> Result<Self::Snapshot, FeedError> {
        self.client.verify_chain().await?;
        let header = self.client.header("latest").await?;
        // A sequencer can announce the next integer second slightly before the local clock.
        let now = std::time::SystemTime::now();
        let current_second = unix_now()?;
        if header.timestamp == current_second.saturating_add(1) {
            let boundary = std::time::UNIX_EPOCH + std::time::Duration::from_secs(header.timestamp);
            if let Ok(delay) = boundary.duration_since(now) {
                tokio::time::sleep(delay).await;
            }
        }
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
            header.hash.clone(),
            header.timestamp,
            unix_now()?,
            self.max_age_seconds,
        )?;
        let cached = self.discovered.lock().await;
        let mut known: HashMap<_, _> = snapshots
            .iter()
            .flat_map(|snapshot| [&snapshot.book.base_token, &snapshot.book.quote_token])
            .filter_map(|address| {
                self.book_config
                    .tokens
                    .get(address)
                    .or_else(|| cached.get(address))
                    .map(|token| (address.clone(), token.clone()))
            })
            .collect();
        drop(cached);
        if !self.discover_tokens {
            snapshots.retain(|snapshot| {
                self.book_config
                    .pair_tokens(&snapshot.book.base_token, &snapshot.book.quote_token)
                    .is_some()
            });
        }
        let new_tokens = if self.discover_tokens {
            tokens::discover(&self.client, self.chain(), &header, &snapshots, &known).await?
        } else {
            HashMap::new()
        };
        known.extend(new_tokens.clone());
        self.client
            .verify_header(&header, self.max_age_seconds)
            .await?;
        let priced_pairs = Self::priced_pairs(&snapshots, &known);
        let mut books = Books::new();
        for snapshot in snapshots {
            let base = known
                .get(&snapshot.book.base_token)
                .ok_or_else(|| FeedError::Parsing("Missing ElFomoFi base token metadata".into()))?
                .clone();
            let quote = known
                .get(&snapshot.book.quote_token)
                .ok_or_else(|| FeedError::Parsing("Missing ElFomoFi quote token metadata".into()))?
                .clone();
            if !self.clears_tvl(&snapshot, &base, &quote, &priced_pairs) {
                continue;
            }
            let id = if base.address < quote.address {
                pair_component_id(PROTOCOL_SYSTEM, &base.address, &quote.address)
            } else {
                pair_component_id(PROTOCOL_SYSTEM, &quote.address, &base.address)
            };
            let updated_at = chrono::DateTime::from_timestamp(snapshot.block_timestamp as i64, 0);
            let state = ElfomofiState::from_snapshot(snapshot, base, quote)
                .map_err(|e| FeedError::Parsing(e.to_string()))?;
            books.insert(pair_book(
                PROTOCOL_SYSTEM,
                PROTOCOL_TYPE,
                self.chain(),
                id,
                state,
                updated_at,
            ));
        }
        self.discovered
            .lock()
            .await
            .extend(new_tokens);
        Ok(BookSnapshot::received_now(books))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    use alloy::{
        primitives::{Address, U256},
        sol_types::SolCall,
    };
    use serde_json::{json, Value};

    use super::{
        super::{
            feed::ElfomofiFeedBuilder,
            models::{getAllOrderbooksReturn, CumulativeLevel, PairOrderbook},
            tokens::{decimalsCall, symbolCall},
        },
        *,
    };
    use crate::snapshot_feed::http::test_support::spawn_http_server;

    fn addr(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }
    fn token(byte: u8) -> Token {
        Token::new(&addr(byte), "KNOWN", 0, 0, &[], Chain::Base, 100)
    }
    fn config() -> BookFeedConfig {
        BookFeedConfig {
            chain: Chain::Base,
            tokens: Arc::new([(addr(1), token(1)), (addr(2), token(2))].into()),
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
        pairs: Arc<std::sync::Mutex<Vec<(u8, u8)>>>,
        reorg: Arc<AtomicBool>,
    ) -> (String, Arc<AtomicUsize>) {
        let timestamp = unix_now().unwrap();
        let metadata_calls = Arc::new(AtomicUsize::new(0));
        let calls = metadata_calls.clone();
        let headers = Arc::new(AtomicUsize::new(0));
        let server = spawn_http_server(move |request| {
            let req: Value = serde_json::from_str(&request.body).unwrap();
            let result = match req["method"].as_str().unwrap() {
                "eth_chainId" => json!("0x2105"),
                "eth_getBlockByNumber" => {
                    let count = headers.fetch_add(1, Ordering::SeqCst);
                    let hash = if reorg.load(Ordering::SeqCst) && count % 2 == 1 { "02" } else { "01" };
                    json!({"number":"0x64", "timestamp":format!("0x{timestamp:x}"), "hash":format!("0x{}", hash.repeat(32))})
                }
                "eth_call" => {
                    assert_eq!(req["params"][1], "0x64", "all helper and metadata reads must share a block");
                    let data = req["params"][0]["data"].as_str().unwrap();
                    if data == format!("0x{}", hex::encode(getAllOrderbooksCall {}.abi_encode())) { helper(&pairs.lock().unwrap(), timestamp) }
                    else if data == format!("0x{}", hex::encode(decimalsCall {}.abi_encode())) {
                        calls.fetch_add(1, Ordering::SeqCst);
                        json!(format!("0x{}", hex::encode(decimalsCall::abi_encode_returns(&0))))
                    } else if data == format!("0x{}", hex::encode(symbolCall {}.abi_encode())) {
                        json!(format!("0x{}", hex::encode(symbolCall::abi_encode_returns(&"NEW".to_owned()))))
                    } else { panic!("unexpected RPC selector") }
                }
                method => panic!("unexpected RPC {method}"),
            };
            Some(("200 OK", json!({"jsonrpc":"2.0", "id":1,"result":result}).to_string()))
        }).await;
        (server.url(), metadata_calls)
    }

    #[tokio::test]
    async fn complete_snapshots_discover_add_remove_replace_and_readd_pairs() {
        let pairs = Arc::new(std::sync::Mutex::new(vec![(1, 2)]));
        let (url, metadata_calls) = mock(pairs.clone(), Arc::new(AtomicBool::new(false))).await;
        let source = ElfomofiBookSource::new(
            config(),
            ElfomofiClient::new(Chain::Base, url).unwrap(),
            30,
            true,
        );
        let first = source.fetch().await.unwrap();
        assert_eq!(first.books.len(), 1);
        let first_id = first
            .books
            .keys()
            .next()
            .unwrap()
            .clone();
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 0, "configured metadata wins");
        *pairs.lock().unwrap() = vec![(1, 2), (1, 3)];
        let second = source.fetch().await.unwrap();
        assert_eq!(second.books.len(), 2);
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
        let discovered = second
            .books
            .values()
            .find(|book| {
                book.component
                    .tokens
                    .iter()
                    .any(|token| token.address == addr(3))
            })
            .unwrap();
        assert_eq!(discovered.component.tokens[1].symbol, "NEW");
        *pairs.lock().unwrap() = vec![(2, 3)];
        let replaced = source.fetch().await.unwrap();
        assert_eq!(replaced.books.len(), 1);
        assert!(!replaced.books.contains_key(&first_id));
        *pairs.lock().unwrap() = vec![];
        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
        *pairs.lock().unwrap() = vec![(2, 1)];
        let restored = source.fetch().await.unwrap();
        assert!(
            restored.books.contains_key(&first_id),
            "pair orientation does not change identity"
        );
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 1, "new token metadata is cached");
    }

    #[tokio::test]
    async fn strict_universe_filters_unknown_tokens_without_reading_metadata() {
        let pairs = Arc::new(std::sync::Mutex::new(vec![(1, 2), (1, 3)]));
        let (url, calls) = mock(pairs, Arc::new(AtomicBool::new(false))).await;
        let source = ElfomofiBookSource::new(
            config(),
            ElfomofiClient::new(Chain::Base, url).unwrap(),
            30,
            false,
        );
        assert_eq!(
            source
                .fetch()
                .await
                .unwrap()
                .books
                .len(),
            1
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn canonical_recheck_rejects_reorg_and_does_not_cache_unpublished_metadata() {
        let pairs = Arc::new(std::sync::Mutex::new(vec![(1, 3)]));
        let reorg = Arc::new(AtomicBool::new(true));
        let (url, calls) = mock(pairs, reorg.clone()).await;
        let source = ElfomofiBookSource::new(
            config(),
            ElfomofiClient::new(Chain::Base, url).unwrap(),
            30,
            true,
        );
        assert!(source.fetch().await.is_err());
        assert!(source
            .discovered
            .lock()
            .await
            .is_empty());
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
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn builder_requires_explicit_matching_endpoints_and_redacts_rpc_credentials() {
        let url = "https://example.test/private-key".to_owned();
        assert!(ElfomofiFeedBuilder::new(config(), url.clone())
            .build()
            .is_err());
        let mut universe = config();
        Arc::make_mut(&mut universe.tokens).insert(
            Bytes::zero(20),
            Token::new(&Bytes::zero(20), "NATIVE", 18, 0, &[], Chain::Base, 100),
        );
        let feed = ElfomofiFeedBuilder::new(universe, url.clone())
            .polling()
            .build()
            .unwrap();
        assert!(!format!("{feed:?}").contains("private-key"));
        assert!(ElfomofiFeedBuilder::new(config(), url.clone())
            .ws_rpc_url("http://invalid".into())
            .build()
            .is_err());
        let feed = ElfomofiFeedBuilder::new(config(), url)
            .ws_rpc_url("wss://example.test/websocket-key".into())
            .build()
            .unwrap();
        assert!(!format!("{feed:?}").contains("websocket-key"));
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
        let mut snapshot = decode_orderbooks(
            Chain::Base,
            &bytes,
            100,
            Bytes::from(vec![1; 32]),
            timestamp,
            timestamp,
            30,
        )
        .unwrap()
        .remove(0);
        snapshot.book.quote_token = usd.clone();
        let base = token(1);
        let quote = Token::new(&usd, "USDC", 0, 0, &[], Chain::Base, 100);
        let known = [(base.address.clone(), base.clone()), (usd, quote.clone())].into();
        let prices = ElfomofiBookSource::priced_pairs(std::slice::from_ref(&snapshot), &known);
        let mut config = config();
        config.min_tvl_usd = 3000.0;
        let mut source = ElfomofiBookSource::new(
            config,
            ElfomofiClient::new(Chain::Base, "https://example.test".into()).unwrap(),
            30,
            false,
        );
        assert!(source.clears_tvl(&snapshot, &base, &quote, &prices));
        source.book_config.min_tvl_usd = 3001.0;
        assert!(!source.clears_tvl(&snapshot, &base, &quote, &prices));
        assert!(
            !source.clears_tvl(&snapshot, &base, &quote, &[]),
            "unknown USD conversion must not clear a positive floor"
        );
    }
    #[test]
    fn an_asks_only_robinhood_book_clears_a_positive_usd_floor() {
        use crate::book::component::PairState;
        let usd: Bytes = "0x5fc5360d0400a0fd4f2af552add042d716f1d168"
            .parse()
            .unwrap();
        let timestamp = unix_now().unwrap();
        let snapshot = Snapshot {
            chain: Chain::Robinhood,
            block_number: 100,
            block_hash: Bytes::from(vec![1; 32]),
            block_timestamp: timestamp,
            observed_at: timestamp,
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
        let source = ElfomofiBookSource::new(
            BookFeedConfig {
                chain: Chain::Robinhood,
                tokens: Arc::new(tokens),
                min_tvl_usd: 3000.0,
            },
            ElfomofiClient::new(Chain::Robinhood, "https://example.test".into()).unwrap(),
            30,
            false,
        );
        assert!(source.clears_tvl(&snapshot, &base, &quote, &prices));
        let state = ElfomofiState::from_snapshot(snapshot, base, quote).unwrap();
        assert_eq!(
            state.directions(),
            vec![(usd, addr(1))],
            "TVL normalization must not invent a quotable side"
        );
    }
}
