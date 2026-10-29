//! Fixtures shared by the BaiBai state and feed tests: recordings of the Base deployment, the
//! states they describe, and a JSON-RPC node that serves one.

use std::{collections::HashMap, sync::Arc};

use alloy::{
    primitives::{aliases::U48, Address, U256},
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getCurrentBlockTimestampCall, Result as Call3Result,
        },
        MULTICALL3_ADDRESS,
    },
    sol_types::{SolCall, SolValue},
};
use serde::Deserialize;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::{
    book::BookFeedConfig,
    pamm::protocols::baibai::{
        math::{Knot, Side},
        reader::{
            availableLiquidityCall, cUnitCall, curveBookCall, custodianCall, knotCall, pairCall,
            quoteTokenCall, ttlCall, validUntilCall, PairView, SideView,
        },
        state::BaibaiState,
        Deployment, BASE_DEPLOYMENT, BASE_LISTED_BASES,
    },
    snapshot_feed::http::test_support::{spawn_http_server, MockHttpServer, MockRequest},
};

/// Recorded by `testdata/record_fixture.py` at three Base blocks: the first with every cursor at
/// zero, the second with WETH's ask partly filled, the third with its bid partly filled.
const RECORDINGS: [&str; 3] = [
    include_str!("testdata/live_53234728.json"),
    include_str!("testdata/live_53234783.json"),
    include_str!("testdata/live_53236603.json"),
];

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Fixture {
    pub block: u64,
    pub timestamp: u64,
    pub ttl: u64,
    pub c_unit: U256,
    pub available_quote: U256,
    pub pairs: Vec<FixturePair>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixturePair {
    pub base: Address,
    pub valid_until: u64,
    pub available_base: U256,
    pub fee_pips: u32,
    pub pair: FixtureView,
    pub cases: Vec<FixtureCase>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureView {
    pub seq: u64,
    pub fill_seq: u64,
    pub last_update_at: u64,
    pub mid: U256,
    pub q_unit: U256,
    pub ask: FixtureSide,
    pub bid: FixtureSide,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureSide {
    pub spread_bps: i16,
    pub depth_bps: u16,
    pub filled: U256,
    pub knots: Vec<[u64; 2]>,
}

/// One recorded quote: `quoteFor` for the router (`amount_out`, `fee`) and the raw curve.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct FixtureCase {
    pub sell_base: bool,
    pub amount_in: U256,
    pub amount_out: U256,
    pub curve_out: U256,
}

/// Every recording.
pub(super) fn fixtures() -> Vec<Fixture> {
    RECORDINGS
        .iter()
        .map(|recording| serde_json::from_str(recording).unwrap())
        .collect()
}

/// The recording with every cursor at zero.
pub(super) fn fixture() -> Fixture {
    serde_json::from_str(RECORDINGS[0]).unwrap()
}

pub(super) fn bytes(address: Address) -> Bytes {
    Bytes::from(address.to_vec())
}

pub(super) fn weth() -> Address {
    BASE_LISTED_BASES[0]
}

pub(super) fn cbbtc() -> Address {
    BASE_LISTED_BASES[1]
}

pub(super) fn token(address: Address) -> Token {
    let (symbol, decimals) = match address {
        a if a == weth() => ("WETH", 18),
        a if a == cbbtc() => ("cbBTC", 8),
        a if a == BASE_DEPLOYMENT.quote => ("USDC", 6),
        _ => ("TKN", 18),
    };
    Token::new(&bytes(address), symbol, decimals, 0, &[Some(10_000)], Chain::Base, 100)
}

/// The recorded deployment's tokens on Base, with no TVL floor.
pub(super) fn book_config() -> BookFeedConfig {
    let tokens: HashMap<Bytes, Token> = [weth(), cbbtc(), BASE_DEPLOYMENT.quote]
        .into_iter()
        .map(|address| (bytes(address), token(address)))
        .collect();
    BookFeedConfig { chain: Chain::Base, tokens: Arc::new(tokens), min_tvl_usd: 0.0 }
}

fn side(side: &FixtureSide) -> Side {
    Side {
        spread_bps: side.spread_bps,
        depth_bps: side.depth_bps,
        filled: side.filled,
        knots: side
            .knots
            .iter()
            .map(|[q, extra]| Knot { q: *q, extra: *extra })
            .collect(),
    }
}

/// The state the recording describes for `pair`.
pub(super) fn state(fixture: &Fixture, pair: &FixturePair) -> BaibaiState {
    BaibaiState {
        base_token: token(pair.base),
        quote_token: token(BASE_DEPLOYMENT.quote),
        mid: pair.pair.mid,
        q_unit: pair.pair.q_unit,
        c_unit: fixture.c_unit,
        ask: side(&pair.pair.ask),
        bid: side(&pair.pair.bid),
        available: [pair.available_base, fixture.available_quote],
        fee_pips: pair.fee_pips,
    }
    .validated()
    .unwrap()
}

/// The first recording's WETH pair.
pub(super) fn weth_state() -> BaibaiState {
    let fixture = fixture();
    state(&fixture, &fixture.pairs[0])
}

/// A JSON-RPC node serving a recording, with knobs for what a test wants it to get wrong. Like
/// the chain, the custodian reverts for an address that is not a token, and a revert in a call
/// that may not fail reverts the whole `aggregate3`.
#[derive(Clone)]
pub(super) struct FakeNode {
    pub fixture: Fixture,
    /// What the entrypoint answers `curveBook()` with.
    pub entrypoint_curve_book: Address,
    /// Overrides the block timestamp, to age every curve.
    pub timestamp: u64,
    /// Addresses with no token behind them, which the custodian reverts for. Any other token the
    /// recording does not list reads as zero custody.
    pub not_tokens: Vec<Address>,
}

impl FakeNode {
    pub(super) fn new() -> Self {
        let fixture = fixture();
        Self {
            timestamp: fixture.timestamp,
            fixture,
            entrypoint_curve_book: BASE_DEPLOYMENT.curve_book,
            not_tokens: Vec::new(),
        }
    }

    pub(super) async fn spawn(self) -> MockHttpServer {
        spawn_http_server(move |request| Some(("200 OK", self.answer(request)))).await
    }

    fn answer(&self, request: &MockRequest) -> String {
        let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(request["method"], "eth_call", "the fake node serves eth_call only");
        let block = &request["params"][1];
        assert!(
            *block == "latest" || *block == format!("{:#x}", self.fixture.block),
            "a read asks for the latest block, then for the block it read: {block}"
        );
        let input = request["params"][0]["input"]
            .as_str()
            .or_else(|| request["params"][0]["data"].as_str())
            .unwrap();
        let input = alloy::primitives::hex::decode(input).unwrap();
        let calls = aggregate3Call::abi_decode(&input)
            .unwrap()
            .calls;
        let mut answers = Vec::with_capacity(calls.len());
        for call in &calls {
            let answer = self.call(call.target, &call.callData);
            if answer.is_none() && !call.allowFailure {
                let error = serde_json::json!({ "code": 3, "message": "Multicall3: call failed" });
                return serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "error": error })
                    .to_string();
            }
            answers.push(Call3Result {
                success: answer.is_some(),
                returnData: answer.unwrap_or_default().into(),
            });
        }
        let encoded = aggregate3Call::abi_encode_returns(&answers);
        let result = format!("0x{}", alloy::primitives::hex::encode(encoded));
        serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }).to_string()
    }

    fn pair(&self, base: Address) -> Option<&FixturePair> {
        self.fixture
            .pairs
            .iter()
            .find(|pair| pair.base == base)
    }

    /// The answer to one call, or `None` where the chain reverts.
    fn call(&self, target: Address, data: &[u8]) -> Option<Vec<u8>> {
        let Deployment { entrypoint, curve_book, custodian, quote } = BASE_DEPLOYMENT;
        let selector: [u8; 4] = data[..4].try_into().unwrap();
        let answer = match (target, selector) {
            (t, getBlockNumberCall::SELECTOR) if t == MULTICALL3_ADDRESS => {
                U256::from(self.fixture.block).abi_encode()
            }
            (t, getCurrentBlockTimestampCall::SELECTOR) if t == MULTICALL3_ADDRESS => {
                U256::from(self.timestamp).abi_encode()
            }
            (t, curveBookCall::SELECTOR) if t == entrypoint => {
                self.entrypoint_curve_book.abi_encode()
            }
            (t, custodianCall::SELECTOR) if t == entrypoint => custodian.abi_encode(),
            (t, quoteTokenCall::SELECTOR) if t == entrypoint || t == curve_book => {
                quote.abi_encode()
            }
            (t, ttlCall::SELECTOR) if t == curve_book => self.fixture.ttl.abi_encode(),
            (t, cUnitCall::SELECTOR) if t == curve_book => self.fixture.c_unit.abi_encode(),
            (t, pairCall::SELECTOR) if t == curve_book => {
                let base = pairCall::abi_decode(data).unwrap().base;
                pairCall::abi_encode_returns(&self.view(base))
            }
            (t, validUntilCall::SELECTOR) if t == curve_book => {
                let base = validUntilCall::abi_decode(data)
                    .unwrap()
                    .base;
                self.pair(base)
                    .map_or(0, |pair| pair.valid_until)
                    .abi_encode()
            }
            (t, knotCall::SELECTOR) if t == curve_book => {
                let call = knotCall::abi_decode(data).unwrap();
                let pair = self.pair(call.base).unwrap();
                let side = if call.side == 0 { &pair.pair.ask } else { &pair.pair.bid };
                let [q, extra] = side.knots[call.index.to::<usize>()];
                (U48::from(q), U48::from(extra)).abi_encode_params()
            }
            (t, availableLiquidityCall::SELECTOR) if t == custodian => {
                let token = availableLiquidityCall::abi_decode(data)
                    .unwrap()
                    .token;
                if token == quote {
                    self.fixture
                        .available_quote
                        .abi_encode()
                } else if self.not_tokens.contains(&token) {
                    return None;
                } else {
                    self.pair(token)
                        .map_or(U256::ZERO, |pair| pair.available_base)
                        .abi_encode()
                }
            }
            _ => panic!("the fake node does not answer {selector:?} on {target}"),
        };
        Some(answer)
    }

    fn view(&self, base: Address) -> PairView {
        let empty = SideView { spreadBps: 0, depthBps: 0, filled: U256::ZERO, knotCount: 0 };
        let Some(pair) = self.pair(base) else {
            return PairView {
                seq: 0,
                fillSeq: 0,
                lastUpdateAt: 0,
                mid: Default::default(),
                qUnit: U256::ZERO,
                ask: empty.clone(),
                bid: empty,
            };
        };
        let side = |side: &FixtureSide| SideView {
            spreadBps: side.spread_bps,
            depthBps: side.depth_bps,
            filled: side.filled,
            knotCount: side.knots.len() as u8,
        };
        PairView {
            seq: pair.pair.seq,
            fillSeq: pair.pair.fill_seq,
            lastUpdateAt: pair.pair.last_update_at,
            mid: pair.pair.mid.to(),
            qUnit: pair.pair.q_unit,
            ask: side(&pair.pair.ask),
            bid: side(&pair.pair.bid),
        }
    }
}
