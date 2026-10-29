//! Reads every configured BaiBai pair over JSON-RPC, all at one block.
//!
//! A read is two `Multicall3.aggregate3` calls: one at the latest block for its number and
//! timestamp, the deployment's wiring, the book's settings, custody and each pair's parameters,
//! then one pinned to that block for the knots those parameters say each side holds.

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes as AlloyBytes, U256},
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getCurrentBlockTimestampCall, Call3,
            Result as Call3Result,
        },
        Provider, RootProvider, MULTICALL3_ADDRESS,
    },
    rpc::types::{TransactionInput, TransactionRequest},
    sol,
    sol_types::SolCall,
};
use reqwest::Url;
use tracing::warn;

use crate::{
    pamm::protocols::baibai::{
        math::{Knot, Side},
        Deployment,
    },
    snapshot_feed::errors::FeedError,
};

// The CurveBook, Entrypoint and Custodian getters, as documented at
// <https://docs.baibai.cx/takers/quoting-and-swapping>.
sol! {
    #[derive(Debug)]
    struct SideView {
        int16 spreadBps;
        uint16 depthBps;
        uint256 filled;
        uint8 knotCount;
    }

    #[derive(Debug)]
    struct PairView {
        uint64 seq;
        uint64 fillSeq;
        uint64 lastUpdateAt;
        uint80 mid;
        uint256 qUnit;
        SideView ask;
        SideView bid;
    }

    function pair(address base) external view returns (PairView memory);
    function knot(address base, uint8 side, uint256 index) external view returns (uint48 q, uint48 extra);
    function ttl() external view returns (uint64);
    function validUntil(address base) external view returns (uint64);
    function cUnit() external view returns (uint256);
    function quoteToken() external view returns (address);
    function curveBook() external view returns (address);
    function custodian() external view returns (address);
    function availableLiquidity(address token) external view returns (uint256);
}

/// `CurveBook.knot`'s side index of the ask and the bid.
const ASK_SIDE: u8 = 0;
const BID_SIDE: u8 = 1;

/// The chain's answer for one listed pair, decoded but not yet validated.
#[derive(Debug, Clone)]
pub(super) struct PairRead {
    pub base: Address,
    pub last_update_at: u64,
    pub valid_until: u64,
    /// Quote atoms per 10^18 base atoms, before each side's spread.
    pub mid: U256,
    pub q_unit: U256,
    pub available_base: U256,
    pub ask: Side,
    pub bid: Side,
}

/// Everything one read returned, at one block.
#[derive(Debug, Clone)]
pub(super) struct BookRead {
    pub block_number: u64,
    pub timestamp: u64,
    pub ttl: u64,
    pub c_unit: U256,
    pub available_quote: U256,
    /// The listed pairs among the configured bases, in the configured order.
    pub pairs: Vec<PairRead>,
}

#[derive(Clone, derive_more::Debug)]
pub(super) struct BaibaiReader {
    #[debug(skip)]
    provider: RootProvider,
    /// Removed from transport errors before they are reported: an RPC URL commonly carries a key,
    /// and the HTTP client's errors quote the URL they failed on.
    #[debug(skip)]
    rpc_url: String,
    deployment: Deployment,
    bases: Vec<Address>,
}

impl BaibaiReader {
    pub(super) fn new(rpc_url: Url, deployment: Deployment, bases: Vec<Address>) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            provider: RootProvider::new_http(rpc_url),
            deployment,
            bases,
        }
    }

    /// Reads every configured base at the latest block. A base the book has never listed, and one
    /// whose calls revert (an address that is no token), is left out; the other pairs are read.
    /// Fails fatally when the deployment's contracts no longer point at each other the way this
    /// integration was built for, since no later read will either.
    pub(super) async fn read(&self) -> Result<BookRead, FeedError> {
        let settings = self.settings_calls();
        let mut first = settings.clone();
        for base in &self.bases {
            first.extend(self.pair_calls(*base));
        }
        let answers = self
            .aggregate(first, BlockId::latest())
            .await?;
        let (settings_answers, pair_answers) = answers.split_at(settings.len());
        let mut read = self.decode_settings(settings_answers)?;

        let mut listed = Vec::new();
        for (base, answers) in self
            .bases
            .iter()
            .zip(pair_answers.chunks(PAIR_CALLS))
        {
            if let Some(pair) = decode_pair(*base, answers)? {
                listed.push(pair);
            }
        }

        let mut second = Vec::new();
        for (pair, view) in &listed {
            second.extend(self.knot_calls(pair.base, view));
        }
        let answers = self
            .aggregate(second, BlockId::number(read.block_number))
            .await?;
        let mut answers = answers.iter();
        for (mut pair, view) in listed {
            let ask = read_knots(&mut answers, view.ask.knotCount)?;
            let bid = read_knots(&mut answers, view.bid.knotCount)?;
            let (Some(ask), Some(bid)) = (ask, bid) else {
                warn!(base = %pair.base, "skipping BaiBai pair whose knots could not be read");
                continue;
            };
            pair.ask.knots = ask;
            pair.bid.knots = bid;
            read.pairs.push(pair);
        }
        Ok(read)
    }

    /// The block, the deployment's wiring, the book's settings and the quote token's custody.
    fn settings_calls(&self) -> Vec<Call3> {
        let Deployment { entrypoint, curve_book, custodian, quote } = self.deployment;
        vec![
            call(MULTICALL3_ADDRESS, getBlockNumberCall {}.abi_encode(), false),
            call(MULTICALL3_ADDRESS, getCurrentBlockTimestampCall {}.abi_encode(), false),
            call(entrypoint, curveBookCall {}.abi_encode(), false),
            call(entrypoint, custodianCall {}.abi_encode(), false),
            call(entrypoint, quoteTokenCall {}.abi_encode(), false),
            call(curve_book, quoteTokenCall {}.abi_encode(), false),
            call(curve_book, ttlCall {}.abi_encode(), false),
            call(curve_book, cUnitCall {}.abi_encode(), false),
            call(custodian, availableLiquidityCall { token: quote }.abi_encode(), false),
        ]
    }

    /// The read's block and book-wide values, with no pairs yet.
    fn decode_settings(&self, answers: &[Call3Result]) -> Result<BookRead, FeedError> {
        let Deployment { entrypoint, curve_book, custodian, quote } = self.deployment;
        let wiring = [
            (
                "entrypoint's curve book",
                decode::<curveBookCall>(&answers[2], "curveBook")?,
                curve_book,
            ),
            (
                "entrypoint's custodian",
                decode::<custodianCall>(&answers[3], "custodian")?,
                custodian,
            ),
            (
                "entrypoint's quote token",
                decode::<quoteTokenCall>(&answers[4], "quoteToken")?,
                quote,
            ),
            (
                "curve book's quote token",
                decode::<quoteTokenCall>(&answers[5], "quoteToken")?,
                quote,
            ),
        ];
        for (what, found, expected) in wiring {
            if found != expected {
                return Err(FeedError::Fatal(format!(
                    "BaiBai entrypoint {entrypoint}: the {what} is {found}, not {expected}; the \
                     deployment changed and the integration needs updating"
                )));
            }
        }
        Ok(BookRead {
            block_number: to_u64(decode::<getBlockNumberCall>(&answers[0], "block number")?)?,
            timestamp: to_u64(decode::<getCurrentBlockTimestampCall>(
                &answers[1],
                "block timestamp",
            )?)?,
            ttl: decode::<ttlCall>(&answers[6], "ttl")?,
            c_unit: decode::<cUnitCall>(&answers[7], "cUnit")?,
            available_quote: decode::<availableLiquidityCall>(&answers[8], "availableLiquidity")?,
            pairs: Vec::new(),
        })
    }

    fn pair_calls(&self, base: Address) -> [Call3; PAIR_CALLS] {
        let Deployment { curve_book, custodian, .. } = self.deployment;
        [
            call(curve_book, pairCall { base }.abi_encode(), true),
            call(curve_book, validUntilCall { base }.abi_encode(), true),
            call(custodian, availableLiquidityCall { token: base }.abi_encode(), true),
        ]
    }

    fn knot_calls(&self, base: Address, view: &PairView) -> Vec<Call3> {
        let curve_book = self.deployment.curve_book;
        let mut calls = Vec::new();
        for (side, count) in [(ASK_SIDE, view.ask.knotCount), (BID_SIDE, view.bid.knotCount)] {
            for index in 0..count {
                let knot = knotCall { base, side, index: U256::from(index) };
                calls.push(call(curve_book, knot.abi_encode(), true));
            }
        }
        calls
    }

    async fn aggregate(
        &self,
        calls: Vec<Call3>,
        at: BlockId,
    ) -> Result<Vec<Call3Result>, FeedError> {
        let expected = calls.len();
        let request = TransactionRequest::default()
            .to(MULTICALL3_ADDRESS)
            .input(TransactionInput::new(
                aggregate3Call { calls }
                    .abi_encode()
                    .into(),
            ));
        let raw = self
            .provider
            .call(request)
            .block(at)
            .await
            .map_err(|e| {
                let error = e
                    .to_string()
                    .replace(&self.rpc_url, "<rpc url>");
                FeedError::Connection(format!("BaiBai multicall: {error}"))
            })?;
        let answers = aggregate3Call::abi_decode_returns(&raw)
            .map_err(|e| FeedError::Parsing(format!("BaiBai multicall: {e}")))?;
        if answers.len() != expected {
            return Err(FeedError::Parsing(format!(
                "BaiBai multicall answered {} of {expected} calls",
                answers.len()
            )));
        }
        Ok(answers)
    }
}

/// Calls per configured base in the first multicall: see [`BaibaiReader::pair_calls`].
const PAIR_CALLS: usize = 3;

fn call(target: Address, data: Vec<u8>, allow_failure: bool) -> Call3 {
    Call3 { target, allowFailure: allow_failure, callData: AlloyBytes::from(data) }
}

/// One configured base's pair, its knots still to read, with the view that says how many there
/// are; `None` when the book has not listed it or one of its calls reverted.
fn decode_pair(
    base: Address,
    answers: &[Call3Result],
) -> Result<Option<(PairRead, PairView)>, FeedError> {
    if answers
        .iter()
        .any(|answer| !answer.success)
    {
        warn!(%base, "skipping BaiBai base whose pair or custody call reverted");
        return Ok(None);
    }
    let view = decode::<pairCall>(&answers[0], "pair")?;
    if view.qUnit == U256::ZERO {
        return Ok(None);
    }
    let side = |side: &SideView| Side {
        spread_bps: side.spreadBps,
        depth_bps: side.depthBps,
        filled: side.filled,
        knots: Vec::new(),
    };
    let pair = PairRead {
        base,
        last_update_at: view.lastUpdateAt,
        valid_until: decode::<validUntilCall>(&answers[1], "validUntil")?,
        mid: U256::from(view.mid),
        q_unit: view.qUnit,
        available_base: decode::<availableLiquidityCall>(&answers[2], "availableLiquidity")?,
        ask: side(&view.ask),
        bid: side(&view.bid),
    };
    Ok(Some((pair, view)))
}

/// The next `count` knots of the second multicall; `None` when one of their calls reverted.
fn read_knots<'a>(
    answers: &mut impl Iterator<Item = &'a Call3Result>,
    count: u8,
) -> Result<Option<Vec<Knot>>, FeedError> {
    let mut knots = Some(Vec::with_capacity(usize::from(count)));
    for _ in 0..count {
        let answer = answers
            .next()
            .ok_or_else(|| FeedError::Parsing("BaiBai multicall answered too few calls".into()))?;
        if !answer.success {
            knots = None;
            continue;
        }
        let knot = decode::<knotCall>(answer, "knot")?;
        if let Some(knots) = &mut knots {
            knots.push(Knot { q: knot.q.to(), extra: knot.extra.to() });
        }
    }
    Ok(knots)
}

fn to_u64(value: U256) -> Result<u64, FeedError> {
    value
        .try_into()
        .map_err(|_| FeedError::Parsing(format!("BaiBai block value {value} exceeds u64")))
}

fn decode<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, FeedError> {
    C::abi_decode_returns(&answer.returnData)
        .map_err(|e| FeedError::Parsing(format!("BaiBai {what}: {e}")))
}
