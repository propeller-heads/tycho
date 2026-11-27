//! One poll: the helper's books at the latest block, checked for freshness and turned into
//! simulate-ready books.

use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use num_traits::ToPrimitive;
use tracing::debug;
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};

use crate::{
    book::{
        component::{pair_book, pair_component_id, PairState},
        levels::{Levels, PriceLevel},
        tvl::tvl_in_usd,
        BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    pamm::protocols::elfomofi::{
        client::ElfomofiClient,
        models::{decode_orderbooks, ElfomofiSupportedChain, HelperRead, PairBook},
        state::{BookSides, ElfomofiState},
        PROTOCOL_SYSTEM, PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
    utils::decimals_scale,
};

/// Polls one chain's helper and publishes every servable pair as a complete [`BookSnapshot`].
#[derive(derive_more::Debug)]
pub(super) struct ElfomofiBookSource {
    pub(super) client: ElfomofiClient,
    pub(super) chain: ElfomofiSupportedChain,
    pub(super) book_config: BookFeedConfig,
    #[debug(skip)]
    pub(super) usd_quote_tokens: HashSet<Bytes>,
    pub(super) max_read_lag: Duration,
    /// The highest block a poll has read, so a node behind a load balancer that answers from an
    /// older block cannot replace newer books with older ones.
    #[debug(skip)]
    pub(super) highest_block: AtomicU64,
}

impl HttpSource for ElfomofiBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// Fails, as a retryable poll, when the read's block is more than `max_read_lag` behind the
    /// local clock or older than a block an earlier poll read. A block ahead of the local clock
    /// passes: a slow local clock does not make the data old.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        self.client.verify_chain().await?;
        let raw = self.client.orderbooks().await?;
        let read = decode_orderbooks(&raw)?;
        self.check_lag(&read, SystemTime::now())?;
        self.check_block_order(read.block_number)?;
        Ok(BookSnapshot::received_now(self.books(&read)))
    }
}

impl ElfomofiBookSource {
    fn check_lag(&self, read: &HelperRead, now: SystemTime) -> Result<(), FeedError> {
        let block_time = UNIX_EPOCH + Duration::from_secs(read.block_timestamp);
        let Ok(lag) = now.duration_since(block_time) else {
            return Ok(());
        };
        if lag > self.max_read_lag {
            return Err(FeedError::Connection(format!(
                "ElFomoFi read block {} is {:.1}s behind the local clock, more than the {:?} allowed",
                read.block_number,
                lag.as_secs_f64(),
                self.max_read_lag
            )));
        }
        Ok(())
    }

    fn check_block_order(&self, block_number: u64) -> Result<(), FeedError> {
        let highest = self
            .highest_block
            .fetch_max(block_number, Ordering::SeqCst);
        if block_number < highest {
            return Err(FeedError::Connection(format!(
                "ElFomoFi read block {block_number}, older than block {highest} already read"
            )));
        }
        Ok(())
    }

    /// The books of the pairs this feed serves: both tokens in the configured universe, at least
    /// one direction tradable, and a TVL that clears the floor. Each pair is valued at the vault's
    /// whole balance of both tokens, as its ladders are.
    fn books(&self, read: &HelperRead) -> Books {
        let usd_prices: Vec<_> = read
            .books
            .iter()
            .filter_map(|book| {
                Some((&book.base, &book.quote, bid_levels(book, &self.book_config)?))
            })
            .collect();
        let mut books = Books::new();
        for pair in &read.books {
            let Some((base, quote)) = self.pair_tokens(pair) else {
                continue;
            };
            let state = ElfomofiState::new(
                self.chain,
                (read.block_number, read.block_timestamp),
                (base.clone(), quote.clone()),
                BookSides {
                    bids: pair.bids.clone(),
                    asks: pair.asks.clone(),
                    base_balance: pair.balance_base.clone(),
                    quote_balance: pair.balance_quote.clone(),
                },
            );
            if state.directions().is_empty() {
                debug!(base = %pair.base, quote = %pair.quote, "skipping ElFomoFi book with no tradable side");
                continue;
            }
            let id = canonical_pair_id(&pair.base, &pair.quote);
            let Some(tvl) = book_tvl(&state, pair, &self.usd_quote_tokens, &usd_prices) else {
                debug!(book = %id, "skipping ElFomoFi book whose value in USD is unknown");
                continue;
            };
            if !self
                .book_config
                .clears_min_tvl(tvl, &id)
            {
                continue;
            }
            books.insert(pair_book(
                PROTOCOL_SYSTEM,
                PROTOCOL_TYPE,
                self.chain.chain(),
                id,
                state,
                None,
            ));
        }
        books
    }

    /// Both tokens of `pair` from the configured universe, on the feed's chain.
    fn pair_tokens(&self, pair: &PairBook) -> Option<(&Token, &Token)> {
        let tokens = self
            .book_config
            .pair_tokens(&pair.base, &pair.quote);
        let chain = self.chain.chain();
        let served = tokens.filter(|(base, quote)| base.chain == chain && quote.chain == chain);
        if served.is_none() {
            debug!(base = %pair.base, quote = %pair.quote, "skipping ElFomoFi pair outside the token list");
        }
        served
    }
}

/// The component id of a pair, with the two addresses in byte order so it does not change when
/// the helper flips the pair round.
pub(super) fn canonical_pair_id(a: &Bytes, b: &Bytes) -> Bytes {
    if a <= b {
        pair_component_id(PROTOCOL_SYSTEM, a, b)
    } else {
        pair_component_id(PROTOCOL_SYSTEM, b, a)
    }
}

/// The first bid segment of `book` as a one-level ladder in human units, which is all
/// [`tvl_in_usd`] needs to price the base token in the quote token; `None` without bids or
/// without the tokens' decimals.
fn bid_levels(book: &PairBook, config: &BookFeedConfig) -> Option<Levels> {
    let (base, quote) = config.pair_tokens(&book.base, &book.quote)?;
    let (input, output) = book.bids.first_rung()?;
    let quantity = input.to_f64()? / decimals_scale(base.decimals);
    let output = output.to_f64()? / decimals_scale(quote.decimals);
    Levels::new(vec![PriceLevel { quantity, price: output / quantity }]).ok()
}

/// The pair's value in USD: both of the vault's balances, valued in the quote token at the book's
/// own price, or in the base token where only that one converts to USD (a pair the helper names
/// with the stablecoin first).
fn book_tvl(
    state: &ElfomofiState,
    pair: &PairBook,
    usd_quote_tokens: &HashSet<Bytes>,
    priced_pairs: &[(&Bytes, &Bytes, Levels)],
) -> Option<f64> {
    let (base, quote) = (state.base_token(), state.quote_token());
    let base_price = state
        .spot_price(base, quote)
        .or_else(|_| {
            state
                .spot_price(quote, base)
                .map(|price| 1.0 / price)
        })
        .ok()?;
    let human = |amount: &num_bigint::BigUint, decimals| {
        amount.to_f64().unwrap_or(f64::INFINITY) / decimals_scale(decimals)
    };
    let (base_amount, quote_amount) =
        (human(&pair.balance_base, base.decimals), human(&pair.balance_quote, quote.decimals));
    let pairs = || {
        priced_pairs
            .iter()
            .map(|(base, quote, levels)| (*base, *quote, levels))
    };
    tvl_in_usd(quote_amount + base_amount * base_price, &pair.quote, usd_quote_tokens, pairs())
        .or_else(|| {
            tvl_in_usd(
                base_amount + quote_amount / base_price,
                &pair.base,
                usd_quote_tokens,
                pairs(),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::pamm::protocols::elfomofi::{
        feed::ElfomofiFeedBuilder,
        test_support::{
            base_book_config, big, bytes, helper_answer, now_secs, usdc, weth, weth_usdc_wire,
            wire_book, FakeNode, ASKS, BIDS, CBBTC, USDC, WETH,
        },
    };

    fn source(url: String, min_tvl_usd: f64) -> ElfomofiBookSource {
        ElfomofiFeedBuilder::new(base_book_config(min_tvl_usd), url)
            .build()
            .unwrap()
            .source
    }

    async fn node(
        answers: Vec<Vec<u8>>,
    ) -> (ElfomofiBookSource, crate::pamm::protocols::elfomofi::test_support::FakeNodeHandle) {
        let node = FakeNode::serving(answers).spawn().await;
        (source(node.server.url(), 0.0), node)
    }

    fn fresh(
        books: Vec<crate::pamm::protocols::elfomofi::models::PairOrderbook>,
        block: u64,
    ) -> Vec<u8> {
        helper_answer(books, block, now_secs())
    }

    fn ids(snapshot: &BookSnapshot<ReceivedAt>) -> Vec<String> {
        let mut ids: Vec<_> = snapshot.books.keys().cloned().collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn a_fresh_read_publishes_its_pairs() {
        let (source, _node) = node(vec![fresh(vec![weth_usdc_wire()], 10)]).await;

        let snapshot = source.fetch().await.unwrap();

        let id = canonical_pair_id(&bytes(WETH), &bytes(USDC));
        assert_eq!(ids(&snapshot), [id.to_string()]);
        let book = &snapshot.books[&id.to_string()];
        assert_eq!(book.component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(book.component.tokens.len(), 2);
    }

    /// The published state sells WETH down the bids and USDC down the asks, each capped by the
    /// vault's balance of the token it pays out: 5,985 USDC stops the bids inside their second
    /// segment (see the state's limit test), and 1.5 WETH stops the asks inside theirs, where
    /// 6,000 USDC buy 1.96 WETH.
    #[tokio::test]
    async fn a_published_book_quotes_its_own_sides_and_balances() {
        let book = wire_book(WETH, USDC, &ASKS, &BIDS, (1_500_000_000_000_000_000, 5_985_000_000));
        let (source, _node) = node(vec![fresh(vec![book], 10)]).await;

        let snapshot = source.fetch().await.unwrap();

        let state =
            &snapshot.books[&canonical_pair_id(&bytes(WETH), &bytes(USDC)).to_string()].state;
        let one_weth = big(1_000_000_000_000_000_000);
        let sold_weth = state
            .get_amount_out(one_weth, &weth(), &usdc())
            .unwrap();
        let bought_weth = state
            .get_amount_out(big(3_000_000_000), &usdc(), &weth())
            .unwrap();
        assert_eq!(sold_weth.amount, big(3_000_000_000));
        assert_eq!(bought_weth.amount, big(990_000_000_000_000_000));
        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (big(2_000_000_000_000_000_000 + 335_008_375), big(5_985_000_000))
        );
        let (_, weth_out) = state
            .get_limits(usdc().address, weth().address)
            .unwrap();
        assert!(weth_out <= big(1_500_000_000_000_000_000), "{weth_out}");
        assert!(weth_out > big(990_000_000_000_000_000), "{weth_out}");
    }

    #[tokio::test]
    async fn a_read_behind_the_clock_fails_the_poll() {
        let stale = helper_answer(vec![weth_usdc_wire()], 10, now_secs() - 10);
        let (source, _node) = node(vec![stale]).await;

        let error = source.fetch().await.unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        assert!(error.to_string().contains("block 10"), "{error}");
    }

    #[tokio::test]
    async fn a_read_ahead_of_the_clock_passes() {
        let ahead = helper_answer(vec![weth_usdc_wire()], 10, now_secs() + 30);
        let (source, _node) = node(vec![ahead]).await;

        assert!(source.fetch().await.is_ok());
    }

    #[tokio::test]
    async fn a_read_of_an_older_block_fails_and_the_same_block_passes() {
        let answers = vec![
            fresh(vec![weth_usdc_wire()], 10),
            fresh(vec![weth_usdc_wire()], 10),
            fresh(vec![weth_usdc_wire()], 9),
        ];
        let (source, _node) = node(answers).await;

        source.fetch().await.unwrap();
        source.fetch().await.unwrap();
        let error = source.fetch().await.unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
    }

    #[tokio::test]
    async fn a_node_on_another_chain_ends_the_feed_and_a_right_one_is_asked_once() {
        let mut wrong = FakeNode::serving(vec![fresh(vec![], 1)]);
        wrong.chain_id = 56;
        let wrong = wrong.spawn().await;
        let error = source(wrong.server.url(), 0.0)
            .fetch()
            .await
            .unwrap_err();
        assert!(error.is_fatal(), "{error}");

        let (right, node) = node(vec![fresh(vec![], 1)]).await;
        right.fetch().await.unwrap();
        right.fetch().await.unwrap();
        assert_eq!(
            node.chain_id_calls
                .load(Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn pairs_outside_the_token_list_are_not_served() {
        let outside = wire_book(
            alloy::primitives::address!("1111111111111111111111111111111111111111"),
            USDC,
            &ASKS,
            &BIDS,
            (1, 1),
        );
        let (source, _node) = node(vec![fresh(vec![outside, weth_usdc_wire()], 1)]).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(ids(&snapshot), [canonical_pair_id(&bytes(WETH), &bytes(USDC)).to_string()]);
    }

    /// The WETH/USDC book holds 100 WETH at about 3,000 USDC and 1,000,000 USDC: about 1.3M USD.
    /// A cbBTC/USDC book with 1 USDC and no cbBTC is worth 1 USD.
    #[tokio::test]
    async fn books_under_the_tvl_floor_are_not_served() {
        let small = wire_book(CBBTC, USDC, &[], &[(100_000_000, 1_000_000)], (0, 1_000_000));
        let node = FakeNode::serving(vec![fresh(vec![small, weth_usdc_wire()], 1)])
            .spawn()
            .await;

        let snapshot = source(node.server.url(), 1_000_000.0)
            .fetch()
            .await
            .unwrap();

        assert_eq!(ids(&snapshot), [canonical_pair_id(&bytes(WETH), &bytes(USDC)).to_string()]);
    }

    #[tokio::test]
    async fn a_book_with_no_tradable_side_is_not_served() {
        let empty = wire_book(WETH, USDC, &[], &[], (1, 1));
        let (source, _node) = node(vec![fresh(vec![empty], 1)]).await;

        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
    }

    /// The helper may name a pair either way round; the component stays the same.
    #[tokio::test]
    async fn a_flipped_pair_keeps_its_component() {
        let flipped =
            wire_book(USDC, WETH, &BIDS, &ASKS, (1_000_000_000_000, 100_000_000_000_000_000_000));
        let answers = vec![fresh(vec![weth_usdc_wire()], 1), fresh(vec![flipped], 2)];
        let (source, _node) = node(answers).await;

        let first = source.fetch().await.unwrap();
        let second = source.fetch().await.unwrap();

        assert_eq!(ids(&first), ids(&second));
    }

    #[tokio::test]
    async fn errors_leave_the_rpc_url_out() {
        let url = "http://127.0.0.1:1/secret-api-key".to_string();

        let error = source(url, 0.0)
            .fetch()
            .await
            .unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        assert!(
            !error
                .to_string()
                .contains("secret-api-key"),
            "{error}"
        );
        assert!(!format!("{:?}", source("http://127.0.0.1:1/secret-api-key".to_string(), 0.0))
            .contains("secret-api-key"));
    }

    /// Reads each chain's live helper once and decodes its answer. Needs `BASE_RPC_URL`,
    /// `BSC_RPC_URL` and `ROBINHOOD_RPC_URL`.
    #[tokio::test]
    #[ignore = "reads the live helpers over the network"]
    async fn live_helpers_decode() {
        for (chain, variable) in [
            (ElfomofiSupportedChain::Base, "BASE_RPC_URL"),
            (ElfomofiSupportedChain::Bsc, "BSC_RPC_URL"),
            (ElfomofiSupportedChain::Robinhood, "ROBINHOOD_RPC_URL"),
        ] {
            let url = std::env::var(variable).unwrap_or_else(|_| panic!("{variable} is not set"));
            let client = ElfomofiClient::new(&url, chain).unwrap();
            client.verify_chain().await.unwrap();

            let read = decode_orderbooks(&client.orderbooks().await.unwrap()).unwrap();

            assert!(!read.books.is_empty(), "{chain:?} serves no books");
            let lag = now_secs().saturating_sub(read.block_timestamp);
            assert!(lag < 30, "{chain:?} read is {lag}s old");
        }
    }
}
