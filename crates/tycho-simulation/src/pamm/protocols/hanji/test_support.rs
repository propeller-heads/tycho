//! Fixtures shared by the Hanji tests: a recording of the Base markets, the states and reads it
//! describes, and a JSON-RPC node and aggregator API that serve it.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::Duration,
};

use alloy::{
    primitives::{address, aliases::U72, Address, U256},
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getChainIdCall, getCurrentBlockTimestampCall,
            Result as Call3Result,
        },
        MULTICALL3_ADDRESS,
    },
    sol_types::{SolCall, SolValue},
};
use reqwest::Url;
use serde::Deserialize;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::{
    book::BookFeedConfig,
    pamm::protocols::hanji::{
        feed::{HanjiBookAssembler, HanjiBookSource, MarketList},
        market::{ApiMarket, Market},
        reader::{
            assembleOrderbookFromOrdersCall, assembleOrderbookFromOrdersReturn, getConfigCall,
            getConfigReturn, pausedCall, whitelistedCall, BookRead, HanjiReader, MarketConfig,
            MarketRead, CONTRACT_QUOTER_SELECTOR, LOB_SELECTOR, QUOTER_SELECTOR,
        },
        state::{HanjiState, Level},
        BASE_DEPLOYMENT, BASE_ROUTER,
    },
    snapshot_feed::http::test_support::{
        spawn_http_server, MockHttpServer, MockRequest, MockResponse,
    },
};

/// Recorded by `testdata/record_fixture.py` at Base block 61,693,481: the API's eight markets,
/// both quoters' books of each, and twelve fills per kind of caller on each of the three whose
/// tokens can be funded (ETH/USDC, cbBTC/USDC and cbBTC/ETH), each fill's output measured as the
/// caller's balance gain. Tycho's router was not whitelisted on any market.
const RECORDING: &str = include_str!("testdata/live_61693481.json");

pub(super) const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
pub(super) const CBBTC: Address = address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf");
pub(super) const ETH_USDC: Address = address!("6a6951db3bb99f76a8ccb9f8a8399b613666298b");
pub(super) const CBBTC_USDC: Address = address!("15c8cb4e336b1e478d155d813830129fb225eee3");
pub(super) const CBBTC_ETH: Address = address!("f455b3c0756908c6a2f44a7f7390053cfe354eea");
/// The NVDAc/USDC market, one of the tokenized equities, which have no contract quoter.
pub(super) const NVDA_USDC: Address = address!("30bfa3b501c8f83114c1a789febb6281a0bcd751");

/// Which quoter's book a recorded fill or state is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Caller {
    /// The transaction's sender, or a whitelisted caller: the FastQuoter's book.
    Direct,
    /// Any other contract: the contract quoter's book.
    Contract,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Fixture {
    pub block: u64,
    pub timestamp: u64,
    pub markets: Vec<FixtureMarket>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureMarket {
    /// The market's entry in the API response, as sent.
    pub api: serde_json::Value,
    pub config: FixtureConfig,
    pub wiring: FixtureWiring,
    pub books: FixtureBooks,
    pub cases: Vec<FixtureCase>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureConfig {
    pub scaling_x: U256,
    pub scaling_y: U256,
    pub token_x: Address,
    pub token_y: Address,
    pub supports_native_eth: bool,
    pub is_token_x_weth: bool,
    pub aggressive_commission_rate: U256,
    pub passive_order_payout_rate: U256,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureWiring {
    pub lob: Address,
    pub quoter: Address,
    /// Zero where the market has none.
    pub contract_quoter: Address,
    /// Whether the market had Tycho's router whitelisted.
    pub router_whitelisted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureBooks {
    pub direct: FixtureBook,
    /// `None` where the market has no contract quoter.
    pub contract: Option<FixtureBook>,
}

/// `[price, shares]` per level, best first.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureBook {
    pub asks: Vec<[U256; 2]>,
    pub bids: Vec<[U256; 2]>,
}

/// One fill the proxy returned to `caller`: selling base (`placeOrder`) or buying it
/// (`placeMarketOrderWithTargetValue`) with `amount_in`, of which `amount_out` reached the caller.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureCase {
    pub caller: Caller,
    pub sell_base: bool,
    pub amount_in: U256,
    pub amount_out: U256,
    /// The proxy call's own gas: for a direct caller, the transaction's less its intrinsic cost.
    pub gas: u64,
}

pub(super) fn fixture() -> Fixture {
    serde_json::from_str(RECORDING).expect("the recording parses")
}

pub(super) fn levels(side: &[[U256; 2]]) -> Vec<Level> {
    side.iter()
        .map(|[price, shares]| Level { price: *price, shares: *shares })
        .collect()
}

impl FixtureBooks {
    pub(super) fn of(&self, caller: Caller) -> Option<&FixtureBook> {
        match caller {
            Caller::Direct => Some(&self.direct),
            Caller::Contract => self.contract.as_ref(),
        }
    }
}

impl FixtureConfig {
    fn fee_rate(&self) -> u64 {
        (self.aggressive_commission_rate + self.passive_order_payout_rate).to()
    }
}

impl FixtureMarket {
    pub(super) fn market(&self) -> Market {
        let entry: ApiMarket = serde_json::from_value(self.api.clone()).expect("an API entry");
        Market::from_api(
            &entry,
            BASE_DEPLOYMENT
                .wrapped_native()
                .unwrap(),
        )
        .expect("a tradable market")
    }

    /// The market as a read for `caller` reports it, or `None` where it has no book for it.
    pub(super) fn read(&self, caller: Caller) -> Option<MarketRead> {
        let book = self.books.of(caller)?;
        Some(MarketRead {
            market: self.market(),
            config: MarketConfig {
                scaling_x: self.config.scaling_x,
                scaling_y: self.config.scaling_y,
                fee_rate: self.config.fee_rate(),
            },
            asks: levels(&book.asks),
            bids: levels(&book.bids),
        })
    }
}

impl Fixture {
    pub(super) fn market(&self, orderbook: Address) -> &FixtureMarket {
        self.markets
            .iter()
            .find(|market| market.market().orderbook == orderbook)
            .expect("a recorded market")
    }

    /// The read a feed for `caller` makes of the recording.
    pub(super) fn read(&self, caller: Caller) -> BookRead {
        BookRead {
            block_number: self.block,
            timestamp: self.timestamp,
            markets: self
                .markets
                .iter()
                .filter_map(|market| market.read(caller))
                .collect(),
        }
    }
}

fn bytes(address: Address) -> Bytes {
    Bytes::from(address.to_vec())
}

pub(super) fn eth() -> Token {
    Chain::Base.native_token()
}

pub(super) fn usdc() -> Token {
    Token::new(&bytes(USDC), "USDC", 6, 0, &[Some(10_000)], Chain::Base, 100)
}

pub(super) fn cbbtc() -> Token {
    Token::new(&bytes(CBBTC), "cbBTC", 8, 0, &[Some(10_000)], Chain::Base, 100)
}

/// ETH, USDC and cbBTC on Base, with no TVL floor: the tokenized equities are left out, as a
/// Tycho token universe leaves them out.
pub(super) fn book_config() -> BookFeedConfig {
    let tokens: HashMap<Bytes, Token> = [eth(), usdc(), cbbtc()]
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect();
    BookFeedConfig { chain: Chain::Base, tokens: Arc::new(tokens), min_tvl_usd: 0.0 }
}

/// The recorded market behind `orderbook`, as a state of the book `caller` gets.
pub(super) fn state(orderbook: Address, caller: Caller) -> HanjiState {
    let fixture = fixture();
    let read = fixture
        .market(orderbook)
        .read(caller)
        .expect("a recorded book");
    let (base, quote) = match orderbook {
        ETH_USDC => (eth(), usdc()),
        CBBTC_USDC => (cbbtc(), usdc()),
        CBBTC_ETH => (cbbtc(), eth()),
        _ => panic!("no tokens for {orderbook}"),
    };
    HanjiState {
        orderbook,
        base_token: base,
        quote_token: quote,
        scaling_x: read.config.scaling_x,
        scaling_y: read.config.scaling_y,
        fee_rate: read.config.fee_rate,
        asks: read.asks,
        bids: read.bids,
    }
    .validated()
    .expect("a recorded state validates")
}

/// A JSON-RPC node and aggregator API serving the recording: the API lists its eight markets,
/// and the node answers the reader's multicalls from their configs, wiring and books.
pub(super) struct FakeNode {
    pub fixture: Fixture,
    pub chain_id: u64,
    /// The block a read is answered from; the recording's unless a test moves it.
    pub block: AtomicU64,
    /// Proxies whose getConfig reverts.
    pub reverting: HashSet<Address>,
    /// Proxies whose getConfig names their tokens the other way round.
    pub misreporting: HashSet<Address>,
    /// Proxies whose CLOB getter reverts, so their wiring does not resolve.
    pub unwired: HashSet<Address>,
    /// Proxies whose getConfig puts native ETH on the other side of the market.
    pub native_flipped: HashSet<Address>,
    /// Rates every getConfig reports in place of the recorded ones.
    pub rates: Option<Rates>,
    /// Proxies that are paused.
    pub paused: HashSet<Address>,
    /// Proxies that have whitelisted Tycho's router: those recorded so, unless a test adds one.
    pub whitelisting: Mutex<HashSet<Address>>,
    /// Contract quoters a test has swapped in, by proxy.
    pub rewired: Mutex<HashMap<Address, Address>>,
    /// Whether the API answers with a server error.
    pub api_down: AtomicBool,
    /// How long the API takes to answer.
    pub api_delay: Mutex<Duration>,
    /// How long the node takes to answer a call.
    pub node_delay: Mutex<Duration>,
    /// How many requests reached the API.
    pub api_requests: AtomicUsize,
    /// The implementation in each proxy's EIP-1967 slot: the deployment's pins unless a test
    /// upgrades one.
    pub implementations: Mutex<HashMap<Address, Address>>,
    /// Whether implementation slot reads fail at the transport.
    pub storage_failing: AtomicBool,
}

impl FakeNode {
    pub(super) fn new() -> Self {
        let fixture = fixture();
        let whitelisting = fixture
            .markets
            .iter()
            .filter(|market| market.wiring.router_whitelisted)
            .map(|market| market.market().orderbook)
            .collect();
        FakeNode {
            block: AtomicU64::new(fixture.block),
            fixture,
            chain_id: 8453,
            reverting: HashSet::new(),
            misreporting: HashSet::new(),
            native_flipped: HashSet::new(),
            unwired: HashSet::new(),
            rates: None,
            paused: HashSet::new(),
            whitelisting: Mutex::new(whitelisting),
            rewired: Mutex::new(HashMap::new()),
            api_down: AtomicBool::new(false),
            api_delay: Mutex::new(Duration::ZERO),
            node_delay: Mutex::new(Duration::ZERO),
            api_requests: AtomicUsize::new(0),
            implementations: Mutex::new(
                BASE_DEPLOYMENT
                    .implementations
                    .iter()
                    .copied()
                    .collect(),
            ),
            storage_failing: AtomicBool::new(false),
        }
    }

    /// Has every proxy whitelist Tycho's router.
    pub(super) fn whitelist_the_router_everywhere(&self) {
        let mut whitelisting = self
            .whitelisting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for market in &self.fixture.markets {
            whitelisting.insert(market.market().orderbook);
        }
    }

    /// Serves the node and the API from one server: the API under its path, the node at `/`.
    pub(super) async fn serve(self: Arc<Self>) -> MockHttpServer {
        spawn_http_server(move |request: &MockRequest| Some(self.answer(request))).await
    }

    fn answer(&self, request: &MockRequest) -> MockResponse {
        if request
            .target
            .ends_with("/aggregators/markets")
        {
            return self.answer_api();
        }
        let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        if request["method"] == "eth_getStorageAt" {
            return self.answer_storage(&request);
        }
        assert_eq!(request["method"], "eth_call", "the reader sends eth_call and eth_getStorageAt");
        assert_eq!(request["params"][0]["to"], serde_json::json!(MULTICALL3_ADDRESS));
        let delay = *self
            .node_delay
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        MockResponse { status: "200 OK", body: self.answer_call(&request).to_string(), delay }
    }

    fn answer_api(&self) -> MockResponse {
        self.api_requests
            .fetch_add(1, Ordering::SeqCst);
        let delay = *self
            .api_delay
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.api_down.load(Ordering::SeqCst) {
            return MockResponse { status: "503 Service Unavailable", body: "down".into(), delay };
        }
        let markets: Vec<_> = self
            .fixture
            .markets
            .iter()
            .map(|market| market.api.clone())
            .collect();
        MockResponse { status: "200 OK", body: serde_json::to_string(&markets).unwrap(), delay }
    }

    fn answer_call(&self, request: &serde_json::Value) -> serde_json::Value {
        assert_eq!(request["params"][1], "latest", "a read asks for the latest block");
        let input = request["params"][0]["input"]
            .as_str()
            .or_else(|| request["params"][0]["data"].as_str())
            .unwrap();
        let input = alloy::primitives::hex::decode(input).unwrap();
        let calls = aggregate3Call::abi_decode(&input)
            .unwrap()
            .calls;
        let answers: Vec<Call3Result> = calls
            .iter()
            .map(|call| {
                let answer = self.call(call.target, &call.callData);
                Call3Result {
                    success: answer.is_some(),
                    returnData: answer.unwrap_or_default().into(),
                }
            })
            .collect();
        let encoded = aggregate3Call::abi_encode_returns(&answers);
        let result = alloy::primitives::hex::encode_prefixed(encoded);
        serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result })
    }

    /// Answers an implementation slot read, which must be at the read's block.
    fn answer_storage(&self, request: &serde_json::Value) -> MockResponse {
        if self
            .storage_failing
            .load(Ordering::SeqCst)
        {
            return MockResponse::from(("502 Bad Gateway", "overloaded".to_string()));
        }
        let params = &request["params"];
        let slot: U256 = serde_json::from_value(params[1].clone()).unwrap();
        assert_eq!(
            slot,
            U256::from_be_bytes(
                alloy::primitives::b256!(
                    "360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
                )
                .0
            ),
            "the reader reads only the EIP-1967 implementation slot"
        );
        assert_eq!(
            params[2],
            serde_json::json!(format!("{:#x}", self.block.load(Ordering::SeqCst))),
            "an implementation is read at the read's block"
        );
        let proxy: Address = serde_json::from_value(params[0].clone()).unwrap();
        let implementation = self
            .implementations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&proxy)
            .copied()
            .unwrap_or_default();
        let result = implementation.into_word();
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result });
        MockResponse::from(("200 OK", body.to_string()))
    }

    fn market(&self, orderbook: Address) -> Option<&FixtureMarket> {
        self.fixture
            .markets
            .iter()
            .find(|market| market.market().orderbook == orderbook)
    }

    fn contract_quoter(&self, orderbook: Address, market: &FixtureMarket) -> Address {
        self.rewired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&orderbook)
            .copied()
            .unwrap_or(market.wiring.contract_quoter)
    }

    /// The answer to one call, or `None` where the chain reverts.
    fn call(&self, target: Address, data: &[u8]) -> Option<Vec<u8>> {
        let selector = <[u8; 4]>::try_from(&data[..4]).unwrap();
        if target == MULTICALL3_ADDRESS {
            let value = match selector {
                getBlockNumberCall::SELECTOR => self.block.load(Ordering::SeqCst),
                getCurrentBlockTimestampCall::SELECTOR => self.fixture.timestamp,
                getChainIdCall::SELECTOR => self.chain_id,
                _ => panic!("the fake node does not answer {selector:?} on Multicall3"),
            };
            return Some(U256::from(value).abi_encode());
        }
        if self.chain_id != Chain::Base.id() {
            // On another chain the contracts have no code: a call succeeds and returns nothing.
            return Some(Vec::new());
        }
        if selector == assembleOrderbookFromOrdersCall::SELECTOR {
            return self.book(target, data);
        }
        let market = self.market(target)?;
        match selector {
            LOB_SELECTOR if self.unwired.contains(&target) => None,
            LOB_SELECTOR => Some(market.wiring.lob.abi_encode()),
            QUOTER_SELECTOR => Some(market.wiring.quoter.abi_encode()),
            CONTRACT_QUOTER_SELECTOR => Some(
                self.contract_quoter(target, market)
                    .abi_encode(),
            ),
            whitelistedCall::SELECTOR => {
                let caller = whitelistedCall::abi_decode(data)
                    .unwrap()
                    .caller;
                assert_eq!(caller, BASE_ROUTER, "the books are read for the configured taker");
                let whitelisting = self
                    .whitelisting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                Some(
                    whitelisting
                        .contains(&target)
                        .abi_encode(),
                )
            }
            pausedCall::SELECTOR => Some(
                self.paused
                    .contains(&target)
                    .abi_encode(),
            ),
            getConfigCall::SELECTOR if self.reverting.contains(&target) => None,
            getConfigCall::SELECTOR => Some(config_answer(
                market,
                self.misreporting.contains(&target),
                self.native_flipped.contains(&target),
                self.rates,
            )),
            _ => panic!("the fake node does not answer {selector:?} on {target}"),
        }
    }

    /// A quoter's book of the CLOB `data` names: the quoter's or the contract quoter's, as
    /// recorded for the market of that CLOB. Ignores the depth asked for: the recording holds at
    /// most the levels the feed asks for.
    fn book(&self, quoter: Address, data: &[u8]) -> Option<Vec<u8>> {
        let call = assembleOrderbookFromOrdersCall::abi_decode(data).unwrap();
        let market = self
            .fixture
            .markets
            .iter()
            .find(|market| market.wiring.lob == call.lobAddress)?;
        let book = if quoter == market.wiring.quoter {
            &market.books.direct
        } else if quoter == market.wiring.contract_quoter {
            market.books.contract.as_ref()?
        } else {
            return None;
        };
        let side = if call.isAsk { &book.asks } else { &book.bids };
        let answer = assembleOrderbookFromOrdersReturn {
            arrayPrices: side
                .iter()
                .map(|[price, _]| price.to::<U72>())
                .collect(),
            arrayShares: side
                .iter()
                .map(|[_, shares]| shares.to::<u128>())
                .collect(),
        };
        Some(assembleOrderbookFromOrdersCall::abi_encode_returns(&answer))
    }
}

/// The commission rates a proxy's `getConfig()` reports, each per 1e18 of the executed value.
#[derive(Debug, Clone, Copy)]
pub(super) struct Rates {
    pub aggressive: u64,
    pub passive: u64,
    pub passive_payout: u64,
}

impl Rates {
    /// Distinct and non-zero, so that reading the wrong one shows.
    pub(super) const DISTINCT: Rates = Rates {
        aggressive: 100_000_000_000_007,
        passive: 70_000_000_000_000,
        passive_payout: 3_000,
    };

    /// The taker's rate the state charges.
    pub(super) fn taker(self) -> u64 {
        self.aggressive + self.passive_payout
    }
}

/// What a proxy answers `getConfig()` with: as recorded, with its tokens swapped, with native ETH
/// on the other side, or with other rates.
fn config_answer(
    market: &FixtureMarket,
    swapped: bool,
    native_flipped: bool,
    rates: Option<Rates>,
) -> Vec<u8> {
    let config = &market.config;
    let (token_x, token_y) =
        if swapped { (config.token_y, config.token_x) } else { (config.token_x, config.token_y) };
    let rates = rates.unwrap_or_else(|| Rates {
        aggressive: config.aggressive_commission_rate.to(),
        passive: 0,
        passive_payout: config.passive_order_payout_rate.to(),
    });
    getConfigCall::abi_encode_returns(&getConfigReturn {
        scalingFactorTokenX: config.scaling_x,
        scalingFactorTokenY: config.scaling_y,
        tokenX: token_x,
        tokenY: token_y,
        supportsNativeEth: config.supports_native_eth,
        isTokenXWeth: config.is_token_x_weth != native_flipped,
        askTrie: Address::ZERO,
        bidTrie: Address::ZERO,
        adminCommissionRate: 0,
        totalAggressiveCommissionRate: rates.aggressive,
        totalPassiveCommissionRate: rates.passive,
        passiveOrderPayoutRate: rates.passive_payout,
        shouldInvokeOnTrade: false,
    })
}

/// How long the source under test gives the market API once it holds a list, short enough that a
/// test of a hanging API stays fast.
pub(super) const TEST_API_TIMEOUT: Duration = Duration::from_millis(200);

/// How long the source under test gives the market API for its first list.
pub(super) const TEST_FIRST_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// A source reading `server` as both the API and the node for Tycho's router, its clock at
/// `now_ms` (Unix milliseconds).
pub(super) fn source(
    server: &MockHttpServer,
    now_ms: u64,
    market_refresh: Duration,
) -> HanjiBookSource {
    HanjiBookSource {
        markets: MarketList::new(
            format!("{}/aggregators/markets", server.url()),
            BASE_DEPLOYMENT
                .wrapped_native()
                .unwrap(),
            market_refresh,
            TEST_API_TIMEOUT,
            TEST_FIRST_FETCH_TIMEOUT,
        ),
        reader: HanjiReader::new(Url::parse(&server.url()).unwrap(), BASE_DEPLOYMENT, BASE_ROUTER),
        assembler: HanjiBookAssembler { book_config: book_config() },
        clock: Arc::new(move || now_ms),
        last_block: Arc::new(AtomicU64::new(0)),
    }
}
