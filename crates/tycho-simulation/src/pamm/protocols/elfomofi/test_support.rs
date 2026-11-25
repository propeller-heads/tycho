//! Fixtures shared by the ElFomoFi tests: recordings of the helper on each chain, a book built by
//! hand, and a JSON-RPC node that serves answers of the tests' choosing.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{address, Address, U256},
    sol_types::SolCall,
};
use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::{
    book::BookFeedConfig,
    pamm::protocols::elfomofi::{
        models::{
            getAllOrderbooksCall, getAllOrderbooksReturn, CumulativeLevel, ElfomofiSupportedChain,
            PairOrderbook,
        },
        state::{BookSides, ElfomofiState, Ladder},
    },
    snapshot_feed::http::test_support::{spawn_http_server, MockHttpServer},
};

/// `getAllOrderbooks()` as each chain's helper answered it, hex-encoded; the block is in the file
/// name and in the answer. All three postdate the pricing upgrade of 2026-11-20.
pub(super) const BASE_RECORDING: &str = include_str!("testdata/base_62017050.hex");
pub(super) const BSC_RECORDING: &str = include_str!("testdata/bsc_135619488.hex");
pub(super) const ROBINHOOD_RECORDING: &str = include_str!("testdata/robinhood_122891612.hex");

pub(super) fn recording(hex: &str) -> Vec<u8> {
    alloy::primitives::hex::decode(hex.trim()).unwrap()
}

pub(super) const WETH: Address = address!("4200000000000000000000000000000000000006");
pub(super) const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
pub(super) const CBBTC: Address = address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf");

pub(super) fn bytes(address: Address) -> Bytes {
    Bytes::from(address.to_vec())
}

pub(super) fn token(address: Address, symbol: &str, decimals: u32) -> Token {
    Token::new(&bytes(address), symbol, decimals, 0, &[Some(10_000)], Chain::Base, 100)
}

pub(super) fn weth() -> Token {
    token(WETH, "WETH", 18)
}

pub(super) fn usdc() -> Token {
    token(USDC, "USDC", 6)
}

pub(super) fn big(value: u128) -> BigUint {
    BigUint::from(value)
}

/// `levels` as a ladder of `(cumulative input, cumulative output)`.
pub(super) fn ladder(levels: &[(u128, u128)]) -> Ladder {
    Ladder::new(
        levels
            .iter()
            .map(|(input, output)| (big(*input), big(*output)))
            .collect(),
    )
    .unwrap()
}

/// A WETH/USDC book on Base, priced around 3,000 USDC:
/// bids sell 1 WETH for 3,000 USDC, then 2 more for 5,970 (2,985 each);
/// asks sell 3,000 USDC for 0.99 WETH, then 6,000 more for 1.96 WETH.
/// The vault holds 100 WETH and 1,000,000 USDC, so neither side is cut by a balance.
pub(super) const BIDS: [(u128, u128); 2] =
    [(1_000_000_000_000_000_000, 3_000_000_000), (3_000_000_000_000_000_000, 8_970_000_000)];
pub(super) const ASKS: [(u128, u128); 2] =
    [(3_000_000_000, 990_000_000_000_000_000), (9_000_000_000, 2_950_000_000_000_000_000)];

pub(super) fn weth_usdc_state() -> ElfomofiState {
    state_with_balances(big(100_000_000_000_000_000_000), big(1_000_000_000_000))
}

pub(super) fn state_with_balances(base_balance: BigUint, quote_balance: BigUint) -> ElfomofiState {
    ElfomofiState::new(
        ElfomofiSupportedChain::Base,
        (62_000_000, 1_795_000_000),
        (weth(), usdc()),
        BookSides { bids: ladder(&BIDS), asks: ladder(&ASKS), base_balance, quote_balance },
    )
}

/// A book in the helper's own types.
pub(super) fn wire_book(
    base: Address,
    quote: Address,
    asks: &[(u128, u128)],
    bids: &[(u128, u128)],
    balances: (u128, u128),
) -> PairOrderbook {
    let levels = |levels: &[(u128, u128)]| {
        levels
            .iter()
            .map(|(input, output)| CumulativeLevel {
                amountIn: U256::from(*input),
                amountOut: U256::from(*output),
            })
            .collect()
    };
    PairOrderbook {
        base,
        quote,
        askCumulativeLevels: levels(asks),
        bidCumulativeLevels: levels(bids),
        balanceBase: U256::from(balances.0),
        balanceQuote: U256::from(balances.1),
    }
}

/// The helper's answer holding `books`, read at `block` and `timestamp`.
pub(super) fn helper_answer(books: Vec<PairOrderbook>, block: u64, timestamp: u64) -> Vec<u8> {
    getAllOrderbooksCall::abi_encode_returns(&getAllOrderbooksReturn {
        books,
        blockNumber: U256::from(block),
        blockTimestamp: U256::from(timestamp),
    })
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The WETH/USDC book of [`weth_usdc_state`] as the helper sends it.
pub(super) fn weth_usdc_wire() -> PairOrderbook {
    wire_book(WETH, USDC, &ASKS, &BIDS, (100_000_000_000_000_000_000, 1_000_000_000_000))
}

/// WETH and USDC on Base, with no TVL floor.
pub(super) fn base_book_config(min_tvl_usd: f64) -> BookFeedConfig {
    let tokens = [weth(), usdc(), token(CBBTC, "cbBTC", 8)]
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect::<HashMap<_, _>>();
    BookFeedConfig { chain: Chain::Base, tokens: Arc::new(tokens), min_tvl_usd }
}

/// A node that serves `eth_chainId` and the helper's `eth_call`. Each `eth_call` gets the next
/// of `answers`, the last one repeating.
pub(super) struct FakeNode {
    pub chain_id: u64,
    pub answers: Vec<Vec<u8>>,
}

/// What a running [`FakeNode`] has been asked.
pub(super) struct FakeNodeHandle {
    pub server: MockHttpServer,
    pub chain_id_calls: Arc<AtomicUsize>,
}

impl FakeNode {
    pub(super) fn serving(answers: Vec<Vec<u8>>) -> Self {
        FakeNode { chain_id: Chain::Base.id(), answers }
    }

    pub(super) async fn spawn(self) -> FakeNodeHandle {
        let chain_id_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&chain_id_calls);
        let served = Mutex::new(0usize);
        let server = spawn_http_server(move |request| {
            let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            let id = request["id"].clone();
            let result = match request["method"].as_str() {
                Some("eth_chainId") => {
                    calls.fetch_add(1, Ordering::SeqCst);
                    serde_json::json!(format!("{:#x}", self.chain_id))
                }
                Some("eth_call") => {
                    assert_eq!(request["params"][1], "latest", "the helper is read at latest");
                    let mut served = served.lock().unwrap();
                    let index = (*served).min(self.answers.len() - 1);
                    *served += 1;
                    serde_json::json!(format!(
                        "0x{}",
                        alloy::primitives::hex::encode(&self.answers[index])
                    ))
                }
                method => panic!("the fake node does not serve {method:?}"),
            };
            let body = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
            Some(("200 OK", body.to_string()))
        })
        .await;
        FakeNodeHandle { server, chain_id_calls }
    }
}
